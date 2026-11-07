//! Provider-independent, non-wire admission values. No credentials or provider text.

use std::sync::Arc;
use std::time::Duration;

/// Monotonic time, using the same origin as call deadlines.
pub type ClockFn = Arc<dyn Fn() -> Duration + Send + Sync>;
pub type IdFn = Arc<dyn Fn() -> Result<String, AiError> + Send + Sync>;
pub type ArtifactReadFn = Arc<dyn Fn(&SessionKey, &ArtifactRef) -> Result<Arc<[u8]>, AiError> + Send + Sync>;

/// Host-owned session identity; validated at the session admission boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub runtime_instance_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallScope {
    pub session: SessionKey,
    pub turn_id: String,
    pub workflow_run_id: String,
}

pub struct CallContext {
    pub scope: CallScope,
    pub control: Arc<super::CallControl>,
    pub deadline: Duration,
    pub limits: AiLimits,
    pub model_budget: Arc<super::ModelCallBudget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Available,
    AuthRequired,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionDescription {
    pub state: ConnectionState,
    pub capabilities: AiCapabilities,
}

/// An authorized immutable reference, not a filesystem path or inline payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub digest: [u8; 32],
    pub size_bytes: u32,
    pub media_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    Image(ArtifactRef),
    Document { name: String, artifact: ArtifactRef },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

/// Opaque domain handle. Creation and branch admission belong to the continuation store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContinuationId(String);

impl ContinuationId {
    pub fn parse(value: &str) -> Result<Self, AiError> {
        if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(AiError::invalid("continuation"));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerateRequest {
    pub connection_id: String,
    pub model_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
    pub options: GenerateOptions,
    pub continuation: Option<ContinuationId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Refusal,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Buffered,
    Streaming,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelReply {
    pub text: Vec<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
    pub continuation: Option<ContinuationId>,
    pub delivery: Delivery,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerateOptions {
    pub delivery: Delivery,
    pub timeout_seconds: u32,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub output_schema: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiCapabilities {
    pub model_generate: bool,
    pub agent_run: bool,
    pub tools: bool,
    pub images: bool,
    pub documents: bool,
    pub structured_output: bool,
    pub model_selection: bool,
    pub usage: bool,
    pub delivery: Delivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiErrorCode {
    InvalidRequest,
    UnsupportedCapability,
    AuthRequired,
    ConnectionChanged,
    ModelError,
    MalformedResponse,
    ResourceExhausted,
    Timeout,
    Cancelled,
    TransportError,
    InternalError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AiErrorDetails {
    None,
    Field { field: &'static str },
    Connection { connection_id: String, http_status: Option<u16> },
    Capability { capability: &'static str },
    Resource { resource: &'static str, limit: u32 },
    Operation { operation: &'static str },
}

/// Only fixed safe message IDs and typed, bounded details enter an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiError {
    code: AiErrorCode,
    message_id: &'static str,
    details: AiErrorDetails,
    // Fixed private classifiers, never provider text or part of the domain/wire details.
    compatibility: [bool; 2],
}

impl AiError {
    pub fn code(&self) -> AiErrorCode {
        self.code
    }

    pub fn message(&self) -> String {
        crate::i18n::tr(self.message_id).to_string()
    }

    pub fn details(&self) -> &AiErrorDetails {
        &self.details
    }

    pub(in crate::agent_chat) fn retry_reasoning_none(&self) -> bool {
        self.compatibility[0]
    }
    pub(in crate::agent_chat) fn retry_go_protocol(&self) -> bool {
        self.compatibility[1]
    }
    pub(super) fn with_compatibility(mut self, reasoning: bool, go: bool) -> Self {
        self.compatibility = [reasoning, go];
        self
    }

    pub(super) fn invalid(field: &'static str) -> Self {
        Self {
            code: AiErrorCode::InvalidRequest,
            message_id: "ui.host_ai_invalid_request",
            details: AiErrorDetails::Field { field },
            compatibility: [false; 2],
        }
    }

    pub(super) fn exhausted(resource: &'static str, limit: u32) -> Self {
        Self {
            code: AiErrorCode::ResourceExhausted,
            message_id: "ui.host_ai_resource_limit",
            details: AiErrorDetails::Resource { resource, limit },
            compatibility: [false; 2],
        }
    }

    pub(super) fn internal(operation: &'static str) -> Self {
        Self {
            code: AiErrorCode::InternalError,
            message_id: if operation == "read_secret" { "ui.host_ai_key_unavailable" } else { "ui.host_ai_internal_error" },
            details: AiErrorDetails::Operation { operation },
            compatibility: [false; 2],
        }
    }

    pub(super) fn connection_changed() -> Self {
        Self {
            code: AiErrorCode::ConnectionChanged,
            message_id: "ui.host_ai_connection_changed",
            details: AiErrorDetails::None,
            compatibility: [false; 2],
        }
    }

    pub(super) fn auth_required() -> Self {
        Self { code: AiErrorCode::AuthRequired, message_id: "ui.ai_http_key", details: AiErrorDetails::None, compatibility: [false; 2] }
    }

    pub(super) fn unsupported(capability: &'static str) -> Self {
        Self {
            code: AiErrorCode::UnsupportedCapability,
            message_id: "ui.host_ai_not_supported",
            details: AiErrorDetails::Capability { capability },
            compatibility: [false; 2],
        }
    }

    pub(super) fn cancelled() -> Self {
        Self { code: AiErrorCode::Cancelled, message_id: "ui.host_ai_cancelled", details: AiErrorDetails::None, compatibility: [false; 2] }
    }

    pub(super) fn timeout() -> Self {
        Self { code: AiErrorCode::Timeout, message_id: "ui.host_ai_timed_out", details: AiErrorDetails::None, compatibility: [false; 2] }
    }

    pub(super) fn malformed() -> Self {
        Self {
            code: AiErrorCode::MalformedResponse,
            message_id: "ui.host_ai_invalid_response",
            details: AiErrorDetails::None,
            compatibility: [false; 2],
        }
    }

    pub(super) fn transport() -> Self {
        Self {
            code: AiErrorCode::TransportError,
            message_id: "ui.host_ai_transport_failed",
            details: AiErrorDetails::None,
            compatibility: [false; 2],
        }
    }

    pub(super) fn model_failed() -> Self {
        Self { code: AiErrorCode::ModelError, message_id: "ui.host_ai_model_failed", details: AiErrorDetails::None, compatibility: [false; 2] }
    }

    pub(super) fn provider_status(connection_id: &str, status: u16, authentication: bool) -> Self {
        let mut error = if authentication { Self::auth_required() } else { Self::model_failed() };
        error.details = AiErrorDetails::Connection { connection_id: connection_id.into(), http_status: Some(status) };
        error
    }
}

#[derive(Clone, Debug)]
pub struct AiLimits {
    pub model_messages: u32,
    pub tool_definitions: u32,
    pub tool_calls: u32,
    pub text_bytes: u32,
    pub tool_json_bytes: u32,
    pub attachments: u32,
    pub attachment_bytes: u32,
    pub attachment_total_bytes: u32,
    pub continuation_handles: u32,
    /// Canonical normalized-message-prefix charge plus continuation storage. Preparation
    /// and Task6 must share messages::normalized_prefix_charge; artifact bytes are separate.
    pub continuation_bytes: u32,
    pub continuation_ttl_seconds: u32,
    pub http_per_connection: u32,
    pub http_per_host: u32,
    pub call_max_seconds: u32,
}

impl AiLimits {
    pub(super) fn intersection(&self, other: &Self) -> Self {
        Self {
            model_messages: self.model_messages.min(other.model_messages),
            tool_definitions: self.tool_definitions.min(other.tool_definitions),
            tool_calls: self.tool_calls.min(other.tool_calls),
            text_bytes: self.text_bytes.min(other.text_bytes),
            tool_json_bytes: self.tool_json_bytes.min(other.tool_json_bytes),
            attachments: self.attachments.min(other.attachments),
            attachment_bytes: self.attachment_bytes.min(other.attachment_bytes),
            attachment_total_bytes: self.attachment_total_bytes.min(other.attachment_total_bytes),
            continuation_handles: self.continuation_handles.min(other.continuation_handles),
            continuation_bytes: self.continuation_bytes.min(other.continuation_bytes),
            continuation_ttl_seconds: self.continuation_ttl_seconds.min(other.continuation_ttl_seconds),
            http_per_connection: self.http_per_connection.min(other.http_per_connection),
            http_per_host: self.http_per_host.min(other.http_per_host),
            call_max_seconds: self.call_max_seconds.min(other.call_max_seconds),
        }
    }

    pub fn contract_defaults() -> Self {
        Self {
            model_messages: 128,
            tool_definitions: 64,
            tool_calls: 32,
            text_bytes: 256 << 10,
            tool_json_bytes: 64 << 10,
            attachments: 4,
            attachment_bytes: 4 << 20,
            attachment_total_bytes: 16 << 20,
            continuation_handles: 32,
            continuation_bytes: 16 << 20,
            continuation_ttl_seconds: 86_400,
            http_per_connection: 2,
            http_per_host: 8,
            call_max_seconds: 600,
        }
    }

    pub fn validate(&self) -> Result<(), AiError> {
        let max = Self::contract_defaults();
        for (field, value, ceiling) in [
            ("model_messages", self.model_messages, max.model_messages),
            ("tool_definitions", self.tool_definitions, max.tool_definitions),
            ("tool_calls", self.tool_calls, max.tool_calls),
            ("text_bytes", self.text_bytes, max.text_bytes),
            ("tool_json_bytes", self.tool_json_bytes, max.tool_json_bytes),
            ("attachments", self.attachments, max.attachments),
            ("attachment_bytes", self.attachment_bytes, max.attachment_bytes),
            ("attachment_total_bytes", self.attachment_total_bytes, max.attachment_total_bytes),
            ("continuation_handles", self.continuation_handles, max.continuation_handles),
            ("continuation_bytes", self.continuation_bytes, max.continuation_bytes),
            ("continuation_ttl_seconds", self.continuation_ttl_seconds, max.continuation_ttl_seconds),
            ("http_per_connection", self.http_per_connection, max.http_per_connection),
            ("http_per_host", self.http_per_host, max.http_per_host),
            ("call_max_seconds", self.call_max_seconds, max.call_max_seconds),
        ] {
            if value == 0 || value > ceiling {
                return Err(AiError::invalid(field));
            }
        }
        Ok(())
    }
}
