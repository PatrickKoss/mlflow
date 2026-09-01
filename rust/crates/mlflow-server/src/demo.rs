//! Ajax demo-data route state machine adjacent to the Phase 20 GenAI surface.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use mlflow_error::MlflowError;
use mlflow_store::store::LifecycleStage;
use mlflow_store::WorkspaceArtifactRoot;
use serde_json::{json, Value};

use crate::state::AppState;
use crate::workspace::Workspace;

const DEMO_EXPERIMENT_NAME: &str = "MLflow Demo";
const DEMO_CUSTOM_VIEW_ID: &str = "mlflow-demo-span-review";
const DEMO_CUSTOM_VIEW_TAG_KEY: &str = "mlflow.customView.view.v1.mlflow-demo-span-review";
const MAX_EXPERIMENT_TAG_VAL_LENGTH: usize = 20_000;
const FEATURES: [(&str, i32); 7] = [
    ("prompts", 1),
    ("traces", 3),
    ("custom_view", 1),
    ("evaluation", 2),
    ("judges", 1),
    ("issues", 4),
    ("review_queues", 1),
];

pub async fn generate(
    State(state): State<AppState>,
    workspace: Workspace,
    body: Bytes,
) -> Response {
    match generate_impl(&state, workspace.name(), &body).await {
        Ok(value) => flask_json(value),
        Err(error) => error.into_response(),
    }
}

pub async fn delete(State(state): State<AppState>, workspace: Workspace) -> Response {
    match delete_impl(&state, workspace.name()).await {
        Ok(value) => flask_json(value),
        Err(error) => error.into_response(),
    }
}

async fn generate_impl(
    state: &AppState,
    workspace: &str,
    body: &[u8],
) -> Result<Value, MlflowError> {
    // `request.get_json(silent=True) or {}`: malformed/non-object/falsey JSON
    // behaves like an empty request and therefore selects every generator.
    let request: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    let selected = selected_features(request.get("features"));
    let mut experiment = state
        .tracking_store()
        .get_experiment_by_name(workspace, DEMO_EXPERIMENT_NAME)
        .await?;

    let to_generate = if let Some(active) = experiment
        .as_ref()
        .filter(|experiment| experiment.lifecycle_stage == LifecycleStage::ACTIVE)
    {
        selected
            .iter()
            .filter(|(name, version)| !feature_is_generated(active, name, *version))
            .copied()
            .collect::<Vec<_>>()
    } else {
        selected.clone()
    };

    if to_generate.is_empty() {
        if let Some(active) = experiment
            .as_ref()
            .filter(|experiment| experiment.lifecycle_stage == LifecycleStage::ACTIVE)
        {
            return Ok(json!({
                "experiment_id": active.experiment_id,
                "features_generated": [],
                "navigation_url": format!("/experiments/{}", active.experiment_id),
                "status": "exists",
            }));
        }
    }

    if !to_generate.is_empty() {
        let experiment_id = match experiment.as_ref() {
            Some(existing) if existing.lifecycle_stage == LifecycleStage::DELETED => {
                state
                    .tracking_store()
                    .restore_experiment(workspace, &existing.experiment_id)
                    .await?;
                existing.experiment_id.clone()
            }
            Some(existing) => existing.experiment_id.clone(),
            None => create_demo_experiment(state, workspace).await?,
        };
        for (name, version) in &to_generate {
            if *name == "custom_view" {
                state
                    .tracking_store()
                    .set_experiment_tag(
                        workspace,
                        &experiment_id,
                        DEMO_CUSTOM_VIEW_TAG_KEY,
                        &serialize_demo_custom_view(),
                    )
                    .await?;
            }
            state
                .tracking_store()
                .set_experiment_tag(
                    workspace,
                    &experiment_id,
                    &format!("mlflow.demo.version.{name}"),
                    &version.to_string(),
                )
                .await?;
        }
        experiment = state
            .tracking_store()
            .get_experiment_by_name(workspace, DEMO_EXPERIMENT_NAME)
            .await?;
    }

    let experiment_id = experiment
        .as_ref()
        .map(|experiment| experiment.experiment_id.clone());
    let navigation_url = experiment_id
        .as_ref()
        .map(|id| format!("/experiments/{id}"))
        .unwrap_or_else(|| "/experiments".to_string());
    Ok(json!({
        "experiment_id": experiment_id,
        "features_generated": to_generate.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        "navigation_url": navigation_url,
        "status": "created",
    }))
}

async fn delete_impl(state: &AppState, workspace: &str) -> Result<Value, MlflowError> {
    let experiment = state
        .tracking_store()
        .get_experiment_by_name(workspace, DEMO_EXPERIMENT_NAME)
        .await?;
    let mut deleted = Vec::new();
    if let Some(experiment) = experiment
        .as_ref()
        .filter(|experiment| experiment.lifecycle_stage == LifecycleStage::ACTIVE)
    {
        for (name, _) in FEATURES {
            let exists = if name == "custom_view" {
                experiment.tags.iter().any(|tag| {
                    tag.key == DEMO_CUSTOM_VIEW_TAG_KEY
                        && tag.value.as_deref().is_some_and(|value| !value.is_empty())
                })
            } else {
                experiment
                    .tags
                    .iter()
                    .any(|tag| tag.key == format!("mlflow.demo.version.{name}"))
            };
            if exists {
                deleted.push(name);
            }
        }
        if deleted.contains(&"custom_view") {
            state
                .tracking_store()
                .delete_experiment_tag(
                    workspace,
                    &experiment.experiment_id,
                    DEMO_CUSTOM_VIEW_TAG_KEY,
                )
                .await?;
        }
        state
            .tracking_store()
            .delete_experiment(workspace, &experiment.experiment_id)
            .await?;
    }
    Ok(json!({"features_deleted": deleted, "status": "deleted"}))
}

fn feature_is_generated(experiment: &mlflow_store::Experiment, name: &str, version: i32) -> bool {
    let version_matches = experiment.tags.iter().any(|tag| {
        tag.key == format!("mlflow.demo.version.{name}")
            && tag.value.as_deref() == Some(&version.to_string())
    });
    if name != "custom_view" {
        return version_matches;
    }
    version_matches
        && experiment.tags.iter().any(|tag| {
            tag.key == DEMO_CUSTOM_VIEW_TAG_KEY
                && tag.value.as_deref().is_some_and(|value| !value.is_empty())
        })
}

fn span_field(span_ref: &Value, field: &str) -> Value {
    json!({"$source": "spanField", "spanRef": span_ref, "field": field})
}

fn span_io_card(prefix: &str, span_ref: Value) -> (String, Vec<Value>) {
    let card_id = format!("{prefix}-card");
    let col_id = format!("{prefix}-col");
    let title_id = format!("{prefix}-title");
    let input_id = format!("{prefix}-in");
    let output_id = format!("{prefix}-out");
    let accuracy_id = format!("{prefix}-accuracy");
    let why_id = format!("{prefix}-why");
    let span_id = json!({"$spanRef": span_ref});
    let options = json!([
        {"label": "Super accurate", "value": "Super accurate"},
        {"label": "Accurate", "value": "Accurate"},
        {"label": "Somewhat accurate", "value": "Somewhat accurate"},
        {"label": "Not very accurate", "value": "Not very accurate"},
        {"label": "Not accurate", "value": "Not accurate"}
    ]);
    let components = vec![
        json!({"id": card_id, "component": "Card", "child": col_id, "renderIfSpan": span_ref}),
        json!({
            "id": col_id,
            "component": "Column",
            "children": [title_id, input_id, output_id, accuracy_id, why_id]
        }),
        json!({
            "id": title_id,
            "component": "Text",
            "variant": "h4",
            "text": span_field(&span_ref, "name")
        }),
        json!({
            "id": input_id,
            "component": "KeyValueViewer",
            "label": "Input",
            "value": span_field(&span_ref, "inputs"),
            "initialFormat": "json"
        }),
        json!({
            "id": output_id,
            "component": "KeyValueViewer",
            "label": "Output",
            "value": span_field(&span_ref, "outputs"),
            "initialFormat": "json"
        }),
        json!({
            "id": accuracy_id,
            "component": "RadioGroup",
            "label": "Accuracy",
            "name": "Accuracy",
            "formId": "feedback",
            "spanId": span_id,
            "options": options
        }),
        json!({
            "id": why_id,
            "component": "FeedbackInputText",
            "label": "Why?",
            "name": "Accuracy",
            "field": "rationale",
            "formId": "feedback",
            "spanId": span_id,
            "placeholder": "Optional rationale"
        }),
    ];
    (card_id, components)
}

/// `mlflow/demo/generators/custom_view.py:build_demo_custom_view`.
fn build_demo_custom_view() -> Value {
    let (root_card_id, root_components) = span_io_card("root", json!("root"));
    let child_span_cards = [
        ("embed", json!({"type": "EMBEDDING", "nth": 0})),
        ("retrieve", json!({"type": "RETRIEVER", "nth": 0})),
        ("chain1", json!({"type": "CHAIN", "nth": 1})),
        ("llm0", json!({"type": "LLM", "nth": 0})),
        ("llm1", json!({"type": "LLM", "nth": 1})),
        ("llm2", json!({"type": "LLM", "nth": 2})),
        ("tool0", json!({"type": "TOOL", "nth": 0})),
        ("tool1", json!({"type": "TOOL", "nth": 1})),
    ];
    let mut child_card_ids = Vec::new();
    let mut child_components = Vec::new();
    for (prefix, selector) in child_span_cards {
        let (card_id, components) = span_io_card(prefix, selector);
        child_card_ids.push(card_id);
        child_components.extend(components);
    }

    let mut children = vec!["metrics".to_string(), root_card_id];
    children.extend(child_card_ids);
    children.push("submit".to_string());
    let mut components = vec![
        json!({"id": "root", "component": "Column", "children": children}),
        json!({
            "id": "metrics",
            "component": "Row",
            "align": "stretch",
            "children": ["stat-status", "stat-latency", "stat-tokens"]
        }),
        json!({
            "id": "stat-status",
            "component": "StatCard",
            "value": {"$source": "metrics.status"},
            "label": "Status",
            "icon": "checklist",
            "tone": "info"
        }),
        json!({
            "id": "stat-latency",
            "component": "StatCard",
            "value": {"$source": "metrics.latency"},
            "label": "Latency",
            "icon": "clock",
            "tone": "info"
        }),
        json!({
            "id": "stat-tokens",
            "component": "StatCard",
            "value": {"$source": "metrics.totalTokens"},
            "label": "Tokens",
            "icon": "hash",
            "tone": "info"
        }),
    ];
    components.extend(root_components);
    components.extend(child_components);
    components.push(json!({
        "id": "submit",
        "component": "FeedbackSubmit",
        "label": "Submit feedback",
        "formId": "feedback"
    }));

    json!({
        "id": DEMO_CUSTOM_VIEW_ID,
        "name": "Span review",
        "label": "Span inputs, outputs, and accuracy",
        "instruction": "Show each span's input and output as cards and collect a per-span Accuracy rating from Super accurate to Not accurate, submitted together.",
        "template": [{
            "version": "v0.9",
            "updateComponents": {"surfaceId": "main", "components": components}
        }],
        "createdAtMs": 1
    })
}

fn serialize_demo_custom_view() -> String {
    let payload = serde_json::to_string(&build_demo_custom_view()).expect("custom view serializes");
    assert!(payload.len() <= MAX_EXPERIMENT_TAG_VAL_LENGTH);
    payload
}

fn selected_features(value: Option<&Value>) -> Vec<(&'static str, i32)> {
    match value {
        None | Some(Value::Null) => FEATURES.to_vec(),
        Some(Value::Array(values)) => FEATURES
            .into_iter()
            .filter(|(name, _)| values.iter().any(|value| value.as_str() == Some(name)))
            .collect(),
        Some(Value::String(value)) => FEATURES
            .into_iter()
            .filter(|(name, _)| value.contains(name))
            .collect(),
        Some(Value::Object(value)) => FEATURES
            .into_iter()
            .filter(|(name, _)| value.contains_key(*name))
            .collect(),
        _ => Vec::new(),
    }
}

async fn create_demo_experiment(state: &AppState, workspace: &str) -> Result<String, MlflowError> {
    match state.workspace_store() {
        Some(workspace_store) => {
            let (root, should_append) = workspace_store
                .resolve_artifact_root(Some(state.tracking_store().artifact_root_uri()), workspace)
                .await?;
            state
                .tracking_store()
                .create_experiment_workspace_scoped(
                    workspace,
                    DEMO_EXPERIMENT_NAME,
                    &[],
                    &WorkspaceArtifactRoot::Scoped {
                        root: root.unwrap_or_default(),
                        workspace: workspace.to_string(),
                        should_append,
                    },
                )
                .await
        }
        None => {
            state
                .tracking_store()
                .create_experiment(workspace, DEMO_EXPERIMENT_NAME, None, &[])
                .await
        }
    }
}

fn flask_json(value: Value) -> Response {
    let mut body = serde_json::to_string(&value).expect("demo response serializes");
    body.push('\n');
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn custom_view_payload_matches_seed_shape() {
        let payload = serialize_demo_custom_view();
        let view: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(view["id"], DEMO_CUSTOM_VIEW_ID);
        assert_eq!(view["name"], "Span review");
        assert_eq!(view["createdAtMs"], 1);
        assert_eq!(view["template"][0]["version"], "v0.9");
        assert_eq!(payload.len(), 12_940);
        assert_eq!(
            format!("{:x}", Sha256::digest(payload.as_bytes())),
            "47a4717dc7df880276ea06b21799c652ccffd9579440351db836775acafcaef8"
        );
    }

    #[test]
    fn custom_view_is_registered_between_traces_and_evaluation() {
        assert_eq!(
            FEATURES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "prompts",
                "traces",
                "custom_view",
                "evaluation",
                "judges",
                "issues",
                "review_queues"
            ]
        );
    }
}
