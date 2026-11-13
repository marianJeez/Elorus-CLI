//! Anthropic's Messages API: the request our turns become, and its stream translated into the
//! OpenAI chunks the assembler already folds.

use serde_json::{Value, json};

use crate::api::chat::Turn;
use crate::api::wire::Chunk;
use crate::conversation::error::ErrorInfo;
use crate::conversation::message::{Message, Part};

pub const VERSION: &str = "2023-06-01";

/// The reminder wrapper elorus uses on every wire; the model trusts it as elorus's own.
const TAG: &str = "system-reminder";

/// A turn as a Messages request. Reasoning effort becomes a thinking budget, which is how
/// Anthropic spells it; assistant thinking is not replayed, since its signatures are not ours.
pub fn request(turn: &Turn<'_>) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for message in turn.messages {
        match message {
            Message::User { parts } => {
                let blocks: Vec<Value> = parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text } if !text.is_empty() => {
                            Some(json!({"type": "text", "text": text}))
                        }
                        Part::Reminder { text } => Some(json!({
                            "type": "text",
                            "text": format!("<{TAG}>\n{text}\n</{TAG}>"),
                        })),
                        _ => None,
                    })
                    .collect();
                push(&mut messages, "user", blocks);
            }
            Message::Tool(result) => push(
                &mut messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": result.content,
                    "is_error": result.is_error,
                })],
            ),
            Message::Assistant(reply) => {
                let blocks: Vec<Value> = reply
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text } if !text.is_empty() => {
                            Some(json!({"type": "text", "text": text}))
                        }
                        Part::ToolCall(call) => {
                            let input =
                                serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                            Some(json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": call.name,
                                "input": input,
                            }))
                        }
                        _ => None,
                    })
                    .collect();
                push(&mut messages, "assistant", blocks);
            }
        }
    }

    let tools: Vec<Value> = turn.tools.iter().filter_map(tool).collect();
    let output = match turn.model.max_output_tokens {
        0 => 8192,
        max => max,
    };
    let mut body = json!({
        "model": turn.model.wire_id(),
        "system": turn.system,
        "messages": messages,
        "max_tokens": output,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(budget) = budget(turn, output) {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
    body
}

/// Adjacent messages of one role are one turn on this wire, tool results included.
fn push(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last["role"] == role
        && let Some(content) = last["content"].as_array_mut()
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

/// An OpenAI function tool as Anthropic names it.
fn tool(tool: &Value) -> Option<Value> {
    let function = tool.get("function")?;
    Some(json!({
        "name": function.get("name")?.as_str()?,
        "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
        "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
    }))
}

/// The thinking budget effort asks for, kept under half of what the reply may hold.
fn budget(turn: &Turn<'_>, max_tokens: u64) -> Option<u64> {
    let wanted = match turn.effort()? {
        "low" => 1024,
        "high" => 8192,
        "max" => 16384,
        _ => 4096,
    };
    let room = max_tokens / 2;
    (room >= 1024).then(|| wanted.min(room))
}

/// Anthropic's stream, folded into chunks as they arrive.
#[derive(Default)]
pub struct Chunks {
    prompt_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
}

impl Chunks {
    pub fn apply(&mut self, data: &str) -> Result<Option<Chunk>, ErrorInfo> {
        let value: Value = serde_json::from_str(data)
            .map_err(|e| ErrorInfo::local("bad_stream", e.to_string()))?;
        match value["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let usage = &value["message"]["usage"];
                let input = usage["input_tokens"].as_u64().unwrap_or(0);
                let read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                let created = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.prompt_tokens = input + read + created;
                self.cached_tokens = read;
                self.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
                chunk(json!({"choices": [], "usage": self.usage()}))
            }
            "content_block_start" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let block = &value["content_block"];
                match block["type"].as_str().unwrap_or_default() {
                    "tool_use" => chunk(json!({"choices": [{"delta": {"tool_calls": [{
                        "index": index,
                        "id": block["id"],
                        "function": {"name": block["name"], "arguments": ""},
                    }]}}]})),
                    "text" if block["text"].as_str().is_some_and(|t| !t.is_empty()) => {
                        chunk(json!({"choices": [{"delta": {"content": block["text"]}}]}))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_delta" => {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let delta = &value["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => chunk(json!({"choices": [{"delta": {
                        "content": delta["text"],
                    }}]})),
                    "thinking_delta" => chunk(json!({"choices": [{"delta": {
                        "reasoning_content": delta["thinking"],
                    }}]})),
                    "input_json_delta" => chunk(json!({"choices": [{"delta": {"tool_calls": [{
                        "index": index,
                        "function": {"arguments": delta["partial_json"]},
                    }]}}]})),
                    _ => Ok(None),
                }
            }
            "message_delta" => {
                if let Some(output) = value["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = output;
                }
                let stop = stop(value["delta"]["stop_reason"].as_str().unwrap_or_default());
                chunk(
                    json!({"choices": [{"delta": {}, "finish_reason": stop}], "usage": self.usage()}),
                )
            }
            "error" => {
                let error = &value["error"];
                let info = ErrorInfo {
                    kind: error["type"]
                        .as_str()
                        .unwrap_or("upstream_error")
                        .to_owned(),
                    message: error["message"].as_str().unwrap_or_default().to_owned(),
                    status: None,
                    request_id: None,
                };
                Ok(Some(Chunk {
                    error: Some(info),
                    ..Chunk::default()
                }))
            }
            _ => Ok(None),
        }
    }

    fn usage(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.output_tokens,
            "prompt_tokens_details": {"cached_tokens": self.cached_tokens},
        })
    }
}

fn stop(reason: &str) -> &str {
    match reason {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        other => other,
    }
}

fn chunk(value: Value) -> Result<Option<Chunk>, ErrorInfo> {
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| ErrorInfo::local("bad_stream", e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Capabilities, Model, Pricing};
    use crate::conversation::message::{Assistant, Finish, ToolCall, ToolResult};

    fn model() -> Model {
        Model {
            id: "anthropic/m".into(),
            provider: "anthropic".into(),
            wire_id: Some("m".into()),
            display_name: String::new(),
            description: String::new(),
            context_window: 1000,
            max_output_tokens: 10000,
            pricing: Pricing::default(),
            capabilities: Capabilities {
                reasoning: true,
                ..Capabilities::default()
            },
        }
    }

    #[test]
    fn a_conversation_becomes_one_alternating_messages_list() {
        let model = model();
        let messages = vec![
            Message::User {
                parts: vec![Part::Text { text: "hi".into() }],
            },
            Message::Assistant(Assistant {
                model: "m".into(),
                ..Assistant::stub(
                    Finish::ToolCalls,
                    vec![
                        Part::Reasoning {
                            text: "think".into(),
                        },
                        Part::Text {
                            text: "reading".into(),
                        },
                        Part::ToolCall(ToolCall {
                            id: "c1".into(),
                            name: "read".into(),
                            arguments: r#"{"path":"a"}"#.into(),
                        }),
                    ],
                )
            }),
            Message::Tool(ToolResult {
                call_id: "c1".into(),
                name: "read".into(),
                content: "text".into(),
                is_error: false,
                details: None,
                asks: Vec::new(),
                refused: None,
                usage: None,
            }),
        ];
        let turn = Turn {
            model: &model,
            effort: Some("high"),
            system: "sys",
            messages: &messages,
            tools: &[],
        };
        let body = request(&turn);
        assert_eq!(body["model"], "m");
        assert_eq!(body["system"], "sys");
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["thinking"]["budget_tokens"], 5000);
    }

    #[test]
    fn tools_lose_the_openai_wrapper() {
        let spec = json!({"type": "function", "function": {
            "name": "read", "description": "reads", "parameters": {"type": "object"},
        }});
        let mapped = tool(&spec).unwrap();
        assert_eq!(mapped["name"], "read");
        assert_eq!(mapped["input_schema"]["type"], "object");
        assert!(mapped.get("function").is_none());
    }

    #[test]
    fn the_stream_becomes_text_reasoning_tool_calls_and_a_finish() {
        let mut chunks = Chunks::default();
        let mut seen = Vec::new();
        for line in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":4,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            if let Some(chunk) = chunks.apply(line).unwrap() {
                seen.push(chunk);
            }
        }
        assert_eq!(seen.len(), 7);
        assert_eq!(
            seen.last().unwrap().choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        let usage = seen.last().unwrap().usage.as_ref().unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (14, 9));
        assert_eq!(usage.cached(), 4);
    }

    #[test]
    fn an_error_event_keeps_its_type() {
        let chunk = Chunks::default()
            .apply(r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.error.unwrap().kind, "overloaded_error");
    }
}
