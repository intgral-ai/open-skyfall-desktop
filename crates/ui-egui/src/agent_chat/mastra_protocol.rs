//! Protocol 1.0 values and admission limits; independent of process and native document types.
//! Byte-level framing/depth/recursive duplicate-key validation belongs to the codec, not serde values.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::json;

pub use codec::{decode_frame, decode_host_reply, decode_runtime_reply, encode_control_frame, encode_frame};
pub use data::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Id(String);

impl Id {
    pub fn parse(value: &str) -> Result<Self, WireError> {
        if value.is_empty()
            || value.len() > 128
            || !value.starts_with(|c: char| c.is_ascii_alphanumeric())
            || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        {
            return Err(WireError::invalid_request());
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn parse(value: &str) -> Result<Self, WireError> {
        let hex = value.strip_prefix("sha256:").ok_or_else(WireError::invalid_request)?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(WireError::invalid_request());
        }
        let mut bytes = [0; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (out, pair) in bytes.iter_mut().zip(pairs) {
            let text = std::str::from_utf8(pair).map_err(|_| WireError::invalid_request())?;
            *out = u8::from_str_radix(text, 16).map_err(|_| WireError::invalid_request())?;
        }
        Ok(Self(bytes))
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        for byte in self.bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Revision(u64);

impl Revision {
    pub fn from_u64(value: u64) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Result<Self, WireError> {
        if value.is_empty() || value.len() > 20 || (value.len() > 1 && value.starts_with('0')) || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(WireError::invalid_request());
        }
        value.parse().map(Self).map_err(|_| WireError::invalid_request())
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

macro_rules! string_serde {
    ($($value:ty),+ $(,)?) => {$(
        impl Serialize for $value {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $value {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::parse(&String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
    )+};
}

string_serde!(Id, Digest, Revision);

mod codec;
mod data;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sender {
    Host,
    Runtime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RequestId(Id);

impl RequestId {
    pub fn parse(value: &str, sender: Sender) -> Result<Self, WireError> {
        let prefix = match sender {
            Sender::Host => "h:",
            Sender::Runtime => "r:",
        };
        if !value.starts_with(prefix) {
            return Err(WireError::invalid_request());
        }
        Id::parse(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let sender = if value.starts_with("h:") { Sender::Host } else { Sender::Runtime };
        Self::parse(&value, sender).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub struct TurnScope {
    pub session_id: Id,
    pub turn_id: Id,
    pub workflow_run_id: Id,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ScopeFields", into = "ScopeFields")]
pub enum Scope {
    Runtime,
    Session(Id),
    Turn(TurnScope),
}

/// An explicit newtype visit rejects serde's missing-field deserializer before visiting Option.
/// A transparent Option wrapper would silently accept a missing required-nullable field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RequiredNullable<T>(pub Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for RequiredNullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Required<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Required<T> {
            type Value = RequiredNullable<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a required value or explicit null")
            }
            fn visit_newtype_struct<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
                Option::deserialize(deserializer).map(RequiredNullable)
            }
        }
        deserializer.deserialize_newtype_struct("RequiredNullable", Required(std::marker::PhantomData))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScopeFields {
    session_id: RequiredNullable<Id>,
    turn_id: RequiredNullable<Id>,
    workflow_run_id: RequiredNullable<Id>,
}

impl TryFrom<ScopeFields> for Scope {
    type Error = WireError;
    fn try_from(fields: ScopeFields) -> Result<Self, Self::Error> {
        match (fields.session_id.0, fields.turn_id.0, fields.workflow_run_id.0) {
            (None, None, None) => Ok(Self::Runtime),
            (Some(id), None, None) => Ok(Self::Session(id)),
            (Some(session_id), Some(turn_id), Some(workflow_run_id)) => Ok(Self::Turn(TurnScope { session_id, turn_id, workflow_run_id })),
            _ => Err(WireError::invalid_request()),
        }
    }
}

impl From<Scope> for ScopeFields {
    fn from(scope: Scope) -> Self {
        let (session, turn, workflow) = match scope {
            Scope::Runtime => (None, None, None),
            Scope::Session(id) => (Some(id), None, None),
            Scope::Turn(turn) => (Some(turn.session_id), Some(turn.turn_id), Some(turn.workflow_run_id)),
        };
        Self { session_id: RequiredNullable(session), turn_id: RequiredNullable(turn), workflow_run_id: RequiredNullable(workflow) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub struct DocumentStamp {
    pub document_id: Id,
    pub revision: Revision,
}

/// The boundary takes native primitives, never importing UI types into a lower layer.
pub fn document_stamp_from_native(document_id: &str, revision: u64) -> Result<DocumentStamp, WireError> {
    Ok(DocumentStamp { document_id: Id::parse(document_id).map_err(|_| WireError::internal())?, revision: Revision::from_u64(revision) })
}

pub fn document_stamp_to_native(stamp: &DocumentStamp) -> (String, u64) {
    (stamp.document_id.as_str().to_owned(), stamp.revision.as_u64())
}

// Keep required wire fields, hard ceilings and the default offer in one table.
macro_rules! limits {
    ($($field:ident: $ceiling:literal),+ $(,)?) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
        pub struct Limits {
            $(#[cfg_attr(all(test, not(target_arch = "wasm32")), schemars(range(min = 1, max = $ceiling)))] pub $field: u32,)+
            pub store_bytes: Revision,
        }
        impl Default for Limits {
            fn default() -> Self {
                Self { $($field: $ceiling,)+ store_bytes: Revision::from_u64(8_589_934_592) }
            }
        }
        impl Limits {
            fn within(&self, ceiling: &Self) -> bool {
                $(self.$field > 0 && self.$field <= ceiling.$field &&)+
                self.store_bytes.as_u64() > 0 && self.store_bytes <= ceiling.store_bytes
            }
        }
    };
}

limits! {
    frame_bytes: 8_388_608,
    json_depth: 64,
    queue_bytes: 16_777_216,
    control_lane_bytes: 65_536,
    pending_requests: 32,
    control_slots: 1,
    request_ledger_entries: 4096,
    runtime_instances: 8,
    sessions: 1,
    active_turns: 1,
    stderr_lines: 40,
    stderr_line_scalars: 400,
    stderr_bytes: 65_536,
    model_messages: 128,
    tool_definitions: 64,
    tool_calls: 32,
    text_bytes: 262_144,
    tool_json_bytes: 65_536,
    attachments: 4,
    attachment_bytes: 4_194_304,
    attachment_total_bytes: 16_777_216,
    history_bytes: 16_777_216,
    history_messages: 128,
    continuation_handles: 32,
    continuation_bytes: 16_777_216,
    continuation_ttl_seconds: 86_400,
    http_per_connection: 2,
    http_per_host: 8,
    cad_writes_in_flight: 1,
    cad_reads_in_flight: 4,
    handshake_seconds: 10,
    session_start_seconds: 30,
    call_default_seconds: 300,
    call_max_seconds: 600,
    cancel_ack_seconds: 5,
    shutdown_seconds: 3,
    validation_workers: 2,
    validation_queue: 8,
    validation_seconds: 600,
    validation_queue_seconds: 300,
    snapshot_bytes: 1_073_741_824,
    snapshot_inputs: 256,
    manifest_bytes: 1_048_576,
    report_bytes: 4_194_304,
    checks: 128,
    evidence_artifacts: 64,
    evidence_bytes: 16_777_216,
    evidence_total_bytes: 67_108_864,
    job_records: 10_000,
    retention_seconds: 604_800,
}

pub const CONTROL_FRAME_BYTES: usize = 4096;

/// Largest unextended local cancel/shutdown frame, including LF. Local error text is
/// redacted plain ASCII (no JSON escapes); reserve its full 2048-byte message allowance.
pub fn minimum_control_frame_bytes() -> usize {
    let id = "x".repeat(128);
    let request_id = format!("h:{}", "x".repeat(126));
    let turn = json!({"sessionId":id,"turnId":id,"workflowRunId":id});
    let runtime = json!({"sessionId":null,"turnId":null,"workflowRunId":null});
    let request = |scope, method, params| json!({"protocolVersion":"1.0","kind":"request","scope":scope,"requestId":request_id,"method":method,"params":params});
    let reply = |scope, state| json!({"protocolVersion":"1.0","kind":"response","scope":scope,"requestId":request_id,"outcome":"ok","result":{"state":state}});
    [
        request(&turn, "turn.cancel", json!({"reason":"shutdown"})),
        request(&turn, "request.cancel", json!({"targetRequestId":request_id})),
        request(&runtime, "runtime.shutdown", json!({"reason":"protocol_error"})),
        reply(&turn, "cancelling"),
        reply(&runtime, "stopping"),
        json!({"protocolVersion":"1.0","kind":"response","scope":turn,"requestId":request_id,"outcome":"error",
            "error":{"code":"CANCELLED","message":"x".repeat(2048),"retry":"never","details":{"operation":"request.cancel"}}}),
    ]
    .iter()
    .map(codec::encoded_size_with_lf)
    .fold(0, usize::max)
}

pub fn validate_limits(limits: &Limits) -> Result<(), WireError> {
    let data_bytes = limits.queue_bytes.checked_sub(limits.control_lane_bytes);
    let reserve = limits.pending_requests.checked_add(limits.control_slots).and_then(|n| n.checked_mul(2)).and_then(|n| n.checked_add(8));
    let minimum = minimum_control_frame_bytes();
    if !limits.within(&Limits::default())
        || !data_bytes.is_some_and(|bytes| limits.frame_bytes <= bytes)
        || !reserve.is_some_and(|entries| limits.request_ledger_entries > entries)
        || u64::from(limits.frame_bytes) < minimum as u64
        || u64::from(limits.control_lane_bytes) < minimum as u64
        || limits.call_default_seconds > limits.call_max_seconds
        || limits.http_per_connection > limits.http_per_host
        || limits.attachment_bytes > limits.attachment_total_bytes
        || limits.evidence_bytes > limits.evidence_total_bytes
    {
        return Err(WireError::invalid_request());
    }
    Ok(())
}

pub fn negotiate_limits(offered: &Limits, returned: &Limits) -> Result<Limits, WireError> {
    validate_limits(offered)?;
    validate_limits(returned)?;
    if !returned.within(offered) {
        return Err(WireError::invalid_request());
    }
    Ok(returned.clone())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub struct Budgets {
    pub model_calls: u32,
    pub agent_tasks: u32,
    pub cad_commands: u32,
    pub repairs: u32,
    pub validation_runs: u32,
    pub wall_time_seconds: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Self { model_calls: 20, agent_tasks: 4, cad_commands: 64, repairs: 2, validation_runs: 3, wall_time_seconds: 1200 }
    }
}

/// Zero budgets are valid admission values. Exhaustion is a later turn-state decision,
/// not an INVALID_REQUEST, and zero wall time must never be replaced by a default.
pub fn validate_budgets(budgets: &Budgets) -> Result<(), WireError> {
    if budgets.model_calls > 20
        || budgets.agent_tasks > 4
        || budgets.cad_commands > 64
        || budgets.repairs > 2
        || budgets.validation_runs > 3
        || budgets.wall_time_seconds > 1200
    {
        return Err(WireError::invalid_request());
    }
    Ok(())
}
