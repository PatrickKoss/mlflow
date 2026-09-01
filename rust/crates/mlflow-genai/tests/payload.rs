use mlflow_genai::SerializedScorer;

const BUILTIN: &str = include_str!("fixtures/builtin_response_length_scorer.json");
const INSTRUCTIONS: &str = include_str!("fixtures/instructions_judge_scorer.json");

#[test]
fn parses_python_builtin_payload() {
    let scorer = SerializedScorer::from_json(BUILTIN).expect("Python fixture parses");
    let SerializedScorer::Builtin(payload) = scorer else {
        panic!("expected builtin payload");
    };
    assert_eq!(payload.class_name, "ResponseLength");
    assert_eq!(payload.common.name, "response_length");
    assert_eq!(payload.common.serialization_version, 1);
    assert_eq!(payload.pydantic_data["unit"], "words");
    assert_eq!(payload.pydantic_data["min_length"], 2);
    assert_eq!(payload.pydantic_data["max_length"], 4);
}

#[test]
fn builtin_extra_headers_round_trip_without_loss() {
    let value = serde_json::json!({
        "name": "safety",
        "builtin_scorer_class": "Safety",
        "builtin_scorer_pydantic_data": {
            "model": "sap-ai-core:/gpt-4.1",
            "extra_headers": {"AI-Resource-Group": "default"}
        },
        "mlflow_version": "3.14.1.dev0",
        "serialization_version": 1
    });
    let scorer = SerializedScorer::from_value(value.clone()).unwrap();
    assert_eq!(scorer.to_json_value(), value);
    let SerializedScorer::Builtin(payload) = scorer else {
        panic!("expected builtin scorer")
    };
    assert_eq!(
        payload.pydantic_data["extra_headers"]["AI-Resource-Group"],
        "default"
    );
}

#[test]
fn parses_python_instructions_payload() {
    let scorer = SerializedScorer::from_json(INSTRUCTIONS).expect("Python fixture parses");
    let SerializedScorer::Instructions(payload) = scorer else {
        panic!("expected instructions payload");
    };
    assert_eq!(payload.common.name, "concise_answer");
    assert_eq!(payload.pydantic_data["model"], "openai:/mock-judge");
}

#[test]
fn preserves_python_ensemble_payload_without_implementing_it() {
    let value = serde_json::json!({
        "name": "conformance-ensemble",
        "aggregations": null,
        "description": null,
        "is_session_level_scorer": false,
        "mlflow_version": "3.15.3.dev0",
        "serialization_version": 1,
        "builtin_scorer_class": null,
        "builtin_scorer_pydantic_data": null,
        "call_source": null,
        "call_signature": null,
        "original_func_name": null,
        "instructions_judge_pydantic_data": null,
        "memory_augmented_judge_data": null,
        "third_party_scorer_data": null,
        "ensemble_scorer_data": {
            "ensemble_fn": "majority_vote",
            "scorers": [{
                "name": "response_length",
                "builtin_scorer_class": "ResponseLength",
                "builtin_scorer_pydantic_data": {"max_length": 100}
            }]
        }
    });
    let scorer = SerializedScorer::from_value(value.clone()).unwrap();
    assert_eq!(scorer.to_json_value(), value);
    let SerializedScorer::Ensemble { common, data } = scorer else {
        panic!("expected ensemble scorer")
    };
    assert_eq!(common.name, "conformance-ensemble");
    assert_eq!(data["ensemble_fn"], "majority_vote");
    assert_eq!(data["scorers"].as_array().unwrap().len(), 1);
    assert!(SerializedScorer::Ensemble { common, data }
        .validate_for_oss_execution()
        .is_ok());
}

#[test]
fn rejects_multiple_representations() {
    let payload = serde_json::json!({
        "name": "ambiguous",
        "builtin_scorer_class": "ResponseLength",
        "builtin_scorer_pydantic_data": {},
        "instructions_judge_pydantic_data": {}
    });
    let error = SerializedScorer::from_json(&payload.to_string()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Failed to parse serialized scorer data: SerializedScorer cannot have multiple types of scorer fields present simultaneously"
    );
}

#[test]
fn rejects_phoenix_metric_for_elastic_2_license() {
    let payload = serde_json::json!({
        "name": "Hallucination",
        "mlflow_version": "3.0.0",
        "serialization_version": 1,
        "third_party_scorer_data": {
            "class": "Hallucination",
            "metric_name": "Hallucination",
            "model": "openai:/gpt-4"
        }
    });
    let scorer = SerializedScorer::from_json(&payload.to_string()).unwrap();
    let error = scorer.validate_for_oss_execution().unwrap_err();
    assert_eq!(
        error.to_string(),
        "Phoenix scorer metric 'Hallucination' is unavailable in the Rust server: \
         arize-phoenix-evals is licensed under Elastic-2.0, which is incompatible with \
         reimplementation in Apache-2.0 MLflow. Use the MLflow builtins Faithfulness \
         (Hallucination), RelevanceToQuery (Relevance), Correctness (QA), or Safety (Toxicity); \
         for Summarization and SQL, use a custom instructions judge."
    );
}
