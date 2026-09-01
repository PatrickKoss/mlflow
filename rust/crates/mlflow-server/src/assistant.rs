//! Native MLflow Assistant HTTP surface (plan T20.1, §12.10).
//!
//! This module owns the localhost gate, file-backed session/config state, SSE
//! framing, and the provider integration seam. CLI process execution belongs
//! to T20.2 and plugs in through [`AssistantProvider`].

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Debug;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Extension, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::Router;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use mlflow_store::{python_json_dumps, EndpointModelConfig, TrackingStore, WORKSPACE_DEFAULT_NAME};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::assistant_custom_view::{custom_view_response_events, CustomViewResponse};
use crate::assistant_providers::{self, ProviderKind};
use crate::openai_compatible::{self, Preset};
use crate::state::AppState;

const PREFIX: &str = "/ajax-api/3.0/mlflow/assistant";
const REMOTE_ACCESS_DETAIL: &str =
    "Assistant API is only accessible from the same host where the MLflow server is running.";
const NO_PROVIDER_DETAIL: &str = "No assistant provider is configured or available.";
const DEV_STUB_ENV: &str = "MLFLOW_ASSISTANT_DEV_STUB_PROVIDERS";
const REMOTE_ASSISTANT_ENV: &str = "MLFLOW_ENABLE_REMOTE_ASSISTANT";
const DEV_STUB_REPLY: &str = "This is a synthetic reply from the MLflow dev stub Claude CLI. The real Claude Code provider is replaced so the Assistant chat panel can be reviewed without credentials or LLM calls. No model was invoked to produce this message.";
const GATEWAY_PROVIDER: &str = "mlflow_gateway";
const GATEWAY_MANAGED_PREFIX: &str = "mlflow-assistant-";
const GATEWAY_UNSUPPORTED_DETAIL: &str = "This MLflow server's tracking backend does not support the AI Gateway. Assistant-managed LLM Connections require a database-backed tracking store.";
const DEFAULT_PROVIDER_ORDER: &[&str] = &["claude_code", "codex", GATEWAY_PROVIDER];
const GATEWAY_VENDOR_MODELS: &[(&str, &str)] = &[
    ("openai", "gpt-5.5"),
    ("anthropic", "claude-sonnet-5"),
    ("gemini", "gemini-3-pro"),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantSession {
    #[serde(default)]
    pub context: Map<String, Value>,
    #[serde(default)]
    pub messages: Vec<AssistantMessage>,
    #[serde(default)]
    pub pending_message: Option<AssistantMessage>,
    #[serde(default)]
    pub provider_session_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub pending_tool_decisions: Map<String, Value>,
    #[serde(default)]
    pub pending_client_tool_results: Map<String, Value>,
}

impl Default for AssistantSession {
    fn default() -> Self {
        Self {
            context: Map::new(),
            messages: Vec::new(),
            pending_message: None,
            provider_session_id: None,
            working_dir: None,
            pending_tool_decisions: Map::new(),
            pending_client_tool_results: Map::new(),
        }
    }
}

/// Python-compatible file store rooted at `$TMPDIR/mlflow-assistant-sessions`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &FsPath {
        &self.root
    }

    pub fn validate_session_id(session_id: &str) -> Result<(), String> {
        Uuid::parse_str(session_id)
            .map(|_| ())
            .map_err(|_| "Invalid session ID format".to_string())
    }

    pub fn session_file(&self, session_id: &str) -> Result<PathBuf, String> {
        Self::validate_session_id(session_id)?;
        Ok(self.root.join(format!("{session_id}.json")))
    }

    pub fn process_file(&self, session_id: &str) -> Result<PathBuf, String> {
        Self::validate_session_id(session_id)?;
        Ok(self.root.join(format!("{session_id}.process.json")))
    }

    pub fn save(&self, session_id: &str, session: &AssistantSession) -> std::io::Result<()> {
        let destination = self
            .session_file(session_id)
            .map_err(std::io::Error::other)?;
        fs::create_dir_all(&self.root)?;
        let value = serde_json::to_value(session).map_err(std::io::Error::other)?;
        self.atomic_write(&destination, python_json_dumps(&value, false).as_bytes())
    }

    pub fn load(&self, session_id: &str) -> std::io::Result<Option<AssistantSession>> {
        let path = match self.session_file(session_id) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(std::io::Error::other)
    }

    pub fn save_process_pid(&self, session_id: &str, pid: i32) -> std::io::Result<()> {
        let path = self
            .process_file(session_id)
            .map_err(std::io::Error::other)?;
        fs::create_dir_all(&self.root)?;
        fs::write(path, format!("{{\"pid\": {pid}}}"))
    }

    pub fn process_pid(&self, session_id: &str) -> std::io::Result<Option<i32>> {
        let path = match self.process_file(session_id) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        Ok(value
            .get("pid")
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok()))
    }

    pub fn clear_process_pid(&self, session_id: &str) -> std::io::Result<()> {
        let path = match self.process_file(session_id) {
            Ok(path) => path,
            Err(_) => return Ok(()),
        };
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn terminate_process(&self, session_id: &str) -> std::io::Result<bool> {
        let Some(pid) = self.process_pid(session_id)? else {
            return Ok(false);
        };
        if pid == 0 {
            return Ok(false);
        }
        // SAFETY: `kill` receives a scalar PID read from the validated session's
        // process file and does not dereference memory.
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        let error = std::io::Error::last_os_error();
        self.clear_process_pid(session_id)?;
        if result == 0 {
            Ok(true)
        } else if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EPERM)) {
            Ok(false)
        } else {
            Err(error)
        }
    }

    fn atomic_write(&self, destination: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
        for _ in 0..100 {
            let temporary = self.root.join(format!("{}.tmp", Uuid::new_v4().simple()));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary);
            let mut file = match file {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let result = (|| {
                file.write_all(bytes)?;
                file.flush()?;
                drop(file);
                fs::rename(&temporary, destination)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            return result;
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate assistant session temporary file",
        ))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantEvent {
    pub event_type: String,
    pub data: Value,
}

impl AssistantEvent {
    pub fn new(event_type: impl Into<String>, data: Value) -> Self {
        Self {
            event_type: event_type.into(),
            data,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self::error_with_session(error, None)
    }

    pub fn error_with_session(error: impl Into<String>, session_id: Option<&str>) -> Self {
        let error = error.into();
        let mut data = Map::from_iter([(
            "error".to_string(),
            Value::String(if error.is_empty() {
                "Exception()".to_string()
            } else {
                error
            }),
        )]);
        if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) {
            data.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
        }
        Self::new("error", Value::Object(data))
    }

    pub fn client_tool_call(
        request_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_input: Value,
        terminal: bool,
    ) -> Self {
        let mut data = Map::from_iter([
            ("request_id".to_string(), Value::String(request_id.into())),
            ("tool_name".to_string(), Value::String(tool_name.into())),
            ("tool_input".to_string(), tool_input),
        ]);
        if terminal {
            data.insert(
                "continuation".to_string(),
                Value::String("terminal".to_string()),
            );
        }
        Self::new("client_tool_call", Value::Object(data))
    }

    pub fn to_sse(&self) -> Bytes {
        Bytes::from(format!(
            "event: {}\ndata: {}\n\n",
            self.event_type,
            python_json_dumps(&self.data, false)
        ))
    }
}

#[derive(Debug, Clone)]
pub struct AssistantProviderRequest {
    pub prompt: String,
    pub tracking_uri: String,
    pub session_id: Option<String>,
    pub mlflow_session_id: String,
    pub cwd: Option<PathBuf>,
    pub context: Map<String, Value>,
    pub config: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantProviderError {
    NotImplemented(String),
    CliNotInstalled(String),
    NotAuthenticated(String),
    NotConfigured(String),
    Internal(String),
}

/// Minimal provider contract shared with T20.2. Implementations own provider
/// execution; this module retains HTTP status mapping and SSE framing.
pub trait AssistantProvider: Send + Sync + Debug {
    fn name(&self) -> &str;
    fn display_name(&self) -> &str {
        self.name()
    }
    fn description(&self) -> &str {
        ""
    }
    fn is_available(
        &self,
        _tracking_store: TrackingStore,
        _config: Option<Value>,
    ) -> BoxFuture<'static, bool> {
        async { true }.boxed()
    }
    fn allows_remote_access(&self) -> bool {
        false
    }
    fn client_tool_delivery(&self) -> &'static str {
        "unsupported"
    }
    fn resolve_skills_path(&self, base_directory: &FsPath) -> PathBuf;
    fn check_connection(
        &self,
        config: Option<Value>,
    ) -> BoxFuture<'static, Result<(), AssistantProviderError>>;
    fn list_models(
        &self,
        base_url: Option<String>,
        api_key: Option<String>,
        config: Option<Value>,
    ) -> BoxFuture<'static, Result<Vec<String>, AssistantProviderError>>;
    fn stream(&self, request: AssistantProviderRequest) -> BoxStream<'static, AssistantEvent>;
}

#[derive(Debug, Clone)]
pub struct AssistantRuntime {
    inner: Arc<AssistantRuntimeInner>,
}

#[derive(Debug)]
struct AssistantRuntimeInner {
    sessions: SessionStore,
    config_path: PathBuf,
    skills_source: PathBuf,
    home: PathBuf,
    providers: Vec<Arc<dyn AssistantProvider>>,
}

impl AssistantRuntime {
    pub fn new(
        session_root: PathBuf,
        config_path: PathBuf,
        skills_source: PathBuf,
        home: PathBuf,
        providers: Vec<Arc<dyn AssistantProvider>>,
    ) -> Self {
        Self {
            inner: Arc::new(AssistantRuntimeInner {
                sessions: SessionStore::new(session_root),
                config_path,
                skills_source,
                home,
                providers,
            }),
        }
    }

    pub fn from_env() -> Self {
        let home = home_dir();
        let session_root = std::env::temp_dir().join("mlflow-assistant-sessions");
        let sessions = SessionStore::new(session_root.clone());
        let config_path = home.join(".mlflow/assistant/config.json");
        let skills_source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../mlflow/assistant/skills");
        let dev_stubs = std::env::var(DEV_STUB_ENV).unwrap_or_default();
        let stub_claude = dev_stubs
            .split(',')
            .map(str::trim)
            .any(|name| name == "claude");
        let providers: Vec<Arc<dyn AssistantProvider>> = vec![
            if stub_claude {
                Arc::new(DevClaudeProvider) as Arc<dyn AssistantProvider>
            } else {
                Arc::new(CliProvider::new(ProviderKind::ClaudeCode, sessions.clone()))
                    as Arc<dyn AssistantProvider>
            },
            Arc::new(CliProvider::new(ProviderKind::Codex, sessions)),
            Arc::new(BuiltinProvider::gateway()),
            Arc::new(BuiltinProvider::ollama()),
        ];
        Self::new(session_root, config_path, skills_source, home, providers)
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.inner.sessions
    }

    fn provider(&self, name: &str) -> Option<Arc<dyn AssistantProvider>> {
        self.inner
            .providers
            .iter()
            .find(|provider| provider.name() == name)
            .cloned()
    }

    fn selected_provider(&self, config: &AssistantConfig) -> Option<Arc<dyn AssistantProvider>> {
        config.providers.iter().find_map(|(name, value)| {
            (value.get("selected").and_then(Value::as_bool) == Some(true))
                .then(|| self.provider(name))
                .flatten()
        })
    }

    async fn resolve_default_provider(
        &self,
        config: &AssistantConfig,
        tracking_store: &TrackingStore,
        remote: bool,
        include_gateway: bool,
    ) -> Option<Arc<dyn AssistantProvider>> {
        for name in DEFAULT_PROVIDER_ORDER {
            if !include_gateway && *name == GATEWAY_PROVIDER {
                continue;
            }
            let Some(provider) = self.provider(name) else {
                continue;
            };
            if remote && !provider.allows_remote_access() {
                continue;
            }
            if provider
                .is_available(tracking_store.clone(), config.providers.get(*name).cloned())
                .await
            {
                return Some(provider);
            }
        }
        None
    }

    async fn resolve_provider(
        &self,
        config: &AssistantConfig,
        tracking_store: &TrackingStore,
        remote: bool,
    ) -> Option<Arc<dyn AssistantProvider>> {
        if let Some(provider) = self.selected_provider(config) {
            return (!remote || provider.allows_remote_access()).then_some(provider);
        }
        self.resolve_default_provider(config, tracking_store, remote, true)
            .await
    }

    fn load_config(&self) -> AssistantConfig {
        AssistantConfig::load(&self.inner.config_path)
    }

    fn save_config(&self, config: &AssistantConfig) -> std::io::Result<()> {
        config.save(&self.inner.config_path)
    }
}

impl Default for AssistantRuntime {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Debug, Clone, Default)]
struct AssistantConfig {
    projects: Map<String, Value>,
    providers: Map<String, Value>,
}

impl AssistantConfig {
    fn load(path: &FsPath) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Self::default();
        };
        let Some(object) = value.as_object() else {
            return Self::default();
        };
        let projects = match object.get("projects") {
            None => Map::new(),
            Some(Value::Object(projects)) => projects.clone(),
            Some(_) => return Self::default(),
        };
        let providers = match object.get("providers") {
            None => Map::new(),
            Some(Value::Object(providers)) => providers.clone(),
            Some(_) => return Self::default(),
        };
        let mut normalized_projects = Map::new();
        for (name, project) in &projects {
            let Some(project) = normalize_project(project) else {
                return Self::default();
            };
            normalized_projects.insert(name.clone(), project);
        }
        let mut normalized_providers = Map::new();
        for (name, provider) in &providers {
            let Some(provider) = normalize_provider(provider) else {
                return Self::default();
            };
            normalized_providers.insert(name.clone(), provider);
        }
        Self {
            projects: normalized_projects,
            providers: normalized_providers,
        }
    }

    fn save(&self, path: &FsPath) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let value = json!({"projects": self.projects, "providers": self.providers});
        let body = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
        fs::write(path, body)
    }

    fn response_value(
        &self,
        is_localhost: bool,
        remote_access_allowed: bool,
        synthesized_provider: Option<&str>,
    ) -> Value {
        let mut providers = self.providers.clone();
        if let Some(name) = synthesized_provider {
            let provider = providers
                .entry(name.to_string())
                .or_insert_with(default_provider);
            provider
                .as_object_mut()
                .expect("normalized provider")
                .insert("selected".to_string(), Value::Bool(true));
        }
        for provider in providers.values_mut() {
            if let Some(provider) = provider.as_object_mut() {
                provider.shift_remove("api_key");
            }
        }
        let mut projects = self.projects.clone();
        if !is_localhost {
            for project in projects.values_mut() {
                if let Some(project) = project.as_object_mut() {
                    project.shift_remove("location");
                }
            }
        }
        json!({
            "providers": providers,
            "projects": projects,
            "remote_access_allowed": remote_access_allowed,
        })
    }

    fn project_path(&self, experiment_id: &str) -> Option<PathBuf> {
        self.projects
            .get(experiment_id)
            .and_then(|project| project.get("location"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }
}

#[derive(Debug, Clone, Copy)]
enum BuiltinKind {
    Gateway,
    Ollama,
}

#[derive(Debug)]
struct BuiltinProvider {
    name: &'static str,
    skills_dir: &'static str,
    kind: BuiltinKind,
}

impl BuiltinProvider {
    fn gateway() -> Self {
        Self {
            name: "mlflow_gateway",
            skills_dir: ".agent",
            kind: BuiltinKind::Gateway,
        }
    }

    fn ollama() -> Self {
        Self {
            name: "ollama",
            skills_dir: ".agent",
            kind: BuiltinKind::Ollama,
        }
    }
}

impl AssistantProvider for BuiltinProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn display_name(&self) -> &str {
        match self.kind {
            BuiltinKind::Gateway => "MLflow AI Gateway",
            BuiltinKind::Ollama => "Ollama",
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            BuiltinKind::Gateway => {
                "AI-powered assistant backed by an MLflow AI Gateway endpoint configured on this server."
            }
            BuiltinKind::Ollama => {
                "AI-powered assistant using a locally running Ollama server."
            }
        }
    }

    fn is_available(
        &self,
        tracking_store: TrackingStore,
        _config: Option<Value>,
    ) -> BoxFuture<'static, bool> {
        let kind = self.kind;
        async move {
            match kind {
                BuiltinKind::Gateway => tracking_store
                    .list_gateway_endpoints(WORKSPACE_DEFAULT_NAME, None, None)
                    .await
                    .is_ok_and(|endpoints| !endpoints.is_empty()),
                BuiltinKind::Ollama => true,
            }
        }
        .boxed()
    }

    fn allows_remote_access(&self) -> bool {
        matches!(self.kind, BuiltinKind::Gateway)
    }

    fn client_tool_delivery(&self) -> &'static str {
        "tool"
    }

    fn resolve_skills_path(&self, base_directory: &FsPath) -> PathBuf {
        base_directory.join(self.skills_dir).join("skills")
    }

    fn check_connection(
        &self,
        config: Option<Value>,
    ) -> BoxFuture<'static, Result<(), AssistantProviderError>> {
        let kind = self.kind;
        async move {
            match kind {
                BuiltinKind::Gateway => Err(AssistantProviderError::NotImplemented(
                    "MLflow AI Gateway connection is verified by the frontend; the assistant backend has no probe to run.".to_string(),
                )),
                BuiltinKind::Ollama => {
                    let base = config
                        .as_ref()
                        .and_then(|value| value.get("base_url"))
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .unwrap_or("http://localhost:11434");
                    let result = reqwest::Client::new()
                        .get(format!("{}/api/tags", base.trim_end_matches('/')))
                        .send()
                        .await;
                    match result {
                        Ok(response) if response.status().is_success() => Ok(()),
                        _ => Err(AssistantProviderError::NotAuthenticated(format!(
                            "Cannot connect to Ollama at {base}. Make sure Ollama is running: ollama serve"
                        ))),
                    }
                }
            }
        }
        .boxed()
    }

    fn list_models(
        &self,
        base_url: Option<String>,
        api_key: Option<String>,
        config: Option<Value>,
    ) -> BoxFuture<'static, Result<Vec<String>, AssistantProviderError>> {
        let kind = self.kind;
        async move {
            if !matches!(kind, BuiltinKind::Ollama) {
                return Err(AssistantProviderError::NotImplemented(String::new()));
            }
            let base = base_url
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    config
                        .as_ref()
                        .and_then(|value| value.get("base_url"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let mut request =
                reqwest::Client::new().get(format!("{}/api/tags", base.trim_end_matches('/')));
            if let Some(api_key) = api_key.filter(|value| !value.is_empty()) {
                request = request.bearer_auth(api_key);
            }
            let response = request.send().await.map_err(|error| {
                AssistantProviderError::NotConfigured(format!(
                    "Cannot connect to Ollama at {base}: {error}"
                ))
            })?;
            let response = response.error_for_status().map_err(|error| {
                AssistantProviderError::NotConfigured(format!(
                    "Cannot connect to Ollama at {base}: {error}"
                ))
            })?;
            let body: Value = response.json().await.map_err(|error| {
                AssistantProviderError::NotConfigured(format!(
                    "Cannot connect to Ollama at {base}: {error}"
                ))
            })?;
            Ok(body
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|model| model.get("model").and_then(Value::as_str))
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .collect())
        }
        .boxed()
    }

    fn stream(&self, request: AssistantProviderRequest) -> BoxStream<'static, AssistantEvent> {
        let value = request.config.as_ref();
        let permissions = value
            .and_then(|value| value.get("permissions"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let config = openai_compatible::Config {
            preset: match self.kind {
                BuiltinKind::Gateway => Preset::MlflowGateway,
                BuiltinKind::Ollama => Preset::Ollama,
            },
            model: value
                .and_then(|value| value.get("model"))
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_string(),
            base_url: value
                .and_then(|value| value.get("base_url"))
                .and_then(Value::as_str)
                .map(str::to_string),
            api_key: None,
            permissions,
        };
        openai_compatible::stream(config, request)
    }
}

#[derive(Debug)]
struct CliProvider {
    kind: ProviderKind,
    sessions: SessionStore,
}

impl CliProvider {
    fn new(kind: ProviderKind, sessions: SessionStore) -> Self {
        Self { kind, sessions }
    }

    fn config(value: Option<Value>) -> assistant_providers::ProviderConfig {
        value
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }
}

impl AssistantProvider for CliProvider {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn display_name(&self) -> &str {
        match self.kind {
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Codex => "Codex",
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            ProviderKind::ClaudeCode => "AI-powered assistant using Claude Code CLI",
            ProviderKind::Codex => "AI-powered assistant using the Codex CLI",
        }
    }

    fn client_tool_delivery(&self) -> &'static str {
        "structured"
    }

    fn is_available(
        &self,
        _tracking_store: TrackingStore,
        _config: Option<Value>,
    ) -> BoxFuture<'static, bool> {
        let kind = self.kind;
        async move { assistant_providers::is_available(kind) }.boxed()
    }

    fn resolve_skills_path(&self, base_directory: &FsPath) -> PathBuf {
        let directory = match self.kind {
            ProviderKind::ClaudeCode => ".claude",
            ProviderKind::Codex => ".codex",
        };
        base_directory.join(directory).join("skills")
    }

    fn check_connection(
        &self,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<(), AssistantProviderError>> {
        let kind = self.kind;
        async move {
            assistant_providers::health(kind)
                .await
                .map_err(|error| match error {
                    assistant_providers::HealthError::NotImplemented(detail) => {
                        AssistantProviderError::NotImplemented(detail)
                    }
                    assistant_providers::HealthError::CliNotInstalled(detail) => {
                        AssistantProviderError::CliNotInstalled(detail)
                    }
                    assistant_providers::HealthError::NotAuthenticated(detail) => {
                        AssistantProviderError::NotAuthenticated(detail)
                    }
                })
        }
        .boxed()
    }

    fn list_models(
        &self,
        _base_url: Option<String>,
        _api_key: Option<String>,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<Vec<String>, AssistantProviderError>> {
        let name = self.kind.name();
        async move {
            Err(AssistantProviderError::NotImplemented(format!(
                "Model listing is not supported for provider '{name}'"
            )))
        }
        .boxed()
    }

    fn stream(&self, request: AssistantProviderRequest) -> BoxStream<'static, AssistantEvent> {
        let kind = self.kind;
        let sessions = self.sessions.clone();
        let session_id = request.mlflow_session_id.clone();
        let provider_config = Self::config(request.config);
        let provider_request = assistant_providers::StreamRequest {
            prompt: request.prompt,
            tracking_uri: request.tracking_uri,
            session_id: request.session_id,
            cwd: request.cwd,
            context: Some(request.context),
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            match assistant_providers::spawn(kind, provider_config, provider_request).await {
                Ok(mut spawned) => {
                    let _ = sessions.save_process_pid(&session_id, spawned.handle.pid() as i32);
                    while let Some(event) = spawned.events.next().await {
                        let mapped = AssistantEvent::new(event.event_type.as_str(), event.data);
                        if sender.send(mapped).await.is_err() {
                            break;
                        }
                    }
                    let _ = sessions.clear_process_pid(&session_id);
                }
                Err(error) => {
                    let _ = sender.send(AssistantEvent::error(error.to_string())).await;
                }
            }
        });
        stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed()
    }
}

#[derive(Debug)]
struct DevClaudeProvider;

impl AssistantProvider for DevClaudeProvider {
    fn name(&self) -> &str {
        "claude_code"
    }

    fn display_name(&self) -> &str {
        "Claude Code"
    }

    fn description(&self) -> &str {
        "AI-powered assistant using Claude Code CLI"
    }

    fn client_tool_delivery(&self) -> &'static str {
        "structured"
    }

    fn resolve_skills_path(&self, base_directory: &FsPath) -> PathBuf {
        base_directory.join(".claude/skills")
    }

    fn check_connection(
        &self,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<(), AssistantProviderError>> {
        async { Ok(()) }.boxed()
    }

    fn list_models(
        &self,
        _base_url: Option<String>,
        _api_key: Option<String>,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<Vec<String>, AssistantProviderError>> {
        async { Err(AssistantProviderError::NotImplemented(String::new())) }.boxed()
    }

    fn stream(&self, request: AssistantProviderRequest) -> BoxStream<'static, AssistantEvent> {
        let session_id = request
            .session_id
            .unwrap_or_else(|| format!("mlflow-dev-stub-{}", Uuid::new_v4().simple()));
        if request
            .context
            .get("customTraceView")
            .is_some_and(|value| value.as_bool().unwrap_or(!value.is_null()))
        {
            let mut events = custom_view_response_events(CustomViewResponse {
                response_type: "render_custom_view".to_string(),
                text: "Created a synthetic Custom View.".to_string(),
                title: "Synthetic Custom View".to_string(),
                messages: vec![json!({"beginRendering":{"surfaceId":"main"}})],
            });
            events.push(AssistantEvent::new(
                "done",
                json!({"result":Value::Null,"session_id":session_id}),
            ));
            return stream::iter(events).boxed();
        }
        stream::iter(vec![
            AssistantEvent::new(
                "message",
                json!({"message": {"role": "assistant", "content": [{"text": DEV_STUB_REPLY}]}}),
            ),
            AssistantEvent::new(
                "stream_event",
                json!({"event": {"type": "usage", "usage": {"prompt_tokens": 8, "completion_tokens": 24, "total_tokens": 32, "cache_read_tokens": 0, "total_cost_usd": 0.0}}}),
            ),
            AssistantEvent::new(
                "done",
                json!({"result": DEV_STUB_REPLY, "session_id": session_id}),
            ),
        ])
        .boxed()
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(&format!("{PREFIX}/message"), post(send_message))
        .route(
            &format!("{PREFIX}/sessions/{{session_id}}/stream"),
            get(stream_response),
        )
        .route(
            &format!("{PREFIX}/sessions/{{session_id}}"),
            patch(patch_session),
        )
        .route(
            &format!("{PREFIX}/sessions/{{session_id}}/permission"),
            post(resolve_permission),
        )
        .route(
            &format!("{PREFIX}/sessions/{{session_id}}/tool-result"),
            post(resolve_client_tool_result),
        )
        .route(
            &format!("{PREFIX}/providers/{{provider}}/health"),
            get(provider_health),
        )
        .route(&format!("{PREFIX}/providers"), get(get_providers))
        .route(
            &format!("{PREFIX}/config"),
            get(get_config).put(update_config),
        )
        .route(
            &format!("{PREFIX}/skills/install"),
            post(install_skills_endpoint),
        )
        .route(
            &format!("{PREFIX}/providers/{{provider}}/models"),
            get(list_provider_models),
        )
        .route_layer(middleware::from_fn(classify_assistant_client))
}

#[derive(Debug, Clone, Copy)]
struct AssistantClient {
    is_localhost: bool,
}

async fn classify_assistant_client(mut request: Request, next: Next) -> Response {
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    // Uvicorn trusts X-Forwarded-For only from its default trusted proxy,
    // 127.0.0.1. Mirror that behavior for a same-host reverse proxy without
    // allowing a remote peer to spoof a loopback address.
    let effective_ip = peer_ip.and_then(|peer_ip| {
        if peer_ip.is_loopback() {
            request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .and_then(|value| value.trim().parse().ok())
                .or(Some(peer_ip))
        } else {
            Some(peer_ip)
        }
    });
    request.extensions_mut().insert(AssistantClient {
        is_localhost: effective_ip.is_some_and(|ip| ip.is_loopback()),
    });
    next.run(request).await
}

fn remote_assistant_enabled() -> Result<bool, ()> {
    match std::env::var(REMOTE_ASSISTANT_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(std::env::VarError::NotUnicode(_)) => Err(()),
        Ok(value) if value.eq_ignore_ascii_case("true") || value == "1" => Ok(true),
        Ok(value) if value.eq_ignore_ascii_case("false") || value == "0" => Ok(false),
        Ok(_) => Err(()),
    }
}

fn enforce_local_only(client: AssistantClient) -> Option<Response> {
    (!client.is_localhost).then(|| detail_response(StatusCode::FORBIDDEN, REMOTE_ACCESS_DETAIL))
}

fn enforce_remote_opt_in(client: AssistantClient) -> Option<Response> {
    if client.is_localhost {
        return None;
    }
    match remote_assistant_enabled() {
        Ok(true) => None,
        Ok(false) => Some(detail_response(StatusCode::FORBIDDEN, REMOTE_ACCESS_DETAIL)),
        Err(()) => Some(internal_error()),
    }
}

fn enforce_provider_remote_access(
    client: AssistantClient,
    provider: Option<&dyn AssistantProvider>,
) -> Option<Response> {
    (!client.is_localhost && !provider.is_some_and(AssistantProvider::allows_remote_access))
        .then(|| detail_response(StatusCode::FORBIDDEN, REMOTE_ACCESS_DETAIL))
}

async fn send_message(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    body: Bytes,
) -> Response {
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let provider = runtime
        .resolve_provider(&config, state.tracking_store(), !client.is_localhost)
        .await;
    if let Some(response) = enforce_provider_remote_access(client, provider.as_deref()) {
        return response;
    }
    let request = match parse_object_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let message = match required_string(&request, "message") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let session_id = match optional_string(&request, "session_id") {
        Ok(Some(value)) if !value.is_empty() => value,
        Ok(_) => Uuid::new_v4().to_string(),
        Err(response) => return *response,
    };
    let experiment_id = match optional_string(&request, "experiment_id") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let context = match request.get("context") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(value)) => value.clone(),
        Some(value) => {
            return field_type_error(
                "context",
                "dict_type",
                "Input should be a valid dictionary",
                value.clone(),
            )
        }
    };
    let working_dir = experiment_id
        .as_deref()
        .and_then(|experiment_id| config.project_path(experiment_id));
    let mut session = match runtime.sessions().load(&session_id) {
        Ok(Some(session)) => session,
        Ok(None) => AssistantSession {
            context: context.clone(),
            working_dir: working_dir.clone(),
            ..Default::default()
        },
        Err(_) => return internal_error(),
    };
    if !context.contains_key("customTraceView") {
        session.context.shift_remove("customTraceView");
    }
    if !context.is_empty() && !session.context.is_empty() {
        session.context.extend(context);
    } else if !context.is_empty() {
        session.context = context;
    }
    if session.working_dir.is_none() && working_dir.is_some() {
        session.working_dir = working_dir;
    }
    let pending = AssistantMessage {
        role: "user".to_string(),
        content: Value::String(message),
    };
    session.pending_message = Some(pending.clone());
    session.messages.push(pending);
    if runtime.sessions().save(&session_id, &session).is_err() {
        return internal_error();
    }
    json_response(
        StatusCode::OK,
        json!({"session_id": session_id, "stream_url": format!("{PREFIX}/sessions/{session_id}/stream")}),
    )
}

async fn stream_response(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let runtime = state.assistant_runtime().clone();
    let config = runtime.load_config();
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let provider = runtime
        .resolve_provider(&config, state.tracking_store(), !client.is_localhost)
        .await;
    if let Some(response) = enforce_provider_remote_access(client, provider.as_deref()) {
        return response;
    }
    let mut session = match runtime.sessions().load(&session_id) {
        Ok(Some(session)) => session,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Session not found"),
        Err(_) => return internal_error(),
    };
    let pending = session.pending_message.take();
    let decisions = std::mem::take(&mut session.pending_tool_decisions);
    let client_results = std::mem::take(&mut session.pending_client_tool_results);
    if pending.is_none() && decisions.is_empty() && client_results.is_empty() {
        return detail_response(StatusCode::BAD_REQUEST, "No pending message to process");
    }
    if runtime.sessions().save(&session_id, &session).is_err() {
        return internal_error();
    }
    let prompt = pending
        .as_ref()
        .and_then(|message| message.content.as_str())
        .unwrap_or("")
        .to_string();
    let mut context = session.context.clone();
    if pending.is_none() && !decisions.is_empty() {
        context.insert("tool_decisions".to_string(), Value::Object(decisions));
    }
    if pending.is_none() && !client_results.is_empty() {
        context.insert(
            "client_tool_results".to_string(),
            Value::Object(client_results),
        );
    }
    let tracking_uri = tracking_uri(&headers);
    let source: BoxStream<'static, AssistantEvent> = match provider {
        Some(provider) => {
            let provider_config = config.providers.get(provider.name()).cloned();
            provider.stream(AssistantProviderRequest {
                prompt,
                tracking_uri,
                session_id: session.provider_session_id.clone(),
                mlflow_session_id: session_id.clone(),
                cwd: session.working_dir.clone(),
                context,
                config: provider_config,
            })
        }
        None => stream::once(async { AssistantEvent::error(NO_PROVIDER_DETAIL) }).boxed(),
    };
    let output = source.then(move |event| {
        let runtime = runtime.clone();
        let session_id = session_id.clone();
        let mut session = session.clone();
        async move {
            if matches!(event.event_type.as_str(), "done" | "error") {
                if let Some(provider_session_id) =
                    event.data.get("session_id").and_then(Value::as_str)
                {
                    session.provider_session_id = Some(provider_session_id.to_string());
                    let _ = runtime.sessions().save(&session_id, &session);
                }
            }
            Ok::<_, Infallible>(event.to_sse())
        }
    });
    let mut response = Response::new(Body::from_stream(output));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

async fn patch_session(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Response {
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let provider = runtime
        .resolve_provider(&config, state.tracking_store(), !client.is_localhost)
        .await;
    if let Some(response) = enforce_provider_remote_access(client, provider.as_deref()) {
        return response;
    }
    let request = match parse_object_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match request.get("status") {
        None => return missing_field("status", Value::Object(request)),
        Some(Value::String(status)) if status == "cancelled" => {}
        Some(value) => return literal_error("status", "'cancelled'", value.clone()),
    }
    let mut session = match runtime.sessions().load(&session_id) {
        Ok(Some(session)) => session,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Session not found"),
        Err(_) => return internal_error(),
    };
    session.pending_tool_decisions.clear();
    session.pending_client_tool_results.clear();
    if runtime.sessions().save(&session_id, &session).is_err() {
        return internal_error();
    }
    let terminated = runtime
        .sessions()
        .terminate_process(&session_id)
        .unwrap_or(false);
    let message = if terminated {
        "Session cancelled and process terminated"
    } else {
        "Session cancelled"
    };
    json_response(StatusCode::OK, json!({"message": message}))
}

async fn resolve_permission(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Response {
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let provider = runtime
        .resolve_provider(&config, state.tracking_store(), !client.is_localhost)
        .await;
    if let Some(response) = enforce_provider_remote_access(client, provider.as_deref()) {
        return response;
    }
    let request = match parse_object_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let request_id = match required_string(&request, "request_id") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let decision = match request.get("decision") {
        None => return missing_field("decision", Value::Object(request)),
        Some(Value::String(value)) if matches!(value.as_str(), "allow" | "deny") => value.clone(),
        Some(value) => return literal_error("decision", "'allow' or 'deny'", value.clone()),
    };
    if let Err(detail) = SessionStore::validate_session_id(&session_id) {
        return detail_response(StatusCode::BAD_REQUEST, &detail);
    }
    let mut session = match runtime.sessions().load(&session_id) {
        Ok(Some(session)) => session,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Session not found"),
        Err(_) => return internal_error(),
    };
    session.pending_tool_decisions = Map::from_iter([(request_id, Value::String(decision))]);
    if runtime.sessions().save(&session_id, &session).is_err() {
        return internal_error();
    }
    json_response(
        StatusCode::OK,
        json!({"session_id": session_id, "stream_url": format!("{PREFIX}/sessions/{session_id}/stream")}),
    )
}

async fn resolve_client_tool_result(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Response {
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let provider = runtime
        .resolve_provider(&config, state.tracking_store(), !client.is_localhost)
        .await;
    if let Some(response) = enforce_provider_remote_access(client, provider.as_deref()) {
        return response;
    }
    let request = match parse_object_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let request_id = match required_string(&request, "request_id") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let content = match required_string(&request, "content") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let is_error = match request.get("is_error") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(value) => {
            return field_type_error(
                "is_error",
                "bool_type",
                "Input should be a valid boolean",
                value.clone(),
            )
        }
    };
    if let Err(detail) = SessionStore::validate_session_id(&session_id) {
        return detail_response(StatusCode::BAD_REQUEST, &detail);
    }
    let mut session = match runtime.sessions().load(&session_id) {
        Ok(Some(session)) => session,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Session not found"),
        Err(_) => return internal_error(),
    };
    session.pending_client_tool_results =
        Map::from_iter([(request_id, json!({"content":content,"is_error":is_error}))]);
    if runtime.sessions().save(&session_id, &session).is_err() {
        return internal_error();
    }
    json_response(
        StatusCode::OK,
        json!({"session_id":session_id,"stream_url":format!("{PREFIX}/sessions/{session_id}/stream")}),
    )
}

async fn provider_health(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    Path(provider): Path<String>,
) -> Response {
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let runtime = state.assistant_runtime();
    let Some(instance) = runtime.provider(&provider) else {
        return detail_response(
            StatusCode::NOT_FOUND,
            &format!("Provider '{provider}' not found"),
        );
    };
    if !client.is_localhost && !instance.allows_remote_access() {
        return detail_response(StatusCode::FORBIDDEN, REMOTE_ACCESS_DETAIL);
    }
    let config = runtime.load_config();
    match instance
        .check_connection(config.providers.get(&provider).cloned())
        .await
    {
        Ok(()) => json_response(StatusCode::OK, json!({"status": "ok"})),
        Err(AssistantProviderError::NotImplemented(detail)) => {
            detail_response(StatusCode::NOT_IMPLEMENTED, &detail)
        }
        Err(AssistantProviderError::CliNotInstalled(detail)) => {
            detail_response(StatusCode::PRECONDITION_FAILED, &detail)
        }
        Err(AssistantProviderError::NotAuthenticated(detail)) => {
            detail_response(StatusCode::UNAUTHORIZED, &detail)
        }
        Err(_) => internal_error(),
    }
}

async fn get_providers(
    State(state): State<AppState>,
    Extension(_client): Extension<AssistantClient>,
) -> Response {
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    let mut providers = Vec::with_capacity(runtime.inner.providers.len());
    let mut availability = HashMap::new();
    for provider in &runtime.inner.providers {
        let available = provider
            .is_available(
                state.tracking_store().clone(),
                config.providers.get(provider.name()).cloned(),
            )
            .await;
        availability.insert(provider.name().to_string(), available);
        providers.push(json!({
            "name": provider.name(),
            "display_name": provider.display_name(),
            "description": provider.description(),
            "available": available,
            "selected": config.providers.get(provider.name()).is_some_and(|value| {
                value.get("selected").and_then(Value::as_bool) == Some(true)
            }),
            "requires_api_key": false,
            "has_api_key": false,
            "allows_remote_access": provider.allows_remote_access(),
            "client_tool_delivery": provider.client_tool_delivery(),
            "model_options": [],
        }));
    }

    let resolved = runtime
        .selected_provider(&config)
        .map(|provider| resolved_provider_value(provider.as_ref(), &config, false))
        .or_else(|| {
            runtime
                .inner
                .providers
                .iter()
                .find(|provider| {
                    provider.name() != GATEWAY_PROVIDER
                        && availability.get(provider.name()) == Some(&true)
                })
                .map(|provider| resolved_provider_value(provider.as_ref(), &config, true))
        });

    json_response(
        StatusCode::OK,
        json!({
            "providers": providers,
            "resolved": resolved,
            "gateway_vendor_options": gateway_vendor_options(),
        }),
    )
}

fn resolved_provider_value(
    provider: &dyn AssistantProvider,
    config: &AssistantConfig,
    auto_selected: bool,
) -> Value {
    let model = config
        .providers
        .get(provider.name())
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .filter(|model| *model != "default")
        .map(str::to_string);
    let gateway_vendor = (provider.name() == GATEWAY_PROVIDER)
        .then(|| {
            model
                .as_deref()
                .and_then(gateway_vendor_from_managed_endpoint)
        })
        .flatten();
    let provider_model = gateway_vendor.and_then(gateway_vendor_model);
    json!({
        "name": provider.name(),
        "model": model,
        "auto_selected": auto_selected,
        "requires_api_key": false,
        "has_api_key": gateway_vendor.is_some(),
        "client_tool_delivery": provider.client_tool_delivery(),
        "model_provider": gateway_vendor,
        "model_options": provider_model.into_iter().collect::<Vec<_>>(),
        "provider_model": provider_model,
    })
}

fn gateway_vendor_options() -> Value {
    Value::Object(
        GATEWAY_VENDOR_MODELS
            .iter()
            .map(|(vendor, model)| ((*vendor).to_string(), json!([model])))
            .collect(),
    )
}

fn gateway_vendor_model(vendor: &str) -> Option<&'static str> {
    GATEWAY_VENDOR_MODELS
        .iter()
        .find_map(|(candidate, model)| (*candidate == vendor).then_some(*model))
}

fn gateway_vendor_from_managed_endpoint(model: &str) -> Option<&str> {
    model
        .strip_prefix(GATEWAY_MANAGED_PREFIX)
        .filter(|vendor| gateway_vendor_model(vendor).is_some())
}

async fn get_config(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
) -> Response {
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    let selected_provider = runtime.selected_provider(&config);
    let provider = match selected_provider.clone() {
        Some(provider) => Some(provider),
        None => {
            runtime
                .resolve_default_provider(
                    &config,
                    state.tracking_store(),
                    !client.is_localhost,
                    false,
                )
                .await
        }
    };
    let remote_access_allowed = match provider.as_deref() {
        None => false,
        Some(provider) => match remote_assistant_enabled() {
            Ok(enabled) => enabled && provider.allows_remote_access(),
            Err(()) => return internal_error(),
        },
    };
    json_response(
        StatusCode::OK,
        config.response_value(
            client.is_localhost,
            remote_access_allowed,
            selected_provider
                .is_none()
                .then(|| provider.as_ref().map(|provider| provider.name()))
                .flatten(),
        ),
    )
}

async fn update_config(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    body: Bytes,
) -> Response {
    if let Some(response) = enforce_local_only(client) {
        return response;
    }
    let request = match parse_object_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    for field in ["providers", "projects"] {
        if let Some(value) = request.get(field) {
            if !value.is_null() && !value.is_object() {
                return field_type_error(
                    field,
                    "dict_type",
                    "Input should be a valid dictionary",
                    value.clone(),
                );
            }
        }
    }
    let runtime = state.assistant_runtime();
    let mut config = runtime.load_config();
    if let Some(providers) = request.get("providers").and_then(Value::as_object) {
        for (name, update) in providers {
            let Some(update) = update.as_object() else {
                return internal_error();
            };
            let gateway_model =
                match store_gateway_api_key(state.tracking_store(), name, update).await {
                    Ok(model) => model,
                    Err(GatewayConnectionError::BadRequest(detail)) => {
                        return detail_response(StatusCode::BAD_REQUEST, &detail)
                    }
                    Err(GatewayConnectionError::Internal) => return internal_error(),
                };
            let existing = config.providers.get(name).cloned();
            let model = gateway_model
                .or_else(|| {
                    update
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            existing
                                .as_ref()
                                .and_then(|value| value.get("model"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .or_else(|| Some("default".to_string()))
                })
                .expect("provider model has a default");
            let mut provider = existing.unwrap_or_else(default_provider);
            let object = provider.as_object_mut().expect("normalized provider");
            object.insert("model".to_string(), Value::String(model));
            if let Some(Value::String(base_url)) = update.get("base_url") {
                object.insert("base_url".to_string(), Value::String(base_url.clone()));
            }
            object.shift_remove("api_key");
            if let Some(permissions) = update.get("permissions") {
                let Some(permissions) = normalize_permissions_update(permissions) else {
                    return internal_error();
                };
                object.insert("permissions".to_string(), permissions);
            }
            let selected = update.get("selected").and_then(Value::as_bool) == Some(true);
            config.providers.insert(name.clone(), provider);
            if selected {
                for (provider_name, provider) in &mut config.providers {
                    provider
                        .as_object_mut()
                        .expect("normalized provider")
                        .insert("selected".to_string(), Value::Bool(provider_name == name));
                }
            }
        }
    }
    if let Some(projects) = request.get("projects").and_then(Value::as_object) {
        for (experiment_id, update) in projects {
            if update.is_null() {
                config.projects.shift_remove(experiment_id);
                continue;
            }
            let Some(update) = update.as_object() else {
                return internal_error();
            };
            let location = update.get("location").and_then(Value::as_str).unwrap_or("");
            let project_path = expand_user(location, &runtime.inner.home);
            if !project_path.exists() {
                return detail_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Project path does not exist: {location}"),
                );
            }
            let project_type = update
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("local");
            if project_type != "local" {
                return internal_error();
            }
            config.projects.insert(
                experiment_id.clone(),
                json!({"type": "local", "location": project_path.to_string_lossy()}),
            );
        }
    }
    if runtime.save_config(&config).is_err() {
        return internal_error();
    }
    let selected_provider = runtime.selected_provider(&config);
    let remote_access_allowed = match selected_provider.as_deref() {
        None => false,
        Some(provider) => match remote_assistant_enabled() {
            Ok(enabled) => enabled && provider.allows_remote_access(),
            Err(()) => return internal_error(),
        },
    };
    json_response(
        StatusCode::OK,
        config.response_value(true, remote_access_allowed, None),
    )
}

#[derive(Debug)]
enum GatewayConnectionError {
    BadRequest(String),
    Internal,
}

async fn store_gateway_api_key(
    store: &TrackingStore,
    provider_name: &str,
    provider_data: &Map<String, Value>,
) -> Result<Option<String>, GatewayConnectionError> {
    let api_key = provider_data
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let gateway_vendor = provider_data.get("gateway_vendor");
    if api_key.is_none() && gateway_vendor.is_none() {
        return Ok(None);
    }
    let Some(api_key) = api_key else {
        return Err(GatewayConnectionError::BadRequest(
            "Gateway vendor connections require an API key.".to_string(),
        ));
    };
    if provider_name != GATEWAY_PROVIDER {
        return Err(GatewayConnectionError::BadRequest(
            "API keys must be stored in LLM Connections through the 'mlflow_gateway' provider."
                .to_string(),
        ));
    }
    let Some(vendor) = gateway_vendor.and_then(Value::as_str) else {
        return Err(GatewayConnectionError::BadRequest(
            "Gateway API keys require a gateway_vendor.".to_string(),
        ));
    };
    let Some(model_name) = gateway_vendor_model(vendor) else {
        return Err(GatewayConnectionError::BadRequest(format!(
            "Unknown Gateway vendor: '{vendor}'"
        )));
    };
    ensure_gateway_connection(store, vendor, model_name, api_key)
        .await
        .map(Some)
}

async fn ensure_gateway_connection(
    store: &TrackingStore,
    vendor: &str,
    model_name: &str,
    api_key: &str,
) -> Result<String, GatewayConnectionError> {
    let name = format!("{GATEWAY_MANAGED_PREFIX}{vendor}");
    let secret_value = HashMap::from([("api_key".to_string(), api_key.to_string())]);
    let secret = match store
        .get_gateway_secret_info(WORKSPACE_DEFAULT_NAME, None, Some(&name))
        .await
    {
        Ok(secret) => store
            .update_gateway_secret(
                WORKSPACE_DEFAULT_NAME,
                &secret.secret_id,
                Some(&secret_value),
                None,
                None,
            )
            .await
            .map_err(gateway_store_error)?,
        Err(error) if error.error_code == mlflow_error::ErrorCode::ResourceDoesNotExist => store
            .create_gateway_secret(
                WORKSPACE_DEFAULT_NAME,
                &name,
                &secret_value,
                Some(vendor),
                &HashMap::new(),
                None,
            )
            .await
            .map_err(gateway_store_error)?,
        Err(error) => return Err(gateway_store_error(error)),
    };

    let model_definition = match store
        .get_gateway_model_definition(WORKSPACE_DEFAULT_NAME, None, Some(&name))
        .await
    {
        Ok(model_definition) => model_definition,
        Err(error) if error.error_code == mlflow_error::ErrorCode::ResourceDoesNotExist => store
            .create_gateway_model_definition(
                WORKSPACE_DEFAULT_NAME,
                &name,
                &secret.secret_id,
                vendor,
                model_name,
                None,
            )
            .await
            .map_err(gateway_store_error)?,
        Err(error) => return Err(gateway_store_error(error)),
    };

    let endpoint = match store
        .get_gateway_endpoint(WORKSPACE_DEFAULT_NAME, None, Some(&name))
        .await
    {
        Ok(endpoint) => endpoint,
        Err(error) if error.error_code == mlflow_error::ErrorCode::ResourceDoesNotExist => store
            .create_gateway_endpoint(
                WORKSPACE_DEFAULT_NAME,
                &name,
                &[EndpointModelConfig {
                    model_definition_id: model_definition.model_definition_id,
                    linkage_type: "PRIMARY".to_string(),
                    weight: 1.0,
                    fallback_order: None,
                }],
                None,
                None,
                None,
                None,
                true,
            )
            .await
            .map_err(gateway_store_error)?,
        Err(error) => return Err(gateway_store_error(error)),
    };
    Ok(endpoint.name.unwrap_or(name))
}

fn gateway_store_error(error: mlflow_error::MlflowError) -> GatewayConnectionError {
    if error.error_code == mlflow_error::ErrorCode::NotImplemented {
        GatewayConnectionError::BadRequest(GATEWAY_UNSUPPORTED_DETAIL.to_string())
    } else {
        GatewayConnectionError::Internal
    }
}

async fn install_skills_endpoint(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    body: Bytes,
) -> Response {
    if let Some(response) = enforce_local_only(client) {
        return response;
    }
    let request = match parse_object_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let install_type = match request.get("type") {
        None => "global".to_string(),
        Some(Value::String(value)) if matches!(value.as_str(), "global" | "project" | "custom") => {
            value.clone()
        }
        Some(value) => {
            return literal_error("type", "'global', 'project' or 'custom'", value.clone())
        }
    };
    let custom_path = match optional_string(&request, "custom_path") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let experiment_id = match optional_string(&request, "experiment_id") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let runtime = state.assistant_runtime();
    let config = runtime.load_config();
    let project_path = if install_type == "project" {
        let Some(experiment_id) = experiment_id else {
            return detail_response(
                StatusCode::BAD_REQUEST,
                "experiment_id required for 'project' type",
            );
        };
        let Some(path) = config.project_path(&experiment_id) else {
            return detail_response(
                StatusCode::BAD_REQUEST,
                &format!("No project path configured for experiment {experiment_id}"),
            );
        };
        Some(path)
    } else {
        None
    };
    let Some(provider) = runtime.selected_provider(&config) else {
        return detail_response(StatusCode::PRECONDITION_FAILED, NO_PROVIDER_DETAIL);
    };
    let destination = match install_type.as_str() {
        "global" => provider.resolve_skills_path(&runtime.inner.home),
        "project" => provider.resolve_skills_path(project_path.as_deref().expect("project path")),
        "custom" => {
            let Some(path) = custom_path.filter(|value| !value.is_empty()) else {
                return detail_response(
                    StatusCode::BAD_REQUEST,
                    "custom_path is required when type='custom'.",
                );
            };
            expand_user(&path, &runtime.inner.home)
        }
        _ => unreachable!(),
    };
    if destination.exists() {
        match list_installed_skills(&destination) {
            Ok(skills) if !skills.is_empty() => {
                return json_response(
                    StatusCode::OK,
                    json!({"installed_skills": skills, "skills_directory": destination.to_string_lossy()}),
                )
            }
            Ok(_) => {}
            Err(_) => return internal_error(),
        }
    }
    let installed = match install_skills(&runtime.inner.skills_source, &destination) {
        Ok(installed) => installed,
        Err(_) => return internal_error(),
    };
    json_response(
        StatusCode::OK,
        json!({"installed_skills": installed, "skills_directory": destination.to_string_lossy()}),
    )
}

#[derive(Debug, Deserialize)]
struct ModelsQuery {
    base_url: Option<String>,
}

async fn list_provider_models(
    State(state): State<AppState>,
    Extension(client): Extension<AssistantClient>,
    Path(provider): Path<String>,
    Query(query): Query<ModelsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = enforce_remote_opt_in(client) {
        return response;
    }
    let runtime = state.assistant_runtime();
    let Some(instance) = runtime.provider(&provider) else {
        return detail_response(
            StatusCode::NOT_FOUND,
            &format!("Provider '{provider}' not found"),
        );
    };
    if !client.is_localhost && !instance.allows_remote_access() {
        return detail_response(StatusCode::FORBIDDEN, REMOTE_ACCESS_DETAIL);
    }
    if provider == GATEWAY_PROVIDER {
        return match state
            .tracking_store()
            .list_gateway_endpoints(WORKSPACE_DEFAULT_NAME, None, None)
            .await
        {
            Ok(endpoints) => {
                let mut models = endpoints
                    .into_iter()
                    .filter_map(|endpoint| endpoint.name)
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>();
                models.sort();
                json_response(StatusCode::OK, json!({"models": models}))
            }
            Err(error) if error.error_code == mlflow_error::ErrorCode::NotImplemented => {
                json_response(StatusCode::OK, json!({"models": []}))
            }
            Err(_) => internal_error(),
        };
    }
    let config = runtime.load_config();
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    match instance
        .list_models(
            query.base_url,
            api_key,
            config.providers.get(&provider).cloned(),
        )
        .await
    {
        Ok(models) => json_response(StatusCode::OK, json!({"models": models})),
        Err(AssistantProviderError::NotImplemented(_)) => detail_response(
            StatusCode::NOT_FOUND,
            &format!("Model listing is not supported for provider '{provider}'"),
        ),
        Err(AssistantProviderError::CliNotInstalled(detail)) => {
            detail_response(StatusCode::PRECONDITION_FAILED, &detail)
        }
        Err(AssistantProviderError::NotConfigured(detail))
        | Err(AssistantProviderError::NotAuthenticated(detail)) => {
            detail_response(StatusCode::SERVICE_UNAVAILABLE, &detail)
        }
        Err(AssistantProviderError::Internal(_)) => internal_error(),
    }
}

fn normalize_provider(value: &Value) -> Option<Value> {
    let value = value.as_object()?;
    let model = value
        .get("model")
        .map(Value::as_str)
        .transpose()?
        .unwrap_or("default");
    let selected = value
        .get("selected")
        .map(Value::as_bool)
        .transpose()?
        .unwrap_or(false);
    let base_url = nullable_string(value.get("base_url"))?;
    let permissions = match value.get("permissions") {
        Some(value) => normalize_permissions(value)?,
        None => default_permissions(),
    };
    let skills = match value.get("skills") {
        Some(Value::Object(skills)) => {
            let skill_type = skills
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("global");
            if !matches!(skill_type, "global" | "project" | "custom") {
                return None;
            }
            let custom_path = nullable_string(skills.get("custom_path"))?;
            json!({"type": skill_type, "custom_path": custom_path})
        }
        None => json!({"type": "global", "custom_path": null}),
        _ => return None,
    };
    Some(json!({
        "model": model,
        "selected": selected,
        "base_url": base_url,
        "permissions": permissions,
        "skills": skills,
    }))
}

fn default_provider() -> Value {
    json!({
        "model": "default",
        "selected": false,
        "base_url": null,
        "permissions": default_permissions(),
        "skills": {"type": "global", "custom_path": null},
    })
}

fn normalize_project(value: &Value) -> Option<Value> {
    let value = value.as_object()?;
    let project_type = value.get("type").and_then(Value::as_str).unwrap_or("local");
    if project_type != "local" {
        return None;
    }
    let location = value.get("location")?.as_str()?;
    Some(json!({"type": "local", "location": location}))
}

fn normalize_permissions(value: &Value) -> Option<Value> {
    let value = value.as_object()?;
    Some(json!({
        "allow_edit_files": optional_bool(value.get("allow_edit_files"), true)?,
        "allow_read_docs": optional_bool(value.get("allow_read_docs"), true)?,
        "full_access": optional_bool(value.get("full_access"), false)?,
    }))
}

fn normalize_permissions_update(value: &Value) -> Option<Value> {
    normalize_permissions(value)
}

fn default_permissions() -> Value {
    json!({"allow_edit_files": true, "allow_read_docs": true, "full_access": false})
}

fn optional_bool(value: Option<&Value>, default: bool) -> Option<bool> {
    value
        .map(Value::as_bool)
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn nullable_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(value)) => Some(Some(value.clone())),
        _ => None,
    }
}

fn parse_object_body(body: &[u8]) -> Result<Map<String, Value>, Box<Response>> {
    if body.is_empty() {
        return Err(Box::new(validation_response(json!({
            "type": "missing", "loc": ["body"], "msg": "Field required", "input": null
        }))));
    }
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        Box::new(validation_response(json!({
            "type": "json_invalid",
            "loc": ["body", error.column().saturating_sub(1)],
            "msg": "JSON decode error",
            "input": {},
            "ctx": {"error": error.to_string()},
        })))
    })?;
    match value {
        Value::Object(value) => Ok(value),
        value => Err(Box::new(validation_response(json!({
            "type": "model_attributes_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "input": value,
        })))),
    }
}

fn required_string(value: &Map<String, Value>, field: &str) -> Result<String, Box<Response>> {
    match value.get(field) {
        None => Err(Box::new(missing_field(field, Value::Object(value.clone())))),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(value) => Err(Box::new(field_type_error(
            field,
            "string_type",
            "Input should be a valid string",
            value.clone(),
        ))),
    }
}

fn optional_string(
    value: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, Box<Response>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(value) => Err(Box::new(field_type_error(
            field,
            "string_type",
            "Input should be a valid string",
            value.clone(),
        ))),
    }
}

fn missing_field(field: &str, input: Value) -> Response {
    validation_response(json!({
        "type": "missing", "loc": ["body", field], "msg": "Field required", "input": input
    }))
}

fn field_type_error(field: &str, kind: &str, message: &str, input: Value) -> Response {
    validation_response(json!({
        "type": kind, "loc": ["body", field], "msg": message, "input": input
    }))
}

fn literal_error(field: &str, expected: &str, input: Value) -> Response {
    validation_response(json!({
        "type": "literal_error",
        "loc": ["body", field],
        "msg": format!("Input should be {expected}"),
        "input": input,
        "ctx": {"expected": expected},
    }))
}

fn validation_response(error: Value) -> Response {
    json_response(StatusCode::UNPROCESSABLE_ENTITY, json!({"detail": [error]}))
}

fn detail_response(status: StatusCode, detail: &str) -> Response {
    json_response(status, json!({"detail": detail}))
}

fn json_response(status: StatusCode, value: Value) -> Response {
    let mut response = Response::new(Body::from(
        serde_json::to_vec(&value).expect("JSON value serialization"),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Internal Server Error",
    )
        .into_response()
}

fn tracking_uri(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expand_user(path: &str, home: &FsPath) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn install_skills(source: &FsPath, destination: &FsPath) -> std::io::Result<Vec<String>> {
    let mut installed = Vec::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || !path.join("SKILL.md").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        copy_tree(&path, &destination.join(&name))?;
        installed.push(name);
    }
    installed.sort();
    Ok(installed)
}

fn copy_tree(source: &FsPath, destination: &FsPath) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else {
            fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn list_installed_skills(destination: &FsPath) -> std::io::Result<Vec<String>> {
    fn visit(path: &FsPath, skills: &mut Vec<String>) -> std::io::Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            if child.is_dir() {
                if child.join("SKILL.md").is_file() {
                    skills.push(entry.file_name().to_string_lossy().into_owned());
                }
                visit(&child, skills)?;
            }
        }
        Ok(())
    }
    let mut skills = Vec::new();
    visit(destination, &mut skills)?;
    skills.sort();
    Ok(skills)
}

trait OptionTranspose<T> {
    fn transpose(self) -> Option<Option<T>>;
}

impl<T> OptionTranspose<T> for Option<Option<T>> {
    fn transpose(self) -> Option<Option<T>> {
        match self {
            Some(Some(value)) => Some(Some(value)),
            Some(None) => None,
            None => Some(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_serialization_matches_python_json_dump() {
        let session = AssistantSession {
            context: Map::from_iter([("trace".to_string(), json!("café"))]),
            messages: vec![AssistantMessage {
                role: "user".to_string(),
                content: json!("hello"),
            }],
            pending_message: Some(AssistantMessage {
                role: "user".to_string(),
                content: json!("hello"),
            }),
            provider_session_id: None,
            working_dir: Some(PathBuf::from("/tmp/project")),
            pending_tool_decisions: Map::new(),
            pending_client_tool_results: Map::new(),
        };
        let value = serde_json::to_value(session).unwrap();
        assert_eq!(
            python_json_dumps(&value, false),
            "{\"context\": {\"trace\": \"caf\\u00e9\"}, \"messages\": [{\"role\": \"user\", \"content\": \"hello\"}], \"pending_message\": {\"role\": \"user\", \"content\": \"hello\"}, \"provider_session_id\": null, \"working_dir\": \"/tmp/project\", \"pending_tool_decisions\": {}, \"pending_client_tool_results\": {}}"
        );
    }

    #[test]
    fn uuid_validation_rejects_traversal() {
        assert!(SessionStore::validate_session_id("../../config").is_err());
        assert!(SessionStore::validate_session_id(&Uuid::new_v4().to_string()).is_ok());
    }

    #[test]
    fn session_save_atomically_replaces_and_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let session_id = Uuid::new_v4().to_string();
        let mut session = AssistantSession::default();
        store.save(&session_id, &session).unwrap();
        session.provider_session_id = Some("second-write".to_string());
        store.save(&session_id, &session).unwrap();

        assert_eq!(store.load(&session_id).unwrap(), Some(session));
        let entries: Vec<_> = fs::read_dir(store.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![format!("{session_id}.json")]);
    }

    #[test]
    fn unsupported_gateway_store_error_has_exact_detail() {
        match gateway_store_error(mlflow_error::MlflowError::not_implemented("unsupported")) {
            GatewayConnectionError::BadRequest(detail) => {
                assert_eq!(detail, GATEWAY_UNSUPPORTED_DETAIL)
            }
            GatewayConnectionError::Internal => panic!("expected a bad-request mapping"),
        }
    }
}
