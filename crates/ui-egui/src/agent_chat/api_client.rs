//! The API backend (desktop only): a model behind an endpoint SolidCraft talks to itself, either
//! OpenAI-compatible (Ollama, LM Studio, OpenAI, any URL the user gives) or Anthropic-compatible
//! (the Anthropic Messages API, or an endpoint that speaks it). SolidCraft runs the tool loop
//! itself, with the same tools the ACP agents get: it starts the same MCP server (`solidcraft-cli
//! mcp --connect` to the chat's own endpoint) and is its client, so every tool call passes the
//! same gate, limits and folder checks. The key, when there is one, comes from the system keychain
//! for this connection only.
//!
//! The loop runs on a thread of its own: a model request can take a while.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use serde_json::{Value, json};

use super::MODEL_LIST_CAP;
use super::acp::ModelInfo;
use super::agents::Protocol;
use super::backend::{ChatBackend, Event, HttpFn, HttpRequest, PromptPart, StartContext, ToolStatus};
use super::host_ai::go::{go_protocol, is_opencode_go};
use super::host_ai::provider::{self, ANTHROPIC_VERSION, ProviderCall, ProviderReply, ProviderState, Reply, WireProtocol as GoProtocol, endpoint};
use super::host_ai::{AiError, AiErrorCode, AiErrorDetails, ConnectionBinding, FinishReason, HostAi, TurnControl};
use super::process::{AgentProcess, FromAgent, ProcessHandle};

/// Longest tool result handed back to the model.
const MAX_TOOL_TEXT: usize = 20_000;
/// Longest name kept for a listed model.
const MODEL_NAME_CAP: usize = 256;
/// Messages kept in the conversation (the system note stays).
const MAX_HISTORY: usize = 240;
/// Pictures of the view kept in the conversation. The whole history goes with every request
/// (up to `MAX_REQUESTS_PER_TURN` per message), so older views are swapped for a short note:
/// otherwise each attached view makes every later request bigger, until the endpoint refuses it.
const MAX_HISTORY_IMAGES: usize = 2;
/// What an older view becomes in the history.
const DROPPED_VIEW_NOTE: &str = "[an earlier picture of the view, removed to keep the request small]";
const DROPPED_THINKING_NOTE: &str = "[earlier reasoning, removed with older history]";
/// How long one tool call may take (it may wait for the user's Allow, declined after four
/// minutes). It does not depend on the endpoint's request timeout: a tool call is SolidCraft's own
/// work and the user's answer, never a wait for the model, so a longer request timeout does not
/// need a longer tool timeout.
const TOOL_TIMEOUT: Duration = Duration::from_secs(330);
/// How often a waiting request or tool call looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(40);
/// The connection test's own timeout: it asks for a token or two, not a whole answer.
const TEST_TIMEOUT_S: u64 = 60;

/// Tools the model is not offered.
const HIDDEN_TOOLS: &[&str] = &["crash_reports"];

/// Chat counter for OpenCode Go session IDs, next to the wall clock below.
static GO_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A stable session ID for OpenCode Go's routing and prompt cache; one per chat,
/// not per request. Unique per chat on this machine; the gateway only needs an
/// opaque stable string.
fn new_session_id() -> String {
    let n = GO_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("solidcraft-{t:x}-{n:x}-{:x}", std::process::id())
}

pub struct ApiConfig {
    pub connection_id: String,
    pub model: String,
    pub host_ai: Arc<HostAi>,
}

enum Cmd {
    Prompt(Vec<PromptPart>, Arc<TurnControl>),
    NewChat,
    SetModel(String),
    SetInstructions(String),
}

pub struct ApiBackend {
    cmds: Option<Sender<Cmd>>,
    events: Receiver<(Option<Arc<TurnControl>>, Event)>,
    turn: Arc<TurnControl>,
    mcp: Option<ProcessHandle>,
}

impl ApiBackend {
    pub fn start(cfg: ApiConfig, ctx: StartContext) -> Result<ApiBackend, String> {
        let binding = cfg.host_ai.bind(&cfg.connection_id, &cfg.model).map_err(direct_error)?;
        let protocol = binding.protocol();
        // One stable session ID per chat for OpenCode Go's routing and prompt cache.
        let session_id = new_session_id();
        // The same MCP server an ACP agent would get.
        let mcp = match &ctx.bridge {
            Some(b) => {
                let argv = vec![b.cli.to_string_lossy().into_owned(), "mcp".into(), "--connect".into(), format!("127.0.0.1:{}", b.port)];
                Some(AgentProcess::spawn(&argv, &ctx.cwd, &["SOLIDCRAFT_CONTROL_TOKEN"], || {})?)
            }
            None => None,
        };
        let handle = mcp.as_ref().map(AgentProcess::handle);
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (ev_tx, events) = channel();
        let turn = Arc::new(TurnControl::new());
        let parent = Arc::clone(&turn);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::from(ctx.wake);
        std::thread::spawn(move || {
            let mut w = Worker {
                cfg,
                binding,
                protocol,
                mcp,
                next: 0,
                tools: Vec::new(),
                tool_names: Vec::new(),
                history: Vec::new(),
                note: super::acp::SYSTEM_NOTE.to_string(),
                session_id,
                gemini_sigs: Default::default(),
                events: ev_tx,
                wake,
                turn: parent,
                in_turn: false,
                no_reasoning: Default::default(),
            };
            w.run(cmd_rx);
        });
        Ok(ApiBackend { cmds: Some(cmd_tx), events, turn, mcp: handle })
    }
}

impl ChatBackend for ApiBackend {
    fn poll(&mut self) -> Vec<Event> {
        self.events
            .try_iter()
            .take(512)
            .filter_map(|(scope, event)| {
                if let Some(scope) = scope {
                    if !Arc::ptr_eq(&scope, &self.turn) {
                        return None;
                    }
                    if scope.check().is_err() && !matches!(event, Event::TurnEnded { .. }) {
                        return None;
                    }
                }
                Some(event)
            })
            .collect()
    }
    fn prompt(&mut self, parts: &[PromptPart]) {
        // The old parent is irrevocably revoked before the new one becomes observable.
        self.turn.cancel();
        self.turn = Arc::new(TurnControl::new());
        if let Some(c) = &self.cmds {
            let _ = c.send(Cmd::Prompt(parts.to_vec(), Arc::clone(&self.turn)));
        }
    }
    fn cancel(&mut self) {
        self.turn.cancel();
    }
    fn set_model(&mut self, model: &str) {
        if let Some(c) = &self.cmds {
            let _ = c.send(Cmd::SetModel(model.to_string()));
        }
    }
    fn set_instructions(&mut self, text: &str) {
        if let Some(c) = &self.cmds {
            let _ = c.send(Cmd::SetInstructions(text.to_string()));
        }
    }
    fn new_session(&mut self) {
        // A running turn is cancelled; the worker resets the history once it has ended.
        self.turn.cancel();
        if let Some(c) = &self.cmds {
            let _ = c.send(Cmd::NewChat);
        }
    }
    fn answer_permission(&mut self, _key: u64, _option: Option<&str>) {}
    fn authenticate(&mut self, _method: &str) {}
    fn pid(&self) -> Option<u32> {
        self.mcp.as_ref().and_then(ProcessHandle::pid)
    }
    fn shutdown(&mut self, wait: bool) {
        self.turn.cancel();
        self.cmds = None;
        if let Some(h) = self.mcp.take() {
            h.shutdown(wait);
        }
    }
}

impl Drop for ApiBackend {
    fn drop(&mut self) {
        self.shutdown(false);
    }
}

struct Worker {
    cfg: ApiConfig,
    binding: ConnectionBinding,
    protocol: Protocol,
    mcp: Option<AgentProcess>,
    next: u64,
    /// The MCP tools in the endpoint's own shape (function tools, or `input_schema` tools).
    tools: Vec<Value>,
    /// The tools' names, whichever shape `tools` has.
    tool_names: Vec<String>,
    /// The conversation in the endpoint's shape, without the system note (sent separately).
    history: Vec<Value>,
    /// The system note every request carries ([`super::acp::system_note`]).
    note: String,
    /// OpenCode Go's stable session ID for this chat (one per chat, not per request).
    session_id: String,
    /// Thought signatures of Gemini function-call parts, by kept call id, for the
    /// next tool request. Stays outside the history so other protocols never see it.
    gemini_sigs: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
    events: Sender<(Option<Arc<TurnControl>>, Event)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    turn: Arc<TurnControl>,
    in_turn: bool,
    /// Set once the endpoint refused tools together with a reasoning effort: later requests ask for none.
    no_reasoning: std::cell::Cell<bool>,
}

impl Worker {
    fn emit(&self, e: Event) {
        let _ = self.events.send((self.in_turn.then(|| Arc::clone(&self.turn)), e));
        (self.wake)();
    }

    fn run(&mut self, cmds: Receiver<Cmd>) {
        if let Err(e) = self.start_mcp() {
            self.emit(Event::Error(e));
        }
        self.emit(Event::ToolsAvailable(!self.tools.is_empty()));
        self.emit(Event::Ready { agent: format!("{} · {}", self.binding.configuration().name, self.cfg.model), images: true });
        // The endpoint's models for the picker. An endpoint that does not list them still shows the
        // configured model, and says why there are no others.
        let configured = ModelInfo { id: self.cfg.model.clone(), name: self.cfg.model.clone(), description: String::new() };
        let available = match self.model_list() {
            Ok(mut list) => {
                self.emit(Event::ModelsNote(None));
                if !self.cfg.model.is_empty() && !list.iter().any(|m| m.id == self.cfg.model) {
                    list.insert(0, configured);
                }
                list
            }
            Err(e) => {
                self.emit(Event::ModelsNote(Some(e)));
                if self.cfg.model.is_empty() { Vec::new() } else { vec![configured] }
            }
        };
        if !available.is_empty() {
            self.emit(Event::Models { current: self.cfg.model.clone(), available });
        }
        for cmd in cmds {
            match cmd {
                Cmd::Prompt(parts, parent) => {
                    self.turn = parent;
                    self.in_turn = true;
                    self.turn(parts);
                    self.in_turn = false;
                }
                Cmd::SetModel(m) => self.cfg.model = m,
                Cmd::SetInstructions(text) => self.note = super::acp::system_note(&text),
                Cmd::NewChat => {
                    self.history.clear();
                    // A new conversation gets a fresh Go session ID for routing and caching.
                    self.session_id = new_session_id();
                    self.gemini_sigs.borrow_mut().clear();
                    self.emit(Event::Ready { agent: format!("{} · {}", self.binding.configuration().name, self.cfg.model), images: true });
                }
            }
        }
        if let Some(mut m) = self.mcp.take() {
            m.shutdown(false);
        }
    }

    /// The endpoint's models, in the shape its protocol lists them.
    fn model_list(&self) -> Result<Vec<ModelInfo>, String> {
        self.cfg.host_ai.list_models(&self.binding).map_err(direct_error)
    }

    /// One MCP request to the server; its `result`.
    fn mcp_call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let Some(m) = self.mcp.as_ref() else { return Err("no SolidCraft tools in this session".into()) };
        self.next += 1;
        let id = self.next;
        m.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = std::time::Instant::now() + TOOL_TIMEOUT;
        loop {
            // Wait in short slices so Stop and Send now do not wait for a slow tool: its late
            // answer is skipped later (its id no longer matches).
            if self.turn.check().is_err() {
                return Err("cancelled by the user".into());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now()).min(CANCEL_POLL);
            match m.from_agent.recv_timeout(left) {
                Ok(FromAgent::Line(l)) => {
                    let Ok(v) = serde_json::from_str::<Value>(l.trim()) else { continue };
                    if v.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(e) = v.get("error") {
                        return Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string());
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Ok(FromAgent::Exited(_)) => return Err("the SolidCraft tools stopped".into()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
                Err(_) => return Err("the SolidCraft tools did not answer".into()),
            }
        }
    }

    fn start_mcp(&mut self) -> Result<(), String> {
        if self.mcp.is_none() {
            return Ok(());
        }
        self.mcp_call(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "solidcraft-agent-chat", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        if let Some(m) = &self.mcp {
            m.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        }
        let list = self.mcp_call("tools/list", json!({}))?;
        self.tools = match self.protocol {
            Protocol::OpenAi => openai_tools(&list),
            Protocol::Anthropic => anthropic_tools(&list),
        };
        self.tool_names = tool_names(&list);
        Ok(())
    }

    fn turn(&mut self, parts: Vec<PromptPart>) {
        let binding = match self.cfg.host_ai.bind(&self.cfg.connection_id, &self.cfg.model) {
            Ok(binding) if binding.protocol() == self.protocol => binding,
            Ok(_) => {
                self.emit(Event::Error(crate::i18n::tr("ui.host_ai_reconnect_required").into()));
                self.emit(Event::TurnEnded { reason: "error".into() });
                return;
            }
            Err(error) => {
                self.emit(Event::Error(direct_error(error)));
                self.emit(Event::TurnEnded { reason: "error".into() });
                return;
            }
        };
        if self.binding.configuration().reasoning_effort != binding.configuration().reasoning_effort {
            self.no_reasoning.set(false);
        }
        self.binding = binding;
        self.history.push(user_message(self.protocol, &parts));
        let dropped = drop_old_views(&mut self.history, MAX_HISTORY_IMAGES);
        let trimmed = self.trim_history();
        if (dropped || trimmed) && self.protocol == Protocol::Anthropic {
            drop_thinking(&mut self.history);
        }
        // No cap on requests: Stop sets `cancel`, checked at the top of every round.
        loop {
            // The history as of here is complete (every tool call answered): a cancelled request
            // goes back to it, so no half-finished round is left to confuse the next message.
            let sound = self.history.len();
            if self.turn.check().is_err() {
                self.emit(Event::TurnEnded { reason: "cancelled".into() });
                return;
            }
            let reply = match self.request() {
                Ok(_) if self.turn.check().is_err() => {
                    self.history.truncate(sound);
                    self.emit(Event::TurnEnded { reason: "cancelled".into() });
                    return;
                }
                Ok(r) => r,
                Err(_) if self.turn.check().is_err() => {
                    self.history.truncate(sound);
                    self.emit(Event::TurnEnded { reason: "cancelled".into() });
                    return;
                }
                Err(e) => {
                    self.emit(Event::Error(e));
                    self.emit(Event::TurnEnded { reason: "error".into() });
                    return;
                }
            };
            if reply.calls.is_empty() {
                self.emit(Event::TurnEnded { reason: reply.stop.into() });
                return;
            }
            // Anthropic takes every result of a round in one user message; OpenAI takes one
            // `tool` message per call.
            let mut results = Vec::new();
            for c in reply.calls.iter().take(32) {
                if self.turn.check().is_err() {
                    break;
                }
                let args = c.arguments.clone().unwrap_or_else(|_| json!({}));
                let title = tool_title(&c.name, &args);
                let tool_id = format!("{}-{}", c.id, self.next);
                self.emit(Event::Tool {
                    id: tool_id.clone(),
                    title: Some(title),
                    kind: Some(String::new()),
                    status: Some(ToolStatus::InProgress),
                    input: c.arguments.as_ref().ok().filter(|a| a.is_object()).cloned(),
                    output: None,
                });
                let (text, failed) = if self.turn.check().is_err() {
                    ("cancelled by the user".to_string(), true)
                } else if let Err(e) = &c.arguments {
                    // A call whose arguments do not parse is answered, never run with `{}`.
                    (format!("Not run: {e}"), true)
                } else if !self.tool_names.iter().any(|n| n == &c.name) {
                    (format!("unknown tool `{}`", c.name), true)
                } else {
                    match self.mcp_call("tools/call", json!({"name": c.name, "arguments": args})) {
                        Ok(r) => tool_text(&r),
                        Err(e) => (e, true),
                    }
                };
                if self.turn.check().is_err() {
                    break;
                }
                let shown: String = text.chars().take(2000).collect();
                self.emit(Event::Tool {
                    id: tool_id,
                    title: None,
                    kind: None,
                    status: Some(if failed { ToolStatus::Failed } else { ToolStatus::Completed }),
                    input: None,
                    output: Some(shown),
                });
                match self.protocol {
                    Protocol::OpenAi => self.history.push(json!({"role": "tool", "tool_call_id": c.id, "content": text})),
                    Protocol::Anthropic => {
                        results.push(json!({"type": "tool_result", "tool_use_id": c.id, "content": text, "is_error": failed}));
                    }
                }
            }
            if self.turn.check().is_err() {
                self.history.truncate(sound);
                self.emit(Event::TurnEnded { reason: "cancelled".into() });
                return;
            }
            if self.protocol == Protocol::Anthropic && !results.is_empty() {
                self.history.push(json!({"role": "user", "content": results}));
            }
        }
    }

    /// Keep the conversation bounded and starting at a user message, so neither protocol sees a
    /// reply or a tool result without the message it answers. True when a message was removed.
    fn trim_history(&mut self) -> bool {
        let before = self.history.len();
        while self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        while self.history.first().is_some_and(|m| !starts_a_turn(m)) {
            self.history.remove(0);
        }
        self.history.len() != before
    }

    /// The reasoning effort the user chose for this endpoint, or `None` to send nothing and let
    /// the provider apply its own default.
    fn reasoning_effort(&self) -> Option<&str> {
        let effort = self.binding.configuration().reasoning_effort.trim();
        (!effort.is_empty()).then_some(effort)
    }

    fn request(&mut self) -> Result<Reply, String> {
        let go = self.protocol == Protocol::OpenAi && is_opencode_go(&self.binding.configuration().base_url);
        let first = if go { go_protocol(self.binding.model_id()) } else { self.protocol.into() };
        let mut order = vec![first];
        if go {
            order.extend([GoProtocol::Chat, GoProtocol::Responses, GoProtocol::Messages, GoProtocol::Gemini].into_iter().filter(|p| *p != first));
        }
        let mut first_error = None;
        for wire in order {
            let mut result = self.attempt(wire);
            if wire == GoProtocol::Chat
                && !self.tools.is_empty()
                && !self.no_reasoning.get()
                && self.reasoning_effort().is_none()
                && result.as_ref().err().is_some_and(AiError::retry_reasoning_none)
            {
                self.no_reasoning.set(true);
                result = self.attempt(wire);
            }
            match result {
                Ok(reply) => return Ok(reply),
                Err(error) => {
                    let retry = go && error.retry_go_protocol();
                    let error = if error.code() == AiErrorCode::Timeout { timed_out(self.limit_s()) } else { direct_error(error) };
                    if first_error.is_none() {
                        first_error = Some(error.clone());
                    }
                    if !retry {
                        return Err(error);
                    }
                }
            }
        }
        Err(first_error.unwrap_or_else(|| crate::i18n::tr("ui.ai_no_answer").into()))
    }

    /// One fresh child per physical attempt, including explicit compatibility attempts.
    fn attempt(&mut self, wire: GoProtocol) -> Result<Reply, AiError> {
        let control = self.turn.new_call()?;
        let timeout = self.limit_s();
        let deadline = self.cfg.host_ai.deadline_after(Duration::from_secs(timeout));
        let effort = if wire == GoProtocol::Chat {
            self.reasoning_effort().or_else(|| (!self.tools.is_empty() && self.no_reasoning.get()).then_some("none")).map(str::to_owned)
        } else {
            None
        };
        let call = ProviderCall {
            wire,
            system: self.note.clone(),
            history: self.history.clone(),
            tools: self.tools.clone(),
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: effort,
            timeout_seconds: timeout,
            http_per_connection: 2,
            http_per_host: 8,
            model_budget: None,
            private: ProviderState {
                routing_session_id: self.session_id.clone(),
                gemini_signatures: self.gemini_sigs.borrow().clone(),
                ..Default::default()
            },
        };
        let proposal = match self.cfg.host_ai.invoke_once(&self.binding, call, &control, deadline) {
            Ok(proposal) => proposal,
            Err(error) => {
                control.cancel();
                return Err(error);
            }
        };
        let result = control.publish(|| {
            self.cfg.host_ai.check_binding(&self.binding)?;
            let (reply, private) = direct_reply(proposal);
            *self.gemini_sigs.borrow_mut() = private.gemini_signatures;
            self.history.push(reply.assistant.clone());
            for (thought, text) in &reply.text {
                // Internal queue only: the external wake callback is below, outside the gate.
                let _ = self.events.send((self.in_turn.then(|| Arc::clone(&self.turn)), Event::Text { thought: *thought, text: text.clone() }));
            }
            Ok(reply)
        });
        (self.wake)();
        result
    }

    /// A non-chat Go answer as the tool loop's Reply.
    #[cfg(test)]
    fn go_reply(protocol: GoProtocol, body: &Value) -> Reply {
        decoded_direct_reply(protocol, true, body)
    }
}

impl Worker {
    /// The endpoint's request timeout, in range.
    fn limit_s(&self) -> u64 {
        self.binding.configuration().request_timeout_s.clamp(super::agents::MIN_REQUEST_TIMEOUT_S, super::agents::MAX_REQUEST_TIMEOUT_S)
    }
}

/// What the chat says when a request ran out of time: the limit, and where to raise it.
pub fn timed_out(limit_s: u64) -> String {
    crate::i18n::format("ui.ai_request_timed_out", &[("seconds", &limit_s.to_string())])
}

/// A failed request as people read it: one that ran into the endpoint's request timeout (the
/// host says so, or it failed at the limit) says the limit and where to raise it.
pub fn request_error(error: String, elapsed: Duration, limit_s: u64) -> String {
    if error == "timeout: global" || elapsed >= Duration::from_secs(limit_s) {
        timed_out(limit_s)
    } else {
        crate::i18n::tr("ui.host_ai_transport_failed").to_string()
    }
}

fn direct_error(error: AiError) -> String {
    match error.details() {
        AiErrorDetails::Connection { http_status: Some(status), .. } => format!("HTTP {status}: {}", error.message()),
        _ => error.message(),
    }
}

/// An endpoint's refusal as people read it.
pub fn http_error(status: u16, body: &Value) -> String {
    let what = match status {
        _ if provider::authentication_error(status, body) => crate::i18n::tr("ui.ai_http_key"),
        404 => crate::i18n::tr("ui.ai_http_not_found"),
        429 => crate::i18n::tr("ui.ai_http_busy"),
        413 => crate::i18n::tr("ui.ai_http_too_large"),
        _ => crate::i18n::tr("ui.host_ai_model_failed"),
    };
    format!("HTTP {status}: {what}")
}

/// The message for what the user typed (and the view, as the endpoint's own content blocks).
fn user_message(protocol: Protocol, parts: &[PromptPart]) -> Value {
    let images = parts.iter().any(|p| matches!(p, PromptPart::Image { .. }));
    if !images {
        let text: Vec<&str> = parts.iter().filter_map(|p| if let PromptPart::Text(t) = p { Some(t.as_str()) } else { None }).collect();
        return json!({"role": "user", "content": text.join("\n")});
    }
    let content: Vec<Value> = parts
        .iter()
        .filter_map(|p| match (protocol, p) {
            (_, PromptPart::Text(t)) => Some(json!({"type": "text", "text": t})),
            (Protocol::OpenAi, PromptPart::Image { mime, base64 }) => {
                Some(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{base64}")}}))
            }
            (Protocol::Anthropic, PromptPart::Image { mime, base64 }) => {
                Some(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": base64}}))
            }
            // These endpoints take no documents (the chat doesn't send any); the message names the file.
            (_, PromptPart::Document { .. }) => None,
        })
        .collect();
    json!({"role": "user", "content": content})
}

/// Whether a history message starts a turn: a user message that is not only tool results.
fn starts_a_turn(message: &Value) -> bool {
    message["role"] == "user" && !message["content"].as_array().is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
}

/// Direct history policy after single-attempt decoding: every kept call gets a result.
fn direct_reply(proposal: ProviderReply) -> (Reply, ProviderState) {
    let ProviderReply { text, mut calls, mut assistant, finish_reason, private, .. } = proposal;
    let cap = if matches!(finish_reason, FinishReason::Length | FinishReason::Refusal) { 0 } else { 32 };
    calls.truncate(cap);
    if cap == 0 {
        if let Some(message) = assistant.as_object_mut() {
            message.remove("tool_calls");
        }
    } else if let Some(native) = assistant.get_mut("tool_calls").and_then(Value::as_array_mut) {
        native.truncate(cap);
    }
    if let Some(blocks) = assistant.get_mut("content").and_then(Value::as_array_mut) {
        blocks.truncate(128);
        let mut count = 0;
        blocks.retain(|b| {
            if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                return true;
            }
            count += 1;
            count <= cap
        });
    }
    (
        Reply {
            text,
            assistant,
            calls: calls.into_iter().map(|c| provider::Call { id: c.id, name: c.name, arguments: c.arguments.map(Value::Object) }).collect(),
            stop: match finish_reason {
                FinishReason::Length => "max_tokens",
                FinishReason::Refusal => "refusal",
                _ => "",
            },
        },
        private,
    )
}

/// Existing codec tests use the same production direct projection, not a retired transport.
#[cfg(test)]
fn decoded_direct_reply(wire: GoProtocol, go: bool, body: &Value) -> Reply {
    let decoded = provider::decode_reply(wire, go, body);
    let finish_reason = match provider::finish_reason(wire, body) {
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::Refusal,
        "tool_calls" => FinishReason::ToolCalls,
        "stop" => FinishReason::Stop,
        _ => FinishReason::Unknown,
    };
    direct_reply(ProviderReply {
        text: decoded.text,
        assistant: decoded.assistant,
        finish_reason,
        usage: None,
        private: ProviderState::default(),
        calls: decoded
            .calls
            .into_iter()
            .map(|c| provider::ProviderToolCall {
                id: c.id,
                name: c.name,
                arguments: c.arguments.and_then(|v| v.as_object().cloned().ok_or(provider::ToolArgumentError::NotObject)),
            })
            .collect(),
    })
    .0
}

#[cfg(test)]
fn reply(protocol: Protocol, body: &Value) -> Reply {
    decoded_direct_reply(protocol.into(), false, body)
}

#[cfg(test)]
fn openai_reply(body: &Value) -> Reply {
    decoded_direct_reply(GoProtocol::Chat, false, body)
}

#[cfg(test)]
fn anthropic_reply(body: &Value) -> Reply {
    decoded_direct_reply(GoProtocol::Messages, false, body)
}

/// Keeps the newest `keep` pictures of the view in the history and swaps every older one for a
/// short note, so the model still knows a view was shown. Text is never touched. Pictures come
/// as OpenAI `image_url` parts or Anthropic `image` blocks, depending on the endpoint. True when
/// a picture was swapped.
fn drop_old_views(history: &mut [Value], keep: usize) -> bool {
    let mut seen = 0;
    let mut dropped = false;
    for msg in history.iter_mut().rev() {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else { continue };
        for part in content.iter_mut().rev() {
            if !matches!(part.get("type").and_then(Value::as_str), Some("image_url" | "image")) {
                continue;
            }
            seen += 1;
            if seen > keep {
                *part = json!({"type": "text", "text": DROPPED_VIEW_NOTE});
                dropped = true;
            }
        }
    }
    dropped
}

/// Takes Anthropic thinking blocks out of every earlier reply. Each one is signed against all
/// the conversation before it, so once a dropped view or trimmed message changes that, the API
/// refuses the whole request (#9417); leaving earlier thinking out is allowed. A reply that was
/// only thinking keeps a note, as the API takes no assistant message without content.
fn drop_thinking(history: &mut [Value]) {
    for msg in history.iter_mut().filter(|m| m["role"] == "assistant") {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else { continue };
        let before = content.len();
        content.retain(|b| !matches!(b.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking")));
        if content.is_empty() && before > 0 {
            content.push(json!({"type": "text", "text": DROPPED_THINKING_NOTE}));
        }
    }
}

/// MCP `tools/list` as OpenAI function tools (the hidden ones left out).
pub fn openai_tools(list: &Value) -> Vec<Value> {
    list.get("tools")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(128)
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?;
                    if HIDDEN_TOOLS.contains(&name) {
                        return None;
                    }
                    let params = t.get("inputSchema").cloned().filter(Value::is_object).unwrap_or(json!({"type": "object", "properties": {}}));
                    Some(json!({"type": "function", "function": {
                        "name": name,
                        "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": params,
                    }}))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// MCP `tools/list` as Anthropic tools (the hidden ones left out).
pub fn anthropic_tools(list: &Value) -> Vec<Value> {
    list.get("tools")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(128)
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?;
                    if HIDDEN_TOOLS.contains(&name) {
                        return None;
                    }
                    let schema = t.get("inputSchema").cloned().filter(Value::is_object).unwrap_or(json!({"type": "object", "properties": {}}));
                    Some(json!({
                        "name": name,
                        "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                        "input_schema": schema,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The offered tools' names, whichever shape the endpoint takes.
fn tool_names(list: &Value) -> Vec<String> {
    list.get("tools")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(128)
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?;
                    (!HIDDEN_TOOLS.contains(&name)).then(|| name.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Headers for an Anthropic-compatible request: the version, and the key when there is one (which
/// only goes over HTTPS or to this computer).
fn anthropic_headers(base_url: &str, key: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let mut headers = vec![("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string())];
    if let Some(k) = key {
        if !super::agents::key_safe_for(base_url) {
            return Err(crate::i18n::tr("ui.ai_key_needs_https").to_string());
        }
        headers.push(("x-api-key".to_string(), k.to_string()));
    }
    Ok(headers)
}

/// The endpoint's models for the picker. The Messages API has no models endpoint at every
/// compatible provider (Qwen's has none), so a failure here leaves only the configured model.
pub fn anthropic_model_list(http: &HttpFn, base_url: &str, key: Option<&str>) -> Result<Vec<ModelInfo>, String> {
    let req = HttpRequest { method: "GET", url: endpoint(base_url, "models"), headers: anthropic_headers(base_url, key)?, body: None, timeout_s: 15 };
    let (status, bytes) = http(&req).map_err(|e| request_error(e, Duration::ZERO, req.timeout_s))?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status != 200 {
        return Err(http_error(status, &v));
    }
    Ok(listed_models(&v, "display_name")?
        .into_iter()
        .map(|(id, name)| ModelInfo { name: name.unwrap_or_else(|| id.clone()), id, description: String::new() })
        .collect())
}

/// Test an Anthropic-compatible endpoint: send the smallest message there is, so the check covers
/// the path, the key and the model even where there is no models endpoint to ask.
pub fn test_anthropic_endpoint(http: &HttpFn, base_url: &str, key: Option<&str>, model: &str) -> Result<String, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err(crate::i18n::tr("ui.ai_need_model").to_string());
    }
    let mut headers = anthropic_headers(base_url, key)?;
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    let body = json!({"model": model, "max_tokens": 16, "messages": [{"role": "user", "content": "Hello"}]});
    let req = HttpRequest {
        method: "POST",
        url: endpoint(base_url, "messages"),
        headers,
        body: Some(body.to_string().into_bytes()),
        timeout_s: TEST_TIMEOUT_S,
    };
    let (status, bytes) = http(&req).map_err(|e| request_error(e, Duration::ZERO, req.timeout_s))?;
    let v = if status == 200 {
        provider::parse_response(&bytes).map_err(provider::ResponseError::message)?
    } else {
        provider::parse_response(&bytes).unwrap_or(Value::Null)
    };
    if status != 200 {
        return Err(http_error(status, &v));
    }
    provider::validate_reply(GoProtocol::Messages, &v).map_err(provider::ResponseError::message)?;
    Ok(crate::i18n::format("ui.ai_test_answered", &[("model", model)]))
}

/// A tool call's result as text for the model, and whether it failed.
fn tool_text(r: &Value) -> (String, bool) {
    let failed = r.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let parts: Vec<String> = r
        .get("content")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(16)
                .map(|c| match c.get("type").and_then(Value::as_str) {
                    Some("text") => c.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                    Some("image") => "[a picture of the view]".to_string(),
                    _ => String::new(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut text = parts.join("\n");
    if text.len() > MAX_TOOL_TEXT {
        let mut cut = MAX_TOOL_TEXT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n…(cut)");
    }
    (text, failed)
}

/// What a tool call card says: the tool and, for `execute` and `batch`, the commands.
fn tool_title(name: &str, args: &Value) -> String {
    match name {
        "execute" => format!("execute: {}", args.get("command").and_then(Value::as_str).unwrap_or("?")),
        "batch" => {
            let ids: Vec<&str> = args
                .get("commands")
                .and_then(Value::as_array)
                .map(|c| c.iter().filter_map(|s| s.get("command").and_then(Value::as_str)).take(4).collect())
                .unwrap_or_default();
            format!("batch: {}", ids.join(", "))
        }
        _ => name.to_string(),
    }
}

/// The ids of a `/models` body and the names the endpoint gives them (when not just the id), in
/// its order. The body is the endpoint's to write: an id that could not be sent back is left out,
/// and a name is cut. A body without a `data` array is no model list (a web page where the
/// endpoint should be, say): an error, so the picker says so and asks again later.
fn listed_models(body: &Value, name_key: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let Some(entries) = body.get("data").and_then(Value::as_array) else {
        return Err(crate::i18n::tr("ui.agent_chat_models_not_a_list").to_string());
    };
    Ok(entries
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str).filter(|id| super::model_id_valid(id))?;
            let name = m.get(name_key).and_then(Value::as_str).filter(|name| *name != id).map(|name| name.chars().take(MODEL_NAME_CAP).collect());
            Some((id.to_string(), name))
        })
        .take(MODEL_LIST_CAP)
        .collect())
}

/// `GET {base_url}/models`: the models, in the order the endpoint lists them. The picker shows the
/// ids, so an endpoint's own name for a model is its description.
fn model_entries(http: &HttpFn, base_url: &str, headers: Vec<(String, String)>) -> Result<Vec<ModelInfo>, String> {
    let req = HttpRequest { method: "GET", url: format!("{base_url}/models"), headers, body: None, timeout_s: 15 };
    let (status, bytes) = http(&req).map_err(|e| request_error(e, Duration::ZERO, req.timeout_s))?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status != 200 {
        return Err(http_error(status, &v));
    }
    Ok(listed_models(&v, "name")?.into_iter().map(|(id, name)| ModelInfo { name: id.clone(), id, description: name.unwrap_or_default() }).collect())
}

/// The endpoint's models for the picker.
pub fn model_list(http: &HttpFn, base_url: &str, key: Option<&str>) -> Result<Vec<ModelInfo>, String> {
    let mut headers = Vec::new();
    if let Some(k) = key {
        if !super::agents::key_safe_for(base_url) {
            return Err(crate::i18n::tr("ui.ai_key_needs_https").to_string());
        }
        headers.push(("Authorization".to_string(), format!("Bearer {k}")));
    }
    model_entries(http, base_url, headers)
}

/// Ask an endpoint for its models (Test connection): how many, and whether `model` is one.
pub fn test_endpoint(http: &HttpFn, base_url: &str, key: Option<&str>, model: &str) -> Result<String, String> {
    let mut headers = Vec::new();
    if let Some(k) = key {
        if !super::agents::key_safe_for(base_url) {
            return Err(crate::i18n::tr("ui.ai_key_needs_https").to_string());
        }
        headers.push(("Authorization".to_string(), format!("Bearer {k}")));
    }
    let models = model_entries(http, base_url, headers)?;
    let found = model.is_empty() || models.iter().any(|m| m.id == model);
    Ok(crate::i18n::format(if found { "ui.ai_test_ok" } else { "ui.ai_test_no_model" }, &[("count", &models.len().to_string()), ("model", model)]))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, mpsc},
        thread,
    };

    use super::super::agents::{AgentConfig, Kind, Protocol};
    use super::super::backend::Secrets;
    use super::super::host_ai::go::{
        go_anthropic_messages, go_gemini_contents, go_gemini_reply, go_gemini_schema, go_gemini_signatures, go_gemini_tools, go_messages_reply,
        go_responses_input, go_responses_reply, go_url,
    };
    use super::super::host_ai::provider::tool_arguments;
    use super::super::host_ai::provider::{ProviderCall, ProviderReply, ProviderState, WireProtocol, invoke_once};
    use super::super::host_ai::{AiError, ConnectionBinding, HostAiDependencies, ModelCallBudget};
    use super::super::host_ai::{AiErrorCode, AiLimits, AiShared, CallControl, ClockFn, HttpAdmission, TurnControl, test_support as connection_test};
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn direct_config(protocol: Protocol, base: &str, model: &str, key: Option<&str>, http: HttpFn, effort: &str) -> ApiConfig {
        let shared = Arc::new(AiShared::default());
        let config = AgentConfig {
            id: "api-original-7".into(),
            name: "Test".into(),
            kind: protocol.kind(),
            base_url: base.into(),
            model: model.into(),
            key: key.is_some(),
            request_timeout_s: 300,
            reasoning_effort: effort.into(),
            ..Default::default()
        };
        shared.connections().replace(&[config]).unwrap();
        let secrets = Arc::new(CurrentSecrets::new());
        *secrets.value.lock().unwrap() = Ok(key.map(str::to_owned));
        ApiConfig { connection_id: "api-original-7".into(), model: model.into(), host_ai: HostAi::direct(shared, Some(http), Some(secrets)).unwrap() }
    }

    const KEY_A: &str = "task2-key-A-long-private-sentinel-never-echo-7b83b230";
    const KEY_B: &str = "task2-key-B-long-private-sentinel-never-echo-6a97ca42";

    fn original_connection() -> AgentConfig {
        AgentConfig {
            id: "api-original-7".into(),
            name: "Original endpoint".into(),
            kind: Kind::Api,
            base_url: "https://models.example/v1".into(),
            model: "picker-default".into(),
            key: true,
            reasoning_effort: "high".into(),
            ..Default::default()
        }
    }

    const RACE_GUARD: Duration = Duration::from_secs(10);

    // Only HTTP/keychain/time are injected; every request traverses the real primitive.
    struct ScriptedHttp {
        recorded: Arc<Mutex<Vec<HttpRequest>>>,
        replies: Mutex<std::collections::VecDeque<(u16, Value)>>,
    }

    impl ScriptedHttp {
        fn new(replies: Vec<(u16, Value)>) -> Arc<Self> {
            Arc::new(Self { recorded: Arc::default(), replies: Mutex::new(replies.into()) })
        }

        fn http(self: &Arc<Self>) -> HttpFn {
            let script = Arc::clone(self);
            Arc::new(move |req| {
                script.recorded.lock().unwrap().push(req.clone());
                let (status, value) = script.replies.lock().unwrap().pop_front().expect("unexpected physical HTTP attempt");
                Ok((status, value.to_string().into_bytes()))
            })
        }
    }

    fn primitive_deps(http: HttpFn) -> HostAiDependencies {
        let shared = Arc::new(AiShared::default());
        shared.connections().replace(&[original_connection()]).unwrap();
        HostAiDependencies {
            shared,
            http,
            secrets: Some(Arc::new(CurrentSecrets::new())),
            artifacts: Arc::new(|_, _| Err(ModelCallBudget::new(0).err().unwrap())),
            clock: Arc::new(|| Duration::ZERO),
            ids: Arc::new(|| Err(ModelCallBudget::new(0).err().unwrap())),
        }
    }

    fn primitive_call(wire: WireProtocol) -> ProviderCall {
        ProviderCall {
            wire,
            system: "system note".into(),
            history: vec![json!({"role": "user", "content": "hello"})],
            tools: vec![json!({"type": "function", "function": {"name": "execute", "parameters": {"type": "object"}}})],
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            timeout_seconds: 60,
            http_per_connection: 2,
            http_per_host: 8,
            model_budget: None,
            private: ProviderState { routing_session_id: "stable-route-7".into(), ..Default::default() },
        }
    }

    fn chat_answer() -> Value {
        json!({"choices": [{"message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}]})
    }

    // These fixtures are literal provider shapes, not generated by the codecs under test.
    fn strict_answer(wire: WireProtocol) -> Value {
        match wire {
            WireProtocol::Chat => chat_answer(),
            WireProtocol::Messages => json!({"content": [{"type": "text", "text": "answer"}], "stop_reason": "end_turn"}),
            WireProtocol::Responses => {
                json!({"output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}], "status": "completed"})
            }
            WireProtocol::Gemini => json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "answer"}]}, "finishReason": "STOP"}]}),
        }
    }

    fn strict_call_answer(wire: WireProtocol, count: usize, arguments: Value) -> Value {
        let calls: Vec<Value> = (0..count)
            .map(|i| match wire {
                WireProtocol::Chat => json!({"id": format!("c{i}"), "type": "function", "function": {"name": "measure", "arguments": arguments}}),
                WireProtocol::Messages => json!({"type": "tool_use", "id": format!("c{i}"), "name": "measure", "input": arguments}),
                WireProtocol::Responses => json!({"type": "function_call", "call_id": format!("c{i}"), "name": "measure", "arguments": arguments}),
                WireProtocol::Gemini => json!({"functionCall": {"name": "measure", "args": arguments}}),
            })
            .collect();
        match wire {
            WireProtocol::Chat => {
                json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": calls}, "finish_reason": "tool_calls"}]})
            }
            WireProtocol::Messages => json!({"content": calls, "stop_reason": "tool_use"}),
            WireProtocol::Responses => json!({"output": calls, "status": "completed"}),
            WireProtocol::Gemini => json!({"candidates": [{"content": {"parts": calls}, "finishReason": "STOP"}]}),
        }
    }

    fn strict_invoke_bytes(wire: WireProtocol, bytes: Vec<u8>) -> Result<ProviderReply, AiError> {
        let deps = primitive_deps(Arc::new(move |_| Ok((200, bytes.clone()))));
        let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
        settled_invoke(&deps, &binding, primitive_call(wire), &Arc::new(CallControl::new()), Duration::from_secs(60))
            .and_then(provider::project_strict_reply)
    }

    fn strict_invoke(wire: WireProtocol, body: Value) -> Result<ProviderReply, AiError> {
        strict_invoke_bytes(wire, body.to_string().into_bytes())
    }

    use super::super::host_ai::messages::prepare_messages;
    use super::super::host_ai::{
        ArtifactReadFn, ArtifactRef, ContentPart, GenerateRequest, MessageRole, ModelMessage, SessionKey, ToolCall, ToolDefinition,
    };

    fn normalized_session() -> SessionKey {
        SessionKey { runtime_instance_id: "runtime-5".into(), session_id: "session-5".into() }
    }

    fn normalized_message(role: MessageRole, text: &str) -> ModelMessage {
        ModelMessage { role, content: vec![ContentPart::Text(text.into())], tool_calls: vec![], tool_call_id: None }
    }

    fn normalized_setup(wire: WireProtocol) -> (HostAiDependencies, ConnectionBinding, GenerateRequest, Arc<ScriptedHttp>) {
        use super::super::host_ai::{Delivery, GenerateOptions};
        let script = ScriptedHttp::new(vec![(200, strict_answer(wire))]);
        let deps = primitive_deps(script.http());
        let mut config = original_connection();
        config.key = false;
        config.reasoning_effort.clear();
        let model = match wire {
            WireProtocol::Messages => {
                config.kind = Kind::Anthropic;
                "claude-test"
            }
            WireProtocol::Responses => {
                config.base_url = "https://opencode.ai/inference/go".into();
                "gpt-test"
            }
            WireProtocol::Gemini => {
                config.base_url = "https://opencode.ai/inference/go".into();
                "gemini-test"
            }
            WireProtocol::Chat => "chosen",
        };
        deps.shared.connections().replace(&[config]).unwrap();
        let binding = deps.shared.connections().bind("api-original-7", model).unwrap();
        let request = GenerateRequest {
            connection_id: binding.connection_id().into(),
            model_id: model.into(),
            messages: vec![normalized_message(MessageRole::User, "hello")],
            tools: vec![ToolDefinition {
                name: "measure".into(),
                description: "Measure a value".into(),
                input_schema: json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}).as_object().unwrap().clone(),
            }],
            options: GenerateOptions {
                delivery: Delivery::Buffered,
                timeout_seconds: 60,
                max_output_tokens: None,
                temperature: None,
                reasoning_effort: None,
                output_schema: None,
            },
            continuation: None,
        };
        (deps, binding, request, script)
    }

    fn normalized_go_messages() -> (HostAiDependencies, ConnectionBinding, GenerateRequest, Arc<ScriptedHttp>) {
        let (deps, binding, request, script) = normalized_setup(WireProtocol::Messages);
        let mut config = binding.configuration().clone();
        config.kind = Kind::Api;
        config.base_url = "https://opencode.ai/inference/go".into();
        deps.shared.connections().replace(&[config]).unwrap();
        let binding = deps.shared.connections().bind(&request.connection_id, &request.model_id).unwrap();
        (deps, binding, request, script)
    }

    // Only external HTTP/artifact storage are doubles. Admission, codecs, worker and strict
    // projection are real; this is not a substitute facade/session implementation.
    fn normalized_invoke(
        deps: &HostAiDependencies,
        binding: &ConnectionBinding,
        request: &GenerateRequest,
        limits: &AiLimits,
        budget: &Arc<ModelCallBudget>,
    ) -> Result<ProviderReply, AiError> {
        let mut call = prepare_messages(&normalized_session(), binding, request, limits, &deps.artifacts, &ProviderState::default())?;
        call.model_budget = Some(Arc::clone(budget));
        settled_invoke(deps, binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).and_then(provider::project_strict_reply)
    }

    fn normalized_round() -> Vec<ModelMessage> {
        let mut assistant = normalized_message(MessageRole::Assistant, "two calls");
        assistant.tool_calls = vec![
            ToolCall { id: "call-a".into(), name: "measure".into(), arguments: json!({"n":1}).as_object().unwrap().clone() },
            ToolCall { id: "call-b".into(), name: "compare".into(), arguments: json!({"n":2}).as_object().unwrap().clone() },
        ];
        let mut b = normalized_message(MessageRole::Tool, "B");
        b.tool_call_id = Some("call-b".into());
        let mut a = normalized_message(MessageRole::Tool, "A");
        a.tool_call_id = Some("call-a".into());
        vec![normalized_message(MessageRole::User, "ask"), assistant, b, a]
    }

    use super::super::host_ai::continuation::{ContinuationStore, prepare_continuation};
    use super::super::host_ai::{IdFn, ModelReply};

    fn continuation_ids() -> IdFn {
        use std::sync::atomic::{AtomicU64, Ordering};
        let next = AtomicU64::new(1);
        Arc::new(move || Ok(format!("{:032x}", next.fetch_add(1, Ordering::SeqCst))))
    }

    fn continuation_answer(private: &ProviderState) -> ProviderReply {
        let assistant = json!({"role":"assistant","content":"done"});
        let mut private = private.clone();
        private.native_assistants.push(assistant.clone());
        ProviderReply {
            text: vec![(false, "done".into())],
            calls: vec![],
            assistant,
            private,
            finish_reason: super::super::host_ai::FinishReason::Stop,
            usage: None,
        }
    }

    fn continuation_extend(request: &mut GenerateRequest, reply: &ModelReply) {
        request.messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: reply.text.iter().cloned().map(ContentPart::Text).collect(),
            tool_calls: reply.tool_calls.clone(),
            tool_call_id: None,
        });
        for call in reply.tool_calls.iter().rev() {
            let mut result = normalized_message(MessageRole::Tool, &format!("result for {}", call.name));
            result.tool_call_id = Some(call.id.clone());
            request.messages.push(result);
        }
        request.messages.push(normalized_message(MessageRole::User, "continue"));
        request.continuation = reply.continuation.clone();
    }

    // Losing native assistant history used to discard signatures/reasoning in the actual
    // next HTTP body, even though the isolated decoder retained them.
    #[test]
    fn host_ai_continuation_preserves_private_state_only() {
        for (wire, go_messages) in [
            (WireProtocol::Chat, false),
            (WireProtocol::Responses, false),
            (WireProtocol::Messages, false),
            (WireProtocol::Messages, true),
            (WireProtocol::Gemini, false),
        ] {
            let (deps, binding, mut request, script) = if go_messages { normalized_go_messages() } else { normalized_setup(wire) };
            let signature = "private-signature-6-never-public";
            let bad_id = "provider / unsafe identity";
            let first = match wire {
                WireProtocol::Chat => json!({"choices":[{"message":{"role":"assistant","content":"visible",
                    "reasoning":"private-reasoning-6","reasoning_content":"private-thought-6",
                    "reasoning_details":[{"index":9,"type":"reasoning.encrypted","data":signature}],
                    "tool_calls":[{"id":bad_id,"type":"function","function":{"name":"measure","arguments":"{\"n\":1}"}}]},"finish_reason":"tool_calls"}]}),
                WireProtocol::Responses => json!({"output":[{"type":"reasoning","id":"r6","summary":[],"encrypted_content":signature},
                    {"type":"function_call","call_id":bad_id,"name":"measure","arguments":"{\"n\":1}"}],"status":"completed"}),
                WireProtocol::Messages => json!({"content":[{"type":"thinking","thinking":"private-thought-6","signature":signature},
                    {"type":"redacted_thinking","data":"private-redacted-6"},{"type":"text","text":"visible"},
                    {"type":"tool_use","id":bad_id,"name":"measure","input":{"n":1}}],"stop_reason":"tool_use"}),
                WireProtocol::Gemini => json!({"candidates":[{"content":{"role":"model","parts":[
                    {"functionCall":{"id":bad_id,"name":"measure","args":{"n":1}},"thoughtSignature":signature},
                    {"functionCall":{"id":"provider-b","name":"compare","args":{"n":2}},"thoughtSignature":"private-second-signature-6"}]},"finishReason":"STOP"}]}),
            };
            *script.replies.lock().unwrap() = vec![(200, first), (200, strict_answer(wire))].into();
            let limits = AiLimits::contract_defaults();
            let session = normalized_session();
            let mut store = ContinuationStore::default();
            store.open_session(session.clone(), limits.clone()).unwrap();
            let lease = store.begin(&session, &binding, &request, Duration::ZERO).unwrap();
            let route = lease.private().routing_session_id.clone();
            let call = prepare_messages(&session, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
            let reply = settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
            let prepared = prepare_continuation(lease, &request, &reply, &continuation_ids()).unwrap();
            let (public, id) = CallControl::new().publish(|| store.commit(prepared, Duration::ZERO)).unwrap();
            assert_eq!(public.continuation.as_ref(), Some(&id));
            assert_ne!(public.tool_calls[0].id, bad_id);
            for secret in [signature, "private-thought-6", "private-reasoning-6", "private-redacted-6", bad_id, &route] {
                assert!(!format!("{public:?}").contains(secret), "{wire:?}: private data leaked");
            }
            continuation_extend(&mut request, &public);
            let lease = store.begin(&session, &binding, &request, Duration::from_secs(1)).unwrap();
            assert_eq!(lease.private().routing_session_id, route);
            let call = prepare_messages(&session, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
            settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
            let requests = script.recorded.lock().unwrap();
            let body: Value = serde_json::from_slice(requests[1].body.as_ref().unwrap()).unwrap();
            let (signature_value, provider_id) = match wire {
                WireProtocol::Chat => {
                    assert_eq!(body["messages"][2]["reasoning"], "private-reasoning-6");
                    assert!(body["messages"][2]["reasoning_details"][0].get("index").is_none());
                    (body["messages"][2]["reasoning_details"][0]["data"].clone(), body["messages"][3]["tool_call_id"].clone())
                }
                WireProtocol::Responses => (body["input"][2]["encrypted_content"].clone(), body["input"][4]["call_id"].clone()),
                WireProtocol::Messages => {
                    assert_eq!(body["messages"][1]["content"][1]["data"], "private-redacted-6");
                    (body["messages"][1]["content"][0]["signature"].clone(), body["messages"][2]["content"][0]["tool_use_id"].clone())
                }
                WireProtocol::Gemini => {
                    assert_eq!(body["contents"][1]["parts"][1]["functionCall"]["name"], "compare");
                    assert_eq!(body["contents"][2]["parts"][0]["functionResponse"]["name"], "compare");
                    assert_eq!(body["contents"][2]["parts"][0]["functionResponse"]["id"], "provider-b");
                    assert_eq!(body["contents"][2]["parts"][1]["functionResponse"]["name"], "measure");
                    (body["contents"][1]["parts"][0]["thoughtSignature"].clone(), body["contents"][2]["parts"][1]["functionResponse"]["id"].clone())
                }
            };
            assert_eq!(signature_value, signature, "{wire:?}");
            assert_eq!(provider_id, bad_id, "exact provider ID replay, {wire:?}");
            if wire != WireProtocol::Chat && binding.protocol() == Protocol::OpenAi {
                let header = |i: usize| requests[i].headers.iter().find(|(name, _)| name == "x-opencode-session").unwrap().1.clone();
                assert_eq!(header(0), route);
                assert_eq!(header(1), route);
            }
            drop(lease);
            request.continuation = None;
            let fresh = store.begin(&session, &binding, &request, Duration::from_secs(2)).unwrap();
            assert_ne!(fresh.private().routing_session_id, route);
            assert!(fresh.private().native_assistants.iter().all(Value::is_null));
        }
    }

    // Rebuilding history only from ModelReply must preserve every native assistant
    // ordinal, including private-only and empty rounds between signed replies.
    #[test]
    fn host_ai_continuation_r1_private_only_replies_replay_all_native_ordinals() {
        for (wire, go_messages) in [
            (WireProtocol::Messages, false),
            (WireProtocol::Messages, true),
            (WireProtocol::Responses, false),
            (WireProtocol::Gemini, false),
            (WireProtocol::Chat, false),
        ] {
            let (deps, binding, mut request, script) = if go_messages { normalized_go_messages() } else { normalized_setup(wire) };
            let private_only = |second: bool| match wire {
                WireProtocol::Messages => {
                    if second {
                        json!({"content":[{"type":"redacted_thinking","data":"r1-private-B"}],"stop_reason":"end_turn"})
                    } else {
                        json!({"content":[{"type":"thinking","thinking":"r1-private-A","signature":"r1-signature-A"}],"stop_reason":"end_turn"})
                    }
                }
                WireProtocol::Responses => {
                    json!({"output":[{"type":"reasoning","id":if second {"rB"} else {"rA"},"summary":[],"encrypted_content":if second {"r1-private-B"} else {"r1-private-A"}}],"status":"completed"})
                }
                WireProtocol::Gemini => {
                    json!({"candidates":[{"content":{"role":"model","parts":[{"text":if second {"r1-private-B"} else {"r1-private-A"},"thought":true,"thoughtSignature":if second {"r1-signature-B"} else {"r1-signature-A"}}]},"finishReason":"STOP"}]})
                }
                WireProtocol::Chat => {
                    json!({"choices":[{"message":{"role":"assistant","content":null,"reasoning_content":if second {"r1-private-B"} else {"r1-private-A"}},"finish_reason":"stop"}]})
                }
            };
            let empty = match wire {
                WireProtocol::Messages => json!({"content":[],"stop_reason":"end_turn"}),
                WireProtocol::Responses => json!({"output":[],"status":"completed"}),
                WireProtocol::Gemini => json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}]}),
                WireProtocol::Chat => json!({"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}),
            };
            *script.replies.lock().unwrap() =
                vec![(200, private_only(false)), (200, empty), (200, private_only(true)), (200, strict_answer(wire))].into();
            let key = normalized_session();
            let limits = AiLimits::contract_defaults();
            let ids = continuation_ids();
            let mut store = ContinuationStore::default();
            store.open_session(key.clone(), limits.clone()).unwrap();
            let mut projections = vec![];
            for round in 0..4 {
                let lease = store.begin(&key, &binding, &request, Duration::from_secs(round)).unwrap();
                let call = prepare_messages(&key, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
                let proposal = settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60))
                    .and_then(provider::project_strict_reply)
                    .unwrap();
                if round == 3 {
                    assert_eq!(proposal.text, vec![(false, "answer".into())]);
                    drop(lease);
                    break;
                }
                let prepared = prepare_continuation(lease, &request, &proposal, &ids).unwrap();
                let (public, _) = CallControl::new().publish(|| store.commit(prepared, Duration::from_secs(round))).unwrap();
                // This constructs the assistant from the returned public projection,
                // never from store internals or a saved private request prefix.
                continuation_extend(&mut request, &public);
                projections.push(public);
            }
            for public in projections {
                assert_eq!(public.text, vec![""]);
                assert!(public.tool_calls.is_empty());
                assert!(!format!("{public:?}").contains("r1-private"));
            }
            let recorded = script.recorded.lock().unwrap();
            assert_eq!(recorded.len(), 4);
            let body: Value = serde_json::from_slice(recorded[3].body.as_ref().unwrap()).unwrap();
            match wire {
                WireProtocol::Messages => {
                    assert_eq!(body["messages"].as_array().unwrap().len(), 7);
                    assert_eq!(body["messages"][1]["content"][0]["signature"], "r1-signature-A");
                    assert_eq!(body["messages"][3], json!({"role":"assistant","content":[{"type":"text","text":""}]}));
                    assert_eq!(body["messages"][5]["content"][0]["data"], "r1-private-B");
                }
                WireProtocol::Responses => {
                    assert_eq!(body["input"].as_array().unwrap().len(), 8);
                    assert_eq!(body["input"][2]["id"], "rA");
                    assert_eq!(body["input"][2]["encrypted_content"], "r1-private-A");
                    assert_eq!(body["input"][4], json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}));
                    assert_eq!(body["input"][6]["id"], "rB");
                    assert_eq!(body["input"][6]["encrypted_content"], "r1-private-B");
                }
                WireProtocol::Gemini => {
                    assert_eq!(body["contents"].as_array().unwrap().len(), 7);
                    assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "r1-signature-A");
                    assert_eq!(body["contents"][3], json!({"role":"model","parts":[{"text":""}]}));
                    assert_eq!(body["contents"][5]["parts"][0]["thoughtSignature"], "r1-signature-B");
                }
                WireProtocol::Chat => {
                    assert_eq!(body["messages"].as_array().unwrap().len(), 8);
                    assert_eq!(body["messages"][2]["reasoning_content"], "r1-private-A");
                    assert_eq!(body["messages"][4]["role"], "assistant");
                    assert_eq!(body["messages"][4]["content"], "");
                    assert_eq!(body["messages"][6]["reasoning_content"], "r1-private-B");
                }
            }
            drop(recorded);
            let mut arbitrary_empty = request.clone();
            arbitrary_empty.messages[1].content.clear();
            for continuation in [request.continuation.clone(), None] {
                arbitrary_empty.continuation = continuation;
                assert_eq!(store.begin(&key, &binding, &arbitrary_empty, Duration::from_secs(4)).err().unwrap().code(), AiErrorCode::InvalidRequest);
            }
        }
    }

    #[test]
    fn host_ai_continuation_r1_empty_finishes_are_publicly_reconstructable() {
        use super::super::host_ai::FinishReason;
        for wire in [WireProtocol::Chat, WireProtocol::Messages, WireProtocol::Responses, WireProtocol::Gemini] {
            for (finish, expected) in [(0, FinishReason::Stop), (1, FinishReason::Length), (2, FinishReason::Refusal)] {
                let (deps, binding, mut request, script) = normalized_setup(wire);
                let empty = match wire {
                    WireProtocol::Chat => {
                        json!({"choices":[{"message":{"role":"assistant","content":null},"finish_reason":(["stop","length","content_filter"][finish])}]})
                    }
                    WireProtocol::Messages => json!({"content":[],"stop_reason":(["end_turn","max_tokens","refusal"][finish])}),
                    WireProtocol::Responses => {
                        if finish == 0 {
                            json!({"output":[],"status":"completed"})
                        } else {
                            json!({"output":[],"status":"incomplete","incomplete_details":{"reason":if finish==1 {"max_output_tokens"} else {"content_filter"}}})
                        }
                    }
                    WireProtocol::Gemini => {
                        json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":(["STOP","MAX_TOKENS","SAFETY"][finish])}]})
                    }
                };
                *script.replies.lock().unwrap() = vec![(200, empty), (200, strict_answer(wire))].into();
                let key = normalized_session();
                let limits = AiLimits::contract_defaults();
                let mut store = ContinuationStore::default();
                store.open_session(key.clone(), limits.clone()).unwrap();
                let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
                let call = prepare_messages(&key, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
                let proposal = settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60))
                    .and_then(provider::project_strict_reply)
                    .unwrap();
                let (public, id) =
                    store.commit(prepare_continuation(lease, &request, &proposal, &continuation_ids()).unwrap(), Duration::ZERO).unwrap();
                assert_eq!(public.continuation.as_ref(), Some(&id));
                assert_eq!(public.finish_reason, expected);
                continuation_extend(&mut request, &public);
                let lease = store.begin(&key, &binding, &request, Duration::from_secs(1)).unwrap();
                let call = prepare_messages(&key, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
                settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
                assert_eq!(public.text, vec![""]);
                assert_eq!(script.recorded.lock().unwrap().len(), 2);
                let recorded = script.recorded.lock().unwrap();
                let body: Value = serde_json::from_slice(recorded[1].body.as_ref().unwrap()).unwrap();
                match wire {
                    WireProtocol::Chat => assert_eq!(body["messages"][2]["role"], "assistant"),
                    WireProtocol::Messages => assert_eq!(body["messages"][1], json!({"role":"assistant","content":[{"type":"text","text":""}]})),
                    WireProtocol::Responses => {
                        assert_eq!(body["input"][2], json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}))
                    }
                    WireProtocol::Gemini => assert_eq!(body["contents"][1], json!({"role":"model","parts":[{"text":""}]})),
                }
            }
        }
    }

    // The first prepare holds its reservation in a real, lock-free ID callback.
    // The second cannot borrow an uncharged charging workspace from the same session.
    #[test]
    fn host_ai_continuation_r1_parallel_charging_workspace_is_reserved_before_walks() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_, binding, root, _) = normalized_setup(WireProtocol::Chat);
        let key = normalized_session();
        let ids = continuation_ids();
        let mut limits = AiLimits::contract_defaults();
        limits.continuation_bytes = 40_960;
        let mut store = ContinuationStore::default();
        store.open_session(key.clone(), limits).unwrap();
        let lease = store.begin(&key, &binding, &root, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let (public, _) = store.commit(prepare_continuation(lease, &root, &reply, &ids).unwrap(), Duration::ZERO).unwrap();
        let mut parent = root.clone();
        continuation_extend(&mut parent, &public);
        let held = store.begin(&key, &binding, &root, Duration::ZERO).unwrap();
        let held_reply = continuation_answer(held.private());
        let second = store.begin(&key, &binding, &parent, Duration::ZERO).unwrap();
        let second_reply = continuation_answer(second.private());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let held_ids: IdFn = Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv_timeout(RACE_GUARD).unwrap();
            Ok("000000000000000000000000000000a1".into())
        });
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            assert!(done_tx.send(prepare_continuation(held, &root, &held_reply, &held_ids)).is_ok());
        });
        entered_rx.recv_timeout(RACE_GUARD).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let second_ids: IdFn = Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok("000000000000000000000000000000a2".into())
        });
        let error = prepare_continuation(second, &parent, &second_reply, &second_ids).err().map(|e| e.code());
        release_tx.send(()).unwrap();
        let prepared = done_rx.recv_timeout(RACE_GUARD).unwrap().unwrap();
        worker.join().unwrap();
        assert_eq!(error, Some(AiErrorCode::ResourceExhausted), "parallel preparations must include their charging workspaces");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "workspace/capacity refusal precedes external IDs");
        // Keep the first owned candidate charged while rejected wide walks exercise
        // both native JSON and object-argument paths. Neither may change the parent.
        for object_arguments in [false, true] {
            let lease = store.begin(&key, &binding, &parent, Duration::ZERO).unwrap();
            let mut reply = continuation_answer(lease.private());
            if object_arguments {
                let arguments: serde_json::Map<String, Value> = (0..1024).map(|i| (format!("k{i:04}"), Value::Null)).collect();
                let call =
                    json!({"id":"wide-call","type":"function","function":{"name":"measure","arguments":serde_json::to_string(&arguments).unwrap()}});
                reply.assistant["tool_calls"] = json!([call]);
                reply.calls.push(provider::ProviderToolCall { id: "wide-call".into(), name: "measure".into(), arguments: Ok(arguments) });
            } else {
                reply.assistant["reasoning_details"] = json!(vec![json!({"type":"reasoning.text","text":"x"}); 1024]);
            }
            *reply.private.native_assistants.last_mut().unwrap() = reply.assistant.clone();
            assert_eq!(prepare_continuation(lease, &parent, &reply, &second_ids).err().unwrap().code(), AiErrorCode::ResourceExhausted);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        let shared = Mutex::new(store);
        let mut guard = shared.lock().unwrap();
        drop(prepared);
        let lease = guard.begin(&key, &binding, &parent, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        drop(guard);
        let prepared = prepare_continuation(lease, &parent, &reply, &ids).unwrap();
        shared.lock().unwrap().commit(prepared, Duration::ZERO).unwrap();
        assert_eq!(shared.lock().unwrap().begin(&key, &binding, &parent, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
    }

    // Typed equality must reject a changed old tool result/argument/artifact, while map
    // insertion order is not an alternate branch.
    #[test]
    fn host_ai_continuation_rejects_other_branch_model_or_session() {
        let (_, binding, mut request, _) = normalized_setup(WireProtocol::Chat);
        request.messages = normalized_round();
        request.messages[0].content.push(ContentPart::Image(ArtifactRef {
            artifact_id: "image-6".into(),
            digest: [6; 32],
            size_bytes: 1,
            media_type: "image/png".into(),
        }));
        let key = normalized_session();
        let mut store = ContinuationStore::default();
        store.open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let (public, _) = store.commit(prepare_continuation(lease, &request, &reply, &continuation_ids()).unwrap(), Duration::ZERO).unwrap();
        continuation_extend(&mut request, &public);
        for variant in 0..8 {
            let mut changed = request.clone();
            match variant {
                0 => changed.messages[0].content[0] = ContentPart::Text("changed".into()),
                1 => {
                    changed.messages[1].tool_calls[0].arguments.insert("n".into(), json!(99));
                }
                2 => changed.messages[2].content[0] = ContentPart::Text("changed result".into()),
                3 => {
                    if let ContentPart::Image(a) = &mut changed.messages[0].content[1] {
                        a.digest[0] = 9;
                    }
                }
                4 => changed.model_id = "other".into(),
                5 => changed.connection_id = "other".into(),
                6 => changed.tools[0].description = "changed".into(),
                _ => changed.options.temperature = Some(0.2),
            }
            assert_eq!(store.begin(&key, &binding, &changed, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
        }
        let other = SessionKey { session_id: "other".into(), ..key.clone() };
        store.open_session(other.clone(), AiLimits::contract_defaults()).unwrap();
        assert_eq!(store.begin(&other, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
        // Re-insertion of an equal typed argument value is not a changed branch.
        let args = &mut request.messages[1].tool_calls[0].arguments;
        let n = args.remove("n").unwrap();
        args.insert("n".into(), n);
        drop(store.begin(&key, &binding, &request, Duration::ZERO).unwrap());
        store.retire(&key);
        store.open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
        assert_eq!(store.begin(&key, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
    }

    // Admissions reserve slots before I/O, and TTL uses the injected monotonic clock.
    #[test]
    fn host_ai_continuation_limits_and_expiry_are_enforced() {
        let (deps, binding, request, script) = normalized_setup(WireProtocol::Chat);
        let key = normalized_session();
        let ids = continuation_ids();
        let mut store = ContinuationStore::default();
        store.open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
        let mut last = request.clone();
        for _ in 0..32 {
            let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
            let reply = continuation_answer(lease.private());
            let (public, _) = store.commit(prepare_continuation(lease, &request, &reply, &ids).unwrap(), Duration::ZERO).unwrap();
            last = request.clone();
            continuation_extend(&mut last, &public);
        }
        let budget = Arc::new(ModelCallBudget::new(1).unwrap());
        let refused = (|| {
            let lease = store.begin(&key, &binding, &request, Duration::ZERO)?;
            let mut call = prepare_messages(&key, &binding, &request, &AiLimits::contract_defaults(), &deps.artifacts, lease.private())?;
            call.model_budget = Some(Arc::clone(&budget));
            settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60))
        })();
        assert_eq!(refused.err().unwrap().code(), AiErrorCode::ResourceExhausted);
        assert!(script.recorded.lock().unwrap().is_empty());
        // A full store may still replace one exclusively leased parent atomically;
        // otherwise every conversation would stall until TTL when the 32nd branch opens.
        let lease = store.begin(&key, &binding, &last, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let (successor, _) = store.commit(prepare_continuation(lease, &last, &reply, &ids).unwrap(), Duration::ZERO).unwrap();
        assert_eq!(store.begin(&key, &binding, &last, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
        continuation_extend(&mut last, &successor);
        assert_eq!(store.begin(&key, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        let seconds = Arc::new(std::sync::atomic::AtomicU64::new(86_400));
        let clock_seconds = Arc::clone(&seconds);
        let clock: ClockFn = Arc::new(move || Duration::from_secs(clock_seconds.load(std::sync::atomic::Ordering::SeqCst)));
        assert_eq!(store.begin(&key, &binding, &last, clock()).err().unwrap().code(), AiErrorCode::InvalidRequest);
        drop(store.begin(&key, &binding, &request, clock()).unwrap());
        store.retire(&key);
        let mut limits = AiLimits::contract_defaults();
        limits.continuation_bytes = 16_384;
        limits.continuation_handles = 1;
        limits.continuation_ttl_seconds = 2;
        store.open_session(key.clone(), limits.clone()).unwrap();
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        assert_eq!(store.begin(&key, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        drop(lease);
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let mut reply = continuation_answer(lease.private());
        reply.private.native_assistants.push(json!("s".repeat(16_384)));
        assert_eq!(prepare_continuation(lease, &request, &reply, &ids).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        drop(store.begin(&key, &binding, &request, Duration::ZERO).unwrap());
        let mut flood = request.clone();
        flood.messages[0].content = vec![ContentPart::Text(String::new()); 256];
        assert_eq!(store.begin(&key, &binding, &flood, Duration::ZERO).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        // No artifact payload/base64 charge: all four separately allowed 4MiB refs fit.
        let mut attachments = request.clone();
        attachments.messages[0].content = (0..4)
            .map(|n| {
                ContentPart::Image(ArtifactRef {
                    artifact_id: format!("image-{n}"),
                    digest: [6; 32],
                    size_bytes: 4 << 20,
                    media_type: "image/png".into(),
                })
            })
            .collect();
        drop(store.begin(&key, &binding, &attachments, Duration::ZERO).unwrap());
        // The exact same one-attempt budget is still usable after pre-HTTP refusal.
        store.retire(&key);
        let limits = AiLimits::contract_defaults();
        store.open_session(key.clone(), limits.clone()).unwrap();
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let mut call = prepare_messages(&key, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
        call.model_budget = Some(Arc::clone(&budget));
        assert_eq!(settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap().text[0].1, "answer");
        assert_eq!(script.recorded.lock().unwrap().len(), 1);
        assert_eq!(normalized_invoke(&deps, &binding, &request, &limits, &budget).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        assert_eq!(script.recorded.lock().unwrap().len(), 1);
    }

    // Drop/error may occur with the caller's mutex still held. It must release capacity
    // without re-locking it, and only a successful successor may invalidate its parent.
    #[test]
    fn host_ai_continuation_serializes_successor_admission() {
        let (_, binding, mut request, _) = normalized_setup(WireProtocol::Chat);
        let key = normalized_session();
        let ids = continuation_ids();
        let mut store = ContinuationStore::default();
        let mut limits = AiLimits::contract_defaults();
        limits.continuation_handles = 1;
        limits.continuation_bytes = 65_536;
        store.open_session(key.clone(), limits).unwrap();
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let (public, parent) = store.commit(prepare_continuation(lease, &request, &reply, &ids).unwrap(), Duration::ZERO).unwrap();
        continuation_extend(&mut request, &public);
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        assert_eq!(store.begin(&key, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
        drop(lease);
        for source in [Arc::new(|| Err(ModelCallBudget::new(0).err().unwrap())) as IdFn, Arc::new(|| Ok("invalid".into())) as IdFn] {
            let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
            let reply = continuation_answer(lease.private());
            assert!(prepare_continuation(lease, &request, &reply, &source).is_err());
        }
        let same = parent.as_str().to_owned();
        let collision: IdFn = Arc::new(move || Ok(same.clone()));
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let prepared = prepare_continuation(lease, &request, &reply, &collision).unwrap();
        assert_eq!(store.commit(prepared, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
        let shared = Arc::new(Mutex::new(store));
        let mut guard = shared.lock().unwrap();
        drop(guard.begin(&key, &binding, &request, Duration::ZERO).unwrap());
        let lease = guard.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        drop(guard);
        let cancelled = CallControl::new();
        let prepared = prepare_continuation(lease, &request, &reply, &ids).unwrap();
        cancelled.cancel();
        assert_eq!(cancelled.publish(|| shared.lock().unwrap().commit(prepared, Duration::ZERO)).err().unwrap().code(), AiErrorCode::Cancelled);
        let lease = shared.lock().unwrap().begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let prepared = prepare_continuation(lease, &request, &reply, &ids).unwrap();
        let control = CallControl::new();
        let (success, _) = control.publish(|| shared.lock().unwrap().commit(prepared, Duration::ZERO)).unwrap();
        assert_eq!(shared.lock().unwrap().begin(&key, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
        assert!(control.publish(|| Ok(())).is_err());
        continuation_extend(&mut request, &success);
        drop(shared.lock().unwrap().begin(&key, &binding, &request, Duration::ZERO).unwrap());
    }

    // These are real Mutex<Store> / CallControl seams, not a replacement facade. Only
    // the external ID function is injected; channel rendezvous, not sleeps, orders races.
    #[test]
    fn host_ai_continuation_id_callbacks_are_outside_publication_locks() {
        for mode in 0..3 {
            let paused = mode == 2;
            let (_, binding, request, _) = normalized_setup(WireProtocol::Chat);
            let key = normalized_session();
            let mut store = ContinuationStore::default();
            store.open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
            let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
            let reply = continuation_answer(lease.private());
            let store = Arc::new(Mutex::new(store));
            let control = Arc::new(CallControl::new());
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let release = Mutex::new(release_rx);
            let s = Arc::clone(&store);
            let c = Arc::clone(&control);
            let k = key.clone();
            let ids: IdFn = Arc::new(move || {
                if paused {
                    entered_tx.send(()).unwrap();
                    release.lock().unwrap().recv_timeout(RACE_GUARD).unwrap();
                } else if mode == 0 {
                    s.lock().unwrap().retire(&k);
                } else {
                    c.cancel();
                }
                Ok("00000000000000000000000000000006".into())
            });
            let s = Arc::clone(&store);
            let c = Arc::clone(&control);
            let (done_tx, done_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let result = prepare_continuation(lease, &request, &reply, &ids)
                    .and_then(|prepared| c.publish(|| s.lock().unwrap().commit(prepared, Duration::ZERO)));
                done_tx.send(result.err().map(|e| e.code())).unwrap();
            });
            if paused {
                entered_rx.recv_timeout(RACE_GUARD).unwrap();
                store.lock().unwrap().retire(&key);
                control.cancel();
                store.lock().unwrap().open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
                release_tx.send(()).unwrap();
            }
            assert!(done_rx.recv_timeout(RACE_GUARD).unwrap().is_some(), "late output/state must never publish");
            worker.join().unwrap();
            // A fresh generation is usable; the retired lease did not resurrect it.
            if mode == 0 {
                store.lock().unwrap().open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
            }
            let (_, _, request, _) = normalized_setup(WireProtocol::Chat);
            drop(store.lock().unwrap().begin(&key, &binding, &request, Duration::ZERO).unwrap());
        }
    }

    // An owned prepared candidate is still only a proposal: both expiry and a new
    // session generation can win before callback-free commit under the real mutex.
    #[test]
    fn host_ai_continuation_revalidates_prepared_generation_ttl_and_total_reservations() {
        let (_, binding, mut request, _) = normalized_setup(WireProtocol::Chat);
        let key = normalized_session();
        let ids = continuation_ids();
        let mut store = ContinuationStore::default();
        let mut limits = AiLimits::contract_defaults();
        limits.continuation_ttl_seconds = 2;
        store.open_session(key.clone(), limits.clone()).unwrap();
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let (public, _) = store.commit(prepare_continuation(lease, &request, &reply, &ids).unwrap(), Duration::ZERO).unwrap();
        continuation_extend(&mut request, &public);
        let lease = store.begin(&key, &binding, &request, Duration::from_secs(1)).unwrap();
        let reply = continuation_answer(lease.private());
        let prepared = prepare_continuation(lease, &request, &reply, &ids).unwrap();
        let shared = Mutex::new(store);
        assert_eq!(shared.lock().unwrap().commit(prepared, Duration::from_secs(2)).err().unwrap().code(), AiErrorCode::InvalidRequest);
        request.continuation = None;
        let lease = shared.lock().unwrap().begin(&key, &binding, &request, Duration::from_secs(2)).unwrap();
        let reply = continuation_answer(lease.private());
        let prepared = prepare_continuation(lease, &request, &reply, &ids).unwrap();
        let mut guard = shared.lock().unwrap();
        guard.retire(&key);
        guard.open_session(key.clone(), limits).unwrap();
        assert_eq!(guard.commit(prepared, Duration::from_secs(2)).err().unwrap().code(), AiErrorCode::InvalidRequest);
        guard.retire(&key);
        let mut limits = AiLimits::contract_defaults();
        limits.continuation_bytes = 16_384;
        guard.open_session(key.clone(), limits).unwrap();
        let (_, _, request, _) = normalized_setup(WireProtocol::Chat);
        let mut leases = vec![];
        for _ in 0..32 {
            match guard.begin(&key, &binding, &request, Duration::ZERO) {
                Ok(lease) => leases.push(lease),
                Err(error) => {
                    assert_eq!(error.code(), AiErrorCode::ResourceExhausted);
                    break;
                }
            }
        }
        assert!(!leases.is_empty() && leases.len() < 32, "storage must bind before handle count");
        assert_eq!(guard.begin(&key, &binding, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        drop(leases.pop());
        drop(guard.begin(&key, &binding, &request, Duration::ZERO).unwrap());
        drop(leases); // under caller's lock, with no reentrant release
    }

    #[test]
    fn host_ai_continuation_rejects_replaying_private_history_into_another_protocol() {
        let (deps, binding, mut request, _) = normalized_setup(WireProtocol::Chat);
        let key = normalized_session();
        let mut store = ContinuationStore::default();
        store.open_session(key.clone(), AiLimits::contract_defaults()).unwrap();
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let reply = continuation_answer(lease.private());
        let (public, _) = store.commit(prepare_continuation(lease, &request, &reply, &continuation_ids()).unwrap(), Duration::ZERO).unwrap();
        continuation_extend(&mut request, &public);
        let mut configuration = binding.configuration().clone();
        configuration.kind = Kind::Anthropic;
        deps.shared.connections().replace(&[configuration]).unwrap();
        let other = deps.shared.connections().bind(&request.connection_id, &request.model_id).unwrap();
        assert_eq!(store.begin(&key, &other, &request, Duration::ZERO).err().unwrap().code(), AiErrorCode::InvalidRequest);
    }

    // Provider IDs can repeat in later rounds, but normalized IDs cannot. The round's
    // native signature/name and exact raw ID must remain paired in the third HTTP body.
    #[test]
    fn host_ai_continuation_tool_identity_is_bijective_per_round() {
        let (deps, binding, mut request, script) = normalized_setup(WireProtocol::Gemini);
        let raw = "provider-a";
        let round = |name: &str, signature: &str| {
            json!({"candidates":[{"content":{"parts":[
            {"text":"private-gemini-thought-6","thought":true},
            {"functionCall":{"id":raw,"name":name,"args":{"n":1}},"thoughtSignature":signature}
        ]},"finishReason":"STOP"}]})
        };
        *script.replies.lock().unwrap() =
            vec![(200, round("measure", "round-A-private")), (200, round("compare", "round-B-private")), (200, strict_answer(WireProtocol::Gemini))]
                .into();
        let key = normalized_session();
        let ids = continuation_ids();
        let limits = AiLimits::contract_defaults();
        let mut store = ContinuationStore::default();
        store.open_session(key.clone(), limits.clone()).unwrap();
        let mut wire_ids = vec![];
        for round_index in 0..2 {
            let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
            let call = prepare_messages(&key, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
            let reply = settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
            let (public, _) = store.commit(prepare_continuation(lease, &request, &reply, &ids).unwrap(), Duration::ZERO).unwrap();
            assert!(public.text.is_empty());
            assert!(!format!("{public:?}").contains("private"));
            if round_index == 0 {
                assert_eq!(public.tool_calls[0].id, raw);
            } else {
                assert_ne!(public.tool_calls[0].id, raw);
            }
            wire_ids.push(public.tool_calls[0].id.clone());
            continuation_extend(&mut request, &public);
        }
        assert_ne!(wire_ids[0], wire_ids[1]);
        let lease = store.begin(&key, &binding, &request, Duration::ZERO).unwrap();
        let call = prepare_messages(&key, &binding, &request, &limits, &deps.artifacts, lease.private()).unwrap();
        settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
        drop(lease);
        let recorded = script.recorded.lock().unwrap();
        let body: Value = serde_json::from_slice(recorded[2].body.as_ref().unwrap()).unwrap();
        for (index, name, signature) in [(1, "measure", "round-A-private"), (3, "compare", "round-B-private")] {
            assert_eq!(body["contents"][index]["parts"][1]["functionCall"]["id"], raw);
            assert_eq!(body["contents"][index]["parts"][1]["thoughtSignature"], signature);
            assert_eq!(body["contents"][index + 1]["parts"][0]["functionResponse"]["id"], raw);
            assert_eq!(body["contents"][index + 1]["parts"][0]["functionResponse"]["name"], name);
        }
        drop(recorded);
        // Duplicates within one round have no bijective inverse and are malformed.
        let mut duplicate = round("measure", "duplicate-private");
        let parts = duplicate["candidates"][0]["content"]["parts"].as_array_mut().unwrap();
        parts.push(parts[1].clone());
        script.replies.lock().unwrap().push_back((200, duplicate));
        let mut fresh = request.clone();
        fresh.continuation = None;
        let lease = store.begin(&key, &binding, &fresh, Duration::ZERO).unwrap();
        let call = prepare_messages(&key, &binding, &fresh, &limits, &deps.artifacts, lease.private()).unwrap();
        let reply = settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
        assert_eq!(prepare_continuation(lease, &fresh, &reply, &ids).err().unwrap().code(), AiErrorCode::MalformedResponse);
        drop(store.begin(&key, &binding, &request, Duration::ZERO).unwrap());
        // A malformed hybrid part must not leave an unprojected private functionCall
        // without a corresponding public call (the legacy Go decoder picks text first).
        let hybrid = json!({"candidates":[{"content":{"parts":[{"text":"visible","functionCall":{"id":"hybrid","name":"measure","args":{"n":1}}}]},"finishReason":"STOP"}]});
        script.replies.lock().unwrap().push_back((200, hybrid));
        let lease = store.begin(&key, &binding, &fresh, Duration::ZERO).unwrap();
        let call = prepare_messages(&key, &binding, &fresh, &limits, &deps.artifacts, lease.private()).unwrap();
        assert_eq!(
            settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::MalformedResponse
        );
        drop(lease);
        drop(store.begin(&key, &binding, &request, Duration::ZERO).unwrap());
    }

    // Removing role/pending-ID admission would send incomplete, duplicate or foreign results.
    #[test]
    fn host_ai_messages_require_complete_matching_tool_rounds() {
        let (deps, binding, mut request, script) = normalized_setup(WireProtocol::Chat);
        let valid = normalized_round();
        let mut cases = vec![];
        let mut duplicate = valid.clone();
        duplicate.push(valid[2].clone());
        cases.push(duplicate);
        let mut unmatched = valid.clone();
        unmatched[2].tool_call_id = Some("foreign".into());
        cases.push(unmatched);
        cases.push(valid[..3].to_vec());
        let mut interrupted = valid[..3].to_vec();
        interrupted.push(normalized_message(MessageRole::User, "new user"));
        cases.push(interrupted);
        let mut interrupted = valid[..3].to_vec();
        interrupted.push(normalized_message(MessageRole::Assistant, "new model"));
        cases.push(interrupted);
        let mut duplicate_call = valid.clone();
        duplicate_call[1].tool_calls[1].id = "call-a".into();
        cases.push(duplicate_call);
        let mut user_calls = valid.clone();
        user_calls[0].tool_calls = valid[1].tool_calls.clone();
        cases.push(user_calls);
        let mut assistant_result = valid.clone();
        assistant_result[1].tool_call_id = Some("call-a".into());
        cases.push(assistant_result);
        let mut missing_id = valid.clone();
        missing_id[2].tool_call_id = None;
        cases.push(missing_id);
        let mut tool_calls = valid.clone();
        tool_calls[2].tool_calls = valid[1].tool_calls.clone();
        cases.push(tool_calls);
        let mut reused = valid.clone();
        reused.extend(valid.clone());
        cases.push(reused);
        let mut late_system = valid.clone();
        late_system.push(normalized_message(MessageRole::System, "late"));
        cases.push(late_system);
        let budget = Arc::new(ModelCallBudget::new(1).unwrap());
        for messages in cases {
            request.messages = messages;
            assert_eq!(
                normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &budget).err().unwrap().code(),
                AiErrorCode::InvalidRequest
            );
            assert!(script.recorded.lock().unwrap().is_empty());
        }
        request.messages = valid;
        normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &budget).unwrap();
        assert_eq!(script.recorded.lock().unwrap().len(), 1, "preflight refusals must not debit the shared attempt budget");

        for (wire, via_go) in [
            (WireProtocol::Chat, false),
            (WireProtocol::Responses, false),
            (WireProtocol::Messages, false),
            (WireProtocol::Messages, true),
            (WireProtocol::Gemini, false),
        ] {
            let (deps, binding, mut request, script) = if via_go { normalized_go_messages() } else { normalized_setup(wire) };
            request.messages = normalized_round();
            request.messages.insert(0, normalized_message(MessageRole::System, "policy"));
            normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &Arc::new(ModelCallBudget::new(1).unwrap())).unwrap();
            let recorded = script.recorded.lock().unwrap();
            let body: Value = serde_json::from_slice(recorded[0].body.as_ref().unwrap()).unwrap();
            match wire {
                WireProtocol::Chat => {
                    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call-a");
                    assert_eq!(body["messages"][2]["tool_calls"][1]["function"]["name"], "compare");
                    assert_eq!(body["messages"][3]["tool_call_id"], "call-b");
                    assert_eq!(body["messages"][4]["tool_call_id"], "call-a");
                }
                WireProtocol::Responses => {
                    assert_eq!(body["input"][3]["call_id"], "call-a");
                    assert_eq!(body["input"][4]["call_id"], "call-b");
                    assert_eq!(body["input"][4]["name"], "compare");
                    assert_eq!(body["input"][5], json!({"type":"function_call_output","call_id":"call-b","output":"B"}));
                    assert_eq!(body["input"][6]["call_id"], "call-a");
                }
                WireProtocol::Messages => {
                    assert_eq!(body["messages"][1]["content"][1], json!({"type":"tool_use","id":"call-a","name":"measure","input":{"n":1}}));
                    assert_eq!(body["messages"][1]["content"][2]["id"], "call-b");
                    assert_eq!(body["messages"][1]["content"][2]["name"], "compare");
                    assert_eq!(
                        body["messages"][2]["content"],
                        json!([{"type":"tool_result","tool_use_id":"call-b","content":"B"},{"type":"tool_result","tool_use_id":"call-a","content":"A"}])
                    );
                }
                WireProtocol::Gemini => {
                    assert_eq!(body["contents"][1]["parts"][1]["functionCall"], json!({"id":"call-a","name":"measure","args":{"n":1}}));
                    assert_eq!(body["contents"][1]["parts"][2]["functionCall"], json!({"id":"call-b","name":"compare","args":{"n":2}}));
                    assert_eq!(
                        body["contents"][2]["parts"],
                        json!([{"functionResponse":{"id":"call-b","name":"compare","response":{"output":"B"}}},{"functionResponse":{"id":"call-a","name":"measure","response":{"output":"A"}}}])
                    );
                }
            }
        }
    }

    fn synthetic_artifact(bytes: &[u8], media_type: &str) -> ArtifactRef {
        use sha2::{Digest, Sha256};
        ArtifactRef {
            artifact_id: "image-5".into(),
            digest: Sha256::digest(bytes).into(),
            size_bytes: bytes.len() as u32,
            media_type: media_type.into(),
        }
    }

    fn authorized_artifacts(reference: ArtifactRef, bytes: Arc<[u8]>, reads: Arc<std::sync::atomic::AtomicUsize>) -> ArtifactReadFn {
        Arc::new(move |session, artifact| {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if session == &normalized_session() && artifact == &reference {
                Ok(Arc::clone(&bytes))
            } else {
                Err(ModelCallBudget::new(0).err().unwrap())
            }
        })
    }

    // Independent inverse, not the production encoder used to compute its own expectation.
    fn decoded_test_image(encoded: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut bits, mut value) = (0u32, 0u32);
        for byte in encoded.bytes().take_while(|b| *b != b'=') {
            let digit = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => panic!("not base64"),
            };
            value = (value << 6) | u32::from(digit);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((value >> bits) as u8);
                value &= (1 << bits) - 1;
            }
        }
        out
    }

    // Bypassing the reader, trusting metadata, or encoding the wrong bytes breaks this test.
    #[test]
    fn host_ai_attachments_are_authorized_and_digest_checked() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Original one-pixel images, never customer/model data or remote fixtures.
        for (format, mime) in [(image::ImageFormat::Png, "image/png"), (image::ImageFormat::Jpeg, "image/jpeg")] {
            let mut bytes = Vec::new();
            let pixel = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([7, 11, 13])));
            pixel.write_to(&mut std::io::Cursor::new(&mut bytes), format).unwrap();
            let reference = synthetic_artifact(&bytes, mime);
            for (wire, via_go) in [
                (WireProtocol::Chat, false),
                (WireProtocol::Responses, false),
                (WireProtocol::Messages, false),
                (WireProtocol::Messages, true),
                (WireProtocol::Gemini, false),
            ] {
                let (mut deps, binding, mut request, script) = if via_go { normalized_go_messages() } else { normalized_setup(wire) };
                let reads = Arc::new(AtomicUsize::new(0));
                deps.artifacts = authorized_artifacts(reference.clone(), Arc::from(bytes.as_slice()), Arc::clone(&reads));
                request.messages[0].content.push(ContentPart::Image(reference.clone()));
                normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &Arc::new(ModelCallBudget::new(1).unwrap())).unwrap();
                assert_eq!(reads.load(Ordering::SeqCst), 1);
                let recorded = script.recorded.lock().unwrap();
                let body: Value = serde_json::from_slice(recorded[0].body.as_ref().unwrap()).unwrap();
                let encoded = match wire {
                    WireProtocol::Chat | WireProtocol::Responses => {
                        let path = if wire == WireProtocol::Chat { "/messages/1/content/1/image_url/url" } else { "/input/1/content/1/image_url" };
                        body.pointer(path).and_then(Value::as_str).unwrap().strip_prefix(&format!("data:{mime};base64,")).unwrap()
                    }
                    WireProtocol::Messages => {
                        assert_eq!(body["messages"][0]["content"][1]["source"]["media_type"], mime);
                        body.pointer("/messages/0/content/1/source/data").and_then(Value::as_str).unwrap()
                    }
                    WireProtocol::Gemini => {
                        assert_eq!(body["contents"][0]["parts"][1]["inlineData"]["mimeType"], mime);
                        body.pointer("/contents/0/parts/1/inlineData/data").and_then(Value::as_str).unwrap()
                    }
                };
                assert_eq!(decoded_test_image(encoded), bytes, "encode only the exact authorized immutable bytes");
            }
        }
        let bytes: Arc<[u8]> = Arc::from(b"\x89PNG\r\n\x1a\noriginal".as_slice());
        let reference = synthetic_artifact(&bytes, "image/png");
        let (mut deps, binding, mut request, script) = normalized_setup(WireProtocol::Chat);
        let reads = Arc::new(AtomicUsize::new(0));
        request.messages[0].content.push(ContentPart::Image(reference.clone()));
        for returned in [
            Arc::from(b"\x89PNG\r\n\x1a\nmutated!".as_slice()),
            Arc::from(b"\x89PNG\r\n\x1a\nshort".as_slice()),
            Arc::from(b"\xff\xd8\xff\xe0original!".as_slice()),
        ] {
            deps.artifacts = authorized_artifacts(reference.clone(), returned, Arc::clone(&reads));
            assert_eq!(
                prepare_messages(
                    &normalized_session(),
                    &binding,
                    &request,
                    &AiLimits::contract_defaults(),
                    &deps.artifacts,
                    &ProviderState::default()
                )
                .err()
                .unwrap()
                .code(),
                AiErrorCode::InvalidRequest
            );
        }
        // Digest matches the returned JPEG, but the declared PNG type is false.
        let jpeg: Arc<[u8]> = Arc::from(b"\xff\xd8\xff\xe0original!".as_slice());
        let false_type = synthetic_artifact(&jpeg, "image/png");
        request.messages[0].content[1] = ContentPart::Image(false_type.clone());
        deps.artifacts = authorized_artifacts(false_type, jpeg, Arc::clone(&reads));
        assert_eq!(
            prepare_messages(&normalized_session(), &binding, &request, &AiLimits::contract_defaults(), &deps.artifacts, &ProviderState::default())
                .err()
                .unwrap()
                .code(),
            AiErrorCode::InvalidRequest
        );
        deps.artifacts = authorized_artifacts(reference.clone(), Arc::clone(&bytes), Arc::clone(&reads));
        request.messages[0].content[1] = ContentPart::Image(reference.clone());
        let other = SessionKey { runtime_instance_id: "other".into(), ..normalized_session() };
        assert_eq!(
            prepare_messages(&other, &binding, &request, &AiLimits::contract_defaults(), &deps.artifacts, &ProviderState::default())
                .err()
                .unwrap()
                .code(),
            AiErrorCode::InvalidRequest
        );
        for id in ["unauthorized", "file:///tmp/image.png", "https://example/image.png", "/tmp/image.png"] {
            let mut wrong = reference.clone();
            wrong.artifact_id = id.into();
            request.messages[0].content[1] = ContentPart::Image(wrong);
            assert_eq!(
                prepare_messages(
                    &normalized_session(),
                    &binding,
                    &request,
                    &AiLimits::contract_defaults(),
                    &deps.artifacts,
                    &ProviderState::default()
                )
                .err()
                .unwrap()
                .code(),
                AiErrorCode::InvalidRequest
            );
        }
        assert!(script.recorded.lock().unwrap().is_empty());
    }

    // Every unsupported semantic must fail before even authorized artifact storage is read.
    #[test]
    fn host_ai_unsupported_media_and_schemas_fail_before_io() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (mut deps, binding, mut request, script) = normalized_setup(WireProtocol::Gemini);
        let bytes: Arc<[u8]> = Arc::from(b"\x89PNG\r\n\x1a\nsynthetic".as_slice());
        let reference = synthetic_artifact(&bytes, "image/png");
        let reads = Arc::new(AtomicUsize::new(0));
        deps.artifacts = authorized_artifacts(reference.clone(), bytes, Arc::clone(&reads));
        request.messages[0].content.push(ContentPart::Image(reference.clone()));
        let budget = Arc::new(ModelCallBudget::new(1).unwrap());
        for part in [
            ContentPart::Document {
                name: "synthetic.pdf".into(),
                artifact: ArtifactRef { media_type: "application/pdf".into(), ..reference.clone() },
            },
            ContentPart::Image(ArtifactRef { media_type: "image/gif".into(), ..reference.clone() }),
            ContentPart::Image(ArtifactRef { media_type: "image/webp".into(), ..reference.clone() }),
        ] {
            request.messages[0].content.push(part);
            assert_eq!(
                normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &budget).err().unwrap().code(),
                AiErrorCode::UnsupportedCapability
            );
            request.messages[0].content.pop();
        }
        let supported = request.tools[0].input_schema.clone();
        for schema in [
            json!({"type":"object","additionalProperties":false}),
            json!({"type":"object","properties":{"x":{"type":"string","pattern":"x+"}}}),
            json!({"type":"object","properties":{"x":{"type":["string","null"]}}}),
            json!({"type":"object","$ref":"#/$defs/X","$defs":{"X":{"type":"object"}}}),
            json!({"type":"object","properties":{"x":{"type":"number","minimum":1}}}),
            json!({"type":"object","oneOf":[{"type":"object"}]}),
            json!({"type":"object","properties":{"x":{"description":"untyped"}}}),
        ] {
            request.tools[0].input_schema = schema.as_object().unwrap().clone();
            assert_eq!(
                normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &budget).err().unwrap().code(),
                AiErrorCode::UnsupportedCapability
            );
        }
        request.tools[0].input_schema = supported;
        request.options.output_schema = Some(json!({"type":"object"}).as_object().unwrap().clone());
        assert_eq!(
            normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &budget).err().unwrap().code(),
            AiErrorCode::UnsupportedCapability
        );
        request.options.output_schema = None;
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(script.recorded.lock().unwrap().is_empty());
        normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &budget).unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(script.recorded.lock().unwrap().len(), 1);

        // These paths retain every schema constraint, rather than using Gemini's legacy reducer.
        for (wire, via_go) in [
            (WireProtocol::Chat, false),
            (WireProtocol::Responses, false),
            (WireProtocol::Messages, false),
            (WireProtocol::Messages, true),
            (WireProtocol::Gemini, false),
        ] {
            let (deps, binding, mut request, script) = if via_go { normalized_go_messages() } else { normalized_setup(wire) };
            let schema = if wire == WireProtocol::Gemini {
                json!({"type":"object","properties":{"nested":{"type":"array","items":{"type":"string","enum":["甲","乙"]}}},"required":["nested"]})
            } else {
                json!({"type":"object","additionalProperties":false,"properties":{"x":{"type":"number","minimum":1}},"required":["x"]})
            };
            request.tools[0].input_schema = schema.as_object().unwrap().clone();
            normalized_invoke(&deps, &binding, &request, &AiLimits::contract_defaults(), &Arc::new(ModelCallBudget::new(1).unwrap())).unwrap();
            let recorded = script.recorded.lock().unwrap();
            let body: Value = serde_json::from_slice(recorded[0].body.as_ref().unwrap()).unwrap();
            let path = match wire {
                WireProtocol::Chat => "/tools/0/function/parameters",
                WireProtocol::Responses => "/tools/0/parameters",
                WireProtocol::Messages => "/tools/0/input_schema",
                WireProtocol::Gemini => "/tools/0/functionDeclarations/0/parameters",
            };
            assert_eq!(body.pointer(path), Some(&schema));
        }
    }

    fn prepare_code(binding: &ConnectionBinding, request: &GenerateRequest, limits: &AiLimits, reader: &ArtifactReadFn) -> Option<AiErrorCode> {
        prepare_messages(&normalized_session(), binding, request, limits, reader, &ProviderState::default()).err().map(|e| e.code())
    }

    // R1: per-block limits must not admit an unbounded normalized prefix or start I/O.
    #[test]
    fn host_ai_normalized_prefix_budget_is_checked_before_io() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (mut deps, binding, mut request, script) = normalized_setup(WireProtocol::Chat);
        let bytes: Arc<[u8]> = Arc::from(b"\x89PNG\r\n\x1a\nsynthetic".as_slice());
        let reference = synthetic_artifact(&bytes, "image/png");
        let reads = Arc::new(AtomicUsize::new(0));
        deps.artifacts = authorized_artifacts(reference.clone(), bytes, Arc::clone(&reads));
        let budget = Arc::new(ModelCallBudget::new(1).unwrap());
        let mut limits = AiLimits::contract_defaults();
        // Every block is individually legal; raw text alone exceeds the approved 16 MiB.
        request.messages[0].content = vec![ContentPart::Text("é".repeat(128 << 10)); 65];
        request.messages[0].content.push(ContentPart::Image(reference.clone()));
        assert_eq!(normalized_invoke(&deps, &binding, &request, &limits, &budget).err().map(|e| e.code()), Some(AiErrorCode::ResourceExhausted));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(script.recorded.lock().unwrap().is_empty());

        // Empty blocks consume structure even with zero text bytes.
        let empty_parts = (16 << 20) / std::mem::size_of::<ContentPart>() + 1;
        request.messages[0].content = vec![ContentPart::Text(String::new()); empty_parts];
        request.messages[0].content.push(ContentPart::Image(reference.clone()));
        assert_eq!(normalized_invoke(&deps, &binding, &request, &limits, &budget).err().map(|e| e.code()), Some(AiErrorCode::ResourceExhausted));
        limits.continuation_bytes = 1024;
        request.messages[0].content = vec![ContentPart::Text("é".repeat(256)); 2];
        request.messages[0].content.push(ContentPart::Image(reference.clone()));
        assert_eq!(normalized_invoke(&deps, &binding, &request, &limits, &budget).err().map(|e| e.code()), Some(AiErrorCode::ResourceExhausted));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(script.recorded.lock().unwrap().is_empty());

        // The canonical fee is typed structure + raw UTF-8 bytes, not attachment bytes.
        request.messages[0].content = vec![ContentPart::Text("éé".into()), ContentPart::Image(reference)];
        let charge =
            std::mem::size_of::<ModelMessage>() + std::mem::size_of::<Vec<ModelMessage>>() + 2 * std::mem::size_of::<ContentPart>() + 4 + 7 + 9;
        limits.continuation_bytes = charge as u32 - 1;
        assert_eq!(normalized_invoke(&deps, &binding, &request, &limits, &budget).err().map(|e| e.code()), Some(AiErrorCode::ResourceExhausted));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(script.recorded.lock().unwrap().is_empty());
        limits.continuation_bytes = charge as u32;
        normalized_invoke(&deps, &binding, &request, &limits, &budget).unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(script.recorded.lock().unwrap().len(), 1, "all refusals leave the limit=1 physical-attempt budget intact");
        limits.continuation_bytes += 1;
        assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), None);
        use super::super::host_ai::messages::normalized_prefix_charge;
        assert_eq!(normalized_prefix_charge(&request.messages, 0, &limits).unwrap(), charge);
        assert_eq!(normalized_prefix_charge(&request.messages, usize::MAX, &limits).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        assert_eq!(normalized_prefix_charge(&[], usize::MAX, &limits).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        // Appending the assistant in Task6 must use the SAME additive canonical rule.
        let assistant = normalized_message(MessageRole::Assistant, "reply");
        let mut generous = AiLimits::contract_defaults();
        let separate = normalized_prefix_charge(std::slice::from_ref(&assistant), charge, &generous).unwrap();
        request.messages.push(assistant);
        assert_eq!(normalized_prefix_charge(&request.messages, 0, &generous).unwrap(), separate);
        generous.continuation_bytes = separate as u32;
        assert_eq!(prepare_code(&binding, &request, &generous, &deps.artifacts), None);
    }

    // R1 characterization: attachment payload has its own quota, not the prefix quota.
    #[test]
    fn host_ai_normalized_prefix_keeps_native_attachment_expansion_bounded() {
        use super::super::host_ai::messages::{normalized_prefix_charge, provider_payload_bound};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let pixel = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([17, 23, 31])));
        let mut bytes = Vec::new();
        pixel.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        // PNG decoders allow trailing bytes after IEND; the image remains our original pixel.
        bytes.resize(4 << 20, 0);
        assert_eq!(image::load_from_memory(&bytes).unwrap().to_rgb8().get_pixel(0, 0).0, [17, 23, 31]);
        let reference = synthetic_artifact(&bytes, "image/png");
        let bytes: Arc<[u8]> = Arc::from(bytes);
        for (wire, via_go) in [
            (WireProtocol::Chat, false),
            (WireProtocol::Responses, false),
            (WireProtocol::Messages, false),
            (WireProtocol::Messages, true),
            (WireProtocol::Gemini, false),
        ] {
            let (mut deps, binding, mut request, script) = if via_go { normalized_go_messages() } else { normalized_setup(wire) };
            let reads = Arc::new(AtomicUsize::new(0));
            deps.artifacts = authorized_artifacts(reference.clone(), Arc::clone(&bytes), Arc::clone(&reads));
            request.messages[0].content = vec![ContentPart::Image(reference.clone()); 4];
            let mut limits = AiLimits::contract_defaults();
            let charge =
                std::mem::size_of::<ModelMessage>() + std::mem::size_of::<Vec<ModelMessage>>() + 4 * (std::mem::size_of::<ContentPart>() + 7 + 9);
            limits.continuation_bytes = charge as u32;
            assert_eq!(normalized_prefix_charge(&request.messages, 0, &limits).unwrap(), charge);
            let upper = provider_payload_bound(&binding, &request, &limits, &ProviderState::default()).unwrap();
            normalized_invoke(&deps, &binding, &request, &limits, &Arc::new(ModelCallBudget::new(1).unwrap())).unwrap();
            assert_eq!(reads.load(Ordering::SeqCst), 4);
            let recorded = script.recorded.lock().unwrap();
            let encoded = recorded[0].body.as_ref().unwrap();
            assert!(encoded.len() > 22_369_632, "four padded base64 images alone contain exactly 22,369,632 bytes");
            assert!(encoded.len() <= upper, "{wire:?}: actual existing encoder must fit the derived bound");
            let body: Value = serde_json::from_slice(encoded).unwrap();
            for i in 0..4 {
                let data = match wire {
                    WireProtocol::Chat => {
                        body["messages"][1]["content"][i]["image_url"]["url"].as_str().unwrap().strip_prefix("data:image/png;base64,").unwrap()
                    }
                    WireProtocol::Responses => {
                        body["input"][1]["content"][i]["image_url"].as_str().unwrap().strip_prefix("data:image/png;base64,").unwrap()
                    }
                    WireProtocol::Messages => body["messages"][0]["content"][i]["source"]["data"].as_str().unwrap(),
                    WireProtocol::Gemini => body["contents"][0]["parts"][i]["inlineData"]["data"].as_str().unwrap(),
                };
                assert_eq!(data.len(), 5_592_408);
                assert_eq!(decoded_test_image(data).as_slice(), bytes.as_ref());
            }
        }
        // Quotes, controls, multibyte text and arguments exercise JSON escaping/name replay.
        for wire in [WireProtocol::Chat, WireProtocol::Responses, WireProtocol::Messages, WireProtocol::Gemini] {
            let (deps, binding, mut request, script) = normalized_setup(wire);
            request.messages = normalized_round();
            let escaped = "\0\n\"\\é".repeat(4096);
            request.messages[0].content.push(ContentPart::Text(escaped.clone()));
            request.messages[1].tool_calls[0].arguments = json!({"x":escaped}).as_object().unwrap().clone();
            let limits = AiLimits::contract_defaults();
            let upper = provider_payload_bound(&binding, &request, &limits, &ProviderState::default()).unwrap();
            normalized_invoke(&deps, &binding, &request, &limits, &Arc::new(ModelCallBudget::new(1).unwrap())).unwrap();
            assert!(script.recorded.lock().unwrap()[0].body.as_ref().unwrap().len() <= upper);
        }
    }

    // Off-by-one byte/count/depth mistakes or truncation make a boundary fail or silently pass.
    #[test]
    fn host_ai_input_limits_refuse_without_truncation() {
        let (deps, binding, mut request, script) = normalized_setup(WireProtocol::Chat);
        let limits = AiLimits::contract_defaults();
        request.messages = vec![normalized_message(MessageRole::User, "x"); 128];
        assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), None);
        request.messages.push(normalized_message(MessageRole::User, "x"));
        assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.messages = vec![normalized_message(MessageRole::User, &"é".repeat(128 << 10))];
        let call = prepare_messages(&normalized_session(), &binding, &request, &limits, &deps.artifacts, &ProviderState::default()).unwrap();
        assert_eq!(call.history[0]["content"].as_str().unwrap().len(), 256 << 10);
        if let ContentPart::Text(t) = &mut request.messages[0].content[0] {
            t.push('x');
        }
        assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.messages = vec![normalized_message(MessageRole::User, "x")];
        request.tools = (0..64).map(|i| ToolDefinition { name: format!("t{i}"), ..request.tools[0].clone() }).collect();
        assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), None);
        request.tools.push(ToolDefinition { name: "t64".into(), ..request.tools[0].clone() });
        assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.tools.truncate(1);
        // {"x":"..."} has exactly eight bytes of JSON overhead. Include non-ASCII bytes.
        for extra in [0, 1] {
            let value = json!({"x": format!("{}{}", "é".repeat(((64 << 10)-8)/2), "a".repeat(extra))});
            assert_eq!(serde_json::to_vec(&value).unwrap().len(), (64 << 10) + extra);
            request.tools[0].input_schema = value.as_object().unwrap().clone();
            assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), (extra == 1).then_some(AiErrorCode::ResourceExhausted));
            request.tools[0].input_schema = json!({"type":"object"}).as_object().unwrap().clone();
            request.messages = normalized_round();
            request.messages[1].tool_calls[0].arguments = value.as_object().unwrap().clone();
            assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), (extra == 1).then_some(AiErrorCode::ResourceExhausted));
            request.messages = vec![normalized_message(MessageRole::User, "x")];
        }
        for depth in [64, 65] {
            let mut value = json!(0);
            for _ in 0..depth {
                value = json!({"x":value});
            }
            request.tools[0].input_schema = value.as_object().unwrap().clone();
            assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), (depth == 65).then_some(AiErrorCode::ResourceExhausted));
            request.tools[0].input_schema = json!({"type":"object"}).as_object().unwrap().clone();
            request.messages = normalized_round();
            request.messages[1].tool_calls[0].arguments = value.as_object().unwrap().clone();
            assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), (depth == 65).then_some(AiErrorCode::ResourceExhausted));
            request.messages = vec![normalized_message(MessageRole::User, "x")];
        }
        for count in [32, 33] {
            let mut assistant = normalized_message(MessageRole::Assistant, "calls");
            assistant.tool_calls =
                (0..count).map(|i| ToolCall { id: format!("c{i}"), name: "measure".into(), arguments: Default::default() }).collect();
            request.messages = vec![normalized_message(MessageRole::User, "x"), assistant];
            request
                .messages
                .extend((0..count).map(|i| ModelMessage { tool_call_id: Some(format!("c{i}")), ..normalized_message(MessageRole::Tool, "ok") }));
            assert_eq!(prepare_code(&binding, &request, &limits, &deps.artifacts), (count == 33).then_some(AiErrorCode::ResourceExhausted));
        }
        request.messages = normalized_round();
        let mut reduced = limits.clone();
        reduced.tool_calls = 1;
        assert_eq!(prepare_code(&binding, &request, &reduced, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.messages = vec![normalized_message(MessageRole::User, "x"); 2];
        reduced = limits.clone();
        reduced.model_messages = 1;
        assert_eq!(prepare_code(&binding, &request, &reduced, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.messages.truncate(1);
        request.tools.push(ToolDefinition { name: "another".into(), ..request.tools[0].clone() });
        reduced = limits.clone();
        reduced.tool_definitions = 1;
        assert_eq!(prepare_code(&binding, &request, &reduced, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.tools.truncate(1);
        reduced = limits.clone();
        reduced.tool_json_bytes = 1;
        assert_eq!(prepare_code(&binding, &request, &reduced, &deps.artifacts), Some(AiErrorCode::ResourceExhausted));
        request.tools[0].input_schema = json!({"type":"object"}).as_object().unwrap().clone();
        let mut four_mib = vec![0u8; 4 << 20];
        four_mib[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let reference = synthetic_artifact(&four_mib, "image/png");
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = authorized_artifacts(reference.clone(), Arc::from(four_mib), Arc::clone(&reads));
        request.messages[0].content = vec![ContentPart::Image(reference.clone()); 4];
        assert_eq!(prepare_code(&binding, &request, &limits, &reader), None, "exact 16 MiB aggregate is not an IPC frame");
        request.messages[0].content.push(ContentPart::Image(reference.clone()));
        assert_eq!(prepare_code(&binding, &request, &limits, &reader), Some(AiErrorCode::ResourceExhausted));
        request.messages[0].content = vec![ContentPart::Image(ArtifactRef { size_bytes: (4 << 20) + 1, ..reference.clone() })];
        assert_eq!(prepare_code(&binding, &request, &limits, &reader), Some(AiErrorCode::ResourceExhausted));
        request.messages[0].content = vec![ContentPart::Image(reference); 4];
        let mut lower = limits.clone();
        lower.attachment_total_bytes -= 1;
        assert_eq!(prepare_code(&binding, &request, &lower, &reader), Some(AiErrorCode::ResourceExhausted));
        lower = limits.clone();
        lower.attachments = 3;
        assert_eq!(prepare_code(&binding, &request, &lower, &reader), Some(AiErrorCode::ResourceExhausted));
        lower = limits.clone();
        lower.attachment_bytes -= 1;
        assert_eq!(prepare_code(&binding, &request, &lower, &reader), Some(AiErrorCode::ResourceExhausted));
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 4, "metadata refusals occur before reads");
        request.messages = vec![normalized_message(MessageRole::User, "abcd")];
        lower = limits.clone();
        lower.text_bytes = 3;
        assert_eq!(prepare_code(&binding, &request, &lower, &reader), Some(AiErrorCode::ResourceExhausted));
        assert!(script.recorded.lock().unwrap().is_empty());
    }

    // I1: a JSON Pointer accepting an object key "0" must not impersonate a provider array.
    #[test]
    fn host_ai_reply_is_strict_about_containers_and_roles() {
        for (wire, body) in [
            (WireProtocol::Chat, json!({"choices": {"0": {"message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}}})),
            (
                WireProtocol::Gemini,
                json!({"candidates": {"0": {"content": {"role": "model", "parts": [{"text": "answer"}]}, "finishReason": "STOP"}}}),
            ),
            (WireProtocol::Gemini, json!({"candidates": {"0": {"finishReason": "SAFETY"}}})),
            (WireProtocol::Messages, json!({"content": {"0": {"type": "text", "text": "answer"}}, "stop_reason": "end_turn"})),
            (
                WireProtocol::Responses,
                json!({"output": {"0": {"type": "message", "content": [{"type": "output_text", "text": "answer"}]}}, "status": "completed"}),
            ),
            (WireProtocol::Gemini, json!({"candidates": [{"content": {"parts": {"0": {"text": "answer"}}}, "finishReason": "STOP"}]})),
        ] {
            assert_eq!(strict_invoke(wire, body.clone()).err().map(|e| e.code()), Some(AiErrorCode::MalformedResponse), "{wire:?}: {body}");
        }
        for (wire, role_path, expected_role) in [
            (WireProtocol::Chat, "/choices/0/message/role", "assistant"),
            (WireProtocol::Messages, "/role", "assistant"),
            (WireProtocol::Responses, "/output/0/role", "assistant"),
            (WireProtocol::Gemini, "/candidates/0/content/role", "model"),
        ] {
            for role in [json!("user"), Value::Null, json!(7), json!({})] {
                let mut body = strict_answer(wire);
                if wire == WireProtocol::Messages {
                    body["role"] = role;
                } else {
                    *body.pointer_mut(role_path).unwrap() = role;
                }
                assert_eq!(strict_invoke(wire, body).err().map(|e| e.code()), Some(AiErrorCode::MalformedResponse), "{wire:?}");
            }
            let mut body = strict_answer(wire);
            if wire == WireProtocol::Messages {
                body["role"] = json!(expected_role);
            } else {
                *body.pointer_mut(role_path).unwrap() = json!(expected_role);
            }
            assert_eq!(strict_invoke(wire, body).unwrap().text, [(false, "answer".into())]);
        }
        for wire in [WireProtocol::Chat, WireProtocol::Gemini] {
            let key = if wire == WireProtocol::Chat { "choices" } else { "candidates" };
            for invalid in [Value::Null, json!("not an array"), json!([null]), json!([[]])] {
                let mut body = strict_answer(wire);
                body[key] = invalid;
                assert_eq!(strict_invoke(wire, body).err().map(|e| e.code()), Some(AiErrorCode::MalformedResponse));
            }
        }
    }

    // I2: mutually exclusive part payloads must not disappear from the declared call count.
    #[test]
    fn host_ai_reply_is_strict_about_gemini_part_payloads() {
        for count in [1, 33] {
            for text in [json!("answer"), json!(""), Value::Null, json!(7)] {
                let parts = vec![json!({"text": text, "functionCall": {"name": "measure", "args": {}}}); count];
                let body = json!({"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}]});
                assert_eq!(
                    strict_invoke(WireProtocol::Gemini, body).err().map(|e| e.code()),
                    Some(AiErrorCode::MalformedResponse),
                    "{count} mixed parts"
                );
            }
        }
        assert_eq!(strict_invoke(WireProtocol::Gemini, strict_call_answer(WireProtocol::Gemini, 32, json!({}))).unwrap().calls.len(), 32);
        assert_eq!(
            strict_invoke(WireProtocol::Gemini, strict_call_answer(WireProtocol::Gemini, 33, json!({}))).err().unwrap().code(),
            AiErrorCode::ResourceExhausted
        );
        let texts = vec![json!({"text": "answer"}); 33];
        let body = json!({"candidates": [{"content": {"parts": texts}, "finishReason": "STOP"}]});
        let reply = strict_invoke(WireProtocol::Gemini, body).unwrap();
        assert_eq!(reply.text.len(), 33);
        assert!(reply.calls.is_empty());
    }

    // I3: decoding this argument creates the actual key; clearing the call afterwards is too late.
    // Malformed/oversize/deep non-executable arguments must be skipped, not decoded then discarded.
    #[test]
    fn host_ai_reply_is_strict_before_decoding_cut_calls() {
        use super::super::host_ai::FinishReason;
        let escaped: String = KEY_A.bytes().map(|b| format!("\\u{b:04x}")).collect();
        let oversized = format!("{{\"ignored-call-marker\":\"{escaped}{}\"}}", "x".repeat(65536));
        let deep = format!("{}{{\"ignored-call-marker\":\"{escaped}\"}}{}", "{\"x\":".repeat(80), "}".repeat(80));
        for wire in [WireProtocol::Chat, WireProtocol::Responses, WireProtocol::Messages, WireProtocol::Gemini] {
            for refusal in [false, true] {
                for args in
                    [json!(oversized), json!(deep), json!("{incomplete ignored-call-marker"), json!({"ignored-call-marker": "x".repeat(65536)})]
                {
                    let mut body = strict_call_answer(wire, 1, args);
                    match wire {
                        WireProtocol::Chat => body["choices"][0]["finish_reason"] = json!(if refusal { "content_filter" } else { "length" }),
                        WireProtocol::Messages => body["stop_reason"] = json!(if refusal { "refusal" } else { "max_tokens" }),
                        WireProtocol::Responses => {
                            body["status"] = json!("incomplete");
                            body["incomplete_details"] = json!({"reason": if refusal { "content_filter" } else { "max_output_tokens" }});
                        }
                        WireProtocol::Gemini => {
                            body["candidates"][0]["finishReason"] = json!(if refusal { "SAFETY" } else { "MAX_TOKENS" });
                            body["candidates"][0]["content"]["parts"][0]["thoughtSignature"] = json!("ignored-call-marker-signature");
                        }
                    }
                    let reply = strict_invoke(wire, body);
                    assert!(reply.is_ok(), "{wire:?}, refusal={refusal}: {:?}", reply.err());
                    let reply = reply.unwrap();
                    assert_eq!(reply.finish_reason, if refusal { FinishReason::Refusal } else { FinishReason::Length });
                    assert!(reply.calls.is_empty());
                    assert!(reply.private.gemini_signatures.is_empty());
                    // Task6 retains provider-native replay separately from the legacy
                    // assistant projection; neither may retain discarded declarations.
                    let native = match wire {
                        WireProtocol::Chat => json!({"role":"assistant","content":null}),
                        WireProtocol::Responses => json!([]),
                        WireProtocol::Messages => json!({"role":"assistant","content":[]}),
                        WireProtocol::Gemini => json!({"role":"model","parts":[]}),
                    };
                    assert_eq!(reply.private.native_assistants, vec![native]);
                    assert!(!reply.private.native_assistants[0].to_string().contains("ignored-call-marker"));
                    assert!(!reply.assistant.to_string().contains("ignored-call-marker"));
                    assert!(reply.assistant.get("tool_calls").is_none());
                }
            }
            // The same executable declarations retain the original resource/argument classes.
            assert_eq!(
                strict_invoke(wire, strict_call_answer(wire, 1, json!({"text": "x".repeat(65536)}))).err().unwrap().code(),
                AiErrorCode::ResourceExhausted
            );
            assert_eq!(strict_invoke(wire, strict_call_answer(wire, 1, json!("{broken"))).err().unwrap().code(), AiErrorCode::MalformedResponse);
            for (count, expected) in [(128, None), (129, Some(AiErrorCode::ResourceExhausted))] {
                let mut body = strict_call_answer(wire, count, json!("{incomplete"));
                match wire {
                    WireProtocol::Chat => body["choices"][0]["finish_reason"] = json!("length"),
                    WireProtocol::Messages => body["stop_reason"] = json!("max_tokens"),
                    WireProtocol::Responses => {
                        body["status"] = json!("incomplete");
                        body["incomplete_details"] = json!({"reason": "max_output_tokens"});
                    }
                    WireProtocol::Gemini => body["candidates"][0]["finishReason"] = json!("MAX_TOKENS"),
                }
                assert_eq!(strict_invoke(wire, body.clone()).err().map(|e| e.code()), expected);
                body["overall-resource-marker"] = json!("x".repeat(262145));
                assert_eq!(strict_invoke(wire, body).err().unwrap().code(), AiErrorCode::ResourceExhausted);
            }
            let mut nested = json!({});
            for _ in 0..65 {
                nested = json!({"x": nested});
            }
            let mut body = strict_call_answer(wire, 1, nested);
            match wire {
                WireProtocol::Chat => body["choices"][0]["finish_reason"] = json!("length"),
                WireProtocol::Messages => body["stop_reason"] = json!("max_tokens"),
                WireProtocol::Responses => {
                    body["status"] = json!("incomplete");
                    body["incomplete_details"] = json!({"reason": "max_output_tokens"});
                }
                WireProtocol::Gemini => body["candidates"][0]["finishReason"] = json!("MAX_TOKENS"),
            }
            assert_eq!(strict_invoke(wire, body).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        }
    }

    // Catches null-success, invented usage totals, permissive argument repair and unbounded decoding.
    #[test]
    fn host_ai_reply_is_strict_and_usage_is_not_invented() {
        use super::super::host_ai::Usage;
        for wire in [WireProtocol::Chat, WireProtocol::Messages, WireProtocol::Responses, WireProtocol::Gemini] {
            for bytes in [b"{".to_vec(), b"<html>ok</html>".to_vec(), b"{}".to_vec(), b"{\"data\": []}".to_vec()] {
                assert_eq!(strict_invoke_bytes(wire, bytes).err().unwrap().code(), AiErrorCode::MalformedResponse, "{wire:?}");
            }
            let wrong = match wire {
                WireProtocol::Chat => json!({"choices": [{"message": {"content": 7}}]}),
                WireProtocol::Messages => json!({"content": [{"type": "text", "text": 7}]}),
                WireProtocol::Responses => json!({"output": [{"type": "message", "content": "answer"}]}),
                WireProtocol::Gemini => json!({"candidates": [{"content": {"parts": [{"text": 7}]}}]}),
            };
            assert_eq!(strict_invoke(wire, wrong).err().unwrap().code(), AiErrorCode::MalformedResponse);
            let finish_path = match wire {
                WireProtocol::Chat => "/choices/0/finish_reason",
                WireProtocol::Messages => "/stop_reason",
                WireProtocol::Responses => "/status",
                WireProtocol::Gemini => "/candidates/0/finishReason",
            };
            let mut wrong_finish = strict_answer(wire);
            *wrong_finish.pointer_mut(finish_path).unwrap() = json!(7);
            assert_eq!(
                strict_invoke(wire, wrong_finish).err().map(|e| e.code()),
                Some(AiErrorCode::MalformedResponse),
                "{wire:?}: finish metadata must be a string or null"
            );
            let mut empty_message = strict_answer(wire);
            match wire {
                WireProtocol::Chat => empty_message["choices"][0]["message"] = json!({}),
                WireProtocol::Messages => empty_message["content"] = json!([{}]),
                WireProtocol::Responses => {
                    empty_message["output"][0]["content"] = json!([]);
                    empty_message.as_object_mut().unwrap().remove("status");
                }
                WireProtocol::Gemini => empty_message["candidates"][0]["content"]["parts"] = json!([{}]),
            }
            assert_eq!(
                strict_invoke(wire, empty_message).err().map(|e| e.code()),
                Some(AiErrorCode::MalformedResponse),
                "{wire:?}: empty unrelated message"
            );
            let (usage_key, input, output, total) = match wire {
                WireProtocol::Chat => ("usage", "prompt_tokens", "completion_tokens", "total_tokens"),
                WireProtocol::Messages | WireProtocol::Responses => ("usage", "input_tokens", "output_tokens", "total_tokens"),
                WireProtocol::Gemini => ("usageMetadata", "promptTokenCount", "candidatesTokenCount", "totalTokenCount"),
            };
            assert_eq!(strict_invoke(wire, strict_answer(wire)).unwrap().usage, None);
            for absent in [Value::Null, json!({})] {
                let mut body = strict_answer(wire);
                body[usage_key] = absent;
                let usage = strict_invoke(wire, body).unwrap().usage;
                assert!(usage.is_none() || usage == Some(Usage { input_tokens: None, output_tokens: None, total_tokens: None }));
            }
            let mut body = strict_answer(wire);
            body[usage_key] = json!({input: 3, output: 5, total: null});
            assert_eq!(strict_invoke(wire, body).unwrap().usage, Some(Usage { input_tokens: Some(3), output_tokens: Some(5), total_tokens: None }));
            let mut body = strict_answer(wire);
            body[usage_key] = json!({total: 11});
            assert_eq!(strict_invoke(wire, body).unwrap().usage, Some(Usage { input_tokens: None, output_tokens: None, total_tokens: Some(11) }));
            let mut body = strict_answer(wire);
            body[usage_key] = json!({input: 0, output: 2147483647_u32, total: 2147483647_u32});
            assert_eq!(
                strict_invoke(wire, body).unwrap().usage,
                Some(Usage { input_tokens: Some(0), output_tokens: Some(2147483647), total_tokens: Some(2147483647) })
            );
            for invalid in [json!([]), json!(true)] {
                let mut body = strict_answer(wire);
                body[usage_key] = invalid;
                assert_eq!(strict_invoke(wire, body).err().unwrap().code(), AiErrorCode::MalformedResponse);
            }
            for invalid in [json!(-1), json!(2147483648_u64), json!(1.5), json!(1.0), json!("7"), json!(true)] {
                let mut body = strict_answer(wire);
                body[usage_key] = json!({input: invalid});
                assert_eq!(strict_invoke(wire, body).err().unwrap().code(), AiErrorCode::MalformedResponse, "{wire:?}");
            }
            let args = if matches!(wire, WireProtocol::Chat | WireProtocol::Responses) { json!("{}") } else { json!({}) };
            assert_eq!(strict_invoke(wire, strict_call_answer(wire, 32, args.clone())).unwrap().calls.len(), 32);
            assert_eq!(strict_invoke(wire, strict_call_answer(wire, 33, args)).err().unwrap().code(), AiErrorCode::ResourceExhausted);
            for bad in [Value::Null, json!(""), json!("{broken"), json!([])] {
                assert_eq!(strict_invoke(wire, strict_call_answer(wire, 1, bad)).err().unwrap().code(), AiErrorCode::MalformedResponse);
            }
            let oversized = strict_call_answer(wire, 1, json!({"text": "x".repeat(65536)}));
            assert_eq!(strict_invoke(wire, oversized).err().unwrap().code(), AiErrorCode::ResourceExhausted);
            let mut body = strict_call_answer(wire, 1, json!("{truncated"));
            match wire {
                WireProtocol::Chat => body["choices"][0]["finish_reason"] = json!("length"),
                WireProtocol::Messages => body["stop_reason"] = json!("max_tokens"),
                WireProtocol::Responses => {
                    body["status"] = json!("incomplete");
                    body["incomplete_details"] = json!({"reason": "max_output_tokens"});
                }
                WireProtocol::Gemini => body["candidates"][0]["finishReason"] = json!("MAX_TOKENS"),
            }
            let reply = strict_invoke(wire, body).unwrap();
            assert!(reply.calls.is_empty());
            assert_eq!(reply.finish_reason, super::super::host_ai::FinishReason::Length);
            let mut body = strict_answer(wire);
            body["unrelated"] = json!("x".repeat((16 << 20) + 1));
            assert_eq!(strict_invoke(wire, body).err().unwrap().code(), AiErrorCode::ResourceExhausted);
            let mut body = strict_answer(wire);
            body["unrelated"] = json!(vec![Value::Null; 70_000]);
            assert_eq!(strict_invoke(wire, body).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        }
    }

    // Catches unknown mapped to Stop/ToolCalls, missing provider failure/refusal/length cases.
    #[test]
    fn host_ai_unknown_finish_is_not_stop() {
        use super::super::host_ai::FinishReason::*;
        for (wire, field, values) in [
            (
                WireProtocol::Chat,
                "/choices/0/finish_reason",
                vec![("stop", Stop), ("tool_calls", ToolCalls), ("length", Length), ("content_filter", Refusal), ("future", Unknown)],
            ),
            (
                WireProtocol::Messages,
                "/stop_reason",
                vec![
                    ("end_turn", Stop),
                    ("stop_sequence", Stop),
                    ("tool_use", ToolCalls),
                    ("max_tokens", Length),
                    ("refusal", Refusal),
                    ("pause_turn", Unknown),
                ],
            ),
            (WireProtocol::Responses, "/status", vec![("completed", Stop), ("future", Unknown)]),
            (
                WireProtocol::Gemini,
                "/candidates/0/finishReason",
                vec![("STOP", Stop), ("MAX_TOKENS", Length), ("SAFETY", Refusal), ("RECITATION", Refusal), ("future", Unknown)],
            ),
        ] {
            for (value, expected) in values {
                let mut body = strict_answer(wire);
                *body.pointer_mut(field).unwrap() = json!(value);
                assert_eq!(strict_invoke(wire, body).unwrap().finish_reason, expected, "{wire:?} {value}");
            }
            let mut body =
                strict_call_answer(wire, 1, if matches!(wire, WireProtocol::Chat | WireProtocol::Responses) { json!("{}") } else { json!({}) });
            *body.pointer_mut(field).unwrap() = json!("future");
            assert_eq!(strict_invoke(wire, body).unwrap().finish_reason, Unknown);
        }
        for body in [json!({"status": "failed", "output": [], "error": {"message": KEY_A}}), json!({"status": "cancelled", "output": []})] {
            assert_eq!(strict_invoke(WireProtocol::Responses, body).err().unwrap().code(), AiErrorCode::ModelError);
        }
        let refused = json!({"output": [{"type": "message", "content": [{"type": "refusal", "refusal": "cannot answer"}]}], "status": "completed"});
        assert_eq!(strict_invoke(WireProtocol::Responses, refused).unwrap().finish_reason, Refusal);
        let blocked = json!({"promptFeedback": {"blockReason": "SAFETY"}});
        assert_eq!(strict_invoke(WireProtocol::Gemini, blocked).unwrap().finish_reason, Refusal);
        assert_eq!(reply(Protocol::OpenAi, &json!({"choices": [{"message": {"content": "answer"}, "finish_reason": "future"}]})).stop, "");
    }

    // Catches ignored options and attempt debits/HTTP before invalid-option rejection.
    #[test]
    fn host_ai_options_are_applied_or_rejected() {
        use super::super::host_ai::provider::{capabilities, validate_options};
        use super::super::host_ai::{Delivery, GenerateOptions};
        let deps = primitive_deps(Arc::new(|_| panic!("option validation performs HTTP")));
        let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
        let caps = capabilities(&binding);
        assert_eq!(caps.delivery, Delivery::Buffered);
        assert!(caps.model_generate && caps.tools && caps.images && caps.model_selection && caps.usage);
        assert!(!caps.agent_run && !caps.documents && !caps.structured_output);
        let base = GenerateOptions {
            delivery: Delivery::Buffered,
            timeout_seconds: 60,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            output_schema: None,
        };
        validate_options(&binding, &base).unwrap(); // inherits frozen Chat high, not the current picker
        for options in [
            GenerateOptions { delivery: Delivery::Streaming, ..base.clone() },
            GenerateOptions { output_schema: Some(Default::default()), ..base.clone() },
            GenerateOptions { timeout_seconds: 0, ..base.clone() },
            GenerateOptions { timeout_seconds: 601, ..base.clone() },
            GenerateOptions { max_output_tokens: Some(0), ..base.clone() },
            GenerateOptions { temperature: Some(f64::INFINITY), ..base.clone() },
            GenerateOptions { reasoning_effort: Some("custom".into()), ..base.clone() },
        ] {
            assert!(validate_options(&binding, &options).is_err());
        }
        for (model, expected) in [("gpt-test", WireProtocol::Responses), ("qwen-test", WireProtocol::Messages), ("gemini-test", WireProtocol::Gemini)]
        {
            let config = AgentConfig { base_url: "https://opencode.ai/inference/go/v1".into(), ..original_connection() };
            deps.shared.connections().replace(&[config]).unwrap();
            let binding = deps.shared.connections().bind("api-original-7", model).unwrap();
            assert_eq!(go_protocol(model), expected);
            assert_eq!(validate_options(&binding, &base).unwrap_err().code(), AiErrorCode::UnsupportedCapability);
        }
        for wire in [WireProtocol::Chat, WireProtocol::Messages, WireProtocol::Responses, WireProtocol::Gemini] {
            let script = ScriptedHttp::new(vec![(200, strict_answer(wire))]);
            let deps = primitive_deps(script.http());
            let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
            let budget = Arc::new(ModelCallBudget::new(1).unwrap());
            for (tokens, temperature, effort) in [
                (Some(0), None, None),
                (Some(131073), None, None),
                (None, Some(f64::NAN), None),
                (None, Some(-0.1), None),
                (None, Some(2.1), None),
                (None, None, Some(if wire == WireProtocol::Chat { "bad\n" } else { "low" })),
            ] {
                let mut call = primitive_call(wire);
                call.max_output_tokens = tokens;
                call.temperature = temperature;
                call.reasoning_effort = effort.map(str::to_string);
                call.model_budget = Some(Arc::clone(&budget));
                assert!(invoke_once(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).is_err(), "{wire:?}");
            }
            assert_eq!(script.recorded.lock().unwrap().len(), 0);
            let mut call = primitive_call(wire);
            call.max_output_tokens = Some(17);
            call.temperature = Some(0.5);
            call.model_budget = Some(budget);
            if wire == WireProtocol::Chat {
                call.reasoning_effort = Some("low".into());
            }
            settled_invoke(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
            let recorded = script.recorded.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            let body: Value = serde_json::from_slice(recorded[0].body.as_ref().unwrap()).unwrap();
            let (token_path, temp_path) = match wire {
                WireProtocol::Chat | WireProtocol::Messages => ("/max_tokens", "/temperature"),
                WireProtocol::Responses => ("/max_output_tokens", "/temperature"),
                WireProtocol::Gemini => ("/generationConfig/maxOutputTokens", "/generationConfig/temperature"),
            };
            assert_eq!(body.pointer(token_path), Some(&json!(17)));
            assert_eq!(body.pointer(temp_path), Some(&json!(0.5)));
            if wire == WireProtocol::Chat {
                assert_eq!(body["reasoning_effort"], "low");
            }
        }
        // The shared primitive is also prepared for legacy direct chat. Its custom Chat effort
        // is encodable, even though strict HostAi validate_options rejects it above.
        let script = ScriptedHttp::new(vec![(200, chat_answer())]);
        let deps = primitive_deps(script.http());
        let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
        let mut legacy = primitive_call(WireProtocol::Chat);
        legacy.reasoning_effort = Some("max".into());
        assert!(settled_invoke(&deps, &binding, legacy, &Arc::new(CallControl::new()), Duration::from_secs(60)).is_ok());
        let recorded = script.recorded.lock().unwrap();
        let body: Value = serde_json::from_slice(recorded[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["reasoning_effort"], "max");
    }

    // Catches arbitrary transport/provider/keychain diagnostic forwarding and request Debug leaks.
    #[test]
    fn host_ai_provider_errors_and_debug_never_echo_credentials() {
        use super::super::host_ai::AiErrorDetails;
        let request = HttpRequest {
            method: "POST",
            url: format!("https://user:{KEY_A}@example.invalid/private?key={KEY_A}"),
            headers: vec![("Authorization".into(), format!("Bearer {KEY_A}")), ("X-Custom".into(), "custom-private-value".into())],
            body: Some(KEY_A.as_bytes().to_vec()),
            timeout_s: 60,
        };
        let debug = format!("{request:?}");
        for private in [KEY_A, "Bearer ", "custom-private-value", "example.invalid", "private?", "116, 97, 115, 107"] {
            assert!(!debug.contains(private), "{private} leaked");
        }
        for signal in [format!("network {KEY_A}"), format!("timeout: global {KEY_A}"), "timeout: globally".into()] {
            let deps = primitive_deps(Arc::new(move |_| Err(signal.clone())));
            let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
            let e = settled_invoke(&deps, &binding, primitive_call(WireProtocol::Chat), &Arc::new(CallControl::new()), Duration::from_secs(60))
                .err()
                .unwrap();
            assert_eq!(e.code(), AiErrorCode::TransportError);
            assert!(!format!("{e:?} {}", e.message()).contains(KEY_A));
        }
        for (status, body, code) in [
            (401, json!({"error": KEY_A}), AiErrorCode::AuthRequired),
            (403, json!({"error": {"type": "authentication_error", "message": KEY_A}}), AiErrorCode::AuthRequired),
            (403, json!({"error": {"message": KEY_A}}), AiErrorCode::ModelError),
            (500, json!({"error": {"message": KEY_A}}), AiErrorCode::ModelError),
        ] {
            let script = ScriptedHttp::new(vec![(status, body.clone())]);
            let deps = primitive_deps(script.http());
            let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
            let e = settled_invoke(&deps, &binding, primitive_call(WireProtocol::Chat), &Arc::new(CallControl::new()), Duration::from_secs(60))
                .err()
                .unwrap();
            assert_eq!(e.code(), code);
            assert!(
                matches!(e.details(), AiErrorDetails::Connection { connection_id, http_status: Some(s) } if connection_id == "api-original-7" && *s == status)
            );
            assert!(!format!("{e:?} {} {}", e.message(), http_error(status, &body)).contains(KEY_A));
        }
        assert!(!request_error(format!("URL/header/body {KEY_A}"), Duration::ZERO, 60).contains(KEY_A));
        assert!(!request_error("timeout: global secret".into(), Duration::ZERO, 60).contains("secret"));
        assert_ne!(request_error("timeout: global secret".into(), Duration::ZERO, 60), timed_out(60));
        let mut deps = primitive_deps(Arc::new(|_| panic!("keychain failure must not dispatch")));
        let secrets = Arc::new(CurrentSecrets::new());
        *secrets.value.lock().unwrap() = Err(format!("keychain {KEY_A} Bearer {KEY_B}"));
        deps.secrets = Some(secrets);
        let binding = deps.shared.connections().bind("api-original-7", "chosen").unwrap();
        let e = settled_invoke(&deps, &binding, primitive_call(WireProtocol::Chat), &Arc::new(CallControl::new()), Duration::from_secs(60))
            .err()
            .unwrap();
        assert_eq!(e.code(), AiErrorCode::InternalError);
        assert!(matches!(e.details(), AiErrorDetails::Operation { operation: "read_secret" }));
        let diagnostics = format!("{e:?} {}", e.message());
        assert!(!diagnostics.contains(KEY_A) && !diagnostics.contains(KEY_B) && !diagnostics.contains("Bearer "));
        assert!(!e.message().is_empty() && e.message().len() <= 2048);
        let page: HttpFn = Arc::new(|_| Ok((200, b"{}".to_vec())));
        assert!(
            test_anthropic_endpoint(&page, "https://models.example", None, "chosen").is_err(),
            "an unrelated 200 is not a successful connection test"
        );
    }

    // Catches wrong endpoint/header/body, a hidden listing, and discarded private proposal data.
    #[test]
    fn host_ai_primitive_makes_one_http_attempt() {
        for (kind, base, wire, model, expected_url, answer) in [
            (Kind::Api, "https://models.example/v1", WireProtocol::Chat, "chosen", "https://models.example/v1/chat/completions", chat_answer()),
            (
                Kind::Api,
                "https://opencode.ai/zen/go/v1",
                WireProtocol::Chat,
                "chosen",
                "https://opencode.ai/inference/go/openai/v1/chat/completions",
                chat_answer(),
            ),
            (
                Kind::Anthropic,
                "https://models.example",
                WireProtocol::Messages,
                "chosen",
                "https://models.example/v1/messages",
                json!({"content": [{"type": "text", "text": "answer"}], "stop_reason": "end_turn"}),
            ),
            (
                Kind::Api,
                "https://opencode.ai/zen/go/v1",
                WireProtocol::Responses,
                "gpt-chosen",
                "https://opencode.ai/inference/go/openai/v1/responses",
                json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "answer"}]}]}),
            ),
            (
                Kind::Api,
                "https://opencode.ai/zen/go/v1",
                WireProtocol::Messages,
                "claude-chosen",
                "https://opencode.ai/inference/go/anthropic/v1/messages",
                json!({"content": [{"type": "text", "text": "answer"}]}),
            ),
            (
                Kind::Api,
                "https://opencode.ai/zen/go/v1",
                WireProtocol::Gemini,
                "gemini-chosen",
                "https://opencode.ai/inference/go/google/v1beta/models/gemini-chosen:generateContent",
                json!({"candidates": [{"content": {"parts": [{"text": "answer"}]}}]}),
            ),
        ] {
            let script = ScriptedHttp::new(vec![(200, answer)]);
            let deps = primitive_deps(script.http());
            deps.shared.connections().replace(&[AgentConfig { kind, base_url: base.into(), ..original_connection() }]).unwrap();
            let binding = deps.shared.connections().bind("api-original-7", model).unwrap();
            let control = Arc::new(CallControl::new());
            let mut call = primitive_call(wire);
            call.history = vec![json!({"role": "user", "content": [
                {"type": "text", "text": "hello"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
            ]})];
            if kind == Kind::Anthropic {
                call.tools = vec![json!({"name": "execute", "input_schema": {"type": "object"}})];
                call.history = vec![json!({"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}
                ]})];
            }
            let proposal: ProviderReply = invoke_once(&deps, &binding, call, &control, Duration::from_secs(60)).unwrap();
            assert_eq!(proposal.text, [(false, "answer".into())]);
            assert!(proposal.calls.is_empty());
            assert_eq!(proposal.assistant["role"], "assistant");
            assert_eq!(proposal.private.routing_session_id, "stable-route-7");
            assert!(proposal.private.gemini_signatures.is_empty());
            assert!(proposal.private.wire_to_provider_tool_ids.is_empty());
            assert!(proposal.usage.is_none());
            // Primitive returns a proposal; it must not spend the caller's publication right.
            assert_eq!(control.publish(|| Ok(7)).unwrap(), 7);
            let recorded = script.recorded.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            let req = &recorded[0];
            assert_eq!(req.method, "POST");
            assert_eq!(req.url, expected_url);
            assert_eq!(req.timeout_s, 60);
            let body: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
            assert_eq!(body.get("model").and_then(Value::as_str), (wire != WireProtocol::Gemini).then_some(model));
            let (image_path, image) = match wire {
                WireProtocol::Chat => ("/messages/1/content/1/image_url/url", "data:image/png;base64,iVBORw0KGgo="),
                WireProtocol::Responses => ("/input/1/content/1/image_url", "data:image/png;base64,iVBORw0KGgo="),
                WireProtocol::Messages => ("/messages/0/content/1/source/data", "iVBORw0KGgo="),
                WireProtocol::Gemini => ("/contents/0/parts/1/inlineData/data", "iVBORw0KGgo="),
            };
            assert_eq!(body.pointer(image_path).and_then(Value::as_str), Some(image));
            let header = |name: &str| req.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
            assert_eq!(
                header(if kind == Kind::Anthropic { "x-api-key" } else { "Authorization" }),
                Some(if kind == Kind::Anthropic { KEY_A.to_string() } else { format!("Bearer {KEY_A}") }).as_deref()
            );
            assert_eq!(header("x-opencode-session"), base.contains("opencode.ai").then_some("stable-route-7"));
        }
    }

    // Catches importing the old fallback/reasoning-none scheduling into the primitive.
    #[test]
    fn host_ai_primitive_never_falls_back_or_retries() {
        for (base, wire, status, body, code, url) in [
            (
                "https://models.example/v1",
                WireProtocol::Chat,
                400,
                json!({"error": {"message": "tools need reasoning_effort none"}}),
                AiErrorCode::ModelError,
                "https://models.example/v1/chat/completions",
            ),
            (
                "https://opencode.ai/zen/go/v1",
                WireProtocol::Responses,
                400,
                json!({"error": {"message": "model does not support this protocol"}}),
                AiErrorCode::ModelError,
                "https://opencode.ai/inference/go/openai/v1/responses",
            ),
            (
                "https://opencode.ai/zen/go/v1",
                WireProtocol::Messages,
                401,
                json!({"error": "credential refused"}),
                AiErrorCode::AuthRequired,
                "https://opencode.ai/inference/go/anthropic/v1/messages",
            ),
        ] {
            let script = ScriptedHttp::new(vec![(status, body)]);
            let deps = primitive_deps(script.http());
            deps.shared.connections().replace(&[AgentConfig { base_url: base.into(), ..original_connection() }]).unwrap();
            let binding = deps.shared.connections().bind("api-original-7", "unknown-model").unwrap();
            let error = invoke_once(&deps, &binding, primitive_call(wire), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap();
            assert_eq!(error.code(), code);
            let posts = script.recorded.lock().unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0].url, url);
        }
    }

    struct NotifyHttpDrop(mpsc::Sender<()>);
    impl Drop for NotifyHttpDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    // A received proposal is not a physical-exit acknowledgement. Serial budget/key tests
    // wait for the real worker's last transport Arc to drop before attempting admission again.
    // This wraps only HTTP ownership, never the primitive, dispatch or reply implementation.
    fn settled_invoke(
        deps: &HostAiDependencies,
        binding: &ConnectionBinding,
        call: ProviderCall,
        control: &Arc<CallControl>,
        deadline: Duration,
    ) -> Result<ProviderReply, AiError> {
        let (exited_tx, exited_rx) = mpsc::channel();
        let exit = NotifyHttpDrop(exited_tx);
        let http = Arc::clone(&deps.http);
        let scoped = HostAiDependencies {
            shared: Arc::clone(&deps.shared),
            http: Arc::new(move |request| {
                let _ = &exit;
                http(request)
            }),
            secrets: deps.secrets.clone(),
            artifacts: Arc::clone(&deps.artifacts),
            clock: Arc::clone(&deps.clock),
            ids: Arc::clone(&deps.ids),
        };
        let result = invoke_once(&scoped, binding, call, control, deadline);
        drop(scoped);
        exited_rx.recv_timeout(RACE_GUARD).unwrap();
        result
    }

    // Catches a permit owned by the waiter and late publication/private-state writes.
    #[test]
    fn host_ai_primitive_cancel_keeps_physical_permit() {
        for expire in [false, true] {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (exited_tx, exited_rx) = mpsc::channel();
            let exit = NotifyHttpDrop(exited_tx);
            let release_rx = Mutex::new(release_rx);
            let http: HttpFn = Arc::new(move |req| {
                let _ = &exit;
                assert_eq!(req.method, "POST");
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(RACE_GUARD).unwrap();
                Ok((
                    200,
                    json!({"choices": [{"message": {"tool_calls": [{"id": "late", "function": {"name": "execute", "arguments": "{}"}}]}}]})
                        .to_string()
                        .into_bytes(),
                ))
            });
            let time = Arc::new(AtomicU64::new(0));
            let mut deps = primitive_deps(http);
            let clock_time = Arc::clone(&time);
            deps.clock = Arc::new(move || Duration::from_secs(clock_time.load(Ordering::SeqCst)));
            let deps = Arc::new(deps);
            let shared = Arc::clone(&deps.shared);
            let binding = shared.connections().bind("api-original-7", "selected-model").unwrap();
            let control = Arc::new(CallControl::new());
            let caller_control = Arc::clone(&control);
            let caller = thread::spawn(move || {
                let mut call = primitive_call(WireProtocol::Chat);
                call.http_per_connection = 1;
                invoke_once(&deps, &binding, call, &caller_control, Duration::from_secs(60))
            });
            entered_rx.recv_timeout(RACE_GUARD).unwrap();
            if expire {
                time.store(60, Ordering::SeqCst);
            } else {
                control.cancel();
            }
            let expected = if expire { AiErrorCode::Timeout } else { AiErrorCode::Cancelled };
            assert_eq!(caller.join().unwrap().err().unwrap().code(), expected);
            let other = connection_test::admit(&shared, "api-original-7").unwrap();
            assert_eq!(connection_test::admit(&shared, "api-original-7").err().unwrap().code(), AiErrorCode::ResourceExhausted);
            let mut private_state_updates_after_cancel = 0;
            assert_eq!(
                control
                    .publish(|| {
                        private_state_updates_after_cancel += 1;
                        Ok(())
                    })
                    .unwrap_err()
                    .code(),
                expected
            );
            assert_eq!(private_state_updates_after_cancel, 0);
            release_tx.send(()).unwrap();
            // The last HTTP Arc belongs to the real physical worker. Its drop follows permit drop.
            exited_rx.recv_timeout(RACE_GUARD).unwrap();
            assert!(connection_test::admit(&shared, "api-original-7").is_ok());
            drop(other);
        }
    }

    // Catches generation/control checks performed only before a blocking keychain read.
    #[test]
    fn host_ai_primitive_rechecks_after_keychain() {
        struct BlockingSecrets {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Secrets for BlockingSecrets {
            fn get(&self, id: &str) -> Result<Option<String>, String> {
                assert_eq!(id, "api-original-7");
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(RACE_GUARD).unwrap();
                Ok(Some(KEY_A.into()))
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Err("unexpected write".into())
            }
            fn delete(&self, _: &str) -> Result<(), String> {
                Err("unexpected delete".into())
            }
        }
        for action in 0..3 {
            let script = ScriptedHttp::new(vec![(200, chat_answer())]);
            let mut deps = primitive_deps(script.http());
            let (entered, entered_rx) = mpsc::channel();
            let (release_tx, release) = mpsc::channel();
            deps.secrets = Some(Arc::new(BlockingSecrets { entered, release: Mutex::new(release) }));
            let deps = Arc::new(deps);
            let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
            let control = Arc::new(CallControl::new());
            let worker_deps = Arc::clone(&deps);
            let worker_control = Arc::clone(&control);
            let budget = Arc::new(ModelCallBudget::new(1).unwrap());
            let worker_budget = Arc::clone(&budget);
            let caller = thread::spawn(move || {
                let mut call = primitive_call(WireProtocol::Chat);
                call.model_budget = Some(worker_budget);
                settled_invoke(&worker_deps, &binding, call, &worker_control, Duration::from_secs(60))
            });
            entered_rx.recv_timeout(RACE_GUARD).unwrap();
            if action == 1 {
                control.cancel();
            } else if action == 0 {
                deps.shared.connections().replace(&[AgentConfig { base_url: "https://changed.example/v1".into(), ..original_connection() }]).unwrap();
            } else {
                deps.shared.connections().replace(&[AgentConfig { key: false, ..original_connection() }]).unwrap();
            }
            release_tx.send(()).unwrap();
            let result = caller.join().unwrap();
            if action == 2 {
                assert!(result.is_ok());
                let recorded = script.recorded.lock().unwrap();
                assert_eq!(recorded.len(), 1);
                assert!(recorded[0].headers.iter().all(|(name, _)| name != "Authorization" && name != "x-api-key"));
            } else {
                assert_eq!(result.err().unwrap().code(), if action == 1 { AiErrorCode::Cancelled } else { AiErrorCode::ConnectionChanged });
                assert!(script.recorded.lock().unwrap().is_empty());
            }
            deps.shared.connections().replace(&[original_connection()]).unwrap();
            let fresh = primitive_deps(script.http());
            let binding = fresh.shared.connections().bind("api-original-7", "selected").unwrap();
            let mut call = primitive_call(WireProtocol::Chat);
            call.model_budget = Some(budget);
            let result = settled_invoke(&fresh, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60));
            if action == 2 {
                assert_eq!(result.err().unwrap().code(), AiErrorCode::ResourceExhausted);
            } else {
                assert!(result.is_ok());
            }
            assert_eq!(script.recorded.lock().unwrap().len(), 1);
        }
    }

    // Catches off-by-one/debit-at-preflight, and failure paths refunding a scheduled attempt.
    #[test]
    fn host_ai_primitive_model_budget_debits_only_dispatch() {
        for invalid in [0, 21, u32::MAX] {
            assert_eq!(ModelCallBudget::new(invalid).err().unwrap().code(), AiErrorCode::InvalidRequest);
        }
        let script = ScriptedHttp::new(vec![(401, json!({"error": "credential"})); 20]);
        let mut deps = primitive_deps(script.http());
        let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
        let budget = Arc::new(ModelCallBudget::new(20).unwrap());
        let call = || {
            let mut call = primitive_call(WireProtocol::Chat);
            call.model_budget = Some(Arc::clone(&budget));
            call
        };
        let cancelled = Arc::new(CallControl::new());
        cancelled.cancel();
        assert_eq!(settled_invoke(&deps, &binding, call(), &cancelled, Duration::from_secs(60)).err().unwrap().code(), AiErrorCode::Cancelled);
        assert_eq!(
            settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::ZERO).err().unwrap().code(),
            AiErrorCode::Timeout
        );
        deps.secrets = None;
        assert_eq!(
            settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::AuthRequired
        );
        let failing_secrets = Arc::new(CurrentSecrets::new());
        *failing_secrets.value.lock().unwrap() = Err(format!("keychain error with {KEY_A}"));
        deps.secrets = Some(failing_secrets);
        assert_eq!(
            settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::InternalError
        );
        deps.secrets = Some(Arc::new(CurrentSecrets::new()));
        let mut invalid = call();
        invalid.timeout_seconds = 0;
        assert_eq!(
            settled_invoke(&deps, &binding, invalid, &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::InvalidRequest
        );
        let held: Vec<_> = (0..2).map(|_| connection_test::admit(&deps.shared, "api-original-7").unwrap()).collect();
        assert_eq!(
            settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::ResourceExhausted
        );
        drop(held);
        deps.shared.connections().replace(&[AgentConfig { base_url: "http://other.example/v1".into(), ..original_connection() }]).unwrap();
        let unsafe_binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
        assert_eq!(
            settled_invoke(&deps, &unsafe_binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::InvalidRequest
        );
        assert_eq!(
            settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::ConnectionChanged
        );
        deps.shared.connections().replace(&[original_connection()]).unwrap();
        let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
        assert!(script.recorded.lock().unwrap().is_empty());
        for _ in 0..20 {
            assert_eq!(
                settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
                AiErrorCode::AuthRequired
            );
        }
        assert_eq!(
            settled_invoke(&deps, &binding, call(), &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
            AiErrorCode::ResourceExhausted
        );
        assert_eq!(script.recorded.lock().unwrap().len(), 20);
    }

    // Catches per-worker budgets instead of a shared atomic budget at the physical seam.
    #[test]
    fn host_ai_primitive_model_budget_is_shared_atomically() {
        struct TogetherSecrets {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Secrets for TogetherSecrets {
            fn get(&self, id: &str) -> Result<Option<String>, String> {
                assert_eq!(id, "api-original-7");
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(RACE_GUARD).unwrap();
                Ok(Some(KEY_A.into()))
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Err("unexpected write".into())
            }
            fn delete(&self, _: &str) -> Result<(), String> {
                Err("unexpected delete".into())
            }
        }
        let script = ScriptedHttp::new(vec![(200, chat_answer())]);
        let mut deps = primitive_deps(script.http());
        let (entered, entered_rx) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        deps.secrets = Some(Arc::new(TogetherSecrets { entered, release: Mutex::new(release) }));
        let deps = Arc::new(deps);
        let budget = Arc::new(ModelCallBudget::new(1).unwrap());
        let callers: Vec<_> = (0..2)
            .map(|_| {
                let deps = Arc::clone(&deps);
                let budget = Arc::clone(&budget);
                thread::spawn(move || {
                    let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
                    let mut call = primitive_call(WireProtocol::Chat);
                    call.model_budget = Some(budget);
                    invoke_once(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).map(|_| ())
                })
            })
            .collect();
        for _ in 0..2 {
            entered_rx.recv_timeout(RACE_GUARD).unwrap();
        }
        for _ in 0..2 {
            release_tx.send(()).unwrap();
        }
        let outcomes: Vec<_> = callers.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(outcomes.iter().filter_map(|r| r.as_ref().err()).map(|e| e.code()).collect::<Vec<_>>(), [AiErrorCode::ResourceExhausted]);
        assert_eq!(script.recorded.lock().unwrap().len(), 1);
    }

    // Catches cached keys, lost tool argument classification, and a credential escaping in private data.
    #[test]
    fn host_ai_primitive_current_keys_and_private_proposals() {
        let answer = json!({"choices": [{"message": {"role": "assistant", "content": "tools", "reasoning_content": "private thought", "tool_calls": [
            {"id": "good", "function": {"name": "execute", "arguments": "{\"n\":7}"}},
            {"id": "bad", "function": {"name": "execute", "arguments": "{"}}
        ]}, "finish_reason": "tool_calls"}]});
        let script = ScriptedHttp::new(vec![
            (200, answer),
            (200, chat_answer()),
            (200, json!({"choices": [{"message": {"content": "safe", "reasoning_content": KEY_B}}]})),
        ]);
        let mut deps = primitive_deps(script.http());
        let secrets = Arc::new(CurrentSecrets::new());
        deps.secrets = Some(secrets.clone());
        let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
        let first =
            settled_invoke(&deps, &binding, primitive_call(WireProtocol::Chat), &Arc::new(CallControl::new()), Duration::from_secs(60)).unwrap();
        assert_eq!(first.finish_reason, super::super::host_ai::FinishReason::ToolCalls);
        let good: &provider::ProviderToolCall = &first.calls[0];
        assert_eq!(good.id, "good");
        assert_eq!(good.name, "execute");
        assert_eq!(good.arguments.as_ref().unwrap().get("n"), Some(&json!(7)));
        assert_eq!(first.calls[1].arguments, Err(provider::ToolArgumentError::InvalidJson));
        assert_eq!(first.private.native_assistants[0]["reasoning_content"], "private thought");
        secrets.set("api-original-7", KEY_B).unwrap();
        let mut next = primitive_call(WireProtocol::Chat);
        next.private = first.private;
        assert!(settled_invoke(&deps, &binding, next, &Arc::new(CallControl::new()), Duration::from_secs(60)).is_ok());
        let error = settled_invoke(&deps, &binding, primitive_call(WireProtocol::Chat), &Arc::new(CallControl::new()), Duration::from_secs(60))
            .err()
            .unwrap();
        assert_eq!(error.code(), AiErrorCode::MalformedResponse);
        assert!(!format!("{error:?}").contains(KEY_B));
        assert_eq!(*secrets.reads.lock().unwrap(), ["api-original-7", "api-original-7", "api-original-7"]);
        let requests = script.recorded.lock().unwrap();
        let auth: Vec<_> = requests.iter().map(|req| req.headers.iter().find(|(name, _)| name == "Authorization").unwrap().1.clone()).collect();
        assert_eq!(auth, [format!("Bearer {KEY_A}"), format!("Bearer {KEY_B}"), format!("Bearer {KEY_B}")]);
    }

    // Catches a raw-body-only scan: argument JSON unescaping and joined text can create the key.
    #[test]
    fn host_ai_primitive_decoded_credentials_do_not_escape() {
        let escaped: String = KEY_A.bytes().map(|byte| format!("\\u{byte:04x}")).collect();
        let (left, right) = KEY_A.split_at(17);
        for (wire, base, answer) in [
            (
                WireProtocol::Chat,
                "https://models.example/v1",
                json!({"choices": [{"message": {"content": null, "tool_calls": [
                    {"id": "echo", "function": {"name": "execute", "arguments": format!("{{\"echo\":\"{escaped}\"}}")}}
                ]}}]}),
            ),
            (
                WireProtocol::Responses,
                "https://opencode.ai/zen/go/v1",
                json!({"output": [{"type": "message", "content": [
                    {"type": "output_text", "text": left}, {"type": "output_text", "text": right}
                ]}]}),
            ),
            (
                WireProtocol::Messages,
                "https://opencode.ai/zen/go/v1",
                json!({"content": [
                    {"type": "text", "text": left}, {"type": "text", "text": right}
                ]}),
            ),
            (
                WireProtocol::Gemini,
                "https://opencode.ai/zen/go/v1",
                json!({"candidates": [{"content": {"parts": [{"text": left}, {"text": right}]}}]}),
            ),
        ] {
            let script = ScriptedHttp::new(vec![(200, answer)]);
            let deps = primitive_deps(script.http());
            deps.shared.connections().replace(&[AgentConfig { base_url: base.into(), ..original_connection() }]).unwrap();
            let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
            let error = invoke_once(&deps, &binding, primitive_call(wire), &Arc::new(CallControl::new()), Duration::from_secs(60)).err();
            assert_eq!(
                error.as_ref().map(|e| e.code()),
                Some(AiErrorCode::MalformedResponse),
                "{wire:?}: decoded credentials must not leave the worker"
            );
            assert!(!format!("{error:?}").contains(KEY_A));
            assert_eq!(script.recorded.lock().unwrap().len(), 1);
        }
    }

    // Catches timeout reported as cancellation, reuse after a host timeout, and refund on transport error.
    #[test]
    fn host_ai_primitive_transport_failure_and_timeout_are_terminal() {
        for (signal, code) in [("timeout: global", AiErrorCode::Timeout), (KEY_A, AiErrorCode::TransportError)] {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = Arc::clone(&calls);
            let deps = primitive_deps(Arc::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Err(signal.into())
            }));
            let binding = deps.shared.connections().bind("api-original-7", "selected").unwrap();
            let control = Arc::new(CallControl::new());
            let budget = Arc::new(ModelCallBudget::new(1).unwrap());
            let mut call = primitive_call(WireProtocol::Chat);
            call.model_budget = Some(Arc::clone(&budget));
            let error = invoke_once(&deps, &binding, call, &control, Duration::from_secs(60)).err().unwrap();
            assert_eq!(error.code(), code);
            assert!(!format!("{error:?}").contains(KEY_A));
            if code == AiErrorCode::Timeout {
                control.cancel();
                assert_eq!(control.publish(|| Ok(())).unwrap_err().code(), AiErrorCode::Timeout);
            }
            let mut call = primitive_call(WireProtocol::Chat);
            call.model_budget = Some(budget);
            assert_eq!(
                invoke_once(&deps, &binding, call, &Arc::new(CallControl::new()), Duration::from_secs(60)).err().unwrap().code(),
                AiErrorCode::ResourceExhausted
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    // Only the external keychain is substituted; catalog/binding/resolution are real.
    struct CurrentSecrets {
        ui_thread: thread::ThreadId,
        value: Mutex<Result<Option<String>, String>>,
        reads: Mutex<Vec<String>>,
    }

    impl CurrentSecrets {
        fn new() -> Self {
            Self { ui_thread: thread::current().id(), value: Mutex::new(Ok(Some(KEY_A.into()))), reads: Mutex::default() }
        }
    }

    impl Secrets for CurrentSecrets {
        fn get(&self, id: &str) -> Result<Option<String>, String> {
            assert_ne!(thread::current().id(), self.ui_thread, "keychain read on simulated frame thread");
            self.reads.lock().unwrap().push(id.into());
            self.value.lock().unwrap().clone()
        }
        fn set(&self, id: &str, secret: &str) -> Result<(), String> {
            assert_eq!(id, "api-original-7");
            *self.value.lock().unwrap() = Ok(Some(secret.into()));
            Ok(())
        }
        fn delete(&self, id: &str) -> Result<(), String> {
            assert_eq!(id, "api-original-7");
            *self.value.lock().unwrap() = Ok(None);
            Ok(())
        }
    }

    // Catches synthetic IDs, a cached credential, missing-key success, and a read during bind/replace.
    #[test]
    fn host_ai_connection_uses_original_id_and_current_secret() {
        let shared = AiShared::default();
        let secrets = CurrentSecrets::new();
        shared.connections().replace(&[original_connection()]).unwrap();
        let binding = shared.connections().bind("api-original-7", "selected-model").unwrap();
        assert_eq!(binding.connection_id(), "api-original-7");
        assert_eq!(binding.model_id(), "selected-model");
        assert_eq!(binding.protocol(), Protocol::OpenAi);
        assert!(secrets.reads.lock().unwrap().is_empty());
        let read = || thread::scope(|s| s.spawn(|| connection_test::read_key(shared.connections(), &binding, Some(&secrets))).join().unwrap());
        assert_eq!(read().unwrap().as_deref(), Some(KEY_A));
        secrets.set("api-original-7", KEY_B).unwrap();
        assert_eq!(read().unwrap().as_deref(), Some(KEY_B));
        secrets.delete("api-original-7").unwrap();
        assert_eq!(read().unwrap_err().code(), AiErrorCode::AuthRequired);
        assert_eq!(*secrets.reads.lock().unwrap(), ["api-original-7", "api-original-7", "api-original-7"]);
        assert!(!format!("{binding:?}").contains(KEY_A));
        assert!(!format!("{binding:?}").contains(KEY_B));
    }

    // Catches generation reset on delete, cross-catalog counter collisions and unbounded tombstones.
    #[test]
    fn host_ai_deleted_readded_connection_does_not_revive_binding() {
        let shared = AiShared::default();
        let config = original_connection();
        shared.connections().replace(std::slice::from_ref(&config)).unwrap();
        let old = shared.connections().bind("api-original-7", "selected-model").unwrap();
        shared.connections().replace(&[]).unwrap();
        assert_eq!(shared.connections().resolve(&old).err().unwrap().code(), AiErrorCode::ConnectionChanged);
        shared.connections().replace(std::slice::from_ref(&config)).unwrap();
        assert_eq!(shared.connections().resolve(&old).err().unwrap().code(), AiErrorCode::ConnectionChanged);
        let fresh = shared.connections().bind("api-original-7", "selected-model").unwrap();
        assert!(shared.connections().resolve(&fresh).is_ok());

        let other = AiShared::default();
        let mut changed = config.clone();
        changed.base_url = "https://other.example/v1".into();
        other.connections().replace(&[changed]).unwrap();
        assert_eq!(other.connections().resolve(&old).err().unwrap().code(), AiErrorCode::ConnectionChanged);
        // Even identical frozen settings from a different window are not the same source.
        let foreign = other.connections().bind("api-original-7", "selected-model").unwrap();
        other.connections().replace(std::slice::from_ref(&config)).unwrap();
        assert_eq!(shared.connections().resolve(&foreign).err().unwrap().code(), AiErrorCode::ConnectionChanged);
        let same_settings_foreign = other.connections().bind("api-original-7", "selected-model").unwrap();
        assert_eq!(shared.connections().resolve(&same_settings_foreign).err().unwrap().code(), AiErrorCode::ConnectionChanged);

        let limiter = Arc::new(HttpAdmission::default());
        let window_a = AiShared::new(Arc::clone(&limiter));
        let window_b = AiShared::new(limiter);
        window_a.connections().replace(std::slice::from_ref(&config)).unwrap();
        assert_eq!(window_b.connections().bind("api-original-7", "selected-model").err().unwrap().code(), AiErrorCode::ConnectionChanged);
        let permits: Vec<_> = (0..2).map(|_| connection_test::admit(&window_a, "api-original-7").unwrap()).collect();
        assert_eq!(connection_test::admit(&window_b, "api-original-7").err().unwrap().code(), AiErrorCode::ResourceExhausted);
        drop(permits);
        assert!(connection_test::admit(&window_b, "api-original-7").is_ok());

        for n in 0..128 {
            let unique = AgentConfig { id: format!("removed-{n}"), ..config.clone() };
            shared.connections().replace(&[unique]).unwrap();
        }
        shared.connections().replace(&[]).unwrap();
        assert_eq!(connection_test::retained_entries(shared.connections()), 0);

        shared.connections().replace(std::slice::from_ref(&config)).unwrap();
        let before_overflow = shared.connections().bind("api-original-7", "selected-model").unwrap();
        connection_test::exhaust_generations(shared.connections());
        let changed = AgentConfig { request_timeout_s: 600, ..config };
        assert_eq!(shared.connections().replace(&[changed]).unwrap_err().code(), AiErrorCode::InternalError);
        assert!(shared.connections().resolve(&before_overflow).is_ok(), "failed replace must be atomic");
    }

    // Catches stale auth policy, picker retargeting and omitted invocation-setting comparisons.
    #[test]
    fn host_ai_generation_freezes_settings_but_not_picker_or_credentials() {
        let shared = AiShared::default();
        let mut config = original_connection();
        shared.connections().replace(std::slice::from_ref(&config)).unwrap();
        let binding = shared.connections().bind("api-original-7", "selected-model").unwrap();
        config.name = "Renamed".into();
        config.model = "new-picker-default".into();
        config.base_url = " https://models.example/v1/ ".into();
        config.reasoning_effort = " high ".into();
        config.key = false;
        shared.connections().replace(std::slice::from_ref(&config)).unwrap();
        assert!(shared.connections().resolve(&binding).is_ok());
        assert_eq!(binding.model_id(), "selected-model");
        assert_eq!(binding.configuration().base_url, "https://models.example/v1");
        assert_eq!(binding.configuration().request_timeout_s, 300);
        assert_eq!(binding.configuration().reasoning_effort, "high");
        assert_eq!(connection_test::read_key(shared.connections(), &binding, None).unwrap(), None);
        config.key = true;
        shared.connections().replace(std::slice::from_ref(&config)).unwrap();
        assert!(shared.connections().resolve(&binding).is_ok());
        assert_eq!(connection_test::read_key(shared.connections(), &binding, None).unwrap_err().code(), AiErrorCode::AuthRequired);

        for changed in [
            AgentConfig { base_url: "https://changed.example/v1".into(), ..config.clone() },
            AgentConfig { request_timeout_s: 600, ..config.clone() },
            AgentConfig { reasoning_effort: "low".into(), ..config.clone() },
            AgentConfig { kind: Kind::Anthropic, ..config.clone() },
            AgentConfig { kind: Kind::Acp, ..config.clone() },
        ] {
            let catalog = AiShared::default();
            catalog.connections().replace(std::slice::from_ref(&config)).unwrap();
            let frozen = catalog.connections().bind("api-original-7", "selected-model").unwrap();
            catalog.connections().replace(&[changed]).unwrap();
            assert_eq!(catalog.connections().resolve(&frozen).err().unwrap().code(), AiErrorCode::ConnectionChanged);
        }

        for (invalid, code) in [
            (AgentConfig { id: " api-original-7".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { base_url: "https:///missing-host".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { base_url: "https://:443/v1".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { base_url: "https://models.example:bad/v1".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { base_url: "https://models.example:65536/v1".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { base_url: "https://[::1/v1".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { base_url: "not-an-endpoint".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { request_timeout_s: 0, ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { reasoning_effort: "x".repeat(65), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { reasoning_effort: "high\nlow".into(), ..config.clone() }, AiErrorCode::InvalidRequest),
            (AgentConfig { kind: Kind::Acp, ..config.clone() }, AiErrorCode::UnsupportedCapability),
        ] {
            shared.connections().replace(std::slice::from_ref(&invalid)).unwrap();
            assert_eq!(shared.connections().bind(&invalid.id, "selected-model").err().map(|error| error.code()), Some(code), "{invalid:?}");
            assert_eq!(connection_test::retained_entries(shared.connections()), 1, "invalid entries remain unavailable, not missing");
            assert_eq!(shared.connections().resolve(&binding).err().unwrap().code(), AiErrorCode::ConnectionChanged);
        }
        assert_eq!(shared.connections().bind("missing", "selected-model").err().unwrap().code(), AiErrorCode::ConnectionChanged);
        shared.connections().replace(&[config.clone(), config]).unwrap();
        assert_eq!(shared.connections().bind("api-original-7", "selected-model").err().unwrap().code(), AiErrorCode::InvalidRequest);
    }

    // Catches keychain error echo, locks held over keychain work, and unsafe HTTP key release.
    #[test]
    fn host_ai_keychain_failure_is_redacted_and_off_frame() {
        let shared = AiShared::default();
        let secrets = CurrentSecrets::new();
        *secrets.value.lock().unwrap() = Err(format!("keychain failed with {KEY_A} {KEY_B}"));
        shared.connections().replace(&[original_connection()]).unwrap();
        let binding = shared.connections().bind("api-original-7", "selected-model").unwrap();
        let error =
            thread::scope(|s| s.spawn(|| connection_test::read_key(shared.connections(), &binding, Some(&secrets))).join().unwrap()).unwrap_err();
        assert_eq!(error.code(), AiErrorCode::InternalError);
        for key in [KEY_A, KEY_B] {
            assert!(!error.message().contains(key));
            assert!(!format!("{error:?}").contains(key));
            assert!(!format!("{:?}", error.details()).contains(key));
        }
        assert!(error.message().len() <= 2048);
        secrets.set("api-original-7", KEY_A).unwrap();
        for (url, safe) in [
            ("https://models.example/v1", true),
            ("http://localhost:11434/v1", true),
            ("http://127.0.0.1:1234/v1", true),
            ("http://[::1]:8080/v1", true),
            ("http://192.168.1.20/v1", false),
            ("http://localhost.evil.example/v1", false),
            ("http://127.0.0.2/v1", false),
            ("http://localhost@evil.example/v1", false),
            ("http://localhost\\@evil.example/v1", false),
        ] {
            let config = AgentConfig { base_url: url.into(), ..original_connection() };
            shared.connections().replace(&[config]).unwrap();
            let result = thread::scope(|s| {
                s.spawn(|| {
                    let binding = shared.connections().bind("api-original-7", "selected-model")?;
                    connection_test::read_key(shared.connections(), &binding, Some(&secrets))
                })
                .join()
                .unwrap()
            });
            if safe {
                assert_eq!(result.unwrap().as_deref(), Some(KEY_A));
            } else {
                assert_eq!(result.unwrap_err().code(), AiErrorCode::InvalidRequest);
            }
        }
        let local = AgentConfig { base_url: "http://localhost:11434/v1".into(), key: false, ..original_connection() };
        shared.connections().replace(&[local]).unwrap();
        let local_binding = shared.connections().bind("api-original-7", "local-model").unwrap();
        assert_eq!(connection_test::read_key(shared.connections(), &local_binding, Some(&secrets)).unwrap(), None);

        // A keychain implementation can synchronously update preferences; resolve has released its lock.
        struct EditingSecrets<'a>(&'a AiShared);
        impl Secrets for EditingSecrets<'_> {
            fn get(&self, id: &str) -> Result<Option<String>, String> {
                assert_eq!(id, "api-original-7");
                self.0.connections().replace(&[]).unwrap();
                Ok(Some(KEY_B.into()))
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Err("unexpected test key write".into())
            }
            fn delete(&self, _: &str) -> Result<(), String> {
                Err("unexpected test key deletion".into())
            }
        }
        shared.connections().replace(&[original_connection()]).unwrap();
        let binding = shared.connections().bind("api-original-7", "selected-model").unwrap();
        thread::scope(|s| s.spawn(|| connection_test::read_key(shared.connections(), &binding, Some(&EditingSecrets(&shared)))).join().unwrap())
            .unwrap();
        assert_eq!(shared.connections().resolve(&binding).err().unwrap().code(), AiErrorCode::ConnectionChanged);
    }

    // Releasing a logical waiter must not release a physical worker's slot.
    #[test]
    fn host_ai_physical_slots_survive_logical_cancel() {
        let admission = HttpAdmission::default();
        let turn = TurnControl::new();
        let mut workers = Vec::new();
        let mut releases = Vec::new();
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let permit = admission.try_acquire("connection", 2, 8).unwrap();
            let control = turn.new_call().unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (reply_tx, reply_rx) = mpsc::channel();
            workers.push(thread::spawn(move || {
                let _physical_permit = permit;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let _ = reply_tx.send(control.publish(|| Ok("late reply")));
            }));
            entered_rx.recv().unwrap();
            releases.push(release_tx);
            waiters.push(reply_rx);
        }
        turn.cancel();
        drop(waiters);
        let third_same_connection = admission.try_acquire("connection", 2, 8).err().unwrap();
        assert_eq!(third_same_connection.code(), AiErrorCode::ResourceExhausted); // 2 held
        let other_workers: Vec<_> = (0..6).map(|i| admission.try_acquire(&format!("other-{i}"), 2, 8).unwrap()).collect();
        let ninth_host_call = admission.try_acquire("ninth", 2, 8).err().unwrap();
        assert_eq!(ninth_host_call.code(), AiErrorCode::ResourceExhausted); // 8 held after waiter drop
        releases.remove(0).send(()).unwrap();
        workers.remove(0).join().unwrap();
        let replacement_after_physical_release = admission.try_acquire("connection", 2, 8);
        assert!(replacement_after_physical_release.is_ok());
        releases.remove(0).send(()).unwrap();
        workers.remove(0).join().unwrap();
        drop(other_workers);
    }

    // A new window/connection must share the host count, even after waiters go away.
    #[test]
    fn host_ai_physical_limit_is_shared_across_connections() {
        let admission = Arc::new(HttpAdmission::default());
        let other_window = Arc::clone(&admission);
        let permits: Vec<_> = (0..8).map(|i| admission.try_acquire(&format!("connection-{}", i / 2), 2, 8).unwrap()).collect();
        let ninth_host_call = other_window.try_acquire("another-connection", 2, 8).err().unwrap();
        assert_eq!(ninth_host_call.code(), AiErrorCode::ResourceExhausted); // 8 held
        drop(permits);
        assert!(other_window.try_acquire("another-connection", 2, 8).is_ok());
    }

    #[test]
    fn host_ai_physical_lower_limits_count_existing_workers() {
        for (connection, host) in [(0, 8), (3, 8), (2, 0), (2, 9)] {
            assert_eq!(HttpAdmission::new(connection, host).err().unwrap().code(), AiErrorCode::InvalidRequest);
            assert_eq!(HttpAdmission::default().try_acquire("a", connection, host).err().unwrap().code(), AiErrorCode::InvalidRequest);
        }
        let admission = HttpAdmission::default();
        let first = admission.try_acquire("a", 2, 8).unwrap();
        assert_eq!(admission.try_acquire("a", 1, 8).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        assert_eq!(admission.try_acquire("b", 2, 1).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        let second = admission.try_acquire("a", 2, 8).unwrap();
        drop((first, second));
        let lower_host = HttpAdmission::new(2, 1).unwrap();
        let held = lower_host.try_acquire("a", 2, 8).unwrap();
        assert_eq!(lower_host.try_acquire("b", 2, 8).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        drop(held);
        let lower_connection = HttpAdmission::new(1, 8).unwrap();
        let held = lower_connection.try_acquire("a", 2, 8).unwrap();
        assert_eq!(lower_connection.try_acquire("a", 2, 8).err().unwrap().code(), AiErrorCode::ResourceExhausted);
        drop(held);
    }

    // Resetting a cancelled/completed control or reusing one across rounds is a bug.
    #[test]
    fn host_ai_call_control_cannot_be_reactivated() {
        let turn = TurnControl::new();
        for reply in ["first round", "second round"] {
            let call = turn.new_call().unwrap();
            assert_eq!(call.publish(|| Ok(reply)).unwrap(), reply);
            assert!(call.publish::<()>(|| panic!("second publication executed")).is_err());
        }
        let cancelled_control = turn.new_call().unwrap();
        turn.cancel();
        turn.cancel();
        let (now, later) = (Duration::from_secs(1), Duration::from_secs(2));
        assert_eq!(cancelled_control.check(now, later).unwrap_err().code(), AiErrorCode::Cancelled);
        assert_eq!(cancelled_control.publish::<()>(|| panic!("cancelled publication executed")).unwrap_err().code(), AiErrorCode::Cancelled);
        assert_eq!(turn.check().unwrap_err().code(), AiErrorCode::Cancelled);
        assert_eq!(turn.new_call().err().unwrap().code(), AiErrorCode::Cancelled);
        let next_turn = TurnControl::new();
        assert_eq!(next_turn.new_call().unwrap().publish(|| Ok("new turn")).unwrap(), "new turn");
    }

    #[test]
    fn host_ai_call_control_repeated_turn_cancel_revokes_later_publication() {
        let turn = Arc::new(TurnControl::new());
        let blocking_child = turn.new_call().unwrap();
        let later_child = turn.new_call().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let publisher = thread::spawn(move || {
            blocking_child.publish(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok("admitted before cancellation")
            })
        });
        entered_rx.recv().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let cancellers: Vec<_> = (0..2)
            .map(|_| {
                let turn = Arc::clone(&turn);
                let done_tx = done_tx.clone();
                thread::spawn(move || {
                    turn.cancel();
                    done_tx.send(()).unwrap();
                })
            })
            .collect();
        drop(done_tx);
        // The first publication stays inside its closure. A completed cancel,
        // not a scheduling guess, must revoke a later publication's admission.
        done_rx.recv().unwrap();
        assert_eq!(turn.check().unwrap_err().code(), AiErrorCode::Cancelled);
        let late_publication = later_child.publish(|| Ok("published after cancel returned"));
        let new_child = turn.new_call().err().map(|error| error.code());
        release_tx.send(()).unwrap();
        assert_eq!(publisher.join().unwrap().unwrap(), "admitted before cancellation");
        for canceller in cancellers {
            canceller.join().unwrap();
        }
        assert_eq!(late_publication.map_err(|error| error.code()), Err(AiErrorCode::Cancelled));
        assert_eq!(new_child, Some(AiErrorCode::Cancelled));
    }

    #[test]
    fn host_ai_call_control_terminal_children_release_registrations() {
        let turn = TurnControl::new();
        let mut retained = Vec::new();
        for _ in 0..32 {
            let completed = turn.new_call().unwrap();
            assert_eq!(Arc::weak_count(&completed), 1, "one active turn registration");
            completed.publish(|| Ok(())).unwrap();
            retained.push(completed);
            for completed in &retained {
                assert_eq!(Arc::weak_count(completed), 0, "a held completed Arc must not retain a turn registration");
            }
            let dropped = turn.new_call().unwrap();
            let observation = Arc::downgrade(&dropped);
            drop(dropped);
            assert!(observation.upgrade().is_none(), "turn tracking must not keep a dropped child alive");
        }
        let timed_out = turn.new_call().unwrap();
        assert_eq!(timed_out.check(Duration::from_secs(1), Duration::from_secs(1)).unwrap_err().code(), AiErrorCode::Timeout);
        assert_eq!(Arc::weak_count(&timed_out), 0);
        let cancelled = turn.new_call().unwrap();
        cancelled.cancel();
        assert_eq!(Arc::weak_count(&cancelled), 0);
        let active = turn.new_call().unwrap();
        turn.cancel();
        assert_eq!(active.publish::<()>(|| panic!("revoked child published")).unwrap_err().code(), AiErrorCode::Cancelled);
        assert_eq!(Arc::weak_count(&active), 0);
        assert_eq!(timed_out.check(Duration::ZERO, Duration::from_secs(2)).unwrap_err().code(), AiErrorCode::Timeout);
    }

    // Publication owns the call lock through the commit; cancellation then becomes terminal.
    #[test]
    fn host_ai_call_control_publication_and_cancel_are_serialized() {
        let call = Arc::new(CallControl::new());
        let publishing = Arc::clone(&call);
        let cancelling = Arc::clone(&call);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let committed = Arc::new(AtomicBool::new(false));
        let commit_result = Arc::clone(&committed);
        let publisher = thread::spawn(move || {
            publishing.publish(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                committed.store(true, Ordering::Release);
                Ok("committed")
            })
        });
        entered_rx.recv().unwrap();
        let canceller = thread::spawn(move || {
            cancelling.cancel();
            assert!(commit_result.load(Ordering::Acquire), "cancel returned before an admitted publication committed");
        });
        release_tx.send(()).unwrap();
        assert_eq!(publisher.join().unwrap().unwrap(), "committed");
        canceller.join().unwrap();
        assert!(call.publish::<()>(|| panic!("late commit executed")).is_err());
    }

    #[test]
    fn host_ai_deadline_is_not_user_cancel() {
        let deadline = Duration::from_secs(10);
        let clock: ClockFn = Arc::new(move || deadline);
        let expired_control = CallControl::new();
        expired_control.check(Duration::from_secs(9), deadline).unwrap();
        assert_eq!(expired_control.check(clock(), deadline).unwrap_err().code(), AiErrorCode::Timeout);
        assert_eq!(expired_control.publish::<()>(|| panic!("timed-out publication executed")).unwrap_err().code(), AiErrorCode::Timeout);
        // An earlier clock value or a new deadline cannot reactivate this lifetime.
        assert_eq!(expired_control.check(Duration::ZERO, Duration::from_secs(20)).unwrap_err().code(), AiErrorCode::Timeout);
        expired_control.cancel();
        assert_eq!(expired_control.check(deadline, deadline).unwrap_err().code(), AiErrorCode::Timeout);
        let cancelled_control = CallControl::new();
        cancelled_control.cancel();
        assert_eq!(cancelled_control.check(deadline, deadline).unwrap_err().code(), AiErrorCode::Cancelled);
    }

    #[test]
    fn host_ai_call_control_limits_reject_zero_and_excess() {
        AiLimits::contract_defaults().validate().unwrap();
        let ceilings = [128, 64, 32, 262_144, 65_536, 4, 4_194_304, 16_777_216, 32, 16_777_216, 86_400, 2, 8, 600];
        for (index, ceiling) in ceilings.into_iter().enumerate() {
            for invalid in [0, ceiling + 1] {
                let mut limits = AiLimits::contract_defaults();
                let fields = [
                    &mut limits.model_messages,
                    &mut limits.tool_definitions,
                    &mut limits.tool_calls,
                    &mut limits.text_bytes,
                    &mut limits.tool_json_bytes,
                    &mut limits.attachments,
                    &mut limits.attachment_bytes,
                    &mut limits.attachment_total_bytes,
                    &mut limits.continuation_handles,
                    &mut limits.continuation_bytes,
                    &mut limits.continuation_ttl_seconds,
                    &mut limits.http_per_connection,
                    &mut limits.http_per_host,
                    &mut limits.call_max_seconds,
                ];
                *fields.into_iter().nth(index).unwrap() = invalid;
                assert_eq!(limits.validate().unwrap_err().code(), AiErrorCode::InvalidRequest);
            }
        }
    }

    #[test]
    fn mcp_tools_become_function_tools_without_crash_reports() {
        let list = json!({"tools": [
            {"name": "batch", "description": "Run commands", "inputSchema": {"type": "object", "properties": {"commands": {"type": "array"}}}},
            {"name": "crash_reports", "description": "x", "inputSchema": {"type": "object"}},
            {"name": "measure"},
        ]});
        let t = openai_tools(&list);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0]["function"]["name"], "batch");
        assert_eq!(t[1]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn pictures_go_as_data_urls_and_text_alone_as_a_string() {
        assert_eq!(user_message(Protocol::OpenAi, &[PromptPart::Text("hi".into())])["content"], "hi");
        let m =
            user_message(Protocol::OpenAi, &[PromptPart::Text("hi".into()), PromptPart::Image { mime: "image/png".into(), base64: "AA==".into() }]);
        assert_eq!(m["content"][1]["image_url"]["url"], "data:image/png;base64,AA==");
        let a = user_message(
            Protocol::Anthropic,
            &[PromptPart::Text("hi".into()), PromptPart::Image { mime: "image/png".into(), base64: "AA==".into() }],
        );
        assert_eq!(a["content"][1]["type"], "image");
        assert_eq!(a["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(a["content"][1]["source"]["data"], "AA==");
    }

    fn view(n: u8) -> Value {
        user_message(
            Protocol::OpenAi,
            &[PromptPart::Text(format!("look {n}")), PromptPart::Image { mime: "image/png".into(), base64: format!("V{n}") }],
        )
    }

    fn pictures(history: &[Value]) -> Vec<String> {
        history
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter_map(|p| p.pointer("/image_url/url").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    #[test]
    fn only_the_newest_views_stay_in_the_history_and_older_ones_become_a_note() {
        let mut h = vec![json!({"role": "system", "content": "note"}), view(1), json!({"role": "assistant", "content": "ok"}), view(2)];
        assert!(!drop_old_views(&mut h, 2));
        assert_eq!(pictures(&h).len(), 2, "two views fit, nothing is dropped");

        h.push(json!({"role": "user", "content": "plain text"}));
        h.push(view(3));
        assert!(drop_old_views(&mut h, 2));
        assert_eq!(pictures(&h), vec!["data:image/png;base64,V2", "data:image/png;base64,V3"]);
        // The oldest view is a note now; the words sent with it, and every other message, stay.
        assert_eq!(h[1]["content"][0]["text"], "look 1");
        assert_eq!(h[1]["content"][1], json!({"type": "text", "text": DROPPED_VIEW_NOTE}));
        assert_eq!(h[0]["content"], "note");
        assert_eq!(h[4]["content"], "plain text");
    }

    #[test]
    fn a_long_chat_with_a_view_per_message_does_not_grow_without_end() {
        let mut h = vec![json!({"role": "system", "content": "note"})];
        let big = "A".repeat(200_000);
        for _ in 0..40 {
            h.push(user_message(
                Protocol::OpenAi,
                &[PromptPart::Text("again".into()), PromptPart::Image { mime: "image/png".into(), base64: big.clone() }],
            ));
            drop_old_views(&mut h, MAX_HISTORY_IMAGES);
            h.push(json!({"role": "assistant", "content": "done"}));
        }
        let size = json!({"messages": h}).to_string().len();
        assert_eq!(pictures(&h).len(), MAX_HISTORY_IMAGES);
        assert!(size < (MAX_HISTORY_IMAGES + 1) * big.len(), "request body is {size} bytes");
    }

    #[test]
    fn anthropic_views_are_dropped_the_same_way() {
        let view = |n: u8| {
            user_message(
                Protocol::Anthropic,
                &[PromptPart::Text(format!("look {n}")), PromptPart::Image { mime: "image/png".into(), base64: format!("V{n}") }],
            )
        };
        let mut h = vec![view(1), json!({"role": "assistant", "content": "ok"}), view(2), view(3)];
        drop_old_views(&mut h, 2);
        let kept: Vec<&str> =
            h.iter().filter_map(|m| m["content"].as_array()).flatten().filter_map(|p| p.pointer("/source/data").and_then(Value::as_str)).collect();
        assert_eq!(kept, vec!["V2", "V3"]);
        assert_eq!(h[0]["content"][0]["text"], "look 1");
        assert_eq!(h[0]["content"][1], json!({"type": "text", "text": DROPPED_VIEW_NOTE}));
    }

    #[test]
    fn a_too_large_request_says_so() {
        let e = http_error(413, &json!({"error": {"message": "Request body is too large."}}));
        assert!(e.starts_with("HTTP 413: "), "{e}");
        assert!(!e.contains("Request body is too large."), "provider diagnostics must stay private: {e}");
    }

    /// A scripted endpoint: one answer with a tool call, then a plain answer. The loop runs the
    /// tool (here without SolidCraft tools: an unknown tool fails) and reports the turn's end.
    #[test]
    fn the_tool_loop_runs_calls_and_ends_the_turn() {
        let answers = std::sync::Mutex::new(vec![
            json!({"choices": [{"message": {"role": "assistant", "content": "Done."}}]}),
            json!({"choices": [{"message": {"role": "assistant", "content": "Measuring.", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "measure", "arguments": "{}"}}]}}]}),
        ]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
            let a = answers.lock().unwrap().pop().unwrap_or(json!({}));
            Ok((200, a.to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "m", None, http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        b.prompt(&[PromptPart::Text("measure it".into())]);
        let events = wait_for_turn(&mut b);
        assert_eq!(events.first(), Some(&Event::ToolsAvailable(false)));
        assert!(events.iter().any(|e| matches!(e, Event::Ready { .. })), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::Tool { status: Some(ToolStatus::Failed), .. })), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Done.")), "{events:?}");
        assert!(matches!(events.last(), Some(Event::TurnEnded { reason }) if reason.is_empty()), "{events:?}");
        // The second request carried the tool's answer.
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["messages"].as_array().unwrap().last().unwrap()["role"], "tool");
        b.shutdown(true);
    }

    /// Cancel while a model request is in flight ends the turn at once (the request is
    /// abandoned, its late answer dropped), leaves no half-finished round in the history and
    /// the next message goes through.
    #[test]
    fn cancelling_a_request_ends_the_turn_and_the_next_message_still_works() {
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            let n = {
                let mut l = log.lock().unwrap();
                l.push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
                l.len()
            };
            if n == 1 {
                // The first request hangs until the test lets it go, then answers late.
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
                return Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "Too late."}}]}).to_string().into_bytes()));
            }
            Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "Steered."}}]}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "m", None, http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        let wait = |b: &mut ApiBackend, events: &mut Vec<Event>| {
            let start = std::time::Instant::now();
            while !events.iter().any(|e| matches!(e, Event::TurnEnded { .. })) && start.elapsed() < Duration::from_secs(10) {
                events.extend(b.poll());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        b.prompt(&[PromptPart::Text("long job".into())]);
        let start = std::time::Instant::now();
        wait_for_seen(&seen);
        b.cancel();
        let mut events = Vec::new();
        wait(&mut b, &mut events);
        assert!(start.elapsed() < Duration::from_secs(8), "the turn ended while the request still hung");
        assert!(matches!(events.last(), Some(Event::TurnEnded { reason }) if reason == "cancelled"), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, Event::Text { .. } | Event::Error(_))), "{events:?}");
        // The next message is answered, and the late answer of the first never shows up.
        b.prompt(&[PromptPart::Text("[interrupted] do this instead".into())]);
        let mut events = Vec::new();
        wait(&mut b, &mut events);
        let _ = release_tx.send(());
        assert!(events.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Steered.")), "{events:?}");
        assert!(matches!(events.last(), Some(Event::TurnEnded { reason }) if reason.is_empty()), "{events:?}");
        let sent = seen.lock().unwrap();
        let roles: Vec<&str> = sent[1]["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
        assert_eq!(roles, ["system", "user", "user"], "no reply or tool call of the cancelled round is left");
        b.shutdown(true);
    }

    /// New chat resets the model's history to the system note: the next request carries nothing
    /// of the conversation before it. The note carries the custom instructions set for each
    /// conversation (#8220): the first one's, then none after they were cleared.
    #[test]
    fn a_new_chat_resets_the_history_to_the_system_note() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
            Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "m", None, http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        let run = |b: &mut ApiBackend, f: &dyn Fn(&mut ApiBackend), want: usize| {
            f(b);
            let mut events = Vec::new();
            let start = std::time::Instant::now();
            while events.iter().filter(|e| matches!(e, Event::TurnEnded { .. } | Event::Ready { .. })).count() < want
                && start.elapsed() < Duration::from_secs(10)
            {
                events.extend(b.poll());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        b.set_instructions("Use M3 screws.");
        run(&mut b, &|b| b.prompt(&[PromptPart::Text("one".into())]), 2);
        b.set_instructions("");
        run(&mut b, &|b| b.new_session(), 1);
        run(&mut b, &|b| b.prompt(&[PromptPart::Text("two".into())]), 1);
        let sent = seen.lock().unwrap();
        assert_eq!(sent[0]["messages"][0]["content"], super::super::acp::system_note("Use M3 screws."));
        let roles: Vec<&str> = sent[1]["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
        assert_eq!(roles, ["system", "user"]);
        assert_eq!(sent[1]["messages"][0]["content"], super::super::acp::SYSTEM_NOTE);
        assert_eq!(sent[1]["messages"][1]["content"], "two");
        b.shutdown(true);
    }

    /// The endpoint's `/models` fills the picker (with the current model first if it is not
    /// listed), and choosing one changes the model of the next request.
    #[test]
    fn the_endpoint_lists_models_and_a_choice_changes_the_next_request() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                assert!(r.url.ends_with("/models"));
                return Ok((200, json!({"data": [{"id": "big"}, {"id": "small"}]}).to_string().into_bytes()));
            }
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
            Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "local", None, http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        let mut events = Vec::new();
        let start = std::time::Instant::now();
        while !events.iter().any(|e| matches!(e, Event::Models { .. })) && start.elapsed() < Duration::from_secs(10) {
            events.extend(b.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        let Some(Event::Models { current, available }) = events.iter().find(|e| matches!(e, Event::Models { .. })) else { panic!("{events:?}") };
        assert_eq!(current, "local");
        assert_eq!(available.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["local", "big", "small"]);
        assert!(events.iter().any(|e| matches!(e, Event::ModelsNote(None))), "{events:?}");
        b.set_model("big");
        b.prompt(&[PromptPart::Text("hi".into())]);
        wait_for_seen(&seen);
        assert_eq!(seen.lock().unwrap()[0]["model"], "big");
        b.shutdown(true);
    }

    /// Go models route by family; the preset's base stays one URL for all of them.
    /// Covers every model the public list returned, so each picks its protocol
    /// at once instead of burning fallback requests on the first turn.
    #[test]
    fn go_models_route_to_their_protocol() {
        for model in [
            "deepseek-v4.1-flash",
            "deepseek-v4-pro",
            "glm-5.1",
            "glm-5.2",
            "glm-5.3",
            "glm-5.3-flash",
            "hy3",
            "hy4-preview",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "kimi-k3",
            "kimi-k3.1-pro",
            "longcat-2.0",
            "mimo-v2.5",
            "mimo-v2.5-pro",
            "mimo-v2.6-flash",
            "mimo-v2.6-pro",
            // minimax-m3.1-flash-free is chat/completions per the Go docs, not messages.
            "minimax-m3.1-flash",
            "minimax-m3.1-flash-free",
            "omen-alpha",
            "step-5-preview",
            "beam-free",
            "something-new",
        ] {
            assert_eq!(go_protocol(model), GoProtocol::Chat, "{model}");
        }
        for model in ["grok-4.6", "grok-4.7", "gpt-6-luna", "gpt-5.6-luna", "muse-spark-1.3-contributor", "muse-spark-1.2-contributor"] {
            assert_eq!(go_protocol(model), GoProtocol::Responses, "{model}");
        }
        for model in [
            "claude-haiku-5-5",
            "minimax-m2.5",
            "minimax-m2.7",
            "minimax-m3",
            "qwen3.6-plus",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-max",
            "qwen3.8-flash",
        ] {
            assert_eq!(go_protocol(model), GoProtocol::Messages, "{model}");
        }
        assert_eq!(go_protocol("gemini-4-argon-contributor"), GoProtocol::Gemini);
        let gemini = go_url("gemini-4-argon-contributor", GoProtocol::Gemini);
        assert!(gemini.starts_with("https://opencode.ai/inference/go/") && gemini.contains("gemini-4-argon-contributor"), "{gemini}");
        assert_eq!(go_url("muse-spark-1.3-contributor", GoProtocol::Responses), "https://opencode.ai/inference/go/openai/v1/responses");
    }

    fn go_cfg(model: &str, http: HttpFn) -> ApiConfig {
        direct_config(Protocol::OpenAi, "https://opencode.ai/inference/go/v1", model, None, http, "")
    }

    fn go_ctx() -> StartContext {
        StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) }
    }

    /// Wait until a scripted endpoint saw a request (ten seconds at most).
    fn wait_for_seen<T>(seen: &std::sync::Mutex<Vec<T>>) {
        for _ in 0..1000 {
            if !seen.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Poll a scripted backend until its turn ends (ten seconds at most).
    fn wait_for_turn(b: &mut ApiBackend) -> Vec<Event> {
        let mut events = Vec::new();
        let start = std::time::Instant::now();
        while !events.iter().any(|e| matches!(e, Event::TurnEnded { .. })) && start.elapsed() < Duration::from_secs(10) {
            events.extend(b.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        events
    }

    /// Every Go protocol carries the session header; other endpoints see none.
    #[test]
    fn go_session_header_goes_on_every_protocol() {
        for model in ["mimo-v2.6-flash", "muse-spark-1.3-contributor", "claude-haiku-5-5", "gemini-4-argon-contributor"] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::<HttpRequest>::new()));
            let log = seen.clone();
            let http: HttpFn = Arc::new(move |r: &HttpRequest| {
                if r.method == "GET" {
                    return Ok((200, b"{\"data\":[]}".to_vec()));
                }
                log.lock().unwrap().push(r.clone());
                Ok((200, strict_answer(go_protocol(model)).to_string().into_bytes()))
            });
            let mut b = ApiBackend::start(go_cfg(model, http), go_ctx()).unwrap();
            b.prompt(&[PromptPart::Text("hi".into())]);
            wait_for_seen(&seen);
            let reqs = seen.lock().unwrap();
            assert!(!reqs.is_empty(), "{model}: no request sent");
            assert!(
                reqs.iter().all(|r| r.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("x-opencode-session"))),
                "{model}: every request needs x-opencode-session"
            );
            // Go speaks Bearer on every path, and Messages takes the version header too.
            assert!(reqs.iter().all(|r| r.headers.iter().any(|(n, v)| n == "Content-Type" && v == "application/json")));
            drop(reqs);
            b.shutdown(true);
        }
        // A non-Go endpoint with a Go-looking model name still sends no session header.
        let seen = Arc::new(std::sync::Mutex::new(Vec::<HttpRequest>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(r.clone());
            Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "https://api.openai.com/v1", "muse-spark-1.3-contributor", None, http, "");
        let mut b = ApiBackend::start(cfg, go_ctx()).unwrap();
        b.prompt(&[PromptPart::Text("hi".into())]);
        wait_for_seen(&seen);
        let reqs = seen.lock().unwrap();
        assert!(reqs.iter().all(|r| r.headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("x-opencode-session"))));
        drop(reqs);
        b.shutdown(true);
    }

    /// The session ID is stable within a chat and fresh after New Chat.
    #[test]
    fn go_session_is_stable_within_a_chat() {
        let sessions = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = sessions.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            let id = r.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("x-opencode-session")).map(|(_, v)| v.clone()).unwrap_or_default();
            log.lock().unwrap().push(id);
            Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}).to_string().into_bytes()))
        });
        let mut b = ApiBackend::start(go_cfg("mimo-v2.6-flash", http), go_ctx()).unwrap();
        let run = |b: &mut ApiBackend, f: &dyn Fn(&mut ApiBackend), want: usize| {
            f(b);
            let mut events = Vec::new();
            let start = std::time::Instant::now();
            while events.iter().filter(|e| matches!(e, Event::TurnEnded { .. } | Event::Ready { .. })).count() < want
                && start.elapsed() < Duration::from_secs(10)
            {
                events.extend(b.poll());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        run(&mut b, &|b| b.prompt(&[PromptPart::Text("one".into())]), 2);
        run(&mut b, &|b| b.prompt(&[PromptPart::Text("still one".into())]), 1);
        run(&mut b, &|b| b.new_session(), 1);
        run(&mut b, &|b| b.prompt(&[PromptPart::Text("two".into())]), 1);
        let ids = sessions.lock().unwrap();
        assert_eq!(ids.len(), 3);
        assert!(!ids[0].is_empty(), "every model request needs x-opencode-session");
        assert_eq!(ids[0], ids[1], "same conversation keeps its session ID");
        assert_ne!(ids[1], ids[2], "New Chat gets a fresh session ID");
        drop(ids);
        b.shutdown(true);
    }

    /// Non-chat Go answers parse into the loop's Reply with OpenAI-shaped history.
    #[test]
    fn go_replies_parse_into_openai_shaped_history() {
        let r = go_responses_reply(&json!({"output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "content": [{"type": "output_text", "text": "Mea"}, {"type": "output_text", "text": "suring."}]},
            {"type": "function_call", "call_id": "c1", "name": "measure", "arguments": "{}"},
        ]}));
        assert_eq!(r.text, [(false, "Measuring.".to_string())]);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "measure");
        assert_eq!(r.assistant["tool_calls"][0]["id"], "c1");
        let m = go_messages_reply(&json!({"content": [
            {"type": "text", "text": "Hi."},
            {"type": "tool_use", "id": "t1", "name": "batch", "input": {"commands": []}},
        ]}));
        assert_eq!(m.text, [(false, "Hi.".to_string())]);
        assert_eq!(m.calls[0].id, "t1");
        assert_eq!(m.calls[0].arguments.as_ref().unwrap(), &json!({"commands": []}));
        assert_eq!(m.assistant["tool_calls"][0]["function"]["arguments"], "{\"commands\":[]}");
        let g = go_gemini_reply(&json!({"candidates": [{"content": {"parts": [
            {"text": "Done."},
            {"functionCall": {"name": "measure", "args": {}}},
        ]}}]}));
        assert_eq!(g.text, [(false, "Done.".to_string())]);
        assert_eq!(g.calls[0].name, "measure");
        assert_eq!(g.assistant["role"], "assistant");
        // Empty answers end the turn instead of breaking it.
        for (protocol, v) in [
            (GoProtocol::Responses, json!({"output": []})),
            (GoProtocol::Messages, json!({"content": []})),
            (GoProtocol::Gemini, json!({"candidates": []})),
        ] {
            let n = Worker::go_reply(protocol, &v);
            assert!(n.text.is_empty() && n.calls.is_empty(), "{v}");
            assert!(n.assistant.get("tool_calls").is_none(), "{v}");
        }
    }

    /// Gemini's thought signature survives the round trip: the next request
    /// replays exactly the signature the answer carried (Gemini 3 rejects the
    /// tool round without it).
    #[test]
    fn go_gemini_signatures_replay_on_the_next_request() {
        let answer = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "measure", "args": {}}, "thoughtSignature": "sig-1"},
        ]}}]});
        let reply = go_gemini_reply(&answer);
        assert_eq!(reply.calls.len(), 1);
        let mut sigs = std::collections::BTreeMap::new();
        for (call, sig) in reply.calls.iter().zip(go_gemini_signatures(&answer)) {
            if let Some(s) = sig {
                sigs.insert(call.id.clone(), s);
            }
        }
        let contents = go_gemini_contents(&[reply.assistant], &sigs);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "measure");
        assert_eq!(contents[0]["parts"][0]["thoughtSignature"], "sig-1");
    }

    /// Two Gemini tool rounds never share a call id, so an old tool result
    /// keeps the name of the function it answered.
    #[test]
    fn go_gemini_call_ids_are_unique_across_rounds() {
        let first = go_gemini_reply(&json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "measure", "args": {}}},
        ]}}]}));
        let second = go_gemini_reply(&json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "batch", "args": {}}},
        ]}}]}));
        assert_ne!(first.calls[0].id, second.calls[0].id, "reused ids answer old results with the newest name");
        let history = vec![
            first.assistant.clone(),
            json!({"role": "tool", "tool_call_id": first.calls[0].id, "content": "1mm"}),
            second.assistant.clone(),
            json!({"role": "tool", "tool_call_id": second.calls[0].id, "content": "ok"}),
        ];
        let contents = go_gemini_contents(&history, &std::collections::BTreeMap::new());
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["name"], "measure");
        assert_eq!(contents[3]["parts"][0]["functionResponse"]["name"], "batch");
    }

    /// A Chat answer keeps the thinking fields the endpoint sent, so the next
    /// tool request replays them (Kimi K3 needs its reasoning back); only the
    /// streaming-only `index` members are dropped.
    #[test]
    fn openai_replies_keep_reasoning_fields_for_replay() {
        let r = openai_reply(&json!({"choices": [{"message": {
            "role": "assistant",
            "content": "Measuring.",
            "reasoning_content": "pick the tool",
            "reasoning_details": [{"type": "reasoning.text", "text": "pick", "index": 0}],
            "refusal": null,
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "measure", "arguments": "{}"}}],
        }}]}));
        assert_eq!(r.assistant["reasoning_content"], "pick the tool");
        assert_eq!(r.assistant["reasoning_details"][0]["text"], "pick");
        assert!(r.assistant["reasoning_details"][0].get("index").is_none());
        assert!(r.assistant.get("refusal").is_none(), "null fields stay out");
        assert_eq!(r.calls.len(), 1);
    }

    /// The model list and connection test are plain GETs: no session header.
    /// Conversation (POST) requests carry it instead (see the session tests).
    #[test]
    fn go_model_list_sends_no_session_header() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<HttpRequest>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            log.lock().unwrap().push(r.clone());
            Ok((200, b"{\"data\":[{\"id\":\"kimi-k3\"}]}".to_vec()))
        });
        let listed = model_list(&http, "https://opencode.ai/inference/go/v1", None).unwrap();
        assert_eq!(listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["kimi-k3"]);
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].url, "https://opencode.ai/inference/go/v1/models");
        assert!(reqs[0].headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("x-opencode-session")));
        drop(reqs);
    }

    /// OpenAI-shaped history converts per Go protocol: system handling, merged
    /// tool answers, output_text for Responses, id-to-name for Gemini.
    #[test]
    fn go_history_converts_for_each_protocol() {
        let history = vec![
            json!({"role": "user", "content": "measure it"}),
            json!({"role": "assistant", "content": "On it.", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "measure", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "batch", "arguments": "{\"commands\":[]}"}},
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "1mm"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "ok"}),
        ];
        let messages = go_anthropic_messages(&history);
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
        assert_eq!(roles, ["user", "assistant", "user"], "{messages:?}");
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2, "both tool answers in one message");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        let input = go_responses_input("sys", &history);
        assert_eq!(input[0]["type"], "message");
        assert!(input.iter().any(|i| i["type"] == "function_call"), "{input:?}");
        assert!(input.iter().any(|i| i["type"] == "function_call_output" && i["output"] == "1mm"), "{input:?}");
        assert!(
            input.iter().any(|i| i["role"] == "assistant"
                && i.get("content").and_then(Value::as_array).is_some_and(|c| c.iter().any(|p| p["type"] == "output_text"))),
            "{input:?}"
        );
        let contents = go_gemini_contents(&history, &std::collections::BTreeMap::new());
        let groles: Vec<&str> = contents.iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
        assert_eq!(groles, ["user", "model", "user"], "{contents:?}");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "measure");
        // Each Go protocol posts its own URL and body shape.
        for (model, want_url) in [
            ("muse-spark-1.3-contributor", "https://opencode.ai/inference/go/openai/v1/responses"),
            ("claude-haiku-5-5", "https://opencode.ai/inference/go/anthropic/v1/messages"),
            ("gemini-4-argon-contributor", "https://opencode.ai/inference/go/google/v1beta/models/gemini-4-argon-contributor:generateContent"),
            ("mimo-v2.6-flash", "https://opencode.ai/inference/go/openai/v1/chat/completions"),
        ] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::<HttpRequest>::new()));
            let log = seen.clone();
            let http: HttpFn = Arc::new(move |r: &HttpRequest| {
                if r.method == "GET" {
                    return Ok((200, b"{\"data\":[]}".to_vec()));
                }
                log.lock().unwrap().push(r.clone());
                Ok((200, strict_answer(go_protocol(model)).to_string().into_bytes()))
            });
            let mut b = ApiBackend::start(go_cfg(model, http), go_ctx()).unwrap();
            b.prompt(&[PromptPart::Text("hi".into())]);
            wait_for_seen(&seen);
            let reqs = seen.lock().unwrap();
            assert_eq!(reqs[0].url, want_url, "{model}");
            let body: Value = serde_json::from_slice(reqs[0].body.as_deref().unwrap_or(b"null")).unwrap();
            assert!(body.get("tools").is_none(), "{model}: no MCP tools in this session, none sent");
            drop(reqs);
            b.shutdown(true);
        }
    }

    /// A Go model the map does not know falls through protocols on the gateway's
    /// mismatch wording; other refusals stop at once, on one request.
    #[test]
    fn go_unknown_models_fall_through_protocols() {
        let urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = urls.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(r.url.clone());
            if r.url.ends_with("/chat/completions") {
                return Ok((
                    400,
                    json!({"error": {"message": "Model something-new does not support this protocol. Use /responses."}}).to_string().into_bytes(),
                ));
            }
            Ok((200, json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "Hi."}]}]}).to_string().into_bytes()))
        });
        let mut b = ApiBackend::start(go_cfg("something-new", http), go_ctx()).unwrap();
        b.prompt(&[PromptPart::Text("hi".into())]);
        let events = wait_for_turn(&mut b);
        assert!(events.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Hi.")), "{events:?}");
        let tried = urls.lock().unwrap();
        assert_eq!(tried.len(), 2, "{tried:?}");
        assert!(tried[0].ends_with("/chat/completions") && tried[1].ends_with("/responses"), "{tried:?}");
        drop(tried);
        b.shutdown(true);
        // Other refusals never cascade: one request, then the error.
        let seen = Arc::new(std::sync::Mutex::new(0_usize));
        let count = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            *count.lock().unwrap() += 1;
            Ok((400, json!({"error": {"message": "context length exceeded"}}).to_string().into_bytes()))
        });
        let mut b = ApiBackend::start(go_cfg("mimo-v2.6-flash", http), go_ctx()).unwrap();
        b.prompt(&[PromptPart::Text("hi".into())]);
        let events = wait_for_turn(&mut b);
        assert_eq!(*seen.lock().unwrap(), 1);
        assert!(events.iter().any(|e| matches!(e, Event::Error(_))), "{events:?}");
        b.shutdown(true);
    }

    /// A Go per-path credential refusal falls through; a non-Go 401 never does.
    #[test]
    fn go_credential_refusals_fall_through() {
        let urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = urls.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(r.url.clone());
            if r.url.ends_with("/messages") {
                return Ok((401, json!({"error": {"message": "Upstream request failed: Go requires a credential"}}).to_string().into_bytes()));
            }
            Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "Hi."}}]}).to_string().into_bytes()))
        });
        let mut b = ApiBackend::start(go_cfg("qwen3.8-max", http), go_ctx()).unwrap();
        b.prompt(&[PromptPart::Text("hi".into())]);
        let events = wait_for_turn(&mut b);
        assert!(events.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Hi.")), "{events:?}");
        let tried = urls.lock().unwrap();
        assert!(tried.len() >= 2 && tried[0].ends_with("/messages"), "{tried:?}");
        drop(tried);
        b.shutdown(true);
        // A non-Go 401 never cascades: one request, then the error.
        let seen = Arc::new(std::sync::Mutex::new(0_usize));
        let count = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            *count.lock().unwrap() += 1;
            Ok((401, json!({"error": {"message": "bad key"}}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "m", None, http, "");
        let mut b = ApiBackend::start(cfg, go_ctx()).unwrap();
        b.prompt(&[PromptPart::Text("hi".into())]);
        let events = wait_for_turn(&mut b);
        assert_eq!(*seen.lock().unwrap(), 1);
        assert!(events.iter().any(|e| matches!(e, Event::Error(_))), "{events:?}");
        b.shutdown(true);
    }

    /// Gemini takes a minimal schema subset: unknown keywords are dropped,
    /// non-object nodes become plain strings, parameters stay objects.
    #[test]
    fn go_gemini_schemas_are_reduced_to_the_supported_subset() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {
                "commands": {"type": "array", "additionalProperties": true, "items": {"type": "string", "default": "x"}},
                "filter": {"description": "q"},
                "bad": [1, 2],
            },
            "required": ["commands"],
        });
        let clean = go_gemini_schema(&schema);
        assert_eq!(clean["type"], "object");
        assert!(clean.get("additionalProperties").is_none() && clean.get("$schema").is_none(), "{clean}");
        assert_eq!(clean.pointer("/properties/commands/type"), Some(&json!("array")));
        assert!(clean.pointer("/properties/commands/additionalProperties").is_none(), "{clean}");
        assert_eq!(clean.pointer("/properties/commands/items"), Some(&json!({"type": "string"})), "{clean}");
        assert_eq!(clean.pointer("/properties/filter/type"), Some(&json!("string")));
        assert_eq!(clean.pointer("/properties/bad/type"), Some(&json!("string")));
        assert_eq!(clean["required"], json!(["commands"]));
        let tools = go_gemini_tools(&[json!({"type": "function", "function": {"name": "batch", "parameters": schema}})]);
        assert!(tools[0].pointer("/parameters/additionalProperties").is_none(), "{tools:?}");
    }

    /// An endpoint that refuses the listing still shows the configured model, and says why first.
    #[test]
    fn a_refused_listing_keeps_the_configured_model_and_says_why() {
        let http: HttpFn = Arc::new(|r: &HttpRequest| {
            assert_eq!(r.method, "GET");
            Ok((401, json!({"error": {"message": "No auth credentials"}}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "local", None, http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        let mut events = Vec::new();
        let start = std::time::Instant::now();
        while !events.iter().any(|e| matches!(e, Event::Models { .. })) && start.elapsed() < Duration::from_secs(10) {
            events.extend(b.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        let listing: Vec<&Event> = events.iter().filter(|e| matches!(e, Event::Models { .. } | Event::ModelsNote(_))).collect();
        let [Event::ModelsNote(Some(note)), Event::Models { current, available }] = listing[..] else { panic!("{events:?}") };
        assert!(note.starts_with("HTTP 401: ") && !note.contains("No auth credentials"), "{note}");
        assert_eq!(current, "local");
        assert_eq!(available.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["local"]);
        b.shutdown(true);
    }

    /// OpenRouter's shape: the name is the description, whatever else an entry holds is ignored, an
    /// id that could not be sent back is left out, and the list stays within its caps.
    #[test]
    fn a_listing_keeps_the_names_and_stays_within_its_caps() {
        let mut data = vec![
            json!({"id": "openai/gpt-x", "name": "OpenAI: GPT X", "description": "Long text", "supported_parameters": ["tools"]}),
            json!({"id": "same", "name": "same", "display_name": "Same One"}),
            json!({"id": "long", "name": "n".repeat(MODEL_NAME_CAP + 10), "display_name": "d".repeat(MODEL_NAME_CAP + 10)}),
            json!({"id": "x".repeat(super::super::MODEL_ID_CAP + 1)}),
            json!({"id": "bell\u{7}"}),
            json!({"id": ""}),
            json!({"id": 7}),
            json!({"name": "no id"}),
        ];
        data.extend((0..MODEL_LIST_CAP + 500).map(|i| json!({"id": format!("m{i}")})));
        let body = json!({"data": data}).to_string().into_bytes();
        let http: HttpFn = Arc::new(move |_: &HttpRequest| Ok((200, body.clone())));
        let list = model_list(&http, "https://openrouter.ai/api/v1", None).unwrap();
        assert_eq!(list.len(), MODEL_LIST_CAP);
        let [first, same, long, next] = &list[..4] else { panic!("{}", list.len()) };
        assert_eq!((first.id.as_str(), first.name.as_str(), first.description.as_str()), ("openai/gpt-x", "openai/gpt-x", "OpenAI: GPT X"));
        assert_eq!((same.id.as_str(), same.description.as_str()), ("same", ""));
        assert_eq!(long.description.chars().count(), MODEL_NAME_CAP);
        assert_eq!(next.id, "m0");
        let list = anthropic_model_list(&http, "https://openrouter.ai/api/v1", None).unwrap();
        assert_eq!(list.len(), MODEL_LIST_CAP);
        let [first, same, long] = &list[..3] else { panic!("{}", list.len()) };
        assert_eq!(first.name, "openai/gpt-x");
        assert_eq!(same.name, "Same One");
        assert_eq!(long.name.chars().count(), MODEL_NAME_CAP);
        assert!(test_endpoint(&http, "https://openrouter.ai/api/v1", None, "openai/gpt-x").unwrap().contains(&MODEL_LIST_CAP.to_string()));
    }

    /// A 200 that is no model list (a web page where the endpoint should be) is an error, so the
    /// picker says why and asks again later; a list with nothing in it is a list.
    #[test]
    fn an_answer_that_is_no_model_list_is_an_error() {
        let expected = crate::i18n::tr("ui.agent_chat_models_not_a_list").to_string();
        let page: HttpFn = Arc::new(|_: &HttpRequest| Ok((200, b"<!doctype html><html><body>Control UI</body></html>".to_vec())));
        assert_eq!(model_list(&page, "http://localhost:18789/v1", None).unwrap_err(), expected);
        assert_eq!(anthropic_model_list(&page, "http://localhost:18789/v1", None).unwrap_err(), expected);
        assert_eq!(test_endpoint(&page, "http://localhost:18789/v1", None, "").unwrap_err(), expected);
        let object: HttpFn = Arc::new(|_: &HttpRequest| Ok((200, json!({"object": "list"}).to_string().into_bytes())));
        assert_eq!(model_list(&object, "http://localhost:18789/v1", None).unwrap_err(), expected);
        let empty: HttpFn = Arc::new(|_: &HttpRequest| Ok((200, json!({"data": []}).to_string().into_bytes())));
        assert!(model_list(&empty, "http://localhost:18789/v1", None).unwrap().is_empty());
    }

    #[test]
    fn tool_arguments_are_an_object_or_an_error() {
        assert_eq!(tool_arguments(None), Ok(json!({})));
        assert_eq!(tool_arguments(Some(&json!(""))), Ok(json!({})));
        assert_eq!(tool_arguments(Some(&json!("{\"a\": 1}"))), Ok(json!({"a": 1})));
        assert_eq!(tool_arguments(Some(&json!({"a": 1}))), Ok(json!({"a": 1})));
        for bad in [json!("{command: solid.box"), json!("[1]"), json!("7"), json!(7), json!([1])] {
            assert!(tool_arguments(Some(&bad)).is_err(), "{bad}");
        }
    }

    /// A call with arguments that are not JSON is answered with the reason, not run with `{}`.
    #[test]
    fn a_call_with_broken_arguments_is_not_run() {
        let answers = std::sync::Mutex::new(vec![
            json!({"choices": [{"message": {"role": "assistant", "content": "Sorry."}}]}),
            json!({"choices": [{"message": {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "batch", "arguments": "{commands: [oops"}}]}}]}),
        ]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
            Ok((200, answers.lock().unwrap().pop().unwrap_or(json!({})).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "m", None, http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        b.prompt(&[PromptPart::Text("go".into())]);
        let events = wait_for_turn(&mut b);
        let failed = events.iter().find_map(|e| match e {
            Event::Tool { status: Some(ToolStatus::Failed), output: Some(o), .. } => Some(o.clone()),
            _ => None,
        });
        assert!(failed.is_some_and(|o| o.contains("Not run") && o.contains("not valid JSON")), "{events:?}");
        // The model was told, in the next request.
        let sent = seen.lock().unwrap();
        let last = sent[1]["messages"].as_array().unwrap().last().unwrap().clone();
        assert!(last["content"].as_str().unwrap_or("").contains("not valid JSON"), "{last}");
        b.shutdown(true);
    }

    #[test]
    fn keys_never_go_over_plain_http_to_another_computer() {
        let http: HttpFn = Arc::new(|_: &HttpRequest| Ok((200, b"{}".to_vec())));
        assert!(test_endpoint(&http, "http://10.0.0.2:11434/v1", Some("k"), "m").is_err());
        let cfg = direct_config(Protocol::OpenAi, "http://10.0.0.2/v1", "m", Some("k"), http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        assert!(ApiBackend::start(cfg, ctx).is_err());
    }

    /// A worker with one function tool whose endpoint answers each POST with the next scripted
    /// (status, body); the requests it received are returned too.
    fn scripted_worker(answers: Vec<(u16, Value)>) -> (Worker, Arc<std::sync::Mutex<Vec<Value>>>) {
        scripted_worker_effort(answers, "")
    }

    /// [`scripted_worker`] with the endpoint's reasoning effort set (empty: send nothing).
    fn scripted_worker_effort(answers: Vec<(u16, Value)>, effort: &str) -> (Worker, Arc<std::sync::Mutex<Vec<Value>>>) {
        let answers = std::sync::Mutex::new(answers.into_iter().rev().collect::<Vec<_>>());
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap()).unwrap());
            let (status, v) = answers.lock().unwrap().pop().expect("no request beyond the script");
            Ok((status, v.to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::OpenAi, "http://localhost:1/v1", "m", Some("sk-secret"), http, effort);
        let binding = cfg.host_ai.bind(&cfg.connection_id, &cfg.model).unwrap();
        let (events, _rx) = channel();
        let w = Worker {
            cfg,
            binding,
            protocol: Protocol::OpenAi,
            mcp: None,
            next: 0,
            tools: vec![json!({"type": "function", "function": {"name": "measure"}})],
            tool_names: vec!["measure".into()],
            history: vec![json!({"role": "user", "content": "hi"})],
            note: super::super::acp::SYSTEM_NOTE.to_string(),
            session_id: "test-session".to_string(),
            gemini_sigs: Default::default(),
            events,
            wake: Arc::new(|| {}),
            turn: Arc::new(TurnControl::new()),
            in_turn: true,
            no_reasoning: Default::default(),
        };
        (w, seen)
    }

    fn refusal() -> (u16, Value) {
        (400, json!({"error": {"message": "Function tools with reasoning_effort are not supported for gpt-6.1-sol in /v1/chat/completions."}}))
    }

    /// Newer OpenAI models answer 400 to function tools without `reasoning_effort: none`: the
    /// request is replayed once with it, and the next ones carry it at once.
    #[test]
    fn a_refusal_of_tools_with_reasoning_effort_is_replayed_once_with_none() {
        let ok = json!({"choices": [{"message": {"role": "assistant", "content": "Done."}}]});
        let (mut w, seen) = scripted_worker(vec![refusal(), (200, ok.clone()), (200, ok.clone())]);
        assert_eq!(w.request().unwrap().assistant, ok["choices"][0]["message"]);
        assert_eq!(w.request().unwrap().assistant, ok["choices"][0]["message"]);
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert!(sent[0].get("reasoning_effort").is_none());
        assert_eq!(sent[1]["reasoning_effort"], "none");
        assert_eq!(sent[2]["reasoning_effort"], "none");
    }

    /// No loop: a second refusal is the error, after exactly two requests; the message shows the
    /// safe status/category but never provider words or the key; an unrelated 400 is not replayed at all.
    #[test]
    fn the_replay_happens_once_and_only_for_that_refusal() {
        let (mut w, seen) = scripted_worker(vec![refusal(), refusal()]);
        let e = w.request().unwrap_err();
        assert!(e.starts_with("HTTP 400") && !e.contains("reasoning_effort") && !e.contains("sk-secret"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), 2);
        let (mut w, seen) = scripted_worker(vec![(400, json!({"error": {"message": "context length exceeded"}}))]);
        let e = w.request().unwrap_err();
        assert!(e.starts_with("HTTP 400") && !e.contains("context length"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// A reasoning effort the user chose goes out as `reasoning_effort` on every request; empty
    /// sends nothing, so existing agents are untouched (#10241).
    #[test]
    fn a_chosen_reasoning_effort_goes_out_on_every_request() {
        let ok = json!({"choices": [{"message": {"role": "assistant", "content": "Done."}}]});
        let (mut w, seen) = scripted_worker_effort(vec![(200, ok.clone()), (200, ok.clone())], "high");
        assert_eq!(w.request().unwrap().assistant, ok["choices"][0]["message"]);
        assert_eq!(w.request().unwrap().assistant, ok["choices"][0]["message"]);
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["reasoning_effort"], "high");
        assert_eq!(sent[1]["reasoning_effort"], "high");
        drop(sent);
        // Empty effort: nothing is sent at all.
        let (mut w, seen) = scripted_worker(vec![(200, ok)]);
        assert!(w.request().is_ok());
        assert!(seen.lock().unwrap()[0].get("reasoning_effort").is_none());
    }

    /// A chosen effort wins over the #3817 `none` retry: the endpoint's refusal of the effort is
    /// shown to the user after exactly one request, never silently replaced by `none` (#10241).
    #[test]
    fn a_chosen_reasoning_effort_wins_over_the_none_retry() {
        let (mut w, seen) = scripted_worker_effort(vec![refusal()], "max");
        let e = w.request().unwrap_err();
        assert!(e.starts_with("HTTP 400") && !e.contains("reasoning_effort"), "{e}");
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 1, "the refusal is shown, not replayed with none");
        assert_eq!(sent[0]["reasoning_effort"], "max");
    }

    #[test]
    fn mcp_tools_become_anthropic_tools_without_crash_reports() {
        let list = json!({"tools": [
            {"name": "batch", "description": "Run commands", "inputSchema": {"type": "object", "properties": {"commands": {"type": "array"}}}},
            {"name": "crash_reports", "description": "x", "inputSchema": {"type": "object"}},
            {"name": "measure"},
        ]});
        let t = anthropic_tools(&list);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0]["name"], "batch");
        assert_eq!(t[0]["input_schema"]["properties"]["commands"]["type"], "array");
        assert_eq!(t[1]["input_schema"]["type"], "object");
        assert_eq!(tool_names(&list), ["batch", "measure"]);
    }

    #[test]
    fn anthropic_answers_read_text_thought_and_tool_calls() {
        let r = anthropic_reply(&json!({"content": [
            {"type": "thinking", "thinking": "checking", "signature": "s"},
            {"type": "text", "text": "Measuring."},
            {"type": "tool_use", "id": "t1", "name": "measure", "input": {"body": "b"}},
        ]}));
        assert_eq!(r.text, [(true, "checking".to_string()), (false, "Measuring.".to_string())]);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].id, "t1");
        assert_eq!(r.calls[0].name, "measure");
        assert_eq!(r.calls[0].arguments, Ok(json!({"body": "b"})));
        assert_eq!(r.assistant["role"], "assistant");
        assert_eq!(r.assistant["content"][2]["type"], "tool_use");
        let broken = anthropic_reply(&json!({"content": [{"type": "tool_use", "name": "measure", "input": "no"}]}));
        assert!(broken.calls[0].arguments.is_err());
        // Calls beyond the loop's cap are left out of the answer too: each kept one is answered.
        let many: Vec<Value> = (0..40).map(|i| json!({"type": "tool_use", "id": format!("t{i}"), "name": "measure", "input": {}})).collect();
        let capped = anthropic_reply(&json!({"content": many}));
        assert_eq!(capped.calls.len(), 32);
        assert_eq!(capped.assistant["content"].as_array().unwrap().len(), 32);
    }

    /// Why an answer stopped reaches the turn's end: cut off at the length limit, or declined,
    /// in either protocol (#9495). A call cut off mid-way is neither run nor kept in the history.
    #[test]
    fn answers_say_why_they_stopped() {
        let thinking = json!({"type": "thinking", "thinking": "", "signature": "s"});
        let call = json!({"type": "tool_use", "id": "t1", "name": "measure", "input": {}});
        for (reason, stop) in [("end_turn", ""), ("tool_use", ""), ("max_tokens", "max_tokens"), ("refusal", "refusal"), ("made_up", "")] {
            assert_eq!(anthropic_reply(&json!({"content": [thinking], "stop_reason": reason})).stop, stop, "{reason}");
        }
        assert_eq!(anthropic_reply(&json!({"content": [thinking]})).stop, "");
        let cut = anthropic_reply(&json!({"content": [thinking, call], "stop_reason": "max_tokens"}));
        assert!(cut.calls.is_empty());
        assert_eq!(cut.assistant["content"], json!([thinking]));

        let openai = |finish: &str, message: Value| openai_reply(&json!({"choices": [{"message": message, "finish_reason": finish}]}));
        let calls = json!([{"id": "c1", "type": "function", "function": {"name": "measure", "arguments": "{\"a\""}}]);
        assert_eq!(openai("stop", json!({"content": "Hi"})).stop, "");
        assert_eq!(openai("content_filter", json!({"content": null})).stop, "refusal");
        assert_eq!(openai("stop", json!({"content": null, "refusal": "I can't help with that."})).stop, "refusal");
        let cut = openai("length", json!({"content": null, "tool_calls": calls}));
        assert_eq!(cut.stop, "max_tokens");
        assert!(cut.calls.is_empty());
        assert!(cut.assistant.get("tool_calls").is_none());
    }

    /// The loop ends the turn with the reason: an answer that is only thinking, cut off at the
    /// length limit, ends as `max_tokens`, not as a normal end with nothing said.
    #[test]
    fn a_cut_off_answer_ends_the_turn_as_max_tokens() {
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            let body = json!({"content": [{"type": "thinking", "thinking": "", "signature": "s"}], "stop_reason": "max_tokens"});
            Ok((200, body.to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::Anthropic, "http://localhost:1", "m", Some("sk-secret"), http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        b.prompt(&[PromptPart::Text("look closer".into())]);
        let events = wait_for_turn(&mut b);
        b.shutdown(true);
        assert!(matches!(events.last(), Some(Event::TurnEnded { reason }) if reason == "max_tokens"), "{events:?}");
    }

    #[test]
    fn anthropic_urls_take_the_base_with_or_without_v1() {
        assert_eq!(endpoint("https://api.anthropic.com", "messages"), "https://api.anthropic.com/v1/messages");
        assert_eq!(endpoint("https://api.anthropic.com/v1", "messages"), "https://api.anthropic.com/v1/messages");
        assert_eq!(endpoint("https://api.anthropic.com/v1/", "messages"), "https://api.anthropic.com/v1/messages");
        assert_eq!(endpoint("https://api.deepseek.com/anthropic/", "models"), "https://api.deepseek.com/anthropic/v1/models");
        assert_eq!(
            endpoint("https://dashscope.aliyuncs.com/apps/anthropic", "messages"),
            "https://dashscope.aliyuncs.com/apps/anthropic/v1/messages"
        );
    }

    /// A scripted Anthropic endpoint: one answer with a tool call, then a plain answer. The loop
    /// runs the tool (here without SolidCraft tools: an unknown tool fails) and reports the end.
    #[test]
    fn the_anthropic_tool_loop_runs_calls_and_ends_the_turn() {
        let answers = std::sync::Mutex::new(vec![
            json!({"content": [{"type": "text", "text": "Done."}]}),
            json!({"content": [
                {"type": "text", "text": "Measuring."},
                {"type": "tool_use", "id": "t1", "name": "measure", "input": {}},
            ]}),
        ]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
            Ok((200, answers.lock().unwrap().pop().unwrap_or(json!({})).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::Anthropic, "http://localhost:1", "m", Some("sk-secret"), http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        b.prompt(&[PromptPart::Text("measure it".into())]);
        let events = wait_for_turn(&mut b);
        assert_eq!(events.first(), Some(&Event::ToolsAvailable(false)));
        assert!(events.iter().any(|e| matches!(e, Event::Ready { .. })), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::Tool { status: Some(ToolStatus::Failed), .. })), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Done.")), "{events:?}");
        assert!(matches!(events.last(), Some(Event::TurnEnded { reason }) if reason.is_empty()), "{events:?}");
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["system"], super::super::acp::SYSTEM_NOTE);
        assert_eq!(sent[0]["max_tokens"], 8192);
        assert_eq!(sent[1]["messages"].as_array().unwrap().last().unwrap()["content"][0]["type"], "tool_result");
        assert_eq!(sent[1]["messages"].as_array().unwrap().last().unwrap()["content"][0]["tool_use_id"], "t1");
        b.shutdown(true);
    }

    /// Thinking blocks are signed against the conversation before them: once an older view is
    /// dropped, the earlier ones go too, or Claude refuses the request (#9417). Until then, and
    /// for replies after the drop, they are sent back as they came.
    #[test]
    fn a_dropped_view_takes_earlier_thinking_with_it() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            if r.method == "GET" {
                return Ok((200, b"{\"data\":[]}".to_vec()));
            }
            let n = {
                let mut l = log.lock().unwrap();
                l.push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
                l.len()
            };
            let content = if n == 2 {
                json!([{"type": "redacted_thinking", "data": "r2"}])
            } else {
                json!([{"type": "thinking", "thinking": "", "signature": format!("s{n}")}, {"type": "text", "text": format!("Seen {n}.")}])
            };
            Ok((200, json!({"content": content, "stop_reason": "end_turn"}).to_string().into_bytes()))
        });
        let cfg = direct_config(Protocol::Anthropic, "http://localhost:1", "m", Some("sk-secret"), http, "");
        let ctx = StartContext { cwd: std::env::temp_dir(), bridge: None, tools: None, wake: Box::new(|| {}) };
        let mut b = ApiBackend::start(cfg, ctx).unwrap();
        for n in 1..=3 {
            b.prompt(&[PromptPart::Text(format!("look {n}")), PromptPart::Image { mime: "image/png".into(), base64: format!("V{n}") }]);
            wait_for_turn(&mut b);
        }
        b.prompt(&[PromptPart::Text("and now?".into())]);
        wait_for_turn(&mut b);
        b.shutdown(true);
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 4);
        let replies = |i: usize| -> Vec<Value> {
            sent[i]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").map(|m| m["content"].clone()).collect()
        };
        // Two views fit: the reply goes back untouched, signature and all.
        assert_eq!(replies(1)[0][0]["signature"], "s1");
        // The third view drops the first: no earlier thinking is left, a thinking-only reply
        // keeps a note, and the words stay.
        let third = replies(2);
        assert_eq!(sent[2]["messages"][0]["content"][1]["text"], DROPPED_VIEW_NOTE);
        assert_eq!(third[0], json!([{"type": "text", "text": "Seen 1."}]));
        assert_eq!(third[1], json!([{"type": "text", "text": DROPPED_THINKING_NOTE}]));
        // Nothing is dropped for the next message, so the reply after the drop keeps its thinking.
        let fourth = replies(3);
        assert_eq!(fourth[..2], third[..]);
        assert_eq!(fourth[2][0]["signature"], "s3");
    }

    #[test]
    fn the_anthropic_test_sends_one_small_message_and_checks_the_key_rule() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<HttpRequest>::new()));
        let log = seen.clone();
        let http: HttpFn = Arc::new(move |r: &HttpRequest| {
            log.lock().unwrap().push(r.clone());
            Ok((200, json!({"content": [{"type": "text", "text": "Hi"}]}).to_string().into_bytes()))
        });
        let message = test_anthropic_endpoint(&http, "https://api.anthropic.com/v1", Some("sk-k"), "claude-opus-5-5").unwrap();
        assert!(message.contains("claude-opus-5-5"), "{message}");
        let sent = seen.lock().unwrap();
        assert_eq!(sent[0].url, "https://api.anthropic.com/v1/messages");
        assert!(sent[0].headers.iter().any(|(k, v)| k == "anthropic-version" && v == ANTHROPIC_VERSION));
        assert!(sent[0].headers.iter().any(|(k, v)| k == "x-api-key" && v == "sk-k"));
        let body: Value = serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 16);
        assert_eq!(body["messages"][0]["content"], "Hello");
        drop(sent);
        assert!(test_anthropic_endpoint(&http, "http://10.0.0.2/anthropic", Some("k"), "m").is_err());
        assert!(test_anthropic_endpoint(&http, "https://api.anthropic.com", None, "").is_err());
    }
}
