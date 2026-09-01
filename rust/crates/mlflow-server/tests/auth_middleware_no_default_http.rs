//! Auth-middleware HTTP tests that require `default_permission = NO_PERMISSIONS`
//! (plan T9.4). These mirror the Python cases that use the
//! `fixtures/no_permission_auth.ini` config: the MV-create source-READ deny and
//! the default-permission deny fallback.
//!
//! Since T9.8, `default_permission` is threaded through the parsed
//! [`mlflow_auth::AuthConfig`] carried by the [`AuthStore`], so these tests
//! build the store with `AuthConfig { default_permission: "NO_PERMISSIONS", .. }`
//! instead of the retired `MLFLOW_AUTH_DEFAULT_PERMISSION` env var. That makes
//! the config per-store rather than process-global, so no cross-test env race
//! remains.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use metrics_exporter_prometheus::PrometheusBuilder;
use mlflow_auth::{AuthConfig, AuthDb, AuthStore};
use mlflow_server::{build_app_with_recorder, AppState, ServerConfig};
use mlflow_store::{Db, PoolConfig, TrackingStore};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

const ART_ROOT: &str = "s3://bucket/mlruns";
const WS: &str = "default";
const ADMIN: (&str, &str) = ("alice_scrypt", "alice-password-123");

fn no_permission_config() -> AuthConfig {
    AuthConfig {
        default_permission: "NO_PERMISSIONS".to_string(),
        ..AuthConfig::default()
    }
}

fn auth_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("mlflow-auth")
        .join("tests")
        .join("fixtures")
        .join("basic_auth.db")
}

fn tracking_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tracking.db")
}

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str, source: &Path) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mlflow_rust_authmw_nd_{}_{}_{}.db",
            tag,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::copy(source, &path).expect("copy fixture");
        TempDb { path }
    }

    fn uri(&self) -> String {
        format!("sqlite:///{}", self.path.display())
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct TestServer {
    base: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    tracking: TrackingStore,
    auth: AuthStore,
    _tracking_db: TempDb,
    _auth_db: TempDb,
    _artifact_dir: Option<TempDir>,
}

impl TestServer {
    async fn start(tag: &str) -> Self {
        Self::start_with_prefix(tag, None).await
    }

    async fn start_with_prefix(tag: &str, static_prefix: Option<&str>) -> Self {
        Self::start_configured(tag, static_prefix, false).await
    }

    async fn start_artifacts_only(tag: &str) -> Self {
        Self::start_configured(tag, None, true).await
    }

    async fn start_configured(
        tag: &str,
        static_prefix: Option<&str>,
        artifacts_only: bool,
    ) -> Self {
        let tracking_db = TempDb::new(&format!("{tag}_track"), &tracking_fixture_path());
        let db = Db::connect(&tracking_db.uri(), PoolConfig::default())
            .await
            .expect("connect tracking fixture");
        let tracking = TrackingStore::new(db, ART_ROOT);

        let auth_db_file = TempDb::new(&format!("{tag}_auth"), &auth_fixture_path());
        let auth_db =
            AuthDb::connect_and_verify_with(&auth_db_file.uri(), None, PoolConfig::default())
                .await
                .expect("connect + verify auth fixture");
        let auth = AuthStore::with_config(auth_db, no_permission_config());

        let artifact_dir = artifacts_only.then(|| TempDir::new().expect("artifact dir"));
        let artifacts_destination = artifact_dir
            .as_ref()
            .map(|dir| format!("file://{}", dir.path().display()));
        let state = if let Some(destination) = artifacts_destination.as_deref() {
            AppState::artifacts_only(
                true,
                Some(
                    mlflow_artifacts::factory::repo_from_uri(destination)
                        .expect("artifact repository"),
                ),
                Some(destination.to_string()),
            )
        } else {
            AppState::new(tracking.clone())
        }
        .with_auth_store(auth.clone());
        let config = ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            static_prefix: static_prefix.map(str::to_string),
            backend_store_uri: None,
            default_artifact_root: None,
            serve_artifacts: true,
            artifacts_only,
            artifacts_destination,
            allowed_hosts: None,
            cors_allowed_origins: None,
            x_frame_options: "SAMEORIGIN".to_string(),
            ..Default::default()
        };
        let recorder = PrometheusBuilder::new().build_recorder().handle();
        let app = build_app_with_recorder(&config, recorder, Some(state));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("server error");
        });

        TestServer {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
            tracking,
            auth,
            _tracking_db: tracking_db,
            _auth_db: auth_db_file,
            _artifact_dir: artifact_dir,
        }
    }

    async fn create_user(&self, username: &str) -> (String, String) {
        let password = format!("{username}-password-1");
        self.auth
            .create_user(username, &password, false)
            .await
            .expect("create user");
        (username.to_string(), password)
    }

    async fn grant(
        &self,
        username: &str,
        resource_type: &str,
        resource_id: &str,
        permission: &str,
    ) {
        self.auth
            .grant_user_permission(username, resource_type, resource_id, permission, WS)
            .await
            .expect("grant");
    }

    async fn create_experiment(&self, name: &str) -> String {
        self.tracking
            .create_experiment(WS, name, None, &[])
            .await
            .expect("create experiment")
    }

    async fn create_run(&self, experiment_id: &str) -> String {
        self.tracking
            .create_run(WS, experiment_id, None, Some(1), Some("r"), &[])
            .await
            .expect("create run")
            .info
            .run_id
    }

    async fn create_dataset(&self, experiment_ids: &[String]) -> String {
        self.tracking
            .create_evaluation_dataset(WS, "auth-dataset", &serde_json::Map::new(), experiment_ids)
            .await
            .expect("create dataset")
            .dataset_id
    }

    async fn create_issue(&self, experiment_id: &str) -> String {
        self.tracking
            .create_issue(
                WS,
                experiment_id,
                "auth issue",
                "auth issue description",
                "open",
                None,
                &[],
                None,
                &[],
                None,
            )
            .await
            .expect("create issue")
            .issue_id
    }

    async fn create_gateway_secret(&self) -> String {
        self.tracking
            .create_gateway_secret(
                WS,
                "auth-secret",
                &HashMap::from([("api_key".to_string(), "secret".to_string())]),
                Some("openai"),
                &HashMap::new(),
                None,
            )
            .await
            .expect("create gateway secret")
            .secret_id
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

fn basic_header(user: &str, pass: &str) -> String {
    let raw = format!("{user}:{pass}");
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    format!("Basic {encoded}")
}

struct HttpResponse {
    status: StatusCode,
    body: String,
}

async fn send(
    base: &str,
    method: Method,
    path: &str,
    auth: Option<(&str, &str)>,
    body: Option<Value>,
) -> HttpResponse {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let uri = format!("{base}{path}");
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((u, p)) = auth {
        builder = builder.header("Authorization", basic_header(u, p));
    }
    let body_bytes = match body {
        Some(v) => {
            builder = builder.header("Content-Type", "application/json");
            Bytes::from(v.to_string())
        }
        None => Bytes::new(),
    };
    let req = builder.body(Full::new(body_bytes)).unwrap();
    let resp = client.request(req).await.expect("request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    HttpResponse { status, body }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_no_permission_denies_read_without_grant() {
    // With `default_permission = NO_PERMISSIONS`, an ungranted user cannot even
    // read an experiment → 403 (the resolver folds `None` → NO_PERMISSIONS is
    // wrong; `None` folds to the *default*, which here is NO_PERMISSIONS).
    let srv = TestServer::start("nd_read").await;
    let exp = srv.create_experiment("nd-read-exp").await;
    let (u, pw) = srv.create_user("nate_nd").await;
    let resp = send(
        &srv.base,
        Method::GET,
        &format!("/api/2.0/mlflow/experiments/get?experiment_id={exp}"),
        Some((&u, &pw)),
        None,
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.body);
    assert_eq!(resp.body, "Permission denied");

    // A READ grant lifts the deny.
    srv.grant(&u, "experiment", &exp, "READ").await;
    let ok = send(
        &srv.base,
        Method::GET,
        &format!("/api/2.0/mlflow/experiments/get?experiment_id={exp}"),
        Some((&u, &pw)),
        None,
    )
    .await;
    assert_ne!(ok.status, StatusCode::FORBIDDEN, "{}", ok.body);
}

#[tokio::test]
async fn fail_closed_defaults_to_deny_for_routes_without_a_decision() {
    let srv = TestServer::start("fail_closed_default").await;
    let (user, password) = srv.create_user("fail_closed_user").await;
    let response = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/unregistered-auth-route",
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);
    assert_eq!(response.body, "Permission denied");
}

#[tokio::test]
async fn datasets_require_permissions_on_every_associated_experiment() {
    let srv = TestServer::start("dataset_auth").await;
    let exp_a = srv.create_experiment("dataset-auth-a").await;
    let exp_b = srv.create_experiment("dataset-auth-b").await;
    let dataset_id = srv.create_dataset(&[exp_a.clone(), exp_b.clone()]).await;
    let (user, password) = srv.create_user("dataset_user").await;
    let auth = Some((user.as_str(), password.as_str()));

    let get_path = format!("/api/3.0/mlflow/datasets/{dataset_id}");
    let denied = send(&srv.base, Method::GET, &get_path, auth, None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    srv.grant(&user, "experiment", &exp_a, "READ").await;
    let still_denied = send(&srv.base, Method::GET, &get_path, auth, None).await;
    assert_eq!(
        still_denied.status,
        StatusCode::FORBIDDEN,
        "{}",
        still_denied.body
    );

    srv.grant(&user, "experiment", &exp_b, "READ").await;
    let allowed = send(&srv.base, Method::GET, &get_path, auth, None).await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);

    let empty_search = send(
        &srv.base,
        Method::POST,
        "/api/3.0/mlflow/datasets/search",
        auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(
        empty_search.status,
        StatusCode::FORBIDDEN,
        "{}",
        empty_search.body
    );
}

#[tokio::test]
async fn issues_resolve_their_experiment_and_normalize_create_bodies() {
    let srv = TestServer::start("issue_auth").await;
    let experiment_id = srv.create_experiment("issue-auth-exp").await;
    let issue_id = srv.create_issue(&experiment_id).await;
    let (user, password) = srv.create_user("issue_user").await;
    let auth = Some((user.as_str(), password.as_str()));

    let get_path = format!("/api/3.0/mlflow/issues/{issue_id}");
    let denied = send(&srv.base, Method::GET, &get_path, auth, None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    srv.grant(&user, "experiment", &experiment_id, "READ").await;
    let allowed = send(&srv.base, Method::GET, &get_path, auth, None).await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);

    let missing_experiment = send(
        &srv.base,
        Method::POST,
        "/api/3.0/mlflow/issues/search",
        auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(
        missing_experiment.status,
        StatusCode::FORBIDDEN,
        "{}",
        missing_experiment.body
    );

    srv.grant(&user, "experiment", &experiment_id, "EDIT").await;
    let double_encoded = send(
        &srv.base,
        Method::POST,
        "/api/3.0/mlflow/issues",
        auth,
        Some(Value::String(
            json!({
                "experiment_id": experiment_id,
                "name": "double-encoded issue",
                "description": "legacy request body"
            })
            .to_string(),
        )),
    )
    .await;
    assert_eq!(
        double_encoded.status,
        StatusCode::OK,
        "{}",
        double_encoded.body
    );
}

#[tokio::test]
async fn gateway_discovery_demo_invoke_and_presigned_rules_match_python() {
    let srv = TestServer::start("misc_auth").await;
    let experiment_id = srv.create_experiment("misc-auth-exp").await;
    let run_id = srv.create_run(&experiment_id).await;
    let (user, password) = srv.create_user("misc_user").await;
    let auth = Some((user.as_str(), password.as_str()));
    let secret_id = srv.create_gateway_secret().await;

    let discovery = send(
        &srv.base,
        Method::GET,
        "/ajax-api/3.0/mlflow/gateway/secrets/config",
        auth,
        None,
    )
    .await;
    assert_eq!(discovery.status, StatusCode::OK, "{}", discovery.body);
    assert!(!discovery.body.contains("using_default_passphrase"));
    let admin_discovery = send(
        &srv.base,
        Method::GET,
        "/ajax-api/3.0/mlflow/gateway/secrets/config",
        Some(ADMIN),
        None,
    )
    .await;
    assert!(admin_discovery.body.contains("using_default_passphrase"));

    let list_path = "/api/3.0/mlflow/gateway/secrets/list";
    let filtered = send(&srv.base, Method::GET, list_path, auth, None).await;
    assert_eq!(filtered.status, StatusCode::OK, "{}", filtered.body);
    assert_eq!(
        json!({"secrets": []}),
        serde_json::from_str::<Value>(&filtered.body).unwrap()
    );
    srv.grant(&user, "gateway_secret", &secret_id, "READ").await;
    let visible = send(&srv.base, Method::GET, list_path, auth, None).await;
    assert_eq!(visible.status, StatusCode::OK, "{}", visible.body);
    assert!(visible.body.contains(&secret_id));

    let generated = send(
        &srv.base,
        Method::POST,
        "/ajax-api/3.0/mlflow/demo/generate",
        auth,
        Some(json!({})),
    )
    .await;
    assert_ne!(
        generated.status,
        StatusCode::FORBIDDEN,
        "{}",
        generated.body
    );
    let deleted = send(
        &srv.base,
        Method::POST,
        "/ajax-api/3.0/mlflow/demo/delete",
        auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::FORBIDDEN, "{}", deleted.body);

    for path in [
        "/ajax-api/3.0/mlflow/issues/invoke",
        "/ajax-api/3.0/mlflow/genai/evaluate/invoke",
    ] {
        let denied = send(
            &srv.base,
            Method::POST,
            path,
            auth,
            Some(json!({"experiment_id": experiment_id})),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);
    }

    let presigned_path = "/api/2.0/mlflow/artifacts/presigned-upload-url";
    let denied = send(
        &srv.base,
        Method::POST,
        presigned_path,
        auth,
        Some(json!({"run_id": run_id, "path": "artifact.bin"})),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    srv.grant(&user, "experiment", &experiment_id, "EDIT").await;
    let allowed = send(
        &srv.base,
        Method::POST,
        presigned_path,
        auth,
        Some(json!({"run_id": run_id, "path": "artifact.bin"})),
    )
    .await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);
}

#[tokio::test]
async fn online_scoring_config_routes_enforce_every_experiment_permission() {
    let srv = TestServer::start("nd_online_configs").await;
    let exp_a = srv.create_experiment("online-auth-a").await;
    let exp_b = srv.create_experiment("online-auth-b").await;
    let scorer_a = srv
        .tracking
        .register_scorer(WS, &exp_a, "judge-a", "{}")
        .await
        .unwrap();
    let scorer_b = srv
        .tracking
        .register_scorer(WS, &exp_b, "judge-b", "{}")
        .await
        .unwrap();
    srv.tracking
        .upsert_online_scoring_config(WS, &exp_a, "judge-a", 0.0, None)
        .await
        .unwrap();
    srv.tracking
        .upsert_online_scoring_config(WS, &exp_b, "judge-b", 0.0, None)
        .await
        .unwrap();
    let (user, password) = srv.create_user("online_config_user").await;
    let auth = Some((user.as_str(), password.as_str()));
    let query_path = format!(
        "/api/3.0/mlflow/scorers/online-configs?scorer_ids={}&scorer_ids={}",
        scorer_a.scorer_id, scorer_b.scorer_id
    );

    let denied_without_grants = send(&srv.base, Method::GET, &query_path, auth, None).await;
    assert_eq!(denied_without_grants.status, StatusCode::FORBIDDEN);

    srv.grant(&user, "experiment", &exp_a, "READ").await;
    let denied_one_of_two = send(&srv.base, Method::GET, &query_path, auth, None).await;
    assert_eq!(denied_one_of_two.status, StatusCode::FORBIDDEN);

    srv.grant(&user, "experiment", &exp_b, "READ").await;
    let allowed_query = send(&srv.base, Method::GET, &query_path, auth, None).await;
    assert_eq!(
        allowed_query.status,
        StatusCode::OK,
        "{}",
        allowed_query.body
    );

    let allowed_body = send(
        &srv.base,
        Method::GET,
        "/ajax-api/3.0/mlflow/scorers/online-configs",
        auth,
        Some(json!({"scorer_ids": [scorer_a.scorer_id, scorer_b.scorer_id]})),
    )
    .await;
    assert_eq!(allowed_body.status, StatusCode::OK, "{}", allowed_body.body);

    let missing_ids = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/scorers/online-configs",
        auth,
        None,
    )
    .await;
    assert_eq!(missing_ids.status, StatusCode::BAD_REQUEST);

    let unrelated_query = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/scorers/online-configs?ignored=value",
        auth,
        None,
    )
    .await;
    assert_eq!(unrelated_query.status, StatusCode::BAD_REQUEST);

    let missing_experiment = send(
        &srv.base,
        Method::PUT,
        "/api/3.0/mlflow/scorers/online-config",
        auth,
        Some(json!({"name": "judge-a", "sample_rate": 0.0})),
    )
    .await;
    assert_eq!(missing_experiment.status, StatusCode::FORBIDDEN);

    let denied_update = send(
        &srv.base,
        Method::PUT,
        "/api/3.0/mlflow/scorers/online-config",
        auth,
        Some(json!({
            "experiment_id": exp_a,
            "name": "judge-a",
            "sample_rate": 0.0,
        })),
    )
    .await;
    assert_eq!(denied_update.status, StatusCode::FORBIDDEN);

    srv.grant(&user, "experiment", &exp_a, "EDIT").await;
    let allowed_update = send(
        &srv.base,
        Method::PUT,
        "/ajax-api/3.0/mlflow/scorers/online-config",
        auth,
        Some(json!({
            "experiment_id": exp_a,
            "name": "judge-a",
            "sample_rate": 0.0,
        })),
    )
    .await;
    assert_eq!(
        allowed_update.status,
        StatusCode::OK,
        "{}",
        allowed_update.body
    );
}

#[tokio::test]
async fn model_version_create_source_read_denied_with_no_default() {
    // The MV-create dual requirement's source-READ half: user has MANAGE on the
    // target model but no READ on the source run's experiment (and the default
    // is NO_PERMISSIONS), so anchoring a model version at that run is denied.
    let srv = TestServer::start("nd_mv").await;
    let exp = srv.create_experiment("nd-mv-exp").await;
    let run = srv.create_run(&exp).await;
    let (u, pw) = srv.create_user("olga_nd").await;
    srv.grant(&u, "registered_model", "nd-model", "MANAGE")
        .await;

    let denied = send(
        &srv.base,
        Method::POST,
        "/api/2.0/mlflow/model-versions/create",
        Some((&u, &pw)),
        Some(json!({"name": "nd-model", "source": "s3://x", "run_id": run})),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);
    assert_eq!(denied.body, "Permission denied");

    // Grant READ on the source experiment → the source-read half now passes
    // (gate reaches the handler, which may 400 on the source URI but not 403).
    srv.grant(&u, "experiment", &exp, "READ").await;
    let allowed = send(
        &srv.base,
        Method::POST,
        "/api/2.0/mlflow/model-versions/create",
        Some((&u, &pw)),
        Some(json!({"name": "nd-model", "source": "s3://x", "run_id": run})),
    )
    .await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);
}

#[tokio::test]
async fn presigned_download_routes_require_run_or_experiment_read() {
    let srv = TestServer::start("nd_presigned").await;
    let exp = srv.create_experiment("nd-presigned-exp").await;
    let run = srv.create_run(&exp).await;
    let (user, password) = srv.create_user("presigned_reader").await;

    let rpc = "/api/2.0/mlflow/artifacts/presigned-download-url";
    let denied = send(
        &srv.base,
        Method::POST,
        rpc,
        Some((&user, &password)),
        Some(json!({"run_id": run, "path": "model.bin"})),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    let proxy =
        format!("/ajax-api/2.0/mlflow-artifacts/presigned/{exp}/run-id/artifacts/model.bin");
    let denied = send(
        &srv.base,
        Method::GET,
        &proxy,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    srv.grant(&user, "experiment", &exp, "READ").await;
    let allowed = send(
        &srv.base,
        Method::POST,
        rpc,
        Some((&user, &password)),
        Some(json!({"run_id": run, "path": "model.bin"})),
    )
    .await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);
    let allowed = send(
        &srv.base,
        Method::GET,
        &proxy,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);
}

#[tokio::test]
async fn artifacts_only_honors_default_workspace_grant_without_tracking_store() {
    let srv = TestServer::start_artifacts_only("nd_artifacts_only_grant").await;
    let experiment_id = "987654321";
    let path =
        format!("/api/2.0/mlflow-artifacts/artifacts/{experiment_id}/run-id/artifacts/model.bin");

    let (granted_user, granted_password) = srv.create_user("artifact_grantee").await;
    let denied = send(
        &srv.base,
        Method::GET,
        &path,
        Some((&granted_user, &granted_password)),
        None,
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    srv.grant(&granted_user, "experiment", experiment_id, "READ")
        .await;
    let allowed = send(
        &srv.base,
        Method::GET,
        &path,
        Some((&granted_user, &granted_password)),
        None,
    )
    .await;
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);
    assert_eq!(allowed.status, StatusCode::NOT_FOUND, "{}", allowed.body);

    let (stranger, stranger_password) = srv.create_user("artifact_stranger").await;
    let no_grant = send(
        &srv.base,
        Method::GET,
        &path,
        Some((&stranger, &stranger_password)),
        None,
    )
    .await;
    assert_eq!(no_grant.status, StatusCode::FORBIDDEN, "{}", no_grant.body);
}

#[tokio::test]
async fn static_prefixed_presigned_proxy_route_is_authorized() {
    let srv = TestServer::start_with_prefix("nd_presigned_static", Some("/mlflow")).await;
    let exp = srv.create_experiment("nd-presigned-static-exp").await;
    let (user, password) = srv.create_user("presigned_static_reader").await;
    let path =
        format!("/mlflow/api/2.0/mlflow-artifacts/presigned/{exp}/run-id/artifacts/model.bin");
    let denied = send(
        &srv.base,
        Method::GET,
        &path,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);
}

#[tokio::test]
async fn mcp_server_permissions_follow_read_edit_manage_lattice() {
    let srv = TestServer::start("nd_mcp_permissions").await;
    let name = "com.example/auth-existing";
    srv.tracking
        .create_mcp_server(WS, name, None, None, None)
        .await
        .unwrap();
    let (user, password) = srv.create_user("mcp_reader").await;
    let path = format!("/api/3.0/mlflow/mcp-servers/{name}");

    let denied = send(
        &srv.base,
        Method::GET,
        &path,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    srv.grant(&user, "mcp_server", name, "READ").await;
    let readable = send(
        &srv.base,
        Method::GET,
        &path,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(readable.status, StatusCode::OK, "{}", readable.body);
    let not_editable = send(
        &srv.base,
        Method::PATCH,
        &path,
        Some((&user, &password)),
        Some(json!({"description": "denied"})),
    )
    .await;
    assert_eq!(
        not_editable.status,
        StatusCode::FORBIDDEN,
        "{}",
        not_editable.body
    );

    srv.grant(&user, "mcp_server", name, "EDIT").await;
    let editable = send(
        &srv.base,
        Method::PATCH,
        &path,
        Some((&user, &password)),
        Some(json!({"description": "allowed"})),
    )
    .await;
    assert_eq!(editable.status, StatusCode::OK, "{}", editable.body);
    let not_deletable = send(
        &srv.base,
        Method::DELETE,
        &path,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(
        not_deletable.status,
        StatusCode::FORBIDDEN,
        "{}",
        not_deletable.body
    );

    srv.grant(&user, "mcp_server", name, "MANAGE").await;
    let deleted = send(
        &srv.base,
        Method::DELETE,
        &path,
        Some((&user, &password)),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
}

#[tokio::test]
async fn mcp_server_creator_gets_manage_for_root_and_auto_created_parent() {
    let srv = TestServer::start("nd_mcp_creator").await;
    let (creator, password) = srv.create_user("mcp_creator").await;
    let (other, other_password) = srv.create_user("mcp_other").await;

    let created = send(
        &srv.base,
        Method::POST,
        "/api/3.0/mlflow/mcp-servers",
        Some((&creator, &password)),
        Some(json!({"name": "com.example/auth-created"})),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let creator_read = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/mcp-servers/com.example/auth-created",
        Some((&creator, &password)),
        None,
    )
    .await;
    assert_eq!(creator_read.status, StatusCode::OK, "{}", creator_read.body);
    let other_denied = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/mcp-servers/com.example/auth-created",
        Some((&other, &other_password)),
        None,
    )
    .await;
    assert_eq!(
        other_denied.status,
        StatusCode::FORBIDDEN,
        "{}",
        other_denied.body
    );

    let deleted = send(
        &srv.base,
        Method::DELETE,
        "/api/3.0/mlflow/mcp-servers/com.example/auth-created",
        Some((&creator, &password)),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    srv.tracking
        .create_mcp_server(WS, "com.example/auth-created", None, None, None)
        .await
        .unwrap();
    let stale_grant_denied = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/mcp-servers/com.example/auth-created",
        Some((&creator, &password)),
        None,
    )
    .await;
    assert_eq!(
        stale_grant_denied.status,
        StatusCode::FORBIDDEN,
        "{}",
        stale_grant_denied.body
    );

    let auto_created = send(
        &srv.base,
        Method::POST,
        "/api/3.0/mlflow/mcp-servers/com.example/auto-parent/versions",
        Some((&creator, &password)),
        Some(json!({
            "server_json": {"name": "com.example/auto-parent", "version": "1.0.0"}
        })),
    )
    .await;
    assert_eq!(auto_created.status, StatusCode::OK, "{}", auto_created.body);
    let auto_parent_read = send(
        &srv.base,
        Method::GET,
        "/api/3.0/mlflow/mcp-servers/com.example/auto-parent",
        Some((&creator, &password)),
        None,
    )
    .await;
    assert_eq!(
        auto_parent_read.status,
        StatusCode::OK,
        "{}",
        auto_parent_read.body
    );
}
