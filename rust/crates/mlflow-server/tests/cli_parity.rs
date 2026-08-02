//! CLI/env parity integration tests (plan T11.1).
//!
//! Spawns the built `mlflow-server` binary and asserts its argument-parsing
//! behaviour: `--help` succeeds and advertises the parity flags, unknown flags
//! are rejected by clap (exit 2), and the fail-loud parity cases
//! (`--app-name` other than `basic-auth`, a mismatched `--registry-store-uri`)
//! exit non-zero with a message naming the flag. The pure config-resolution
//! logic is unit-tested in `src/config.rs`; this file covers the process-level
//! contract that deploy scripts see.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Path to the `mlflow-server` binary under test (Cargo sets `CARGO_BIN_EXE_*`
/// for integration tests of a binary crate).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mlflow-server")
}

/// Run the binary with `args` and an empty environment (so ambient
/// `MLFLOW_*` vars from the developer shell can't perturb the assertions),
/// returning (exit_code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(bin())
        .args(args)
        .env_clear()
        .output()
        .expect("spawn mlflow-server");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn help_exits_zero_and_lists_parity_flags() {
    let (code, stdout, _stderr) = run(&["--help"]);
    assert_eq!(code, 0);
    for flag in [
        "--backend-store-uri",
        "--read-replica-backend-store-uri",
        "--registry-store-uri",
        "--default-artifact-root",
        "--serve-artifacts",
        "--no-serve-artifacts",
        "--artifacts-destination",
        "--artifacts-only",
        "--host",
        "--port",
        "--workers",
        "--static-prefix",
        "--allowed-hosts",
        "--cors-allowed-origins",
        "--x-frame-options",
        "--expose-prometheus",
        "--app-name",
        "--workspace-store-uri",
        "--enable-workspaces",
        "--disable-workspaces",
    ] {
        assert!(stdout.contains(flag), "help output missing {flag}");
    }
}

#[test]
fn unknown_flag_is_rejected() {
    let (code, _stdout, stderr) = run(&["--not-a-real-flag"]);
    // clap exits 2 on argument errors.
    assert_eq!(code, 2);
    assert!(stderr.contains("--not-a-real-flag") || stderr.contains("unexpected"));
}

#[test]
fn unsupported_app_name_fails_loudly() {
    let (code, _stdout, stderr) = run(&["--app-name", "wsgi-magic"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("--app-name") && stderr.contains("wsgi-magic"),
        "stderr did not name the unsupported --app-name value: {stderr}"
    );
}

#[test]
fn mismatched_registry_store_uri_fails_loudly() {
    let (code, _stdout, stderr) = run(&[
        "--backend-store-uri",
        "sqlite:///a.db",
        "--registry-store-uri",
        "postgresql://other/db",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--registry-store-uri"),
        "stderr did not name --registry-store-uri: {stderr}"
    );
}

#[test]
fn artifacts_only_workspaces_initializes_workspace_store_but_not_tracking_backend() {
    let temp = tempfile::TempDir::new().unwrap();
    let workspace_db = temp.path().join("workspace.db");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("tracking.db"),
        &workspace_db,
    )
    .unwrap();
    let default_tracking = temp.path().join("mlflow.db");
    let artifact_dir = temp.path().join("artifacts");
    std::fs::create_dir(&artifact_dir).unwrap();

    let mut child = Command::new(bin())
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--artifacts-only",
            "--enable-workspaces",
            "--workspace-store-uri",
            &format!("sqlite:///{}", workspace_db.display()),
            "--artifacts-destination",
            &format!("file://{}", artifact_dir.display()),
        ])
        .current_dir(temp.path())
        .env_clear()
        .spawn()
        .expect("spawn artifacts-only server");

    for _ in 0..20 {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("artifacts-only server exited during startup: {status}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !default_tracking.exists(),
        "artifacts-only startup initialized the tracking backend"
    );
}

#[test]
fn invalid_static_prefix_fails_loudly() {
    // Missing leading slash: `_validate_static_prefix` parity.
    let (code, _stdout, stderr) = run(&["--static-prefix", "no-leading-slash"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("--static-prefix"),
        "stderr did not name --static-prefix: {stderr}"
    );
}

#[test]
fn artifacts_only_workspace_matrix_requires_workspace_store_uri() {
    let (code, _stdout, stderr) = run(&["--artifacts-only", "--enable-workspaces"]);
    assert_ne!(code, 0);
    assert!(stderr.contains(
        "--workspace-store-uri is required when combining --enable-workspaces with \
         --artifacts-only so artifact requests can be validated against a workspace provider."
    ));

    // Supplying the required URI passes CLI validation and proceeds to store
    // initialization, where this deliberately missing database fails.
    let (code, _stdout, stderr) = run(&[
        "--artifacts-only",
        "--enable-workspaces",
        "--workspace-store-uri",
        "sqlite:///definitely-missing-workspace.db",
    ]);
    assert_ne!(code, 0);
    assert!(
        !stderr.contains("--workspace-store-uri is required"),
        "valid combination was rejected by CLI validation: {stderr}"
    );
}
