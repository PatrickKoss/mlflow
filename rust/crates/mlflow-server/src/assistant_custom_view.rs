//! Structured Custom View responses for Assistant CLI providers.

use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::assistant::AssistantEvent;

pub const RENDER_CUSTOM_VIEW_TOOL_NAME: &str = "render_custom_view";

pub const CUSTOM_VIEW_STRUCTURED_OUTPUT_INSTRUCTIONS: &str = concat!(
    "Your final response MUST match the provided JSON schema. Set \"type\" to\n",
    "\"render_custom_view\" only when the user asks to build or modify the custom view;\n",
    "put the complete A2UI specification in \"messages\" and a short view name in \"title\". For any\n",
    "other response, set \"type\" to \"message\", answer in \"text\", and return an empty \"title\" and\n",
    "\"messages\" array. Do not call a render tool and do not wrap the JSON in a code fence.\n",
);

pub const STRINGIFIED_CUSTOM_VIEW_STRUCTURED_OUTPUT_INSTRUCTIONS: &str = concat!(
    "Your final response MUST match the provided JSON schema. Set \"type\" to\n",
    "\"render_custom_view\" only when the user asks to build or modify the custom view;\n",
    "put the complete A2UI specification in \"messages\" and a short view name in \"title\". For any\n",
    "other response, set \"type\" to \"message\", answer in \"text\", and return an empty \"title\" and\n",
    "\"messages\" array. Do not call a render tool and do not wrap the JSON in a code fence.\n",
    "\nFor this provider, the response schema defines \"messages\" as a string. JSON-encode the complete\n",
    "A2UI message array into that string. For a normal message, return \"messages\": \"[]\". This is only\n",
    "the transport encoding: the decoded value must be the same A2UI message array described above.\n",
    "The decoded string must start with \"[\", end with \"]\", and contain nothing after that final \"]\".\n",
);

pub fn custom_view_response_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": ["message", RENDER_CUSTOM_VIEW_TOOL_NAME]},
            "text": {
                "type": "string",
                "description": "The conversational response shown in chat. For a rendered view, briefly describe what was created or changed."
            },
            "title": {
                "type": "string",
                "description": "A short display title when type is render_custom_view; otherwise an empty string."
            },
            "messages": {
                "type": "array",
                "description": "The complete A2UI message list when type is render_custom_view; otherwise empty.",
                "items": {"type": "object"}
            }
        },
        "required": ["type", "text", "title", "messages"],
        "additionalProperties": false
    })
}

pub fn stringified_custom_view_response_schema() -> Value {
    let mut schema = custom_view_response_schema();
    schema["properties"]["messages"] = json!({
        "type": "string",
        "description": "The complete A2UI message array encoded as JSON when type is render_custom_view; otherwise the JSON string '[]'."
    });
    schema
}

#[derive(Debug, Clone, PartialEq)]
pub struct CustomViewResponse {
    pub response_type: String,
    pub text: String,
    pub title: String,
    pub messages: Vec<Value>,
}

pub fn parse_custom_view_response(value: Value) -> Result<CustomViewResponse, String> {
    let value = match value {
        Value::String(value) => serde_json::from_str(&value).map_err(|error| error.to_string())?,
        value => value,
    };
    let object = value
        .as_object()
        .ok_or_else(|| "Input should be a valid dictionary or object".to_string())?;
    for key in object.keys() {
        if !matches!(key.as_str(), "type" | "text" | "title" | "messages") {
            return Err(format!("Extra inputs are not permitted: {key}"));
        }
    }
    let response_type = required_string(object, "type")?;
    if !matches!(
        response_type.as_str(),
        "message" | RENDER_CUSTOM_VIEW_TOOL_NAME
    ) {
        return Err("type must be 'message' or 'render_custom_view'".to_string());
    }
    let text = required_string(object, "text")?;
    let title = required_string(object, "title")?;
    let messages = match object.get("messages") {
        Some(Value::Array(messages)) => messages.clone(),
        Some(Value::String(messages)) => parse_stringified_messages(messages)?,
        Some(_) => return Err("messages must be an array".to_string()),
        None => return Err("messages: Field required".to_string()),
    };
    if messages.iter().any(|message| !message.is_object()) {
        return Err("messages entries must be objects".to_string());
    }
    Ok(CustomViewResponse {
        response_type,
        text,
        title,
        messages,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key}: Field required")),
    }
}

fn parse_stringified_messages(value: &str) -> Result<Vec<Value>, String> {
    if let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(value) {
        return Ok(messages);
    }

    let mut stream = serde_json::Deserializer::from_str(value).into_iter::<Value>();
    let parsed = stream
        .next()
        .transpose()
        .map_err(|_| "messages must be a JSON-encoded array".to_string())?
        .ok_or_else(|| "messages must be a JSON-encoded array".to_string())?;
    let trailing = value[stream.byte_offset()..].trim();
    if let Value::Array(messages) = parsed {
        if !trailing.is_empty()
            && trailing.len() <= 4
            && trailing
                .chars()
                .all(|character| matches!(character, '}' | ']'))
        {
            return Ok(messages);
        }
    }
    Err("messages must be a JSON-encoded array".to_string())
}

pub fn custom_view_response_events(response: CustomViewResponse) -> Vec<AssistantEvent> {
    let mut events = Vec::new();
    if !response.text.is_empty() {
        events.push(AssistantEvent::new(
            "message",
            json!({"message":{"role":"assistant","content":[{"text":response.text}]}}),
        ));
    }
    if response.response_type == RENDER_CUSTOM_VIEW_TOOL_NAME {
        let request_id = Uuid::new_v4().to_string();
        let tool_input = json!({"title":response.title,"messages":response.messages});
        events.push(AssistantEvent::new(
            "message",
            json!({"message":{"role":"assistant","content":[{"id":request_id,"name":RENDER_CUSTOM_VIEW_TOOL_NAME,"input":tool_input}]}}),
        ));
        events.push(AssistantEvent::client_tool_call(
            request_id,
            RENDER_CUSTOM_VIEW_TOOL_NAME,
            tool_input,
            true,
        ));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_stringified_messages_suffix_tolerance() {
        for suffix in ["", "}", "]}", "}}]]"] {
            let messages = format!(
                "{}{suffix}",
                serde_json::to_string(&json!([{"beginRendering":{"surfaceId":"main"}}])).unwrap()
            );
            let response = parse_custom_view_response(json!({
                "type":"render_custom_view",
                "text":"Built it",
                "title":"Errors",
                "messages":messages,
            }))
            .unwrap();
            assert_eq!(response.messages.len(), 1);
        }
        for suffix in ["abc", "}}]]]", "}"] {
            let value = if suffix == "}" {
                "{}".to_string()
            } else {
                format!("[]{suffix}")
            };
            assert!(parse_custom_view_response(json!({
                "type":"message","text":"x","title":"","messages":value
            }))
            .is_err());
        }
    }

    #[test]
    fn custom_view_events_end_in_terminal_client_tool_call() {
        let events = custom_view_response_events(CustomViewResponse {
            response_type: RENDER_CUSTOM_VIEW_TOOL_NAME.to_string(),
            text: "Built it".to_string(),
            title: "Errors".to_string(),
            messages: vec![json!({"beginRendering":{"surfaceId":"main"}})],
        });
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].event_type, "client_tool_call");
        assert_eq!(events[2].data["continuation"], "terminal");
    }
}
