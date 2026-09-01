//! Sandboxed tools used by OpenAI-compatible Assistant providers.
//!
//! File descriptors are opened relative to a canonical workspace descriptor
//! with Linux `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS)`.
//! The descriptor-relative open is the enforcement point: the earlier
//! canonicalization is only for Python-compatible policy messages and cannot
//! be raced into following a replacement symlink.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reqwest::Url;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::assistant_custom_view::RENDER_CUSTOM_VIEW_TOOL_NAME;
use crate::assistant_providers::PermissionsConfig;

const ALLOWED_BASH_COMMANDS: &[&str] = &["mlflow", "python", "python3"];
const BASH_TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_CAP: u64 = 1024 * 1024;
const ASSISTANT_SANDBOX_ENV: &str = "MLFLOW_ENABLE_ASSISTANT_SANDBOX";
const REMOTE_ASSISTANT_ENV: &str = "MLFLOW_ENABLE_REMOTE_ASSISTANT";
const SANDBOX_IMAGE_ENV: &str = "MLFLOW_SANDBOX_DOCKER_IMAGE";
const DEFAULT_SANDBOX_IMAGE: &str = "mlflow-sandbox:latest";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub fn static_permission_error(
    tool_name: &str,
    tool_input: &Value,
    permissions: &PermissionsConfig,
    cwd: Option<&Path>,
) -> Option<String> {
    if permissions.full_access {
        return None;
    }
    let Some(tool_input) = tool_input.as_object() else {
        return Some("Permission denied: malformed tool input".to_string());
    };
    if tool_name == "Bash" {
        let command = match tool_input.get("command") {
            None => "",
            Some(Value::String(command)) => command,
            Some(_) => return Some("Permission denied: malformed command".to_string()),
        }
        .trim();
        if let Err(error) = validate_restricted_bash(command, cwd) {
            return Some(error);
        }
    }
    if matches!(tool_name, "Read" | "Write" | "Edit") && !permissions.allow_edit_files {
        return Some(format!("Permission denied: {tool_name} is not allowed"));
    }
    if matches!(tool_name, "Write" | "Edit") && cwd.is_none() {
        return Some(format!(
            "Permission denied: {tool_name} requires a configured project directory"
        ));
    }
    if matches!(tool_name, "Read" | "Write" | "Edit") {
        let raw_path = tool_input
            .get("file_path")
            .filter(|value| python_truthy(value))
            .or_else(|| tool_input.get("path").filter(|value| python_truthy(value)));
        if let Some(raw_path) = raw_path {
            let Some(raw_path_string) = raw_path.as_str() else {
                return Some(format!(
                    "Permission denied: malformed path {}",
                    python_repr(raw_path)
                ));
            };
            let Some(root) = cwd else {
                return Some(format!(
                    "Permission denied: {tool_name} requires a configured project directory"
                ));
            };
            if raw_path_string.contains('\0') {
                return Some(format!(
                    "Permission denied: malformed path {}",
                    python_repr(raw_path)
                ));
            }
            if let Err(error) = resolve_workspace_relative(raw_path_string, root) {
                return Some(if error.kind() == std::io::ErrorKind::PermissionDenied {
                    format!(
                        "Permission denied: path {raw_path_string} is outside the workspace {}",
                        root.display()
                    )
                } else {
                    format!(
                        "Permission denied: malformed path {}",
                        python_repr(raw_path)
                    )
                });
            }
        }
    }
    None
}

pub async fn execute_tool(
    tool_name: &str,
    tool_input: &Value,
    cwd: Option<&Path>,
    tracking_uri: Option<&str>,
    permissions: &PermissionsConfig,
) -> ToolResult {
    if let Some(error) = static_permission_error(tool_name, tool_input, permissions, cwd) {
        return ToolResult::error(error);
    }
    let result = match tool_name {
        "Bash" => execute_bash(tool_input, cwd, tracking_uri, permissions.full_access).await,
        "Read" if permissions.full_access || cwd.is_none() => {
            execute_file_unconfined(tool_input, cwd, FileOperation::Read)
        }
        "Write" if permissions.full_access => {
            execute_file_unconfined(tool_input, cwd, FileOperation::Write)
        }
        "Edit" if permissions.full_access => {
            execute_file_unconfined(tool_input, cwd, FileOperation::Edit)
        }
        "Read" => execute_file(tool_input, cwd, FileOperation::Read),
        "Write" => execute_file(tool_input, cwd, FileOperation::Write),
        "Edit" => execute_file(tool_input, cwd, FileOperation::Edit),
        _ => return ToolResult::error(format!("Unknown tool: {tool_name}")),
    };
    match result {
        Ok(result) => result,
        Err(error) => ToolResult::error(format!("Tool execution failed: {error}")),
    }
}

async fn execute_bash(
    input: &Value,
    cwd: Option<&Path>,
    tracking_uri: Option<&str>,
    full_access: bool,
) -> std::io::Result<ToolResult> {
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if command.is_empty() {
        return Ok(ToolResult::error("No command provided"));
    }
    if assistant_sandbox_enabled() {
        return Ok(execute_bash_in_sandbox(command, cwd, tracking_uri, full_access).await);
    }
    execute_bash_on_host(command, cwd, tracking_uri, full_access).await
}

async fn execute_bash_on_host(
    command: &str,
    cwd: Option<&Path>,
    tracking_uri: Option<&str>,
    full_access: bool,
) -> std::io::Result<ToolResult> {
    let mut child = if full_access {
        let mut child = Command::new("/bin/sh");
        child.arg("-c").arg(command);
        child
    } else {
        let argv = shlex::split(command).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "malformed command")
        })?;
        let mut child = Command::new(&argv[0]);
        child.args(&argv[1..]);
        child
    };
    child
        .current_dir(cwd.unwrap_or_else(|| Path::new(".")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(tracking_uri) = tracking_uri {
        child.env("MLFLOW_TRACKING_URI", tracking_uri);
    }
    let mut child = child.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(read_capped(stdout));
    let stderr_task = tokio::spawn(read_capped(stderr));
    let status = match timeout(BASH_TIMEOUT, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            return Ok(ToolResult::error("Command timed out after 120 seconds"));
        }
    };
    let stdout = stdout_task.await.map_err(std::io::Error::other)??;
    let stderr = stderr_task.await.map_err(std::io::Error::other)??;
    let mut combined = stdout;
    combined.extend_from_slice(&stderr);
    combined.truncate(OUTPUT_CAP as usize);
    let output = String::from_utf8_lossy(&combined).trim().to_string();
    if status.success() {
        Ok(ToolResult::ok(if output.is_empty() {
            "(no output)".to_string()
        } else {
            output
        }))
    } else {
        Ok(ToolResult::error(if output.is_empty() {
            format!("Exit code: {}", status.code().unwrap_or(-1))
        } else {
            output
        }))
    }
}

pub fn assistant_sandbox_enabled() -> bool {
    assistant_sandbox_enabled_from(
        parse_bool_env(ASSISTANT_SANDBOX_ENV),
        parse_bool_env(REMOTE_ASSISTANT_ENV) == Some(true),
        || find_executable("docker").is_some(),
    )
}

fn assistant_sandbox_enabled_from(
    override_value: Option<bool>,
    remote_enabled: bool,
    docker_available: impl FnOnce() -> bool,
) -> bool {
    override_value.unwrap_or_else(|| remote_enabled && docker_available())
}

fn parse_bool_env(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

async fn execute_bash_in_sandbox(
    command: &str,
    cwd: Option<&Path>,
    tracking_uri: Option<&str>,
    full_access: bool,
) -> ToolResult {
    let argv = if full_access {
        vec!["sh".to_string(), "-c".to_string(), command.to_string()]
    } else {
        match shlex::split(command) {
            Some(argv) => argv,
            None => return ToolResult::error("Permission denied: malformed command"),
        }
    };
    match run_in_sandbox(&argv, cwd, sandbox_environment(tracking_uri)).await {
        Ok(result) => result,
        Err(error) => ToolResult::error(format!(
            "Sandbox is enabled but the command could not be run: {error}"
        )),
    }
}

fn sandbox_environment(tracking_uri: Option<&str>) -> Vec<(String, String)> {
    let mut environment = Vec::new();
    if let Some(uri) = tracking_uri.and_then(uri_without_credentials) {
        environment.push((
            "MLFLOW_TRACKING_URI".to_string(),
            to_container_host_uri(uri),
        ));
    }
    if let Some(uri) = std::env::var("MLFLOW_REGISTRY_URI")
        .ok()
        .as_deref()
        .and_then(uri_without_credentials)
    {
        environment.push((
            "MLFLOW_REGISTRY_URI".to_string(),
            to_container_host_uri(uri),
        ));
    }
    environment
}

fn uri_without_credentials(uri: &str) -> Option<&str> {
    let parsed = Url::parse(uri).ok()?;
    (parsed.username().is_empty() && parsed.password().is_none()).then_some(uri)
}

pub fn to_container_host_uri(uri: &str) -> String {
    if uri.is_empty() {
        return String::new();
    }
    let Ok(parsed) = Url::parse(uri) else {
        return uri.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return uri.to_string();
    };
    if !matches!(
        host.trim_matches(['[', ']']).to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "0.0.0.0" | "::1"
    ) {
        return uri.to_string();
    }
    let Some(authority_start) = uri.find("://").map(|index| index + 3) else {
        return uri.to_string();
    };
    let authority_end = uri[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(uri.len());
    let authority = &uri[authority_start..authority_end];
    let (userinfo, host_port) = authority
        .rsplit_once('@')
        .map_or(("", authority), |(userinfo, host_port)| {
            (userinfo, host_port)
        });
    let port = if host_port.starts_with('[') {
        host_port
            .find(']')
            .map(|index| &host_port[index + 1..])
            .unwrap_or("")
    } else {
        host_port
            .rsplit_once(':')
            .map(|(_, port)| &host_port[host_port.len() - port.len() - 1..])
            .unwrap_or("")
    };
    let userinfo = if userinfo.is_empty() {
        String::new()
    } else {
        format!("{userinfo}@")
    };
    format!(
        "{}{userinfo}host.docker.internal{port}{}",
        &uri[..authority_start],
        &uri[authority_end..]
    )
}

fn sandbox_docker_argv(
    container_name: &str,
    image: &str,
    command: &[String],
    cwd: Option<&Path>,
    environment: &[(String, String)],
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "--network".to_string(),
        "bridge".to_string(),
        "--add-host".to_string(),
        "host.docker.internal:host-gateway".to_string(),
        "--memory".to_string(),
        "1g".to_string(),
        "--memory-swap".to_string(),
        "1g".to_string(),
        "--cpus".to_string(),
        "1".to_string(),
        "--pids-limit".to_string(),
        "256".to_string(),
        "--read-only".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges:true".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--user".to_string(),
        format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        }),
        "--env".to_string(),
        "HOME=/tmp".to_string(),
    ];
    for (key, value) in environment {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }
    if let Some(cwd) = cwd {
        args.extend([
            "--volume".to_string(),
            format!("{}:/workspace:rw", cwd.display()),
            "--workdir".to_string(),
            "/workspace".to_string(),
        ]);
    }
    args.push(image.to_string());
    args.extend(command.iter().cloned());
    args
}

async fn run_in_sandbox(
    command: &[String],
    cwd: Option<&Path>,
    environment: Vec<(String, String)>,
) -> Result<ToolResult, String> {
    let docker = find_executable("docker")
        .ok_or_else(|| "Docker CLI is not available on PATH".to_string())?;
    let image = std::env::var(SANDBOX_IMAGE_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string());
    ensure_sandbox_image(&docker, &image).await?;

    let container_name = format!("mlflow-assistant-sandbox-{}", uuid::Uuid::new_v4().simple());
    let args = sandbox_docker_argv(&container_name, &image, command, cwd, &environment);
    let mut child = Command::new(&docker)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start sandbox container: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(read_capped(stdout));
    let stderr_task = tokio::spawn(read_capped(stderr));
    let status = match timeout(BASH_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            remove_sandbox_container(&docker, &container_name).await;
            return Err(format!("Sandbox execution failed while waiting: {error}"));
        }
        Err(_) => {
            let _ = child.kill().await;
            remove_sandbox_container(&docker, &container_name).await;
            return Ok(ToolResult::error("Command timed out after 120 seconds"));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let stderr = stderr_task
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    remove_sandbox_container(&docker, &container_name).await;
    let mut combined = stdout;
    combined.extend(stderr);
    combined.truncate(OUTPUT_CAP as usize);
    let output = String::from_utf8_lossy(&combined).trim().to_string();
    Ok(if status.success() {
        ToolResult::ok(if output.is_empty() {
            "(no output)"
        } else {
            &output
        })
    } else {
        ToolResult::error(if output.is_empty() {
            format!("Exit code: {}", status.code().unwrap_or(-1))
        } else {
            output
        })
    })
}

async fn ensure_sandbox_image(docker: &Path, image: &str) -> Result<(), String> {
    let inspect = Command::new(docker)
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|error| format!("Docker daemon is not reachable: {error}"))?;
    if inspect.success() {
        return Ok(());
    }
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    std::fs::write(
        directory.path().join("Dockerfile"),
        "FROM python:3.11-slim\nRUN pip install --no-cache-dir mlflow\n",
    )
    .map_err(|error| error.to_string())?;
    let output = Command::new(docker)
        .args(["build", "--tag", image])
        .arg(directory.path())
        .output()
        .await
        .map_err(|error| format!("Failed to prepare sandbox image {image:?}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "Failed to prepare sandbox image {image:?}: {detail}"
        ))
    }
}

async fn remove_sandbox_container(docker: &Path, name: &str) {
    let _ = Command::new(docker)
        .args(["rm", "--force", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

fn find_executable(binary: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(binary))
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(reader: R) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(OUTPUT_CAP).read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn validate_restricted_bash(command: &str, cwd: Option<&Path>) -> Result<(), String> {
    let argv =
        shlex::split(command).ok_or_else(|| "Permission denied: malformed command".to_string())?;
    if argv.is_empty() {
        return Err(allowed_commands_error());
    }
    if command.contains(['$', '`', '~', '\n', '\r', '<', '>']) {
        return Err("Permission denied: shell expansion or redirection is not allowed".to_string());
    }
    if command.contains([';', '&', '|']) {
        return Err("Permission denied: command chaining is not allowed".to_string());
    }
    if argv
        .iter()
        .any(|arg| matches!(arg.as_str(), "|" | "||" | "&&" | ";" | "&"))
    {
        return Err("Permission denied: command chaining is not allowed".to_string());
    }
    if !ALLOWED_BASH_COMMANDS.contains(&argv[0].as_str()) || argv[0].contains('=') {
        return Err(allowed_commands_error());
    }
    if matches!(argv[0].as_str(), "python" | "python3") {
        if cwd.is_none() {
            return Err(format!(
                "Permission denied: {} requires a configured project directory",
                argv[0]
            ));
        }
        if let Some(index) = argv.iter().position(|arg| arg == "-c") {
            let script = argv.get(index + 1).map(String::as_str).unwrap_or("");
            let dangerous = [
                "open(",
                "pathlib",
                "os.",
                "subprocess",
                "shutil",
                "socket",
                "__import__",
                "eval(",
                "exec(",
            ];
            if dangerous.iter().any(|needle| script.contains(needle)) {
                return Err(
                    "Permission denied: Python code may access resources outside the workspace"
                        .to_string(),
                );
            }
        }
    }
    if let Some(root) = cwd {
        for arg in argv.iter().skip(1).filter(|arg| !arg.starts_with('-')) {
            if arg.contains('\\') || arg.contains("file://") {
                return Err("Permission denied: ambiguous path syntax is not allowed".to_string());
            }
            if looks_like_path(arg) && resolve_workspace_relative(arg, root).is_err() {
                return Err(format!(
                    "Permission denied: path {arg} is outside the workspace {}",
                    root.display()
                ));
            }
        }
    }
    Ok(())
}

fn allowed_commands_error() -> String {
    "Permission denied: only mlflow, python, python3 commands are allowed".to_string()
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with('.') || value.contains('/') || value.contains("\\")
}

fn file_path(input: &Value) -> Option<&str> {
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => {
            let mut output = String::from("'");
            for character in value.chars() {
                match character {
                    '\'' => output.push_str("\\'"),
                    '\\' => output.push_str("\\\\"),
                    '\0' => output.push_str("\\x00"),
                    '\n' => output.push_str("\\n"),
                    '\r' => output.push_str("\\r"),
                    '\t' => output.push_str("\\t"),
                    character if character.is_control() => {
                        output.push_str(&format!("\\x{:02x}", character as u32));
                    }
                    character => output.push(character),
                }
            }
            output.push('\'');
            output
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("'{key}': {}", python_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Number(value) => value.to_string(),
    }
}

#[derive(Clone, Copy)]
enum FileOperation {
    Read,
    Write,
    Edit,
}

fn execute_file(
    input: &Value,
    cwd: Option<&Path>,
    operation: FileOperation,
) -> std::io::Result<ToolResult> {
    let Some(raw_path) = file_path(input) else {
        return Ok(ToolResult::error("No file_path provided"));
    };
    let root = cwd.unwrap_or_else(|| Path::new("/"));
    let relative = resolve_workspace_relative(raw_path, root)?;
    let flags = match operation {
        FileOperation::Read | FileOperation::Edit => libc::O_RDONLY,
        FileOperation::Write => libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
    };
    if matches!(operation, FileOperation::Write) {
        secure_create_parents(root, relative.parent().unwrap_or_else(|| Path::new("")))?;
    }
    let mode = if flags & libc::O_CREAT != 0 { 0o666 } else { 0 };
    let mut file = secure_open(root, &relative, flags, mode)?;
    match operation {
        FileOperation::Read => {
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            Ok(ToolResult::ok(content))
        }
        FileOperation::Write => {
            let content = input.get("content").and_then(Value::as_str).unwrap_or("");
            file.write_all(content.as_bytes())?;
            Ok(ToolResult::ok(format!(
                "Wrote {} bytes to {raw_path}",
                content.len()
            )))
        }
        FileOperation::Edit => {
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            let old = input
                .get("old_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let Some(index) = content.find(old) else {
                return Ok(ToolResult::error(format!(
                    "old_string not found in {raw_path}"
                )));
            };
            content.replace_range(index..index + old.len(), new);
            drop(file);
            let mut file = secure_open(root, &relative, libc::O_WRONLY | libc::O_TRUNC, 0)?;
            file.write_all(content.as_bytes())?;
            Ok(ToolResult::ok(format!("Edited {raw_path}")))
        }
    }
}

fn execute_file_unconfined(
    input: &Value,
    cwd: Option<&Path>,
    operation: FileOperation,
) -> std::io::Result<ToolResult> {
    let Some(raw_path) = file_path(input) else {
        return Ok(ToolResult::error("No file_path provided"));
    };
    let expanded = expand_user(raw_path);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        cwd.unwrap_or_else(|| Path::new(".")).join(expanded)
    };
    match operation {
        FileOperation::Read => Ok(ToolResult::ok(std::fs::read_to_string(path)?)),
        FileOperation::Write => {
            let content = input.get("content").and_then(Value::as_str).unwrap_or("");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
            Ok(ToolResult::ok(format!(
                "Wrote {} bytes to {raw_path}",
                content.len()
            )))
        }
        FileOperation::Edit => {
            let mut content = std::fs::read_to_string(&path)?;
            let old = input
                .get("old_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let Some(index) = content.find(old) else {
                return Ok(ToolResult::error(format!(
                    "old_string not found in {raw_path}"
                )));
            };
            content.replace_range(index..index + old.len(), new);
            std::fs::write(path, content)?;
            Ok(ToolResult::ok(format!("Edited {raw_path}")))
        }
    }
}

fn expand_user(raw: &str) -> PathBuf {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(raw.trim_start_matches("~/"));
        }
    }
    PathBuf::from(raw)
}

fn resolve_workspace_relative(raw: &str, root: &Path) -> std::io::Result<PathBuf> {
    if raw.starts_with('~') && raw != "~" && !raw.starts_with("~/") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsupported user-home expansion",
        ));
    }
    let root = root.canonicalize()?;
    let expanded = expand_user(raw);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    };
    let normalized = normalize_lexically(&candidate)?;
    let mut existing = normalized.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path escapes workspace",
            )
        })?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path escapes workspace",
            )
        })?;
    }
    let mut resolved = existing.canonicalize()?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    resolved
        .strip_prefix(&root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path escapes workspace",
            )
        })
}

fn normalize_lexically(path: &Path) -> std::io::Result<PathBuf> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                output.push(component)
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "path escapes workspace",
                    ));
                }
            }
        }
    }
    Ok(output)
}

#[cfg(target_os = "linux")]
fn secure_open(root: &Path, relative: &Path, flags: i32, mode: u32) -> std::io::Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;
    let root = File::open(root)?;
    let relative =
        CString::new(relative.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: mode as u64,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: all pointers reference live values for the duration of the
    // syscall; the returned descriptor is owned exactly once by `File`.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as i32
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: `fd` is a fresh successful openat2 result.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
fn secure_create_parents(root: &Path, relative: &Path) -> std::io::Result<()> {
    let mut descriptors = vec![File::open(root)?];
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_encoded_bytes()).map_err(std::io::Error::other)?;
        let parent = descriptors.last().expect("root descriptor").as_raw_fd();
        // SAFETY: `parent` and `name` are live descriptors/strings. EEXIST is
        // expected; the following O_NOFOLLOW directory open validates it.
        let mkdir = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o777) };
        if mkdir != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        // SAFETY: openat returns a new descriptor owned once below.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh successful openat result.
        descriptors.push(unsafe { File::from_raw_fd(fd) });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn secure_open(_root: &Path, _relative: &Path, _flags: i32, _mode: u32) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure assistant file tools require Linux openat2",
    ))
}

#[cfg(not(target_os = "linux"))]
fn secure_create_parents(_root: &Path, _relative: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure assistant file tools require Linux descriptor-relative opens",
    ))
}

pub fn tools_schema() -> Value {
    json!([
        {"type":"function","function":{"name":"Bash","description":"Execute a shell command to query or interact with MLflow. Use 'mlflow' CLI commands or Python one-liners with the MLflow SDK.","parameters":{"type":"object","properties":{"command":{"type":"string","description":"The shell command to execute."}},"required":["command"]}}},
        {"type":"function","function":{"name":"Read","description":"Read the contents of a file.","parameters":{"type":"object","properties":{"file_path":{"type":"string","description":"Absolute or relative path to the file."}},"required":["file_path"]}}},
        {"type":"function","function":{"name":"Write","description":"Write content to a file (creates or overwrites).","parameters":{"type":"object","properties":{"file_path":{"type":"string","description":"Absolute or relative path to the file."},"content":{"type":"string","description":"Content to write."}},"required":["file_path","content"]}}},
        {"type":"function","function":{"name":"Edit","description":"Replace the first occurrence of old_string with new_string in a file.","parameters":{"type":"object","properties":{"file_path":{"type":"string","description":"Absolute or relative path to the file."},"old_string":{"type":"string","description":"Exact string to find."},"new_string":{"type":"string","description":"String to replace it with."}},"required":["file_path","old_string","new_string"]}}},
        {"type":"function","function":{"name":RENDER_CUSTOM_VIEW_TOOL_NAME,"description":"Render a custom trace view in the UI: a reusable, trace-agnostic layout of cards, stat tiles, key-value viewers, and assessment boards, built from the current trace's data. Call this once you've designed the layout; the client renders it and reports back whether it applied successfully.","parameters":{"type":"object","properties":{"title":{"type":"string","description":"Short display title for the view."},"messages":{"type":"array","description":"A2UI message list describing the view's component tree.","items":{"type":"object"}}},"required":["title","messages"]}}}
    ])
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn tool_executor_denial_matrix_matches_python() {
        let permissions = PermissionsConfig::default();
        let workspace = TempDir::new().unwrap();
        let cases = [
            (
                "Bash",
                json!([]),
                None,
                "Permission denied: malformed tool input",
            ),
            (
                "Bash",
                json!({"command":123}),
                None,
                "Permission denied: malformed command",
            ),
            (
                "Bash",
                json!({"command":"python3 -c 'print(1)'"}),
                None,
                "Permission denied: python3 requires a configured project directory",
            ),
            (
                "Read",
                json!({"file_path":"README.md"}),
                None,
                "Permission denied: Read requires a configured project directory",
            ),
            (
                "Write",
                json!({"file_path":"out"}),
                None,
                "Permission denied: Write requires a configured project directory",
            ),
            (
                "Read",
                json!({"file_path":[1]}),
                Some(workspace.path()),
                "Permission denied: malformed path [1]",
            ),
            (
                "Read",
                json!({"file_path":"foo\0bar"}),
                Some(workspace.path()),
                "Permission denied: malformed path 'foo\\x00bar'",
            ),
        ];
        for (tool, input, cwd, expected) in cases {
            assert_eq!(
                static_permission_error(tool, &input, &permissions, cwd).as_deref(),
                Some(expected),
                "{tool} {input}"
            );
        }
    }

    #[test]
    fn to_container_host_uri_table_matches_python() {
        for (input, expected) in [
            ("http://127.0.0.1:5000", "http://host.docker.internal:5000"),
            ("http://localhost:5000", "http://host.docker.internal:5000"),
            ("http://0.0.0.0:5000", "http://host.docker.internal:5000"),
            (
                "http://localhost:5000/path?exp=localhost",
                "http://host.docker.internal:5000/path?exp=localhost",
            ),
            ("http://[::1]:5000", "http://host.docker.internal:5000"),
            (
                "http://localhost.example.com:5000",
                "http://localhost.example.com:5000",
            ),
            (
                "http://localhost:notaport/api",
                "http://localhost:notaport/api",
            ),
            (
                "https://tracking.example.com",
                "https://tracking.example.com",
            ),
        ] {
            assert_eq!(to_container_host_uri(input), expected);
        }
        let userinfo = "tok:v";
        assert_eq!(
            to_container_host_uri(&format!("http://{userinfo}@127.0.0.1:5000/api")),
            format!("http://{userinfo}@host.docker.internal:5000/api")
        );
        assert_eq!(to_container_host_uri(""), "");
    }

    #[test]
    fn assistant_sandbox_tri_state_and_credential_filter_match_python() {
        assert!(assistant_sandbox_enabled_from(Some(true), false, || false));
        assert!(!assistant_sandbox_enabled_from(Some(false), true, || true));
        assert!(assistant_sandbox_enabled_from(None, true, || true));
        assert!(!assistant_sandbox_enabled_from(None, true, || false));
        assert!(!assistant_sandbox_enabled_from(None, false, || true));

        assert_eq!(
            uri_without_credentials("http://127.0.0.1:5000"),
            Some("http://127.0.0.1:5000")
        );
        let credentials = "u:p";
        assert_eq!(
            uri_without_credentials(&format!(
                "postgresql://{credentials}@db.internal:5432/tracking"
            )),
            None
        );
    }

    #[test]
    fn sandbox_docker_argv_has_hardening_and_workspace_flags() {
        let args = sandbox_docker_argv(
            "fixture",
            "custom:image",
            &["mlflow".to_string(), "--version".to_string()],
            Some(Path::new("/project")),
            &[("MLFLOW_TRACKING_URI".to_string(), "http://host".to_string())],
        );
        let joined = args.join(" ");
        for flag in [
            "--network bridge",
            "--add-host host.docker.internal:host-gateway",
            "--memory 1g",
            "--memory-swap 1g",
            "--cpus 1",
            "--pids-limit 256",
            "--read-only",
            "--cap-drop ALL",
            "--security-opt no-new-privileges:true",
            "--tmpfs /tmp",
            "--volume /project:/workspace:rw",
            "--workdir /workspace",
            "--env HOME=/tmp",
        ] {
            assert!(joined.contains(flag), "missing {flag}: {joined}");
        }
        let environment = args
            .windows(2)
            .filter_map(|pair| (pair[0] == "--env").then_some(pair[1].as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            environment,
            ["HOME=/tmp", "MLFLOW_TRACKING_URI=http://host"]
        );
        assert!(joined.ends_with("custom:image mlflow --version"));
    }

    #[test]
    fn descriptor_open_rejects_directory_relink_after_policy_resolution() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("workspace");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("target"), "sentinel").unwrap();
        std::fs::create_dir(root.join("slot")).unwrap();
        std::fs::write(root.join("slot/target"), "inside").unwrap();

        let resolved = resolve_workspace_relative("slot/target", &root).unwrap();
        std::fs::rename(root.join("slot"), root.join("old-slot")).unwrap();
        symlink(&outside, root.join("slot")).unwrap();

        assert!(secure_open(&root, &resolved, libc::O_WRONLY | libc::O_TRUNC, 0).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("target")).unwrap(),
            "sentinel"
        );
    }

    #[test]
    fn descriptor_open_rejects_file_symlink_swap_after_policy_resolution() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let outside = fixture.path().join("outside");
        std::fs::write(&outside, "sentinel").unwrap();
        std::fs::write(root.join("target"), "inside").unwrap();

        let resolved = resolve_workspace_relative("target", &root).unwrap();
        std::fs::rename(root.join("target"), root.join("old-target")).unwrap();
        symlink(&outside, root.join("target")).unwrap();

        assert!(secure_open(&root, &resolved, libc::O_WRONLY | libc::O_TRUNC, 0).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel");
    }
}
