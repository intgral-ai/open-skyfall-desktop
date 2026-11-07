//! Shared wire codecs. Scheduling/retries and direct-chat history policy stay in the caller.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Value, json};

use super::super::agents::Protocol;
use super::go;

pub(in crate::agent_chat) const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_MAX_TOKENS: u64 = 8192;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WireProtocol {
    Chat,
    Responses,
    Messages,
    Gemini,
}

impl From<Protocol> for WireProtocol {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAi => Self::Chat,
            Protocol::Anthropic => Self::Messages,
        }
    }
}

/// Borrowed encoding inputs shared with the legacy direct transport adapter.
pub(in crate::agent_chat) struct RequestParts<'a> {
    pub wire: WireProtocol,
    pub model: &'a str,
    pub system: &'a str,
    pub history: &'a [Value],
    pub tools: &'a [Value],
    pub go: bool,
    pub signatures: &'a BTreeMap<String, String>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<&'a str>,
}

pub(in crate::agent_chat) fn request_body(parts: RequestParts<'_>) -> Value {
    let RequestParts { wire, model, system, history, tools, go, signatures, max_output_tokens, temperature, reasoning_effort } = parts;
    let mut body = match wire {
        WireProtocol::Chat => {
            let mut messages = Vec::with_capacity(history.len().saturating_add(1));
            messages.push(json!({"role": "system", "content": system}));
            messages.extend(history.iter().cloned());
            json!({"model": model, "messages": messages})
        }
        WireProtocol::Responses => json!({"model": model, "input": go::go_responses_input(system, history)}),
        WireProtocol::Messages => json!({
            "model": model, "max_tokens": max_output_tokens.map(u64::from).unwrap_or(ANTHROPIC_MAX_TOKENS),
            "system": system, "messages": if go { go::go_anthropic_messages(history) } else { history.to_vec() },
        }),
        WireProtocol::Gemini => json!({
            "contents": go::go_gemini_contents(history, signatures),
            "systemInstruction": {"parts": [{"text": system}]},
        }),
    };
    if !tools.is_empty() {
        body["tools"] = match wire {
            WireProtocol::Chat => Value::Array(tools.to_vec()),
            WireProtocol::Responses => Value::Array(go::go_responses_tools(tools)),
            WireProtocol::Messages if go => Value::Array(go::go_anthropic_tools(tools)),
            WireProtocol::Messages => Value::Array(tools.to_vec()),
            WireProtocol::Gemini => json!([{"functionDeclarations": go::go_gemini_tools(tools)}]),
        };
    }
    if let Some(tokens) = max_output_tokens {
        match wire {
            WireProtocol::Chat | WireProtocol::Messages => body["max_tokens"] = json!(tokens),
            WireProtocol::Responses => body["max_output_tokens"] = json!(tokens),
            WireProtocol::Gemini => body["generationConfig"] = json!({"maxOutputTokens": tokens}),
        }
    }
    if let Some(temperature) = temperature {
        if wire == WireProtocol::Gemini {
            if body.get("generationConfig").is_none() {
                body["generationConfig"] = json!({});
            }
            body["generationConfig"]["temperature"] = json!(temperature);
        } else {
            body["temperature"] = json!(temperature);
        }
    }
    if let Some(effort) = reasoning_effort {
        match wire {
            WireProtocol::Chat => body["reasoning_effort"] = json!(effort),
            WireProtocol::Responses => body["reasoning"] = json!({"effort": effort}),
            // Strict admission rejects these combinations; legacy direct chat omits them.
            WireProtocol::Messages | WireProtocol::Gemini => {}
        }
    }
    body
}

pub(in crate::agent_chat) fn endpoint(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.to_ascii_lowercase().ends_with("/v1") { format!("{base}/{path}") } else { format!("{base}/v1/{path}") }
}

pub(in crate::agent_chat) fn request_url(base: &str, model: &str, wire: WireProtocol, go: bool) -> String {
    if go {
        return go::go_url(model, wire);
    }
    match wire {
        WireProtocol::Messages => endpoint(base, "messages"),
        WireProtocol::Chat => format!("{base}/chat/completions"),
        WireProtocol::Responses => format!("{base}/responses"),
        WireProtocol::Gemini => format!("{base}/models/{model}:generateContent"),
    }
}

pub(in crate::agent_chat) fn request_headers(go: bool, wire: WireProtocol, key: Option<&str>, session: &str) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    if go {
        headers.push(("x-opencode-session".into(), session.into()));
    }
    if wire == WireProtocol::Messages {
        headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
    }
    if let Some(key) = key {
        headers.push(if wire == WireProtocol::Messages && !go {
            ("x-api-key".into(), key.into())
        } else {
            ("Authorization".into(), format!("Bearer {key}"))
        });
    }
    headers
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolArgumentError {
    NotObject,
    InvalidJson,
}

impl fmt::Display for ToolArgumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotObject => "the tool arguments must be a JSON object",
            Self::InvalidJson => "the tool arguments were not valid JSON; send the call again with a JSON object",
        })
    }
}

pub(in crate::agent_chat) fn tool_arguments(raw: Option<&Value>) -> Result<Value, ToolArgumentError> {
    let text = match raw {
        None | Some(Value::Null) => return Ok(json!({})),
        Some(v @ Value::Object(_)) => return Ok(v.clone()),
        Some(Value::String(s)) => s.trim(),
        Some(_) => return Err(ToolArgumentError::NotObject),
    };
    if text.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err(ToolArgumentError::NotObject),
        Err(_) => Err(ToolArgumentError::InvalidJson),
    }
}

#[derive(Debug)]
pub(in crate::agent_chat) struct Call {
    pub id: String,
    pub name: String,
    pub arguments: Result<Value, ToolArgumentError>,
}

/// Unclipped codec result. Only the direct caller applies its old history/display caps.
#[derive(Debug)]
pub(in crate::agent_chat) struct Reply {
    pub text: Vec<(bool, String)>,
    pub calls: Vec<Call>,
    pub assistant: Value,
    pub stop: &'static str,
}

// Bound the buffered decode before any clones, argument parsing or active-gate work.
pub(in crate::agent_chat) const MAX_RESPONSE_BYTES: usize = 16 << 20;
const MAX_RESPONSE_NODES: usize = 65_536;
const MAX_TEXT_BYTES: usize = 256 << 10;
const MAX_TOOL_BYTES: usize = 64 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent_chat) enum ResponseError {
    Malformed,
    Resource(&'static str, u32),
    Model,
}

impl ResponseError {
    pub(in crate::agent_chat) fn message(self) -> String {
        if let Self::Resource(resource, limit) = self {
            return format!("{} [{resource}: {limit}]", crate::i18n::tr("ui.host_ai_resource_limit"));
        }
        crate::i18n::tr(match self {
            Self::Malformed => "ui.host_ai_invalid_response",
            Self::Resource(_, _) => "ui.host_ai_resource_limit",
            Self::Model => "ui.host_ai_model_failed",
        })
        .to_string()
    }
}

pub(in crate::agent_chat) fn parse_response(bytes: &[u8]) -> Result<Value, ResponseError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ResponseError::Resource("response_bytes", MAX_RESPONSE_BYTES as u32));
    }
    let value = serde_json::from_slice(bytes).map_err(|_| ResponseError::Malformed)?;
    bounded_response_tree(&value)?;
    Ok(value)
}

fn bounded_response_tree(root: &Value) -> Result<(), ResponseError> {
    let mut pending = vec![(root, 0usize)];
    let mut count = 0usize;
    while let Some((value, depth)) = pending.pop() {
        count += 1;
        if depth > 64 {
            return Err(ResponseError::Resource("response_depth", 64));
        }
        if count > MAX_RESPONSE_NODES {
            return Err(ResponseError::Resource("response_complexity", MAX_RESPONSE_NODES as u32));
        }
        let children = match value {
            Value::Array(values) => values.len(),
            Value::Object(values) => values.len(),
            _ => 0,
        };
        if children.saturating_add(pending.len()).saturating_add(count) > MAX_RESPONSE_NODES {
            return Err(ResponseError::Resource("response_complexity", MAX_RESPONSE_NODES as u32));
        }
        match value {
            Value::String(text) if text.len() > MAX_TEXT_BYTES => return Err(ResponseError::Resource("text_bytes", MAX_TEXT_BYTES as u32)),
            Value::Array(values) => pending.extend(values.iter().map(|v| (v, depth + 1))),
            Value::Object(values) => {
                if values.keys().any(|key| key.len() > MAX_TEXT_BYTES) {
                    return Err(ResponseError::Resource("text_bytes", MAX_TEXT_BYTES as u32));
                }
                pending.extend(values.values().map(|v| (v, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Closed finish classification. Unknown values never acquire Stop semantics, even with calls.
pub(in crate::agent_chat) fn finish_reason(wire: WireProtocol, body: &Value) -> &'static str {
    match wire {
        WireProtocol::Chat => {
            if body.pointer("/choices/0/message/refusal").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()) {
                return "refusal";
            }
            match body.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
                Some("stop") => "stop",
                Some("tool_calls" | "function_call") => "tool_calls",
                Some("length" | "max_tokens") => "max_tokens",
                Some("refusal" | "content_filter") => "refusal",
                _ => "",
            }
        }
        WireProtocol::Messages => match body.get("stop_reason").and_then(Value::as_str) {
            Some("end_turn" | "stop_sequence") => "stop",
            Some("tool_use") => "tool_calls",
            Some("max_tokens" | "length") => "max_tokens",
            Some("refusal") => "refusal",
            _ => "",
        },
        WireProtocol::Responses => {
            let refusal = body.get("output").and_then(Value::as_array).is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| parts.iter().any(|part| part.get("type").and_then(Value::as_str) == Some("refusal")))
                })
            });
            if refusal {
                return "refusal";
            }
            match body.get("status").and_then(Value::as_str) {
                Some("completed") => {
                    if body
                        .get("output")
                        .and_then(Value::as_array)
                        .is_some_and(|items| items.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("function_call")))
                    {
                        "tool_calls"
                    } else {
                        "stop"
                    }
                }
                Some("incomplete") => match body.pointer("/incomplete_details/reason").and_then(Value::as_str) {
                    Some("max_output_tokens") => "max_tokens",
                    Some("content_filter") => "refusal",
                    _ => "",
                },
                _ => "",
            }
        }
        WireProtocol::Gemini => {
            let reason = body.pointer("/candidates/0/finishReason").or_else(|| body.pointer("/promptFeedback/blockReason")).and_then(Value::as_str);
            match reason {
                Some("STOP") => {
                    if body
                        .pointer("/candidates/0/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| parts.iter().any(|p| p.get("functionCall").is_some()))
                    {
                        "tool_calls"
                    } else {
                        "stop"
                    }
                }
                Some("MAX_TOKENS") => "max_tokens",
                Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" | "IMAGE_PROHIBITED_CONTENT") => {
                    "refusal"
                }
                _ => "",
            }
        }
    }
}

pub(in crate::agent_chat) fn usage_counts(wire: WireProtocol, body: &Value) -> Result<Option<[Option<u32>; 3]>, ResponseError> {
    let (key, fields) = match wire {
        WireProtocol::Chat => ("usage", ["prompt_tokens", "completion_tokens", "total_tokens"]),
        WireProtocol::Messages | WireProtocol::Responses => ("usage", ["input_tokens", "output_tokens", "total_tokens"]),
        WireProtocol::Gemini => ("usageMetadata", ["promptTokenCount", "candidatesTokenCount", "totalTokenCount"]),
    };
    let Some(usage) = body.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
    let usage = usage.as_object().ok_or(ResponseError::Malformed)?;
    let mut counts = [None; 3];
    for (count, field) in counts.iter_mut().zip(fields) {
        if let Some(value) = usage.get(field).filter(|v| !v.is_null()) {
            let value = value.as_u64().filter(|n| *n <= i32::MAX as u64).ok_or(ResponseError::Malformed)?;
            *count = Some(value as u32);
        }
    }
    Ok(Some(counts))
}

/// Shape validation shared by the actual direct caller and the prepared strict primitive.
/// Argument errors remain values in the legacy codec so its tool loop can answer Not run.
pub(in crate::agent_chat) fn validate_reply(wire: WireProtocol, body: &Value) -> Result<(), ResponseError> {
    bounded_response_tree(body)?;
    if !body.is_object() {
        return Err(ResponseError::Malformed);
    }
    if body.get("error").is_some_and(|v| !v.is_null())
        || (wire == WireProtocol::Responses && matches!(body.get("status").and_then(Value::as_str), Some("failed" | "cancelled")))
    {
        return Err(ResponseError::Model);
    }
    // JSON Pointer treats an object key "0" like an array index. Check the actual
    // containers and any supplied role before consulting finish metadata or decoding.
    let chat_message = if wire == WireProtocol::Chat {
        let message = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(Value::as_object)
            .and_then(|choice| choice.get("message"))
            .and_then(Value::as_object)
            .ok_or(ResponseError::Malformed)?;
        if message.get("role").is_some_and(|role| role.as_str() != Some("assistant")) {
            return Err(ResponseError::Malformed);
        }
        Some(message)
    } else {
        None
    };
    if wire == WireProtocol::Messages && body.get("role").is_some_and(|role| role.as_str() != Some("assistant")) {
        return Err(ResponseError::Malformed);
    }
    let gemini_content = if wire == WireProtocol::Gemini {
        let content = match body.get("candidates") {
            None => None,
            Some(candidates) => {
                let candidates = candidates.as_array().ok_or(ResponseError::Malformed)?;
                match candidates.first() {
                    None => None, // A blocked prompt may have no candidate.
                    Some(candidate) => candidate
                        .as_object()
                        .ok_or(ResponseError::Malformed)?
                        .get("content")
                        .map(|content| content.as_object().ok_or(ResponseError::Malformed))
                        .transpose()?,
                }
            }
        };
        if content.is_some_and(|content| content.get("role").is_some_and(|role| role.as_str() != Some("model"))) {
            return Err(ResponseError::Malformed);
        }
        content
    } else {
        None
    };
    usage_counts(wire, body)?;
    let finish_path = match wire {
        WireProtocol::Chat => "/choices/0/finish_reason",
        WireProtocol::Messages => "/stop_reason",
        WireProtocol::Responses => "/status",
        WireProtocol::Gemini => "/candidates/0/finishReason",
    };
    if body.pointer(finish_path).is_some_and(|v| !v.is_null() && !v.is_string()) {
        return Err(ResponseError::Malformed);
    }
    let reason = finish_reason(wire, body);
    let allows_empty = matches!(reason, "stop" | "max_tokens" | "refusal");
    let valid = match wire {
        WireProtocol::Chat => chat_message.is_some_and(|message| {
            let content = message.get("content").is_none_or(|v| v.is_null() || v.is_string());
            let calls = message.get("tool_calls").is_none_or(|v| {
                v.is_null()
                    || v.as_array().is_some_and(|calls| calls.iter().all(|c| c.is_object() && c.get("function").is_some_and(Value::is_object)))
            });
            let refusal = message.get("refusal").is_none_or(|v| v.is_null() || v.is_string());
            let payload = message.get("content").is_some_and(|v| v.is_string() || (v.is_null() && allows_empty))
                || message.get("tool_calls").and_then(Value::as_array).is_some_and(|calls| !calls.is_empty())
                || message.get("refusal").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
            content && calls && refusal && payload
        }),
        WireProtocol::Messages => body.get("content").and_then(Value::as_array).is_some_and(|blocks| {
            (!blocks.is_empty() || allows_empty)
                && blocks.iter().all(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => b.get("text").is_some_and(Value::is_string),
                    Some("thinking") => b.get("thinking").or_else(|| b.get("text")).is_some_and(Value::is_string),
                    Some("redacted_thinking") => b.get("data").is_some_and(Value::is_string),
                    Some("tool_use") => b.get("name").is_some_and(Value::is_string),
                    _ => false,
                })
        }),
        WireProtocol::Responses => body.get("output").and_then(Value::as_array).is_some_and(|items| {
            (!items.is_empty() || allows_empty)
                && items.iter().all(|item| match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        item.get("role").is_none_or(|v| v.as_str() == Some("assistant"))
                            && item.get("content").and_then(Value::as_array).is_some_and(|parts| {
                                (!parts.is_empty() || allows_empty)
                                    && parts.iter().all(|p| match p.get("type").and_then(Value::as_str) {
                                        Some("output_text") => p.get("text").is_some_and(Value::is_string),
                                        Some("refusal") => p.get("refusal").is_some_and(Value::is_string),
                                        _ => false,
                                    })
                            })
                    }
                    Some("function_call") => item.get("name").is_some_and(Value::is_string),
                    Some("reasoning") => item.get("summary").is_some_and(Value::is_array),
                    _ => false,
                })
        }),
        WireProtocol::Gemini => {
            if let Some(content) = gemini_content {
                content.get("parts").and_then(Value::as_array).is_some_and(|parts| {
                    (!parts.is_empty() || allows_empty)
                        && parts.iter().all(|p| match (p.get("text"), p.get("functionCall")) {
                            (Some(text), None) => text.is_string(),
                            (None, Some(call)) => call.is_object(),
                            _ => false, // Never let the decoder's text branch hide a declaration.
                        })
                })
            } else {
                matches!(reason, "refusal" | "max_tokens")
            }
        }
    };
    if !valid {
        return Err(ResponseError::Malformed);
    }
    Ok(())
}

/// Only recognized authentication shapes distinguish a 403 from an ordinary model refusal.
pub(in crate::agent_chat) fn authentication_error(status: u16, body: &Value) -> bool {
    status == 401
        || (status == 403
            && (body.pointer("/error/type").and_then(Value::as_str) == Some("authentication_error")
                || matches!(body.pointer("/error/code").and_then(Value::as_str), Some("invalid_api_key" | "authentication_error"))
                || body.pointer("/error/status").and_then(Value::as_str) == Some("UNAUTHENTICATED")))
}

fn stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens" | "length") => "max_tokens",
        Some("refusal" | "content_filter") => "refusal",
        _ => "",
    }
}

pub(in crate::agent_chat) fn decode_reply(wire: WireProtocol, go: bool, body: &Value) -> Reply {
    let mut reply = match wire {
        WireProtocol::Chat => openai_reply(body),
        WireProtocol::Responses => go::go_responses_reply(body),
        WireProtocol::Messages if go => go::go_messages_reply(body),
        WireProtocol::Messages => anthropic_reply(body),
        WireProtocol::Gemini => go::go_gemini_reply(body),
    };
    reply.stop = match finish_reason(wire, body) {
        "max_tokens" => "max_tokens",
        "refusal" => "refusal",
        _ => "", // Includes Unknown: preserve the old direct-chat display contract.
    };
    reply
}

fn sanitize_reasoning_details(details: Option<&Value>) -> Option<Value> {
    let items = details?.as_array()?;
    let clean = items
        .iter()
        .filter_map(|d| {
            let mut c = d.as_object()?.clone();
            c.remove("index");
            Some(Value::Object(c))
        })
        .collect();
    Some(Value::Array(clean))
}

fn openai_reply(body: &Value) -> Reply {
    let message = body.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
    let text = message.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()).map(|t| vec![(false, t.to_string())]).unwrap_or_default();
    let refused = message.get("refusal").and_then(Value::as_str).is_some_and(|r| !r.trim().is_empty());
    let stop = if refused { "refusal" } else { stop_reason(body.pointer("/choices/0/finish_reason").and_then(Value::as_str)) };
    let raw = message.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
    let calls = raw
        .iter()
        .enumerate()
        .map(|(i, c)| Call {
            id: c.get("id").and_then(Value::as_str).map_or_else(|| format!("call-{i}"), str::to_string),
            name: c.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string(),
            arguments: tool_arguments(c.pointer("/function/arguments")),
        })
        .collect();
    let mut assistant = json!({"role": "assistant", "content": message.get("content").cloned().unwrap_or(Value::Null)});
    for key in ["reasoning_content", "reasoning", "refusal"] {
        if let Some(v) = message.get(key)
            && !v.is_null()
        {
            assistant[key] = v.clone();
        }
    }
    if let Some(clean) = sanitize_reasoning_details(message.get("reasoning_details")) {
        assistant["reasoning_details"] = clean;
    }
    if !raw.is_empty() {
        assistant["tool_calls"] = Value::Array(raw);
    }
    Reply { text, calls, assistant, stop }
}

fn anthropic_reply(body: &Value) -> Reply {
    let content = body.get("content").cloned().unwrap_or(Value::Null);
    let stop = stop_reason(body.get("stop_reason").and_then(Value::as_str));
    let mut text = Vec::new();
    let mut calls = Vec::new();
    if let Some(blocks) = content.as_array() {
        for (i, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
                        text.push((false, t.to_string()));
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").or_else(|| block.get("text")).and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
                        text.push((true, t.to_string()));
                    }
                }
                Some("tool_use") => calls.push(Call {
                    id: block.get("id").and_then(Value::as_str).map_or_else(|| format!("toolu-{i}"), str::to_string),
                    name: block.get("name").and_then(Value::as_str).unwrap_or("").into(),
                    arguments: match block.get("input") {
                        None | Some(Value::Null) => Ok(json!({})),
                        Some(v @ Value::Object(_)) => Ok(v.clone()),
                        Some(_) => Err(ToolArgumentError::NotObject),
                    },
                }),
                _ => {}
            }
        }
    } else if let Some(t) = content.as_str().filter(|t| !t.trim().is_empty()) {
        text.push((false, t.into()));
    }
    Reply { text, calls, assistant: json!({"role": "assistant", "content": content}), stop }
}

// Both native callers use the same worker-owned current-key and physical-admission path.
pub(in crate::agent_chat) use invocation::{
    ProviderCall, ProviderReply, ProviderState, ProviderToolCall, capabilities, invoke_once, project_strict_reply, validate_options,
};

mod invocation {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use serde_json::Map;

    use super::super::connections::resolve_secret;
    use super::super::types::{FinishReason, Usage};
    use super::super::{AiError, CallControl, ConnectionBinding, HostAiDependencies, ModelCallBudget};
    use super::*;
    use crate::agent_chat::backend::HttpRequest;

    pub(crate) struct ProviderCall {
        pub wire: WireProtocol,
        pub system: String,
        pub history: Vec<Value>,
        pub tools: Vec<Value>,
        pub max_output_tokens: Option<u32>,
        pub temperature: Option<f64>,
        pub reasoning_effort: Option<String>,
        pub timeout_seconds: u64,
        pub http_per_connection: u32,
        pub http_per_host: u32,
        pub model_budget: Option<Arc<ModelCallBudget>>,
        pub private: ProviderState,
    }

    #[derive(Clone, Default)]
    pub(crate) struct ProviderState {
        pub routing_session_id: String,
        pub native_assistants: Vec<Value>,
        pub gemini_signatures: BTreeMap<String, String>,
        pub wire_to_provider_tool_ids: BTreeMap<String, String>,
    }

    pub(crate) struct ProviderToolCall {
        pub id: String,
        pub name: String,
        pub arguments: Result<Map<String, Value>, ToolArgumentError>,
    }

    pub(crate) struct ProviderReply {
        pub text: Vec<(bool, String)>,
        pub calls: Vec<ProviderToolCall>,
        pub assistant: Value,
        pub finish_reason: FinishReason,
        pub usage: Option<Usage>,
        pub private: ProviderState,
    }

    /// Strict HostAi projection, separate from legacy Not run/clipping policy. The primitive
    /// preserves argument classifications so a direct caller can still answer malformed proposals.
    pub(in crate::agent_chat) fn project_strict_reply(reply: ProviderReply) -> Result<ProviderReply, AiError> {
        if reply.calls.len() > 32 {
            return Err(AiError::exhausted("tool_calls", 32));
        }
        if reply.calls.iter().any(|call| call.arguments.is_err()) {
            return Err(AiError::malformed());
        }
        Ok(reply)
    }

    pub(in crate::agent_chat) fn capabilities(_binding: &ConnectionBinding) -> super::super::types::AiCapabilities {
        use super::super::types::{AiCapabilities, Delivery};
        AiCapabilities {
            model_generate: true,
            agent_run: false,
            tools: true,
            images: true,
            documents: false,
            structured_output: false,
            model_selection: true,
            usage: true,
            delivery: Delivery::Buffered,
        }
    }

    pub(in crate::agent_chat) fn validate_options(
        binding: &ConnectionBinding,
        options: &super::super::types::GenerateOptions,
    ) -> Result<(), AiError> {
        use super::super::types::Delivery;
        if options.delivery != Delivery::Buffered {
            return Err(AiError::unsupported("streaming"));
        }
        if options.output_schema.is_some() {
            return Err(AiError::unsupported("structuredOutput"));
        }
        if options.timeout_seconds == 0
            || options.timeout_seconds > 600
            || u64::from(options.timeout_seconds) > binding.configuration().request_timeout_s
        {
            return Err(AiError::invalid("timeout_seconds"));
        }
        let wire = if binding.protocol() == Protocol::OpenAi && go::is_opencode_go(&binding.configuration().base_url) {
            go::go_protocol(binding.model_id())
        } else {
            binding.protocol().into()
        };
        let effort = options.reasoning_effort.as_deref().or_else(|| {
            let default = binding.configuration().reasoning_effort.as_str();
            (!default.is_empty()).then_some(default)
        });
        validate_call_options(wire, options.max_output_tokens, options.temperature, effort)?;
        if effort.is_some_and(|e| !matches!(e, "none" | "low" | "medium" | "high")) {
            return Err(AiError::unsupported("reasoningEffort"));
        }
        Ok(())
    }

    fn validate_call_options(wire: WireProtocol, tokens: Option<u32>, temperature: Option<f64>, effort: Option<&str>) -> Result<(), AiError> {
        if tokens.is_some_and(|n| !(1..=131_072).contains(&n)) {
            return Err(AiError::invalid("max_output_tokens"));
        }
        if temperature.is_some_and(|t| !t.is_finite() || !(0.0..=2.0).contains(&t)) {
            return Err(AiError::invalid("temperature"));
        }
        if wire == WireProtocol::Messages && temperature.is_some_and(|t| t > 1.0) {
            return Err(AiError::invalid("temperature"));
        }
        if let Some(effort) = effort {
            if effort.len() > 64 || effort.chars().any(char::is_control) {
                return Err(AiError::invalid("reasoning_effort"));
            }
            // The shared primitive must preserve direct chat's encodable custom Chat effort.
            // Strict HostAi's subset is enforced separately by validate_options, before dispatch.
            if wire != WireProtocol::Chat {
                return Err(AiError::unsupported("reasoningEffort"));
            }
        }
        Ok(())
    }

    fn domain_error(error: ResponseError) -> AiError {
        match error {
            ResponseError::Malformed => AiError::malformed(),
            ResponseError::Resource(resource, limit) => AiError::exhausted(resource, limit),
            ResponseError::Model => AiError::model_failed(),
        }
    }

    fn bounded_calls(wire: WireProtocol, body: &Value, cut: bool) -> Result<Vec<Option<ToolArgumentError>>, AiError> {
        let (items, call_type) = match wire {
            WireProtocol::Chat => (body.pointer("/choices/0/message/tool_calls"), None),
            WireProtocol::Messages => (body.get("content"), Some("tool_use")),
            WireProtocol::Responses => (body.get("output"), Some("function_call")),
            WireProtocol::Gemini => (body.pointer("/candidates/0/content/parts"), None),
        };
        let items = items.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let is_call = |item: &&Value| match wire {
            WireProtocol::Gemini => item.get("functionCall").is_some(),
            _ => call_type.is_none_or(|tag| item.get("type").and_then(Value::as_str) == Some(tag)),
        };
        if items.iter().filter(is_call).count() > 128 {
            return Err(AiError::exhausted("provider_tool_calls", 128));
        }
        if cut {
            return Ok(Vec::new());
        }
        let mut errors = Vec::new();
        for item in items.iter().filter(is_call) {
            let (id, name, args) = match wire {
                WireProtocol::Chat => (item.get("id"), item.pointer("/function/name"), item.pointer("/function/arguments")),
                WireProtocol::Messages => (item.get("id"), item.get("name"), item.get("input")),
                WireProtocol::Responses => (item.get("call_id"), item.get("name"), item.get("arguments")),
                WireProtocol::Gemini => (None, item.pointer("/functionCall/name"), item.pointer("/functionCall/args")),
            };
            if wire != WireProtocol::Gemini
                && !id.and_then(Value::as_str).is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            {
                return Err(AiError::malformed());
            }
            if !name.and_then(Value::as_str).is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)) {
                return Err(AiError::malformed());
            }
            let Some(args) = args.filter(|v| !v.is_null()) else {
                errors.push(Some(ToolArgumentError::NotObject));
                continue;
            };
            let parsed;
            let args = if let Value::String(text) = args {
                if matches!(wire, WireProtocol::Messages | WireProtocol::Gemini) {
                    errors.push(Some(ToolArgumentError::NotObject));
                    continue;
                }
                if text.len() > MAX_TOOL_BYTES {
                    return Err(AiError::exhausted("tool_json_bytes", MAX_TOOL_BYTES as u32));
                }
                parsed = match serde_json::from_str::<Value>(text) {
                    Ok(value) => value,
                    Err(_) => {
                        errors.push(Some(ToolArgumentError::InvalidJson));
                        continue;
                    }
                };
                &parsed
            } else {
                args
            };
            if !args.is_object() {
                errors.push(Some(ToolArgumentError::NotObject));
                continue;
            }
            bounded_response_tree(args).map_err(domain_error)?;
            // Stop serialization at the cap, without allocating an arbitrarily large buffer.
            struct BoundedWriter(usize);
            impl std::io::Write for BoundedWriter {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    if bytes.len() > self.0 {
                        return Err(std::io::Error::other("limit"));
                    }
                    self.0 -= bytes.len();
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            serde_json::to_writer(BoundedWriter(MAX_TOOL_BYTES), args).map_err(|_| AiError::exhausted("tool_json_bytes", MAX_TOOL_BYTES as u32))?;
            errors.push(None);
        }
        Ok(errors)
    }

    pub(in crate::agent_chat) fn invoke_once(
        deps: &HostAiDependencies,
        binding: &ConnectionBinding,
        call: ProviderCall,
        control: &Arc<CallControl>,
        deadline: Duration,
    ) -> Result<ProviderReply, AiError> {
        control.check((deps.clock)(), deadline)?;
        deps.shared.connections().resolve(binding)?;
        if call.timeout_seconds == 0 {
            return Err(AiError::invalid("timeout_seconds"));
        }
        validate_call_options(call.wire, call.max_output_tokens, call.temperature, call.reasoning_effort.as_deref())?;
        let permit = deps.shared.http.try_acquire(binding.connection_id(), call.http_per_connection as usize, call.http_per_host as usize)?;
        let shared = Arc::clone(&deps.shared);
        let binding = binding.clone();
        let http = Arc::clone(&deps.http);
        let secrets = deps.secrets.clone();
        let clock = Arc::clone(&deps.clock);
        let worker_control = Arc::clone(control);
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("solidcraft-ai-http".into())
            .spawn(move || {
                let _physical_permit = permit;
                let result = (|| {
                    worker_control.check(clock(), deadline)?;
                    let connection = shared.connections().resolve(&binding)?;
                    let key = resolve_secret(&connection, secrets.as_deref())?;
                    // No catalog lock or call-control lock spans keychain/HTTP work.
                    worker_control.check(clock(), deadline)?;
                    let current = shared.connections().resolve(&binding)?;
                    let key = if current.configuration().key { key } else { None };
                    if current.configuration().key && key.is_none() {
                        return Err(AiError::auth_required());
                    }
                    let key_text = key.as_ref().map(|k| k.as_str());
                    let config = current.configuration();
                    let go = binding.protocol() == Protocol::OpenAi && go::is_opencode_go(&config.base_url);
                    let url = request_url(&config.base_url, binding.model_id(), call.wire, go);
                    if key_text.is_some() && !crate::agent_chat::agents::key_safe_for(&url) {
                        return Err(AiError::invalid("base_url"));
                    }
                    let body = request_body(RequestParts {
                        wire: call.wire,
                        model: binding.model_id(),
                        system: &call.system,
                        history: &call.history,
                        tools: &call.tools,
                        go,
                        signatures: &call.private.gemini_signatures,
                        max_output_tokens: call.max_output_tokens,
                        temperature: call.temperature,
                        reasoning_effort: call.reasoning_effort.as_deref(),
                    });
                    let mut req = HttpRequest {
                        method: "POST",
                        url,
                        headers: request_headers(go, call.wire, key_text, &call.private.routing_session_id),
                        body: Some(body.to_string().into_bytes()),
                        timeout_s: call.timeout_seconds,
                    };
                    worker_control.while_active(clock(), deadline, || {
                        // Last generation/auth-policy check is inside dispatch admission:
                        // no injected clock/keychain work intervenes before the debit.
                        let latest = shared.connections().resolve(&binding)?;
                        if !latest.configuration().key {
                            req.headers.retain(|(name, _)| name != "Authorization" && name != "x-api-key");
                        } else if key_text.is_none() {
                            return Err(AiError::auth_required());
                        }
                        if let Some(budget) = &call.model_budget {
                            budget.debit()?;
                        }
                        Ok(())
                    })?;
                    // Exactly one physical dispatch. Any failure after admission consumes the debit.
                    let response = http(&req);
                    worker_control.check(clock(), deadline)?;
                    let (status, bytes) =
                        response.map_err(|error| if error == "timeout: global" { AiError::timeout() } else { AiError::transport() })?;
                    if !(200..300).contains(&status) {
                        // An oversized/unparseable refusal is still a status failure; never echo it.
                        let error_body = parse_response(&bytes).unwrap_or(Value::Null);
                        let detail = error_body.pointer("/error/message").or_else(|| error_body.get("error")).and_then(Value::as_str).unwrap_or("");
                        let lower = detail.to_ascii_lowercase();
                        let reasoning = status == 400 && detail.contains("reasoning_effort");
                        let next_go = lower.contains("does not support this protocol") || (status == 401 && lower.contains("credential"));
                        return Err(AiError::provider_status(binding.connection_id(), status, authentication_error(status, &error_body))
                            .with_compatibility(reasoning, next_go));
                    }
                    let mut body = parse_response(&bytes).map_err(domain_error)?;
                    validate_reply(call.wire, &body).map_err(|e| {
                        if e == ResponseError::Model { AiError::provider_status(binding.connection_id(), status, false) } else { domain_error(e) }
                    })?;
                    if key_text.is_some_and(|key| contains_secret(&body, key) || state_contains_secret(&call.private, key)) {
                        return Err(AiError::malformed());
                    }
                    let reason = finish_reason(call.wire, &body);
                    let cut = matches!(reason, "max_tokens" | "refusal");
                    let argument_errors = bounded_calls(call.wire, &body, cut)?;
                    let usage = usage_counts(call.wire, &body).map_err(domain_error)?.map(|[input_tokens, output_tokens, total_tokens]| Usage {
                        input_tokens,
                        output_tokens,
                        total_tokens,
                    });
                    if cut {
                        // Discard non-executable declarations before entering the active gate:
                        // no argument JSON parsing/copying, Gemini IDs or signatures for them.
                        // Overall response limits and the native 128-call bound already ran.
                        match call.wire {
                            WireProtocol::Chat => {
                                if let Some(message) = body.pointer_mut("/choices/0/message").and_then(Value::as_object_mut) {
                                    message.remove("tool_calls");
                                }
                            }
                            WireProtocol::Messages => {
                                if let Some(blocks) = body.get_mut("content").and_then(Value::as_array_mut) {
                                    blocks.retain(|block| block.get("type").and_then(Value::as_str) != Some("tool_use"));
                                }
                            }
                            WireProtocol::Responses => {
                                if let Some(items) = body.get_mut("output").and_then(Value::as_array_mut) {
                                    items.retain(|item| item.get("type").and_then(Value::as_str) != Some("function_call"));
                                }
                            }
                            WireProtocol::Gemini => {
                                if let Some(parts) = body.pointer_mut("/candidates/0/content/parts").and_then(Value::as_array_mut) {
                                    parts.retain(|part| part.get("functionCall").is_none());
                                }
                            }
                        }
                    }
                    // This only builds a proposal. Publication/continuation storage belongs to the caller.
                    worker_control.while_active(clock(), deadline, || {
                        let mut decoded = decode_reply(call.wire, go, &body);
                        if decoded.text.iter().any(|(_, text)| text.len() > MAX_TEXT_BYTES) {
                            return Err(AiError::exhausted("text_bytes", MAX_TEXT_BYTES as u32));
                        }
                        // Argument JSON unescaping and text joining can create a credential
                        // that was not present as a contiguous string in the raw JSON body.
                        if key_text.is_some_and(|key| decoded_contains_secret(&decoded, key)) {
                            return Err(AiError::malformed());
                        }
                        for (call, error) in decoded.calls.iter_mut().zip(argument_errors) {
                            if let Some(error) = error {
                                call.arguments = Err(error);
                            }
                        }
                        // Keep native replay independently of the legacy Go adapter's
                        // OpenAI-shaped history. Never export these values to ModelReply.
                        let native_assistant = match call.wire {
                            WireProtocol::Chat => decoded.assistant.clone(), // already sanitized reasoning_details
                            WireProtocol::Messages => json!({"role":"assistant","content":body.get("content")}),
                            WireProtocol::Responses => body.get("output").cloned().unwrap_or_else(|| json!([])),
                            WireProtocol::Gemini => {
                                let mut native = body.pointer("/candidates/0/content").cloned().unwrap_or_else(|| json!({"role":"model","parts":[]}));
                                native["role"] = json!("model");
                                let parts = native.get_mut("parts").and_then(Value::as_array_mut).ok_or_else(AiError::malformed)?;
                                for (tool, part) in decoded.calls.iter_mut().zip(parts.iter_mut().filter(|p| p.get("functionCall").is_some())) {
                                    let id = match part.pointer("/functionCall/id") {
                                        Some(Value::String(id)) => id.clone(),
                                        None => super::super::continuation::random_id()?,
                                        _ => return Err(AiError::malformed()),
                                    };
                                    tool.id = id.clone();
                                    part["functionCall"]["id"] = json!(id);
                                }
                                decoded.text = parts
                                    .iter()
                                    .filter_map(|part| {
                                        part.get("text")
                                            .and_then(Value::as_str)
                                            .map(|text| (part.get("thought").and_then(Value::as_bool) == Some(true), text.to_owned()))
                                    })
                                    .collect();
                                // The legacy adapter still consumes assistant-shaped history.
                                // Keep its IDs consistent with the decoded declarations, while
                                // retaining complete native parts only in private state.
                                if let Some(calls) = decoded.assistant.get_mut("tool_calls").and_then(Value::as_array_mut) {
                                    for (legacy, tool) in calls.iter_mut().zip(&decoded.calls) {
                                        legacy["id"] = json!(tool.id);
                                    }
                                }
                                native
                            }
                        };
                        let mut private = call.private;
                        if call.wire == WireProtocol::Gemini && !decoded.calls.is_empty() {
                            for (call, signature) in decoded.calls.iter().zip(go::go_gemini_signatures(&body)) {
                                if let Some(signature) = signature {
                                    private.gemini_signatures.insert(call.id.clone(), signature);
                                }
                            }
                        }
                        private.native_assistants.push(native_assistant);
                        let finish_reason = match reason {
                            "stop" => FinishReason::Stop,
                            "tool_calls" => FinishReason::ToolCalls,
                            "max_tokens" => FinishReason::Length,
                            "refusal" => FinishReason::Refusal,
                            _ => FinishReason::Unknown,
                        };
                        Ok(ProviderReply {
                            text: decoded.text,
                            assistant: decoded.assistant,
                            finish_reason,
                            usage,
                            private,
                            calls: decoded
                                .calls
                                .into_iter()
                                .map(|c| ProviderToolCall {
                                    id: c.id,
                                    name: c.name,
                                    arguments: c.arguments.and_then(|v| match v {
                                        Value::Object(o) => Ok(o),
                                        _ => Err(ToolArgumentError::NotObject),
                                    }),
                                })
                                .collect(),
                        })
                    })
                })();
                let _ = tx.send(result);
            })
            .map_err(|_| AiError::internal("spawn_provider"))?;
        loop {
            control.check((deps.clock)(), deadline)?;
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => {
                    control.check((deps.clock)(), deadline)?;
                    if result.as_ref().err().is_some_and(|e| e.code() == super::super::AiErrorCode::Timeout) {
                        control.check(deadline, deadline)?;
                    }
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(AiError::internal("provider_worker")),
            }
        }
    }

    fn contains_secret(value: &Value, key: &str) -> bool {
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            match value {
                Value::String(text) if text.contains(key) => return true,
                Value::Array(values) => pending.extend(values),
                Value::Object(values) => {
                    if values.keys().any(|name| name.contains(key)) {
                        return true;
                    }
                    pending.extend(values.values());
                }
                _ => {}
            }
        }
        false
    }

    fn state_contains_secret(state: &ProviderState, key: &str) -> bool {
        state.routing_session_id.contains(key)
            || state.native_assistants.iter().any(|v| contains_secret(v, key))
            || state.gemini_signatures.iter().chain(state.wire_to_provider_tool_ids.iter()).any(|(k, v)| k.contains(key) || v.contains(key))
    }

    fn decoded_contains_secret(reply: &Reply, key: &str) -> bool {
        contains_secret(&reply.assistant, key)
            || reply.text.iter().any(|(_, text)| text.contains(key))
            || reply.calls.iter().any(|call| {
                call.id.contains(key) || call.name.contains(key) || call.arguments.as_ref().is_ok_and(|arguments| contains_secret(arguments, key))
            })
    }
}
