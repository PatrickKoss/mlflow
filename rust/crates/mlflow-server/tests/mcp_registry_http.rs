use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use mlflow_server::{build_app_with_recorder, AppState, ServerConfig};
use mlflow_store::{Db, PoolConfig, TrackingStore};
use serde_json::{json, Value};
use tower::ServiceExt;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tracking.db")
}

struct Fixture {
    path: PathBuf,
    app: axum::Router,
}

impl Fixture {
    async fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mlflow_rust_mcp_{tag}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::copy(fixture_path(), &path).unwrap();
        let db = Db::connect(
            &format!("sqlite:///{}", path.display()),
            PoolConfig::default(),
        )
        .await
        .unwrap();
        let state = AppState::new(TrackingStore::new(db, "s3://bucket/mlruns"));
        let config = ServerConfig {
            disable_security_middleware: true,
            ..Default::default()
        };
        let recorder = PrometheusBuilder::new().build_recorder().handle();
        Self {
            path,
            app: build_app_with_recorder(&config, recorder, Some(state)),
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        let body = match body {
            Some(value) => {
                request = request.header("content-type", "application/json");
                Body::from(value.to_string())
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[tokio::test]
async fn full_crud_is_served_on_both_prefixes_and_preserves_json() {
    for (tag, prefix) in [
        ("api", "/api/3.0/mlflow/mcp-servers"),
        ("ajax", "/ajax-api/3.0/mlflow/mcp-servers"),
    ] {
        let fixture = Fixture::new(tag).await;
        let name = "com.example/server";
        let (status, created) = fixture
            .request(
                Method::POST,
                prefix,
                Some(json!({
                    "name": name,
                    "description": "server",
                    "icons": [{
                        "src": "https://example.com/icon.png",
                        "mimeType": " IMAGE/PNG ",
                        "future": null
                    }]
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["icons"][0]["mimeType"], "image/png");
        assert!(created["icons"][0].get("future").unwrap().is_null());
        let encoded_name = name.replace('/', "%2F");
        let (status, encoded) = fixture
            .request(Method::GET, &format!("{prefix}/{encoded_name}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{encoded}");

        let (status, version) = fixture
            .request(
                Method::POST,
                &format!("{prefix}/{name}/versions"),
                Some(json!({
                    "server_json": {
                        "name": name,
                        "version": "1.0.0",
                        "future": {"explicit_null": null, "kept": true}
                    },
                    "status": "active",
                    "tools": []
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{version}");
        assert_eq!(
            version["server_json"]["future"]["explicit_null"],
            json!(null)
        );
        assert_eq!(version["tools"], json!([]));

        let (status, _) = fixture
            .request(
                Method::POST,
                &format!("{prefix}/{name}/aliases"),
                Some(json!({"alias": "prod/us", "version": "1.0.0"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, slash_alias) = fixture
            .request(
                Method::GET,
                &format!("{prefix}/{encoded_name}/aliases/prod%2Fus"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{slash_alias}");
        assert_eq!(slash_alias["version"], "1.0.0");

        let (status, _) = fixture
            .request(
                Method::POST,
                &format!("{prefix}/{name}/aliases"),
                Some(json!({"alias": "prod", "version": "1.0.0"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, endpoint) = fixture
            .request(
                Method::POST,
                &format!("{prefix}/{name}/endpoints"),
                Some(json!({"url": "https://mcp.example.com", "server_alias": "prod"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{endpoint}");
        assert_eq!(endpoint["resolved_version"]["version"], "1.0.0");
        let endpoint_id = endpoint["id"].as_str().unwrap();

        let (status, searched) = fixture.request(Method::GET, prefix, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            searched["mcp_servers"][0]["access_endpoints"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let (status, _) = fixture
            .request(
                Method::DELETE,
                &format!("{prefix}/{name}/endpoints/{endpoint_id}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn validation_and_pagination_return_mlflow_errors() {
    let fixture = Fixture::new("errors").await;
    let prefix = "/api/3.0/mlflow/mcp-servers";
    let (status, error) = fixture
        .request(Method::POST, prefix, Some(json!({"name": "bad"})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error_code"], "INVALID_PARAMETER_VALUE");

    let (status, error) = fixture
        .request(
            Method::POST,
            prefix,
            Some(json!({
                "name": "com.example/private-icon",
                "icons": [{"src": "https://127.0.0.1/icon.png"}]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("non-public IP address"));

    let name = "com.example/semver";
    let (status, error) = fixture
        .request(
            Method::POST,
            &format!("{prefix}/{name}/versions"),
            Some(json!({"server_json": {"name": name, "version": "01.0.0"}})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("semantic version"));

    for index in 0..3 {
        let _ = fixture
            .request(
                Method::POST,
                prefix,
                Some(json!({"name": format!("com.example/page-{index}")})),
            )
            .await;
    }
    let (status, page) = fixture
        .request(Method::GET, &format!("{prefix}?max_results=2"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["mcp_servers"].as_array().unwrap().len(), 2);
    assert!(page["next_page_token"].is_string());
}

#[tokio::test]
async fn version_envelopes_connect_options_and_response_only_fields_match_python() {
    let fixture = Fixture::new("version_net_state").await;
    let prefix = "/api/3.0/mlflow/mcp-servers";
    let name = "com.example/enveloped";
    let (status, version) = fixture
        .request(
            Method::POST,
            &format!("{prefix}/{name}/versions"),
            Some(json!({
                "server_json": {
                    "server": {
                        "name": name,
                        "version": "1.0.0",
                        "icons": [{
                            "src": "https://example.com/server.svg",
                            "source": "server"
                        }]
                    },
                    "ignored_envelope_metadata": true
                },
                "display_name": "ignored version field",
                "tools": [{
                    "name": "echo",
                    "icons": [{
                        "src": "https://example.com/tool.svg",
                        "source": "server"
                    }]
                }],
                "connect_options": {"packages": {"hidden": true}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{version}");
    assert!(version.get("display_name").is_none());
    assert_eq!(version["connect_options"]["packages"]["hidden"], true);
    assert!(version["server_json"]["icons"][0].get("source").is_none());
    assert!(version["tools"][0]["icons"][0].get("source").is_none());

    let (status, unchanged) = fixture
        .request(
            Method::PATCH,
            &format!("{prefix}/{name}/versions/1.0.0"),
            Some(json!({"display_name": "still ignored"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{unchanged}");
    assert_eq!(unchanged["connect_options"], version["connect_options"]);
    assert!(unchanged.get("display_name").is_none());

    let (status, cleared) = fixture
        .request(
            Method::PATCH,
            &format!("{prefix}/{name}/versions/1.0.0"),
            Some(json!({"connect_options": null})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["connect_options"], json!({}));

    let (status, replaced) = fixture
        .request(
            Method::PATCH,
            &format!("{prefix}/{name}/versions/1.0.0"),
            Some(json!({"connect_options": {"remotes": {"hidden": false}}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(
        replaced["connect_options"],
        json!({"remotes": {"hidden": false}})
    );

    let (status, error) = fixture
        .request(
            Method::POST,
            &format!("{prefix}/com.example/bad-envelope/versions"),
            Some(json!({"server_json": {"server": "not-an-object"}})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("server_json.server: Input should be a valid dictionary"));
}

#[tokio::test]
async fn server_icon_resolution_and_source_write_rejection_match_python() {
    let fixture = Fixture::new("icon_net_state").await;
    let prefix = "/api/3.0/mlflow/mcp-servers";
    let name = "com.example/icons";
    let (status, created) = fixture
        .request(
            Method::POST,
            prefix,
            Some(json!({
                "name": name,
                "icons": [
                    {"src": "https://example.com/server-dark.svg", "theme": "dark"},
                    {"src": "https://example.com/server-default.svg"}
                ]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        created["icons"],
        json!([
            {
                "src": "https://example.com/server-dark.svg",
                "theme": "dark",
                "source": "server"
            },
            {"src": "https://example.com/server-default.svg", "source": "server"}
        ])
    );
    let (status, _) = fixture
        .request(
            Method::POST,
            &format!("{prefix}/{name}/versions"),
            Some(json!({
                "server_json": {
                    "name": name,
                    "version": "1.0.0",
                    "icons": [
                        {"src": "https://example.com/version-dark.svg", "theme": "dark"},
                        {"src": "https://example.com/version-light.svg", "theme": "light"},
                        {"src": "https://example.com/version-default.svg"}
                    ]
                },
                "status": "active"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, server) = fixture
        .request(Method::GET, &format!("{prefix}/{name}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{server}");
    assert_eq!(
        server["icons"],
        json!([
            {
                "src": "https://example.com/server-dark.svg",
                "theme": "dark",
                "source": "server"
            },
            {"src": "https://example.com/server-default.svg", "source": "server"},
            {
                "src": "https://example.com/version-light.svg",
                "theme": "light",
                "source": "version"
            }
        ])
    );

    let (status, updated) = fixture
        .request(
            Method::PATCH,
            &format!("{prefix}/{name}"),
            Some(json!({"description": "updated without changing icons"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["icons"], server["icons"]);

    let (status, searched) = fixture
        .request(
            Method::GET,
            &format!("{prefix}?filter_string=name%20%3D%20%27com.example%2Ficons%27"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{searched}");
    assert_eq!(searched["mcp_servers"][0]["icons"], server["icons"]);

    let empty_name = "com.example/empty-icons";
    fixture
        .request(
            Method::POST,
            prefix,
            Some(json!({"name": empty_name, "icons": []})),
        )
        .await;
    fixture
        .request(
            Method::POST,
            &format!("{prefix}/{empty_name}/versions"),
            Some(json!({
                "server_json": {
                    "name": empty_name,
                    "version": "1.0.0",
                    "icons": [{"src": "https://example.com/version.svg"}]
                },
                "status": "active"
            })),
        )
        .await;
    let (_, empty) = fixture
        .request(Method::GET, &format!("{prefix}/{empty_name}"), None)
        .await;
    assert_eq!(empty["icons"], json!([]));

    let expected = "Invalid icons[0].source: 'version' is response-only and cannot be written. \
                    Writing it would persist a version-resolved icon as a server override. Omit \
                    source, or send source='server' when echoing a server icon.";
    let (status, error) = fixture
        .request(
            Method::POST,
            prefix,
            Some(json!({
                "name": "com.example/rejected-icon",
                "icons": [{"src": "https://example.com/icon.svg", "source": "version"}]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["message"], expected);

    let (status, error) = fixture
        .request(
            Method::PATCH,
            &format!("{prefix}/{name}"),
            Some(json!({
                "icons": [{
                    "src": "https://example.com/update.svg",
                    "source": "version"
                }]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["message"], expected);

    let expected = "Invalid tools[0].icons[0].source: 'version' is response-only and cannot be \
                    written. Writing it would persist a version-resolved icon as a server \
                    override. Omit source, or send source='server' when echoing a server icon.";
    let (status, error) = fixture
        .request(
            Method::PATCH,
            &format!("{prefix}/{name}/versions/1.0.0"),
            Some(json!({
                "tools": [{
                    "name": "rejected-update",
                    "icons": [{
                        "src": "https://example.com/update-tool.svg",
                        "source": "version"
                    }]
                }]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["message"], expected);
}

#[tokio::test]
async fn create_tools_preserve_null_empty_tristate_through_endpoints() {
    let fixture = Fixture::new("tool_tristate").await;
    let prefix = "/api/3.0/mlflow/mcp-servers";
    for (suffix, tools, expected) in [
        ("omitted", None, Value::Null),
        ("null", Some(Value::Null), Value::Null),
        ("empty", Some(json!([])), json!([])),
    ] {
        let name = format!("com.example/tools-{suffix}");
        let mut body = json!({
            "server_json": {"name": name, "version": "1.0.0"},
            "status": "active"
        });
        if let Some(tools) = tools {
            body["tools"] = tools;
        }
        let (status, _) = fixture
            .request(
                Method::POST,
                &format!("{prefix}/{name}/versions"),
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, endpoint) = fixture
            .request(
                Method::POST,
                &format!("{prefix}/{name}/endpoints"),
                Some(json!({
                    "url": format!("https://mcp.example.com/{suffix}"),
                    "server_version": "1.0.0"
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{endpoint}");
        assert_eq!(endpoint["tools"], expected);
    }
}
