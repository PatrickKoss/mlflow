use std::collections::BTreeMap;
use std::sync::OnceLock;

use base64::Engine;
use regex::Regex;
use serde_json::{json, Map, Value};
use url::Url;

use crate::trace::{conversation, TraceView};
use crate::{
    AssessmentSource, EngineError, EvalItem, Feedback, InstructionsJudgePayload, ScorerExecutor,
};

const JUDGE_BASE_PROMPT: &str = "You are an expert judge tasked with evaluating the performance of an AI\nagent on a particular query. You will be given instructions that describe the criteria and\nmethodology for evaluating the agent's performance on the query.";
const RESULT_DESCRIPTION: &str = "The evaluation rating/result";
const RATIONALE_DESCRIPTION: &str = "Detailed explanation for the evaluation";
const EMPTY_TRACE_USER_MESSAGE: &str = "Use the tools to inspect the trace and return the JSON rating per the system message. This message and your tool calls in this chat are not the input or response being judged. The trace lives only behind the tools.";
const IMAGE_TURN_TOOL_CALL_ID: &str = "_mlflow_image_turn_tool_call_id";
const DEFAULT_MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug)]
struct SpanImageResult {
    span_id: String,
    data_url: String,
}

enum JudgeToolResult {
    Text(String),
    Image(SpanImageResult),
}

const TRACE_PROMPT: &str = " Your job is to analyze a trace of the agent's execution on the
query and provide an evaluation rating in accordance with the instructions.

A *trace* is a step-by-step record of how the agent processed the query, including the input query
itself, all intermediate steps, decisions, and outputs. Each step in a trace is represented as a
*span*, which includes the inputs and outputs of that step, as well as latency information and
metadata.

The instructions containing the evaluation criteria and methodology are provided below, and they
refer to a placeholder called {{ trace }}. To read the actual trace, you will need to use the
tools provided to you. These tools enable you to 1. fetch trace metadata, timing, & execution
details, 2. list all spans in the trace with inputs and outputs, 3. search for specific text or
patterns across the entire trace, and much more. These tools do *not* require you to specify a
particular trace; the tools will select the relevant trace automatically (however, you *will* need
to specify *span* IDs when retrieving specific spans).

**Important: do not grade this conversation.** Your tool calls and their results in this chat
are how you inspect the trace; they are not actions the traced agent took. Inspect the trace via
the tools before producing a verdict.

In order to follow the instructions precisely and correctly, you must think methodically and act
step-by-step:

1. Thoroughly read the instructions to understand what information you need to gather from the trace
   in order to perform the evaluation, according to the criteria and methodology specified.
2. Look at the tools available to you, and use as many of them as necessary in order to gather the
   information you need from the trace.
3. Carefully read and analyze the information you gathered.
4. Think critically about whether you have enough information to produce an evaluation rating in
   accordance with the instructions. If you do not have enough information, or if you suspect that
   there is additional relevant information in the trace that you haven't gathered, then go back
   to steps 2 and 3.
5. Once you have gathered enough information, provide your evaluation rating in accordance with the
   instructions.

You *must* format your evaluation rating as a JSON object with the following fields. Pay close
attention to the field type of the evaluation rating (string, boolean, numeric, etc.), and ensure
that it conforms to the instructions.

Evaluation Rating Fields
------------------------
{evaluation_rating_fields}

Instructions
------------------------
{instructions}
";

pub(crate) async fn execute_instructions(
    executor: &ScorerExecutor,
    payload: &InstructionsJudgePayload,
    item: &EvalItem,
    gateway_url: Option<&str>,
) -> Result<Feedback, EngineError> {
    let instructions = required_str(&payload.pydantic_data, "instructions")?;
    let model_uri = required_str(&payload.pydantic_data, "model")?;
    let fields = instruction_field_order(instructions);
    validate_required_fields(&fields, item)?;
    let rationale_first = payload
        .pydantic_data
        .get("generate_rationale_first")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let schema = response_format(&payload.pydantic_data, rationale_first)?;
    let is_trace = fields.contains(&"trace");
    let system = if is_trace {
        let descriptions = output_field_descriptions(
            payload.pydantic_data.get("feedback_value_type"),
            rationale_first,
        );
        format!("{JUDGE_BASE_PROMPT}{TRACE_PROMPT}")
            .replace("{evaluation_rating_fields}", &descriptions)
            .replace("{instructions}", instructions)
    } else {
        add_output_format_instructions(
            &format!("{JUDGE_BASE_PROMPT}\n\nYour task: {instructions}."),
            payload.pydantic_data.get("feedback_value_type"),
            rationale_first,
        )
    };
    let user = build_user_message(
        &fields,
        item,
        payload
            .pydantic_data
            .get("include_tool_calls_in_conversation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        payload
            .pydantic_data
            .get("include_timing_in_conversation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )?;
    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ];
    let inference = payload
        .pydantic_data
        .get("inference_params")
        .and_then(Value::as_object);
    let completion = invoke(
        executor,
        model_uri,
        messages,
        schema,
        InvokeOptions {
            inference,
            extra_headers: extra_headers(&payload.pydantic_data),
            trace: if is_trace { item.trace.as_ref() } else { None },
            gateway_url,
        },
    )
    .await?;
    let mut feedback = completion.feedback(&payload.common.name, model_uri)?;
    feedback.metadata.get_or_insert_with(BTreeMap::new).insert(
        "guideline".to_string(),
        Value::String(instructions.to_string()),
    );
    Ok(feedback)
}

pub(crate) async fn invoke_prompt(
    executor: &ScorerExecutor,
    model_uri: &str,
    name: &str,
    prompt: String,
    inference: Option<&Map<String, Value>>,
    extra_headers: Option<&Map<String, Value>>,
    gateway_url: Option<&str>,
) -> Result<Feedback, EngineError> {
    let completion = invoke(
        executor,
        model_uri,
        vec![json!({"role": "user", "content": prompt})],
        default_response_format(),
        InvokeOptions {
            inference,
            extra_headers,
            trace: None,
            gateway_url,
        },
    )
    .await?;
    completion.feedback(name, model_uri)
}

struct JudgeCompletion {
    body: Value,
    content: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    cost_usd: f64,
}

impl JudgeCompletion {
    fn feedback(self, name: &str, model_uri: &str) -> Result<Feedback, EngineError> {
        let cleaned = strip_markdown_code_blocks(&self.content);
        let parsed: Value = serde_json::from_str(&cleaned)
            .map_err(|error| EngineError::MalformedGatewayResponse(error.to_string()))?;
        let value = parsed
            .get("result")
            .cloned()
            .ok_or(EngineError::InvalidScorerField("result"))?;
        let rationale = parsed
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .replace("Let's think step by step. ", "");
        let mut metadata = BTreeMap::new();
        if self.prompt_tokens > 0 {
            metadata.insert(
                "mlflow.assessment.judgeInputTokens".to_string(),
                json!(self.prompt_tokens),
            );
        }
        if self.completion_tokens > 0 {
            metadata.insert(
                "mlflow.assessment.judgeOutputTokens".to_string(),
                json!(self.completion_tokens),
            );
        }
        if self.cost_usd != 0.0 {
            metadata.insert(
                "mlflow.assessment.judgeCost".to_string(),
                json!(self.cost_usd),
            );
        }
        let _ = self.body;
        Ok(Feedback {
            name: name.to_string(),
            value,
            rationale,
            source: Some(AssessmentSource {
                source_type: "LLM_JUDGE".to_string(),
                source_id: Some(model_uri.to_string()),
            }),
            metadata: (!metadata.is_empty()).then_some(metadata),
            span_id: None,
            trace_id: None,
        })
    }
}

struct InvokeOptions<'a> {
    inference: Option<&'a Map<String, Value>>,
    extra_headers: Option<&'a Map<String, Value>>,
    trace: Option<&'a Value>,
    gateway_url: Option<&'a str>,
}

async fn invoke(
    executor: &ScorerExecutor,
    model_uri: &str,
    mut messages: Vec<Value>,
    response_format: Value,
    options: InvokeOptions<'_>,
) -> Result<JudgeCompletion, EngineError> {
    let InvokeOptions {
        inference,
        extra_headers,
        trace,
        gateway_url,
    } = options;
    let model = model_uri
        .split_once(":/")
        .map(|(_, model)| model)
        .filter(|model| !model.is_empty())
        .unwrap_or(model_uri);
    let tools: Value = serde_json::from_str(include_str!("judge_tools.json"))
        .expect("judge tool definitions are valid JSON");
    let max_iterations = std::env::var("MLFLOW_JUDGE_MAX_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20);
    let mut prompt_tokens = 0;
    let mut completion_tokens = 0;
    let mut cost_usd = 0.0;
    for _ in 0..max_iterations {
        let mut request = Map::new();
        request.insert("model".to_string(), Value::String(model.to_string()));
        request.insert(
            "messages".to_string(),
            Value::Array(provider_messages(&messages)),
        );
        if trace.is_some() {
            request.insert("tools".to_string(), tools.clone());
            request.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        }
        request.insert("response_format".to_string(), response_format.clone());
        if let Some(inference) = inference {
            request.extend(inference.clone());
        }
        let mut http_request = executor
            .client()
            .post(gateway_url.ok_or(EngineError::MissingGatewayUrl)?)
            .json(&Value::Object(request));
        if let Some(extra_headers) = extra_headers {
            for (name, value) in extra_headers {
                let value = value.as_str().ok_or_else(|| {
                    EngineError::Gateway(format!("extra header {name:?} must have a string value"))
                })?;
                http_request = http_request.header(name, value);
            }
        }
        let response = http_request
            .send()
            .await
            .map_err(|error| EngineError::Gateway(error.to_string()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| EngineError::Gateway(error.to_string()))?;
        if !status.is_success() {
            if is_context_window_error(status.as_u16(), &body) {
                if let Some(trimmed) = remove_oldest_tool_call_pair(&messages) {
                    messages = trimmed;
                    continue;
                }
            }
            return Err(EngineError::Gateway(format!("HTTP {status}: {body}")));
        }
        prompt_tokens += body
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        completion_tokens += body
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        cost_usd += body
            .pointer("/_hidden_params/response_cost")
            .or_else(|| body.get("response_cost"))
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let message = body
            .pointer("/choices/0/message")
            .and_then(Value::as_object)
            .ok_or_else(|| EngineError::MalformedGatewayResponse(body.to_string()))?;
        let tool_calls = message.get("tool_calls").and_then(Value::as_array);
        if let Some(tool_calls) = tool_calls.filter(|calls| !calls.is_empty()) {
            let trace = trace.ok_or_else(|| {
                EngineError::MalformedGatewayResponse("tool call without trace".to_string())
            })?;
            messages.push(Value::Object(message.clone()));
            let mut tool_responses = Vec::new();
            let mut image_turns = Vec::new();
            for call in tool_calls {
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| EngineError::MalformedGatewayResponse(call.to_string()))?;
                let arguments = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let arguments: Value = serde_json::from_str(arguments)
                    .map_err(|error| EngineError::Tool(error.to_string()))?;
                let call_id = call.get("id").cloned().unwrap_or(Value::Null);
                match invoke_judge_tool(executor, trace, name, &arguments).await {
                    JudgeToolResult::Text(content) => tool_responses.push(json!({
                        "role": "tool",
                        "content": content,
                        "tool_call_id": call_id,
                        "name": name,
                    })),
                    JudgeToolResult::Image(image) => {
                        tool_responses.push(json!({
                            "role": "tool",
                            "content":format!(
                                "Image for span {} fetched; it is shown in the following user message. Inspect it to answer.",
                                image.span_id
                            ),
                            "tool_call_id":call_id,
                            "name":name,
                        }));
                        image_turns.push(json!({
                            "role":"user",
                            "content":[
                                {"type":"text","text":format!("Fetched image for span {}:", image.span_id)},
                                {"type":"image_url","image_url":{"url":image.data_url}},
                            ],
                            (IMAGE_TURN_TOOL_CALL_ID):call_id,
                        }));
                    }
                }
            }
            messages.extend(tool_responses);
            messages.extend(image_turns);
            continue;
        }
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::MalformedGatewayResponse(body.to_string()))?
            .to_string();
        return Ok(JudgeCompletion {
            body,
            content,
            prompt_tokens,
            completion_tokens,
            cost_usd,
        });
    }
    Err(EngineError::Gateway(format!(
        "Judge model exceeded maximum number of iterations ({max_iterations})"
    )))
}

fn extra_headers(data: &Map<String, Value>) -> Option<&Map<String, Value>> {
    data.get("extra_headers").and_then(Value::as_object)
}

fn provider_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if let Some(message) = message.as_object_mut() {
                message.remove(IMAGE_TURN_TOOL_CALL_ID);
            }
            message
        })
        .collect()
}

async fn invoke_judge_tool(
    executor: &ScorerExecutor,
    trace: &Value,
    name: &str,
    arguments: &Value,
) -> JudgeToolResult {
    if name == "get_span_image" {
        return get_span_image(executor, trace, arguments).await;
    }
    match TraceView::new(trace).invoke_tool(name, arguments) {
        Ok(content) => JudgeToolResult::Text(content),
        Err(error) => JudgeToolResult::Text(format!("Error: {error}")),
    }
}

#[derive(Debug)]
struct AttachmentRef {
    attachment_id: String,
    content_type: String,
    size: Option<usize>,
}

async fn get_span_image(
    executor: &ScorerExecutor,
    trace: &Value,
    arguments: &Value,
) -> JudgeToolResult {
    let trace_id = trace
        .pointer("/info/trace_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let span_id = arguments
        .get("span_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let spans = trace
        .pointer("/data/spans")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if spans.is_empty() {
        return tool_text(format!("Error: trace '{trace_id}' has no spans"));
    }
    let Some(span) = spans
        .iter()
        .find(|span| span.get("span_id").and_then(Value::as_str) == Some(span_id))
    else {
        return tool_text(format!(
            "Error: span '{span_id}' not found in trace '{trace_id}'"
        ));
    };
    let serialized = serde_json::to_string(span).unwrap_or_default();
    let refs = attachment_ref_regex()
        .find_iter(&serialized)
        .map(|found| found.as_str())
        .collect::<Vec<_>>();
    if refs.is_empty() {
        return tool_text(format!(
            "Error: no mlflow-attachment:// image reference found in span '{span_id}' of trace '{trace_id}'"
        ));
    }
    let parsed = match arguments.get("attachment_index") {
        None | Some(Value::Null) => refs.iter().find_map(|value| {
            parse_attachment_ref(value).filter(|item| item.content_type.starts_with("image/"))
        }),
        Some(index) => {
            let index = index.as_i64().unwrap_or(-1);
            if index < 0 || usize::try_from(index).map_or(true, |index| index >= refs.len()) {
                return tool_text(format!(
                    "Error: attachment_index {index} is out of range for span '{span_id}' of trace '{trace_id}', which has {} attachment reference(s)",
                    refs.len()
                ));
            }
            let Some(parsed) = parse_attachment_ref(refs[index as usize]) else {
                return tool_text(format!(
                    "Error: could not parse attachment reference in span '{span_id}' of trace '{trace_id}'"
                ));
            };
            if !parsed.content_type.starts_with("image/") {
                return tool_text(format!(
                    "Error: attachment in span '{span_id}' of trace '{trace_id}' is not an image (content_type='{}')",
                    parsed.content_type
                ));
            }
            Some(parsed)
        }
    };
    let Some(parsed) = parsed else {
        return tool_text(format!(
            "Error: no image attachment found in span '{span_id}' of trace '{trace_id}'"
        ));
    };
    let limit = attachment_size_limit();
    if parsed.size.is_some_and(|size| size > limit) {
        return oversized_image(span_id, trace_id, parsed.size.unwrap(), limit);
    }
    let bytes = match executor
        .download_trace_attachment(trace_id, &parsed.attachment_id)
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            return tool_text(format!(
                "Error: failed to download image attachment in span '{span_id}' of trace '{trace_id}': {error}"
            ))
        }
    };
    if bytes.len() > limit {
        return oversized_image(span_id, trace_id, bytes.len(), limit);
    }
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    JudgeToolResult::Image(SpanImageResult {
        span_id: span_id.to_string(),
        data_url: format!("data:{};base64,{data}", parsed.content_type),
    })
}

fn tool_text(value: String) -> JudgeToolResult {
    JudgeToolResult::Text(value)
}

fn oversized_image(span_id: &str, trace_id: &str, size: usize, limit: usize) -> JudgeToolResult {
    tool_text(format!(
        "Error: image attachment in span '{span_id}' of trace '{trace_id}' is {size} bytes, exceeding the {limit} byte limit"
    ))
}

fn attachment_ref_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r#"mlflow-attachment://[^\s\"'\\]+"#).unwrap())
}

fn parse_attachment_ref(reference: &str) -> Option<AttachmentRef> {
    let url = Url::parse(reference).ok()?;
    if url.scheme() != "mlflow-attachment" {
        return None;
    }
    let attachment_id = url.host_str()?.to_string();
    let params = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let content_type = params.get("content_type")?.to_string();
    let _trace_id = params.get("trace_id")?;
    let size = params
        .get("size")
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| (value > 0).then(|| usize::try_from(value).ok()).flatten());
    Some(AttachmentRef {
        attachment_id,
        content_type,
        size,
    })
}

fn attachment_size_limit() -> usize {
    std::env::var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| (value > 0).then(|| usize::try_from(value).ok()).flatten())
        .unwrap_or(DEFAULT_MAX_IMAGE_BYTES)
}

fn remove_oldest_tool_call_pair(messages: &[Value]) -> Option<Vec<Value>> {
    let assistant_index = messages.iter().position(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
    })?;
    let ids = messages[assistant_index]
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| call.get("id"))
        .cloned()
        .collect::<Vec<_>>();
    Some(
        messages
            .iter()
            .enumerate()
            .filter(|(index, message)| {
                if *index == assistant_index {
                    return false;
                }
                let tool_response = message.get("role").and_then(Value::as_str) == Some("tool")
                    && message
                        .get("tool_call_id")
                        .is_some_and(|id| ids.contains(id));
                let image_turn = message
                    .get(IMAGE_TURN_TOOL_CALL_ID)
                    .is_some_and(|id| ids.contains(id));
                !tool_response && !image_turn
            })
            .map(|(_, message)| message.clone())
            .collect(),
    )
}

fn is_context_window_error(status: u16, body: &Value) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let detail = body.to_string().to_ascii_lowercase();
    (detail.contains("context window") || detail.contains("context length"))
        && (detail.contains("exceed") || detail.contains("maximum") || detail.contains("too long"))
}

fn build_user_message(
    fields: &[&str],
    item: &EvalItem,
    include_tool_calls: bool,
    include_timing: bool,
) -> Result<String, EngineError> {
    let mut parts = Vec::new();
    for field in fields.iter().filter(|field| **field != "trace") {
        let value = match *field {
            "inputs" => item.inputs.as_ref().map(pretty_json),
            "outputs" => item.outputs.as_ref().map(pretty_json),
            "expectations" => item.expectations.as_ref().map(pretty_json),
            "conversation" => item.session.as_ref().map(|session| {
                pretty_json(&Value::Array(conversation(
                    session,
                    include_tool_calls,
                    include_timing,
                )))
            }),
            _ => None,
        };
        if let Some(value) = value {
            parts.push(format!("{field}: {value}"));
        }
    }
    Ok(if parts.is_empty() {
        EMPTY_TRACE_USER_MESSAGE.to_string()
    } else {
        parts.join("\n")
    })
}

fn validate_required_fields(fields: &[&str], item: &EvalItem) -> Result<(), EngineError> {
    for field in fields {
        let missing = match *field {
            "inputs" => item.inputs.is_none(),
            "outputs" => item.outputs.is_none(),
            "expectations" => item.expectations.is_none(),
            "trace" => item.trace.is_none(),
            "conversation" => item.session.is_none(),
            _ => false,
        };
        if missing {
            let field = match *field {
                "inputs" => "inputs",
                "outputs" => "outputs",
                "expectations" => "expectations",
                "trace" => "trace",
                "conversation" => "session",
                _ => unreachable!("instruction fields are closed"),
            };
            return Err(EngineError::InvalidScorerField(field));
        }
    }
    Ok(())
}

fn instruction_field_order(instructions: &str) -> Vec<&'static str> {
    const FIELDS: [&str; 5] = ["inputs", "outputs", "trace", "expectations", "conversation"];
    let mut positions = FIELDS
        .into_iter()
        .filter_map(|field| {
            instructions
                .match_indices("{{")
                .find_map(|(start, _)| {
                    let tail = &instructions[start + 2..];
                    let end = tail.find("}}")?;
                    (tail[..end].trim() == field).then_some(start)
                })
                .map(|position| (position, field))
        })
        .collect::<Vec<_>>();
    positions.sort_unstable_by_key(|(position, _)| *position);
    positions.into_iter().map(|(_, field)| field).collect()
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("serde_json::Value serialization cannot fail")
}

fn add_output_format_instructions(
    prompt: &str,
    schema: Option<&Value>,
    rationale_first: bool,
) -> String {
    format!(
        "{prompt}\n\nYou *must* format your evaluation rating as a JSON object with the following fields (no markdown). Pay close attention to the field type of the evaluation rating (string, boolean, numeric, etc.), and ensure that it conforms to the instructions.\n\n{}",
        output_field_descriptions(schema, rationale_first)
    )
}

fn output_field_descriptions(schema: Option<&Value>, rationale_first: bool) -> String {
    let result_type = schema.map(format_type).unwrap_or_else(|| "str".to_string());
    let result = format!("- result ({result_type}): {RESULT_DESCRIPTION}");
    let rationale = format!("- rationale (str): {RATIONALE_DESCRIPTION}");
    if rationale_first {
        format!("{rationale}\n{result}")
    } else {
        format!("{result}\n{rationale}")
    }
}

fn format_type(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return format!(
            "Literal[{}]",
            values
                .iter()
                .map(|value| match value {
                    Value::String(value) => format!("'{value}'"),
                    value => value.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    match schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string")
    {
        "integer" => "int",
        "number" => "float",
        "boolean" => "bool",
        "object" => "<class 'dict'>",
        "array" => "<class 'list'>",
        _ => "str",
    }
    .to_string()
}

fn response_format(data: &Map<String, Value>, rationale_first: bool) -> Result<Value, EngineError> {
    let mut result = data
        .get("feedback_value_type")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| Map::from_iter([("type".to_string(), json!("string"))]));
    result.insert("description".to_string(), json!(RESULT_DESCRIPTION));
    let rationale = json!({
        "description": RATIONALE_DESCRIPTION,
        "title": "Rationale",
        "type": "string"
    });
    let mut properties = Map::new();
    let mut required = Vec::new();
    if rationale_first {
        properties.insert("rationale".to_string(), rationale.clone());
        properties.insert("result".to_string(), Value::Object(result));
        required.extend([json!("rationale"), json!("result")]);
    } else {
        properties.insert("result".to_string(), Value::Object(result));
        properties.insert("rationale".to_string(), rationale);
        required.extend([json!("result"), json!("rationale")]);
    }
    Ok(json!({
        "type": "json_schema",
        "json_schema": {
            "name": "ResponseFormat",
            "schema": {
                "properties": properties,
                "required": required,
                "title": "ResponseFormat",
                "type": "object",
                "additionalProperties": false
            },
            "strict": true
        }
    }))
}

fn default_response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "JudgeEvaluation",
            "schema": {
                "properties": {
                    "result": {"description": RESULT_DESCRIPTION, "title": "Result", "type": "string"},
                    "rationale": {"description": RATIONALE_DESCRIPTION, "title": "Rationale", "type": "string"}
                },
                "required": ["result", "rationale"],
                "title": "JudgeEvaluation",
                "type": "object",
                "additionalProperties": false
            },
            "strict": true
        }
    })
}

fn strip_markdown_code_blocks(response: &str) -> String {
    let cleaned = response.trim();
    if cleaned.starts_with("```") {
        let lines = cleaned.lines().collect::<Vec<_>>();
        let end = lines
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, line)| line.trim() == "```")
            .map(|(index, _)| index)
            .unwrap_or(lines.len());
        return lines[1..end].join("\n");
    }
    if let Some(start) = cleaned.to_ascii_lowercase().find("```json\n") {
        let body = &cleaned[start + 8..];
        if let Some(end) = body.find("\n```") {
            return body[..end].trim().to_string();
        }
    }
    cleaned.to_string()
}

fn required_str<'a>(
    data: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, EngineError> {
    data.get(field)
        .and_then(Value::as_str)
        .ok_or(EngineError::InvalidScorerField(field))
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use axum::Router;

    use super::*;
    use crate::store::TrackingClient;
    use crate::{JobKind, WorkerRequest, NATIVE_WORKER_PROTOCOL_VERSION};

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn attachment_executor(bytes: &'static [u8]) -> ScorerExecutor {
        let app = Router::new().route(
            "/ajax-api/3.0/mlflow/get-trace-artifact",
            get(move || async move { bytes }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let request = WorkerRequest {
            protocol_version: NATIVE_WORKER_PROTOCOL_VERSION,
            job_id: "job-image".to_string(),
            job_kind: JobKind::InvokeScorer,
            params: json!({}),
            workspace: Some("images".to_string()),
            subject: json!({}),
        };
        let client =
            TrackingClient::from_request_at(&request, Some(&format!("http://{address}"))).unwrap();
        ScorerExecutor::new().with_tracking_client(client)
    }

    fn trace_with_refs(refs: &[&str]) -> Value {
        json!({
            "info":{"trace_id":"tr-image"},
            "data":{"spans":[{
                "span_id":"span-image",
                "attributes":{"mlflow.spanOutputs":refs},
            }]}
        })
    }

    fn text(result: JudgeToolResult) -> String {
        let JudgeToolResult::Text(value) = result else {
            panic!("expected text tool result")
        };
        value
    }

    #[tokio::test]
    async fn get_span_image_selects_images_and_returns_exact_errors() {
        let _env = ENV_LOCK.lock().await;
        std::env::remove_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE");
        let executor = attachment_executor(b"image-bytes").await;
        let text_ref = "mlflow-attachment://00000000-0000-0000-0000-000000000001?content_type=text%2Fplain&trace_id=tr-image&size=2";
        let image_ref = "mlflow-attachment://00000000-0000-0000-0000-000000000002?content_type=image%2Fpng&trace_id=tr-image&size=11";
        let trace = trace_with_refs(&[text_ref, image_ref]);
        let result = get_span_image(&executor, &trace, &json!({"span_id":"span-image"})).await;
        let JudgeToolResult::Image(result) = result else {
            panic!("expected image")
        };
        assert_eq!(result.span_id, "span-image");
        assert_eq!(result.data_url, "data:image/png;base64,aW1hZ2UtYnl0ZXM=");

        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &trace,
                    &json!({"span_id":"span-image","attachment_index":0}),
                )
                .await,
            ),
            "Error: attachment in span 'span-image' of trace 'tr-image' is not an image (content_type='text/plain')"
        );
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &trace,
                    &json!({"span_id":"span-image","attachment_index":2}),
                )
                .await,
            ),
            "Error: attachment_index 2 is out of range for span 'span-image' of trace 'tr-image', which has 2 attachment reference(s)"
        );
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &json!({"info":{"trace_id":"tr-image"},"data":{"spans":[]}}),
                    &json!({"span_id":"span-image"}),
                )
                .await,
            ),
            "Error: trace 'tr-image' has no spans"
        );
        assert_eq!(
            text(get_span_image(&executor, &trace, &json!({"span_id":"missing"})).await,),
            "Error: span 'missing' not found in trace 'tr-image'"
        );
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &trace_with_refs(&[]),
                    &json!({"span_id":"span-image"}),
                )
                .await,
            ),
            "Error: no mlflow-attachment:// image reference found in span 'span-image' of trace 'tr-image'"
        );
        let malformed =
            trace_with_refs(&["mlflow-attachment://attachment-without-required-query-fields"]);
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &malformed,
                    &json!({"span_id":"span-image","attachment_index":0}),
                )
                .await,
            ),
            "Error: could not parse attachment reference in span 'span-image' of trace 'tr-image'"
        );
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &trace_with_refs(&[text_ref]),
                    &json!({"span_id":"span-image"}),
                )
                .await,
            ),
            "Error: no image attachment found in span 'span-image' of trace 'tr-image'"
        );
    }

    #[tokio::test]
    async fn get_span_image_enforces_advertised_and_downloaded_size_caps() {
        let _env = ENV_LOCK.lock().await;
        std::env::set_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE", "3");
        let executor = attachment_executor(b"four").await;
        let advertised = trace_with_refs(&[
            "mlflow-attachment://00000000-0000-0000-0000-000000000002?content_type=image%2Fpng&trace_id=tr-image&size=4",
        ]);
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &advertised,
                    &json!({"span_id":"span-image"}),
                )
                .await,
            ),
            "Error: image attachment in span 'span-image' of trace 'tr-image' is 4 bytes, exceeding the 3 byte limit"
        );
        let unadvertised = trace_with_refs(&[
            "mlflow-attachment://00000000-0000-0000-0000-000000000002?content_type=image%2Fpng&trace_id=tr-image",
        ]);
        assert_eq!(
            text(
                get_span_image(
                    &executor,
                    &unadvertised,
                    &json!({"span_id":"span-image"}),
                )
                .await,
            ),
            "Error: image attachment in span 'span-image' of trace 'tr-image' is 4 bytes, exceeding the 3 byte limit"
        );
        std::env::remove_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE");
    }

    #[tokio::test]
    async fn image_size_limit_defaults_and_only_positive_env_values_override() {
        let _env = ENV_LOCK.lock().await;
        std::env::remove_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE");
        assert_eq!(attachment_size_limit(), DEFAULT_MAX_IMAGE_BYTES);
        for ignored in ["0", "-1", "not-an-integer"] {
            std::env::set_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE", ignored);
            assert_eq!(attachment_size_limit(), DEFAULT_MAX_IMAGE_BYTES);
        }
        std::env::set_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE", "17");
        assert_eq!(attachment_size_limit(), 17);
        std::env::remove_var("MLFLOW_TRACE_MAX_ATTACHMENT_SIZE");
    }

    #[test]
    fn image_turn_tags_are_not_serialized_and_prune_with_tool_pairs() {
        let messages = vec![
            json!({"role":"system","content":"judge"}),
            json!({"role":"assistant","tool_calls":[
                {"id":"call-image","function":{"name":"get_span_image","arguments":"{}"}},
                {"id":"call-info","function":{"name":"get_trace_info","arguments":"{}"}},
            ]}),
            json!({"role":"tool","tool_call_id":"call-image","content":"image ack"}),
            json!({"role":"tool","tool_call_id":"call-info","content":"trace info"}),
            json!({
                "role":"user",
                "content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}],
                (IMAGE_TURN_TOOL_CALL_ID):"call-image",
            }),
            json!({"role":"user","content":"keep"}),
        ];
        let wire = provider_messages(&messages);
        assert!(wire[4].get(IMAGE_TURN_TOOL_CALL_ID).is_none());
        assert_eq!(wire[2]["role"], "tool");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[4]["role"], "user");

        let pruned = remove_oldest_tool_call_pair(&messages).unwrap();
        assert_eq!(
            pruned,
            vec![
                json!({"role":"system","content":"judge"}),
                json!({"role":"user","content":"keep"}),
            ]
        );
    }
}
