//! Byte and parameter boundary. Transport/correlation failures are fatal; only a
//! valid request envelope may receive a recoverable method/parameter error.

use super::data::TextPayload;
use super::*;
use serde::de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::io::{self, Write};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Envelope {
    #[serde(rename_all = "camelCase")]
    Request {
        protocol_version: String,
        scope: Scope,
        request_id: RequestId,
        method: String,
        params: Object,
        #[serde(default, deserialize_with = "optional_object")]
        extensions: Option<Object>,
    },
    #[serde(rename_all = "camelCase")]
    Event {
        protocol_version: String,
        scope: Scope,
        sequence: u32,
        event: String,
        request_id: RequiredNullable<Id>,
        data: Object,
        #[serde(default, deserialize_with = "optional_object")]
        extensions: Option<Object>,
    },
}

// Responses are decoded separately: outcome changes the exact allowed field set.
#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase", deny_unknown_fields)]
enum Outcome {
    Ok { result: Object },
    Error { error: WireError },
}

fn optional_object<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Object>, D::Error> {
    Object::deserialize(d).map(Some)
}

fn opposite(sender: Sender) -> Sender {
    match sender {
        Sender::Host => Sender::Runtime,
        Sender::Runtime => Sender::Host,
    }
}

fn scope_allowed(method: &str, scope: &Scope) -> bool {
    match method {
        "runtime.hello" | "runtime.shutdown" | "runtime.ready" => matches!(scope, Scope::Runtime),
        "session.start" | "session.reset" => matches!(scope, Scope::Session(_)),
        "connection.describe" | "validation.status" | "validation.report" | "validation.cancel" | "connection.state" => {
            !matches!(scope, Scope::Runtime)
        }
        "request.cancel" => true, // Equality with the target is the pending ledger's job.
        _ => matches!(scope, Scope::Turn(_)),
    }
}

pub fn decode_frame(bytes: &[u8], phase: DecodePhase, sender: Sender, limits: &Limits) -> Result<DecodeResult, ProtocolViolation> {
    if validate_limits(limits).is_err() || bytes.len() > limits.frame_bytes as usize || bytes.last() != Some(&b'\n') {
        return Err(ProtocolViolation);
    }
    let payload = bytes.strip_suffix(b"\n").ok_or(ProtocolViolation)?;
    if payload.contains(&b'\n') || payload.last() == Some(&b'\r') {
        return Err(ProtocolViolation);
    }
    std::str::from_utf8(payload).map_err(|_| ProtocolViolation)?;
    // Storage accounting is linear in admitted input, including Value/map nodes,
    // decoded keys and strings. No size_hint-driven allocation or remainder drain.
    let budget = Cell::new(payload.len().saturating_mul(64));
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let value = JsonSeed { depth: 0, limit: limits.json_depth, budget: &budget }.deserialize(&mut deserializer).map_err(|_| ProtocolViolation)?;
    deserializer.end().map_err(|_| ProtocolViolation)?;
    decode_value(value, phase, sender, limits)
}

fn decode_value(mut value: Value, phase: DecodePhase, sender: Sender, limits: &Limits) -> Result<DecodeResult, ProtocolViolation> {
    // Flattening an internally-tagged enum with another internally-tagged enum
    // obscures strict unknown-field validation. Remove and decode the response
    // branch explicitly, then decode only its common envelope.
    let object = value.as_object_mut().ok_or(ProtocolViolation)?;
    let response = if object.get("kind") == Some(&json!("response")) {
        let outcome = object.remove("outcome").ok_or(ProtocolViolation)?;
        let mut body = Object::new();
        body.insert("outcome".to_owned(), outcome.clone());
        let field = match outcome.as_str() {
            Some("ok") => "result",
            Some("error") => "error",
            _ => return Err(ProtocolViolation),
        };
        body.insert(field.to_owned(), object.remove(field).ok_or(ProtocolViolation)?);
        let body = serde_json::from_value::<Outcome>(Value::Object(body)).map_err(|_| ProtocolViolation)?;
        object.insert("outcome".to_owned(), json!({}));
        Some(body)
    } else {
        None
    };
    // Common response parsing is kept independent of its body.
    if let Some(body) = response {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Header {
            protocol_version: String,
            kind: String,
            scope: Scope,
            request_id: RequestId,
            outcome: Object,
            #[serde(default, deserialize_with = "optional_object")]
            extensions: Option<Object>,
        }
        let h: Header = serde_json::from_value(value).map_err(|_| ProtocolViolation)?;
        if h.kind != "response" || !h.outcome.is_empty() {
            return Err(ProtocolViolation);
        }
        version(&h.protocol_version, phase)?;
        RequestId::parse(h.request_id.as_str(), opposite(sender)).map_err(|_| ProtocolViolation)?;
        let header = ResponseHeader { protocol_version: h.protocol_version, request_id: h.request_id, scope: h.scope, extensions: h.extensions };
        let body = match body {
            Outcome::Ok { result } => ResponseBody::Ok(PendingResult(result)),
            Outcome::Error { error } => ResponseBody::Error(error),
        };
        return Ok(DecodeResult::Frame(Frame::Response { header, body }));
    }
    let envelope: Envelope = serde_json::from_value(value).map_err(|_| ProtocolViolation)?;
    match envelope {
        Envelope::Request { protocol_version, scope, request_id, method, params, extensions } => {
            version(&protocol_version, phase)?;
            RequestId::parse(request_id.as_str(), sender).map_err(|_| ProtocolViolation)?;
            let header = RequestHeader { protocol_version, scope, request_id, method, extensions };
            let known = matches!(
                header.method.as_str(),
                "runtime.hello"
                    | "session.start"
                    | "turn.start"
                    | "turn.cancel"
                    | "request.cancel"
                    | "session.reset"
                    | "runtime.shutdown"
                    | "connection.describe"
                    | "model.generate"
                    | "agent.run"
                    | "validation.submit"
                    | "validation.status"
                    | "validation.report"
                    | "validation.cancel"
            );
            if !known {
                return Ok(DecodeResult::RequestError { header, error: WireError::local(ErrorCode::MethodNotFound) });
            }
            if !scope_allowed(&header.method, &header.scope) || (phase == DecodePhase::Bootstrap && header.method != "runtime.hello") {
                return Err(ProtocolViolation);
            }
            let host_method = matches!(
                header.method.as_str(),
                "runtime.hello" | "session.start" | "turn.start" | "turn.cancel" | "session.reset" | "runtime.shutdown"
            );
            if header.method != "request.cancel" && host_method != (sender == Sender::Host) {
                return Err(ProtocolViolation);
            }
            let body = if sender == Sender::Host {
                HostRequest::decode(&header.method, Value::Object(params), limits).map(RequestBody::Host)
            } else if header.method == "request.cancel" {
                typed::<RequestCancelParams>(Value::Object(params), limits).map(RequestBody::Cancel)
            } else {
                HostCall::decode(&header.method, Value::Object(params), limits).map(RequestBody::Runtime)
            };
            let body = body.and_then(|body| {
                let cancel = match &body {
                    RequestBody::Host(HostRequest::RequestCancel(p)) | RequestBody::Cancel(p) => Some(p),
                    _ => None,
                };
                if let Some(p) = cancel {
                    RequestId::parse(p.target_request_id.as_str(), sender)?;
                }
                Ok(body)
            });
            match body {
                Ok(body) => Ok(DecodeResult::Frame(Frame::Request { header, body })),
                Err(error) => Ok(DecodeResult::RequestError { header, error }),
            }
        }
        Envelope::Event { protocol_version, scope, sequence, event, request_id, data, extensions } => {
            version(&protocol_version, phase)?;
            if phase != DecodePhase::Negotiated || sequence == 0 || sequence > 2_147_483_647 || !scope_allowed(&event, &scope) {
                return Err(ProtocolViolation);
            }
            let body = if sender == Sender::Host {
                HostEvent::decode(&event, Value::Object(data), limits).map(EventBody::Host)
            } else {
                RuntimeEvent::decode(&event, Value::Object(data), limits).map(EventBody::Runtime)
            }
            .map_err(|_| ProtocolViolation)?;
            Ok(DecodeResult::Frame(Frame::Event { header: EventHeader { protocol_version, sequence, request_id, scope, extensions }, body }))
        }
    }
}

fn version(version: &str, phase: DecodePhase) -> Result<(), ProtocolViolation> {
    if phase == DecodePhase::Negotiated && version != "1.0" { Err(ProtocolViolation) } else { Ok(()) }
}

pub fn decode_runtime_reply(method: &str, value: Value, limits: &Limits) -> Result<RuntimeReply, ProtocolViolation> {
    RuntimeReply::decode(method, value, limits).map_err(|_| ProtocolViolation)
}
pub fn decode_host_reply(method: &str, value: Value, limits: &Limits) -> Result<HostReply, ProtocolViolation> {
    let reply = HostReply::decode(method, value, limits).map_err(|_| ProtocolViolation)?;
    let content = match &reply {
        HostReply::ModelGenerate(x) => Some(&x.content),
        HostReply::AgentRun(x) => Some(&x.content),
        _ => None,
    };
    if content.is_some_and(|c| c.iter().any(|b| !matches!(b, ContentBlock::Text { .. }))) {
        return Err(ProtocolViolation);
    }
    Ok(reply)
}

pub(super) fn typed<T: DeserializeOwned + TextPayload>(value: Value, limits: &Limits) -> Result<T, WireError> {
    validate_data(&value, limits)?;
    let typed: T = serde_json::from_value(value).map_err(|_| WireError::invalid_request())?;
    typed.validate_text(limits)?;
    Ok(typed)
}

#[derive(Clone, Copy)]
struct JsonSeed<'a> {
    depth: u32,
    limit: u32,
    budget: &'a Cell<usize>,
}
impl JsonSeed<'_> {
    fn charge<E: de::Error>(&self, bytes: usize) -> Result<(), E> {
        let remaining = self.budget.get().checked_sub(bytes).ok_or_else(|| E::custom("JSON allocation limit."))?;
        self.budget.set(remaining);
        Ok(())
    }
    fn child<E: de::Error>(&self) -> Result<Self, E> {
        if self.depth >= self.limit {
            return Err(E::custom("JSON depth limit."));
        }
        Ok(Self { depth: self.depth + 1, ..*self })
    }
}
impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.charge::<D::Error>(64)?;
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonSeed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v).map(Value::Number).ok_or_else(|| E::custom("Nonfinite JSON number."))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        self.charge::<E>(v.len())?;
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.charge::<E>(v.len())?;
        Ok(Value::String(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let child = self.child::<A::Error>()?;
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(child)? {
            values.try_reserve(1).map_err(|_| de::Error::custom("JSON allocation limit."))?;
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let child = self.child::<A::Error>()?;
        let mut values = Object::new();
        while let Some(key) = map.next_key::<String>()? {
            self.charge::<A::Error>(key.len().saturating_add(64))?;
            if values.contains_key(&key) {
                return Err(de::Error::custom("Duplicate JSON key."));
            }
            values.insert(key, map.next_value_seed(child)?);
        }
        Ok(Value::Object(values))
    }
}

fn bounded_size(value: &Value, cap: usize) -> Result<usize, WireError> {
    let mut writer = CountingWriter { size: 0, cap };
    serde_json::to_writer(&mut writer, value).map_err(|_| WireError::invalid_request())?;
    Ok(writer.size)
}

pub(super) fn encoded_size_with_lf(value: &Value) -> usize {
    match bounded_size(value, usize::MAX) {
        Ok(size) => size.saturating_add(1),
        Err(_) => usize::MAX,
    }
}
struct CountingWriter {
    size: usize,
    cap: usize,
}
impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.size = self.size.checked_add(bytes.len()).filter(|n| *n <= self.cap).ok_or_else(|| io::Error::other("JSON capacity exceeded."))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn depth(value: &Value, current: u32, limit: u32) -> Result<(), WireError> {
    match value {
        Value::Object(map) => {
            if current >= limit {
                return Err(WireError::invalid_request());
            }
            for v in map.values() {
                depth(v, current + 1, limit)?;
            }
        }
        Value::Array(list) => {
            if current >= limit {
                return Err(WireError::invalid_request());
            }
            for v in list {
                depth(v, current + 1, limit)?;
            }
        }
        _ => (),
    }
    Ok(())
}

pub(super) fn validate_data(value: &Value, limits: &Limits) -> Result<(), WireError> {
    validate_limits(limits)?;
    depth(value, 0, limits.json_depth)?;
    bounded_size(value, limits.frame_bytes.saturating_sub(1) as usize)?;
    declared(value, "", limits)?;
    attachments(value, limits)?;
    Ok(())
}

// Only declared protocol fields use the JS-safe integer rule. Explicitly opaque
// objects retain finite negative/decimal/large geometry numbers unchanged.
fn declared(value: &Value, key: &str, limits: &Limits) -> Result<(), WireError> {
    let bad = WireError::invalid_request;
    if matches!(key, "arguments" | "inputSchema" | "outputSchema") && !value.is_null() {
        bounded_size(value, limits.tool_json_bytes as usize)?;
        return Ok(());
    }
    if matches!(key, "params" | "parameters" | "expected" | "actual" | "measured" | "output" | "selection") {
        return Ok(());
    }
    match value {
        Value::Number(number) => {
            if key == "temperature" {
                if !number.as_f64().is_some_and(|n| n.is_finite() && (0.0..=2.0).contains(&n)) {
                    return Err(bad());
                }
            } else if matches!(key, "value" | "absTolerance" | "relTolerance") {
                if !finite(number, key != "value") {
                    return Err(bad());
                }
            } else if !number.as_u64().is_some_and(|n| n <= 2_147_483_647) {
                return Err(bad());
            }
            if key == "timeoutSeconds" && number.as_u64() == Some(0) {
                return Err(bad());
            }
            if key == "maxOutputTokens" && !number.as_u64().is_some_and(|n| (1..=131072).contains(&n)) {
                return Err(bad());
            }
        }
        Value::String(text) => {
            // Text blocks are checked on the decoded DTO, not by arbitrary JSON
            // keys or string values. Provider-option metadata has its own cap.
            if key == "reasoningEffort" && text.len() > 64 {
                return Err(bad());
            }
        }
        Value::Array(list) => {
            let cap = match key {
                "messages" => limits.model_messages,
                "tools" => limits.tool_definitions,
                "toolCalls" => limits.tool_calls,
                "steps" => 64,
                "inputs" => limits.snapshot_inputs,
                "checks" => limits.checks,
                "evidence" => limits.evidence_artifacts,
                _ => u32::MAX,
            };
            if list.len() > cap as usize {
                return Err(bad());
            }
            for child in list {
                declared(child, "", limits)?;
            }
            if key == "messages" {
                validate_messages(list)?;
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                declared(child, key, limits)?;
            }
            if let Some(v) = map.get("limits") {
                validate_limits(&serde_json::from_value::<Limits>(v.clone()).map_err(|_| bad())?)?;
            }
            if let Some(v) = map.get("budgets") {
                validate_budgets(&serde_json::from_value::<Budgets>(v.clone()).map_err(|_| bad())?)?;
            }
            if map.contains_key("selectedVersion") && map.get("selectedVersion").and_then(Value::as_str) != Some("1.0") {
                return Err(bad());
            }
            if map.contains_key("inputs") {
                validate_manifest(map, limits)?;
            }
            if map.get("checks").is_some_and(Value::is_array) {
                validate_checks(map, limits)?;
            }
            if let Some(steps) = map.get("steps").and_then(Value::as_array) {
                unique_ids(steps, "id", false)?;
            }
            if let Some(calls) = map.get("toolCalls").and_then(Value::as_array) {
                unique_ids(calls, "id", false)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn unique_ids(items: &[Value], field: &str, sorted: bool) -> Result<(), WireError> {
    let mut seen = BTreeSet::new();
    let mut previous = None;
    for item in items {
        let id = item.get(field).and_then(Value::as_str).ok_or_else(WireError::invalid_request)?;
        if !seen.insert(id) || (sorted && previous.is_some_and(|p| p >= id)) {
            return Err(WireError::invalid_request());
        }
        previous = Some(id);
    }
    Ok(())
}

fn validate_messages(messages: &[Value]) -> Result<(), WireError> {
    let mut pending = BTreeSet::new();
    let mut all_calls = BTreeSet::new();
    for message in messages {
        let message: ModelMessage = serde_json::from_value(message.clone()).map_err(|_| WireError::invalid_request())?;
        if message.role != Role::Assistant && !message.tool_calls.is_empty() {
            return Err(WireError::invalid_request());
        }
        if message.role == Role::Tool {
            let id = message.tool_call_id.0.ok_or_else(WireError::invalid_request)?;
            if !pending.remove(id.as_str()) {
                return Err(WireError::invalid_request());
            }
        } else {
            if message.tool_call_id.0.is_some() || (!pending.is_empty() && message.role == Role::User) {
                return Err(WireError::invalid_request());
            }
            for call in &message.tool_calls {
                if !all_calls.insert(call.id.as_str().to_owned()) {
                    return Err(WireError::invalid_request());
                }
                pending.insert(call.id.as_str().to_owned());
            }
        }
    }
    if !pending.is_empty() {
        return Err(WireError::invalid_request());
    }
    Ok(())
}

fn attachments(value: &Value, limits: &Limits) -> Result<(), WireError> {
    fn blocks(value: &Value, count: &mut u64, total: &mut u64, limits: &Limits) -> Result<(), WireError> {
        match value {
            Value::Object(map) => {
                if let Some(kind) = map.get("type").and_then(Value::as_str).filter(|k| matches!(*k, "image" | "document")) {
                    let artifact: ArtifactRef = serde_json::from_value(map.get("artifact").cloned().ok_or_else(WireError::invalid_request)?)
                        .map_err(|_| WireError::invalid_request())?;
                    let valid = match kind {
                        "image" => matches!(artifact.media_type.as_str(), "image/png" | "image/jpeg"),
                        _ => artifact.media_type == "application/pdf",
                    };
                    if !valid || artifact.size_bytes > limits.attachment_bytes {
                        return Err(WireError::invalid_request());
                    }
                    *count += 1;
                    *total += u64::from(artifact.size_bytes);
                    if *count > u64::from(limits.attachments) || *total > u64::from(limits.attachment_total_bytes) {
                        return Err(WireError::invalid_request());
                    }
                }
                for (key, child) in map {
                    if matches!(key.as_str(), "content" | "messages" | "parts" | "task") {
                        blocks(child, count, total, limits)?;
                    }
                }
            }
            Value::Array(list) => {
                for child in list {
                    blocks(child, count, total, limits)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    blocks(value, &mut 0, &mut 0, limits)
}

fn validate_manifest(map: &Object, limits: &Limits) -> Result<(), WireError> {
    let bad = WireError::invalid_request;
    let manifest: SnapshotManifest = serde_json::from_value(Value::Object(map.clone())).map_err(|_| bad())?;
    bounded_size(&Value::Object(map.clone()), limits.manifest_bytes as usize)?;
    let mut previous = None;
    let mut ids = BTreeSet::new();
    let mut designs = 0;
    let mut bytes = 0_u64;
    for input in &manifest.inputs {
        let path = &input.path;
        if path.len() > 240
            || path.contains(['\\', '\0'])
            || path.split('/').any(|c| c.is_empty() || c == ".." || c == ".")
            || previous.is_some_and(|p: &String| p >= path)
            || !ids.insert(input.dependency_id.as_str())
        {
            return Err(bad());
        }
        previous = Some(path);
        bytes += u64::from(input.size_bytes);
        if input.kind == InputKind::Design {
            designs += 1;
            if !path.ends_with(".sc") {
                return Err(bad());
            }
        }
    }
    if designs != 1 || bytes > u64::from(limits.snapshot_bytes) {
        return Err(bad());
    }
    Ok(())
}

fn validate_checks(map: &Object, limits: &Limits) -> Result<(), WireError> {
    let checks = map.get("checks").and_then(Value::as_array).ok_or_else(WireError::invalid_request)?;
    let profile = map.contains_key("profileId");
    if profile && checks.is_empty() {
        return Err(WireError::invalid_request());
    }
    unique_ids(checks, "checkId", profile)?;
    if profile {
        for check in checks {
            let requirement: CheckRequirement = serde_json::from_value(check.clone()).map_err(|_| WireError::invalid_request())?;
            let parameters = Value::Object(requirement.parameters);
            match requirement.kind.as_str() {
                "input.integrity" | "native.rebuild" => {
                    if parameters.as_object().is_none_or(|o| !o.is_empty()) {
                        return Err(WireError::invalid_request());
                    }
                }
                "parameters.required" => {
                    let p: ParameterRequirements = serde_json::from_value(parameters).map_err(|_| WireError::invalid_request())?;
                    if p.requirements.is_empty()
                        || p.requirements.len() > 64
                        || p.requirements.iter().any(|r| !finite(&r.value, false) || !finite(&r.abs_tolerance, true))
                    {
                        return Err(WireError::invalid_request());
                    }
                }
                "geometry.exact" => {
                    let p: GeometryMeasurements = serde_json::from_value(parameters).map_err(|_| WireError::invalid_request())?;
                    if p.measurements.is_empty()
                        || p.measurements.len() > 64
                        || p.measurements
                            .iter()
                            .any(|r| !finite(&r.expected, false) || !finite(&r.abs_tolerance, true) || !finite(&r.rel_tolerance, true))
                    {
                        return Err(WireError::invalid_request());
                    }
                }
                // Unknown check kinds remain shape-valid, not a supported/evaluated check.
                _ => (),
            }
        }
    }
    if map.contains_key("runner") {
        bounded_size(&Value::Object(map.clone()), limits.report_bytes as usize)?;
        let mut count = 0_u64;
        let mut bytes = 0_u64;
        for check in checks {
            if let Some(evidence) = check.get("evidence").and_then(Value::as_array) {
                for artifact in evidence {
                    let artifact: ArtifactRef = serde_json::from_value(artifact.clone()).map_err(|_| WireError::invalid_request())?;
                    count += 1;
                    bytes += u64::from(artifact.size_bytes);
                    if artifact.size_bytes > limits.evidence_bytes
                        || count > u64::from(limits.evidence_artifacts)
                        || bytes > u64::from(limits.evidence_total_bytes)
                    {
                        return Err(WireError::invalid_request());
                    }
                }
            }
        }
    }
    Ok(())
}

fn finite(number: &serde_json::Number, nonnegative: bool) -> bool {
    number.as_f64().is_some_and(|n| n.is_finite() && (!nonnegative || n >= 0.0))
}

struct CappedWriter {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("Frame capacity exceeded."));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn encode_frame(frame: &Frame, limits: &Limits) -> Result<Vec<u8>, WireError> {
    let value = match frame {
        Frame::Request { header: h, body } => {
            let (method, params) = match body {
                RequestBody::Host(x) => (x.method(), x.value()?),
                RequestBody::Runtime(x) => (x.method(), x.value()?),
                RequestBody::Cancel(x) => ("request.cancel", serde_json::to_value(x).map_err(|_| WireError::invalid_request())?),
            };
            if method != h.method {
                return Err(WireError::invalid_request());
            }
            let mut v = json!({"protocolVersion":h.protocol_version,"kind":"request","scope":h.scope,"requestId":h.request_id,"method":method,"params":params});
            if let Some(x) = &h.extensions {
                v["extensions"] = Value::Object(x.clone());
            }
            v
        }
        Frame::Response { header: h, body } => {
            let mut v = json!({"protocolVersion":h.protocol_version,"kind":"response","scope":h.scope,"requestId":h.request_id});
            match body {
                ResponseBody::Ok(x) => {
                    v["outcome"] = json!("ok");
                    v["result"] = Value::Object(x.0.clone());
                }
                ResponseBody::Error(x) => {
                    v["outcome"] = json!("error");
                    v["error"] = serde_json::to_value(x).map_err(|_| WireError::invalid_request())?;
                }
            }
            if let Some(x) = &h.extensions {
                v["extensions"] = Value::Object(x.clone());
            }
            v
        }
        Frame::Event { header: h, body } => {
            let (event, data) = match body {
                EventBody::Host(x) => (x.method(), x.value()?),
                EventBody::Runtime(x) => (x.method(), x.value()?),
            };
            let mut v = json!({"protocolVersion":h.protocol_version,"kind":"event","scope":h.scope,"sequence":h.sequence,"requestId":h.request_id,"event":event,"data":data});
            if let Some(x) = &h.extensions {
                v["extensions"] = Value::Object(x.clone());
            }
            v
        }
    };
    let mut writer = CappedWriter { bytes: Vec::new(), cap: limits.frame_bytes as usize };
    serde_json::to_writer(&mut writer, &value).map_err(|_| WireError::invalid_request())?;
    writer.write_all(b"\n").map_err(|_| WireError::invalid_request())?;
    let sender = match frame {
        Frame::Request { body: RequestBody::Host(_), .. } | Frame::Event { body: EventBody::Host(_), .. } => Sender::Host,
        Frame::Response { header, .. } => {
            if header.request_id.as_str().starts_with("h:") {
                Sender::Runtime
            } else {
                Sender::Host
            }
        }
        _ => Sender::Runtime,
    };
    // A bootstrap refusal has no method tag. Only this error branch can retain
    // the unselected envelope version; negotiated decoding still rejects it.
    let refusal = matches!(frame,Frame::Response{body:ResponseBody::Error(error),header} if error.code==ErrorCode::IncompatibleVersion && header.scope==Scope::Runtime);
    let phase = if refusal || value.get("method") == Some(&json!("runtime.hello")) { DecodePhase::Bootstrap } else { DecodePhase::Negotiated };
    if !matches!(decode_frame(&writer.bytes, phase, sender, limits), Ok(DecodeResult::Frame(_))) {
        return Err(WireError::invalid_request());
    }
    Ok(writer.bytes)
}

/// For the *local* reserved lane only. Ordinary peer errors may legitimately have
/// escaped messages/extensions larger than this lane; decoding never assumes 4 KiB.
pub fn encode_control_frame(frame: &Frame, limits: &Limits) -> Result<Vec<u8>, WireError> {
    let control = match frame {
        Frame::Request { header, .. } => matches!(header.method.as_str(), "turn.cancel" | "request.cancel" | "runtime.shutdown"),
        Frame::Response { body: ResponseBody::Error(error), .. } => {
            // Eligibility only: the caller's pending ledger classifies the
            // original control method (responses deliberately have no method tag).
            matches!(
                error.code,
                ErrorCode::Cancelled
                    | ErrorCode::TransportError
                    | ErrorCode::InternalError
                    | ErrorCode::InvalidRequest
                    | ErrorCode::ResourceExhausted
            )
        }
        Frame::Response { body: ResponseBody::Ok(result), .. } => {
            result.0.len() == 1 && result.0.get("state").and_then(Value::as_str).is_some_and(|s| matches!(s, "cancelling" | "terminal" | "stopping"))
        }
        _ => false,
    };
    if !control {
        return Err(WireError::invalid_request());
    }
    let bounded = Limits { frame_bytes: limits.frame_bytes.min(CONTROL_FRAME_BYTES as u32).min(limits.control_lane_bytes), ..limits.clone() };
    encode_frame(frame, &bounded)
}
