//! T20.1 Assistant route, session-store, remote-access, and SSE coverage.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, Method, Response, StatusCode};
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use http_body_util::BodyExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use mlflow_server::assistant::{
    AssistantEvent, AssistantProvider, AssistantProviderError, AssistantProviderRequest,
    AssistantRuntime,
};
use mlflow_server::{build_app_with_recorder, AppState, ServerConfig};
use mlflow_store::{Db, PoolConfig, TrackingStore, WORKSPACE_DEFAULT_NAME};
use serde_json::{json, Value};
use tower::ServiceExt;

const PREFIX: &str = "/ajax-api/3.0/mlflow/assistant";
type ModelCalls = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tracking.db")
}

#[derive(Debug)]
struct ScriptedProvider {
    requests: Arc<Mutex<Vec<AssistantProviderRequest>>>,
    model_calls: ModelCalls,
}

impl AssistantProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn resolve_skills_path(&self, base_directory: &Path) -> PathBuf {
        base_directory.join(".scripted/skills")
    }

    fn check_connection(
        &self,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<(), AssistantProviderError>> {
        async { Ok(()) }.boxed()
    }

    fn list_models(
        &self,
        base_url: Option<String>,
        api_key: Option<String>,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<Vec<String>, AssistantProviderError>> {
        self.model_calls.lock().unwrap().push((base_url, api_key));
        async { Ok(vec!["fixture-a".to_string(), "fixture-b".to_string()]) }.boxed()
    }

    fn stream(&self, request: AssistantProviderRequest) -> BoxStream<'static, AssistantEvent> {
        let error_only = request.prompt == "error only";
        self.requests.lock().unwrap().push(request);
        if error_only {
            return stream::iter(vec![AssistantEvent::error_with_session(
                "scripted error",
                Some("provider-error-session"),
            )])
            .boxed();
        }
        stream::iter(vec![
            AssistantEvent::new(
                "message",
                json!({"message": {"role": "assistant", "content": "hello"}}),
            ),
            AssistantEvent::new(
                "stream_event",
                json!({"event": {"type": "content_delta", "delta": {"text": "!"}}}),
            ),
            AssistantEvent::new(
                "permission_request",
                json!({"request_id": "tool-1", "tool_name": "Bash", "tool_input": {"command": "pwd"}}),
            ),
            AssistantEvent::new("interrupted", json!({"message": "Assistant was interrupted"})),
            AssistantEvent::new("error", json!({"error": "scripted error"})),
            AssistantEvent::new(
                "done",
                json!({"result": null, "session_id": "provider-session-1"}),
            ),
        ])
        .boxed()
    }
}

#[derive(Debug)]
struct FailingProvider {
    name: &'static str,
    health: AssistantProviderError,
    models: AssistantProviderError,
}

impl AssistantProvider for FailingProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn client_tool_delivery(&self) -> &'static str {
        if self.name == "mlflow_gateway" {
            "tool"
        } else {
            "unsupported"
        }
    }

    fn resolve_skills_path(&self, base_directory: &Path) -> PathBuf {
        base_directory.join(".fixture/skills")
    }

    fn check_connection(
        &self,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<(), AssistantProviderError>> {
        let error = self.health.clone();
        async move { Err(error) }.boxed()
    }

    fn list_models(
        &self,
        _base_url: Option<String>,
        _api_key: Option<String>,
        _config: Option<Value>,
    ) -> BoxFuture<'static, Result<Vec<String>, AssistantProviderError>> {
        let error = self.models.clone();
        async move { Err(error) }.boxed()
    }

    fn stream(&self, _request: AssistantProviderRequest) -> BoxStream<'static, AssistantEvent> {
        stream::empty().boxed()
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    app: axum::Router,
    runtime: AssistantRuntime,
    store: TrackingStore,
    requests: Arc<Mutex<Vec<AssistantProviderRequest>>>,
    model_calls: ModelCalls,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("assistant.db");
        std::fs::copy(fixture_path(), &db_path).unwrap();
        let db = Db::connect(
            &format!("sqlite:///{}", db_path.display()),
            PoolConfig::default(),
        )
        .await
        .unwrap();
        let store =
            TrackingStore::new(db, directory.path().join("artifacts").display().to_string());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model_calls = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(ScriptedProvider {
            requests: requests.clone(),
            model_calls: model_calls.clone(),
        });
        let skills = directory.path().join("bundled-skills");
        std::fs::create_dir_all(skills.join("alpha")).unwrap();
        std::fs::write(skills.join("alpha/SKILL.md"), "---\nname: alpha\n---\n").unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let providers: Vec<Arc<dyn AssistantProvider>> = vec![
            provider,
            Arc::new(FailingProvider {
                name: "not_implemented",
                health: AssistantProviderError::NotImplemented("fixture no probe".to_string()),
                models: AssistantProviderError::NotImplemented(String::new()),
            }),
            Arc::new(FailingProvider {
                name: "cli_missing",
                health: AssistantProviderError::CliNotInstalled("fixture cli missing".to_string()),
                models: AssistantProviderError::CliNotInstalled("fixture cli missing".to_string()),
            }),
            Arc::new(FailingProvider {
                name: "auth_missing",
                health: AssistantProviderError::NotAuthenticated(
                    "fixture auth missing".to_string(),
                ),
                models: AssistantProviderError::NotConfigured("fixture models missing".to_string()),
            }),
            Arc::new(FailingProvider {
                name: "mlflow_gateway",
                health: AssistantProviderError::NotImplemented(String::new()),
                models: AssistantProviderError::NotImplemented(String::new()),
            }),
        ];
        let runtime = AssistantRuntime::new(
            directory.path().join("sessions"),
            home.join(".mlflow/assistant/config.json"),
            skills,
            home,
            providers,
        );
        let recorder = PrometheusBuilder::new().build_recorder().handle();
        let app = build_app_with_recorder(
            &ServerConfig {
                disable_security_middleware: true,
                ..Default::default()
            },
            recorder,
            Some(AppState::new(store.clone()).with_assistant_runtime(runtime.clone())),
        );
        Self {
            _directory: directory,
            app,
            runtime,
            store,
            requests,
            model_calls,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        peer: IpAddr,
        extra_headers: &[(&str, &str)],
    ) -> Response<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:5000");
        for (name, value) in extra_headers {
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
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer, 4242)));
        self.app.clone().oneshot(request).await.unwrap()
    }

    async fn local(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, axum::http::HeaderMap, Bytes) {
        collect(
            self.request(method, path, body, IpAddr::V4(Ipv4Addr::LOCALHOST), &[])
                .await,
        )
        .await
    }

    async fn select_provider(&self) {
        let (status, _, _) = self
            .local(
                Method::PUT,
                &format!("{PREFIX}/config"),
                Some(json!({"providers": {"scripted": {"selected": true}}})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn gateway_api_keys_validate_and_create_idempotent_llm_connections() {
    let fixture = Fixture::new().await;
    for (provider, update, detail) in [
        (
            "mlflow_gateway",
            json!({"gateway_vendor":"openai"}),
            "Gateway vendor connections require an API key.",
        ),
        (
            "claude_code",
            json!({"api_key":"obvious-fake-key"}),
            "API keys must be stored in LLM Connections through the 'mlflow_gateway' provider.",
        ),
        (
            "mlflow_gateway",
            json!({"api_key":"obvious-fake-key"}),
            "Gateway API keys require a gateway_vendor.",
        ),
        (
            "mlflow_gateway",
            json!({"api_key":"obvious-fake-key","gateway_vendor":"unknown"}),
            "Unknown Gateway vendor: 'unknown'",
        ),
    ] {
        let (status, _, body) = fixture
            .local(
                Method::PUT,
                &format!("{PREFIX}/config"),
                Some(json!({"providers": {provider: update}})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json_body(&body), json!({"detail": detail}));
    }

    for api_key in ["obvious-fake-key-one", "obvious-fake-key-two"] {
        let (status, _, body) = fixture
            .local(
                Method::PUT,
                &format!("{PREFIX}/config"),
                Some(json!({
                    "providers": {
                        "mlflow_gateway": {
                            "api_key": api_key,
                            "gateway_vendor": "openai",
                            "selected": true,
                        }
                    }
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let response = json_body(&body);
        assert_eq!(
            response["providers"]["mlflow_gateway"]["model"],
            "mlflow-assistant-openai"
        );
        assert!(response["providers"]["mlflow_gateway"]
            .get("api_key")
            .is_none());
    }

    let name = "mlflow-assistant-openai";
    let secret = fixture
        .store
        .get_gateway_secret_info(WORKSPACE_DEFAULT_NAME, None, Some(name))
        .await
        .unwrap();
    assert_eq!(secret.provider.as_deref(), Some("openai"));
    assert_eq!(
        fixture
            .store
            .get_decrypted_gateway_secret(WORKSPACE_DEFAULT_NAME, &secret.secret_id)
            .await
            .unwrap(),
        json!({"api_key":"obvious-fake-key-two"})
    );
    let model = fixture
        .store
        .get_gateway_model_definition(WORKSPACE_DEFAULT_NAME, None, Some(name))
        .await
        .unwrap();
    assert_eq!(model.secret_id.as_deref(), Some(secret.secret_id.as_str()));
    assert_eq!(model.provider, "openai");
    assert_eq!(model.model_name, "gpt-5.5");
    let endpoint = fixture
        .store
        .get_gateway_endpoint(WORKSPACE_DEFAULT_NAME, None, Some(name))
        .await
        .unwrap();
    assert_eq!(endpoint.model_mappings.len(), 1);
    assert_eq!(
        endpoint.model_mappings[0].model_definition_id,
        model.model_definition_id
    );
    assert_eq!(
        fixture
            .store
            .list_gateway_model_definitions(WORKSPACE_DEFAULT_NAME, None, None)
            .await
            .unwrap()
            .iter()
            .filter(|definition| definition.name == name)
            .count(),
        1
    );
    assert_eq!(
        fixture
            .store
            .list_gateway_endpoints(WORKSPACE_DEFAULT_NAME, None, None)
            .await
            .unwrap()
            .iter()
            .filter(|endpoint| endpoint.name.as_deref() == Some(name))
            .count(),
        1
    );
    let config_text = std::fs::read_to_string(
        fixture
            ._directory
            .path()
            .join("home/.mlflow/assistant/config.json"),
    )
    .unwrap();
    assert!(!config_text.contains("obvious-fake-key"));

    let (status, _, body) = fixture
        .local(Method::GET, &format!("{PREFIX}/providers"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body)["resolved"],
        json!({
            "name":"mlflow_gateway",
            "model":"mlflow-assistant-openai",
            "auto_selected":false,
            "requires_api_key":false,
            "has_api_key":true,
            "client_tool_delivery":"tool",
            "model_provider":"openai",
            "model_options":["gpt-5.5"],
            "provider_model":"gpt-5.5",
        })
    );
}

#[tokio::test]
async fn providers_response_has_exact_none_shape_for_an_empty_registry() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("assistant-empty.db");
    std::fs::copy(fixture_path(), &db_path).unwrap();
    let db = Db::connect(
        &format!("sqlite:///{}", db_path.display()),
        PoolConfig::default(),
    )
    .await
    .unwrap();
    let store = TrackingStore::new(db, directory.path().join("artifacts").display().to_string());
    let home = directory.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let runtime = AssistantRuntime::new(
        directory.path().join("sessions"),
        home.join(".mlflow/assistant/config.json"),
        directory.path().join("skills"),
        home,
        Vec::new(),
    );
    let recorder = PrometheusBuilder::new().build_recorder().handle();
    let app = build_app_with_recorder(
        &ServerConfig {
            disable_security_middleware: true,
            ..Default::default()
        },
        recorder,
        Some(AppState::new(store).with_assistant_runtime(runtime)),
    );
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(format!("{PREFIX}/providers"))
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        4242,
    )));
    let (_, _, body) = collect(app.oneshot(request).await.unwrap()).await;
    assert_eq!(
        json_body(&body),
        json!({
            "providers":[],
            "resolved":null,
            "gateway_vendor_options":{
                "openai":["gpt-5.5"],
                "anthropic":["claude-sonnet-5"],
                "gemini":["gemini-3-pro"],
            },
        })
    );
}

async fn collect(response: Response<Body>) -> (StatusCode, axum::http::HeaderMap, Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

fn json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

#[tokio::test]
async fn remote_access_policy_covers_all_routes_and_accepts_ipv6_loopback() {
    let fixture = Fixture::new().await;
    let gated_routes = [
        (Method::POST, format!("{PREFIX}/message")),
        (Method::GET, format!("{PREFIX}/sessions/id/stream")),
        (Method::PATCH, format!("{PREFIX}/sessions/id")),
        (Method::POST, format!("{PREFIX}/sessions/id/permission")),
        (Method::POST, format!("{PREFIX}/sessions/id/tool-result")),
        (Method::GET, format!("{PREFIX}/providers/id/health")),
        (Method::PUT, format!("{PREFIX}/config")),
        (Method::POST, format!("{PREFIX}/skills/install")),
        (Method::GET, format!("{PREFIX}/providers/id/models")),
    ];
    for (method, path) in gated_routes {
        let (status, _, body) = collect(
            fixture
                .request(
                    method,
                    &path,
                    None,
                    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                    &[],
                )
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(
            body,
            r#"{"detail":"Assistant API is only accessible from the same host where the MLflow server is running."}"#
        );
    }

    let (status, _, body) = collect(
        fixture
            .request(
                Method::GET,
                &format!("{PREFIX}/config"),
                None,
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                &[],
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"providers":{},"projects":{},"remote_access_allowed":false}"#
    );

    let (status, _, body) = collect(
        fixture
            .request(
                Method::GET,
                &format!("{PREFIX}/config"),
                None,
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                &[],
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"providers":{},"projects":{},"remote_access_allowed":false}"#
    );
}

#[tokio::test]
async fn config_health_models_and_skills_routes_match_python_shapes() {
    let fixture = Fixture::new().await;
    let (status, _, body) = fixture
        .local(Method::GET, &format!("{PREFIX}/config"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"providers":{},"projects":{},"remote_access_allowed":false}"#
    );

    let project = fixture._directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let update = json!({
        "providers": {"scripted": {"model": "fixture", "selected": true, "permissions": {"full_access": true}}},
        "projects": {"7": {"location": project}},
    });
    let (status, _, body) = fixture
        .local(Method::PUT, &format!("{PREFIX}/config"), Some(update))
        .await;
    assert_eq!(status, StatusCode::OK);
    let config = json_body(&body);
    assert_eq!(config["providers"]["scripted"]["model"], "fixture");
    assert_eq!(config["providers"]["scripted"]["selected"], true);
    assert_eq!(
        config["providers"]["scripted"]["permissions"],
        json!({"allow_edit_files": true, "allow_read_docs": true, "full_access": true})
    );
    assert_eq!(config["projects"]["7"]["type"], "local");

    let (status, _, body) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/providers/scripted/health"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"status":"ok"}"#);
    let (status, _, body) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/providers/missing/health"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, r#"{"detail":"Provider 'missing' not found"}"#);
    for (provider, status, detail) in [
        (
            "not_implemented",
            StatusCode::NOT_IMPLEMENTED,
            "fixture no probe",
        ),
        (
            "cli_missing",
            StatusCode::PRECONDITION_FAILED,
            "fixture cli missing",
        ),
        (
            "auth_missing",
            StatusCode::UNAUTHORIZED,
            "fixture auth missing",
        ),
    ] {
        let (actual, _, body) = fixture
            .local(
                Method::GET,
                &format!("{PREFIX}/providers/{provider}/health"),
                None,
            )
            .await;
        assert_eq!(actual, status);
        assert_eq!(json_body(&body), json!({"detail": detail}));
    }

    let response = fixture
        .request(
            Method::GET,
            &format!("{PREFIX}/providers/scripted/models?base_url=http%3A%2F%2Ffixture"),
            None,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &[("x-api-key", "obvious-fake-model-key")],
        )
        .await;
    let (status, _, body) = collect(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"models":["fixture-a","fixture-b"]}"#);
    assert_eq!(
        fixture.model_calls.lock().unwrap().as_slice(),
        &[(
            Some("http://fixture".to_string()),
            Some("obvious-fake-model-key".to_string())
        )]
    );
    let (status, _, body) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/providers/not_implemented/models"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&body),
        json!({"detail": "Model listing is not supported for provider 'not_implemented'"})
    );
    let (status, _, body) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/providers/auth_missing/models"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(&body),
        json!({"detail": "fixture models missing"})
    );

    let (status, _, body) = fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/skills/install"),
            Some(json!({"type": "project", "experiment_id": "7"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let installed = json_body(&body);
    assert_eq!(installed["installed_skills"], json!(["alpha"]));
    assert_eq!(
        installed["skills_directory"],
        project.join(".scripted/skills").to_string_lossy().as_ref()
    );
    assert!(project.join(".scripted/skills/alpha/SKILL.md").is_file());
}

#[tokio::test]
async fn message_stream_permission_and_cancel_lifecycle_is_persistent() {
    let fixture = Fixture::new().await;
    fixture.select_provider().await;
    let (status, _, body) = fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/message"),
            Some(json!({"message": "hello", "context": {"traceId": "tr-1"}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let sent = json_body(&body);
    let session_id = sent["session_id"].as_str().unwrap();
    assert_eq!(
        sent["stream_url"],
        format!("{PREFIX}/sessions/{session_id}/stream")
    );
    let session_file = fixture
        .runtime
        .sessions()
        .root()
        .join(format!("{session_id}.json"));
    let stored = std::fs::read_to_string(&session_file).unwrap();
    assert_eq!(
        stored,
        format!(
            "{{\"context\": {{\"traceId\": \"tr-1\"}}, \"messages\": [{{\"role\": \"user\", \"content\": \"hello\"}}], \"pending_message\": {{\"role\": \"user\", \"content\": \"hello\"}}, \"provider_session_id\": null, \"working_dir\": null, \"pending_tool_decisions\": {{}}, \"pending_client_tool_results\": {{}}}}"
        )
    );

    let (status, headers, stream) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert_eq!(
        stream,
        concat!(
            "event: message\n",
            "data: {\"message\": {\"role\": \"assistant\", \"content\": \"hello\"}}\n\n",
            "event: stream_event\n",
            "data: {\"event\": {\"type\": \"content_delta\", \"delta\": {\"text\": \"!\"}}}\n\n",
            "event: permission_request\n",
            "data: {\"request_id\": \"tool-1\", \"tool_name\": \"Bash\", \"tool_input\": {\"command\": \"pwd\"}}\n\n",
            "event: interrupted\n",
            "data: {\"message\": \"Assistant was interrupted\"}\n\n",
            "event: error\n",
            "data: {\"error\": \"scripted error\"}\n\n",
            "event: done\n",
            "data: {\"result\": null, \"session_id\": \"provider-session-1\"}\n\n",
        )
    );
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].prompt, "hello");
        assert_eq!(requests[0].tracking_uri, "http://localhost:5000");
        assert_eq!(requests[0].context["traceId"], "tr-1");
    }
    let session = fixture
        .runtime
        .sessions()
        .load(session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        session.provider_session_id.as_deref(),
        Some("provider-session-1")
    );
    assert!(session.pending_message.is_none());

    let (status, _, body) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, r#"{"detail":"No pending message to process"}"#);

    let (status, _, body) = fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/sessions/{session_id}/permission"),
            Some(json!({"request_id": "tool-1", "decision": "allow"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body)["stream_url"],
        format!("{PREFIX}/sessions/{session_id}/stream")
    );
    let (status, _, _) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].prompt, "");
        assert_eq!(
            requests[1].context["tool_decisions"],
            json!({"tool-1": "allow"})
        );
    }

    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    fixture
        .runtime
        .sessions()
        .save_process_pid(session_id, child.id() as i32)
        .unwrap();
    let (status, _, body) = fixture
        .local(
            Method::PATCH,
            &format!("{PREFIX}/sessions/{session_id}"),
            Some(json!({"status": "cancelled"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"message":"Session cancelled and process terminated"}"#
    );
    child.wait().unwrap();
    assert!(!fixture
        .runtime
        .sessions()
        .root()
        .join(format!("{session_id}.process.json"))
        .exists());
}

#[tokio::test]
async fn client_tool_result_resumes_once_and_a_new_message_supersedes_stale_state() {
    let fixture = Fixture::new().await;
    fixture.select_provider().await;
    let project = fixture._directory.path().join("late-project");
    std::fs::create_dir_all(&project).unwrap();
    let (status, _, _) = fixture
        .local(
            Method::PUT,
            &format!("{PREFIX}/config"),
            Some(json!({"projects":{"7":{"location":project}}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/message"),
            Some(json!({
                "message":"build a view",
                "context":{"customTraceView":{"surfaceId":"main"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = json_body(&body)["session_id"].as_str().unwrap().to_string();
    let (status, _, _) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/sessions/{session_id}/tool-result"),
            Some(json!({"request_id":"view-1","content":"applied","is_error":false})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body)["stream_url"],
        format!("{PREFIX}/sessions/{session_id}/stream")
    );
    let (status, _, _) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(
            requests[1].context["client_tool_results"],
            json!({"view-1":{"content":"applied","is_error":false}})
        );
    }
    let session = fixture
        .runtime
        .sessions()
        .load(&session_id)
        .unwrap()
        .unwrap();
    assert!(session.pending_client_tool_results.is_empty());

    fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/sessions/{session_id}/tool-result"),
            Some(json!({"request_id":"stale","content":"old"})),
        )
        .await;
    fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/message"),
            Some(json!({
                "session_id":session_id,
                "message":"new turn",
                "experiment_id":"7"
            })),
        )
        .await;
    let (status, _, _) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests[2].prompt, "new turn");
    assert_eq!(requests[2].cwd.as_deref(), Some(project.as_path()));
    assert!(requests[2].context.get("client_tool_results").is_none());
    assert!(requests[2].context.get("customTraceView").is_none());
}

#[tokio::test]
async fn provider_session_id_is_persisted_when_a_stream_ends_in_error() {
    let fixture = Fixture::new().await;
    fixture.select_provider().await;
    let (status, _, body) = fixture
        .local(
            Method::POST,
            &format!("{PREFIX}/message"),
            Some(json!({"message":"error only"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = json_body(&body)["session_id"].as_str().unwrap().to_string();

    let (status, _, body) = fixture
        .local(
            Method::GET,
            &format!("{PREFIX}/sessions/{session_id}/stream"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "event: error\ndata: {\"error\": \"scripted error\", \"session_id\": \"provider-error-session\"}\n\n"
    );
    let session = fixture
        .runtime
        .sessions()
        .load(&session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        session.provider_session_id.as_deref(),
        Some("provider-error-session")
    );
    assert!(session.pending_message.is_none());
}

#[tokio::test]
async fn fastapi_validation_and_session_errors_are_exact() {
    let fixture = Fixture::new().await;
    let cases = [
        (
            Method::POST,
            format!("{PREFIX}/message"),
            Some(json!({})),
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"detail":[{"type":"missing","loc":["body","message"],"msg":"Field required","input":{}}]}),
        ),
        (
            Method::PATCH,
            format!("{PREFIX}/sessions/nope"),
            Some(json!({"status":"other"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"detail":[{"type":"literal_error","loc":["body","status"],"msg":"Input should be 'cancelled'","input":"other","ctx":{"expected":"'cancelled'"}}]}),
        ),
        (
            Method::POST,
            format!("{PREFIX}/sessions/nope/permission"),
            Some(json!({"request_id":"x","decision":"allow"})),
            StatusCode::BAD_REQUEST,
            json!({"detail":"Invalid session ID format"}),
        ),
        (
            Method::GET,
            format!("{PREFIX}/sessions/nope/stream"),
            None,
            StatusCode::NOT_FOUND,
            json!({"detail":"Session not found"}),
        ),
        (
            Method::POST,
            format!("{PREFIX}/skills/install"),
            Some(json!({"type":"custom"})),
            StatusCode::PRECONDITION_FAILED,
            json!({"detail":"No assistant provider is configured or available."}),
        ),
    ];
    for (method, path, request, expected_status, expected_body) in cases {
        let (status, _, body) = fixture.local(method, &path, request).await;
        assert_eq!(
            status,
            expected_status,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(json_body(&body), expected_body, "{path}");
    }
}
