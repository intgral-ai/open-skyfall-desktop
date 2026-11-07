//! Go routing and wire conversions, without fallback or request scheduling.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use super::provider::{Call, Reply, ToolArgumentError, WireProtocol, tool_arguments};

static GO_GEMINI_CALL_COUNTER: AtomicU64 = AtomicU64::new(1);
const GO_BASE: &str = "https://opencode.ai/inference/go";
const GO_PROTOCOL_TABLE: &[(&[&str], WireProtocol)] = &[
    (&["gemini-"], WireProtocol::Gemini),
    (&["grok-", "gpt-", "muse-spark-"], WireProtocol::Responses),
    (&["minimax-m3.1-flash"], WireProtocol::Chat),
    (&["minimax-", "claude-", "qwen"], WireProtocol::Messages),
];

pub(in crate::agent_chat) fn is_opencode_go(base: &str) -> bool {
    let u = base.trim().to_ascii_lowercase();
    u.starts_with("https://opencode.ai/") && u.contains("/go")
}

pub(in crate::agent_chat) fn go_protocol(model: &str) -> WireProtocol {
    let m = model.trim().to_ascii_lowercase();
    for (prefixes, protocol) in GO_PROTOCOL_TABLE {
        if prefixes.iter().any(|p| m.starts_with(p)) {
            return *protocol;
        }
    }
    WireProtocol::Chat
}

pub(in crate::agent_chat) fn go_url(model: &str, protocol: WireProtocol) -> String {
    match protocol {
        WireProtocol::Gemini => format!("{GO_BASE}/google/v1beta/models/{}:generateContent", model.trim().to_ascii_lowercase()),
        WireProtocol::Responses => format!("{GO_BASE}/openai/v1/responses"),
        WireProtocol::Messages => format!("{GO_BASE}/anthropic/v1/messages"),
        WireProtocol::Chat => format!("{GO_BASE}/openai/v1/chat/completions"),
    }
}

fn go_fn_parts(t: &Value) -> Option<(&Value, &str, Value)> {
    let f = t.get("function")?;
    let params = f.get("parameters").cloned().unwrap_or(json!({"type": "object"}));
    Some((f.get("name")?, f.get("description").and_then(Value::as_str).unwrap_or(""), params))
}

fn go_shaped_tool(name: &Value, desc: &str, params: Value, params_key: &str, type_tag: Option<&str>) -> Value {
    let mut m = Map::new();
    if let Some(t) = type_tag {
        m.insert("type".into(), Value::String(t.into()));
    }
    m.insert("name".into(), name.clone());
    m.insert("description".into(), Value::String(desc.into()));
    m.insert(params_key.into(), params);
    Value::Object(m)
}

pub(in crate::agent_chat) fn go_anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            // Native normalized projections have already passed strict admission.
            if t.get("input_schema").is_some() && t.get("function").is_none() {
                return Some(t.clone());
            }
            let (name, desc, params) = go_fn_parts(t)?;
            Some(go_shaped_tool(name, desc, params, "input_schema", None))
        })
        .collect()
}

pub(in crate::agent_chat) fn go_responses_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            if t.get("parameters").is_some() && t.get("function").is_none() {
                return Some(t.clone());
            }
            let (name, desc, params) = go_fn_parts(t)?;
            Some(go_shaped_tool(name, desc, params, "parameters", Some("function")))
        })
        .collect()
}

/// Legacy direct-chat schema reduction. Normalized HostAi validation must reject
/// unsupported schema semantics before invoking this encoder (Task5).
pub(in crate::agent_chat) fn go_gemini_property(schema: &Value) -> Value {
    let Some(o) = schema.as_object() else { return json!({"type": "string"}) };
    let mut out = Map::new();
    out.insert("type".into(), Value::String(o.get("type").and_then(Value::as_str).unwrap_or("string").into()));
    if let Some(d) = o.get("description").and_then(Value::as_str) {
        out.insert("description".into(), json!(d));
    }
    if let Some(e) = o.get("enum").and_then(Value::as_array) {
        out.insert("enum".into(), Value::Array(e.clone()));
    }
    if let Some(items) = o.get("items") {
        out.insert("items".into(), go_gemini_property(items));
    }
    if let Some(props) = o.get("properties").and_then(Value::as_object) {
        out.insert("properties".into(), Value::Object(props.iter().map(|(k, v)| (k.clone(), go_gemini_property(v))).collect()));
    } else if out.get("type").and_then(Value::as_str) == Some("object") {
        out.insert("properties".into(), json!({}));
    }
    if let Some(req) = o.get("required").and_then(Value::as_array)
        && !req.is_empty()
        && req.iter().all(Value::is_string)
    {
        out.insert("required".into(), Value::Array(req.clone()));
    }
    Value::Object(out)
}

pub(in crate::agent_chat) fn go_gemini_schema(schema: &Value) -> Value {
    let mut root = go_gemini_property(schema);
    root["type"] = json!("object");
    if root.get("properties").is_none() {
        root["properties"] = json!({});
    }
    root
}

pub(in crate::agent_chat) fn go_gemini_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            if t.get("parameters").is_some() && t.get("function").is_none() {
                return Some(t.clone());
            }
            let (name, desc, params) = go_fn_parts(t)?;
            Some(json!({"name": name, "description": desc, "parameters": go_gemini_schema(&params)}))
        })
        .collect()
}

fn go_push_item(msgs: &mut Vec<Value>, role: &str, key: &str, item: Value) {
    if let Some(last) = msgs.last_mut() {
        let same = last.get("role").and_then(Value::as_str) == Some(role);
        if same && let Some(items) = last.get_mut(key).and_then(Value::as_array_mut) {
            items.push(item);
            return;
        }
    }
    let mut m = Map::new();
    m.insert("role".into(), json!(role));
    m.insert(key.into(), Value::Array(vec![item]));
    msgs.push(Value::Object(m));
}

fn go_push_block(msgs: &mut Vec<Value>, role: &str, block: Value) {
    go_push_item(msgs, role, "content", block);
}
fn go_push_part(msgs: &mut Vec<Value>, role: &str, part: Value) {
    go_push_item(msgs, role, "parts", part);
}

fn go_data_url(url: &str) -> Option<(&str, &str)> {
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    if mime.is_empty() || data.is_empty() { None } else { Some((mime, data)) }
}
fn openai_part_text(p: &Value) -> Option<&str> {
    p.get("text").and_then(Value::as_str)
}
fn openai_part_image_url(p: &Value) -> Option<&str> {
    p.pointer("/image_url/url").and_then(Value::as_str)
}
fn openai_tool_call(c: &Value) -> (&str, &str) {
    (c.get("id").and_then(Value::as_str).unwrap_or(""), c.pointer("/function/name").and_then(Value::as_str).unwrap_or(""))
}

pub(in crate::agent_chat) fn go_anthropic_messages(history: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in history {
        if let Some(role @ ("user" | "assistant")) = m.get("role").and_then(Value::as_str)
            && let Some(parts) = m.get("content").and_then(Value::as_array)
            && parts.iter().all(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("text" | "image" | "tool_use" | "tool_result" | "thinking" | "redacted_thinking")
                )
            })
        {
            for part in parts {
                go_push_block(&mut out, role, part.clone());
            }
            continue;
        }
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" => match m.get("content") {
                Some(Value::String(t)) => go_push_block(&mut out, "user", json!({"type":"text","text":t})),
                Some(Value::Array(parts)) => {
                    for p in parts {
                        if let Some(t) = openai_part_text(p) {
                            go_push_block(&mut out, "user", json!({"type":"text","text":t}));
                        } else if let Some(url) = openai_part_image_url(p)
                            && let Some((mime, data)) = go_data_url(url)
                        {
                            go_push_block(&mut out, "user", json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}));
                        }
                    }
                }
                _ => {}
            },
            "assistant" => {
                if let Some(t) = m.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    go_push_block(&mut out, "assistant", json!({"type":"text","text":t}));
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let (id, name) = openai_tool_call(c);
                        let input = tool_arguments(c.pointer("/function/arguments")).unwrap_or_else(|_| json!({}));
                        go_push_block(&mut out, "assistant", json!({"type":"tool_use","id":id,"name":name,"input":input}));
                    }
                }
            }
            "tool" => go_push_block(
                &mut out,
                "user",
                json!({"type":"tool_result","tool_use_id":m.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),"content":m.get("content").and_then(Value::as_str).unwrap_or("")}),
            ),
            _ => {}
        }
    }
    out
}

pub(in crate::agent_chat) fn go_responses_input(system: &str, history: &[Value]) -> Vec<Value> {
    let mut out = vec![json!({"type":"message","role":"system","content":[{"type":"input_text","text":system}]})];
    for m in history {
        if matches!(m.get("type").and_then(Value::as_str), Some("message" | "function_call" | "function_call_output" | "reasoning")) {
            out.push(m.clone());
            continue;
        }
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "system" => if let Some(t) = m.get("content").and_then(Value::as_str) {
                out.push(json!({"type":"message","role":"system","content":[{"type":"input_text","text":t}]}));
            },
            "user" => match m.get("content") {
                Some(Value::String(t)) => out.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":t}]})),
                Some(Value::Array(parts)) => {
                    let content: Vec<Value> = parts.iter().filter_map(|p| {
                        if let Some(t) = openai_part_text(p) { Some(json!({"type":"input_text","text":t})) }
                        else { openai_part_image_url(p).map(|url| json!({"type":"input_image","image_url":url})) }
                    }).collect();
                    if !content.is_empty() { out.push(json!({"type":"message","role":"user","content":content})); }
                }
                _ => {}
            },
            "assistant" => {
                if let Some(t) = m.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    out.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":t}]}));
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let (id,name) = openai_tool_call(c);
                        let args = match c.pointer("/function/arguments") { Some(Value::String(s)) if !s.trim().is_empty() => s.clone(), Some(v @ Value::Object(_)) => v.to_string(), _ => "{}".into() };
                        out.push(json!({"type":"function_call","call_id":id,"name":name,"arguments":args}));
                    }
                }
            }
            "tool" => out.push(json!({"type":"function_call_output","call_id":m.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),"output":m.get("content").and_then(Value::as_str).unwrap_or("")})),
            _ => {}
        }
    }
    out
}

fn go_call_names(history: &[Value]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for m in history {
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                if let (Some(id), Some(name)) = (c.get("id").and_then(Value::as_str), c.pointer("/function/name").and_then(Value::as_str)) {
                    names.insert(id.into(), name.into());
                }
            }
        }
    }
    names
}

pub(in crate::agent_chat) fn go_gemini_contents(history: &[Value], sigs: &BTreeMap<String, String>) -> Vec<Value> {
    let names = if history.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("tool")) { go_call_names(history) } else { HashMap::new() };
    let mut out = Vec::new();
    for m in history {
        if let Some(role @ ("user" | "model")) = m.get("role").and_then(Value::as_str)
            && let Some(parts) = m.get("parts").and_then(Value::as_array)
        {
            for part in parts {
                go_push_part(&mut out, role, part.clone());
            }
            continue;
        }
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" => match m.get("content") {
                Some(Value::String(t)) => go_push_part(&mut out, "user", json!({"text":t})),
                Some(Value::Array(parts)) => {
                    for p in parts {
                        if let Some(t) = openai_part_text(p) {
                            go_push_part(&mut out, "user", json!({"text":t}));
                        } else if let Some(url) = openai_part_image_url(p)
                            && let Some((mime, data)) = go_data_url(url)
                        {
                            go_push_part(&mut out, "user", json!({"inlineData":{"mimeType":mime,"data":data}}));
                        }
                    }
                }
                _ => {}
            },
            "assistant" => {
                if let Some(t) = m.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    go_push_part(&mut out, "model", json!({"text":t}));
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let (id, name) = openai_tool_call(c);
                        let args = tool_arguments(c.pointer("/function/arguments")).unwrap_or_else(|_| json!({}));
                        let call = json!({"name":name,"args":args});
                        go_push_part(
                            &mut out,
                            "model",
                            if let Some(sig) = sigs.get(id) {
                                json!({"functionCall":call,"thoughtSignature":sig})
                            } else {
                                json!({"functionCall":call})
                            },
                        );
                    }
                }
            }
            "tool" => {
                let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                let name = names.get(id).map(String::as_str).unwrap_or("tool");
                let text = m.get("content").and_then(Value::as_str).unwrap_or("");
                go_push_part(&mut out, "user", json!({"functionResponse":{"name":name,"response":{"output":text}}}));
            }
            _ => {}
        }
    }
    out
}

#[derive(Default)]
struct GoCallAccum {
    text: Vec<(bool, String)>,
    calls: Vec<Call>,
    kept_calls: Vec<Value>,
}
impl GoCallAccum {
    fn push_text(&mut self, text: &str) {
        self.text.push((false, text.into()));
    }
    fn push_call(&mut self, id: String, name: String, arguments: Result<Value, ToolArgumentError>, raw_args: String) {
        self.calls.push(Call { id: id.clone(), name: name.clone(), arguments });
        self.kept_calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":raw_args}}));
    }
    fn into_reply(self, wire: WireProtocol, body: &Value) -> Reply {
        let joined: String = self.text.iter().map(|(_, t)| t.as_str()).collect();
        let mut assistant = json!({"role":"assistant","content":if joined.is_empty() {Value::Null} else {Value::String(joined)}});
        if !self.kept_calls.is_empty() {
            assistant["tool_calls"] = Value::Array(self.kept_calls);
        }
        let stop = match super::provider::finish_reason(wire, body) {
            "max_tokens" => "max_tokens",
            "refusal" => "refusal",
            _ => "",
        };
        Reply { text: self.text, calls: self.calls, assistant, stop }
    }
}

pub(in crate::agent_chat) fn go_responses_reply(body: &Value) -> Reply {
    let mut acc = GoCallAccum::default();
    if let Some(items) = body.get("output").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        let joined: String = parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
                        if !joined.is_empty() {
                            acc.push_text(&joined);
                        }
                    }
                }
                Some("function_call") => {
                    let id = item.get("call_id").and_then(Value::as_str).map_or_else(|| format!("call-{}", acc.calls.len()), str::to_string);
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("").into();
                    let raw = match item.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => "{}".into(),
                    };
                    acc.push_call(id, name, tool_arguments(item.get("arguments")), raw);
                }
                _ => {}
            }
        }
    }
    acc.into_reply(WireProtocol::Responses, body)
}

pub(in crate::agent_chat) fn go_messages_reply(body: &Value) -> Reply {
    let mut acc = GoCallAccum::default();
    if let Some(blocks) = body.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
                        acc.push_text(t);
                    }
                }
                Some("tool_use") => {
                    let id = b.get("id").and_then(Value::as_str).map_or_else(|| format!("toolu-{}", acc.calls.len()), str::to_string);
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("").into();
                    let raw = b.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into());
                    acc.push_call(id, name, tool_arguments(b.get("input")), raw);
                }
                _ => {}
            }
        }
    }
    acc.into_reply(WireProtocol::Messages, body)
}

pub(in crate::agent_chat) fn go_gemini_reply(body: &Value) -> Reply {
    let mut acc = GoCallAccum::default();
    if let Some(parts) = body.pointer("/candidates/0/content/parts").and_then(Value::as_array) {
        for p in parts {
            if let Some(t) = p.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                acc.push_text(t);
            } else if let Some(call) = p.get("functionCall") {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("").into();
                let raw = call.get("args").map(Value::to_string).unwrap_or_else(|| "{}".into());
                let id = format!("go-{}", GO_GEMINI_CALL_COUNTER.fetch_add(1, Ordering::Relaxed));
                acc.push_call(id, name, tool_arguments(call.get("args")), raw);
            }
        }
    }
    acc.into_reply(WireProtocol::Gemini, body)
}

pub(in crate::agent_chat) fn go_gemini_signatures(body: &Value) -> Vec<Option<String>> {
    body.pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("functionCall").is_some())
                .map(|p| p.get("thoughtSignature").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
