//! Artifact HTTP surface (plan T5.1-T5.3, §3.11).
//!
//! This module wires the artifact-plane handlers to [`AppState`], reusing the
//! store-agnostic streaming core in the `mlflow-artifacts` crate
//! ([`mlflow_artifacts::send_artifact_response`], [`ArtifactRepo`],
//! [`mlflow_artifacts::validate_path_is_safe`]). It ports:
//!
//! * **T5.1** `GET /get-artifact?run_id=&path=` — `get_artifact_handler`
//!   (`handlers.py:1519`): resolve the run's `artifact_uri`, stream the file.
//! * **T5.2** the `MlflowArtifactsService` proxy (download/upload/list/delete +
//!   multipart create/complete/abort + presigned) under
//!   `/(api|ajax-api)/2.0/mlflow-artifacts/...` (`handlers.py:3536-3878`), gated
//!   by `--serve-artifacts` (`_disable_unless_serve_artifacts`). Streams both
//!   directions — no whole-body buffering (the Python WSGI-bridge defect the
//!   plan calls out).
//! * **T5.3** ajax `POST /ajax-api/2.0/mlflow/upload-artifact`
//!   (`upload_artifact_handler`, `handlers.py:2408`), `listLoggedModelArtifacts`
//!   (`_list_logged_model_artifacts`, `handlers.py:5403`; proto-route-table), and
//!   the ajax-only logged-model artifact file download
//!   (`get_logged_model_artifact_handler`, `handlers.py:5214`).
//!
//! Multipart + presigned-URL endpoints go through the repo trait, whose local-FS
//! backend returns `NOT_IMPLEMENTED` (parity with `LocalArtifactRepository`,
//! which lacks the multipart/presigned mixins).
//!
//! * **T5.4** `GET /model-versions/get-artifact?name=&version=&path=`
//!   (`get_model_version_artifact_handler`, `handlers.py:3033`): resolve
//!   `storage_location or source` via the [`mlflow_registry::RegistryStore`],
//!   then stream the file through the same proxied/direct resolution seam as
//!   T5.1. `models:/name/version`-sourced versions already carry a resolved
//!   `storage_location` (the registry store's `create_model_version`
//!   resolves that at write time, per `mlflow-registry`'s docs), so no extra
//!   indirection is needed here.

use std::collections::HashMap;

use axum::body::{Body, Bytes};
use axum::extract::{MatchedPath, Path, State};
use axum::http::header::{self, HeaderValue};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::stream::StreamExt;
use mlflow_error::{ErrorCode, MlflowError};
use mlflow_proto::mlflow as pb;
use mlflow_proto::mlflow::artifacts as art_pb;

use crate::proto_http::{
    parse_query_pairs, parse_request_lenient, parse_request_with_path_params, proto_response,
};
use crate::schema_validation::{SchemaEntry, Validator};
use crate::state::{proxied_run_artifact_destination_path, AppState};
use crate::workspace::Workspace;

/// Cap for the ajax `upload-artifact` body (`10 * 1024 * 1024`,
/// `handlers.py:2424`).
pub(crate) const MAX_UPLOAD_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;

/// Cap for the proxy multipart control-message bodies (create/complete/abort).
/// Artifact bytes never pass through here — they stream via `proxy_upload`.
const MAX_CONTROL_BODY_BYTES: usize = 16 * 1024 * 1024;

const PRESIGNED_DOWNLOAD_SCHEMA: &[SchemaEntry] = &[
    SchemaEntry {
        param: "run_id",
        validators: &[Validator::Required, Validator::String],
    },
    SchemaEntry {
        param: "path",
        validators: &[Validator::Required, Validator::String],
    },
    SchemaEntry {
        param: "expiration",
        validators: &[Validator::IntLike],
    },
];

// ===========================================================================
// T5.1 — GET /get-artifact
// ===========================================================================

/// `get_artifact_handler` (`handlers.py:1519`), served at the root
/// `/get-artifact` (`mlflow/server/__init__.py:111`). Plain `run_id`/`run_uuid`
/// + `path` query params (not a proto message).
pub async fn get_artifact(
    State(state): State<AppState>,
    workspace: Workspace,
    parts: Parts,
) -> Response {
    let result = async {
        let pairs = parts.uri.query().map(parse_query_pairs).unwrap_or_default();
        // `run_id or run_uuid` — `request.args["path"]` raises KeyError (→ 400)
        // when missing, which `catch_mlflow_exception` does NOT wrap; Flask
        // returns its own 400 BadRequest page. We surface a BAD_REQUEST MlflowError
        // for the missing-`path` case (observably a 400 with a JSON body — a
        // documented, benign deviation from Flask's HTML 400).
        let run_id = query_param(&pairs, "run_id").or_else(|| query_param(&pairs, "run_uuid"));
        let Some(path) = query_param(&pairs, "path") else {
            return Err(MlflowError::new(
                "Request must specify a 'path' query parameter.",
                ErrorCode::BadRequest,
            ));
        };
        let safe_path = mlflow_artifacts::validate_path_is_safe(&path)?;
        let Some(run_id) = run_id.filter(|s| !s.is_empty()) else {
            return Err(MlflowError::new(
                "Request must specify a 'run_id' query parameter.",
                ErrorCode::BadRequest,
            ));
        };

        let run = state
            .tracking_store()
            .get_run(workspace.name(), &run_id)
            .await?;
        let artifact_uri = run.info.artifact_uri.unwrap_or_default();
        let resolved = state.resolve_artifact(&artifact_uri, &safe_path)?;
        Ok::<_, MlflowError>((resolved.repo, resolved.path))
    }
    .await;

    match result {
        Ok((repo, path)) => mlflow_artifacts::send_artifact_response(repo.as_ref(), &path).await,
        Err(e) => e.into_response(),
    }
}

/// `GET /mlflow/artifacts/list?run_id=&run_uuid=&path=` (`list_artifacts_impl`).
/// Resolve the owning run's artifact root and return run-relative `FileInfo`
/// paths. `run_uuid` remains accepted for older UI clients.
pub async fn list_run_artifacts(
    State(state): State<AppState>,
    workspace: Workspace,
    parts: Parts,
) -> Result<Response, MlflowError> {
    let pairs = parts.uri.query().map(parse_query_pairs).unwrap_or_default();
    let run_id = query_param(&pairs, "run_id")
        .filter(|value| !value.is_empty())
        .or_else(|| query_param(&pairs, "run_uuid").filter(|value| !value.is_empty()))
        .ok_or_else(|| {
            MlflowError::invalid_parameter_value("Missing value for required parameter 'run_id'.")
        })?;
    let validated = match query_param(&pairs, "path").filter(|value| !value.is_empty()) {
        Some(path) => Some(mlflow_artifacts::validate_path_is_safe(&path)?),
        None => None,
    };

    let run = state
        .tracking_store()
        .get_run(workspace.name(), &run_id)
        .await?;
    let artifact_uri = run.info.artifact_uri.unwrap_or_default();
    let files = list_artifacts_at(&state, &artifact_uri, validated.as_deref()).await?;
    proto_response(
        &pb::list_artifacts::Response {
            root_uri: Some(artifact_uri),
            files,
            next_page_token: None,
        },
        "mlflow.ListArtifacts.Response",
    )
}

// ===========================================================================
// T5.4 — GET /model-versions/get-artifact
// ===========================================================================

/// `get_model_version_artifact_handler` (`handlers.py:3033`), served at the
/// root `/model-versions/get-artifact` (`mlflow/server/__init__.py:117`) —
/// no ajax alias, matching Python. Plain `name`/`version`/`path` query
/// params (not a proto message).
pub async fn get_model_version_artifact(
    State(state): State<AppState>,
    workspace: Workspace,
    parts: Parts,
) -> Response {
    let result = async {
        let pairs = parts.uri.query().map(parse_query_pairs).unwrap_or_default();
        // `request.args["path"]` raises KeyError (→ 400) when missing, which
        // `catch_mlflow_exception` does NOT wrap; same documented, benign
        // deviation as `get_artifact`'s missing-`path` case (see its doc
        // comment above).
        let Some(path) = query_param(&pairs, "path") else {
            return Err(MlflowError::new(
                "Request must specify a 'path' query parameter.",
                ErrorCode::BadRequest,
            ));
        };
        let safe_path = mlflow_artifacts::validate_path_is_safe(&path)?;
        // `request.args.get("name")` — `None` when absent. The registry
        // store's `_validate_model_name` (invoked inside
        // `get_model_version_download_uri`) raises the exact same "Missing
        // value for required parameter" `INVALID_PARAMETER_VALUE` error for
        // `None` as for `""`, so passing through the empty string reproduces
        // Python's error byte-for-byte without a separate required-param
        // guard here.
        let name = query_param(&pairs, "name").unwrap_or_default();
        // `request.args.get("version")` — `None` when absent. UNLIKE `name`,
        // `_validate_model_version` (`mlflow/utils/validation.py:684`) has no
        // explicit `is None` check: it calls `int(model_version)` inside a
        // `try/except ValueError`, and `int(None)` raises `TypeError`, NOT
        // `ValueError` — so the exception is NOT caught there. It propagates
        // out of the SQLAlchemy store's `ManagedSessionMaker` context
        // manager, whose blanket `except Exception as e: raise
        // MlflowException(message=e, error_code=INTERNAL_ERROR) from e`
        // (`mlflow/store/db/utils.py:188`) wraps it into a 500
        // `INTERNAL_ERROR`, distinct from the 400 `INVALID_PARAMETER_VALUE`
        // a present-but-non-numeric `version` (e.g. `"abc"`, or `""` for
        // `version=` with an empty value) gets from the caught `ValueError`
        // path. Verified against the real Python handler. We special-case
        // the missing-query-param case to reproduce this exact asymmetry;
        // `validate_model_version` inside the registry store handles the
        // present-but-invalid cases identically to Python.
        let Some(version) = query_param(&pairs, "version") else {
            return Err(MlflowError::internal_error(
                "int() argument must be a string, a bytes-like object or a real number, not \
                 'NoneType'",
            ));
        };

        let registry = state.registry_store()?;
        let artifact_uri = registry
            .get_model_version_download_uri(workspace.name(), &name, &version)
            .await?;
        let resolved = state.resolve_artifact(&artifact_uri, &safe_path)?;
        Ok::<_, MlflowError>((resolved.repo, resolved.path))
    }
    .await;

    match result {
        Ok((repo, path)) => mlflow_artifacts::send_artifact_response(repo.as_ref(), &path).await,
        Err(e) => e.into_response(),
    }
}

// ===========================================================================
// T5.3 — ajax POST /ajax-api/2.0/mlflow/upload-artifact
// ===========================================================================

/// `upload_artifact_handler` (`handlers.py:2408`): `run_uuid` + `path` query
/// params, raw request body (max 10 MB), written under the run's artifacts.
pub async fn upload_artifact(
    State(state): State<AppState>,
    workspace: Workspace,
    parts: Parts,
    body: Bytes,
) -> Response {
    let result = async {
        let pairs = parts.uri.query().map(parse_query_pairs).unwrap_or_default();
        let run_uuid = query_param(&pairs, "run_uuid").filter(|s| !s.is_empty());
        let Some(run_uuid) = run_uuid else {
            return Err(MlflowError::invalid_parameter_value(
                "Request must specify run_uuid.",
            ));
        };
        let path = query_param(&pairs, "path").filter(|s| !s.is_empty());
        let Some(path) = path else {
            return Err(MlflowError::invalid_parameter_value(
                "Request must specify path.",
            ));
        };
        let safe_path = mlflow_artifacts::validate_path_is_safe(&path)?;

        if body.len() > MAX_UPLOAD_ARTIFACT_BYTES {
            return Err(MlflowError::invalid_parameter_value(
                "Artifact size is too large. Max size is 10MB.",
            ));
        }
        if body.is_empty() {
            return Err(MlflowError::invalid_parameter_value(
                "Request must specify data.",
            ));
        }

        let run = state
            .tracking_store()
            .get_run(workspace.name(), &run_uuid)
            .await?;
        let artifact_uri = run.info.artifact_uri.clone().unwrap_or_default();

        // Python writes the file at `<run artifact root>/<path>` (the run's
        // artifact repo joins `dirname` and logs `basename`); resolving the
        // artifact URI against the full `path` yields the same destination for
        // both the direct and proxied cases.
        let resolved = state.resolve_artifact(&artifact_uri, &safe_path)?;
        let stream = futures::stream::once(async move { Ok(body) }).boxed();
        resolved.repo.put(&resolved.path, stream).await?;
        Ok::<_, MlflowError>(())
    }
    .await;

    match result {
        // Python returns `Response(mimetype="application/json")` with an empty
        // body (no proto message) — a 200 with an empty JSON-typed body.
        Ok(()) => Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::empty())
            .expect("valid response"),
        Err(e) => e.into_response(),
    }
}

// ===========================================================================
// T5.3 — ajax logged-model artifact file download + listLoggedModelArtifacts
// ===========================================================================

/// `get_logged_model_artifact_handler` (`handlers.py:5214`), served at
/// `/ajax-api/2.0/mlflow/logged-models/{model_id}/artifacts/files`
/// (`mlflow/server/__init__.py:166`). `artifact_file_path` query param.
pub async fn get_logged_model_artifact(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(path_params): Path<HashMap<String, String>>,
    parts: Parts,
) -> Response {
    let result = async {
        let model_id = path_param(&path_params, "model_id")?;
        let pairs = parts.uri.query().map(parse_query_pairs).unwrap_or_default();
        let artifact_file_path =
            query_param(&pairs, "artifact_file_path").filter(|s| !s.is_empty());
        let Some(artifact_file_path) = artifact_file_path else {
            return Err(MlflowError::new(
                "Request must include the \"artifact_file_path\" query parameter.",
                ErrorCode::BadRequest,
            ));
        };
        let safe_path = mlflow_artifacts::validate_path_is_safe(&artifact_file_path)?;

        let model = state
            .tracking_store()
            .get_logged_model(workspace.name(), &model_id, false)
            .await?;
        let resolved = state.resolve_artifact(&model.artifact_location, &safe_path)?;
        Ok::<_, MlflowError>((resolved.repo, resolved.path))
    }
    .await;

    match result {
        Ok((repo, path)) => mlflow_artifacts::send_artifact_response(repo.as_ref(), &path).await,
        Err(e) => e.into_response(),
    }
}

/// `_list_logged_model_artifacts` (`handlers.py:5403`) — proto-route-table GET
/// `/mlflow/logged-models/{model_id}/artifacts/directories`.
pub async fn list_logged_model_artifacts(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(path_params): Path<HashMap<String, String>>,
    parts: Parts,
    body: Bytes,
) -> Result<Response, MlflowError> {
    let model_id = path_param(&path_params, "model_id")?;
    let req: pb::ListLoggedModelArtifacts = parse_request_with_path_params(
        &parts,
        &body,
        "mlflow.ListLoggedModelArtifacts",
        &[("model_id", model_id.clone())],
    )?;
    let dir_path = req
        .artifact_directory_path
        .as_deref()
        .filter(|_| req.artifact_directory_path.is_some());
    let validated = match dir_path {
        Some(p) => Some(mlflow_artifacts::validate_path_is_safe(p)?),
        None => None,
    };

    let model = state
        .tracking_store()
        .get_logged_model(workspace.name(), &model_id, false)
        .await?;

    let files = list_artifacts_at(&state, &model.artifact_location, validated.as_deref()).await?;
    let resp = pb::list_logged_model_artifacts::Response {
        root_uri: Some(model.artifact_location),
        files,
        next_page_token: None,
    };
    proto_response(&resp, "mlflow.ListLoggedModelArtifacts.Response")
}

/// List artifacts under an artifact root at `relative_path`, mirroring the
/// direct vs proxied branch in `_list_artifacts_for_proxied_run_artifact_root` /
/// `list_artifacts`. Returns proto `FileInfo`s.
///
/// * Direct (non-proxied): `get_artifact_repository(root).list_artifacts(path)`
///   — full run-relative paths.
/// * Proxied + servable: list from the `--artifacts-destination` repo at the
///   resolved destination path, then rewrite each entry's path to its basename
///   re-joined under `relative_path` (Python's
///   `posixpath.join(relative_path, basename)`).
pub(crate) async fn list_artifacts_at(
    state: &AppState,
    artifact_root: &str,
    relative_path: Option<&str>,
) -> Result<Vec<pb::FileInfo>, MlflowError> {
    if state.is_servable_proxied_run_artifact_root(artifact_root) {
        let repo = state.proxied_artifacts_repo()?;
        let dest = proxied_run_artifact_destination_path(artifact_root, relative_path)?;
        let entries = repo.list(Some(&dest)).await?;
        Ok(entries
            .into_iter()
            .map(|f| {
                let base = basename(&f.path);
                let run_relative = match relative_path {
                    Some(rel) if !rel.is_empty() => format!("{}/{base}", rel.trim_end_matches('/')),
                    _ => base.to_string(),
                };
                pb::FileInfo {
                    path: Some(run_relative),
                    is_dir: Some(f.is_dir),
                    file_size: f.file_size,
                }
            })
            .collect())
    } else {
        let repo = mlflow_artifacts::factory::repo_from_uri(artifact_root)?;
        let entries = repo.list(relative_path).await?;
        Ok(entries
            .into_iter()
            .map(|f| pb::FileInfo {
                path: Some(f.path),
                is_dir: Some(f.is_dir),
                file_size: f.file_size,
            })
            .collect())
    }
}

/// `POST /mlflow/artifacts/presigned-download-url`.
pub async fn create_presigned_download_url(
    State(state): State<AppState>,
    workspace: Workspace,
    parts: Parts,
    body: Bytes,
) -> Result<Response, MlflowError> {
    let req: pb::CreatePresignedDownloadUrl = parse_request_lenient(
        &parts,
        &body,
        "mlflow.CreatePresignedDownloadUrl",
        PRESIGNED_DOWNLOAD_SCHEMA,
    )?;
    let run_id = req.run_id.unwrap_or_default();
    let path = mlflow_artifacts::validate_path_is_safe(&req.path.unwrap_or_default())?;
    let expiration = match req.expiration {
        Some(value) => value,
        None => mlflow_artifacts::presigned_download_ttl_seconds()?,
    };
    if !(1..=604_800).contains(&expiration) {
        return Err(MlflowError::invalid_parameter_value(format!(
            "expiration must be between 1 and 604800 seconds (got {expiration})."
        )));
    }

    let run = state
        .tracking_store()
        .get_run(workspace.name(), &run_id)
        .await?;
    let artifact_uri = run.info.artifact_uri.unwrap_or_default();
    let scheme = artifact_uri_scheme(&artifact_uri);
    if matches!(
        scheme.as_deref(),
        Some("http" | "https" | "mlflow-artifacts")
    ) {
        return Err(MlflowError::invalid_parameter_value(format!(
            "Presigned download is not supported for runs with proxied artifact storage \
             (artifact URI scheme: {}). This endpoint requires a run with a direct cloud \
             storage artifact URI.",
            scheme.unwrap()
        )));
    }

    let repo = mlflow_artifacts::factory::repo_from_uri(&artifact_uri)?;
    if !repo.supports_multipart_download() {
        return Err(MlflowError::not_implemented(
            "Presigned download is not supported for the current artifact repository",
        ));
    }
    let presigned = repo
        .get_download_presigned_url(&path, expiration as u64)
        .await?;
    proto_response(
        &pb::create_presigned_download_url::Response {
            presigned_url: Some(presigned.url),
            headers: presigned.headers.into_iter().collect(),
            file_size: presigned.file_size,
        },
        "mlflow.CreatePresignedDownloadUrl.Response",
    )
}

/// `_disable_if_artifacts_only`: plain-text 503 with Flask's matched URL rule,
/// including `<param>` placeholders rather than the concrete request path.
pub async fn artifacts_only_disabled(parts: Parts) -> Response {
    let matched = parts
        .extensions
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or_else(|| parts.uri.path());
    let rule = axum_rule_to_flask(matched);
    let body = format!(
        "Endpoint: {rule} disabled due to the mlflow server running in `--artifacts-only` mode. \
         To enable tracking server functionality, run `mlflow server` without \
         `--artifacts-only`"
    );
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .expect("valid response")
}

fn axum_rule_to_flask(rule: &str) -> String {
    let mut output = String::with_capacity(rule.len());
    let mut rest = rule;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let parameter = &rest[start + 1..start + 1 + end];
        if let Some(wildcard) = parameter.strip_prefix('*') {
            output.push_str("<path:");
            output.push_str(wildcard);
            output.push('>');
        } else {
            output.push('<');
            output.push_str(parameter);
            output.push('>');
        }
        rest = &rest[start + end + 2..];
    }
    output.push_str(rest);
    output
}

/// `_get_workspace_scoped_repo_path_if_enabled`: isolate proxy-repository
/// paths while preserving the legacy root layout for the default workspace.
fn workspace_scoped_repo_path(
    state: &AppState,
    workspace: &Workspace,
    artifact_path: Option<&str>,
) -> Result<Option<String>, MlflowError> {
    if state.workspace_store().is_none() {
        return Ok(artifact_path.map(str::to_string));
    }

    let normalized = artifact_path.unwrap_or_default().trim_start_matches('/');
    let workspace_name = workspace.name();
    let base = format!("workspaces/{workspace_name}");
    if normalized.is_empty() {
        return Ok(
            if workspace_name == crate::workspace::DEFAULT_WORKSPACE_NAME {
                artifact_path.map(str::to_string)
            } else {
                Some(base)
            },
        );
    }

    if workspace_name == crate::workspace::DEFAULT_WORKSPACE_NAME
        && !normalized.starts_with("workspaces/")
    {
        return Ok(artifact_path.map(str::to_string));
    }

    if normalized == "workspaces" || normalized.starts_with("workspaces/") {
        let prefixed = normalized.strip_prefix("workspaces").unwrap();
        let prefixed = prefixed.strip_prefix('/').unwrap_or_default();
        let Some((requested_workspace, _)) = prefixed.split_once('/') else {
            if prefixed.is_empty() {
                return Err(MlflowError::invalid_parameter_value(
                    "Artifact paths prefixed with 'workspaces/' must include a workspace name.",
                ));
            }
            if prefixed != workspace_name {
                return Err(MlflowError::invalid_parameter_value(format!(
                    "Artifact path targets workspace '{prefixed}' but the workspace specified in \
                     the request is '{workspace_name}'."
                )));
            }
            return Ok(Some(normalized.to_string()));
        };
        if requested_workspace.is_empty() {
            return Err(MlflowError::invalid_parameter_value(
                "Artifact paths prefixed with 'workspaces/' must include a workspace name.",
            ));
        }
        if requested_workspace != workspace_name {
            return Err(MlflowError::invalid_parameter_value(format!(
                "Artifact path targets workspace '{requested_workspace}' but the workspace \
                 specified in the request is '{workspace_name}'."
            )));
        }
        return Ok(Some(normalized.to_string()));
    }

    Ok(Some(format!("{base}/{normalized}")))
}

// ===========================================================================
// T5.2 — MlflowArtifactsService proxy (gated by --serve-artifacts)
// ===========================================================================

/// `_disable_unless_serve_artifacts` (`handlers.py:1186`): when `--serve-artifacts`
/// is off, return the exact 503 body Python sends, naming the matched route.
fn disabled_response(parts: &Parts) -> Response {
    let rule = parts
        .extensions
        .get::<MatchedPath>()
        .map(|m| m.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let body = format!(
        "Endpoint: {rule} disabled due to the mlflow server running with \
         `--no-serve-artifacts`. To enable artifacts server functionality, run \
         `mlflow server` with `--serve-artifacts`"
    );
    (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
}

/// GET `/mlflow-artifacts/artifacts/{*artifact_path}` — `_download_artifact`
/// (`handlers.py:3538`). Streams the file from the proxy repo.
pub async fn proxy_download(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    parts: Parts,
) -> Response {
    if !state.serve_artifacts() {
        return fastapi_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "Artifact serving is disabled. Run `mlflow server` with `--serve-artifacts` to enable.",
        );
    }
    if state.artifacts_destination().is_none() {
        return fastapi_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "Artifact serving is not configured.",
        );
    }
    let repo = match state.proxied_artifacts_repo() {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let safe_path = match mlflow_artifacts::validate_path_is_safe(&artifact_path).and_then(|path| {
        workspace_scoped_repo_path(&state, &workspace, Some(&path))
            .map(|path| path.expect("non-empty artifact path remains present"))
    }) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    if let Some(local_path) = repo.local_path(&safe_path) {
        return local_file_response(
            &local_path,
            &safe_path,
            parts
                .headers
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok()),
        )
        .await;
    }
    mlflow_artifacts::send_artifact_response(repo.as_ref(), &safe_path).await
}

/// PUT `/mlflow-artifacts/artifacts/{*artifact_path}` — `_upload_artifact`
/// (`handlers.py:3573`). Streams the request body into the proxy repo (no
/// buffering).
pub async fn proxy_upload(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    _parts: Parts,
    body: Body,
) -> Response {
    if !state.serve_artifacts() {
        return fastapi_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "Artifact serving is disabled. Run `mlflow server` with `--serve-artifacts` to enable.",
        );
    }
    if state.artifacts_destination().is_none() {
        return fastapi_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "Artifact serving is not configured.",
        );
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let safe = mlflow_artifacts::validate_path_is_safe(&artifact_path)?;
        let safe = workspace_scoped_repo_path(&state, &workspace, Some(&safe))?
            .expect("non-empty artifact path remains present");
        if safe.ends_with('/') {
            return Ok::<_, MlflowError>(Some(fastapi_detail(
                StatusCode::BAD_REQUEST,
                "Artifact path must include a filename (cannot end with '/').",
            )));
        }
        let stream = body
            .into_data_stream()
            .map(|chunk| {
                chunk.map_err(|e| MlflowError::internal_error(format!("Upload read error: {e}")))
            })
            .boxed();
        repo.put(&safe, stream).await?;
        Ok::<_, MlflowError>(None)
    }
    .await;
    match result {
        Ok(Some(response)) => response,
        Ok(None) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .expect("valid response"),
        Err(e) => e.into_response(),
    }
}

/// DELETE `/mlflow-artifacts/artifacts/{*artifact_path}` —
/// `_delete_artifact_mlflow_artifacts` (`handlers.py:3621`).
pub async fn proxy_delete(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    parts: Parts,
) -> Response {
    if !state.serve_artifacts() {
        return disabled_response(&parts);
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let safe = mlflow_artifacts::validate_path_is_safe(&artifact_path)?;
        let safe = workspace_scoped_repo_path(&state, &workspace, Some(&safe))?
            .expect("non-empty artifact path remains present");
        repo.delete(&safe).await?;
        Ok::<_, MlflowError>(())
    }
    .await;
    match result {
        Ok(()) => proto_json(
            &art_pb::delete_artifact::Response {},
            "mlflow.artifacts.DeleteArtifact.Response",
        ),
        Err(e) => e.into_response(),
    }
}

/// GET `/mlflow-artifacts/artifacts?path=` — `_list_artifacts_mlflow_artifacts`
/// (`handlers.py:3598`). Each returned `FileInfo` path is reduced to its
/// basename (`posixpath.basename`).
pub async fn proxy_list(
    State(state): State<AppState>,
    workspace: Workspace,
    parts: Parts,
) -> Response {
    if !state.serve_artifacts() {
        return disabled_response(&parts);
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let pairs = parts.uri.query().map(parse_query_pairs).unwrap_or_default();
        // `HasField("path")`: validate iff present.
        let validated = match query_param(&pairs, "path") {
            Some(p) => Some(mlflow_artifacts::validate_path_is_safe(&p)?),
            None => None,
        };
        let scoped = workspace_scoped_repo_path(&state, &workspace, validated.as_deref())?;
        let files = repo.list(scoped.as_deref()).await?;
        let proto_files = files
            .into_iter()
            .map(|f| pb::FileInfo {
                path: Some(basename(&f.path).to_string()),
                is_dir: Some(f.is_dir),
                file_size: f.file_size,
            })
            .collect();
        Ok::<_, MlflowError>(pb::list_artifacts::Response {
            files: proto_files,
            root_uri: None,
            next_page_token: None,
        })
    }
    .await;
    match result {
        Ok(resp) => proto_json(&resp, "mlflow.ListArtifacts.Response"),
        Err(e) => e.into_response(),
    }
}

/// POST `/mlflow-artifacts/mpu/create/{*artifact_path}` —
/// `_create_multipart_upload_artifact` (`handlers.py:3749`). Local FS is not a
/// `MultipartUploadMixin`, so this returns `NOT_IMPLEMENTED`.
pub async fn proxy_create_multipart(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Response {
    if !state.serve_artifacts() {
        return disabled_response(&parts);
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let artifact_path = mlflow_artifacts::validate_path_is_safe(&artifact_path)?;
        let artifact_path = workspace_scoped_repo_path(&state, &workspace, Some(&artifact_path))?
            .expect("non-empty artifact path remains present");
        let req: art_pb::CreateMultipartUpload =
            parse_control_body(&body, "mlflow.artifacts.CreateMultipartUpload")?;
        let path =
            mlflow_artifacts::multipart_upload_path(&req.path.unwrap_or_default(), &artifact_path);
        let num_parts = req.num_parts.unwrap_or_default();
        let res = repo.create_multipart_upload(&path, num_parts).await?;
        Ok::<_, MlflowError>(art_pb::create_multipart_upload::Response {
            upload_id: Some(res.upload_id),
            credentials: res
                .credentials
                .into_iter()
                .map(|c| art_pb::MultipartUploadCredential {
                    url: Some(c.url),
                    part_number: Some(c.part_number),
                    headers: c.headers.into_iter().collect(),
                })
                .collect(),
        })
    }
    .await;
    match result {
        Ok(resp) => proto_json(&resp, "mlflow.artifacts.CreateMultipartUpload.Response"),
        Err(e) => e.into_response(),
    }
}

/// POST `/mlflow-artifacts/mpu/complete/{*artifact_path}` —
/// `_complete_multipart_upload_artifact` (`handlers.py:3783`).
pub async fn proxy_complete_multipart(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Response {
    if !state.serve_artifacts() {
        return disabled_response(&parts);
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let artifact_path = mlflow_artifacts::validate_path_is_safe(&artifact_path)?;
        let artifact_path = workspace_scoped_repo_path(&state, &workspace, Some(&artifact_path))?
            .expect("non-empty artifact path remains present");
        let req: art_pb::CompleteMultipartUpload =
            parse_control_body(&body, "mlflow.artifacts.CompleteMultipartUpload")?;
        let path =
            mlflow_artifacts::multipart_upload_path(&req.path.unwrap_or_default(), &artifact_path);
        let upload_id = req.upload_id.unwrap_or_default();
        let parts: Vec<mlflow_artifacts::repo::MultipartUploadPart> = req
            .parts
            .into_iter()
            .map(|p| mlflow_artifacts::repo::MultipartUploadPart {
                part_number: p.part_number.unwrap_or_default(),
                etag: p.etag.unwrap_or_default(),
                url: p.url.unwrap_or_default(),
            })
            .collect();
        repo.complete_multipart_upload(&path, &upload_id, &parts)
            .await?;
        Ok::<_, MlflowError>(())
    }
    .await;
    match result {
        Ok(()) => proto_json(
            &art_pb::complete_multipart_upload::Response {},
            "mlflow.artifacts.CompleteMultipartUpload.Response",
        ),
        Err(e) => e.into_response(),
    }
}

/// POST `/mlflow-artifacts/mpu/abort/{*artifact_path}` —
/// `_abort_multipart_upload_artifact` (`handlers.py:3817`).
pub async fn proxy_abort_multipart(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Response {
    if !state.serve_artifacts() {
        return disabled_response(&parts);
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let artifact_path = mlflow_artifacts::validate_path_is_safe(&artifact_path)?;
        let artifact_path = workspace_scoped_repo_path(&state, &workspace, Some(&artifact_path))?
            .expect("non-empty artifact path remains present");
        let req: art_pb::AbortMultipartUpload =
            parse_control_body(&body, "mlflow.artifacts.AbortMultipartUpload")?;
        let path =
            mlflow_artifacts::multipart_upload_path(&req.path.unwrap_or_default(), &artifact_path);
        let upload_id = req.upload_id.unwrap_or_default();
        repo.abort_multipart_upload(&path, &upload_id).await?;
        Ok::<_, MlflowError>(())
    }
    .await;
    match result {
        Ok(()) => proto_json(
            &art_pb::abort_multipart_upload::Response {},
            "mlflow.artifacts.AbortMultipartUpload.Response",
        ),
        Err(e) => e.into_response(),
    }
}

/// GET `/mlflow-artifacts/presigned/{*artifact_path}` —
/// `_get_presigned_download_url` (`handlers.py:3848`). Local FS has no presigned
/// URLs (`_validate_support_multipart_download` → NOT_IMPLEMENTED).
pub async fn proxy_presigned_download(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(artifact_path): Path<String>,
    parts: Parts,
) -> Response {
    if !state.serve_artifacts() {
        return disabled_response(&parts);
    }
    let result = async {
        let repo = state.proxied_artifacts_repo()?;
        let path = mlflow_artifacts::validate_path_is_safe(&artifact_path)?;
        let path = workspace_scoped_repo_path(&state, &workspace, Some(&path))?
            .expect("non-empty artifact path remains present");
        if !repo.supports_multipart_download() {
            return Err(MlflowError::not_implemented(
                "Multipart download is not supported for the current artifact repository",
            ));
        }
        let ttl = mlflow_artifacts::presigned_download_ttl_seconds()?;
        let ttl = u64::try_from(ttl).map_err(|_| {
            MlflowError::internal_error(format!("Invalid presigned download expiration: {ttl}"))
        })?;
        repo.get_download_presigned_url(&path, ttl).await
    }
    .await;
    match result {
        Ok(result) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "url": result.url,
                "headers": result.headers.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
                "file_size": result.file_size,
            })
            .to_string(),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

// ===========================================================================
// helpers
// ===========================================================================

fn query_param(pairs: &[(String, String)], name: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn artifact_uri_scheme(uri: &str) -> Option<String> {
    let (scheme, _) = uri.split_once(':')?;
    (!scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then(|| scheme.to_ascii_lowercase())
}

fn fastapi_detail(status: StatusCode, detail: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({"detail": detail}).to_string(),
        ))
        .expect("valid response")
}

async fn local_file_response(
    path: &std::path::Path,
    artifact_path: &str,
    range_header: Option<&str>,
) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return MlflowError::resource_does_not_exist(format!(
                "No such artifact: '{artifact_path}'"
            ))
            .into_response();
        }
        Err(error) => {
            return MlflowError::internal_error(format!(
                "Failed to inspect artifact '{}': {error}",
                path.display()
            ))
            .into_response();
        }
    };
    if metadata.is_dir() {
        return fastapi_detail(
            StatusCode::BAD_REQUEST,
            &format!("Artifact path refers to a directory, not a file: '{artifact_path}'"),
        );
    }

    let size = metadata.len();
    let (status, start, end) = match parse_single_range(range_header, size) {
        Ok(Some((start, end))) => (StatusCode::PARTIAL_CONTENT, start, end),
        Ok(None) => (StatusCode::OK, 0, size.saturating_sub(1)),
        Err(RangeError::Malformed) => {
            return fastapi_detail(StatusCode::BAD_REQUEST, "Malformed Range header.");
        }
        Err(RangeError::Unsatisfiable) => {
            let mut response = fastapi_detail(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Requested Range Not Satisfiable",
            );
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).expect("valid range header"),
            );
            return response;
        }
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) => {
            return MlflowError::internal_error(format!(
                "Failed to open artifact '{}': {error}",
                path.display()
            ))
            .into_response();
        }
    };
    if let Err(error) = file.seek(std::io::SeekFrom::Start(start)).await {
        return MlflowError::internal_error(format!(
            "Failed to seek artifact '{}': {error}",
            path.display()
        ))
        .into_response();
    }
    let stream = futures::stream::try_unfold((file, length), |(mut file, remaining)| async move {
        if remaining == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        let mut chunk = vec![0; remaining.min(64 * 1024) as usize];
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        chunk.truncate(read);
        Ok(Some((Bytes::from(chunk), (file, remaining - read as u64))))
    });
    let filename = artifact_path.rsplit('/').next().unwrap_or(artifact_path);
    let mut response = Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            mlflow_artifacts::mime::guess_mime_type(artifact_path),
        )
        .header(
            header::CONTENT_DISPOSITION,
            mlflow_artifacts::mime::content_disposition_attachment(filename),
        )
        .header("X-Content-Type-Options", "nosniff")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from_stream(stream))
        .expect("valid response");
    if status == StatusCode::PARTIAL_CONTENT {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
                .expect("valid range header"),
        );
    }
    response
}

enum RangeError {
    Malformed,
    Unsatisfiable,
}

fn parse_single_range(value: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, RangeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.strip_prefix("bytes=").ok_or(RangeError::Malformed)?;
    if value.contains(',') {
        return Err(RangeError::Malformed);
    }
    let (start, end) = value.split_once('-').ok_or(RangeError::Malformed)?;
    if size == 0 {
        return Err(RangeError::Unsatisfiable);
    }
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| RangeError::Malformed)?;
        if suffix == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        let start = size.saturating_sub(suffix);
        return Ok(Some((start, size - 1)));
    }
    let start: u64 = start.parse().map_err(|_| RangeError::Malformed)?;
    if start >= size {
        return Err(RangeError::Unsatisfiable);
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| RangeError::Malformed)?
    };
    if end < start {
        return Err(RangeError::Unsatisfiable);
    }
    Ok(Some((start, end.min(size - 1))))
}

fn path_param(params: &HashMap<String, String>, name: &str) -> Result<String, MlflowError> {
    params
        .get(name)
        .cloned()
        .ok_or_else(|| MlflowError::internal_error(format!("Missing path parameter '{name}'.")))
}

fn basename(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((_, name)) => name,
        None => path,
    }
}

/// Render a proto message as MLflow pretty JSON (`Response(...); set_data(
/// message_to_json(...))`).
fn proto_json<M: prost::Message>(msg: &M, type_name: &str) -> Response {
    match proto_response(msg, type_name) {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

/// Parse a bounded control-message body with the MLflow JSON codec
/// (unknown-field tolerant), mirroring `_get_request_message`.
fn parse_control_body<M: prost::Message + Default>(
    body: &Bytes,
    type_name: &str,
) -> Result<M, MlflowError> {
    if body.len() > MAX_CONTROL_BODY_BYTES {
        return Err(MlflowError::invalid_parameter_value(
            "Request body is too large.",
        ));
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| MlflowError::invalid_parameter_value("Request body is not valid UTF-8"))?;
    let json = if text.trim().is_empty() { "{}" } else { text };
    mlflow_proto::from_mlflow_json(json, type_name)
        .map_err(|e| MlflowError::invalid_parameter_value(format!("Malformed request: {e}")))
}
