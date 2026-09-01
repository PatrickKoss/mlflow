//! Generic jobs GET/cancel handlers.
//!
//! Python currently exposes two aliases with intentionally different JSON:
//! the Flask `/mlflow/jobs` handlers return lifecycle details only, while the
//! native FastAPI `/jobs` router returns the complete job model.

use axum::extract::{Extension, Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use mlflow_error::MlflowError;
use mlflow_genai::JobKind;
use mlflow_store::{python_json_dumps, Job, JobStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth_middleware::AuthContext;
use crate::state::AppState;
use crate::workspace::Workspace;

#[derive(Serialize)]
pub(crate) struct FastApiJob {
    job_id: String,
    creation_time: i64,
    job_name: String,
    params: Value,
    timeout: Option<f64>,
    status: JobStatus,
    result: Option<Value>,
    retry_count: i64,
    last_update_time: i64,
    status_details: Option<Value>,
}

impl FastApiJob {
    fn from_job(job: Job) -> Result<Self, MlflowError> {
        let params = serde_json::from_str(&job.params)
            .map_err(|error| MlflowError::internal_error(error.to_string()))?;
        let result = job.parsed_result()?;
        Ok(Self {
            job_id: job.job_id,
            creation_time: job.creation_time,
            job_name: job.job_name,
            params,
            timeout: job.timeout,
            status: job.status,
            result,
            retry_count: job.retry_count,
            last_update_time: job.last_update_time,
            status_details: job.status_details,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct SubmitJobPayload {
    job_name: String,
    params: Value,
    timeout: Option<f64>,
}

#[derive(Deserialize)]
pub(crate) struct SearchJobPayload {
    job_name: Option<String>,
    params: Option<Value>,
    statuses: Option<Vec<JobStatus>>,
}

#[derive(Serialize)]
pub(crate) struct SearchJobsResponse {
    jobs: Vec<FastApiJob>,
}

/// Python's native job router catches `MlflowException` and raises FastAPI's
/// `HTTPException`, so its failures deliberately do not use the Flask/MLflow
/// error envelope returned by the legacy aliases below.
pub(crate) struct FastApiJobError(MlflowError);

impl From<MlflowError> for FastApiJobError {
    fn from(error: MlflowError) -> Self {
        Self(error)
    }
}

impl IntoResponse for FastApiJobError {
    fn into_response(self) -> Response {
        let status = self.0.http_status();
        let body = serde_json::json!({"detail": self.0.message});
        (status, Json(body)).into_response()
    }
}

pub(crate) async fn flask_get_job(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(job_id): Path<String>,
) -> Result<Response, MlflowError> {
    let job = state.job_store().get_job(workspace.name(), &job_id).await?;
    flask_json(serde_json::json!({
        "result": job.parsed_result()?,
        "status": job.status,
        "status_details": job.status_details,
    }))
}

pub(crate) async fn flask_cancel_job(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(job_id): Path<String>,
) -> Result<Response, MlflowError> {
    let job = state
        .job_store()
        .cancel_job(workspace.name(), &job_id)
        .await?;
    flask_json(serde_json::json!({
        "result": job.parsed_result()?,
        "status": job.status,
    }))
}

pub(crate) async fn fastapi_get_job(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(job_id): Path<String>,
) -> Result<Json<FastApiJob>, FastApiJobError> {
    let job = state.job_store().get_job(workspace.name(), &job_id).await?;
    Ok(Json(FastApiJob::from_job(job)?))
}

pub(crate) async fn fastapi_submit_job(
    State(state): State<AppState>,
    workspace: Workspace,
    auth: Option<Extension<AuthContext>>,
    Json(payload): Json<SubmitJobPayload>,
) -> Result<Json<FastApiJob>, FastApiJobError> {
    payload.job_name.parse::<JobKind>().map_err(|_| {
        MlflowError::invalid_parameter_value(format!("Invalid job name: {}", payload.job_name))
    })?;
    if !payload.params.is_object() {
        return Err(MlflowError::invalid_parameter_value(
            "When calling 'submit_job', the 'params' argument must be a dict.",
        )
        .into());
    }
    let creator = auth.as_ref().map(|Extension(auth)| auth.username.as_str());
    let job = state
        .job_store()
        .create_job_with_creator(
            workspace.name(),
            &payload.job_name,
            &python_json_dumps(&payload.params, false),
            payload.timeout,
            creator,
        )
        .await?;
    Ok(Json(FastApiJob::from_job(job)?))
}

pub(crate) async fn fastapi_search_jobs(
    State(state): State<AppState>,
    workspace: Workspace,
    Json(payload): Json<SearchJobPayload>,
) -> Result<Json<SearchJobsResponse>, FastApiJobError> {
    let statuses = payload.statuses.unwrap_or_default();
    let jobs = state
        .job_store()
        .list_jobs(
            workspace.name(),
            payload.job_name.as_deref(),
            &statuses,
            None,
            None,
            payload.params.as_ref(),
        )
        .await?
        .into_iter()
        .map(FastApiJob::from_job)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(SearchJobsResponse { jobs }))
}

pub(crate) async fn fastapi_cancel_job(
    State(state): State<AppState>,
    workspace: Workspace,
    Path(job_id): Path<String>,
) -> Result<Json<FastApiJob>, FastApiJobError> {
    let job = state
        .job_store()
        .cancel_job(workspace.name(), &job_id)
        .await?;
    Ok(Json(FastApiJob::from_job(job)?))
}

/// Flask's `jsonify` sorts keys, emits compact JSON, and appends one newline.
/// Callers construct values in that sorted order, so preserving insertion
/// order yields byte-identical bodies.
fn flask_json(value: Value) -> Result<Response, MlflowError> {
    let mut body = serde_json::to_string(&value)
        .map_err(|error| MlflowError::internal_error(error.to_string()))?;
    body.push('\n');
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}
