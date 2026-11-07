//! Caller-owned branch map and two-phase, non-reentrant reservation ownership.
//! No public/wire projection contains native assistants, signatures or routing IDs.

use std::collections::{BTreeSet, HashMap};
use std::mem::size_of;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use serde_json::Value;

use super::messages::{CHARGING_WORKSPACE_BYTES, normalized_prefix_charge, retained_json_charge, retained_object_charge, valid_id, validate_request};
use super::provider::{ProviderReply, ProviderState, WireProtocol};
use super::{
    AiError, AiLimits, ConnectionBinding, ContentPart, ContinuationId, Delivery, GenerateRequest, IdFn, MessageRole, ModelMessage, ModelReply,
    SessionKey, ToolCall,
};

/// OS entropy only. A failed entropy source is a sanitized error, never a fallback.
pub(in crate::agent_chat) fn random_id() -> Result<String, AiError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| AiError::internal("random_id"))?;
    use std::fmt::Write;
    let mut text = String::with_capacity(32);
    for byte in bytes {
        write!(text, "{byte:02x}").map_err(|_| AiError::internal("random_id"))?;
    }
    Ok(text)
}

#[derive(Default)]
pub(in crate::agent_chat) struct ContinuationStore {
    sessions: HashMap<SessionKey, Session>,
}

struct Session {
    generation: Arc<Generation>,
    entries: HashMap<ContinuationId, Entry>,
}

/// Only this narrow quota/revocation owner is shared, never the store or handle map.
struct Generation {
    live: AtomicBool,
    bytes: AtomicUsize,
    slots: AtomicUsize,
    limits: AiLimits,
}

struct Reservation {
    generation: Arc<Generation>,
    bytes: usize,
    owns_slot: bool,
}

impl Reservation {
    fn new(generation: &Arc<Generation>, bytes: usize, owns_slot: bool) -> Result<Self, AiError> {
        generation.check()?;
        add_atomic(&generation.bytes, bytes, generation.limits.continuation_bytes, "continuation_bytes")?;
        if owns_slot && let Err(error) = add_atomic(&generation.slots, 1, generation.limits.continuation_handles, "continuation_handles") {
            generation.bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err(error);
        }
        Ok(Self { generation: Arc::clone(generation), bytes, owns_slot })
    }

    fn grow(&mut self, additional: usize) -> Result<(), AiError> {
        self.generation.check()?;
        let bytes = self.bytes.checked_add(additional).ok_or_else(|| exhausted(&self.generation.limits))?;
        add_atomic(&self.generation.bytes, additional, self.generation.limits.continuation_bytes, "continuation_bytes")?;
        self.bytes = bytes;
        Ok(())
    }

    fn shrink(&mut self, bytes: usize) {
        // Called only with a checked, smaller final charge after transient owners drop.
        if let Some(released) = self.bytes.checked_sub(bytes) {
            self.generation.bytes.fetch_sub(released, Ordering::AcqRel);
            self.bytes = bytes;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Error/drop can run UNDER the caller's store mutex. Never acquire a lock here.
        self.generation.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        if self.owns_slot {
            self.generation.slots.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Generation {
    fn check(&self) -> Result<(), AiError> {
        if self.live.load(Ordering::Acquire) { Ok(()) } else { Err(AiError::invalid("session")) }
    }
}

fn add_atomic(counter: &AtomicUsize, bytes: usize, limit: u32, resource: &'static str) -> Result<(), AiError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| used.checked_add(bytes).filter(|n| *n <= limit as usize))
        .map(|_| ())
        .map_err(|_| AiError::exhausted(resource, limit))
}

struct Entry {
    request: GenerateRequest,
    private: ProviderState,
    wire: WireProtocol,
    created: Duration,
    exclusive: Weak<()>,
    reservation: Reservation,
}

/// Move-only proof of this session generation, reserved capacity and parent exclusivity.
pub(in crate::agent_chat) struct ContinuationLease {
    key: SessionKey,
    request: GenerateRequest,
    private: ProviderState,
    wire: WireProtocol,
    began: Duration,
    token: Arc<()>,
    reservation: Reservation,
}

impl ContinuationLease {
    pub(in crate::agent_chat) fn private(&self) -> &ProviderState {
        &self.private
    }
}

/// The facade's original owned input, distinct from the lease/candidate copies.
/// Keep the payload BEFORE its reservation: every return/drop path destroys the
/// input first, then refunds only this exact generation without taking a lock.
pub(in crate::agent_chat) struct OwnedRequest {
    request: GenerateRequest,
    _reservation: Reservation,
}

impl OwnedRequest {
    pub(in crate::agent_chat) fn request(&self) -> &GenerateRequest {
        &self.request
    }
}

/// No Clone, serde, public fields or external constructors. The very same lease survives
/// ID preparation and is consumed exactly once by callback-free commit.
pub(in crate::agent_chat) struct PreparedContinuation {
    id: ContinuationId,
    request: GenerateRequest,
    private: ProviderState,
    reply: ModelReply,
    charge: usize,
    // Drop candidate data BEFORE releasing its reservation, including on errors
    // under the caller's store mutex. Field destruction follows declaration order.
    lease: ContinuationLease,
}

impl ContinuationStore {
    pub(in crate::agent_chat) fn open_session(&mut self, key: SessionKey, limits: AiLimits) -> Result<(), AiError> {
        limits.validate()?;
        if !valid_id(&key.runtime_instance_id) || !valid_id(&key.session_id) || self.sessions.contains_key(&key) {
            return Err(AiError::invalid("session"));
        }
        let baseline = sum(
            &limits,
            &[size_of::<Session>() + size_of::<Generation>() + size_of::<SessionKey>(), key.runtime_instance_id.len(), key.session_id.len()],
        )?;
        let generation = Arc::new(Generation { live: AtomicBool::new(true), bytes: AtomicUsize::new(baseline), slots: AtomicUsize::new(0), limits });
        self.sessions.insert(key, Session { generation, entries: HashMap::new() });
        Ok(())
    }

    /// Move the original input under this session's existing aggregate quota
    /// BEFORE begin clones it or any external artifact/HTTP/ID work can run.
    pub(in crate::agent_chat) fn own_request(
        &self,
        key: &SessionKey,
        binding: &ConnectionBinding,
        request: GenerateRequest,
        limits: &AiLimits,
    ) -> Result<OwnedRequest, AiError> {
        let session = self.sessions.get(key).ok_or_else(|| AiError::invalid("session"))?;
        let generation = &session.generation;
        let _charging_workspace = Reservation::new(generation, CHARGING_WORKSPACE_BYTES, false)?;
        validate_request(key, binding, &request, limits)?;
        // Same canonical request calculation as begin/candidate storage. These
        // walks are sequential under one pre-reserved workspace, never nested.
        let bytes = sum(&generation.limits, &[request_charge(&request, &generation.limits)?, size_of::<Reservation>()])?;
        let reservation = Reservation::new(generation, bytes, false)?;
        // No fallible operation after admission: payload and refund now have
        // structural destruction order, not function-argument/local drop order.
        Ok(OwnedRequest { request, _reservation: reservation })
    }

    pub(in crate::agent_chat) fn begin(
        &mut self,
        key: &SessionKey,
        binding: &ConnectionBinding,
        request: &GenerateRequest,
        now: Duration,
    ) -> Result<ContinuationLease, AiError> {
        let session = self.sessions.get_mut(key).ok_or_else(|| AiError::invalid("session"))?;
        let generation = &session.generation;
        generation.check()?;
        let limits = &generation.limits;
        // Metadata/canonical/object/private charging share one fixed-depth workspace
        // at a time. Pay its full contribution before entering any of those walks.
        // This byte-only guard never changes handle count or a parent's exclusive token.
        let _charging_workspace = Reservation::new(generation, CHARGING_WORKSPACE_BYTES, false)?;
        let (wire, _) = validate_request(key, binding, request, limits)?;
        session
            .entries
            .retain(|_, entry| now.checked_sub(entry.created).is_none_or(|age| age < Duration::from_secs(limits.continuation_ttl_seconds.into())));
        let parent = match &request.continuation {
            Some(id) => {
                let entry = session.entries.get(id).ok_or_else(|| AiError::invalid("continuation"))?;
                if now < entry.created || entry.wire != wire || entry.exclusive.upgrade().is_some() || !extends(&entry.request, request) {
                    return Err(AiError::invalid("continuation"));
                }
                Some(entry)
            }
            None => None,
        };
        let assistants = request.messages.iter().filter(|m| m.role == MessageRole::Assistant).count();
        let request_bytes = request_charge(request, limits)?;
        let private_bytes = parent.map(|p| state_charge(&p.private, limits)).transpose()?.unwrap_or(size_of::<ProviderState>());
        let placeholders = assistants.checked_mul(size_of::<Value>()).ok_or_else(|| exhausted(limits))?;
        // Two request owners (lease + prepared candidate), and up to three private
        // copies (lease, physical call/reply, candidate). Actual new reply is added
        // before prepare clones it. Opaque route and all identity map entries count.
        let bytes = sum(
            limits,
            &[
                overhead(),
                request_bytes,
                request_bytes,
                private_bytes,
                private_bytes,
                private_bytes,
                placeholders,
                placeholders,
                placeholders,
                32 * 3,
                key.runtime_instance_id.len(),
                key.session_id.len(),
                request
                    .messages
                    .iter()
                    .map(|m| m.tool_calls.len())
                    .sum::<usize>()
                    .checked_mul(3 * (2 * 128 + 2 * size_of::<String>() + size_of::<[usize; 8]>()))
                    .ok_or_else(|| exhausted(limits))?,
            ],
        )?;
        // A successor exclusively claims its parent's existing handle slot. Bytes for
        // both versions still count until successful replacement; no live entry evicts.
        let reservation = Reservation::new(generation, bytes, parent.is_none())?;
        let mut private = parent.map(|p| p.private.clone()).unwrap_or_default();
        if private.routing_session_id.is_empty() {
            private.routing_session_id = random_id()?;
        }
        private.native_assistants.resize(assistants, Value::Null);
        for call in request.messages.iter().flat_map(|m| &m.tool_calls) {
            private.wire_to_provider_tool_ids.entry(call.id.clone()).or_insert_with(|| call.id.clone());
        }
        let token = Arc::new(());
        let lease =
            ContinuationLease { key: key.clone(), request: request.clone(), private, wire, began: now, token: Arc::clone(&token), reservation };
        if let Some(id) = &request.continuation
            && let Some(entry) = session.entries.get_mut(id)
        {
            entry.exclusive = Arc::downgrade(&token);
        }
        Ok(lease)
    }

    /// Caller owns store + publication locks. No external work, including clock/IDs,
    /// occurs here; all errors drop the reservation without re-entering either lock.
    pub(in crate::agent_chat) fn commit(&mut self, prepared: PreparedContinuation, now: Duration) -> Result<(ModelReply, ContinuationId), AiError> {
        let generation = &prepared.lease.reservation.generation;
        generation.check()?;
        let session = self.sessions.get_mut(&prepared.lease.key).ok_or_else(|| AiError::invalid("session"))?;
        if !Arc::ptr_eq(generation, &session.generation) || now < prepared.lease.began {
            return Err(AiError::invalid("continuation"));
        }
        if let Some(parent) = &prepared.lease.request.continuation {
            let entry = session.entries.get(parent).ok_or_else(|| AiError::invalid("continuation"))?;
            if now < entry.created
                || now.saturating_sub(entry.created) >= Duration::from_secs(generation.limits.continuation_ttl_seconds.into())
                || !entry.exclusive.upgrade().is_some_and(|token| Arc::ptr_eq(&token, &prepared.lease.token))
                || !entry.reservation.owns_slot
                || prepared.lease.reservation.owns_slot
            {
                return Err(AiError::invalid("continuation"));
            }
        }
        if session.entries.contains_key(&prepared.id) {
            return Err(AiError::invalid("continuation"));
        }
        if prepared.charge > prepared.lease.reservation.bytes
            || generation.bytes.load(Ordering::Acquire) > generation.limits.continuation_bytes as usize
            || generation.slots.load(Ordering::Acquire) > generation.limits.continuation_handles as usize
        {
            return Err(exhausted(&generation.limits));
        }
        let PreparedContinuation { lease, id, request, private, reply, charge } = prepared;
        let ContinuationLease { key, request: original, private: prior, token, wire, mut reservation, .. } = lease;
        // All validation is over. No fallible operation follows parent removal.
        if let Some(parent) = &original.continuation
            && let Some(mut old) = session.entries.remove(parent)
        {
            // Transfer one existing slot without an intermediate increment/decrement.
            old.reservation.owns_slot = false;
            reservation.owns_slot = true;
        }
        drop((key, original, prior, token));
        reservation.shrink(charge);
        session.entries.insert(id.clone(), Entry { request, private, wire, created: now, exclusive: Weak::new(), reservation });
        Ok((reply, id))
    }

    pub(in crate::agent_chat) fn retire(&mut self, key: &SessionKey) {
        if let Some(session) = self.sessions.get(key) {
            session.generation.live.store(false, Ordering::Release);
        }
        // Revocation precedes removal. Outstanding leases retain only their revoked
        // generation's quota; a new session under this key cannot resurrect them.
        self.sessions.remove(key);
    }
}

fn extends(prefix: &GenerateRequest, next: &GenerateRequest) -> bool {
    prefix.connection_id == next.connection_id
        && prefix.model_id == next.model_id
        && prefix.tools == next.tools
        && prefix.options == next.options
        && next.messages.len() > prefix.messages.len()
        && next.messages.starts_with(&prefix.messages)
}

fn exhausted(limits: &AiLimits) -> AiError {
    AiError::exhausted("continuation_bytes", limits.continuation_bytes)
}

fn sum(limits: &AiLimits, values: &[usize]) -> Result<usize, AiError> {
    values.iter().try_fold(0usize, |total, value| {
        total.checked_add(*value).filter(|n| *n <= limits.continuation_bytes as usize).ok_or_else(|| exhausted(limits))
    })
}

fn overhead() -> usize {
    2 * (size_of::<Entry>() + size_of::<ContinuationLease>() + size_of::<PreparedContinuation>() + size_of::<ModelReply>() + size_of::<SessionKey>())
}

fn request_charge(request: &GenerateRequest, limits: &AiLimits) -> Result<usize, AiError> {
    let mut charge = normalized_prefix_charge(&request.messages, 0, limits)?;
    charge = sum(
        limits,
        &[
            charge,
            size_of::<GenerateRequest>(),
            request.connection_id.len(),
            request.model_id.len(),
            32,
            request.options.reasoning_effort.as_ref().map_or(0, String::len),
        ],
    )?;
    for tool in &request.tools {
        charge = sum(
            limits,
            &[
                charge,
                size_of::<super::ToolDefinition>(),
                tool.name.len(),
                tool.description.len(),
                retained_object_charge(&tool.input_schema, limits)?,
            ],
        )?;
    }
    if let Some(schema) = &request.options.output_schema {
        charge = sum(limits, &[charge, retained_object_charge(schema, limits)?])?;
    }
    Ok(charge)
}

fn state_charge(private: &ProviderState, limits: &AiLimits) -> Result<usize, AiError> {
    let mut charge = sum(limits, &[size_of::<ProviderState>(), private.routing_session_id.len()])?;
    for value in &private.native_assistants {
        charge = sum(limits, &[charge, retained_json_charge(value, limits)?])?;
    }
    for (key, value) in private.gemini_signatures.iter().chain(private.wire_to_provider_tool_ids.iter()) {
        charge = sum(limits, &[charge, 2 * size_of::<String>() + size_of::<[usize; 8]>(), key.len(), value.len()])?;
    }
    Ok(charge)
}

fn reply_charge(reply: &ProviderReply, limits: &AiLimits) -> Result<usize, AiError> {
    let mut charge =
        sum(limits, &[size_of::<ProviderReply>(), state_charge(&reply.private, limits)?, retained_json_charge(&reply.assistant, limits)?])?;
    for (_, text) in &reply.text {
        charge = sum(limits, &[charge, size_of::<(bool, String)>(), text.len()])?;
    }
    for call in &reply.calls {
        let arguments = call.arguments.as_ref().map_err(|_| AiError::malformed())?;
        charge = sum(
            limits,
            &[charge, size_of::<super::provider::ProviderToolCall>(), call.id.len(), call.name.len(), retained_object_charge(arguments, limits)?],
        )?;
    }
    Ok(charge)
}

/// Must run outside BOTH store and CallControl publication locks. No public success,
/// parent supersession or store-map mutation occurs during this function.
pub(in crate::agent_chat) fn prepare_continuation(
    mut lease: ContinuationLease,
    request: &GenerateRequest,
    reply: &ProviderReply,
    ids: &IdFn,
) -> Result<PreparedContinuation, AiError> {
    lease.reservation.generation.check()?;
    if &lease.request != request {
        return Err(AiError::invalid("continuation"));
    }
    let limits = lease.reservation.generation.limits.clone();
    if reply.calls.len() > limits.tool_calls as usize {
        return Err(AiError::exhausted("tool_calls", limits.tool_calls));
    }
    if reply.text.iter().any(|(_, text)| text.len() > limits.text_bytes as usize) {
        return Err(AiError::exhausted("text_bytes", limits.text_bytes));
    }
    // Charge the bounded workspace BEFORE looking into reply/private/argument trees,
    // against current aggregate use (including concurrent prepares), not the ceiling.
    // Keep it through candidate request/state charging and every error/drop path.
    let _charging_workspace = Reservation::new(&lease.reservation.generation, CHARGING_WORKSPACE_BYTES, false)?;
    let raw = reply_charge(reply, &limits)?;
    let candidates =
        (reply.calls.len() + 1).checked_mul(128 + 2 * size_of::<String>() + size_of::<[usize; 8]>()).ok_or_else(|| exhausted(&limits))?;
    // Includes borrowed reply, its candidate private clone, two normalized projections,
    // ID/map candidates and scratch. Admission precedes ALL candidate cloning/callbacks.
    let additional = sum(&limits, &[raw, raw, raw, raw, candidates, candidates])?;
    lease.reservation.grow(additional)?;
    if reply.private.routing_session_id != lease.private.routing_session_id
        || reply.private.native_assistants.len() != lease.private.native_assistants.len() + 1
        || !reply.private.native_assistants.starts_with(&lease.private.native_assistants)
        || reply.private.wire_to_provider_tool_ids != lease.private.wire_to_provider_tool_ids
    {
        return Err(AiError::malformed());
    }
    let native = reply.private.native_assistants.last().ok_or_else(AiError::malformed)?;
    let id = ContinuationId::parse(&ids()?)?;
    lease.reservation.generation.check()?;
    let mut used: BTreeSet<String> = request.messages.iter().flat_map(|m| m.tool_calls.iter().map(|c| c.id.clone())).collect();
    if !used.insert(id.as_str().to_owned()) {
        return Err(AiError::invalid("continuation"));
    }
    let mut round = BTreeSet::new();
    let mut private = lease.private.clone();
    private.native_assistants.push(native.clone());
    let mut tool_calls = Vec::with_capacity(reply.calls.len());
    for (index, call) in reply.calls.iter().enumerate() {
        if !round.insert(call.id.as_str())
            || call.id.is_empty()
            || call.id.len() > 256
            || call.name.is_empty()
            || call.name.len() > 256
            || call.name.chars().any(char::is_control)
        {
            return Err(AiError::malformed());
        }
        let wire_id =
            if valid_id(&call.id) && !used.contains(&call.id) { call.id.clone() } else { ContinuationId::parse(&ids()?)?.as_str().to_owned() };
        lease.reservation.generation.check()?;
        if !used.insert(wire_id.clone()) {
            return Err(AiError::invalid("continuation"));
        }
        private.wire_to_provider_tool_ids.insert(wire_id.clone(), call.id.clone());
        if lease.wire == WireProtocol::Gemini
            && let Some(signature) = native
                .get("parts")
                .and_then(Value::as_array)
                .and_then(|parts| parts.iter().filter(|p| p.get("functionCall").is_some()).nth(index))
                .and_then(|part| part.get("thoughtSignature"))
                .and_then(Value::as_str)
        {
            private.gemini_signatures.insert(wire_id.clone(), signature.to_owned());
        }
        tool_calls.push(ToolCall {
            id: wire_id,
            name: call.name.clone(),
            arguments: call.arguments.as_ref().map_err(|_| AiError::malformed())?.clone(),
        });
    }
    let mut public = ModelReply {
        text: reply.text.iter().filter(|(thought, _)| !thought).map(|(_, text)| text.clone()).collect(),
        tool_calls,
        finish_reason: reply.finish_reason,
        usage: reply.usage.clone(),
        continuation: Some(id.clone()),
        delivery: Delivery::Buffered,
    };
    // A caller rebuilding ModelMessage from this public value must retain an
    // assistant position even for private-only or empty successful replies.
    // Empty text is already legal input; do not permit content=[] globally.
    if public.text.is_empty() && public.tool_calls.is_empty() {
        public.text.push(String::new());
    }
    let assistant = ModelMessage {
        role: MessageRole::Assistant,
        content: public.text.iter().cloned().map(ContentPart::Text).collect(),
        tool_calls: public.tool_calls.clone(),
        tool_call_id: None,
    };
    // The stored prefix and the returned public reconstruction use the same parts.
    let mut candidate = request.clone();
    normalized_prefix_charge(std::slice::from_ref(&assistant), normalized_prefix_charge(&candidate.messages, 0, &limits)?, &limits)?;
    candidate.messages.push(assistant);
    candidate.continuation = None;
    let charge = sum(&limits, &[overhead(), request_charge(&candidate, &limits)?, state_charge(&private, &limits)?])?;
    if charge > lease.reservation.bytes {
        return Err(exhausted(&limits));
    }
    lease.reservation.generation.check()?;
    Ok(PreparedContinuation { lease, id, request: candidate, private, reply: public, charge })
}
