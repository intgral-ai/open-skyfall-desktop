//! Protocol shapes only. None of these DTOs invokes a provider, authorizes a reference,
//! applies a command, verifies an artifact, or accepts a terminal proposal.

use super::*;
use serde_json::{Map, Value};

pub type Object = Map<String, Value>;

macro_rules! dto {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
        pub struct $name { $(pub $field: $ty,)* }
        impl $name {
            pub fn validate(&self, limits: &Limits) -> Result<(), WireError> {
                super::codec::validate_data(&serde_json::to_value(self).map_err(|_| WireError::invalid_request())?, limits)?;
                self.validate_text(limits)
            }
        }
    };
}
macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
        pub enum $name { $(#[serde(rename = $wire)] $variant,)+ }
    };
}

wire_enum!(ErrorCode {
    IncompatibleVersion=>"INCOMPATIBLE_VERSION", InvalidRequest=>"INVALID_REQUEST", MethodNotFound=>"METHOD_NOT_FOUND",
    UnsupportedCapability=>"UNSUPPORTED_CAPABILITY", AuthRequired=>"AUTH_REQUIRED", ConnectionChanged=>"CONNECTION_CHANGED",
    PermissionDenied=>"PERMISSION_DENIED", ModelError=>"MODEL_ERROR", MalformedResponse=>"MALFORMED_RESPONSE",
    TaskFailed=>"TASK_FAILED", InvalidProposal=>"INVALID_PROPOSAL", StaleDocument=>"STALE_DOCUMENT",
    DependencyChanged=>"DEPENDENCY_CHANGED", InvalidCandidate=>"INVALID_CANDIDATE", ProfileUnsupported=>"PROFILE_UNSUPPORTED",
    ReportMismatch=>"REPORT_MISMATCH", NotReady=>"NOT_READY", NotFound=>"NOT_FOUND", IdempotencyConflict=>"IDEMPOTENCY_CONFLICT",
    RequestExpired=>"REQUEST_EXPIRED", ResourceExhausted=>"RESOURCE_EXHAUSTED", Timeout=>"TIMEOUT", Cancelled=>"CANCELLED",
    TransportError=>"TRANSPORT_ERROR", OutcomeUnknown=>"OUTCOME_UNKNOWN", InternalError=>"INTERNAL_ERROR"
});
wire_enum!(Retry { Never=>"never", ReadOnly=>"read_only", SameSubmission=>"same_submission" });

dto!(EmptyDetails {});
dto!(SupportedDetails { supported: Vec<String> });
dto!(CapabilityDetails { capability: String });
dto!(ConnectionDetails { connection_id: Id });
dto!(PermissionDetails { permission_id: Id });
dto!(StaleDetails { expected: DocumentStamp, current: RequiredNullable<DocumentStamp> });
dto!(SubmissionDetails { submission_request_id: Id });
dto!(ResourceDetails { resource: String, limit: u32 });

fn optional_nonnull<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
macro_rules! optional_details {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
        pub struct $name { $(#[serde(default, deserialize_with="optional_nonnull", skip_serializing_if="Option::is_none")] pub $field: Option<$ty>,)* }
    };
}
optional_details!(FieldMethodDetails { field: String, method: String });
optional_details!(CandidateDetails { candidate_id: Id, dependency_id: Id });
optional_details!(ProfileDetails { run_id: Id, profile_id: Id });
optional_details!(ReferenceDetails { run_id: Id, artifact_id: Id });
optional_details!(OperationDetails { operation: String });

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub struct ModelErrorDetails {
    pub connection_id: Id,
    #[serde(default, deserialize_with = "optional_nonnull", skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ErrorDetails {
    Empty(EmptyDetails),
    Supported(SupportedDetails),
    FieldMethod(FieldMethodDetails),
    Capability(CapabilityDetails),
    Connection(ConnectionDetails),
    Permission(PermissionDetails),
    Model(ModelErrorDetails),
    Stale(StaleDetails),
    Candidate(CandidateDetails),
    Profile(ProfileDetails),
    Reference(ReferenceDetails),
    Submission(SubmissionDetails),
    Resource(ResourceDetails),
    Operation(OperationDetails),
}

#[derive(Clone, PartialEq, Serialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
    pub retry: Retry,
    pub details: ErrorDetails,
}

impl WireError {
    pub(super) fn local(code: ErrorCode) -> Self {
        let message = match code {
            ErrorCode::InvalidRequest => "Invalid protocol value.",
            ErrorCode::InternalError => "Internal protocol value error.",
            ErrorCode::MethodNotFound => "Unknown protocol method.",
            _ => "Protocol transport failed.",
        };
        Self { code, message: message.to_owned(), retry: Retry::Never, details: ErrorDetails::Empty(EmptyDetails {}) }
    }
    pub(super) fn invalid_request() -> Self {
        Self::local(ErrorCode::InvalidRequest)
    }
    pub(super) fn internal() -> Self {
        Self::local(ErrorCode::InternalError)
    }
}

// Never include peer/provider text or details in local diagnostics.
impl fmt::Debug for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireError").field("code", &self.code).field("retry", &self.retry).finish_non_exhaustive()
    }
}
impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Protocol service error.")
    }
}
impl std::error::Error for WireError {}

impl<'de> Deserialize<'de> for WireError {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            code: ErrorCode,
            message: String,
            retry: Retry,
            details: Object,
        }
        let fields = Fields::deserialize(d)?;
        if fields.message.len() > 2048 {
            return Err(de::Error::custom("Invalid protocol error."));
        }
        let value = Value::Object(fields.details);
        fn parse<T: de::DeserializeOwned>(v: Value) -> Result<T, serde_json::Error> {
            serde_json::from_value(v)
        }
        let details = if value.as_object().is_some_and(Map::is_empty) {
            ErrorDetails::Empty(EmptyDetails {})
        } else {
            use ErrorCode::*;
            let result = match fields.code {
                IncompatibleVersion => parse(value).map(ErrorDetails::Supported),
                InvalidRequest | MethodNotFound => parse(value).map(ErrorDetails::FieldMethod),
                UnsupportedCapability => parse(value).map(ErrorDetails::Capability),
                AuthRequired | ConnectionChanged | TaskFailed | InvalidProposal => parse(value).map(ErrorDetails::Connection),
                PermissionDenied => parse(value).map(ErrorDetails::Permission),
                ModelError | MalformedResponse => parse(value).map(ErrorDetails::Model),
                StaleDocument => parse(value).map(ErrorDetails::Stale),
                DependencyChanged | InvalidCandidate => parse(value).map(ErrorDetails::Candidate),
                ProfileUnsupported | ReportMismatch => parse(value).map(ErrorDetails::Profile),
                NotReady | NotFound => parse(value).map(ErrorDetails::Reference),
                IdempotencyConflict | RequestExpired => parse(value).map(ErrorDetails::Submission),
                ResourceExhausted => parse(value).map(ErrorDetails::Resource),
                Timeout | Cancelled | TransportError | OutcomeUnknown | InternalError => parse(value).map(ErrorDetails::Operation),
            };
            result.map_err(|_| de::Error::custom("Invalid protocol error details."))?
        };
        if matches!(&details, ErrorDetails::Model(x) if x.http_status.is_some_and(|v| v > 2_147_483_647))
            || matches!(&details, ErrorDetails::Resource(x) if x.limit > 2_147_483_647)
        {
            return Err(de::Error::custom("Invalid protocol integer."));
        }
        Ok(Self { code: fields.code, message: fields.message, retry: fields.retry, details })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timestamp(String);
impl Timestamp {
    pub fn parse(text: &str) -> Result<Self, WireError> {
        let b = text.as_bytes();
        if b.len() != 24
            || !b.iter().enumerate().all(|(i, c)| match i {
                4 | 7 => *c == b'-',
                10 => *c == b'T',
                13 | 16 => *c == b':',
                19 => *c == b'.',
                23 => *c == b'Z',
                _ => c.is_ascii_digit(),
            })
        {
            return Err(WireError::invalid_request());
        }
        let n = |range: std::ops::Range<usize>| text.get(range).and_then(|s| s.parse::<u32>().ok()).ok_or_else(WireError::invalid_request);
        let (year, month, day) = (n(0..4)?, n(5..7)?, n(8..10)?);
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
            2 => 28,
            _ => 0,
        };
        if day == 0 || day > days || n(11..13)? > 23 || n(14..16)? > 59 || n(17..19)? > 59 {
            return Err(WireError::invalid_request());
        }
        Ok(Self(text.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
string_serde!(Timestamp);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionKey(Id);
impl SubmissionKey {
    pub fn parse(text: &str) -> Result<Self, WireError> {
        let id = Id::parse(text)?;
        let (millis, uuid) = text.split_once(':').ok_or_else(WireError::invalid_request)?;
        if millis.is_empty()
            || !millis.bytes().all(|c| c.is_ascii_digit())
            || millis.parse::<u64>().is_err()
            || uuid.len() != 36
            || !uuid.bytes().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == b'-',
                14 => c == b'4',
                19 => b"89abAB".contains(&c),
                _ => c.is_ascii_hexdigit(),
            })
        {
            return Err(WireError::invalid_request());
        }
        Ok(Self(id))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl fmt::Display for SubmissionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
string_serde!(SubmissionKey);

wire_enum!(Delivery {Buffered=>"buffered", Streaming=>"streaming"});
wire_enum!(ConnectionState {Unknown=>"unknown", Available=>"available", AuthRequired=>"auth_required", Unavailable=>"unavailable"});
wire_enum!(ConnectionAvailability {Available=>"available", AuthRequired=>"auth_required", Unavailable=>"unavailable"});
wire_enum!(Role {System=>"system", User=>"user", Assistant=>"assistant", Tool=>"tool"});
wire_enum!(FinishReason {Stop=>"stop", ToolCalls=>"tool_calls", Length=>"length", Refusal=>"refusal", Unknown=>"unknown"});
wire_enum!(CancelReason {User=>"user", Reset=>"reset", Deadline=>"deadline", Shutdown=>"shutdown"});
wire_enum!(ShutdownReason {AppExit=>"app_exit", Reconfigure=>"reconfigure", ProtocolError=>"protocol_error"});
wire_enum!(ReadyState {Ready=>"ready"});
wire_enum!(AcceptedState {Accepted=>"accepted"});
wire_enum!(CancellationState {Cancelling=>"cancelling", Terminal=>"terminal"});
wire_enum!(StoppingState {Stopping=>"stopping"});
wire_enum!(CompletedState {Completed=>"completed"});
wire_enum!(BridgeName {SolidCraftApp=>"solidcraft-app"});
wire_enum!(DelegationMode {ProposalOnly=>"proposal_only"});
wire_enum!(SchemaVersion {V1=>"1.0"});
wire_enum!(StageState {Running=>"running", Completed=>"completed", Failed=>"failed"});
wire_enum!(StepState {Pending=>"pending", Running=>"running", Completed=>"completed", Failed=>"failed"});
wire_enum!(TextChannel {Answer=>"answer", Progress=>"progress"});
wire_enum!(PermissionState {Waiting=>"waiting", Allowed=>"allowed", Denied=>"denied"});
wire_enum!(TurnTerminal {ValidatedSuccess=>"validated_success", QualityFailed=>"quality_failed", TaskFailed=>"task_failed", AuthRequired=>"auth_required", BudgetExhausted=>"budget_exhausted", Cancelled=>"cancelled", TransportError=>"transport_error", OutcomeUnknown=>"outcome_unknown"});
wire_enum!(Changes {None=>"none", Applied=>"applied", RolledBack=>"rolled_back", Unknown=>"unknown"});
wire_enum!(ValidationState {Queued=>"queued", Running=>"running", Passed=>"passed", QualityFailed=>"quality_failed", InfrastructureError=>"infrastructure_error", Cancelled=>"cancelled", Unknown=>"unknown"});
wire_enum!(ReportState {Passed=>"passed", QualityFailed=>"quality_failed", InfrastructureError=>"infrastructure_error", Cancelled=>"cancelled", Unknown=>"unknown"});
wire_enum!(CheckState {Passed=>"passed", Failed=>"failed", Error=>"error", Unsupported=>"unsupported", NotRun=>"not_run"});
wire_enum!(WriteState {Applied=>"applied", CommandFailed=>"command_failed", ExpectationFailed=>"expectation_failed", Pending=>"pending", Stale=>"stale", Cancelled=>"cancelled", Unknown=>"unknown"});
wire_enum!(InputKind {Design=>"design", Component=>"component", Import=>"import", Requirements=>"requirements"});

dto!(ArtifactRef { artifact_id: Id, digest: Digest, size_bytes: u32, media_type: String });
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub enum ContentBlock {
    Text { text: String },
    Image { artifact: ArtifactRef },
    Document { name: String, artifact: ArtifactRef },
}
dto!(AiCapabilities {
    model_generate: bool,
    agent_run: bool,
    tools: bool,
    images: bool,
    documents: bool,
    structured_output: bool,
    model_selection: bool,
    usage: bool,
    delivery: Delivery
});
dto!(ModelMessage {role:Role, content:Vec<ContentBlock>, tool_calls:Vec<ToolCall>, tool_call_id:RequiredNullable<Id>});
dto!(ToolCall { id: Id, name: String, arguments: Object });
dto!(ToolDefinition { name: String, description: String, input_schema: Object });
dto!(Usage {input_tokens:RequiredNullable<u32>, output_tokens:RequiredNullable<u32>, total_tokens:RequiredNullable<u32>});
dto!(RuntimeCapabilities {workflow_ids:Vec<Id>, host_model_calls:bool, host_agent_tasks:bool, guarded_execute:bool, guarded_batch:bool, snapshot_validation:bool});
dto!(BridgeRef { name: BridgeName, bridge_id: Id });
dto!(TurnBinding { workflow_id: Id, connection_id: Id, model_id: String, document: DocumentStamp, profile: ProfileRef });
dto!(TurnContext {selection:Vec<Object>, requirements:RequiredNullable<ArtifactRef>});
dto!(AuthMethod { id: Id, name: String, description: String });
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub struct ModelOptions {
    pub delivery: Delivery,
    pub timeout_seconds: u32,
    pub max_output_tokens: RequiredNullable<u32>,
    #[serde(serialize_with = "serialize_temperature")]
    pub temperature: RequiredNullable<f64>,
    pub reasoning_effort: RequiredNullable<String>,
    pub output_schema: RequiredNullable<Object>,
}
fn serialize_temperature<S: Serializer>(value: &RequiredNullable<f64>, s: S) -> Result<S::Ok, S::Error> {
    if value.0.is_some_and(|n| !n.is_finite()) {
        return Err(serde::ser::Error::custom("Invalid temperature."));
    }
    value.serialize(s)
}
impl ModelOptions {
    pub fn validate(&self, limits: &Limits) -> Result<(), WireError> {
        super::codec::validate_data(&serde_json::to_value(self).map_err(|_| WireError::invalid_request())?, limits)
    }
}
dto!(AgentContext {document:DocumentStamp, requirements:RequiredNullable<ArtifactRef>, delegation_mode:DelegationMode});

dto!(RuntimeHelloParams {supported_versions:Vec<String>, host_version:String, limits:Limits});
dto!(RuntimeHelloResult {
    selected_version: String,
    runtime_instance_id: Id,
    runtime_version: String,
    capabilities: RuntimeCapabilities,
    limits: Limits
});
dto!(SessionStartParams {workflow_id:Id, instructions:String, bridge:RequiredNullable<BridgeRef>});
dto!(SessionStartResult { state: ReadyState, workflow_id: Id });
dto!(TurnStartParams {binding:TurnBinding, parts:Vec<ContentBlock>, context:TurnContext, budgets:Budgets});
dto!(TurnStartResult { state: AcceptedState });
dto!(TurnCancelParams { reason: CancelReason });
dto!(TurnCancelResult { state: CancellationState });
dto!(RequestCancelParams { target_request_id: RequestId });
dto!(RequestCancelResult { state: CancellationState });
dto!(SessionResetParams { next_session_id: Id, instructions: String });
dto!(SessionResetResult { state: ReadyState, session_id: Id });
dto!(RuntimeShutdownParams { reason: ShutdownReason });
dto!(RuntimeShutdownResult { state: StoppingState });
dto!(ConnectionDescribeParams { connection_id: Id, model_id: String });
dto!(ConnectionDescribeResult {state:ConnectionAvailability, capabilities:AiCapabilities, auth_methods:Vec<AuthMethod>});
dto!(ModelGenerateParams {connection_id:Id, model_id:String, messages:Vec<ModelMessage>, tools:Vec<ToolDefinition>, options:ModelOptions, continuation:RequiredNullable<Id>});
dto!(ModelGenerateResult {content:Vec<ContentBlock>, tool_calls:Vec<ToolCall>, finish_reason:FinishReason, usage:RequiredNullable<Usage>, continuation:RequiredNullable<Id>, delivery:Delivery});
dto!(AgentRunParams {connection_id:Id, model_id:String, task:Vec<ContentBlock>, context:AgentContext, timeout_seconds:u32});
dto!(AgentRunResult {agent_session_id:Id, state:CompletedState, content:Vec<ContentBlock>, stop_reason:String, usage:RequiredNullable<Usage>});
dto!(ValidationSubmitParams { candidate: CandidateRef, profile: ProfileRef, request_id: SubmissionKey });
dto!(ValidationSubmitResult { run_id: Id, state: ValidationState, candidate: CandidateRef, profile: ProfileRef });
dto!(ValidationStatusParams { run_id: Id });
dto!(ValidationStatusResult {
    run_id: Id,
    state: ValidationState,
    candidate: CandidateRef,
    profile: ProfileRef,
    updated_at: Timestamp,
    report_available: bool
});
dto!(ValidationReportParams { run_id: Id });
dto!(ValidationReportResult { report: ValidationReport, reference: ArtifactRef });
dto!(ValidationCancelParams { run_id: Id });
dto!(ValidationCancelResult { run_id: Id, state: ValidationState });

dto!(ProfileRef { profile_id: Id, version: String, digest: Digest });
dto!(CheckProfile {schema_version:SchemaVersion, profile_id:Id, version:String, checks:Vec<CheckRequirement>});
dto!(CheckRequirement { check_id: Id, kind: String, required: bool, parameters: Object });
dto!(ParameterRequirements {requirements:Vec<ParameterRequirement>});
dto!(ParameterRequirement { name: String, value: serde_json::Number, abs_tolerance: serde_json::Number });
dto!(GeometryMeasurements {measurements:Vec<GeometryMeasurement>});
dto!(GeometryMeasurement {
    command: String,
    params: Object,
    result_pointer: String,
    expected: serde_json::Number,
    abs_tolerance: serde_json::Number,
    rel_tolerance: serde_json::Number
});
dto!(CandidateRef { candidate_id: Id, source: DocumentStamp, candidate_digest: Digest, manifest_digest: Digest, artifact: ArtifactRef });
dto!(SnapshotManifest {schema_version:SchemaVersion, candidate_id:Id, source:DocumentStamp, captured_at:Timestamp, format_version:String, engine_version:String, inputs:Vec<ManifestInput>});
dto!(ManifestInput {path:String, kind:InputKind, dependency_id:Id, dependency_version:RequiredNullable<String>, digest:Digest, size_bytes:u32});
dto!(ValidationReport {schema_version:SchemaVersion, run_id:Id, candidate_id:Id, candidate_digest:Digest, manifest_digest:Digest, profile:ProfileRef, runner:Runner, state:ReportState, started_at:RequiredNullable<Timestamp>, finished_at:Timestamp, checks:Vec<CheckResult>, error:RequiredNullable<WireError>});
dto!(Runner { runner_id: Id, version: String, engine_version: String });
dto!(CheckResult {check_id:Id, required:bool, state:CheckState, expected:Object, actual:Object, evidence:Vec<ArtifactRef>});
dto!(WriteOutcome {state:WriteState, request:WriteRequest, before:DocumentStamp, after:RequiredNullable<DocumentStamp>, completed:RequiredNullable<u32>, rollback:Rollback, expectation:RequiredNullable<Expectation>, error:RequiredNullable<WireError>});
dto!(WriteRequest { bridge_id: Id, mcp_session_id: Id, request_id: Id });
dto!(Rollback {requested:bool, attempted:bool, restored:RequiredNullable<bool>, undone:RequiredNullable<u32>});
dto!(Expectation {ok:bool, measured:Object, expected:Object, mismatch:Vec<String>});

dto!(RuntimeReady { runtime_instance_id: Id });
dto!(ConnectionStateEvent {connection_id:Id, state:ConnectionState, auth_methods:Vec<AuthMethod>});
dto!(WorkflowStage { stage_id: Id, title: String, state: StageState });
dto!(WorkflowText { text: String, channel: TextChannel });
dto!(WorkflowPlan {steps:Vec<PlanStep>});
dto!(PlanStep { id: Id, title: String, state: StepState });
dto!(WorkflowTool {call_id:Id, name:String, state:StageState, output:RequiredNullable<Object>});
dto!(ModelDelta { index: u32, text: String });
dto!(TurnFinished {state:TurnTerminal, reason:String, document:DocumentStamp, changes:Changes, validation_run_id:RequiredNullable<Id>, report:RequiredNullable<ArtifactRef>});
dto!(ToolProgress { call_id: Id, title: String, state: StageState });
dto!(PermissionProgress { permission_id: Id, title: String, state: PermissionState });
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "content", rename_all = "lowercase", deny_unknown_fields)]
#[cfg_attr(all(test, not(target_arch = "wasm32")), derive(schemars::JsonSchema))]
pub enum AgentProgressContent {
    Text(ProgressText),
    Tool(ToolProgress),
    Plan(WorkflowPlan),
    Permission(PermissionProgress),
}
dto!(ProgressText { text: String });
#[derive(Clone, Debug, PartialEq)]
pub struct AgentProgress {
    pub agent_session_id: Id,
    pub content: AgentProgressContent,
}
impl Serialize for AgentProgress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::to_value(&self.content).map_err(serde::ser::Error::custom)?;
        object
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("Invalid progress."))?
            .insert("agentSessionId".to_owned(), serde_json::to_value(&self.agent_session_id).map_err(serde::ser::Error::custom)?);
        object.serialize(s)
    }
}
impl<'de> Deserialize<'de> for AgentProgress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Fields {
            agent_session_id: Id,
            kind: String,
            content: Object,
        }
        let fields = Fields::deserialize(d)?;
        let content =
            serde_json::from_value(json!({"kind":fields.kind,"content":fields.content})).map_err(|_| de::Error::custom("Invalid progress."))?;
        Ok(Self { agent_session_id: fields.agent_session_id, content })
    }
}

/// Only typed text blocks/deltas consume textBytes (contract section 4). All
/// other strings keep their declared identity/enum/option/error/frame bounds.
/// Explicit classification prevents opaque objects named `content` or `text`
/// from either hiding a real block or being mistaken for one.
pub(super) trait TextPayload {
    fn validate_text(&self, _limits: &Limits) -> Result<(), WireError> {
        Ok(())
    }
}

impl<T: TextPayload> TextPayload for Box<T> {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        self.as_ref().validate_text(limits)
    }
}

fn text_bytes(text: &str, limits: &Limits) -> Result<(), WireError> {
    if text.len() > limits.text_bytes as usize { Err(WireError::invalid_request()) } else { Ok(()) }
}

fn text_blocks(blocks: &[ContentBlock], limits: &Limits) -> Result<(), WireError> {
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            text_bytes(text, limits)?;
        }
    }
    Ok(())
}

impl TextPayload for ModelMessage {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_blocks(&self.content, limits)
    }
}
impl TextPayload for TurnStartParams {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_blocks(&self.parts, limits)
    }
}
impl TextPayload for ModelGenerateParams {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        for message in &self.messages {
            message.validate_text(limits)?;
        }
        Ok(())
    }
}
impl TextPayload for ModelGenerateResult {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_blocks(&self.content, limits)
    }
}
impl TextPayload for AgentRunParams {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_blocks(&self.task, limits)
    }
}
impl TextPayload for AgentRunResult {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_blocks(&self.content, limits)
    }
}
impl TextPayload for WorkflowText {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_bytes(&self.text, limits)
    }
}
impl TextPayload for ModelDelta {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_bytes(&self.text, limits)
    }
}
impl TextPayload for ProgressText {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        text_bytes(&self.text, limits)
    }
}
impl TextPayload for AgentProgress {
    fn validate_text(&self, limits: &Limits) -> Result<(), WireError> {
        match &self.content {
            AgentProgressContent::Text(text) => text.validate_text(limits),
            _ => Ok(()),
        }
    }
}

// No text blocks in these DTOs. Instructions, labels/titles, descriptions,
// reasons, IDs, versions and media types are not ContentBlock::Text payloads.
// Opaque objects retain their separate JSON/frame limits, not a textBytes cap.
macro_rules! non_text_payloads {
    ($($ty:ty),+ $(,)?) => { $(impl TextPayload for $ty {})+ };
}
non_text_payloads!(
    EmptyDetails,
    SupportedDetails,
    CapabilityDetails,
    ConnectionDetails,
    PermissionDetails,
    StaleDetails,
    SubmissionDetails,
    ResourceDetails,
    ArtifactRef,
    AiCapabilities,
    ToolCall,
    ToolDefinition,
    Usage,
    RuntimeCapabilities,
    BridgeRef,
    TurnBinding,
    TurnContext,
    AuthMethod,
    AgentContext,
    RuntimeHelloParams,
    RuntimeHelloResult,
    SessionStartParams,
    SessionStartResult,
    TurnStartResult,
    TurnCancelParams,
    TurnCancelResult,
    RequestCancelParams,
    RequestCancelResult,
    SessionResetParams,
    SessionResetResult,
    RuntimeShutdownParams,
    RuntimeShutdownResult,
    ConnectionDescribeParams,
    ConnectionDescribeResult,
    ValidationSubmitParams,
    ValidationSubmitResult,
    ValidationStatusParams,
    ValidationStatusResult,
    ValidationReportParams,
    ValidationReportResult,
    ValidationCancelParams,
    ValidationCancelResult,
    ProfileRef,
    CheckProfile,
    CheckRequirement,
    ParameterRequirements,
    ParameterRequirement,
    GeometryMeasurements,
    GeometryMeasurement,
    CandidateRef,
    SnapshotManifest,
    ManifestInput,
    ValidationReport,
    Runner,
    CheckResult,
    WriteOutcome,
    WriteRequest,
    Rollback,
    Expectation,
    RuntimeReady,
    ConnectionStateEvent,
    WorkflowStage,
    WorkflowPlan,
    PlanStep,
    WorkflowTool,
    TurnFinished,
    ToolProgress,
    PermissionProgress,
);

macro_rules! method_sum {
    ($name:ident { $($variant:ident ($payload:ty) => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Debug, PartialEq)]
        pub enum $name { $($variant($payload),)+ }
        impl $name {
            pub fn method(&self)->&'static str {match self {$(Self::$variant(_)=>$wire,)+}}
            pub(super) fn value(&self)->Result<Value,WireError> {match self {$(Self::$variant(x)=>serde_json::to_value(x),)+}.map_err(|_|WireError::invalid_request())}
            pub(super) fn decode(method:&str, value:Value, limits:&Limits)->Result<Self,WireError> {
                match method { $($wire=>{let typed:$payload=super::codec::typed(value,limits)?;Ok(Self::$variant(typed))},)+ _=>Err(WireError::local(ErrorCode::MethodNotFound)) }
            }
        }
    };
}
method_sum!(HostRequest {
    RuntimeHello(RuntimeHelloParams)=>"runtime.hello", SessionStart(SessionStartParams)=>"session.start", TurnStart(Box<TurnStartParams>)=>"turn.start",
    TurnCancel(TurnCancelParams)=>"turn.cancel", RequestCancel(RequestCancelParams)=>"request.cancel", SessionReset(SessionResetParams)=>"session.reset", RuntimeShutdown(RuntimeShutdownParams)=>"runtime.shutdown"
});
method_sum!(RuntimeReply {
    RuntimeHello(Box<RuntimeHelloResult>)=>"runtime.hello", SessionStart(SessionStartResult)=>"session.start", TurnStart(TurnStartResult)=>"turn.start",
    TurnCancel(TurnCancelResult)=>"turn.cancel", RequestCancel(RequestCancelResult)=>"request.cancel", SessionReset(SessionResetResult)=>"session.reset", RuntimeShutdown(RuntimeShutdownResult)=>"runtime.shutdown"
});
method_sum!(HostCall {
    ConnectionDescribe(ConnectionDescribeParams)=>"connection.describe", ModelGenerate(ModelGenerateParams)=>"model.generate", AgentRun(AgentRunParams)=>"agent.run",
    ValidationSubmit(Box<ValidationSubmitParams>)=>"validation.submit", ValidationStatus(ValidationStatusParams)=>"validation.status", ValidationReport(ValidationReportParams)=>"validation.report", ValidationCancel(ValidationCancelParams)=>"validation.cancel"
});
method_sum!(HostReply {
    ConnectionDescribe(ConnectionDescribeResult)=>"connection.describe", ModelGenerate(ModelGenerateResult)=>"model.generate", AgentRun(AgentRunResult)=>"agent.run",
    ValidationSubmit(ValidationSubmitResult)=>"validation.submit", ValidationStatus(ValidationStatusResult)=>"validation.status", ValidationReport(ValidationReportResult)=>"validation.report", ValidationCancel(ValidationCancelResult)=>"validation.cancel"
});
method_sum!(HostEvent {ConnectionState(ConnectionStateEvent)=>"connection.state", AgentProgress(AgentProgress)=>"agent.progress", ModelDelta(ModelDelta)=>"model.delta"});
method_sum!(RuntimeEvent {RuntimeReady(RuntimeReady)=>"runtime.ready", WorkflowStage(WorkflowStage)=>"workflow.stage", WorkflowText(WorkflowText)=>"workflow.text", WorkflowPlan(WorkflowPlan)=>"workflow.plan", WorkflowTool(WorkflowTool)=>"workflow.tool", TurnFinished(TurnFinished)=>"turn.finished"});
#[derive(Clone, Debug, PartialEq)]
pub enum CallProgress {
    AgentProgress(AgentProgress),
    ModelDelta(ModelDelta),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodePhase {
    Bootstrap,
    Negotiated,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestHeader {
    pub protocol_version: String,
    pub request_id: RequestId,
    pub scope: Scope,
    pub method: String,
    pub extensions: Option<Object>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ResponseHeader {
    pub protocol_version: String,
    pub request_id: RequestId,
    pub scope: Scope,
    pub extensions: Option<Object>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct EventHeader {
    pub protocol_version: String,
    pub sequence: u32,
    pub request_id: RequiredNullable<Id>,
    pub scope: Scope,
    pub extensions: Option<Object>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum RequestBody {
    Host(HostRequest),
    Runtime(HostCall),
    Cancel(RequestCancelParams),
}

/// A successful response is deliberately unresolved, never a final host-service result.
/// The pending ledger must call the appropriate method-specific decoder before delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingResult(pub(super) Object);
impl PendingResult {
    pub fn decode_runtime(self, method: &str, limits: &Limits) -> Result<RuntimeReply, ProtocolViolation> {
        super::decode_runtime_reply(method, Value::Object(self.0), limits)
    }
    pub fn decode_host(self, method: &str, limits: &Limits) -> Result<HostReply, ProtocolViolation> {
        super::decode_host_reply(method, Value::Object(self.0), limits)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseBody {
    Ok(PendingResult),
    Error(WireError),
}
impl ResponseBody {
    pub fn host_ok(reply: HostReply, limits: &Limits) -> Result<Self, WireError> {
        let value = reply.value()?;
        super::decode_host_reply(reply.method(), value.clone(), limits).map_err(|_| WireError::invalid_request())?;
        match value {
            Value::Object(object) => Ok(Self::Ok(PendingResult(object))),
            _ => Err(WireError::internal()),
        }
    }
    pub fn runtime_ok(reply: RuntimeReply, limits: &Limits) -> Result<Self, WireError> {
        let value = reply.value()?;
        super::decode_runtime_reply(reply.method(), value.clone(), limits).map_err(|_| WireError::invalid_request())?;
        match value {
            Value::Object(object) => Ok(Self::Ok(PendingResult(object))),
            _ => Err(WireError::internal()),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum EventBody {
    Host(HostEvent),
    Runtime(RuntimeEvent),
}
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Request { header: RequestHeader, body: RequestBody },
    Response { header: ResponseHeader, body: ResponseBody },
    Event { header: EventHeader, body: EventBody },
}
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeResult {
    Frame(Frame),
    RequestError { header: RequestHeader, error: WireError },
}

/// Local diagnostics never retain hostile bytes or serde/provider error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolViolation;
impl ProtocolViolation {
    pub fn wire_error(self) -> WireError {
        WireError::local(ErrorCode::TransportError)
    }
}
impl fmt::Display for ProtocolViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid protocol transport frame.")
    }
}
impl std::error::Error for ProtocolViolation {}
