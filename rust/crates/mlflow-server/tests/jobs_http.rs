//! Wire tests for the legacy Flask and native FastAPI generic-jobs aliases.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use mlflow_auth::{AuthDb, AuthStore};
use mlflow_server::{build_app_with_recorder, AppState, ServerConfig};
use mlflow_store::{Db, JobStatus, JobStore, PoolConfig, TrackingStore};
use serde_json::{json, Value};
use tower::ServiceExt;

const WS: &str = "default";

fn fixture_path() -> PathBuf {
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
    path: PathBuf,
    auth_path: Option<PathBuf>,
    jobs: JobStore,
    app: axum::Router,
}

impl Fixture {
    async fn new(tag: &str) -> Self {
        Self::new_with_auth(tag, false).await
    }

    async fn new_with_auth(tag: &str, with_auth: bool) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mlflow_rust_jobs_http_{tag}_{}_{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::copy(fixture_path(), &path).expect("copy fixture");
        let uri = format!("sqlite:///{}", path.display());
        let db = Db::connect(&uri, PoolConfig::default())
            .await
            .expect("connect fixture");
        let jobs = JobStore::new(db.clone());
        let mut state = AppState::new(TrackingStore::new(db, "s3://bucket/mlruns"));
        let auth_path = if with_auth {
            let auth_path = path.with_extension("auth.db");
            std::fs::copy(auth_fixture_path(), &auth_path).expect("copy auth fixture");
            let auth_uri = format!("sqlite:///{}", auth_path.display());
            let auth_db = AuthDb::connect_and_verify_with(&auth_uri, None, PoolConfig::default())
                .await
                .expect("connect auth fixture");
            state = state.with_auth_store(AuthStore::new(auth_db));
            Some(auth_path)
        } else {
            None
        };
        let config = ServerConfig {
            disable_security_middleware: true,
            ..Default::default()
        };
        let recorder = PrometheusBuilder::new().build_recorder().handle();
        let app = build_app_with_recorder(&config, recorder, Some(state));
        Self {
            path,
            auth_path,
            jobs,
            app,
        }
    }

    async fn request(&self, method: Method, path: &str) -> (StatusCode, String, String) {
        self.request_with_auth(method, path, None).await
    }

    async fn request_with_auth(
        &self,
        method: Method,
        path: &str,
        authorization: Option<&str>,
    ) -> (StatusCode, String, String) {
        self.request_json_with_auth(method, path, authorization, None)
            .await
    }

    async fn request_json_with_auth(
        &self,
        method: Method,
        path: &str,
        authorization: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, String, String) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            String::from_utf8(body.to_vec()).unwrap(),
            content_type,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(path) = &self.auth_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tokio::test]
async fn flask_alias_matches_python_bytes_for_get_and_cancel() {
    let fixture = Fixture::new("flask").await;
    let job = fixture
        .jobs
        .create_job(WS, "wire_job", r#"{"a": 1}"#, Some(2.5))
        .await
        .unwrap();
    let (status, body, content_type) = fixture
        .request(
            Method::GET,
            &format!("/ajax-api/3.0/mlflow/jobs/{}", job.job_id),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json");
    assert_eq!(
        body,
        "{\"result\":null,\"status\":\"PENDING\",\"status_details\":null}\n"
    );

    let (status, body, _) = fixture
        .request(
            Method::PATCH,
            &format!("/ajax-api/3.0/mlflow/jobs/cancel/{}", job.job_id),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"result\":null,\"status\":\"CANCELED\"}\n");
    assert_eq!(
        fixture.jobs.get_job(WS, &job.job_id).await.unwrap().status,
        JobStatus::Canceled
    );
}

#[tokio::test]
async fn fastapi_alias_returns_the_complete_job_model() {
    let fixture = Fixture::new("fastapi").await;
    let job = fixture
        .jobs
        .create_job(WS, "wire_job", r#"{"a": 1}"#, Some(2.5))
        .await
        .unwrap();
    fixture
        .jobs
        .update_status_details(WS, &job.job_id, &json!({"progress": 10}))
        .await
        .unwrap();
    let (status, body, content_type) = fixture
        .request(Method::GET, &format!("/ajax-api/3.0/jobs/{}", job.job_id))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["job_id"], job.job_id);
    assert_eq!(body["job_name"], "wire_job");
    assert_eq!(body["params"], json!({"a": 1}));
    assert_eq!(body["timeout"], 2.5);
    assert_eq!(body["status"], "PENDING");
    assert_eq!(body["result"], Value::Null);
    assert_eq!(body["retry_count"], 0);
    assert_eq!(body["status_details"], json!({"progress": 10}));
    assert!(body.get("creator").is_none());
    assert!(body["creation_time"].is_i64());
    assert!(body["last_update_time"].is_i64());
}

#[tokio::test]
async fn missing_and_finalized_errors_keep_mlflow_shape() {
    let fixture = Fixture::new("errors").await;
    let (status, body, _) = fixture
        .request(Method::GET, "/ajax-api/3.0/mlflow/jobs/missing")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("\"error_code\": \"RESOURCE_DOES_NOT_EXIST\""));
    assert!(body.contains("Job with ID missing not found"));

    let job = fixture
        .jobs
        .create_job(WS, "done", "{}", None)
        .await
        .unwrap();
    fixture
        .jobs
        .finish_job(WS, &job.job_id, "null")
        .await
        .unwrap();
    let (status, body, _) = fixture
        .request(
            Method::PATCH,
            &format!("/ajax-api/3.0/mlflow/jobs/cancel/{}", job.job_id),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("already finalized with status: SUCCEEDED"));
}

#[tokio::test]
async fn fastapi_alias_errors_use_http_exception_shape() {
    let fixture = Fixture::new("fastapi_errors").await;
    let (status, body, content_type) = fixture
        .request(Method::GET, "/ajax-api/3.0/jobs/missing")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type, "application/json");
    assert_eq!(body, r#"{"detail":"Job with ID missing not found"}"#);
}

#[tokio::test]
async fn native_submit_records_creator_and_search_only_requires_authentication() {
    let fixture = Fixture::new_with_auth("auth", true).await;
    let credentials =
        base64::engine::general_purpose::STANDARD.encode("bob_pbkdf2:bob-password-4567");
    let authorization = format!("Basic {credentials}");
    let payload = json!({
        "job_name": "run_online_trace_scorer",
        "params": {"experiment_id": "0", "online_scorers": []}
    });

    let (status, _, _) = fixture
        .request_json_with_auth(
            Method::POST,
            "/ajax-api/3.0/jobs/",
            None,
            Some(payload.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body, _) = fixture
        .request_json_with_auth(
            Method::POST,
            "/ajax-api/3.0/jobs/",
            Some(&authorization),
            Some(payload),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body.get("creator").is_none());
    let job = fixture
        .jobs
        .get_job(WS, body["job_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(job.creator.as_deref(), Some("bob_pbkdf2"));

    let (status, _, _) = fixture
        .request_json_with_auth(
            Method::POST,
            "/ajax-api/3.0/jobs/search",
            None,
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body, _) = fixture
        .request_json_with_auth(
            Method::POST,
            "/ajax-api/3.0/jobs/search",
            Some(&authorization),
            Some(json!({"job_name": "run_online_trace_scorer"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["jobs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn owner_and_admin_pass_while_non_owner_is_forbidden() {
    let fixture = Fixture::new_with_auth("ownership", true).await;
    let bob = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("bob_pbkdf2:bob-password-4567")
    );
    let admin = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("alice_scrypt:alice-password-123")
    );

    for path in ["/ajax-api/3.0/mlflow/jobs/{}", "/ajax-api/3.0/jobs/{}"] {
        let owned = fixture
            .jobs
            .create_job_with_creator(WS, "auth_job", "{}", None, Some("bob_pbkdf2"))
            .await
            .unwrap();
        let path = path.replace("{}", &owned.job_id);
        assert_eq!(
            fixture
                .request_with_auth(Method::GET, &path, Some(&bob))
                .await
                .0,
            StatusCode::OK
        );

        let other = fixture
            .jobs
            .create_job_with_creator(WS, "auth_job", "{}", None, Some("someone_else"))
            .await
            .unwrap();
        let path = path.replace(&owned.job_id, &other.job_id);
        let (status, body, _) = fixture
            .request_with_auth(Method::GET, &path, Some(&bob))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "Permission denied");
        assert_eq!(
            fixture
                .request_with_auth(Method::GET, &path, Some(&admin))
                .await
                .0,
            StatusCode::OK
        );
    }
}
