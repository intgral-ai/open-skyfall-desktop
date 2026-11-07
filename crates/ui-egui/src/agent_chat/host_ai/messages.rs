//! Strict normalized admission and authorized artifact adaptation, not a wire envelope.
//! All caller-controlled metadata is checked before storage I/O or payload copying.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

use super::go;
use super::provider::{ProviderCall, ProviderState, WireProtocol, capabilities, validate_options};
use super::{AiError, AiLimits, ArtifactReadFn, ArtifactRef, ConnectionBinding, ContentPart, GenerateRequest, MessageRole, ModelMessage, SessionKey};
use crate::agent_chat::agents::Protocol;
use crate::agent_chat::backend::base64;

pub(in crate::agent_chat) fn prepare_messages(
    session: &SessionKey,
    binding: &ConnectionBinding,
    request: &GenerateRequest,
    limits: &AiLimits,
    artifacts: &ArtifactReadFn,
    private: &ProviderState,
) -> Result<ProviderCall, AiError> {
    let (wire, references) = validate_request(session, binding, request, limits)?;
    let payload_bound = provider_payload_bound(binding, request, limits, private)?;

    // Resolve exact scoped immutable references only. Check every returned buffer independently
    // before encoding any of them; there is no URL/path/URI or PromptPart fallback here.
    let mut resolved = Vec::with_capacity(references.len());
    let mut total = 0usize;
    for reference in references {
        let bytes = artifacts(session, reference)?;
        bound(bytes.len(), limits.attachment_bytes, "attachment_bytes")?;
        total = total.checked_add(bytes.len()).ok_or_else(|| AiError::exhausted("attachment_total_bytes", limits.attachment_total_bytes))?;
        bound(total, limits.attachment_total_bytes, "attachment_total_bytes")?;
        if bytes.len() != reference.size_bytes as usize {
            return Err(AiError::invalid("artifact"));
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != reference.digest {
            return Err(AiError::invalid("artifact"));
        }
        let format = image::guess_format(&bytes).map_err(|_| AiError::invalid("media_type"))?;
        if !matches!((reference.media_type.as_str(), format), ("image/png", image::ImageFormat::Png) | ("image/jpeg", image::ImageFormat::Jpeg)) {
            return Err(AiError::invalid("media_type"));
        }
        resolved.push((reference, bytes));
    }
    let encoded: Vec<_> = resolved
        .iter()
        .map(|(reference, bytes)| {
            let expected = base64_bytes(bytes.len())?;
            let data = base64(bytes);
            if data.len() != expected {
                return Err(AiError::internal("encode_artifact"));
            }
            Ok((*reference, data))
        })
        .collect::<Result<_, AiError>>()?;
    let (system, history) = encode_history(request, wire, &encoded, private)?;
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            match wire {
                WireProtocol::Chat => {
                    json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})
                }
                WireProtocol::Responses => json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema}),
                WireProtocol::Messages => json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema}),
                // Native declarations bypass only the legacy reducer, not strict admission.
                WireProtocol::Gemini => json!({"name":tool.name,"description":tool.description,"parameters":tool.input_schema}),
            }
        })
        .collect();
    // Check the actual native projection with serde's encoder, without allocating a JSON
    // buffer. The physical worker retains the original provider encoder/dispatch contract.
    let system_bytes = serialized_bytes(&system, payload_bound, "provider_payload_bytes")?;
    let remaining = payload_bound.checked_sub(system_bytes).ok_or_else(|| AiError::internal("provider_payload_bound"))?;
    let history_bytes = serialized_bytes(&history, remaining, "provider_payload_bytes")?;
    serialized_bytes(
        &tools,
        remaining.checked_sub(history_bytes).ok_or_else(|| AiError::internal("provider_payload_bound"))?,
        "provider_payload_bytes",
    )?;
    let effort = request.options.reasoning_effort.as_deref().or_else(|| {
        let default = binding.configuration().reasoning_effort.as_str();
        (!default.is_empty()).then_some(default)
    });
    Ok(ProviderCall {
        wire,
        system,
        history,
        tools,
        max_output_tokens: request.options.max_output_tokens,
        temperature: request.options.temperature,
        reasoning_effort: effort.map(str::to_owned),
        timeout_seconds: request.options.timeout_seconds.into(),
        http_per_connection: limits.http_per_connection,
        http_per_host: limits.http_per_host,
        // Orchestration attaches the turn's budget after continuation reservation.
        model_budget: None,
        private: private.clone(),
    })
}

/// Metadata-only admission shared by preparation and pre-HTTP continuation reservation.
pub(super) fn validate_request<'a>(
    session: &SessionKey,
    binding: &ConnectionBinding,
    request: &'a GenerateRequest,
    limits: &AiLimits,
) -> Result<(WireProtocol, Vec<&'a ArtifactRef>), AiError> {
    limits.validate()?;
    if !valid_id(&session.runtime_instance_id) || !valid_id(&session.session_id) {
        return Err(AiError::invalid("session"));
    }
    if request.connection_id != binding.connection_id() || request.model_id != binding.model_id() {
        return Err(AiError::invalid("binding"));
    }
    validate_options(binding, &request.options)?;
    if request.options.timeout_seconds > limits.call_max_seconds {
        return Err(AiError::invalid("timeout_seconds"));
    }
    let wire = if binding.protocol() == Protocol::OpenAi && go::is_opencode_go(&binding.configuration().base_url) {
        go::go_protocol(binding.model_id())
    } else {
        binding.protocol().into()
    };
    bound(request.messages.len(), limits.model_messages, "model_messages")?;
    bound(request.tools.len(), limits.tool_definitions, "tool_definitions")?;
    // This domain admission is independent of any upstream IPC frame admission. Even an
    // empty-part flood must fail before walking/copying all parts or consulting artifacts.
    normalized_prefix_charge(&request.messages, 0, limits)?;
    let references = validate_input(request, limits, wire)?;
    let caps = capabilities(binding);
    if !caps.tools && !request.tools.is_empty() {
        return Err(AiError::unsupported("tools"));
    }
    if !caps.images && !references.is_empty() {
        return Err(AiError::unsupported("images"));
    }

    Ok((wire, references))
}

fn bound(count: usize, limit: u32, resource: &'static str) -> Result<(), AiError> {
    if count > limit as usize { Err(AiError::exhausted(resource, limit)) } else { Ok(()) }
}

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}

/// Canonical, additive normalized *message prefix* reservation, shared with Task6.
///
/// Per message: size_of<ModelMessage> + size_of<Vec<ModelMessage>> (conservatively
/// charging the outer vector header again for each entry). Per content part/call: its
/// typed size, including empty parts. Add the raw UTF-8 bytes of every owned string;
/// artifacts contribute only their reference's strings (digest/size are inline).
/// Arguments add compact JSON bytes AND Value/key structural sizes, not just payload.
/// This is a pessimistic logical storage charge, not an allocator/RSS measurement.
///
/// `initial` is an already canonical charge; Task6 can append the returned assistant
/// with this same function without independently estimating the prefix or double-counting
/// an outer envelope. Tools/options and provider-private state are NOT message prefix;
/// tools/options have separate input bounds, and Task6 must reserve its private state too.
pub(in crate::agent_chat) fn normalized_prefix_charge(messages: &[ModelMessage], initial: usize, limits: &AiLimits) -> Result<usize, AiError> {
    let mut charge = initial;
    let limit = limits.continuation_bytes;
    for message in messages {
        add_prefix(&mut charge, size_of::<ModelMessage>() + size_of::<Vec<ModelMessage>>(), limit)?;
        let parts = message.content.len().checked_mul(size_of::<ContentPart>()).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?;
        add_prefix(&mut charge, parts, limit)?;
        let calls =
            message.tool_calls.len().checked_mul(size_of::<super::ToolCall>()).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?;
        add_prefix(&mut charge, calls, limit)?;
        if let Some(id) = &message.tool_call_id {
            add_prefix(&mut charge, id.len(), limit)?;
        }
        for part in &message.content {
            let reference = match part {
                ContentPart::Text(text) => {
                    add_prefix(&mut charge, text.len(), limit)?;
                    continue;
                }
                ContentPart::Image(reference) => reference,
                ContentPart::Document { name, artifact } => {
                    add_prefix(&mut charge, name.len(), limit)?;
                    artifact
                }
            };
            add_prefix(&mut charge, reference.artifact_id.len(), limit)?;
            add_prefix(&mut charge, reference.media_type.len(), limit)?;
        }
        for call in &message.tool_calls {
            add_prefix(&mut charge, call.id.len(), limit)?;
            add_prefix(&mut charge, call.name.len(), limit)?;
            let json = bounded_object(&call.arguments, limits.tool_json_bytes)?;
            add_prefix(&mut charge, json.bytes, limit)?;
            add_prefix(
                &mut charge,
                json.nodes.checked_mul(size_of::<Value>()).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?,
                limit,
            )?;
            add_prefix(
                &mut charge,
                json.keys.checked_mul(size_of::<String>()).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?,
                limit,
            )?;
        }
    }
    add_prefix(&mut charge, 0, limit)?;
    Ok(charge)
}

fn add_prefix(charge: &mut usize, bytes: usize, limit: u32) -> Result<(), AiError> {
    *charge = charge.checked_add(bytes).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?;
    bound(*charge, limit, "continuation_bytes")
}

fn base64_bytes(bytes: usize) -> Result<usize, AiError> {
    bytes.checked_add(2).map(|n| n / 3).and_then(|n| n.checked_mul(4)).ok_or_else(|| AiError::internal("provider_payload_bound"))
}

/// A derived encoder bound, NOT another quota or an IPC envelope allowance. UTF-8
/// JSON strings expand at most sixfold; call names/IDs occur at most twice (call/result).
/// Typed structural fees dominate the corresponding native per-item keys/delimiters.
/// Empty request header/options use the actual existing provider encoder. Base64 adds
/// exactly sum(4*ceil(bytes/3)) separately, never against continuation_bytes.
pub(in crate::agent_chat) fn provider_payload_bound(
    binding: &ConnectionBinding,
    request: &GenerateRequest,
    limits: &AiLimits,
    private: &ProviderState,
) -> Result<usize, AiError> {
    let overflow = || AiError::internal("provider_payload_bound");
    let mut components = normalized_prefix_charge(&request.messages, 0, limits)?;
    for tool in &request.tools {
        for bytes in [
            size_of::<super::ToolDefinition>(),
            tool.name.len(),
            tool.description.len(),
            bounded_object(&tool.input_schema, limits.tool_json_bytes)?.bytes,
        ] {
            components = components.checked_add(bytes).ok_or_else(overflow)?;
        }
    }
    // These maps are trusted internal continuation state, not supplied domain messages.
    // Task6 owns their storage admission; include their actual wire string contributions.
    for (key, value) in private.wire_to_provider_tool_ids.iter().chain(private.gemini_signatures.iter()) {
        components = components.checked_add(key.len()).and_then(|n| n.checked_add(value.len())).ok_or_else(overflow)?;
    }
    // Replayed native reasoning/signatures are independent of the public prefix. The
    // store charges them separately; include their real escaped bytes in this derived
    // encoder bound as well (never an IPC cap or a base64 reference charge).
    for assistant in &private.native_assistants {
        components =
            components.checked_add(serialized_bytes(assistant, limits.continuation_bytes as usize, "continuation_bytes")?).ok_or_else(overflow)?;
    }
    let wire = if binding.protocol() == Protocol::OpenAi && go::is_opencode_go(&binding.configuration().base_url) {
        go::go_protocol(binding.model_id())
    } else {
        binding.protocol().into()
    };
    let effort = request.options.reasoning_effort.as_deref().or_else(|| {
        let default = binding.configuration().reasoning_effort.as_str();
        (!default.is_empty()).then_some(default)
    });
    let header = super::provider::request_body(super::provider::RequestParts {
        wire,
        model: binding.model_id(),
        system: "",
        history: &[],
        tools: &[],
        go: binding.protocol() == Protocol::OpenAi && go::is_opencode_go(&binding.configuration().base_url),
        signatures: &private.gemini_signatures,
        max_output_tokens: request.options.max_output_tokens,
        temperature: request.options.temperature,
        reasoning_effort: effort,
    });
    let header_bytes = serialized_bytes(&header, usize::MAX, "provider_payload_bytes")?;
    let mut bytes = components.checked_mul(2).and_then(|n| n.checked_mul(6)).and_then(|n| n.checked_add(header_bytes)).ok_or_else(overflow)?;
    for part in request.messages.iter().flat_map(|message| &message.content) {
        if let ContentPart::Image(reference) = part {
            bytes = bytes.checked_add(base64_bytes(reference.size_bytes as usize)?).ok_or_else(overflow)?;
        }
    }
    Ok(bytes)
}

fn validate_input<'a>(request: &'a GenerateRequest, limits: &AiLimits, wire: WireProtocol) -> Result<Vec<&'a ArtifactRef>, AiError> {
    bound(request.messages.len(), limits.model_messages, "model_messages")?;
    bound(request.tools.len(), limits.tool_definitions, "tool_definitions")?;
    if request.messages.is_empty() {
        return Err(AiError::invalid("messages"));
    }
    let mut names = BTreeSet::new();
    for tool in &request.tools {
        if !valid_tool_name(&tool.name) || !names.insert(tool.name.as_str()) {
            return Err(AiError::invalid("tool_name"));
        }
        bound(tool.description.len(), limits.text_bytes, "text_bytes")?;
        bounded_object(&tool.input_schema, limits.tool_json_bytes)?;
        if wire == WireProtocol::Gemini {
            gemini_schema(&tool.input_schema)?;
        }
    }
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut started = false;
    let mut attachments = Vec::new();
    let mut total = 0usize;
    for message in &request.messages {
        bound(message.tool_calls.len(), limits.tool_calls, "tool_calls")?;
        if message.role != MessageRole::Tool && message.tool_call_id.is_some() {
            return Err(AiError::invalid("tool_call_id"));
        }
        if message.role != MessageRole::Assistant && !message.tool_calls.is_empty() {
            return Err(AiError::invalid("tool_calls"));
        }
        if message.content.is_empty() && message.tool_calls.is_empty() {
            return Err(AiError::invalid("content"));
        }
        match message.role {
            MessageRole::Tool => {
                let id = message.tool_call_id.as_deref().filter(|id| valid_id(id)).ok_or_else(|| AiError::invalid("tool_call_id"))?;
                if !pending.remove(id) {
                    return Err(AiError::invalid("tool_call_id"));
                }
            }
            MessageRole::System => {
                if started {
                    return Err(AiError::invalid("role"));
                }
            }
            MessageRole::User | MessageRole::Assistant => {
                if !pending.is_empty() {
                    return Err(AiError::invalid("tool_results"));
                }
                started = true;
            }
        }
        for call in &message.tool_calls {
            if !valid_id(&call.id) || !valid_tool_name(&call.name) || !seen.insert(call.id.as_str()) {
                return Err(AiError::invalid("tool_calls"));
            }
            bounded_object(&call.arguments, limits.tool_json_bytes)?;
            pending.insert(call.id.as_str());
        }
        for part in &message.content {
            match part {
                ContentPart::Text(text) => bound(text.len(), limits.text_bytes, "text_bytes")?,
                ContentPart::Document { .. } => return Err(AiError::unsupported("documents")),
                ContentPart::Image(reference) => {
                    if !matches!(reference.media_type.as_str(), "image/png" | "image/jpeg") {
                        return Err(AiError::unsupported("mediaType"));
                    }
                    if message.role != MessageRole::User || !valid_id(&reference.artifact_id) || reference.size_bytes == 0 {
                        return Err(AiError::invalid("artifact"));
                    }
                    bound(reference.size_bytes as usize, limits.attachment_bytes, "attachment_bytes")?;
                    bound(attachments.len() + 1, limits.attachments, "attachments")?;
                    total = total
                        .checked_add(reference.size_bytes as usize)
                        .ok_or_else(|| AiError::exhausted("attachment_total_bytes", limits.attachment_total_bytes))?;
                    bound(total, limits.attachment_total_bytes, "attachment_total_bytes")?;
                    attachments.push(reference);
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(AiError::invalid("tool_results"));
    }
    Ok(attachments)
}

/// Borrow one iterator per container depth, never one pending reference per child.
/// Reverse traversal preserves the former LIFO walk's order and error classifications.
enum ChargingChildren<'a> {
    Array(std::slice::Iter<'a, Value>),
    Object(serde_json::map::Values<'a>),
}

impl<'a> ChargingChildren<'a> {
    fn next_back(&mut self) -> Option<&'a Value> {
        match self {
            Self::Array(values) => values.next_back(),
            Self::Object(values) => values.next_back(),
        }
    }
}

struct ChargingWorkspace<'a> {
    frames: [Option<ChargingChildren<'a>>; 64],
    depth: usize,
}

/// Temporary contribution to the EXISTING continuation_bytes quota. Continuation's
/// begin/prepare reserve it before entering any retained-data charging walk.
pub(super) const CHARGING_WORKSPACE_BYTES: usize = size_of::<ChargingWorkspace<'static>>();

impl<'a> ChargingWorkspace<'a> {
    fn new() -> Self {
        Self { frames: std::array::from_fn(|_| None), depth: 0 }
    }

    fn push(&mut self, children: ChargingChildren<'a>) -> Result<(), AiError> {
        let frame = self.frames.get_mut(self.depth).ok_or_else(|| AiError::exhausted("tool_json_depth", 64))?;
        *frame = Some(children);
        self.depth += 1; // get_mut above establishes depth < 64
        Ok(())
    }

    fn next(&mut self) -> Result<Option<(&'a Value, usize)>, AiError> {
        while let Some(index) = self.depth.checked_sub(1) {
            let frame = self.frames.get_mut(index).ok_or_else(|| AiError::internal("charging_workspace"))?;
            let children = frame.as_mut().ok_or_else(|| AiError::internal("charging_workspace"))?;
            if let Some(value) = children.next_back() {
                return Ok(Some((value, self.depth)));
            }
            *frame = None;
            self.depth = index;
        }
        Ok(None)
    }
}

/// Bound containers before recursive serde/copying. The root object counts as one
/// container; a scalar inside the 64th container is not a 65th container. All callers
/// (including the unchanged canonical prefix rule) share this fixed-depth traversal.
struct JsonSize {
    bytes: usize,
    nodes: usize,
    keys: usize,
}

fn bounded_object(object: &Map<String, Value>, limit: u32) -> Result<JsonSize, AiError> {
    bound(object.len(), limit, "tool_json_bytes")?;
    let mut workspace = ChargingWorkspace::new();
    workspace.push(ChargingChildren::Object(object.values()))?;
    let mut frontier = object.len();
    let mut count = 1usize;
    let mut keys = object.len();
    if object.keys().any(|key| key.len() > limit as usize) {
        return Err(AiError::exhausted("tool_json_bytes", limit));
    }
    while let Some((value, depth)) = workspace.next()? {
        frontier = frontier.checked_sub(1).ok_or_else(|| AiError::internal("charging_workspace"))?;
        count = count.checked_add(1).ok_or_else(|| AiError::exhausted("tool_json_bytes", limit))?;
        let children = match value {
            Value::Object(map) => map.len(),
            Value::Array(array) => array.len(),
            _ => 0,
        };
        // Every visited node needs at least one JSON byte, so this is not a new ceiling.
        frontier = frontier.checked_add(children).ok_or_else(|| AiError::exhausted("tool_json_bytes", limit))?;
        bound(count.saturating_add(frontier), limit, "tool_json_bytes")?;
        match value {
            Value::String(text) => bound(text.len(), limit, "tool_json_bytes")?,
            Value::Object(map) => {
                if depth >= 64 {
                    return Err(AiError::exhausted("tool_json_depth", 64));
                }
                if map.keys().any(|key| key.len() > limit as usize) {
                    return Err(AiError::exhausted("tool_json_bytes", limit));
                }
                keys = keys.checked_add(map.len()).ok_or_else(|| AiError::exhausted("tool_json_bytes", limit))?;
                workspace.push(ChargingChildren::Object(map.values()))?;
            }
            Value::Array(array) => {
                if depth >= 64 {
                    return Err(AiError::exhausted("tool_json_depth", 64));
                }
                workspace.push(ChargingChildren::Array(array.iter()))?;
            }
            _ => {}
        }
    }
    Ok(JsonSize { bytes: serialized_bytes(object, limit as usize, "tool_json_bytes")?, nodes: count, keys })
}

fn serialized_bytes(value: &impl serde::Serialize, limit: usize, resource: &'static str) -> Result<usize, AiError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
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
    // Count the actual escaped UTF-8 JSON bytes, without copying or allocating a JSON string.
    let mut counter = Counter(limit);
    serde_json::to_writer(&mut counter, value).map_err(|_| AiError::exhausted(resource, u32::try_from(limit).unwrap_or(u32::MAX)))?;
    Ok(limit - counter.0)
}

/// Owned private JSON plus the existing conservative per-node structural fee. Walk
/// borrowed values in an already-reserved, fixed-depth workspace before serde/cloning.
/// Neither node counting nor object-argument charging allocates a width-sized queue.
pub(super) fn retained_json_charge(value: &Value, limits: &AiLimits) -> Result<usize, AiError> {
    let limit = limits.continuation_bytes;
    let mut charge = 0;
    let mut workspace = ChargingWorkspace::new();
    let mut current = Some((value, 0usize));
    let mut frontier = 1usize;
    while let Some((value, depth)) = current {
        frontier = frontier.checked_sub(1).ok_or_else(|| AiError::internal("charging_workspace"))?;
        // Retain the original Value/key/container fee; workspace is reserved separately
        // BEFORE the walk, not estimated from width or added after it finishes.
        add_prefix(&mut charge, size_of::<Value>() + size_of::<String>() + 12 * size_of::<usize>(), limit)?;
        let children = match value {
            Value::Array(a) => a.len(),
            Value::Object(m) => m.len(),
            _ => 0,
        };
        // Preserve the old cardinality/byte validation and its priority over depth
        // errors, using ONE integer rather than a pending-reference queue. This is
        // not scratch admission: the actual fixed workspace was paid before entry.
        frontier = frontier.checked_add(children).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?;
        let cardinality_bytes = frontier.checked_mul(size_of::<(&Value, usize)>()).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?;
        bound(cardinality_bytes, limit, "continuation_bytes")?;
        if children != 0 && depth >= 64 {
            return Err(AiError::exhausted("tool_json_depth", 64));
        }
        match value {
            Value::Array(a) if !a.is_empty() => workspace.push(ChargingChildren::Array(a.iter()))?,
            Value::Object(m) if !m.is_empty() => workspace.push(ChargingChildren::Object(m.values()))?,
            _ => {}
        }
        current = workspace.next()?;
    }
    let remaining = (limit as usize).checked_sub(charge).ok_or_else(|| AiError::exhausted("continuation_bytes", limit))?;
    add_prefix(&mut charge, serialized_bytes(value, remaining, "continuation_bytes")?, limit)?;
    Ok(charge)
}

pub(super) fn retained_object_charge(object: &Map<String, Value>, limits: &AiLimits) -> Result<usize, AiError> {
    let size = bounded_object(object, limits.tool_json_bytes)?;
    size.nodes
        .checked_mul(size_of::<Value>() + size_of::<String>() + 12 * size_of::<usize>())
        .and_then(|n| n.checked_add(size.bytes))
        .ok_or_else(|| AiError::exhausted("continuation_bytes", limits.continuation_bytes))
}

/// Explicit, lossless subset: typed object/array/scalars, description, string enum and required.
/// Anything else (including refs, unions and constraints) is refused, never reduced to a hint.
fn gemini_schema(root: &Map<String, Value>) -> Result<(), AiError> {
    let unsupported = || AiError::unsupported("toolSchema");
    if root.get("type").and_then(Value::as_str) != Some("object") {
        return Err(unsupported());
    }
    let mut pending = vec![root];
    while let Some(schema) = pending.pop() {
        if schema.keys().any(|key| !matches!(key.as_str(), "type" | "description" | "enum" | "properties" | "required" | "items")) {
            return Err(unsupported());
        }
        let kind = schema.get("type").and_then(Value::as_str).ok_or_else(unsupported)?;
        if !matches!(kind, "object" | "array" | "string" | "integer" | "number" | "boolean") {
            return Err(unsupported());
        }
        if schema.get("description").is_some_and(|v| !v.is_string()) {
            return Err(unsupported());
        }
        if let Some(values) = schema.get("enum") {
            let values = values.as_array().filter(|a| !a.is_empty()).ok_or_else(unsupported)?;
            if kind != "string" || values.iter().any(|v| !v.is_string()) {
                return Err(unsupported());
            }
        }
        if let Some(properties) = schema.get("properties") {
            if kind != "object" {
                return Err(unsupported());
            }
            let properties = properties.as_object().ok_or_else(unsupported)?;
            for property in properties.values() {
                pending.push(property.as_object().ok_or_else(unsupported)?);
            }
        }
        if let Some(required) = schema.get("required") {
            if kind != "object" {
                return Err(unsupported());
            }
            let required = required.as_array().ok_or_else(unsupported)?;
            let mut names = BTreeSet::new();
            for name in required {
                let name = name.as_str().ok_or_else(unsupported)?;
                if !names.insert(name) || !schema.get("properties").and_then(Value::as_object).is_some_and(|p| p.contains_key(name)) {
                    return Err(unsupported());
                }
            }
        }
        if let Some(items) = schema.get("items") {
            if kind != "array" {
                return Err(unsupported());
            }
            pending.push(items.as_object().ok_or_else(unsupported)?);
        } else if kind == "array" {
            return Err(unsupported());
        }
    }
    Ok(())
}

fn joined_text(message: &ModelMessage) -> String {
    message.content.iter().filter_map(|part| if let ContentPart::Text(text) = part { Some(text.as_str()) } else { None }).collect()
}

fn push_group(history: &mut Vec<Value>, role: &str, key: &str, parts: Vec<Value>) {
    if let Some(last) = history.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(previous) = last.get_mut(key).and_then(Value::as_array_mut)
    {
        previous.extend(parts);
        return;
    }
    let mut message = Map::new();
    message.insert("role".into(), json!(role));
    message.insert(key.into(), Value::Array(parts));
    history.push(Value::Object(message));
}

fn encode_history(
    request: &GenerateRequest,
    wire: WireProtocol,
    images: &[(&ArtifactRef, String)],
    private: &ProviderState,
) -> Result<(String, Vec<Value>), AiError> {
    let mut system = Vec::new();
    let mut history = Vec::new();
    let mut names = BTreeMap::new();
    let mut assistant_index = 0usize;
    let provider_id = |id: &str| private.wire_to_provider_tool_ids.get(id).cloned().unwrap_or_else(|| id.to_owned());
    for message in &request.messages {
        if message.role == MessageRole::System {
            system.push(joined_text(message));
            continue;
        }
        for call in &message.tool_calls {
            names.insert(call.id.as_str(), call.name.as_str());
        }
        if message.role == MessageRole::Assistant {
            let native = private.native_assistants.get(assistant_index).filter(|v| !v.is_null()).filter(|native| {
                let parts = match wire {
                    WireProtocol::Chat => return true, // already contributes an assistant message
                    WireProtocol::Responses => native.as_array(),
                    WireProtocol::Messages => native.get("content").and_then(Value::as_array),
                    WireProtocol::Gemini => native.get("parts").and_then(Value::as_array),
                };
                // Preserve the empty native slot in state, but encode its canonical
                // public empty text below instead of flattening/dropping its position.
                parts.is_none_or(|parts| !parts.is_empty())
            });
            assistant_index += 1;
            if let Some(native) = native {
                match wire {
                    WireProtocol::Responses => {
                        history.extend(native.as_array().ok_or_else(|| AiError::internal("continuation_history"))?.iter().cloned())
                    }
                    WireProtocol::Chat | WireProtocol::Messages | WireProtocol::Gemini => history.push(native.clone()),
                }
                continue;
            }
        }
        if message.role == MessageRole::Tool {
            let id = message.tool_call_id.as_deref().ok_or_else(|| AiError::invalid("tool_call_id"))?;
            let text = joined_text(message);
            match wire {
                WireProtocol::Chat => history.push(json!({"role":"tool","tool_call_id":provider_id(id),"content":text})),
                WireProtocol::Responses => history.push(json!({"type":"function_call_output","call_id":provider_id(id),"output":text})),
                WireProtocol::Messages => {
                    push_group(&mut history, "user", "content", vec![json!({"type":"tool_result","tool_use_id":provider_id(id),"content":text})])
                }
                WireProtocol::Gemini => {
                    let name = names.get(id).ok_or_else(|| AiError::invalid("tool_call_id"))?;
                    push_group(
                        &mut history,
                        "user",
                        "parts",
                        vec![json!({"functionResponse":{"id":provider_id(id),"name":name,"response":{"output":text}}})],
                    );
                }
            }
            continue;
        }
        let assistant = message.role == MessageRole::Assistant;
        let role = if assistant { "assistant" } else { "user" };
        let mut parts = Vec::new();
        for part in &message.content {
            parts.push(match part {
                ContentPart::Text(text) => match wire {
                    WireProtocol::Chat | WireProtocol::Messages => json!({"type":"text","text":text}),
                    WireProtocol::Responses => json!({"type":if assistant {"output_text"} else {"input_text"},"text":text}),
                    WireProtocol::Gemini => json!({"text":text}),
                },
                ContentPart::Image(reference) => {
                    let data = images.iter().find(|(r, _)| *r == reference).map(|(_, data)| data).ok_or_else(|| AiError::invalid("artifact"))?;
                    match wire {
                        WireProtocol::Chat => json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{data}", reference.media_type)}}),
                        WireProtocol::Responses => json!({"type":"input_image","image_url":format!("data:{};base64,{data}", reference.media_type)}),
                        WireProtocol::Messages => json!({"type":"image","source":{"type":"base64","media_type":reference.media_type,"data":data}}),
                        WireProtocol::Gemini => json!({"inlineData":{"mimeType":reference.media_type,"data":data}}),
                    }
                }
                ContentPart::Document { .. } => return Err(AiError::unsupported("documents")),
            });
        }
        let mut calls = Vec::new();
        for call in &message.tool_calls {
            let id = provider_id(&call.id);
            match wire {
                WireProtocol::Chat => calls.push(json!({"id":id,"type":"function","function":{"name":call.name,"arguments":serde_json::to_string(&call.arguments).map_err(|_| AiError::invalid("arguments"))?}})),
                WireProtocol::Responses => calls.push(json!({"type":"function_call","call_id":id,"name":call.name,"arguments":serde_json::to_string(&call.arguments).map_err(|_| AiError::invalid("arguments"))?})),
                WireProtocol::Messages => parts.push(json!({"type":"tool_use","id":id,"name":call.name,"input":call.arguments})),
                WireProtocol::Gemini => {
                    let mut part = json!({"functionCall":{"id":id,"name":call.name,"args":call.arguments}});
                    if let Some(signature) = private.gemini_signatures.get(&call.id) { part["thoughtSignature"] = json!(signature); }
                    parts.push(part);
                }
            }
        }
        match wire {
            WireProtocol::Chat => {
                let content = if message.content.iter().any(|part| matches!(part, ContentPart::Image(_))) {
                    Value::Array(parts)
                } else {
                    json!(joined_text(message))
                };
                let mut value = json!({"role":role,"content":content});
                if !calls.is_empty() {
                    value["tool_calls"] = Value::Array(calls);
                }
                history.push(value);
            }
            WireProtocol::Responses => {
                if !parts.is_empty() {
                    history.push(json!({"type":"message","role":role,"content":parts}));
                }
                history.extend(calls);
            }
            WireProtocol::Messages => push_group(&mut history, role, "content", parts),
            WireProtocol::Gemini => push_group(&mut history, if assistant { "model" } else { "user" }, "parts", parts),
        }
    }
    Ok((system.join("\n\n"), history))
}
