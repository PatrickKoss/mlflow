use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use mlflow_auth::{AuthDb, AuthStore};
use mlflow_server::assistant::AssistantRuntime;
use mlflow_server::{build_app_with_recorder, AppState, ServerConfig};
use mlflow_store::{Db, JobStore, PoolConfig, TrackingStore};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const STATIC_PREFIX: &str = "/p";

fn tracking_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tracking.db")
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

struct Fixture {
    _directory: TempDir,
    app: axum::Router,
    jobs: JobStore,
}

impl Fixture {
    async fn new(with_auth: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let tracking_path = directory.path().join("tracking.db");
        std::fs::copy(tracking_fixture_path(), &tracking_path).unwrap();
        let db = Db::connect(
            &format!("sqlite:///{}", tracking_path.display()),
            PoolConfig::default(),
        )
        .await
        .unwrap();
        let jobs = JobStore::new(db.clone());
        let runtime = AssistantRuntime::new(
            directory.path().join("sessions"),
            directory.path().join("assistant-config.json"),
            directory.path().join("skills"),
            directory.path().join("home"),
            Vec::new(),
        );
        let mut state = AppState::with_artifacts(
            TrackingStore::new(db, directory.path().join("artifacts").display().to_string()),
            false,
            None,
            None,
        )
        .with_assistant_runtime(runtime);
        if with_auth {
            let auth_path = directory.path().join("basic_auth.db");
            std::fs::copy(auth_fixture_path(), &auth_path).unwrap();
            let auth_db = AuthDb::connect_and_verify_with(
                &format!("sqlite:///{}", auth_path.display()),
                None,
                PoolConfig::default(),
            )
            .await
            .unwrap();
            state = state.with_auth_store(AuthStore::new(auth_db));
        }
        let config = ServerConfig {
            static_prefix: Some(STATIC_PREFIX.to_string()),
            disable_security_middleware: true,
            ..Default::default()
        };
        let recorder = PrometheusBuilder::new().build_recorder().handle();
        let app = build_app_with_recorder(&config, recorder, Some(state));
        Self {
            _directory: directory,
            app,
            jobs,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:5000");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&body).unwrap())
            }
            None => Body::empty(),
        };
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            5001,
        )));
        self.app.clone().oneshot(request).await.unwrap()
    }
}

#[tokio::test]
async fn native_routers_are_only_served_under_static_prefix() {
    let fixture = Fixture::new(false).await;
    let job = fixture
        .jobs
        .create_job("default", "static_prefix", "{}", None)
        .await
        .unwrap();
    let probes = [
        (
            Method::GET,
            format!("/ajax-api/3.0/jobs/{}", job.job_id),
            None,
        ),
        (
            Method::GET,
            "/ajax-api/3.0/mlflow/assistant/config".to_string(),
            None,
        ),
        (
            Method::POST,
            "/gateway/mlflow/v1/chat/completions".to_string(),
            Some(json!({})),
        ),
        (Method::POST, "/v1/traces".to_string(), Some(json!({}))),
    ];

    for (method, path, body) in probes {
        let prefixed = fixture
            .request(
                method.clone(),
                &format!("{STATIC_PREFIX}{path}"),
                body.clone(),
                &[],
            )
            .await;
        assert_ne!(prefixed.status(), StatusCode::NOT_FOUND, "{path}");
        let bare = fixture.request(method, &path, body, &[]).await;
        assert_eq!(bare.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn assistant_stream_url_includes_static_prefix() {
    let fixture = Fixture::new(false).await;
    let response = fixture
        .request(
            Method::POST,
            "/p/ajax-api/3.0/mlflow/assistant/message",
            Some(json!({"message": "hello"})),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(value["stream_url"]
        .as_str()
        .unwrap()
        .starts_with("/p/ajax-api/3.0/mlflow/assistant/sessions/"));
}

#[tokio::test]
async fn auth_dispatch_uses_the_unprefixed_gateway_path() {
    let fixture = Fixture::new(true).await;
    let credentials =
        base64::engine::general_purpose::STANDARD.encode("alice_scrypt:alice-password-123");
    let mlflow_authorization = format!("Basic {credentials}");
    let response = fixture
        .request(
            Method::POST,
            "/p/gateway/mlflow/v1/chat/completions",
            Some(json!({})),
            &[
                ("authorization", "Bearer provider-key"),
                ("x-mlflow-authorization", &mlflow_authorization),
            ],
        )
        .await;
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response
        .headers()
        .contains_key("x-mlflow-gateway-duration-ms"));
}

#[tokio::test]
async fn artifact_proxy_is_served_prefixed_and_unprefixed() {
    let fixture = Fixture::new(false).await;
    let path = "/api/2.0/mlflow-artifacts/artifacts/file.txt";
    let bare = fixture.request(Method::GET, path, None, &[]).await;
    assert_eq!(bare.status(), StatusCode::SERVICE_UNAVAILABLE);
    let prefixed = fixture
        .request(Method::GET, &format!("{STATIC_PREFIX}{path}"), None, &[])
        .await;
    assert_eq!(prefixed.status(), StatusCode::SERVICE_UNAVAILABLE);
}
