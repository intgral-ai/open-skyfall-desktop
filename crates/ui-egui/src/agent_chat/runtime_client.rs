//! Native, bounded full-duplex runtime client and deterministic transport policy.
//! The worker owns the machine; injected services run only after apply returns.
//! Queue credit is logical wire ownership, not an allocator/RSS guarantee.

mod worker;
pub use worker::{PreparedEvent, PreparedRequest, ReplyPort, RuntimeClient, RuntimeSender};

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use super::mastra_protocol::*;
use super::process::InboundCredit;
use super::process::PipeLane;

pub struct RuntimeLaunch {
    pub executable: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    pub cwd: std::path::PathBuf,
    pub host_version: String,
    pub limits: Limits,
}
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}
pub trait IdSource: Send + Sync {
    fn next_id(&self) -> Result<Id, WireError>;
}
/// Nonblocking trusted flag/continuation retirement only. The worker calls
/// these hooks outside all IPC locks, before subsequent notices and dispatch.
pub trait ScopeLifecycle: Send + Sync {
    fn close_turn(&self, scope: &TurnScope, reason: CloseReason);
    fn retire_session(&self, session_id: &Id);
}
/// Trusted native adapter. Move input AND its token/port guard together into
/// asynchronous owners. Retain a guard until all managed input is destroyed,
/// even after finish/cancel/retirement. Independent copies need caller budgets.
/// Arbitrary contract-violating native Rust is not automatically constrained.
/// Finishing a port is NOT evidence that a separately retained input was freed.
pub trait HostDispatch: Send + Sync {
    fn dispatch(&self, call: CallbackCall, reply: ReplyPort);
}
pub struct RuntimeServices {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdSource>,
    pub admission: Arc<RuntimeAdmission>,
    pub lifecycle: Arc<dyn ScopeLifecycle>,
    pub dispatch: Arc<dyn HostDispatch>,
}
struct AdmissionState {
    limit: u32,
    active: u32,
    uncertain: Vec<super::process::ProcessHandle>,
}
impl AdmissionState {
    fn refresh_cleaned(&mut self) {
        let before = self.uncertain.len();
        self.uncertain.retain(|handle| handle.try_cleanup_result() != Some(Ok(())));
        self.active -= (before - self.uncertain.len()) as u32;
    }
}
/// One shared host pool, 1..8. Failed/uncertain owned cleanup retains its permit.
pub struct RuntimeAdmission(std::sync::Mutex<AdmissionState>);
impl RuntimeAdmission {
    pub fn new(limit: u32) -> Result<Self, ClientError> {
        if !(1..=8).contains(&limit) {
            return Err(ClientError { code: ErrorCode::InvalidRequest });
        }
        Ok(Self(std::sync::Mutex::new(AdmissionState { limit, active: 0, uncertain: Vec::new() })))
    }
    pub fn lower_limit(&self, limit: u32) -> Result<(), ClientError> {
        let mut state = self.0.lock().map_err(|_| ClientError { code: ErrorCode::TransportError })?;
        state.refresh_cleaned();
        if !(1..=state.limit).contains(&limit) {
            return Err(ClientError { code: ErrorCode::InvalidRequest });
        }
        if state.active > limit {
            return Err(ClientError { code: ErrorCode::ResourceExhausted });
        }
        state.limit = limit;
        Ok(())
    }
}
pub struct OsIds;
impl IdSource for OsIds {
    fn next_id(&self) -> Result<Id, WireError> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| error(ErrorCode::InternalError))?;
        use std::fmt::Write;
        let mut text = String::with_capacity(32);
        for byte in random {
            write!(&mut text, "{byte:02x}").map_err(|_| error(ErrorCode::InternalError))?;
        }
        Id::parse(&text).map_err(|_| error(ErrorCode::InternalError))
    }
}

const MAX_SEQUENCE: u32 = 2_147_483_647;

#[derive(Clone, Debug, PartialEq)]
pub struct ClientError {
    pub code: ErrorCode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceCapabilities {
    pub connection_describe: bool,
    pub model_generate: bool,
    pub agent_run: bool,
    pub validation: bool,
    pub streaming: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeInfo {
    pub selected_version: String,
    pub runtime_instance_id: Id,
    pub runtime_version: String,
    pub capabilities: RuntimeCapabilities,
    pub limits: Limits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    User,
    Reset,
    Deadline,
    Shutdown,
    TransportLoss,
    TerminalProposal,
    HostTerminal,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransportCause {
    ExpectedEof,
    Error(WireError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientNotice {
    RuntimeReady(RuntimeInfo),
    RequestFinished { request_id: RequestId, scope: Scope, result: Result<RuntimeReply, WireError> },
    Event { scope: Scope, event: RuntimeEvent },
    TurnFinishedProposal { scope: TurnScope, proposal: TurnFinished },
    TurnClosed { scope: TurnScope, cause: CloseReason },
    TransportClosed { cause: TransportCause, stderr_tail: Vec<String> },
}

/// Immutable association; only the machine can mint a callback token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackToken {
    generation: Id,
    instance: Id,
    request_id: RequestId,
    method: &'static str,
    scope: Scope,
    // Last: every header field is dropped before its final ownership guard.
    credit: Option<InboundCredit>,
}

#[derive(Clone, Debug)]
pub struct Cancellation(Arc<AtomicBool>, Arc<OnceLock<Vec<Arc<AtomicBool>>>>);
impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) || self.1.get().is_some_and(|parents| parents.iter().any(|p| p.load(Ordering::Acquire)))
    }
    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct CallbackCall {
    pub scope: Scope,
    pub call: HostCall,
    pub cancellation: Cancellation,
    pub deadline: Instant,
    pub limits: Limits,
    // Named fields/types unchanged; no Drop implementation restricts owned moves.
    // Payload and all other fields are destroyed BEFORE this final guard.
    pub token: CallbackToken,
}

#[derive(Debug)]
pub enum MachineAction {
    Send { ticket: u64, bytes: Vec<u8>, lane: PipeLane },
    Dispatch(Box<CallbackCall>),
    Notice { ticket: u64, notice: Box<ClientNotice> },
    Rejected { request_id: Option<RequestId>, error: WireError },
    Coalesced { request_id: RequestId, existing_request_id: RequestId },
    CloseTurn { scope: TurnScope, reason: CloseReason },
    RetireSession(Id),
    StopTree,
}

pub enum MachineInput {
    Services { generation: Id, services: ServiceCapabilities },
    Submit { generation: Id, request_id: RequestId, scope: Scope, request: HostRequest, deadline: Instant },
    Emit { generation: Id, scope: Scope, event: HostEvent, cause: Option<RequestId> },
    Peer { generation: Id, sender: Sender, decoded: DecodeResult },
    CallbackFinished { token: CallbackToken, result: Box<Result<HostReply, WireError>> },
    Progress { token: CallbackToken, event: CallProgress },
    Written { generation: Id, ticket: u64 },
    NoticeConsumed { generation: Id, ticket: u64 },
    InputFailed { generation: Id },
    WriteFailed { generation: Id },
    Eof { generation: Id, stderr_tail: Vec<String> },
    Tick,
    CompleteTurn { generation: Id, scope: TurnScope, terminal: TurnTerminal },
    Shutdown { generation: Id },
    IoReleased { generation: Id },
    CancelQueued { generation: Id, request_id: RequestId, scope: Scope, request: HostRequest, deadline: Instant, reason: CloseReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Starting,
    Ready,
    Stopping,
    Stopped,
    TransportError,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Pending,
    Completed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnPhase {
    Admitted,
    Active,
    Running,
    Validating,
    Finishing,
}

#[derive(Clone)]
enum Operation {
    Hello { versions: Vec<String> },
    Session { workflow: Id },
    Turn,
    TurnCancel,
    Cancel(RequestId),
    Reset { next: Id, previous: Session },
    Shutdown,
    Callback { delivery: Delivery },
}

#[derive(Clone)]
struct Session {
    id: Id,
    workflow: Id,
    // Bridge configuration is passed through session.start's wire frame, not
    // consulted by transport policy. Never clone it into reset/ID tombstones.
}

struct Entry {
    id: RequestId,
    scope: Scope,
    method: &'static str,
    operation: Operation,
    status: Status,
    control: bool,
    deadline: Instant,
    held_bytes: usize,
    owns_slot: bool,
    reply_ticket: Option<u64>,
    cancellation: Option<Cancellation>,
    // Logical retirement cannot release the physically owned action/charge.
    running: bool,
}
struct Turn {
    scope: TurnScope,
    binding: TurnBinding,
    phase: TurnPhase,
    deadline: Instant,
    // A proposal closes admission, not the host's still-pending decision window.
    closed: bool,
    interruption: Option<CloseReason>,
    proposal: Option<TurnFinished>,
}
struct Lease {
    ticket: u64,
    bytes: usize,
    control: bool,
    completion: bool,
    request_control: bool,
}

/// One finite ledger across both directions. Completed identities never leave it.
pub struct RuntimeMachine {
    generation: Id,
    limits: Limits,
    services: ServiceCapabilities,
    state: State,
    info: Option<RuntimeInfo>,
    info_bytes: usize,
    session: Option<Session>,
    retired_sessions: Vec<Id>,
    turn: Option<Turn>,
    retired_turns: Vec<TurnScope>,
    ledger: Vec<Entry>,
    outgoing: Vec<Lease>,
    notices: Vec<Lease>,
    next_ticket: u64,
    host_sequence: u32,
    peer_sequence: u32,
    startup_deadline: Instant,
    stop_deadline: Option<Instant>,
    expected_eof: bool,
    tree_stop_ordered: bool,
    rotate: bool,
    closure_reserve: usize,
    transport_base: usize,
}

fn error(code: ErrorCode) -> WireError {
    WireError {
        code,
        message: match code {
            ErrorCode::ResourceExhausted => "Runtime admission is full.",
            ErrorCode::UnsupportedCapability => "Runtime capability is unavailable.",
            ErrorCode::InvalidRequest => "Invalid runtime request state.",
            ErrorCode::Cancelled => "Runtime request was cancelled.",
            ErrorCode::Timeout => "Runtime request deadline expired.",
            _ => "Runtime transport closed.",
        }
        .into(),
        retry: Retry::Never,
        details: ErrorDetails::Empty(EmptyDetails {}),
    }
}

impl RuntimeMachine {
    pub fn new(generation: Id, limits: Limits, now: Instant) -> Result<Self, ClientError> {
        validate_limits(&limits).map_err(|_| ClientError { code: ErrorCode::InvalidRequest })?;
        let startup_deadline =
            now.checked_add(Duration::from_secs(u64::from(limits.handshake_seconds))).ok_or(ClientError { code: ErrorCode::InvalidRequest })?;
        // Storage accounting only, not another wire envelope or a negotiated floor.
        // Reserve the bounded local terminal metadata; diagnostic text uses only
        // the capacity actually left when the transport closes.
        let max_id = "x".repeat(128);
        let closure_reserve =
            serde_json::to_vec(&serde_json::json!({"scope":{"sessionId":max_id,"turnId":max_id,"workflowRunId":max_id},"cause":"TerminalProposal"}))
                .map_err(|_| ClientError { code: ErrorCode::InternalError })?
                .len();
        let transport_base = serde_json::to_vec(&serde_json::json!({"cause":error(ErrorCode::TransportError),"stderrTail":[]}))
            .map_err(|_| ClientError { code: ErrorCode::InternalError })?
            .len();
        Ok(Self {
            generation,
            limits,
            services: ServiceCapabilities::default(),
            state: State::Starting,
            info: None,
            info_bytes: 0,
            session: None,
            retired_sessions: Vec::new(),
            turn: None,
            retired_turns: Vec::new(),
            ledger: Vec::new(),
            outgoing: Vec::new(),
            notices: Vec::new(),
            next_ticket: 1,
            host_sequence: 0,
            peer_sequence: 0,
            startup_deadline,
            stop_deadline: None,
            expected_eof: false,
            tree_stop_ordered: false,
            rotate: false,
            closure_reserve,
            transport_base,
        })
    }

    pub fn apply(&mut self, input: MachineInput, now: Instant) -> Vec<MachineAction> {
        // Trusted process generation is checked BEFORE inspecting peer counters,
        // IDs or leases. Old process input cannot consume a new generation's ID.
        let generation = match &input {
            MachineInput::Services { generation, .. }
            | MachineInput::Submit { generation, .. }
            | MachineInput::Emit { generation, .. }
            | MachineInput::Peer { generation, .. }
            | MachineInput::Written { generation, .. }
            | MachineInput::NoticeConsumed { generation, .. }
            | MachineInput::InputFailed { generation }
            | MachineInput::WriteFailed { generation }
            | MachineInput::Eof { generation, .. }
            | MachineInput::CompleteTurn { generation, .. }
            | MachineInput::Shutdown { generation }
            | MachineInput::IoReleased { generation } => Some(generation),
            MachineInput::CancelQueued { generation, .. } => Some(generation),
            MachineInput::CallbackFinished { token, .. } | MachineInput::Progress { token, .. } => Some(&token.generation),
            MachineInput::Tick => None,
        };
        if generation.is_some_and(|g| g != &self.generation) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        // Expiry wins at the exact boundary, including a reply arriving without
        // an intervening timer tick. Stale generations above cannot tick us.
        if !matches!(input, MachineInput::Tick | MachineInput::Written { .. } | MachineInput::NoticeConsumed { .. } | MachineInput::IoReleased { .. })
        {
            self.tick(now, &mut actions);
        }
        match input {
            MachineInput::Written { ticket, .. } => {
                self.outgoing.retain(|l| l.ticket != ticket);
                for e in &mut self.ledger {
                    if e.reply_ticket == Some(ticket) {
                        e.reply_ticket = None;
                    }
                }
            }
            MachineInput::IoReleased { .. } => {
                self.outgoing.clear();
                for entry in &mut self.ledger {
                    entry.reply_ticket = None;
                }
            }
            MachineInput::CancelQueued { request_id, scope, request, deadline, reason, .. } => {
                if self.retired_scope(&scope) {
                    self.reject(
                        Some(request_id),
                        if reason == CloseReason::Deadline { ErrorCode::Timeout } else { ErrorCode::Cancelled },
                        &mut actions,
                    );
                } else {
                    let turn = matches!(request, HostRequest::TurnStart(_));
                    let mut staged = Vec::new();
                    self.submit(request_id.clone(), scope, request, deadline, now, &mut staged);
                    // This owned frame NEVER entered Pipes. Destroy it before
                    // reclaiming its exact unsent lease; this is NOT Written.
                    for action in staged {
                        if let MachineAction::Send { ticket, bytes, .. } = action {
                            drop(bytes);
                            self.outgoing.retain(|l| l.ticket != ticket);
                        } else {
                            actions.push(action);
                        }
                    }
                    if turn {
                        self.close_turn(reason, &mut actions);
                    }
                    if let Some(n) = self.index(&request_id) {
                        if reason == CloseReason::Deadline {
                            self.expire_host(n, ErrorCode::Timeout, &mut actions);
                        } else {
                            self.cancel_host(n, &mut actions);
                        }
                    }
                }
            }
            MachineInput::NoticeConsumed { ticket, .. } => self.notices.retain(|l| l.ticket != ticket),
            MachineInput::Services { services, .. } if self.state == State::Starting && self.ledger.is_empty() => self.services = services,
            MachineInput::Services { .. } => self.reject(None, ErrorCode::InvalidRequest, &mut actions),
            MachineInput::Submit { request_id, scope, request, deadline, .. } => self.submit(request_id, scope, request, deadline, now, &mut actions),
            MachineInput::Emit { scope, event, cause, .. } => self.emit(scope, event, cause, now, &mut actions),
            MachineInput::Peer { sender, decoded, .. } => self.peer(sender, decoded, now, &mut actions),
            MachineInput::CallbackFinished { token, result } => self.finish_callback(token, *result, now, &mut actions),
            MachineInput::Progress { token, event } => self.progress(token, event, now, &mut actions),
            MachineInput::InputFailed { .. } | MachineInput::WriteFailed { .. } => self.fail(Vec::new(), &mut actions),
            MachineInput::Eof { stderr_tail, .. } => self.eof(stderr_tail, &mut actions),
            MachineInput::Tick => self.tick(now, &mut actions),
            MachineInput::CompleteTurn { scope, terminal, .. } => {
                // The trusted host owns its outcome. This is neither wire
                // confirmation nor CAD/report validation in the transport.
                let _ = terminal;
                if self.turn.as_ref().is_some_and(|t| t.scope == scope && t.phase == TurnPhase::Finishing) {
                    self.retired_turns.push(scope);
                    self.turn = None;
                } else {
                    self.reject(None, ErrorCode::InvalidRequest, &mut actions);
                }
            }
            MachineInput::Shutdown { .. } => self.stop(now, &mut actions),
        }
        if self.rotate
            && self.turn.is_none()
            && self.outgoing.is_empty()
            && !self.ledger.iter().any(|e| e.status == Status::Pending || e.held_bytes > 0 || e.reply_ticket.is_some())
        {
            self.stop(now, &mut actions);
        }
        actions
    }

    fn reject(&self, request_id: Option<RequestId>, code: ErrorCode, actions: &mut Vec<MachineAction>) {
        actions.push(MachineAction::Rejected { request_id, error: error(code) });
    }
    fn live(&self) -> bool {
        matches!(self.state, State::Starting | State::Ready)
    }
    fn index(&self, id: &RequestId) -> Option<usize> {
        self.ledger.iter().position(|e| &e.id == id)
    }
    fn reserve(&self) -> usize {
        2 * (self.limits.pending_requests + self.limits.control_slots) as usize + 8
    }
    fn ordinary_available(&mut self) -> bool {
        if self.rotate || self.ledger.len() >= self.limits.request_ledger_entries as usize - self.reserve() {
            self.rotate = true;
            false
        } else {
            true
        }
    }
    fn pending_available(&self, host: bool, control: bool) -> bool {
        let pending = self
            .ledger
            .iter()
            .filter(|e| {
                e.owns_slot
                    && (e.status == Status::Pending || (!host && (e.running || e.reply_ticket.is_some())))
                    && e.id.as_str().starts_with(if host { "h:" } else { "r:" })
                    && e.control == control
            })
            .count();
        let completions = if host { self.notices.iter().filter(|l| l.completion && l.request_control == control).count() } else { 0 };
        pending + completions < if control { self.limits.control_slots } else { self.limits.pending_requests } as usize
    }
    fn incoming_bytes(&self) -> usize {
        self.ledger.iter().map(|e| e.held_bytes).sum::<usize>() + self.notices.iter().map(|l| l.bytes).sum::<usize>()
    }
    fn incoming_fits(&self, bytes: usize) -> bool {
        let data = self.ledger.iter().filter(|e| !e.control).map(|e| e.held_bytes).sum::<usize>()
            + self.notices.iter().filter(|l| !l.control).map(|l| l.bytes).sum::<usize>();
        data.checked_add(bytes).is_some_and(|n| n <= (self.limits.queue_bytes - self.limits.control_lane_bytes) as usize) && self.control_fits(bytes)
    }
    fn completion_reservation(&self, id: &RequestId, scope: &Scope) -> Option<usize> {
        let mut maximum = 0;
        for code in [ErrorCode::Cancelled, ErrorCode::Timeout, ErrorCode::TransportError, ErrorCode::InternalError] {
            let frame = Frame::Response {
                header: ResponseHeader { protocol_version: "1.0".into(), request_id: id.clone(), scope: scope.clone(), extensions: None },
                body: ResponseBody::Error(error(code)),
            };
            maximum = maximum.max(encode_frame(&frame, &self.limits).ok()?.len());
        }
        Some(maximum)
    }
    fn control_fits(&self, bytes: usize) -> bool {
        // controlLaneBytes is reserved capacity, not a second private maximum:
        // control completions can borrow unused data capacity. Data remains
        // capped separately; all bytes plus unspent terminal cells fit queueBytes.
        let terminal = self.notices.iter().filter(|l| l.control && !l.completion).map(|l| l.bytes).sum::<usize>();
        let unspent = (self.closure_reserve + self.transport_base).saturating_sub(terminal);
        self.incoming_bytes().checked_add(bytes).and_then(|n| n.checked_add(unspent)).is_some_and(|n| n <= self.limits.queue_bytes as usize)
    }
    fn lease(&mut self) -> Option<u64> {
        let n = self.next_ticket;
        self.next_ticket = self.next_ticket.checked_add(1)?;
        Some(n)
    }

    fn send(&mut self, frame: Frame, lane: PipeLane, actions: &mut Vec<MachineAction>) -> bool {
        let control = lane == PipeLane::Control;
        let bytes = if control { encode_control_frame(&frame, &self.limits) } else { encode_frame(&frame, &self.limits) };
        let Ok(bytes) = bytes else {
            return false;
        };
        let total = self.outgoing.iter().map(|l| l.bytes).sum::<usize>();
        let data = self.outgoing.iter().filter(|l| !l.control).map(|l| l.bytes).sum::<usize>();
        let slots = self.outgoing.iter().filter(|l| l.control == control).count();
        if slots >= if control { self.limits.control_slots } else { self.limits.pending_requests } as usize
            || total + bytes.len() > self.limits.queue_bytes as usize
            || (!control && data + bytes.len() > (self.limits.queue_bytes - self.limits.control_lane_bytes) as usize)
        {
            return false;
        }
        let Some(ticket) = self.lease() else {
            return false;
        };
        self.outgoing.push(Lease { ticket, bytes: bytes.len(), control, completion: false, request_control: false });
        actions.push(MachineAction::Send { ticket, bytes, lane });
        true
    }

    fn notice(&mut self, notice: ClientNotice, bytes: usize, actions: &mut Vec<MachineAction>) -> bool {
        // Completion cells and a bounded terminal reserve are distinct from the
        // ordinary notice lane. The worker must ack consumption, not just dequeue.
        let completion = matches!(&notice, ClientNotice::RequestFinished { .. });
        let ordinary = self.notices.iter().filter(|l| !l.completion && !l.control).count();
        if self.notices.len() >= self.reserve() || (!completion && ordinary >= self.limits.pending_requests as usize) || !self.incoming_fits(bytes) {
            return false;
        }
        let Some(ticket) = self.lease() else {
            return false;
        };
        self.notices.push(Lease { ticket, bytes, control: false, completion, request_control: false });
        actions.push(MachineAction::Notice { ticket, notice: Box::new(notice) });
        true
    }

    fn control_completion(&mut self, notice: ClientNotice, bytes: usize, actions: &mut Vec<MachineAction>) -> bool {
        if self.notices.len() >= self.reserve() || !self.control_fits(bytes) {
            return false;
        }
        let Some(ticket) = self.lease() else {
            return false;
        };
        self.notices.push(Lease { ticket, bytes, control: true, completion: true, request_control: true });
        actions.push(MachineAction::Notice { ticket, notice: Box::new(notice) });
        true
    }

    fn terminal_notice(&mut self, notice: ClientNotice, bytes: usize, actions: &mut Vec<MachineAction>) {
        // Two lifetime completion cells (TurnClosed, TransportClosed) use the
        // reserved lane, not the full ordinary notice queue. Neither is replaced.
        // Tickets cannot exhaust: ledger IDs <=4096 and both event counters <=MAX_SEQUENCE.
        if let Some(ticket) = self.lease() {
            self.notices.push(Lease { ticket, bytes, control: true, completion: false, request_control: false });
            actions.push(MachineAction::Notice { ticket, notice: Box::new(notice) });
        }
    }

    fn submit(&mut self, id: RequestId, scope: Scope, request: HostRequest, deadline: Instant, now: Instant, actions: &mut Vec<MachineAction>) {
        if !self.live() && !(self.state == State::Stopping && matches!(&request, HostRequest::RuntimeShutdown(_))) {
            self.reject(Some(id), ErrorCode::TransportError, actions);
            return;
        }
        if RequestId::parse(id.as_str(), Sender::Host).is_err() || self.index(&id).is_some() {
            self.reject(Some(id), ErrorCode::InvalidRequest, actions);
            return;
        }
        let method = request.method();
        let control = matches!(request, HostRequest::TurnCancel(_) | HostRequest::RequestCancel(_) | HostRequest::RuntimeShutdown(_));
        let valid = match &request {
            HostRequest::RuntimeHello(p) => {
                self.state == State::Starting
                    && self.info.is_none()
                    && self.ledger.is_empty()
                    && scope == Scope::Runtime
                    && p.limits == self.limits
                    && p.supported_versions.iter().any(|v| v == "1.0")
            }
            HostRequest::SessionStart(p) => {
                self.state == State::Ready
                    && self.session.is_none()
                    && !self.ledger.iter().any(|e| e.status == Status::Pending && matches!(e.method, "session.start" | "session.reset"))
                    && matches!(&scope, Scope::Session(s) if !self.retired_sessions.contains(s))
                    && self.info.as_ref().is_some_and(|i| i.capabilities.workflow_ids.contains(&p.workflow_id))
            }
            HostRequest::TurnStart(p) => {
                self.state == State::Ready
                    && self.turn.is_none()
                    && matches!(&scope,Scope::Turn(s) if self.session.as_ref().is_some_and(|session|session.id==s.session_id && session.workflow==p.binding.workflow_id) && !self.retired_turns.contains(s))
            }
            HostRequest::TurnCancel(_) => self.turn.as_ref().is_some_and(|t| scope == Scope::Turn(t.scope.clone())),
            HostRequest::RequestCancel(p) => self
                .index(&p.target_request_id)
                .is_some_and(|n| self.ledger.get(n).is_some_and(|e| e.id.as_str().starts_with("h:") && e.scope == scope)),
            HostRequest::SessionReset(p) => {
                self.state == State::Ready
                    && self.session.as_ref().is_some_and(|s| scope == Scope::Session(s.id.clone()) && p.next_session_id != s.id)
                    && !self.retired_sessions.contains(&p.next_session_id)
                    && !self.ledger.iter().any(|e| e.status == Status::Pending && e.method == "session.reset")
            }
            HostRequest::RuntimeShutdown(_) => scope == Scope::Runtime,
        };
        if !valid {
            self.reject(Some(id), ErrorCode::InvalidRequest, actions);
            return;
        }
        if control
            && let Some(existing) = self.ledger.iter().find(|e| {
                e.method == method
                    && match (&e.operation, &request) {
                        (Operation::Cancel(a), HostRequest::RequestCancel(b)) => a == &b.target_request_id,
                        (_, HostRequest::TurnCancel(_) | HostRequest::RuntimeShutdown(_)) => e.scope == scope,
                        _ => false,
                    }
            })
        {
            actions.push(MachineAction::Coalesced { request_id: id, existing_request_id: existing.id.clone() });
            return;
        }
        if matches!(&request, HostRequest::TurnStart(_)) && self.notices.iter().any(|l| l.control && !l.completion) {
            self.reject(Some(id), ErrorCode::ResourceExhausted, actions);
            return;
        }
        let Some(completion_bytes) = self.completion_reservation(&id, &scope) else {
            self.reject(Some(id), ErrorCode::InvalidRequest, actions);
            return;
        };
        let cell_fits = if control { self.control_fits(completion_bytes) } else { self.incoming_fits(completion_bytes) };
        if (!control && !self.ordinary_available())
            || !self.pending_available(true, control)
            || self.ledger.len() >= self.limits.request_ledger_entries as usize
            || !cell_fits
        {
            self.reject(Some(id), ErrorCode::ResourceExhausted, actions);
            return;
        }
        let operation = match &request {
            HostRequest::RuntimeHello(p) => Operation::Hello { versions: p.supported_versions.clone() },
            HostRequest::SessionStart(p) => Operation::Session { workflow: p.workflow_id.clone() },
            HostRequest::TurnStart(_) => Operation::Turn,
            HostRequest::TurnCancel(_) => Operation::TurnCancel,
            HostRequest::RequestCancel(p) => Operation::Cancel(p.target_request_id.clone()),
            HostRequest::SessionReset(p) => {
                let Some(previous) = self.session.clone() else {
                    self.reject(Some(id), ErrorCode::InvalidRequest, actions);
                    return;
                };
                Operation::Reset { next: p.next_session_id.clone(), previous }
            }
            HostRequest::RuntimeShutdown(_) => Operation::Shutdown,
        };
        let frame = Frame::Request {
            header: RequestHeader {
                protocol_version: "1.0".into(),
                request_id: id.clone(),
                scope: scope.clone(),
                method: method.into(),
                extensions: None,
            },
            body: RequestBody::Host(request.clone()),
        };
        // Reserve the physical frame before admitting control, but expose it only
        // after synchronous retirement and the trusted closure actions.
        let mut sends = Vec::new();
        if !self.send(frame, if control { PipeLane::Control } else { PipeLane::Data }, &mut sends) {
            self.reject(Some(id), ErrorCode::ResourceExhausted, actions);
            return;
        }
        let cap = match request {
            HostRequest::RuntimeHello(_) => self.limits.handshake_seconds,
            HostRequest::SessionStart(_) => self.limits.session_start_seconds,
            HostRequest::TurnCancel(_) | HostRequest::RequestCancel(_) => self.limits.cancel_ack_seconds,
            HostRequest::RuntimeShutdown(_) => self.limits.shutdown_seconds,
            _ => self.limits.call_max_seconds,
        };
        let deadline = deadline.min(now.checked_add(Duration::from_secs(u64::from(cap))).unwrap_or(now));
        self.ledger.push(Entry {
            id: id.clone(),
            scope: scope.clone(),
            method,
            operation,
            status: Status::Pending,
            control,
            deadline,
            held_bytes: completion_bytes,
            owns_slot: true,
            reply_ticket: None,
            cancellation: None,
            running: false,
        });
        match request {
            HostRequest::TurnStart(p) => {
                if let Scope::Turn(scope) = scope {
                    let deadline = now.checked_add(Duration::from_secs(u64::from(p.budgets.wall_time_seconds))).unwrap_or(now);
                    self.turn = Some(Turn {
                        scope,
                        binding: p.binding,
                        phase: TurnPhase::Admitted,
                        deadline,
                        closed: false,
                        interruption: None,
                        proposal: None,
                    });
                }
            }
            HostRequest::RequestCancel(p) => {
                if let Some(n) = self.index(&p.target_request_id) {
                    self.cancel_host(n, actions);
                }
            }
            HostRequest::TurnCancel(p) => self.close_turn(
                match p.reason {
                    CancelReason::User => CloseReason::User,
                    CancelReason::Reset => CloseReason::Reset,
                    CancelReason::Deadline => CloseReason::Deadline,
                    CancelReason::Shutdown => CloseReason::Shutdown,
                },
                actions,
            ),
            HostRequest::SessionReset(_) => {
                self.close_turn(CloseReason::Reset, actions);
                self.retire_session(actions);
            }
            HostRequest::RuntimeShutdown(_) => {
                self.close_turn(CloseReason::Shutdown, actions);
                self.retire_session(actions);
                if self.live() {
                    self.state = State::Stopping;
                    if self.stop_deadline.is_none() {
                        self.stop_deadline = now.checked_add(Duration::from_secs(u64::from(self.limits.shutdown_seconds)));
                    }
                }
            }
            _ => {}
        }
        if self.live() || self.state == State::Stopping {
            actions.extend(sends);
        }
    }

    fn peer(&mut self, sender: Sender, decoded: DecodeResult, now: Instant, actions: &mut Vec<MachineAction>) {
        if matches!(self.state, State::Stopped | State::TransportError) {
            return;
        }
        if sender != Sender::Runtime {
            self.fail(Vec::new(), actions);
            return;
        }
        match decoded {
            DecodeResult::RequestError { header, error } => {
                if header.protocol_version != "1.0"
                    || RequestId::parse(header.request_id.as_str(), Sender::Runtime).is_err()
                    || matches!(
                        header.method.as_str(),
                        "runtime.hello" | "session.start" | "turn.start" | "turn.cancel" | "session.reset" | "runtime.shutdown"
                    )
                    || (!self.current_scope(&header.scope)
                        && !self.retired_scope(&header.scope)
                        && !(header.scope == Scope::Runtime && error.code == ErrorCode::MethodNotFound))
                {
                    self.fail(Vec::new(), actions);
                    return;
                }
                self.incoming_request(header, None, Some(error), 0, now, actions);
            }
            DecodeResult::Frame(frame) => {
                // Even constructed DTO inputs cross the same direction/envelope
                // boundary as actual bytes before correlation or effects.
                let Ok(bytes) = encode_frame(&frame, &self.limits) else {
                    self.fail(Vec::new(), actions);
                    return;
                };
                let phase = if self.info.is_some() { DecodePhase::Negotiated } else { DecodePhase::Bootstrap };
                if decode_frame(&bytes, phase, sender, &self.limits).is_err() {
                    self.fail(Vec::new(), actions);
                    return;
                }
                match frame {
                    Frame::Response { header, body } => self.response(header, body, bytes.len(), now, actions),
                    Frame::Request { header, body } => self.incoming_request(header, Some(body), None, bytes.len(), now, actions),
                    Frame::Event { header, body } => self.peer_event(header, body, bytes.len(), now, actions),
                }
            }
        }
    }

    fn current_scope(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Runtime => false,
            Scope::Session(s) => self.session.as_ref().is_some_and(|session| &session.id == s),
            Scope::Turn(s) => self.turn.as_ref().is_some_and(|t| &t.scope == s && t.phase != TurnPhase::Finishing),
        }
    }
    fn retired_scope(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Session(s) => self.retired_sessions.contains(s),
            Scope::Turn(s) => {
                self.retired_sessions.contains(&s.session_id)
                    || self.retired_turns.contains(s)
                    || self.turn.as_ref().is_some_and(|t| &t.scope == s && t.phase == TurnPhase::Finishing)
            }
            Scope::Runtime => false,
        }
    }

    fn incoming_request(
        &mut self,
        header: RequestHeader,
        body: Option<RequestBody>,
        request_error: Option<WireError>,
        bytes: usize,
        now: Instant,
        actions: &mut Vec<MachineAction>,
    ) {
        if self.index(&header.request_id).is_some() || !header.request_id.as_str().starts_with("r:") || header.protocol_version != "1.0" {
            self.fail(Vec::new(), actions);
            return;
        }
        let control = matches!(&body, Some(RequestBody::Cancel(_))) || (request_error.is_some() && header.method == "request.cancel");
        let unknown_runtime = header.scope == Scope::Runtime && request_error.as_ref().is_some_and(|e| e.code == ErrorCode::MethodNotFound);
        if self.state != State::Ready || (!self.current_scope(&header.scope) && !self.retired_scope(&header.scope) && !control && !unknown_runtime) {
            self.fail(Vec::new(), actions);
            return;
        }
        if self.ledger.len() >= self.limits.request_ledger_entries as usize {
            self.fail(Vec::new(), actions);
            return;
        }
        let method = match &body {
            Some(RequestBody::Runtime(call)) => call.method(),
            Some(RequestBody::Cancel(_)) => "request.cancel",
            _ if control => "request.cancel",
            _ => "unknown",
        };
        let bytes_fit = if control { bytes <= CONTROL_FRAME_BYTES && self.control_fits(bytes) } else { self.incoming_fits(bytes) };
        let coalesced = control
            && !self.pending_available(false, true)
            && matches!(&body, Some(RequestBody::Cancel(p)) if self.index(&p.target_request_id).is_some_and(|n|self.ledger.get(n).is_some_and(|e|e.scope==header.scope && e.status!=Status::Pending)));
        let admitted = (control || self.ordinary_available()) && (self.pending_available(false, control) || coalesced) && bytes_fit;
        let deadline = now.checked_add(Duration::from_secs(u64::from(self.limits.call_default_seconds))).unwrap_or(now);
        let cancellation = Cancellation(Arc::new(AtomicBool::new(false)), Arc::new(OnceLock::new()));
        let delivery = match &body {
            Some(RequestBody::Runtime(HostCall::ModelGenerate(p))) => p.options.delivery,
            _ => Delivery::Buffered,
        };
        self.ledger.push(Entry {
            id: header.request_id.clone(),
            scope: header.scope.clone(),
            method,
            operation: Operation::Callback { delivery },
            status: Status::Pending,
            control,
            deadline,
            held_bytes: if admitted { bytes } else { 0 },
            owns_slot: admitted && !coalesced,
            reply_ticket: None,
            cancellation: Some(cancellation.clone()),
            running: false,
        });
        let n = self.ledger.len() - 1;
        if !admitted {
            self.respond(n, Err(error(ErrorCode::ResourceExhausted)), actions);
            return;
        }
        if let Some(error) = request_error {
            self.respond(n, Err(error), actions);
            return;
        }
        if self.retired_scope(&header.scope) && !control {
            cancellation.cancel();
            self.respond(n, Err(error(ErrorCode::Cancelled)), actions);
            return;
        }
        match body {
            Some(RequestBody::Cancel(p)) => self.cancel_callback_request(n, p, actions),
            Some(RequestBody::Runtime(call)) => {
                let supported = match &call {
                    HostCall::ConnectionDescribe(_) => self.services.connection_describe,
                    HostCall::ModelGenerate(p) => {
                        self.services.model_generate
                            && self.info.as_ref().is_some_and(|i| i.capabilities.host_model_calls)
                            && (p.options.delivery == Delivery::Buffered || self.services.streaming)
                    }
                    HostCall::AgentRun(_) => self.services.agent_run && self.info.as_ref().is_some_and(|i| i.capabilities.host_agent_tasks),
                    _ => self.services.validation && self.info.as_ref().is_some_and(|i| i.capabilities.snapshot_validation),
                };
                if !supported {
                    self.respond(n, Err(error(ErrorCode::UnsupportedCapability)), actions);
                    return;
                }
                if !self.binding_matches(&header.scope, &call) {
                    self.respond(n, Err(error(ErrorCode::InvalidRequest)), actions);
                    return;
                }
                let timeout = match &call {
                    HostCall::ModelGenerate(p) => p.options.timeout_seconds,
                    HostCall::AgentRun(p) => p.timeout_seconds,
                    _ => self.limits.call_default_seconds,
                }
                .min(self.limits.call_max_seconds);
                let turn_deadline =
                    if matches!(&header.scope, Scope::Turn(_)) { self.turn.as_ref().map_or(deadline, |t| t.deadline) } else { deadline };
                let deadline = now.checked_add(Duration::from_secs(u64::from(timeout))).unwrap_or(now).min(turn_deadline);
                if deadline <= now {
                    self.respond(n, Err(error(ErrorCode::Timeout)), actions);
                    return;
                }
                let Some(info) = &self.info else {
                    self.fail(Vec::new(), actions);
                    return;
                };
                let token = CallbackToken {
                    generation: self.generation.clone(),
                    instance: info.runtime_instance_id.clone(),
                    request_id: header.request_id,
                    method,
                    scope: header.scope.clone(),
                    credit: None,
                };
                if let Some(e) = self.ledger.get_mut(n) {
                    e.deadline = deadline;
                    e.running = true;
                }
                actions.push(MachineAction::Dispatch(Box::new(CallbackCall {
                    token,
                    scope: header.scope,
                    call,
                    cancellation,
                    deadline,
                    limits: self.limits.clone(),
                })));
            }
            _ => self.fail(Vec::new(), actions),
        }
    }

    fn binding_matches(&self, scope: &Scope, call: &HostCall) -> bool {
        if matches!(call, HostCall::ValidationStatus(_) | HostCall::ValidationReport(_) | HostCall::ValidationCancel(_)) {
            // current_scope and capabilities were checked before this point;
            // the injected service owns authorization of the referenced run.
            return true;
        }
        if let HostCall::ConnectionDescribe(p) = call {
            return !matches!(scope, Scope::Turn(_))
                || self.turn.as_ref().is_some_and(|t| p.connection_id == t.binding.connection_id && p.model_id == t.binding.model_id);
        }
        let Some(t) = &self.turn else {
            return false;
        };
        if !matches!(scope, Scope::Turn(s) if s == &t.scope && t.phase != TurnPhase::Finishing) {
            return false;
        }
        match call {
            HostCall::ModelGenerate(p) => p.connection_id == t.binding.connection_id && p.model_id == t.binding.model_id,
            HostCall::AgentRun(p) => {
                p.connection_id == t.binding.connection_id && p.model_id == t.binding.model_id && p.context.document == t.binding.document
            }
            _ => true,
        }
    }

    fn respond(&mut self, n: usize, result: Result<ResponseBody, WireError>, actions: &mut Vec<MachineAction>) {
        let Some(e) = self.ledger.get_mut(n) else {
            self.fail(Vec::new(), actions);
            return;
        };
        let body = result.unwrap_or_else(ResponseBody::Error);
        let cancelled = matches!(&body,ResponseBody::Error(e) if matches!(e.code, ErrorCode::Cancelled | ErrorCode::Timeout));
        e.status = if cancelled { Status::Cancelled } else { Status::Completed };
        if !e.running {
            e.held_bytes = 0;
        }
        let frame = Frame::Response {
            header: ResponseHeader { protocol_version: "1.0".into(), request_id: e.id.clone(), scope: e.scope.clone(), extensions: None },
            body,
        };
        let lane = if e.control { PipeLane::Control } else { PipeLane::Data };
        // The reserved control slot may already own an idempotent cancel's
        // earlier acknowledgement. A separately bounded data frame can carry
        // the new identity's response, without releasing/overwriting that slot.
        // Validate the local control-frame cap before any data-lane fallback.
        if lane == PipeLane::Control && encode_control_frame(&frame, &self.limits).is_err() {
            self.fail(Vec::new(), actions);
            return;
        }
        let sent = if lane == PipeLane::Control {
            self.send(frame.clone(), lane, actions) || self.send(frame, PipeLane::Data, actions)
        } else {
            self.send(frame, lane, actions)
        };
        if sent {
            let ticket = self.outgoing.last().map(|l| l.ticket);
            if let Some(e) = self.ledger.get_mut(n) {
                e.reply_ticket = ticket;
            }
        } else {
            self.fail(Vec::new(), actions);
        }
    }

    fn callback_index(&self, token: &CallbackToken) -> Option<usize> {
        if token.generation != self.generation || !self.info.as_ref().is_some_and(|i| i.runtime_instance_id == token.instance) {
            return None;
        }
        self.index(&token.request_id).filter(|n| {
            self.ledger
                .get(*n)
                .is_some_and(|e| e.scope == token.scope && e.method == token.method && e.status == Status::Pending && self.current_scope(&e.scope))
        })
    }
    fn finish_callback(&mut self, token: CallbackToken, result: Result<HostReply, WireError>, now: Instant, actions: &mut Vec<MachineAction>) {
        // Physical completion releases only the action half, never a wire lease.
        if token.generation == self.generation
            && self.info.as_ref().is_some_and(|i| i.runtime_instance_id == token.instance)
            && let Some(e) = self.ledger.iter_mut().find(|e| e.id == token.request_id && e.scope == token.scope && e.method == token.method)
        {
            if !e.running {
                return;
            }
            e.running = false;
            e.held_bytes = 0;
        } else {
            return;
        }
        let Some(n) = self.callback_index(&token) else {
            return;
        };
        if self.ledger.get(n).is_some_and(|e| now >= e.deadline) {
            self.respond(n, Err(error(ErrorCode::Timeout)), actions);
            return;
        }
        let result = match result {
            Ok(reply) if reply.method() == token.method => ResponseBody::host_ok(reply, &self.limits),
            Ok(_) => Err(error(ErrorCode::InvalidRequest)),
            Err(e) => Ok(ResponseBody::Error(e)),
        };
        // This input acknowledges that the owned service action has ended.
        // A service's CANCELLED result is not a still-running peer cancellation.
        if let Some(e) = self.ledger.get_mut(n) {
            e.held_bytes = 0;
        }
        self.respond(n, result, actions);
    }

    fn cancel_callback_request(&mut self, n: usize, p: RequestCancelParams, actions: &mut Vec<MachineAction>) {
        let target = self.index(&p.target_request_id).filter(|target| {
            *target != n
                && self.ledger.get(*target).zip(self.ledger.get(n)).is_some_and(|(a, b)| a.id.as_str().starts_with("r:") && a.scope == b.scope)
        });
        let Some(target) = target else {
            self.respond(n, Err(error(ErrorCode::InvalidRequest)), actions);
            return;
        };
        if self.ledger.get(target).is_some_and(|e| e.status == Status::Pending) {
            if let Some(c) = self.ledger.get(target).and_then(|e| e.cancellation.as_ref()) {
                c.cancel();
            }
            self.respond(target, Err(error(ErrorCode::Cancelled)), actions);
            if !self.live() {
                // The target reply may have terminated the transport. Never
                // create a control acknowledgement after its terminal actions.
                return;
            }
        }
        let reply = ResponseBody::runtime_ok(RuntimeReply::RequestCancel(RequestCancelResult { state: CancellationState::Terminal }), &self.limits);
        self.respond(n, reply, actions);
    }

    fn response(&mut self, header: ResponseHeader, body: ResponseBody, bytes: usize, now: Instant, actions: &mut Vec<MachineAction>) {
        let Some(n) = self.index(&header.request_id) else {
            self.fail(Vec::new(), actions);
            return;
        };
        let Some(e) = self.ledger.get(n) else {
            self.fail(Vec::new(), actions);
            return;
        };
        if !e.id.as_str().starts_with("h:") || e.scope != header.scope || e.status == Status::Completed {
            self.fail(Vec::new(), actions);
            return;
        }
        let method = e.method;
        let operation = e.operation.clone();
        let cancelled = e.status == Status::Cancelled;
        let control = e.control;
        let reserved_bytes = e.held_bytes;
        let result = match body {
            ResponseBody::Ok(p) => match p.decode_runtime(method, &self.limits) {
                Ok(reply) => Ok(reply),
                Err(_) => {
                    self.fail(Vec::new(), actions);
                    return;
                }
            },
            ResponseBody::Error(e) => Err(e),
        };
        if cancelled {
            return;
        }
        if header.protocol_version != "1.0"
            && !matches!((&operation,&result),(Operation::Hello {..},Err(e)) if e.code==ErrorCode::IncompatibleVersion)
        {
            self.fail(Vec::new(), actions);
            return;
        }
        match (&operation, &result) {
            (Operation::Hello { versions }, Ok(RuntimeReply::RuntimeHello(p))) => {
                if p.selected_version != "1.0" || !versions.contains(&p.selected_version) {
                    self.fail(Vec::new(), actions);
                    return;
                }
                let Ok(limits) = negotiate_limits(&self.limits, &p.limits) else {
                    self.fail(Vec::new(), actions);
                    return;
                };
                if self.outgoing.iter().map(|l| l.bytes).sum::<usize>() > limits.queue_bytes as usize
                    || self.incoming_bytes() > limits.queue_bytes as usize
                {
                    self.fail(Vec::new(), actions);
                    return;
                }
                self.limits = limits.clone();
                self.info_bytes = bytes;
                self.info = Some(RuntimeInfo {
                    selected_version: p.selected_version.clone(),
                    runtime_instance_id: p.runtime_instance_id.clone(),
                    runtime_version: p.runtime_version.clone(),
                    capabilities: p.capabilities.clone(),
                    limits,
                });
            }
            (Operation::Hello { .. }, Err(e)) if e.code == ErrorCode::IncompatibleVersion => {
                self.state = State::Stopping;
                self.expected_eof = true;
                self.stop_deadline = now.checked_add(Duration::from_secs(u64::from(self.limits.shutdown_seconds)));
            }
            (Operation::Session { workflow }, Ok(RuntimeReply::SessionStart(p))) => {
                if &p.workflow_id != workflow {
                    self.fail(Vec::new(), actions);
                    return;
                }
                if let Scope::Session(id) = &header.scope {
                    self.session = Some(Session { id: id.clone(), workflow: workflow.clone() });
                }
            }
            (Operation::Turn, Ok(RuntimeReply::TurnStart(_))) => {
                if let Some(t) = &mut self.turn
                    && header.scope == Scope::Turn(t.scope.clone())
                    && t.phase == TurnPhase::Admitted
                {
                    t.phase = TurnPhase::Active;
                }
            }
            (Operation::Turn, Err(_)) => {
                if self.turn.as_ref().is_some_and(|t| header.scope == Scope::Turn(t.scope.clone())) {
                    // A refusal is not host terminal verification. Close further
                    // work and retain Finishing until matching trusted completion;
                    // close_turn preserves any earlier terminal proposal.
                    self.close_turn(CloseReason::TerminalProposal, actions);
                }
            }
            (Operation::Reset { next, previous }, Ok(RuntimeReply::SessionReset(p))) => {
                if next != &p.session_id {
                    self.fail(Vec::new(), actions);
                    return;
                }
                self.session = Some(Session { id: next.clone(), workflow: previous.workflow.clone() });
                if let Some(t) = self.turn.take() {
                    self.retired_turns.push(t.scope);
                }
            }
            (Operation::Shutdown, Ok(RuntimeReply::RuntimeShutdown(_))) => {
                self.expected_eof = true;
            }
            _ => {}
        }
        if let Some(e) = self.ledger.get_mut(n) {
            e.status = Status::Completed;
            e.held_bytes = 0;
        }
        let notice = ClientNotice::RequestFinished { request_id: header.request_id, scope: header.scope, result };
        let delivered = if control { self.control_completion(notice, bytes, actions) } else { self.notice(notice, bytes, actions) };
        if !delivered {
            if let Some(e) = self.ledger.get_mut(n) {
                e.status = Status::Pending;
                e.held_bytes = reserved_bytes;
            }
            self.fail(Vec::new(), actions);
        }
    }

    fn valid_cause(&self, scope: &Scope, cause: &RequiredNullable<Id>, host: bool) -> bool {
        let Some(id) = &cause.0 else {
            return true;
        };
        self.ledger.iter().any(|e| e.id.as_str() == id.as_str() && e.id.as_str().starts_with(if host { "h:" } else { "r:" }) && &e.scope == scope)
    }
    fn peer_event(&mut self, header: EventHeader, body: EventBody, bytes: usize, now: Instant, actions: &mut Vec<MachineAction>) {
        if header.sequence <= self.peer_sequence
            || header.sequence > MAX_SEQUENCE
            || header.protocol_version != "1.0"
            || !self.valid_cause(&header.scope, &header.request_id, true)
        {
            self.fail(Vec::new(), actions);
            return;
        }
        let EventBody::Runtime(event) = body else {
            self.fail(Vec::new(), actions);
            return;
        };
        // Validate sequence/cause BEFORE any retired-scope discard.
        self.peer_sequence = header.sequence;
        if self.retired_scope(&header.scope) {
            if header.sequence == MAX_SEQUENCE {
                self.stop(now, actions);
            }
            return;
        }
        match event {
            RuntimeEvent::RuntimeReady(p) => {
                if self.state != State::Starting
                    || header.scope != Scope::Runtime
                    || !self.info.as_ref().is_some_and(|i| i.runtime_instance_id == p.runtime_instance_id)
                    || header
                        .request_id
                        .0
                        .as_ref()
                        .is_some_and(|id| !self.ledger.iter().any(|e| e.id.as_str() == id.as_str() && e.method == "runtime.hello"))
                {
                    self.fail(Vec::new(), actions);
                    return;
                }
                self.state = State::Ready;
                if let Some(info) = self.info.clone()
                    && !self.notice(ClientNotice::RuntimeReady(info), bytes.max(self.info_bytes), actions)
                {
                    self.fail(Vec::new(), actions);
                }
            }
            RuntimeEvent::TurnFinished(proposal) => {
                let Scope::Turn(scope) = header.scope else {
                    self.fail(Vec::new(), actions);
                    return;
                };
                if !self.turn.as_ref().is_some_and(|t| t.scope == scope) {
                    self.fail(Vec::new(), actions);
                    return;
                }
                self.close_turn(CloseReason::TerminalProposal, actions);
                if let Some(t) = &mut self.turn {
                    t.proposal = Some(proposal.clone());
                }
                if !self.notice(ClientNotice::TurnFinishedProposal { scope, proposal }, bytes, actions) {
                    self.fail(Vec::new(), actions);
                }
            }
            event => {
                if self.state != State::Ready || !self.current_scope(&header.scope) {
                    self.fail(Vec::new(), actions);
                    return;
                }
                if let Some(t) = &mut self.turn {
                    t.phase = if matches!(event,RuntimeEvent::WorkflowStage(ref s) if s.stage_id.as_str()=="validating") {
                        TurnPhase::Validating
                    } else {
                        TurnPhase::Running
                    };
                }
                if !self.notice(ClientNotice::Event { scope: header.scope, event }, bytes, actions) {
                    self.fail(Vec::new(), actions);
                }
            }
        }
        if header.sequence == MAX_SEQUENCE {
            self.stop(now, actions);
        }
    }

    fn emit(&mut self, scope: Scope, event: HostEvent, cause: Option<RequestId>, now: Instant, actions: &mut Vec<MachineAction>) {
        if !self.live() || self.state != State::Ready || !self.current_scope(&scope) {
            self.reject(None, ErrorCode::InvalidRequest, actions);
            return;
        }
        if !matches!(&event, HostEvent::ConnectionState(_)) {
            self.reject(None, ErrorCode::InvalidRequest, actions);
            return;
        }
        if let HostEvent::ConnectionState(p) = &event
            && matches!(&scope, Scope::Turn(_))
            && !self.turn.as_ref().is_some_and(|t| t.binding.connection_id == p.connection_id)
        {
            self.reject(None, ErrorCode::InvalidRequest, actions);
            return;
        }
        let cause = cause.and_then(|id| Id::parse(id.as_str()).ok());
        if let Some(id) = &cause
            && !self.ledger.iter().any(|e| e.id.as_str() == id.as_str() && e.scope == scope)
        {
            self.reject(None, ErrorCode::InvalidRequest, actions);
            return;
        }
        self.send_event(scope, event, cause, now, actions);
    }
    fn progress(&mut self, token: CallbackToken, event: CallProgress, now: Instant, actions: &mut Vec<MachineAction>) {
        let Some(n) = self.callback_index(&token) else {
            self.reject(None, ErrorCode::InvalidRequest, actions);
            return;
        };
        let event = match event {
            CallProgress::AgentProgress(p) if token.method == "agent.run" => HostEvent::AgentProgress(p),
            CallProgress::ModelDelta(p)
                if token.method == "model.generate"
                    && self.services.streaming
                    && self.ledger.get(n).is_some_and(|e| matches!(e.operation, Operation::Callback { delivery: Delivery::Streaming })) =>
            {
                HostEvent::ModelDelta(p)
            }
            CallProgress::ModelDelta(_) => {
                self.reject(None, ErrorCode::UnsupportedCapability, actions);
                return;
            }
            _ => {
                self.reject(None, ErrorCode::InvalidRequest, actions);
                return;
            }
        };
        let cause = Id::parse(token.request_id.as_str()).ok();
        self.send_event(token.scope, event, cause, now, actions);
    }
    fn send_event(&mut self, scope: Scope, event: HostEvent, cause: Option<Id>, now: Instant, actions: &mut Vec<MachineAction>) {
        if self.host_sequence >= MAX_SEQUENCE {
            self.stop(now, actions);
            self.reject(None, ErrorCode::ResourceExhausted, actions);
            return;
        }
        let sequence = self.host_sequence + 1;
        let frame = Frame::Event {
            header: EventHeader { protocol_version: "1.0".into(), sequence, request_id: RequiredNullable(cause), scope, extensions: None },
            body: EventBody::Host(event),
        };
        if self.send(frame, PipeLane::Data, actions) {
            self.host_sequence = sequence;
            if sequence == MAX_SEQUENCE {
                self.stop(now, actions);
            }
        } else {
            self.reject(None, ErrorCode::ResourceExhausted, actions);
        }
    }

    fn close_turn(&mut self, reason: CloseReason, actions: &mut Vec<MachineAction>) {
        let Some(t) = &mut self.turn else {
            return;
        };
        if t.interruption.is_some() || (t.closed && reason == CloseReason::TerminalProposal) {
            return;
        }
        t.closed = true;
        if reason != CloseReason::TerminalProposal {
            t.interruption = Some(reason);
        }
        t.phase = TurnPhase::Finishing;
        let scope = t.scope.clone();
        for e in &mut self.ledger {
            if e.scope == Scope::Turn(scope.clone())
                && let Some(c) = &e.cancellation
                && reason != CloseReason::TerminalProposal
            {
                c.cancel();
            }
        }
        actions.push(MachineAction::CloseTurn { scope: scope.clone(), reason });
        if reason != CloseReason::TerminalProposal {
            self.terminal_notice(ClientNotice::TurnClosed { scope: scope.clone(), cause: reason }, self.closure_reserve, actions);
        }
        // A proposal retires publication, not host verification. Interrupted
        // callbacks get exactly one terminal reply while the pipe survives;
        // their physical action charge remains until actual CallbackFinished.
        for n in 0..self.ledger.len() {
            let Some(e) = self.ledger.get(n) else { continue };
            if e.scope != Scope::Turn(scope.clone()) || e.status != Status::Pending {
                continue;
            }
            if e.id.as_str().starts_with("r:") {
                if reason == CloseReason::TerminalProposal || !self.live() {
                    if let Some(e) = self.ledger.get_mut(n) {
                        e.status = Status::Cancelled;
                    }
                } else {
                    self.respond(n, Err(error(if reason == CloseReason::Deadline { ErrorCode::Timeout } else { ErrorCode::Cancelled })), actions);
                }
            }
        }
    }
    fn retire_session(&mut self, actions: &mut Vec<MachineAction>) {
        let Some(session) = self.session.take() else { return };
        let id = session.id;
        self.retired_sessions.push(id.clone());
        // All trusted closure actions precede notices, wire output and cleanup.
        // No hook runs here; the worker must honor this ordering outside locks.
        let at =
            actions.iter().position(|a| !matches!(a, MachineAction::CloseTurn { .. } | MachineAction::RetireSession(_))).unwrap_or(actions.len());
        actions.insert(at, MachineAction::RetireSession(id.clone()));
        for n in 0..self.ledger.len() {
            let Some(e) = self.ledger.get(n) else { continue };
            let belongs = match &e.scope {
                Scope::Session(s) => s == &id,
                Scope::Turn(t) => t.session_id == id,
                Scope::Runtime => false,
            };
            if !belongs {
                continue;
            }
            if let Some(c) = &e.cancellation {
                c.cancel();
            }
            if e.status != Status::Pending || matches!(e.operation, Operation::Reset { .. }) {
                continue;
            }
            if self.live() {
                if e.id.as_str().starts_with("h:") {
                    self.cancel_host(n, actions);
                } else {
                    self.respond(n, Err(error(ErrorCode::Cancelled)), actions);
                }
            }
        }
    }
    fn cancel_host(&mut self, n: usize, actions: &mut Vec<MachineAction>) {
        self.expire_host(n, ErrorCode::Cancelled, actions);
    }
    fn expire_host(&mut self, n: usize, code: ErrorCode, actions: &mut Vec<MachineAction>) {
        let Some(e) = self.ledger.get_mut(n) else {
            return;
        };
        if e.status != Status::Pending {
            return;
        }
        e.status = Status::Cancelled;
        let reserved_bytes = e.held_bytes;
        let control = e.control;
        e.held_bytes = 0;
        let notice = ClientNotice::RequestFinished { request_id: e.id.clone(), scope: e.scope.clone(), result: Err(error(code)) };
        if !(if control { self.control_completion(notice, reserved_bytes, actions) } else { self.notice(notice, reserved_bytes, actions) }) {
            self.fail(Vec::new(), actions);
        }
    }
    fn tick(&mut self, now: Instant, actions: &mut Vec<MachineAction>) {
        if matches!(self.state, State::Stopped | State::TransportError) {
            return;
        }
        if self.turn.as_ref().is_some_and(|t| t.interruption.is_none() && now >= t.deadline) {
            self.close_turn(CloseReason::Deadline, actions);
        }
        for n in 0..self.ledger.len() {
            let Some(e) = self.ledger.get(n) else { continue };
            if e.status != Status::Pending || now < e.deadline {
                continue;
            }
            let startup = matches!(e.operation, Operation::Hello { .. } | Operation::Session { .. });
            let cancel = matches!(e.operation, Operation::TurnCancel | Operation::Cancel(_));
            if let Some(c) = &e.cancellation {
                c.cancel();
            }
            if e.id.as_str().starts_with("h:") {
                self.expire_host(n, ErrorCode::Timeout, actions);
            } else {
                self.respond(n, Err(error(ErrorCode::Timeout)), actions);
            }
            if startup {
                self.fail(Vec::new(), actions);
                return;
            }
            if cancel {
                self.stop(now, actions);
            }
        }
        if self.state == State::Starting && now >= self.startup_deadline {
            self.fail(Vec::new(), actions);
            return;
        }
        if self.stop_deadline.is_some_and(|d| now >= d) {
            self.fail(Vec::new(), actions);
        }
    }
    fn stop(&mut self, now: Instant, actions: &mut Vec<MachineAction>) {
        if matches!(self.state, State::Stopped | State::TransportError) {
            return;
        }
        self.close_turn(CloseReason::Shutdown, actions);
        self.retire_session(actions);
        if matches!(self.state, State::Stopped | State::TransportError) {
            return;
        }
        self.state = State::Stopping;
        if self.stop_deadline.is_none() {
            self.stop_deadline = now.checked_add(Duration::from_secs(u64::from(self.limits.shutdown_seconds)));
        }
        if !self.tree_stop_ordered {
            self.tree_stop_ordered = true;
            actions.push(MachineAction::StopTree);
        }
    }
    fn eof(&mut self, stderr_tail: Vec<String>, actions: &mut Vec<MachineAction>) {
        if matches!(self.state, State::Stopped | State::TransportError) {
            return;
        }
        self.close_transport(self.expected_eof, stderr_tail, actions);
    }
    fn tail(&self, lines: Vec<String>) -> (Vec<String>, usize) {
        let capacity = (self.limits.queue_bytes as usize).saturating_sub(self.incoming_bytes());
        let mut bytes = self.transport_base;
        let mut raw_bytes = 0;
        let mut tail = Vec::new();
        for line in lines.into_iter().take(self.limits.stderr_lines as usize) {
            let overhead = 2 + usize::from(!tail.is_empty());
            if bytes + overhead > capacity {
                break;
            }
            bytes += overhead;
            let mut bounded = String::new();
            for c in line.chars().take(self.limits.stderr_line_scalars as usize) {
                let encoded = match c {
                    '"' | '\\' | '\n' | '\r' | '\t' => 2,
                    c if c <= '\u{1f}' => 6,
                    c => c.len_utf8(),
                };
                if bytes + encoded > capacity || raw_bytes + c.len_utf8() > self.limits.stderr_bytes as usize {
                    break;
                }
                bounded.push(c);
                bytes += encoded;
                raw_bytes += c.len_utf8();
            }
            tail.push(bounded);
        }
        (tail, bytes)
    }
    fn fail(&mut self, stderr_tail: Vec<String>, actions: &mut Vec<MachineAction>) {
        self.close_transport(false, stderr_tail, actions);
    }
    fn close_transport(&mut self, expected: bool, stderr_tail: Vec<String>, actions: &mut Vec<MachineAction>) {
        if matches!(self.state, State::Stopped | State::TransportError) {
            return;
        }
        self.state = if expected { State::Stopped } else { State::TransportError };
        // Close trusted admission before exposing completion or cleanup actions.
        self.close_turn(CloseReason::TransportLoss, actions);
        self.retire_session(actions);
        let mut completions = Vec::new();
        for e in &mut self.ledger {
            if let Some(c) = &e.cancellation {
                c.cancel();
            }
            if e.status == Status::Pending {
                e.status = Status::Cancelled;
                if e.id.as_str().starts_with("h:") {
                    let bytes = e.held_bytes;
                    e.held_bytes = 0;
                    completions.push((
                        ClientNotice::RequestFinished {
                            request_id: e.id.clone(),
                            scope: e.scope.clone(),
                            result: Err(error(ErrorCode::TransportError)),
                        },
                        bytes,
                        e.control,
                    ));
                }
            }
            if e.id.as_str().starts_with("r:") && !e.running {
                // An undispatched input (including the current cancel request)
                // has no physical action owner after transport termination.
                // Running charges and un-Written reply tickets remain owned.
                e.held_bytes = 0;
            }
        }
        // Each admitted host call already reserved its bounded completion cell.
        for (notice, bytes, control) in completions {
            // Moving a pre-reserved cell preserves its byte/slot reservation.
            if let Some(ticket) = self.lease() {
                self.notices.push(Lease { ticket, bytes, control, completion: true, request_control: control });
                actions.push(MachineAction::Notice { ticket, notice: Box::new(notice) });
            }
        }
        let (tail, bytes) = self.tail(stderr_tail);
        self.terminal_notice(
            ClientNotice::TransportClosed {
                cause: if expected { TransportCause::ExpectedEof } else { TransportCause::Error(error(ErrorCode::TransportError)) },
                stderr_tail: tail,
            },
            bytes,
            actions,
        );
        if !self.tree_stop_ordered {
            self.tree_stop_ordered = true;
            actions.push(MachineAction::StopTree);
        }
    }
}
