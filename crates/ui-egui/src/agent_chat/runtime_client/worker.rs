//! One native worker, using the existing process owner and shared pipe workers.
use super::*;
use crate::agent_chat::process::{
    AgentProcess, InboundCredit, InboundPool, ProcessEvent, ProcessHandle, ProcessIoError, ProcessIoLimits, ProcessLaunch, StrictProcess,
};
use std::collections::VecDeque;
use std::sync::atomic::AtomicU8;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, Weak};

fn local(code: ErrorCode) -> ClientError {
    ClientError { code }
}
fn io_error(_: ProcessIoError) -> ClientError {
    local(ErrorCode::TransportError)
}
// Off-frame only. Keep spare encoder capacity out of retained mailboxes and
// the existing capacity-accounted writer; no 2*frame negotiated floor is needed.
fn compact(bytes: Vec<u8>) -> Vec<u8> {
    bytes.into_boxed_slice().into_vec()
}
fn reason_code(reason: CloseReason) -> u8 {
    match reason {
        CloseReason::User => 1,
        CloseReason::Reset => 2,
        CloseReason::Deadline => 3,
        CloseReason::Shutdown => 4,
        CloseReason::TransportLoss => 5,
        CloseReason::HostTerminal => 6,
        CloseReason::TerminalProposal => 0,
    }
}
fn reason_from(code: u8) -> Option<CloseReason> {
    match code {
        1 => Some(CloseReason::User),
        2 => Some(CloseReason::Reset),
        3 => Some(CloseReason::Deadline),
        4 => Some(CloseReason::Shutdown),
        5 => Some(CloseReason::TransportLoss),
        6 => Some(CloseReason::HostTerminal),
        _ => None,
    }
}

struct Permit {
    pool: Arc<RuntimeAdmission>,
    handle: Option<ProcessHandle>,
}
impl Permit {
    fn reserve(pool: Arc<RuntimeAdmission>) -> Result<Self, ClientError> {
        {
            let mut state = pool.0.lock().map_err(|_| local(ErrorCode::TransportError))?;
            state.refresh_cleaned();
            if state.active >= state.limit {
                return Err(local(ErrorCode::ResourceExhausted));
            }
            state.active += 1;
        }
        Ok(Self { pool, handle: None })
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.shutdown(false);
        }
        if let Ok(mut state) = self.pool.0.lock() {
            match self.handle.take() {
                Some(handle) if handle.try_cleanup_result() != Some(Ok(())) => state.uncertain.push(handle),
                _ => state.active -= 1,
            }
        }
    }
}

#[derive(Debug)]
/// Move-only, pre-serialized request tied to native client/limits/scope snapshots.
pub struct PreparedRequest {
    owner: Weak<()>,
    client: Id,
    epoch: u64,
    session: Option<Id>,
    scope: Scope,
    request_id: RequestId,
    deadline: Instant,
    reset: bool,
    control: bool,
    turn: bool,
    turn_flags: Option<Arc<ScopeFlags>>,
    intent: Option<Intent>,
    completion_bytes: usize,
    bytes: Vec<u8>,
}
#[derive(Debug)]
/// Move-only ready event. Its final sequence is assigned by outgoing dispatch.
pub struct PreparedEvent {
    owner: Weak<()>,
    client: Id,
    epoch: u64,
    session: Option<Id>,
    scope: Scope,
    bytes: Vec<u8>,
    scope_flags: Option<Arc<ScopeFlags>>,
}

#[derive(Debug)]
struct ScopeFlags {
    scope: TurnScope,
    admission: AtomicBool,
    cancelled: Arc<AtomicBool>,
    bound: AtomicBool,
    reason: AtomicU8,
    local_only: AtomicBool,
}
impl ScopeFlags {
    fn record(&self, reason: CloseReason) {
        let code = reason_code(reason);
        if code != 0 {
            let _ = self.reason.compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
        }
    }
    fn reason(&self) -> Option<CloseReason> {
        reason_from(self.reason.load(Ordering::Acquire))
    }
}
struct Outgoing {
    key: u64,
    bytes: usize,
    control: bool,
    ticket: Option<u64>,
    callback: Option<RequestId>,
}
impl Outgoing {
    fn ready_slot(&self) -> bool {
        self.callback.is_none() || self.ticket.is_some()
    }
}
struct Pending {
    id: RequestId,
    scope: Scope,
    control: bool,
    done: bool,
    completion: Option<InboundCredit>,
    intent: Option<Intent>,
}
struct NoticeCell {
    ticket: u64,
    notice: ClientNotice,
    bytes: usize,
    // Last: notice payload destroyed or moved to drain BEFORE refund.
    credits: Vec<InboundCredit>,
}
enum Ready {
    Request(PreparedRequest),
    Event(PreparedEvent),
    Reply { token: CallbackToken, bytes: Vec<u8> },
    Progress { token: CallbackToken, bytes: Vec<u8> },
    Finished { token: CallbackToken },
}
enum InboundRoute {
    Callback(RequestId),
    Response(RequestId),
    Ready,
    Proposal(TurnScope),
    Event(Scope),
    None,
}
struct Work {
    key: u64,
    ready: Ready,
}
#[derive(Clone, Debug)]
enum Intent {
    CancelTurn(TurnScope, CancelReason),
    CancelRequest(RequestId),
    Shutdown(ShutdownReason),
}
impl PartialEq for Intent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::CancelTurn(a, _), Self::CancelTurn(b, _)) => a == b,
            (Self::CancelRequest(a), Self::CancelRequest(b)) => a == b,
            (Self::Shutdown(_), Self::Shutdown(_)) => true,
            _ => false,
        }
    }
}
fn request_intent(scope: &Scope, request: &HostRequest) -> Option<Intent> {
    match request {
        HostRequest::TurnCancel(p) => match scope {
            Scope::Turn(s) => Some(Intent::CancelTurn(s.clone(), p.reason)),
            _ => None,
        },
        HostRequest::RequestCancel(p) => Some(Intent::CancelRequest(p.target_request_id.clone())),
        HostRequest::RuntimeShutdown(p) => Some(Intent::Shutdown(p.reason)),
        _ => None,
    }
}
struct Mailbox {
    limits: Limits,
    epoch: u64,
    session: Option<Id>,
    retiring: Option<Id>,
    turn: Option<Arc<ScopeFlags>>,
    ready: bool,
    closed: bool,
    next_key: u64,
    data: VecDeque<Work>,
    control_work: VecDeque<Work>,
    intent: Option<Intent>,
    inflight: Option<Intent>,
    cancel_id: Option<RequestId>,
    shutdown_id: Option<RequestId>,
    complete: Option<(TurnScope, TurnTerminal)>,
    outgoing: Vec<Outgoing>,
    pending: Vec<Pending>,
    notices: VecDeque<NoticeCell>,
    consumed: Vec<u64>,
    callbacks: Vec<Weak<CallbackLife>>,
    transport_credit: Option<InboundCredit>,
    close_credit: Option<InboundCredit>,
}
struct Shared {
    identity: Arc<()>,
    generation: Id,
    state: Mutex<Mailbox>,
    incoming: Arc<InboundPool>,
    ids: Arc<dyn IdSource>,
    close_bytes: usize,
    wake: SyncSender<()>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    abort: AtomicBool,
    stopped_admission: AtomicBool,
    cancelled: Arc<AtomicBool>,
    drain_pending: AtomicBool,
    urgent_reason: AtomicU8,
    handle: ProcessHandle,
}
impl Shared {
    fn signal(&self) {
        let _ = self.wake.try_send(());
    }
    fn force(&self) {
        self.stopped_admission.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
        self.abort.store(true, Ordering::Release);
        self.handle.shutdown(false);
        self.signal();
    }
    fn writable(&self) -> Result<(), ClientError> {
        if self.stopped_admission.load(Ordering::Acquire) { Err(local(ErrorCode::TransportError)) } else { Ok(()) }
    }
}
impl Mailbox {
    fn control_scope(&self, scope: &Scope, intent: &Intent) -> bool {
        match intent {
            Intent::CancelTurn(s, _) => scope == &Scope::Turn(s.clone()) && self.turn.as_ref().is_some_and(|t| t.scope == *s),
            Intent::CancelRequest(id) => self.pending.iter().any(|p| &p.id == id && &p.scope == scope),
            Intent::Shutdown(_) => scope == &Scope::Runtime,
        }
    }
    fn scope_open(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Runtime => true,
            Scope::Session(id) => self.session.as_ref().is_none_or(|current| current == id) && self.retiring.as_ref() != Some(id),
            Scope::Turn(scope) => {
                self.session.as_ref() == Some(&scope.session_id)
                    && self.retiring.as_ref() != Some(&scope.session_id)
                    && self.turn.as_ref().is_none_or(|t| t.scope == *scope && t.admission.load(Ordering::Acquire))
            }
        }
    }
    fn reserve_out(&mut self, bytes: usize, control: bool) -> Result<u64, ClientError> {
        self.reserve_output(bytes, control, None)
    }
    fn reserve_callback(&mut self, bytes: usize, id: RequestId) -> Result<u64, ClientError> {
        self.reserve_output(bytes, false, Some(id))
    }
    fn reserve_output(&mut self, bytes: usize, control: bool, callback: Option<RequestId>) -> Result<u64, ClientError> {
        let total = self.outgoing.iter().map(|o| o.bytes).sum::<usize>();
        let data = self.outgoing.iter().filter(|o| !o.control).map(|o| o.bytes).sum::<usize>();
        if (callback.is_none()
            && self.outgoing.iter().filter(|o| o.control == control && o.ready_slot()).count()
                >= if control { self.limits.control_slots } else { self.limits.pending_requests } as usize)
            || (callback.is_some() && self.outgoing.iter().filter(|out| out.callback.is_some()).count() >= self.limits.pending_requests as usize)
            || total.checked_add(bytes).is_none_or(|n| n > self.limits.queue_bytes as usize)
            || (!control && data.checked_add(bytes).is_none_or(|n| n > (self.limits.queue_bytes - self.limits.control_lane_bytes) as usize))
        {
            return Err(local(ErrorCode::ResourceExhausted));
        }
        let key = self.next_key;
        self.next_key = self.next_key.checked_add(1).ok_or(local(ErrorCode::ResourceExhausted))?;
        // Dormant callback cells are owned by the already-admitted opposite-
        // direction call slot. They consume bytes, but become an output frame
        // slot only at dispatch. Otherwise pendingRequests=1 could never stream.
        self.outgoing.push(Outgoing { key, bytes, control, ticket: None, callback });
        Ok(key)
    }
    fn assign(&mut self, key: u64, ticket: u64, bytes: usize) -> bool {
        let Some(old) = self.outgoing.iter().find(|o| o.key == key) else { return false };
        if !old.ready_slot() && self.outgoing.iter().filter(|out| !out.control && out.ready_slot()).count() >= self.limits.pending_requests as usize {
            return false;
        }
        let total = self.outgoing.iter().map(|o| o.bytes).sum::<usize>();
        let data = self.outgoing.iter().filter(|o| !o.control).map(|o| o.bytes).sum::<usize>();
        if total - old.bytes + bytes > self.limits.queue_bytes as usize
            || (!old.control && data - old.bytes + bytes > (self.limits.queue_bytes - self.limits.control_lane_bytes) as usize)
        {
            return false;
        }
        if let Some(out) = self.outgoing.iter_mut().find(|o| o.key == key) {
            out.ticket = Some(ticket);
            out.bytes = bytes;
            true
        } else {
            false
        }
    }
    fn revoke(&mut self, scope: Option<&TurnScope>, cancel: bool) {
        if let Some(turn) = &self.turn
            && scope.is_none_or(|scope| scope == &turn.scope)
        {
            turn.admission.store(false, Ordering::Release);
            if cancel {
                turn.cancelled.store(true, Ordering::Release);
            }
        }
        self.callbacks.retain(|weak| {
            let Some(life) = weak.upgrade() else { return false };
            if scope.is_none_or(|scope| life.scope == Scope::Turn(scope.clone())) {
                life.live.store(false, Ordering::Release);
                if cancel {
                    life.cancellation.cancel();
                }
            }
            !life.finished.load(Ordering::Acquire)
        });
    }
}

pub struct RuntimeClient {
    shared: Arc<Shared>,
}
#[derive(Clone)]
pub struct RuntimeSender {
    shared: Arc<Shared>,
    ids: Arc<dyn IdSource>,
}
impl RuntimeClient {
    /// Off-frame startup: exact executable, fallible threads and host permit.
    pub fn start(launch: RuntimeLaunch, services: RuntimeServices, repaint: Arc<dyn Fn() + Send + Sync>) -> Result<Self, ClientError> {
        validate_limits(&launch.limits).map_err(|_| local(ErrorCode::InvalidRequest))?;
        let generation = services.ids.next_id().map_err(|e| local(e.code))?;
        let now = services.clock.now();
        let machine = RuntimeMachine::new(generation.clone(), launch.limits.clone(), now)?;
        let mut permit = Permit::reserve(services.admission.clone())?;
        let (wake, receive) = sync_channel(1);
        let pipe_wake = wake.clone();
        let io = ProcessIoLimits {
            frame_bytes: launch.limits.frame_bytes as usize,
            queue_bytes: launch.limits.queue_bytes as usize,
            control_lane_bytes: launch.limits.control_lane_bytes as usize,
            stderr_lines: launch.limits.stderr_lines as usize,
            stderr_line_scalars: launch.limits.stderr_line_scalars as usize,
            stderr_bytes: launch.limits.stderr_bytes as usize,
        };
        let process =
            AgentProcess::spawn_strict(&ProcessLaunch { executable: launch.executable, args: launch.args, cwd: launch.cwd }, io, move || {
                let _ = pipe_wake.try_send(());
            })
            .map_err(|_| local(ErrorCode::TransportError))?;
        permit.handle = Some(process.handle());
        let incoming = process.inbound();
        let transport_credit = incoming.reserve_control(machine.transport_base).map_err(io_error)?;
        let close_credit = incoming.reserve_control(machine.closure_reserve).map_err(io_error)?;
        let ordinary = launch.limits.pending_requests as usize;
        let controls = launch.limits.control_slots as usize;
        let notices = machine.reserve();
        let shared = Arc::new(Shared {
            identity: Arc::new(()),
            generation,
            state: Mutex::new(Mailbox {
                limits: launch.limits,
                epoch: 0,
                session: None,
                retiring: None,
                turn: None,
                ready: false,
                closed: false,
                next_key: 1,
                data: VecDeque::with_capacity(2 * ordinary + controls),
                control_work: VecDeque::with_capacity(controls),
                intent: None,
                inflight: None,
                cancel_id: None,
                shutdown_id: None,
                complete: None,
                outgoing: Vec::with_capacity(2 * ordinary + controls),
                pending: Vec::with_capacity(ordinary + controls),
                notices: VecDeque::with_capacity(notices),
                consumed: Vec::with_capacity(notices),
                callbacks: Vec::with_capacity(ordinary + controls),
                transport_credit: Some(transport_credit),
                close_credit: Some(close_credit),
            }),
            incoming,
            ids: services.ids.clone(),
            close_bytes: machine.closure_reserve,
            wake,
            repaint,
            abort: AtomicBool::new(false),
            stopped_admission: AtomicBool::new(false),
            cancelled: Arc::new(AtomicBool::new(false)),
            drain_pending: AtomicBool::new(false),
            urgent_reason: AtomicU8::new(0),
            handle: process.handle(),
        });
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("mastra-runtime".into())
            .spawn(move || {
                Worker {
                    shared: worker_shared,
                    process,
                    machine,
                    services,
                    receive,
                    _permit: permit,
                    shutdown_ticket: None,
                    retained_info: None,
                    retained_proposal: None,
                    tail_credit: None,
                    internal_failure: false,
                }
                .run(launch.host_version);
            })
            .map_err(|_| local(ErrorCode::TransportError))?;
        Ok(Self { shared })
    }
    pub fn sender(&self) -> RuntimeSender {
        RuntimeSender { shared: self.shared.clone(), ids: self.shared.ids.clone() }
    }
    /// UI-safe explicit consuming handoff to the trusted host, bounded by both
    /// limits. IPC retains no returned payload; caller clones are caller-owned.
    pub fn drain(&mut self, max_items: usize, max_bytes: usize) -> Vec<ClientNotice> {
        let mut result = Vec::new();
        let mut bytes = 0usize;
        if let Ok(mut state) = self.shared.state.try_lock() {
            let count = max_items.min(state.notices.len());
            for _ in 0..count {
                let Some(front) = state.notices.front() else { break };
                if front.bytes > max_bytes.saturating_sub(bytes) {
                    break;
                }
                let Some(cell) = state.notices.pop_front() else { break };
                let NoticeCell { ticket, notice, bytes: charge, credits } = cell;
                if let ClientNotice::RequestFinished { request_id, .. } = &notice {
                    state.pending.retain(|p| &p.id != request_id);
                }
                // Explicit terminal handoff. No internal payload copy survives.
                result.push(notice);
                bytes += charge;
                state.consumed.push(ticket);
                drop(credits);
            }
        } else {
            self.shared.drain_pending.store(true, Ordering::Release);
        }
        self.shared.signal();
        result
    }
}
impl Drop for RuntimeClient {
    fn drop(&mut self) {
        self.shared.force();
    }
}

impl RuntimeSender {
    /// Off-frame: IDs, validation and serialization happen here, never on submit.
    pub fn prepare_request(&self, scope: Scope, request: HostRequest, deadline: Instant) -> Result<PreparedRequest, ClientError> {
        self.shared.writable()?;
        let ready_intent = request_intent(&scope, &request);
        let (limits, epoch, session) = {
            let state = self.shared.state.lock().map_err(|_| local(ErrorCode::TransportError))?;
            if state.closed || !state.ready || (!state.scope_open(&scope) && !ready_intent.as_ref().is_some_and(|i| state.control_scope(&scope, i))) {
                return Err(local(if state.closed { ErrorCode::TransportError } else { ErrorCode::InvalidRequest }));
            }
            (state.limits.clone(), state.epoch, state.session.clone())
        };
        let id = self.ids.next_id().map_err(|e| local(e.code))?;
        let request_id = RequestId::parse(&format!("h:{}", id.as_str()), Sender::Host).map_err(|_| local(ErrorCode::InternalError))?;
        let control = matches!(request, HostRequest::TurnCancel(_) | HostRequest::RequestCancel(_) | HostRequest::RuntimeShutdown(_));
        let reset = matches!(request, HostRequest::SessionReset(_));
        let turn = matches!(request, HostRequest::TurnStart(_));
        let exhausted = matches!(&request,HostRequest::TurnStart(p) if p.budgets.wall_time_seconds==0);
        let intent = ready_intent;
        let turn_flags = if turn {
            match &scope {
                Scope::Turn(scope) => Some(Arc::new(ScopeFlags {
                    scope: scope.clone(),
                    admission: AtomicBool::new(!exhausted),
                    cancelled: Arc::new(AtomicBool::new(exhausted)),
                    bound: AtomicBool::new(false),
                    reason: AtomicU8::new(if exhausted { reason_code(CloseReason::Deadline) } else { 0 }),
                    local_only: AtomicBool::new(exhausted),
                })),
                _ => None,
            }
        } else {
            None
        };
        let frame = Frame::Request {
            header: RequestHeader {
                protocol_version: "1.0".into(),
                request_id: request_id.clone(),
                scope: scope.clone(),
                method: request.method().into(),
                extensions: None,
            },
            body: RequestBody::Host(request),
        };
        let bytes = if control { encode_control_frame(&frame, &limits) } else { encode_frame(&frame, &limits) }.map_err(|e| local(e.code))?;
        let bytes = compact(bytes);
        let mut completion_bytes = 0;
        for code in [ErrorCode::Cancelled, ErrorCode::Timeout, ErrorCode::TransportError, ErrorCode::InternalError] {
            let f = Frame::Response {
                header: ResponseHeader { protocol_version: "1.0".into(), request_id: request_id.clone(), scope: scope.clone(), extensions: None },
                body: ResponseBody::Error(error(code)),
            };
            completion_bytes = completion_bytes.max(encode_frame(&f, &limits).map_err(|e| local(e.code))?.len());
        }
        Ok(PreparedRequest {
            owner: Arc::downgrade(&self.shared.identity),
            client: self.shared.generation.clone(),
            epoch,
            session,
            scope,
            request_id,
            deadline,
            control,
            reset,
            turn,
            turn_flags,
            intent,
            completion_bytes,
            bytes,
        })
    }
    /// UI-safe bounded admission of an already-ready object; never waits on IO.
    pub fn try_submit(&self, request: PreparedRequest) -> Result<RequestId, ClientError> {
        self.enqueue_request(request, false)
    }
    fn enqueue_request(&self, mut request: PreparedRequest, reset: bool) -> Result<RequestId, ClientError> {
        self.shared.writable()?;
        if !Weak::ptr_eq(&request.owner, &Arc::downgrade(&self.shared.identity)) || request.client != self.shared.generation {
            return Err(local(ErrorCode::InvalidRequest));
        }
        let mut state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(_) => {
                if reset || request.control {
                    if reset {
                        let _ = self.shared.urgent_reason.compare_exchange(0, reason_code(CloseReason::Reset), Ordering::AcqRel, Ordering::Acquire);
                    }
                    self.shared.force();
                }
                return Err(local(ErrorCode::ResourceExhausted));
            }
        };
        if !Weak::ptr_eq(&request.owner, &Arc::downgrade(&self.shared.identity))
            || request.client != self.shared.generation
            || request.epoch != state.epoch
            || request.session != state.session
            || state.closed
            || (!state.scope_open(&request.scope) && !reset && !request.intent.as_ref().is_some_and(|i| state.control_scope(&request.scope, i)))
            || request.reset != reset
        {
            return Err(local(ErrorCode::InvalidRequest));
        }
        if reset {
            if let Some(turn) = &state.turn {
                turn.record(CloseReason::Reset);
            }
            state.retiring = state.session.clone();
            state.revoke(None, true);
        }
        if let Some(intent) = &request.intent {
            match intent {
                Intent::CancelTurn(scope, reason) => {
                    if !state.control_scope(&request.scope, intent) {
                        return Err(local(ErrorCode::InvalidRequest));
                    }
                    if let Some(t) = &state.turn {
                        t.record(match reason {
                            CancelReason::User => CloseReason::User,
                            CancelReason::Reset => CloseReason::Reset,
                            CancelReason::Deadline => CloseReason::Deadline,
                            CancelReason::Shutdown => CloseReason::Shutdown,
                        });
                    }
                    state.revoke(Some(scope), true);
                }
                Intent::CancelRequest(id) => {
                    if state.pending.iter().find(|p| &p.id == id).is_none_or(|p| p.done) {
                        return Err(local(ErrorCode::InvalidRequest));
                    }
                }
                Intent::Shutdown(_) => {
                    if let Some(t) = &state.turn {
                        t.record(CloseReason::Shutdown);
                    }
                    state.revoke(None, true);
                    self.shared.stopped_admission.store(true, Ordering::Release);
                    self.shared.cancelled.store(true, Ordering::Release);
                }
            }
            let known =
                state.pending.iter().find(|p| p.control && p.intent.as_ref() == Some(intent)).map(|p| p.id.clone()).or_else(|| match intent {
                    Intent::CancelTurn(_, _) => state.cancel_id.clone(),
                    Intent::Shutdown(_) => state.shutdown_id.clone(),
                    _ => None,
                });
            if let Some(id) = known {
                return Ok(id);
            }
            if state.inflight.as_ref() == Some(intent) {
                return Err(local(ErrorCode::ResourceExhausted));
            }
            if state.inflight.is_some() || state.intent.as_ref().is_some_and(|i| i != intent) || state.pending.iter().any(|p| p.control) {
                drop(state);
                self.shared.force();
                return Err(local(ErrorCode::ResourceExhausted));
            }
            if state.intent.as_ref() == Some(intent) {
                state.intent = None;
            }
        }
        if state.pending.iter().filter(|p| p.control == request.control).count()
            >= if request.control { state.limits.control_slots } else { state.limits.pending_requests } as usize
        {
            drop(state);
            if reset || request.control {
                self.shared.force();
            }
            return Err(local(ErrorCode::ResourceExhausted));
        }
        let credit = match self.shared.incoming.try_reserve(request.completion_bytes, request.control) {
            Ok(c) => c,
            Err(_) => {
                drop(state);
                if reset || request.control {
                    self.shared.force();
                }
                return Err(local(ErrorCode::ResourceExhausted));
            }
        };
        let close = if request.turn && state.close_credit.is_none() {
            match self.shared.incoming.try_reserve(self.shared.close_bytes, true) {
                Ok(c) => Some(c),
                Err(_) => return Err(local(ErrorCode::ResourceExhausted)),
            }
        } else {
            None
        };
        let key = match state.reserve_out(request.bytes.len(), request.control) {
            Ok(key) => key,
            Err(e) => {
                drop(state);
                if reset || request.control {
                    self.shared.force();
                }
                return Err(e);
            }
        };
        let id = request.request_id.clone();
        match &request.intent {
            Some(Intent::CancelTurn(_, _)) => state.cancel_id = Some(id.clone()),
            Some(Intent::Shutdown(_)) => state.shutdown_id = Some(id.clone()),
            _ => {}
        }
        if let Some(credit) = close {
            state.close_credit = Some(credit);
        }
        if request.turn && state.turn.is_none() {
            state.turn = request.turn_flags.take();
        }
        state.pending.push(Pending {
            id: id.clone(),
            scope: request.scope.clone(),
            control: request.control,
            done: false,
            completion: Some(credit),
            intent: request.intent.clone(),
        });
        let work = Work { key, ready: Ready::Request(request) };
        if matches!(&work.ready,Ready::Request(p) if p.control) {
            state.control_work.push_back(work);
        } else {
            state.data.push_back(work);
        }
        drop(state);
        self.shared.signal();
        Ok(id)
    }
    /// Off-frame validation/serialization with maximum sequence-header capacity.
    pub fn prepare_event(&self, scope: Scope, event: HostEvent, cause: Option<RequestId>) -> Result<PreparedEvent, ClientError> {
        self.shared.writable()?;
        if !matches!(event, HostEvent::ConnectionState(_)) {
            return Err(local(ErrorCode::InvalidRequest));
        }
        let (limits, epoch, session, scope_flags) = {
            let state = self.shared.state.lock().map_err(|_| local(ErrorCode::TransportError))?;
            if !state.ready || state.closed || !state.scope_open(&scope) || matches!(&scope,Scope::Session(id) if state.session.as_ref()!=Some(id)) {
                return Err(local(ErrorCode::InvalidRequest));
            }
            (
                state.limits.clone(),
                state.epoch,
                state.session.clone(),
                state.turn.as_ref().filter(|turn| scope == Scope::Turn(turn.scope.clone())).cloned(),
            )
        };
        let cause = cause.map(|id| Id::parse(id.as_str()).map_err(|_| local(ErrorCode::InvalidRequest))).transpose()?;
        let frame = Frame::Event {
            header: EventHeader {
                protocol_version: "1.0".into(),
                sequence: MAX_SEQUENCE,
                request_id: RequiredNullable(cause),
                scope: scope.clone(),
                extensions: None,
            },
            body: EventBody::Host(event),
        };
        let bytes = encode_frame(&frame, &limits).map_err(|e| local(e.code))?;
        let bytes = compact(bytes);
        Ok(PreparedEvent {
            owner: Arc::downgrade(&self.shared.identity),
            client: self.shared.generation.clone(),
            epoch,
            session,
            scope,
            bytes,
            scope_flags,
        })
    }
    pub fn try_emit(&self, event: PreparedEvent) -> Result<(), ClientError> {
        self.shared.writable()?;
        if !Weak::ptr_eq(&event.owner, &Arc::downgrade(&self.shared.identity))
            || event.client != self.shared.generation
            || event.scope_flags.as_ref().is_some_and(|flags| !flags.admission.load(Ordering::Acquire))
        {
            return Err(local(ErrorCode::InvalidRequest));
        }
        let mut state = self.shared.state.try_lock().map_err(|_| local(ErrorCode::ResourceExhausted))?;
        if !Weak::ptr_eq(&event.owner, &Arc::downgrade(&self.shared.identity))
            || event.client != self.shared.generation
            || event.epoch != state.epoch
            || event.session != state.session
            || state.closed
            || !state.scope_open(&event.scope)
        {
            return Err(local(ErrorCode::InvalidRequest));
        }
        let key = state.reserve_out(event.bytes.len(), false)?;
        state.data.push_back(Work { key, ready: Ready::Event(event) });
        drop(state);
        self.shared.signal();
        Ok(())
    }
    fn intent(&self, intent: Intent) -> Result<Option<RequestId>, ClientError> {
        let mut state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(_) => {
                let reason = match &intent {
                    Intent::Shutdown(_) => CloseReason::Shutdown,
                    Intent::CancelTurn(_, CancelReason::Reset) => CloseReason::Reset,
                    Intent::CancelTurn(_, CancelReason::Deadline) => CloseReason::Deadline,
                    Intent::CancelTurn(_, CancelReason::Shutdown) => CloseReason::Shutdown,
                    _ => CloseReason::User,
                };
                let _ = self.shared.urgent_reason.compare_exchange(0, reason_code(reason), Ordering::AcqRel, Ordering::Acquire);
                self.shared.force();
                return Err(local(ErrorCode::ResourceExhausted));
            }
        };
        if state.closed {
            return Err(local(ErrorCode::TransportError));
        }
        match &intent {
            Intent::CancelTurn(scope, reason) => {
                if state.turn.as_ref().is_none_or(|t| t.scope != *scope) {
                    return Err(local(ErrorCode::InvalidRequest));
                }
                if let Some(t) = &state.turn
                    && t.scope == *scope
                {
                    t.record(match reason {
                        CancelReason::User => CloseReason::User,
                        CancelReason::Reset => CloseReason::Reset,
                        CancelReason::Deadline => CloseReason::Deadline,
                        CancelReason::Shutdown => CloseReason::Shutdown,
                    });
                }
                state.revoke(Some(scope), true);
            }
            Intent::Shutdown(_) => {
                if let Some(t) = &state.turn {
                    t.record(CloseReason::Shutdown);
                }
                state.revoke(None, true);
                self.shared.stopped_admission.store(true, Ordering::Release);
                self.shared.cancelled.store(true, Ordering::Release);
            }
            Intent::CancelRequest(id) => {
                let Some(pending) = state.pending.iter().find(|p| &p.id == id) else {
                    return Err(local(ErrorCode::InvalidRequest));
                };
                if pending.done {
                    return Ok(None);
                }
                let queued = state.data.iter().find_map(|w| match &w.ready {
                    Ready::Request(p) if p.turn && &p.request_id == id => match &p.scope {
                        Scope::Turn(scope) => Some(scope.clone()),
                        _ => None,
                    },
                    _ => None,
                });
                if let Some(scope) = queued {
                    if let Some(turn) = &state.turn {
                        turn.record(CloseReason::User);
                    }
                    state.revoke(Some(&scope), true);
                }
            }
        }
        let known = match &intent {
            Intent::CancelTurn(_, _) => state.cancel_id.clone(),
            Intent::Shutdown(_) => state.shutdown_id.clone(),
            Intent::CancelRequest(_) => None,
        };
        if let Some(id) = known {
            return Ok(Some(id));
        }
        if matches!(&intent, Intent::CancelTurn(_, _)) && state.turn.as_ref().is_some_and(|t| t.local_only.load(Ordering::Acquire)) {
            return Ok(None);
        }
        if let Some(p) = state.pending.iter().find(|p| p.control && p.intent.as_ref() == Some(&intent)) {
            return Ok(Some(p.id.clone()));
        }
        if state.intent.as_ref() == Some(&intent) {
            return Ok(None);
        }
        if state.inflight.as_ref() == Some(&intent) {
            return Ok(None);
        }
        if state.intent.is_some() || state.inflight.is_some() || state.pending.iter().any(|p| p.control) {
            drop(state);
            self.shared.force();
            return Err(local(ErrorCode::ResourceExhausted));
        }
        state.intent = Some(intent);
        drop(state);
        self.shared.signal();
        Ok(None)
    }
    /// UI-safe local closure/intent. None also covers deferred wire identity;
    /// the worker, not this method, invokes the injected ID source.
    pub fn cancel_request(&self, target: &RequestId) -> Result<Option<RequestId>, ClientError> {
        self.intent(Intent::CancelRequest(target.clone()))
    }
    /// UI-safe synchronous revocation before bounded intent admission. The wire
    /// identity is deferred; None never means local cancellation was postponed.
    pub fn cancel_turn(&self, scope: &TurnScope, reason: CancelReason) -> Result<Option<RequestId>, ClientError> {
        self.intent(Intent::CancelTurn(scope.clone(), reason))
    }
    pub fn reset(&self, request: PreparedRequest) -> Result<RequestId, ClientError> {
        // Closure and ready-object admission share one bounded transaction.
        self.enqueue_request(request, true)
    }
    pub fn complete_turn(&self, scope: &TurnScope, terminal: TurnTerminal) -> Result<(), ClientError> {
        let mut state = self.shared.state.try_lock().map_err(|_| local(ErrorCode::ResourceExhausted))?;
        if state.closed || state.turn.as_ref().is_none_or(|t| t.scope != *scope || t.admission.load(Ordering::Acquire)) {
            return Err(local(ErrorCode::InvalidRequest));
        }
        if state.complete.is_some() {
            return Err(local(ErrorCode::ResourceExhausted));
        }
        state.complete = Some((scope.clone(), terminal));
        drop(state);
        self.shared.signal();
        Ok(())
    }
    pub fn shutdown(&self, reason: ShutdownReason) -> Result<(), ClientError> {
        self.intent(Intent::Shutdown(reason)).map(|_| ())
    }
}

struct Fallback {
    bytes: Vec<u8>,
    key: u64,
}
struct CallbackLife {
    scope: Scope,
    request_id: RequestId,
    cancellation: Cancellation,
    live: AtomicBool,
    finished: AtomicBool,
    streaming: bool,
    fallback: Mutex<Option<Fallback>>,
    // Last: scope/header projections also die before the shared refund guard.
    credit: Option<InboundCredit>,
}
impl CallbackLife {
    fn take_fallback(&self) -> (Option<Fallback>, bool) {
        match self.fallback.lock() {
            Ok(mut cell) => (cell.take(), false),
            Err(e) => (e.into_inner().take(), true),
        }
    }
}
pub struct ReplyPort {
    shared: Arc<Shared>,
    life: Arc<CallbackLife>,
    limits: Limits,
    // Last: fallback/headers are destroyed before the callback guard.
    token: Option<CallbackToken>,
}
impl ReplyPort {
    /// Off-frame. Progress is bound to this immutable callback, not a new scope.
    pub fn emit(&self, event: CallProgress) -> Result<(), ClientError> {
        let token = self.token.as_ref().ok_or(local(ErrorCode::InvalidRequest))?;
        if !self.life.live.load(Ordering::Acquire) || self.life.cancellation.is_cancelled() || self.shared.stopped_admission.load(Ordering::Acquire) {
            return Err(local(ErrorCode::InvalidRequest));
        }
        let event = match event {
            CallProgress::ModelDelta(p) if self.life.streaming => HostEvent::ModelDelta(p),
            CallProgress::ModelDelta(_) => return Err(local(ErrorCode::UnsupportedCapability)),
            CallProgress::AgentProgress(p) if token.method == "agent.run" => HostEvent::AgentProgress(p),
            _ => return Err(local(ErrorCode::InvalidRequest)),
        };
        let frame = Frame::Event {
            header: EventHeader {
                protocol_version: "1.0".into(),
                sequence: MAX_SEQUENCE,
                scope: token.scope.clone(),
                request_id: RequiredNullable(Some(Id::parse(token.request_id.as_str()).map_err(|_| local(ErrorCode::InternalError))?)),
                extensions: None,
            },
            body: EventBody::Host(event),
        };
        let bytes = encode_frame(&frame, &self.limits).map_err(|e| local(e.code))?;
        let bytes = compact(bytes);
        self.enqueue(Ready::Progress { token: token.clone(), bytes }, None)
    }
    /// Off-frame, consuming completion. Any separately retained input must keep
    /// its own token/guard until the actual payload/header data has been dropped.
    pub fn finish(mut self, reply: Result<HostReply, WireError>) -> Result<(), ClientError> {
        let token = self.token.as_ref().ok_or(local(ErrorCode::InvalidRequest))?;
        if !self.life.live.load(Ordering::Acquire) || self.life.cancellation.is_cancelled() || self.shared.abort.load(Ordering::Acquire) {
            // Release of this port is NOT release of a retained call/token.
            return Err(local(ErrorCode::InvalidRequest));
        }
        let body = match reply {
            Ok(reply) if reply.method() == token.method => ResponseBody::host_ok(reply, &self.limits).map_err(|e| local(e.code))?,
            Ok(_) => return Err(local(ErrorCode::InvalidRequest)),
            Err(error) => ResponseBody::Error(error),
        };
        let bytes = encode_frame(
            &Frame::Response {
                header: ResponseHeader {
                    protocol_version: "1.0".into(),
                    request_id: token.request_id.clone(),
                    scope: token.scope.clone(),
                    extensions: None,
                },
                body,
            },
            &self.limits,
        )
        .map_err(|e| local(e.code))?;
        let bytes = compact(bytes);
        self.life.finished.store(true, Ordering::Release);
        let (fallback, poisoned) = self.life.take_fallback();
        if poisoned {
            self.shared.force();
        }
        let key = fallback.map(|cell| {
            let key = cell.key;
            drop(cell.bytes);
            key
        });
        let token = self.token.take().ok_or(local(ErrorCode::InvalidRequest))?;
        let completion = token.clone();
        let result = self.enqueue(Ready::Reply { token, bytes }, key);
        if result.as_ref().is_err_and(|error| error.code == ErrorCode::InvalidRequest) {
            self.retire(completion, key);
        } else if result.is_err() {
            if let Some(key) = key
                && let Ok(mut state) = self.shared.state.lock()
            {
                state.outgoing.retain(|o| o.key != key);
            }
            self.shared.force();
        }
        result
    }
    fn retire(&self, token: CallbackToken, key: Option<u64>) {
        if let Ok(mut state) = self.shared.state.lock() {
            if !state.closed {
                state.data.push_back(Work { key: key.unwrap_or(0), ready: Ready::Finished { token } });
            } else if let Some(key) = key {
                state.outgoing.retain(|out| out.key != key || out.ticket.is_some());
            }
        }
        self.shared.signal();
        if self.shared.drain_pending.swap(false, Ordering::AcqRel) {
            (self.shared.repaint)();
        }
    }
    fn enqueue(&self, ready: Ready, existing: Option<u64>) -> Result<(), ClientError> {
        let bytes = match &ready {
            Ready::Reply { bytes, .. } | Ready::Progress { bytes, .. } => bytes.len(),
            _ => return Err(local(ErrorCode::InternalError)),
        };
        let mut state = self.shared.state.lock().map_err(|_| local(ErrorCode::TransportError))?;
        if !self.life.live.load(Ordering::Acquire) || self.life.cancellation.is_cancelled() || self.life.credit.is_none() || state.closed {
            return Err(local(ErrorCode::InvalidRequest));
        }
        let key = if let Some(key) = existing {
            let total = state.outgoing.iter().map(|o| o.bytes).sum::<usize>();
            let data = state.outgoing.iter().filter(|o| !o.control).map(|o| o.bytes).sum::<usize>();
            let old = state.outgoing.iter().find(|o| o.key == key).ok_or(local(ErrorCode::InvalidRequest))?.bytes;
            if total - old + bytes > state.limits.queue_bytes as usize
                || data - old + bytes > (state.limits.queue_bytes - state.limits.control_lane_bytes) as usize
            {
                return Err(local(ErrorCode::ResourceExhausted));
            }
            if let Some(out) = state.outgoing.iter_mut().find(|o| o.key == key) {
                out.bytes = bytes;
            }
            key
        } else {
            state.reserve_out(bytes, false)?
        };
        state.data.push_back(Work { key, ready });
        drop(state);
        self.shared.signal();
        Ok(())
    }
}
impl Drop for ReplyPort {
    fn drop(&mut self) {
        self.life.finished.store(true, Ordering::Release);
        if let Some(token) = self.token.take() {
            let (fallback, poisoned) = self.life.take_fallback();
            if poisoned {
                self.shared.force();
            }
            if self.life.live.load(Ordering::Acquire)
                && !self.life.cancellation.is_cancelled()
                && !self.shared.abort.load(Ordering::Acquire)
                && fallback.is_some()
            {
                let Some(Fallback { bytes, key }) = fallback else {
                    return;
                };
                let completion = token.clone();
                if let Err(error) = self.enqueue(Ready::Reply { token, bytes }, Some(key)) {
                    if error.code == ErrorCode::InvalidRequest {
                        self.retire(completion, Some(key));
                        return;
                    }
                    if let Ok(mut state) = self.shared.state.lock() {
                        state.outgoing.retain(|o| o.key != key);
                    }
                    self.shared.force();
                }
            } else {
                let key = fallback.map(|cell| {
                    let key = cell.key;
                    drop(cell.bytes);
                    key
                });
                self.retire(token, key);
            }
        }
        self.shared.signal();
        if self.shared.drain_pending.swap(false, Ordering::AcqRel) {
            (self.shared.repaint)();
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    process: StrictProcess,
    machine: RuntimeMachine,
    services: RuntimeServices,
    receive: Receiver<()>,
    _permit: Permit,
    shutdown_ticket: Option<u64>,
    retained_info: Option<InboundCredit>,
    retained_proposal: Option<InboundCredit>,
    tail_credit: Option<InboundCredit>,
    internal_failure: bool,
}

impl Worker {
    fn request_id(&self) -> Result<RequestId, ClientError> {
        let id = self.services.ids.next_id().map_err(|e| local(e.code))?;
        RequestId::parse(&format!("h:{}", id.as_str()), Sender::Host).map_err(|_| local(ErrorCode::InternalError))
    }
    fn run(mut self, host_version: String) {
        let now = self.services.clock.now();
        let services = ServiceCapabilities { connection_describe: true, model_generate: true, agent_run: true, validation: true, streaming: true };
        self.apply(MachineInput::Services { generation: self.shared.generation.clone(), services }, None, None, now);
        match self.request_id() {
            Ok(id) => {
                let request = HostRequest::RuntimeHello(RuntimeHelloParams {
                    supported_versions: vec!["1.0".into()],
                    host_version,
                    limits: self.machine.limits.clone(),
                });
                self.internal_submit(id, Scope::Runtime, request, now);
            }
            Err(e) => {
                self.internal_failure = e.code == ErrorCode::InternalError;
                self.fail(now);
            }
        }
        loop {
            let now = self.services.clock.now();
            if self.shared.abort.load(Ordering::Acquire) {
                self.fail(now);
            } else {
                self.expired_turn(now);
            }
            let mut incoming = false;
            loop {
                let now = self.services.clock.now();
                self.expired_turn(now);
                match self.process.try_recv() {
                    Ok(Some(ProcessEvent::Written(ticket))) => {
                        incoming = true;
                        self.apply(MachineInput::Written { generation: self.shared.generation.clone(), ticket }, None, None, now);
                        if let Ok(mut state) = self.shared.state.lock() {
                            state.outgoing.retain(|out| out.ticket != Some(ticket));
                        }
                        if self.shutdown_ticket == Some(ticket) {
                            self.process.close_input();
                        }
                    }
                    Ok(Some(ProcessEvent::Frame(frame))) => {
                        incoming = true;
                        let (bytes, credit) = frame.into_parts();
                        let phase = if self.machine.info.is_some() { DecodePhase::Negotiated } else { DecodePhase::Bootstrap };
                        let decoded = decode_frame(&bytes, phase, Sender::Runtime, &self.machine.limits);
                        drop(bytes);
                        let now = self.services.clock.now();
                        self.expired_turn(now);
                        match decoded {
                            Ok(decoded) => {
                                let control = match &decoded {
                                    DecodeResult::Frame(Frame::Request { body: RequestBody::Cancel(_), .. }) => true,
                                    DecodeResult::Frame(Frame::Response { header, .. }) => {
                                        self.machine.ledger.iter().any(|e| e.id == header.request_id && e.control)
                                    }
                                    DecodeResult::RequestError { header, .. } => header.method == "request.cancel",
                                    _ => false,
                                };
                                if !control && credit.classify_data().is_err() {
                                    drop(decoded);
                                    drop(credit);
                                    self.fail(now);
                                } else {
                                    self.apply(
                                        MachineInput::Peer { generation: self.shared.generation.clone(), sender: Sender::Runtime, decoded },
                                        None,
                                        Some(credit),
                                        now,
                                    );
                                }
                            }
                            Err(_) => {
                                drop(credit);
                                self.fail(now);
                            }
                        }
                    }
                    Ok(Some(ProcessEvent::IoFailed(_))) => {
                        incoming = true;
                        self.fail(now);
                    }
                    Ok(Some(ProcessEvent::Exited(tail))) => {
                        let tail = self.fit_tail(tail);
                        self.apply(MachineInput::Eof { generation: self.shared.generation.clone(), stderr_tail: tail }, None, None, now);
                        self.tail_credit = None;
                        // All shared IO threads have ended: no writer buffer remains.
                        self.apply(MachineInput::IoReleased { generation: self.shared.generation.clone() }, None, None, now);
                        if let Ok(mut state) = self.shared.state.lock() {
                            state.outgoing.retain(|out| out.ticket.is_none());
                        }
                        self.process.shutdown(true);
                        (self.shared.repaint)();
                        return;
                    }
                    Ok(None) => break,
                    Err(ProcessIoError::ResourceExhausted) => {
                        // Busy bookkeeping is not an empty channel. Retrying
                        // the untouched receive avoids consuming the last wake
                        // and then sleeping with an already-ready frame.
                        incoming = true;
                        std::thread::yield_now();
                        break;
                    }
                    Err(_) => {
                        self.fail(now);
                        break;
                    }
                }
            }
            let now = self.services.clock.now();
            self.expired_turn(now);
            self.apply(MachineInput::Tick, None, None, now);
            let (consumed, complete, mut intent, mut work) = match self.shared.state.lock() {
                Ok(mut state) => {
                    let capacity = state.consumed.capacity();
                    let consumed = std::mem::replace(&mut state.consumed, Vec::with_capacity(capacity));
                    let complete = state.complete.take();
                    let intent = state.intent.take();
                    state.inflight = intent.clone();
                    let queued = match &intent {
                        Some(Intent::CancelRequest(id)) => state.data.iter().position(|w| matches!(&w.ready,Ready::Request(p) if &p.request_id==id)),
                        _ if self.machine.turn.is_none() => match &intent {
                            Some(Intent::CancelTurn(scope, _)) => {
                                state.data.iter().position(|w| matches!(&w.ready,Ready::Request(p) if p.turn&&p.scope==Scope::Turn(scope.clone())))
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    let queued_control = match &intent {
                        Some(Intent::CancelRequest(id)) => {
                            state.control_work.iter().position(|w| matches!(&w.ready,Ready::Request(p) if &p.request_id==id))
                        }
                        _ => None,
                    };
                    let can_reply =
                        state.outgoing.iter().filter(|out| !out.control && out.ready_slot()).count() < state.limits.pending_requests as usize;
                    let data_index = state.data.iter().position(|work| can_reply || !matches!(work.ready, Ready::Reply { .. }));
                    let work = queued
                        .and_then(|index| state.data.remove(index))
                        .or_else(|| queued_control.and_then(|index| state.control_work.remove(index)))
                        .or_else(|| state.control_work.pop_front())
                        .or_else(|| data_index.and_then(|index| state.data.remove(index)));
                    (consumed, complete, intent, work)
                }
                Err(_) => {
                    self.shared.force();
                    self.process.shutdown(true);
                    return;
                }
            };
            if matches!(&work,Some(Work {ready:Ready::Request(p),..}) if p.reset)
                && let Some(reset) = work.take()
            {
                self.work(reset, now, None);
            }
            for ticket in consumed {
                self.apply(MachineInput::NoticeConsumed { generation: self.shared.generation.clone(), ticket }, None, None, now);
            }
            if let Some((scope, terminal)) = complete {
                self.apply(MachineInput::CompleteTurn { generation: self.shared.generation.clone(), scope, terminal }, None, None, now);
            }
            if matches!(self.machine.state, State::Stopped | State::TransportError) {
                intent = None;
                if let Some(ready) = work.take() {
                    let key = ready.key;
                    drop(ready);
                    if let Ok(mut state) = self.shared.state.lock() {
                        state.outgoing.retain(|out| out.key != key || out.ticket.is_some());
                    }
                }
            }
            let had_work = work.is_some();
            let pre_cancel = match (&intent, &work) {
                (Some(Intent::CancelRequest(id)), Some(Work { ready: Ready::Request(p), .. }))
                    if &p.request_id == id && self.machine.index(id).is_none() =>
                {
                    Some(CloseReason::User)
                }
                (Some(Intent::CancelTurn(scope, reason)), Some(Work { ready: Ready::Request(p), .. }))
                    if self.machine.turn.is_none() && p.turn && p.scope == Scope::Turn(scope.clone()) =>
                {
                    Some(match reason {
                        CancelReason::User => CloseReason::User,
                        CancelReason::Reset => CloseReason::Reset,
                        CancelReason::Deadline => CloseReason::Deadline,
                        CancelReason::Shutdown => CloseReason::Shutdown,
                    })
                }
                _ => None,
            };
            if pre_cancel.is_none()
                && let Some(intent) = intent
            {
                self.control(intent, now);
            }
            if let Some(work) = work {
                self.work(work, now, pre_cancel);
            }
            if pre_cancel.is_some()
                && let Ok(mut state) = self.shared.state.lock()
            {
                state.inflight = None;
            }
            let tick = self.services.clock.now();
            self.expired_turn(tick);
            self.apply(MachineInput::Tick, None, None, tick);
            // A UI try-lock miss must not consume the last repaint and sleep
            // forever with ready notices. Re-arm AFTER releasing worker locks.
            if self.shared.drain_pending.swap(false, Ordering::AcqRel) {
                (self.shared.repaint)();
            }
            if incoming || had_work {
                continue;
            }
            let next = if matches!(self.machine.state, State::Stopped | State::TransportError) {
                None
            } else {
                self.machine
                    .ledger
                    .iter()
                    .filter(|e| e.status == Status::Pending)
                    .map(|e| e.deadline)
                    .chain(self.machine.turn.iter().filter(|t| t.interruption.is_none()).map(|t| t.deadline))
                    .chain(self.machine.stop_deadline)
                    .chain((self.machine.state == State::Starting).then_some(self.machine.startup_deadline))
                    .min()
            };
            let wait = next.map_or(Duration::from_secs(1), |d| d.saturating_duration_since(self.services.clock.now()));
            let _ = self.receive.recv_timeout(wait);
        }
    }
    fn fail(&mut self, now: Instant) {
        self.apply(MachineInput::InputFailed { generation: self.shared.generation.clone() }, None, None, now);
        self.process.shutdown(false);
    }
    fn expired_turn(&mut self, now: Instant) {
        let Some(scope) =
            self.machine.turn.as_ref().filter(|turn| turn.interruption.is_none() && now >= turn.deadline).map(|turn| turn.scope.clone())
        else {
            return;
        };
        let decision = self.shared.state.lock().ok().map(|state| {
            let interrupted = state.turn.as_ref().is_some_and(|turn| turn.reason().is_some());
            (interrupted, state.intent.is_some() || state.inflight.is_some() || state.pending.iter().any(|p| p.control))
        });
        match decision {
            Some((true, _)) => {}
            Some((false, true)) => self.fail(now),
            Some((false, false)) => self.control(Intent::CancelTurn(scope, CancelReason::Deadline), now),
            None => self.shared.force(),
        }
    }
    fn fit_tail(&mut self, tail: Vec<String>) -> Vec<String> {
        if matches!(self.machine.state, State::Stopped | State::TransportError) {
            return Vec::new();
        }
        let encoded = |c: char| match c {
            '"' | '\\' | '\n' | '\r' | '\t' => 2,
            c if c <= '\u{1f}' => 6,
            c => c.len_utf8(),
        };
        let wanted = tail.iter().enumerate().map(|(i, line)| 2 + usize::from(i != 0) + line.chars().map(encoded).sum::<usize>()).sum::<usize>();
        let mut available = wanted.min((self.machine.limits.queue_bytes as usize).saturating_sub(self.shared.incoming.used()));
        while available != 0 {
            match self.shared.incoming.reserve_control(available) {
                Ok(credit) => {
                    self.tail_credit = Some(credit);
                    break;
                }
                Err(ProcessIoError::ResourceExhausted) => {
                    let next = wanted.min((self.machine.limits.queue_bytes as usize).saturating_sub(self.shared.incoming.used()));
                    if next >= available {
                        available = 0;
                        break;
                    }
                    available = next;
                }
                Err(_) => {
                    available = 0;
                    break;
                }
            }
        }
        // Diagnostics, unlike valid service errors, may be clipped. Reserve
        // their real encoded capacity BEFORE conversion so a producer racing
        // EOF cannot steal the final transport completion's storage.
        let mut result = Vec::new();
        let mut used = 0;
        for line in tail.into_iter().take(self.machine.limits.stderr_lines as usize) {
            let overhead = 2 + usize::from(!result.is_empty());
            if overhead > available.saturating_sub(used) {
                break;
            }
            used += overhead;
            let mut clipped = String::new();
            for c in line.chars().take(self.machine.limits.stderr_line_scalars as usize) {
                let n = encoded(c);
                if n > available.saturating_sub(used) {
                    break;
                }
                clipped.push(c);
                used += n;
            }
            result.push(clipped);
        }
        result
    }
    fn internal_submit(&mut self, id: RequestId, scope: Scope, request: HostRequest, now: Instant) {
        let control = matches!(request, HostRequest::TurnCancel(_) | HostRequest::RequestCancel(_) | HostRequest::RuntimeShutdown(_));
        let reserve = self
            .machine
            .completion_reservation(&id, &scope)
            .and_then(|n| if control { self.shared.incoming.reserve_control(n).ok() } else { self.shared.incoming.reserve(n).ok() });
        let Some(credit) = reserve else {
            self.fail(now);
            return;
        };
        if let Ok(mut state) = self.shared.state.lock() {
            let intent = request_intent(&scope, &request);
            state.inflight = None;
            match &intent {
                Some(Intent::CancelTurn(_, _)) => state.cancel_id = Some(id.clone()),
                Some(Intent::Shutdown(_)) => state.shutdown_id = Some(id.clone()),
                _ => {}
            }
            state.pending.push(Pending {
                id: id.clone(),
                scope: scope.clone(),
                control: matches!(request, HostRequest::TurnCancel(_) | HostRequest::RequestCancel(_) | HostRequest::RuntimeShutdown(_)),
                done: false,
                completion: Some(credit),
                intent,
            });
        }
        let deadline = now.checked_add(Duration::from_secs(u64::from(self.machine.limits.call_max_seconds))).unwrap_or(now);
        self.apply(
            MachineInput::Submit { generation: self.shared.generation.clone(), request_id: id.clone(), scope, request, deadline },
            None,
            None,
            now,
        );
        if control && self.machine.index(&id).is_none() && !matches!(self.machine.state, State::Stopped | State::TransportError) {
            self.fail(now);
        }
    }
    fn control(&mut self, intent: Intent, now: Instant) {
        if matches!(&intent,Intent::CancelTurn(scope,_) if self.machine.turn.as_ref().is_none_or(|turn|turn.scope!=*scope)) {
            if let Ok(mut state) = self.shared.state.lock() {
                state.inflight = None;
            }
            return;
        }
        let existing = {
            let mut state = match self.shared.state.lock() {
                Ok(s) => s,
                Err(_) => {
                    self.shared.force();
                    return;
                }
            };
            if let Some(p) = state.pending.iter().find(|p| p.control && p.intent.as_ref() == Some(&intent)) {
                let id = p.id.clone();
                state.inflight = None;
                Some(Ok(id))
            } else if state.pending.iter().any(|p| p.control) {
                state.inflight = None;
                Some(Err(()))
            } else {
                None
            }
        };
        match existing {
            Some(Ok(_)) => return,
            Some(Err(())) => {
                self.fail(now);
                return;
            }
            None => {}
        }
        let (scope, request) = match intent {
            Intent::CancelTurn(scope, reason) => (Scope::Turn(scope), HostRequest::TurnCancel(TurnCancelParams { reason })),
            Intent::CancelRequest(target_request_id) => {
                let Some(scope) = self.machine.ledger.iter().find(|e| e.id == target_request_id).map(|e| e.scope.clone()) else {
                    self.fail(now);
                    return;
                };
                (scope, HostRequest::RequestCancel(RequestCancelParams { target_request_id }))
            }
            Intent::Shutdown(reason) => (Scope::Runtime, HostRequest::RuntimeShutdown(RuntimeShutdownParams { reason })),
        };
        match self.request_id() {
            Ok(id) => self.internal_submit(id, scope, request, now),
            Err(e) => {
                self.internal_failure = e.code == ErrorCode::InternalError;
                self.fail(now);
            }
        }
    }
    fn work(&mut self, work: Work, now: Instant, pre_cancel: Option<CloseReason>) {
        let key = work.key;
        // Inner returns only after every untransferred ready payload is really
        // dropped, including parse/failure exits. Never leak an unsent lease.
        self.work_inner(work, now, pre_cancel);
        if let Ok(mut state) = self.shared.state.lock() {
            state.outgoing.retain(|out| out.key != key || out.ticket.is_some());
        }
    }
    fn work_inner(&mut self, work: Work, now: Instant, pre_cancel: Option<CloseReason>) {
        let Work { key, ready } = work;
        let (input, expected) = match ready {
            Ready::Request(p) => {
                let decoded = decode_frame(
                    &p.bytes,
                    if matches!(self.machine.state, State::Starting) { DecodePhase::Bootstrap } else { DecodePhase::Negotiated },
                    Sender::Host,
                    &self.machine.limits,
                );
                let id = p.request_id.clone();
                let DecodeResult::Frame(Frame::Request { body: RequestBody::Host(request), .. }) = (match decoded {
                    Ok(d) => d,
                    Err(_) => {
                        self.fail(now);
                        return;
                    }
                }) else {
                    self.fail(now);
                    return;
                };
                drop(p.bytes);
                let revoked = if p.turn && self.machine.retired_scope(&p.scope) {
                    Some(CloseReason::Reset)
                } else if p.turn && self.machine.turn.is_none() {
                    self.shared
                        .state
                        .lock()
                        .ok()
                        .and_then(|state| state.turn.as_ref().filter(|t| p.scope == Scope::Turn(t.scope.clone())).and_then(|t| t.reason()))
                } else {
                    None
                };
                let input = if let Some(reason) = revoked.or(pre_cancel) {
                    if p.turn
                        && let Ok(state) = self.shared.state.lock()
                        && let Some(t) = &state.turn
                    {
                        t.local_only.store(true, Ordering::Release);
                    }
                    MachineInput::CancelQueued {
                        generation: self.shared.generation.clone(),
                        request_id: id.clone(),
                        scope: p.scope,
                        request,
                        deadline: p.deadline,
                        reason,
                    }
                } else {
                    MachineInput::Submit {
                        generation: self.shared.generation.clone(),
                        request_id: id.clone(),
                        scope: p.scope,
                        request,
                        deadline: p.deadline,
                    }
                };
                (input, Some(id))
            }
            Ready::Event(p) => {
                let open = !self.shared.stopped_admission.load(Ordering::Acquire)
                    && self.machine.current_scope(&p.scope)
                    && self.shared.state.lock().is_ok_and(|state| state.scope_open(&p.scope));
                if !open {
                    drop(p);
                    if let Ok(mut state) = self.shared.state.lock() {
                        state.outgoing.retain(|o| o.key != key);
                    }
                    return;
                }
                let decoded = decode_frame(&p.bytes, DecodePhase::Negotiated, Sender::Host, &self.machine.limits);
                drop(p.bytes);
                let Ok(DecodeResult::Frame(Frame::Event { header, body: EventBody::Host(event) })) = decoded else {
                    self.fail(now);
                    return;
                };
                let cause = header
                    .request_id
                    .0
                    .and_then(|id| RequestId::parse(id.as_str(), if id.as_str().starts_with("h:") { Sender::Host } else { Sender::Runtime }).ok());
                (MachineInput::Emit { generation: self.shared.generation.clone(), scope: p.scope, event, cause }, None)
            }
            Ready::Reply { token, bytes } => {
                let decoded = decode_frame(&bytes, DecodePhase::Negotiated, Sender::Host, &self.machine.limits);
                drop(bytes);
                let result = match decoded {
                    Ok(DecodeResult::Frame(Frame::Response { body: ResponseBody::Ok(result), .. })) => {
                        result.decode_host(token.method, &self.machine.limits).map_err(|_| error(ErrorCode::InvalidRequest))
                    }
                    Ok(DecodeResult::Frame(Frame::Response { body: ResponseBody::Error(error), .. })) => Err(error),
                    _ => {
                        self.fail(now);
                        return;
                    }
                };
                let id = token.request_id.clone();
                (MachineInput::CallbackFinished { token, result: Box::new(result) }, Some(id))
            }
            Ready::Progress { token, bytes } => {
                if self.machine.callback_index(&token).is_none()
                    || self
                        .machine
                        .ledger
                        .iter()
                        .find(|entry| entry.id == token.request_id)
                        .and_then(|entry| entry.cancellation.as_ref())
                        .is_some_and(Cancellation::is_cancelled)
                {
                    drop(bytes);
                    drop(token);
                    return;
                }
                let decoded = decode_frame(&bytes, DecodePhase::Negotiated, Sender::Host, &self.machine.limits);
                drop(bytes);
                let progress = match decoded {
                    Ok(DecodeResult::Frame(Frame::Event { body: EventBody::Host(HostEvent::ModelDelta(p)), .. })) => CallProgress::ModelDelta(p),
                    Ok(DecodeResult::Frame(Frame::Event { body: EventBody::Host(HostEvent::AgentProgress(p)), .. })) => {
                        CallProgress::AgentProgress(p)
                    }
                    _ => {
                        self.fail(now);
                        return;
                    }
                };
                (MachineInput::Progress { token, event: progress }, None)
            }
            Ready::Finished { token } => (MachineInput::CallbackFinished { token, result: Box::new(Err(error(ErrorCode::Cancelled))) }, None),
        };
        self.apply(input, (key != 0).then_some((key, expected)), None, self.services.clock.now());
    }
    fn snapshot(&mut self) -> bool {
        let mut state = match self.shared.state.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let live = !matches!(self.machine.state, State::Stopped | State::TransportError);
        let limits_changed = state.limits != self.machine.limits && live;
        if limits_changed
            && self.shared.incoming.lower(self.machine.limits.queue_bytes as usize, self.machine.limits.control_lane_bytes as usize).is_err()
        {
            return false;
        }
        // A successful hello must constrain the SAME host pool even when it
        // accepts an already-lowered offer verbatim. The launch offer alone is
        // not negotiation, and byte/epoch equality cannot waive this check.
        if self.machine.info.is_some() && live {
            let pool_limit = match self.services.admission.0.lock() {
                Ok(pool) => pool.limit,
                Err(_) => return false,
            };
            if self.machine.limits.runtime_instances < pool_limit
                && self.services.admission.lower_limit(self.machine.limits.runtime_instances).is_err()
            {
                return false;
            }
        }
        if limits_changed {
            state.limits = self.machine.limits.clone();
            state.epoch += 1;
        }
        state.ready = self.machine.state == State::Ready;
        state.closed = matches!(self.machine.state, State::Stopped | State::TransportError);
        state.session = self.machine.session.as_ref().map(|s| s.id.clone());
        if state.session.as_ref().is_some_and(|s| state.retiring.as_ref().is_some_and(|old| s != old)) {
            state.retiring = None;
        }
        if let Some(turn) = &self.machine.turn {
            if state.turn.as_ref().is_none_or(|t| t.scope != turn.scope) {
                state.cancel_id = None;
                state.turn = Some(Arc::new(ScopeFlags {
                    scope: turn.scope.clone(),
                    admission: AtomicBool::new(!turn.closed),
                    cancelled: Arc::new(AtomicBool::new(turn.interruption.is_some())),
                    bound: AtomicBool::new(true),
                    reason: AtomicU8::new(0),
                    local_only: AtomicBool::new(false),
                }));
            }
            if let Some(t) = &state.turn {
                t.bound.store(true, Ordering::Release);
                if let Some(reason) = turn.interruption {
                    t.record(reason);
                }
            }
            if turn.closed {
                state.revoke(Some(&turn.scope), turn.interruption.is_some());
            }
        } else if state.turn.as_ref().is_some_and(|t| t.bound.load(Ordering::Acquire)) {
            state.turn = None;
            state.cancel_id = None;
        }
        if state.closed {
            state.revoke(None, true);
            state.intent = None;
            state.inflight = None;
            state.complete = None;
            self.shared.stopped_admission.store(true, Ordering::Release);
            self.shared.cancelled.store(true, Ordering::Release);
            let work = std::mem::take(&mut state.data);
            let controls = std::mem::take(&mut state.control_work);
            let keys: Vec<_> = work.iter().chain(controls.iter()).map(|w| w.key).collect();
            drop(work);
            drop(controls);
            state.outgoing.retain(|out| !keys.contains(&out.key));
        }
        true
    }
    fn discard_unpublished(&mut self, actions: Vec<MachineAction>) {
        for action in actions {
            if let MachineAction::Notice { ticket, notice } = action {
                self.machine.notices.retain(|lease| lease.ticket != ticket);
                if let ClientNotice::RequestFinished { request_id, .. } = notice.as_ref() {
                    let reserved = self.shared.state.lock().ok().and_then(|state| {
                        state.pending.iter().find(|p| p.id == *request_id && !p.done).and_then(|p| p.completion.as_ref().map(InboundCredit::bytes))
                    });
                    if let Some(bytes) = reserved
                        && let Some(entry) = self.machine.ledger.iter_mut().find(|e| e.id == *request_id)
                    {
                        // The typed peer result was never handed off. Restore
                        // only its ORIGINAL pre-reserved local completion cell;
                        // fatal cleanup will resolve it once, not lose it.
                        entry.status = Status::Pending;
                        entry.held_bytes = bytes;
                    }
                }
                drop(notice);
            }
            // Other owned action payloads die here, before their caller drops
            // the corresponding inbound credit. No successful Written is forged.
        }
    }
    fn apply(&mut self, input: MachineInput, source: Option<(u64, Option<RequestId>)>, credit: Option<InboundCredit>, now: Instant) {
        let progress_input = matches!(&input, MachineInput::Progress { .. });
        let reset_input = matches!(&input, MachineInput::Submit { request: HostRequest::SessionReset(_), .. });
        let failure_input = matches!(&input, MachineInput::InputFailed { .. } | MachineInput::WriteFailed { .. } | MachineInput::Eof { .. });
        if !reset_input {
            let reset = self.shared.state.lock().ok().and_then(|mut state| {
                state.data.iter().position(|work| matches!(&work.ready,Ready::Request(p) if p.reset)).and_then(|index| state.data.remove(index))
            });
            if let Some(reset) = reset {
                self.work(reset, now, None);
            }
        }
        let route = match &input {
            MachineInput::Peer { decoded: DecodeResult::Frame(Frame::Request { header, .. }), .. } => {
                InboundRoute::Callback(header.request_id.clone())
            }
            MachineInput::Peer { decoded: DecodeResult::Frame(Frame::Response { header, .. }), .. } => {
                InboundRoute::Response(header.request_id.clone())
            }
            MachineInput::Peer {
                decoded: DecodeResult::Frame(Frame::Event { body: EventBody::Runtime(RuntimeEvent::RuntimeReady(_)), .. }), ..
            } => InboundRoute::Ready,
            MachineInput::Peer {
                decoded: DecodeResult::Frame(Frame::Event { header, body: EventBody::Runtime(RuntimeEvent::TurnFinished(_)) }),
                ..
            } => match &header.scope {
                Scope::Turn(s) => InboundRoute::Proposal(s.clone()),
                _ => InboundRoute::None,
            },
            MachineInput::Peer { decoded: DecodeResult::Frame(Frame::Event { header, .. }), .. } => InboundRoute::Event(header.scope.clone()),
            _ => InboundRoute::None,
        };
        let was_info = self.machine.info.is_some();
        let was_proposal = self.machine.turn.as_ref().is_some_and(|t| t.proposal.is_some());
        let urgent = reason_from(self.shared.urgent_reason.load(Ordering::Acquire));
        let local_reason =
            self.shared.state.lock().ok().and_then(|s| s.turn.as_ref().and_then(|t| t.reason().or(urgent).map(|r| (t.scope.clone(), r))));
        let mut actions = Vec::new();
        if let Some((scope, reason)) = local_reason
            && self.machine.turn.as_ref().is_some_and(|t| t.scope == scope && t.interruption.is_none())
            && (reason != CloseReason::Reset || reset_input || failure_input)
        {
            self.machine.close_turn(reason, &mut actions);
        }
        if reset_input && self.machine.turn.is_none() {
            let queued = self.shared.state.lock().ok().and_then(|state| {
                state.turn.as_ref().filter(|turn| !turn.bound.load(Ordering::Acquire)).map(|turn| {
                    turn.bound.store(true, Ordering::Release);
                    (turn.scope.clone(), turn.reason().unwrap_or(CloseReason::Reset))
                })
            });
            if let Some((scope, reason)) = queued {
                actions.push(MachineAction::CloseTurn { scope: scope.clone(), reason });
                self.machine.terminal_notice(ClientNotice::TurnClosed { scope, cause: reason }, self.machine.closure_reserve, &mut actions);
            }
        }
        actions.extend(self.machine.apply(input, now));
        if matches!(self.machine.state, State::Stopped | State::TransportError) {
            let (pending, unbound) = match self.shared.state.lock() {
                Ok(state) => (
                    state
                        .pending
                        .iter()
                        .filter(|p| !p.done && self.machine.index(&p.id).is_none())
                        .map(|p| (p.id.clone(), p.scope.clone(), p.control, p.completion.as_ref().map_or(0, InboundCredit::bytes)))
                        .collect::<Vec<_>>(),
                    state.turn.as_ref().filter(|t| !t.bound.load(Ordering::Acquire)).map(|t| {
                        t.bound.store(true, Ordering::Release);
                        (t.scope.clone(), t.reason().or(urgent).unwrap_or(CloseReason::TransportLoss))
                    }),
                ),
                Err(_) => (Vec::new(), None),
            };
            if let Some((scope, reason)) = unbound {
                actions.insert(0, MachineAction::CloseTurn { scope: scope.clone(), reason });
                self.machine.terminal_notice(ClientNotice::TurnClosed { scope, cause: reason }, self.machine.closure_reserve, &mut actions);
            }
            for (id, scope, control, bytes) in pending {
                // These accepted-but-not-yet-machine-bound calls already own a
                // bounded completion cell in the SAME native receive pool.
                if let Some(ticket) = self.machine.lease() {
                    self.machine.notices.push(Lease { ticket, bytes, control, completion: true, request_control: control });
                    actions.push(MachineAction::Notice {
                        ticket,
                        notice: Box::new(ClientNotice::RequestFinished { request_id: id, scope, result: Err(error(ErrorCode::TransportError)) }),
                    });
                }
            }
            // All trusted hooks precede all notices; global close follows local
            // completions, including accepted calls that never entered Pipes.
            actions.sort_by_key(|a| match a {
                MachineAction::CloseTurn { .. } | MachineAction::RetireSession(_) => 0,
                MachineAction::Notice { notice, .. } if matches!(notice.as_ref(), ClientNotice::TransportClosed { .. }) => 2,
                MachineAction::StopTree => 3,
                _ => 1,
            });
        }
        // Synchronous UI closure can contribute a trusted CloseTurn prefix to
        // a later reset's machine actions. RetireSession must still precede
        // EVERY following notice/send/dispatch, not only fatal-path notices.
        actions.sort_by_key(|a| u8::from(!matches!(a, MachineAction::CloseTurn { .. } | MachineAction::RetireSession(_))));
        if !was_info && self.machine.info.is_some() {
            match self.shared.incoming.reserve(self.machine.info_bytes) {
                Ok(c) => self.retained_info = Some(c),
                Err(_) => {
                    self.machine.info = None;
                    self.discard_unpublished(actions);
                    drop(credit);
                    self.fail(now);
                    return;
                }
            }
        }
        if !was_proposal && self.machine.turn.as_ref().is_some_and(|t| t.proposal.is_some()) {
            let bytes = credit.as_ref().map_or(0, InboundCredit::bytes);
            match self.shared.incoming.reserve(bytes) {
                Ok(c) => self.retained_proposal = Some(c),
                Err(_) => {
                    if let Some(t) = &mut self.machine.turn {
                        t.proposal = None;
                    }
                    self.discard_unpublished(actions);
                    drop(credit);
                    self.fail(now);
                    return;
                }
            }
        }
        if self.machine.turn.is_none() {
            self.retained_proposal = None;
        }
        if !self.snapshot() {
            self.discard_unpublished(actions);
            drop(credit);
            self.fail(now);
            return;
        }
        let mut actions: VecDeque<_> = actions.into();
        let mut credit = credit;
        let mut source = source;
        while let Some(action) = actions.pop_front() {
            match action {
                MachineAction::CloseTurn { scope, reason } => {
                    if reason != CloseReason::TerminalProposal || self.machine.turn.as_ref().is_none_or(|turn| turn.interruption.is_none()) {
                        self.services.lifecycle.close_turn(&scope, reason);
                    }
                }
                MachineAction::RetireSession(id) => self.services.lifecycle.retire_session(&id),
                MachineAction::Send { ticket, bytes, lane } => {
                    if matches!(self.machine.state, State::Stopped | State::TransportError) {
                        drop(bytes);
                        continue;
                    }
                    let bytes = compact(bytes);
                    let header = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
                    let identity = header.as_ref().and_then(|v| v.get("requestId").and_then(|v| v.as_str()).map(str::to_owned));
                    let response = header.as_ref().is_some_and(|value| value["kind"] == "response");
                    let close_error = header
                        .as_ref()
                        .is_some_and(|value| matches!(value["error"]["code"].as_str(), Some("CANCELLED" | "TIMEOUT" | "TRANSPORT_ERROR")));
                    let retired_reply = response
                        && !close_error
                        && identity.as_ref().and_then(|id| self.machine.ledger.iter().find(|entry| entry.id.as_str() == id)).is_some_and(|entry| {
                            entry.method != "request.cancel"
                                && entry.id.as_str().starts_with("r:")
                                && (!self.machine.current_scope(&entry.scope) || entry.cancellation.as_ref().is_some_and(Cancellation::is_cancelled))
                        });
                    if retired_reply {
                        drop(header);
                        drop(bytes);
                        self.machine.outgoing.retain(|lease| lease.ticket != ticket);
                        for entry in &mut self.machine.ledger {
                            if entry.reply_ticket == Some(ticket) {
                                entry.reply_ticket = None;
                            }
                        }
                        continue;
                    }
                    let existing = source
                        .as_ref()
                        .filter(|(_, id)| match id {
                            Some(id) => identity.as_deref() == Some(id.as_str()),
                            None => header.as_ref().is_some_and(|v| v["kind"] == "event"),
                        })
                        .map(|(key, _)| *key);
                    drop(header);
                    let accepted = match self.shared.state.lock() {
                        Ok(mut state) => {
                            let reserved = if response {
                                identity.as_ref().and_then(|id| {
                                    state
                                        .outgoing
                                        .iter()
                                        .find(|out| out.callback.as_ref().is_some_and(|callback| callback.as_str() == id))
                                        .map(|out| out.key)
                                })
                            } else {
                                None
                            };
                            if reserved.is_some() {
                                if let Some(life) = state
                                    .callbacks
                                    .iter()
                                    .filter_map(Weak::upgrade)
                                    .find(|life| identity.as_deref() == Some(life.request_id.as_str()))
                                {
                                    let (fallback, poisoned) = life.take_fallback();
                                    if let Some(cell) = fallback {
                                        drop(cell.bytes);
                                    }
                                    if poisoned {
                                        self.shared.force();
                                    }
                                }
                                if let Some(index) = state.data.iter().position(
                                    |work| matches!(&work.ready,Ready::Reply {token,..} if identity.as_deref()==Some(token.request_id.as_str())),
                                ) && let Some(work) = state.data.remove(index)
                                    && let Ready::Reply { token, bytes } = work.ready
                                {
                                    drop(bytes);
                                    state.data.push_front(Work { key: work.key, ready: Ready::Finished { token } });
                                }
                                let mut retired = Vec::new();
                                state.data.retain(|work| {
                                    let discard =
                                        matches!(&work.ready,Ready::Progress {token,..} if identity.as_deref()==Some(token.request_id.as_str()));
                                    if discard {
                                        retired.push(work.key);
                                    }
                                    !discard
                                });
                                state.outgoing.retain(|out| !retired.contains(&out.key));
                            }
                            let key = existing.or(reserved).or_else(|| state.reserve_out(bytes.len(), lane == PipeLane::Control).ok());
                            key.is_some_and(|key| state.assign(key, ticket, bytes.len()))
                        }
                        Err(_) => false,
                    };
                    if existing.is_some() {
                        source = None;
                    }
                    if !accepted {
                        drop(bytes);
                        self.fail(now);
                        return;
                    }
                    if identity
                        .as_ref()
                        .is_some_and(|id| self.machine.ledger.iter().any(|e| e.id.as_str() == id && matches!(e.operation, Operation::Shutdown)))
                    {
                        self.shutdown_ticket = Some(ticket);
                    }
                    if self.process.try_send_tracked(ticket, bytes, lane).is_err() {
                        self.fail(now);
                        return;
                    }
                }
                MachineAction::Dispatch(mut call) => {
                    if !matches!(&route,InboundRoute::Callback(id) if id==&call.token.request_id) {
                        drop(call);
                        self.fail(now);
                        return;
                    }
                    call.token.credit = credit.take();
                    if call.token.credit.is_none() {
                        drop(call);
                        self.fail(now);
                        return;
                    }
                    let turn = {
                        self.shared
                            .state
                            .lock()
                            .ok()
                            .and_then(|s| s.turn.as_ref().filter(|t| call.scope == Scope::Turn(t.scope.clone())).map(|t| t.cancelled.clone()))
                    };
                    let mut parents = vec![self.shared.cancelled.clone()];
                    if let Some(turn) = turn {
                        parents.push(turn);
                    }
                    let _ = call.cancellation.1.set(parents);
                    let life = Arc::new(CallbackLife {
                        scope: call.scope.clone(),
                        request_id: call.token.request_id.clone(),
                        cancellation: call.cancellation.clone(),
                        live: AtomicBool::new(true),
                        finished: AtomicBool::new(false),
                        streaming: matches!(&call.call,HostCall::ModelGenerate(p) if p.options.delivery==Delivery::Streaming),
                        fallback: Mutex::new(None),
                        credit: call.token.credit.clone(),
                    });
                    let frame = Frame::Response {
                        header: ResponseHeader {
                            protocol_version: "1.0".into(),
                            request_id: call.token.request_id.clone(),
                            scope: call.scope.clone(),
                            extensions: None,
                        },
                        body: ResponseBody::Error(error(ErrorCode::InternalError)),
                    };
                    let fallback = match encode_frame(&frame, &self.machine.limits) {
                        Ok(b) => b,
                        Err(_) => {
                            drop(call);
                            self.fail(now);
                            return;
                        }
                    };
                    let fallback = compact(fallback);
                    let mut reservation = fallback.len();
                    for code in [ErrorCode::Cancelled, ErrorCode::Timeout, ErrorCode::TransportError] {
                        let f = Frame::Response {
                            header: ResponseHeader {
                                protocol_version: "1.0".into(),
                                request_id: call.token.request_id.clone(),
                                scope: call.scope.clone(),
                                extensions: None,
                            },
                            body: ResponseBody::Error(error(code)),
                        };
                        if let Ok(bytes) = encode_frame(&f, &self.machine.limits) {
                            reservation = reservation.max(bytes.len());
                        } else {
                            drop(call);
                            self.fail(now);
                            return;
                        }
                    }
                    let key = {
                        let mut state = match self.shared.state.lock() {
                            Ok(s) => s,
                            Err(_) => {
                                drop(call);
                                self.shared.force();
                                return;
                            }
                        };
                        match state.reserve_callback(reservation, call.token.request_id.clone()) {
                            Ok(key) => {
                                state.callbacks.retain(|w| w.upgrade().is_some_and(|l| !l.finished.load(Ordering::Acquire)));
                                state.callbacks.push(Arc::downgrade(&life));
                                key
                            }
                            Err(_) => {
                                drop(state);
                                drop(call);
                                self.fail(now);
                                return;
                            }
                        }
                    };
                    match life.fallback.lock() {
                        Ok(mut cell) => *cell = Some(Fallback { bytes: fallback, key }),
                        Err(_) => {
                            drop(call);
                            self.shared.force();
                            return;
                        }
                    }
                    let token = call.token.clone();
                    let port = ReplyPort { shared: self.shared.clone(), life, limits: call.limits.clone(), token: Some(token) };
                    if call.cancellation.is_cancelled()
                        || self.shared.stopped_admission.load(Ordering::Acquire)
                        || !port.life.live.load(Ordering::Acquire)
                    {
                        drop(call);
                        drop(port);
                        continue;
                    }
                    // No pipe, mailbox, machine or document lock is held here.
                    self.services.dispatch.dispatch(*call, port);
                }
                MachineAction::Notice { ticket, mut notice } => {
                    let revoked = match notice.as_ref() {
                        ClientNotice::Event { scope, .. } => self.shared.state.lock().is_ok_and(|state| !state.scope_open(scope)),
                        ClientNotice::TurnFinishedProposal { scope, .. } => self
                            .shared
                            .state
                            .lock()
                            .is_ok_and(|state| state.turn.as_ref().is_none_or(|turn| turn.scope != *scope || turn.reason().is_some())),
                        _ => false,
                    };
                    if revoked {
                        drop(notice);
                        self.machine.notices.retain(|lease| lease.ticket != ticket);
                        drop(credit.take());
                        continue;
                    }
                    if self.internal_failure
                        && let ClientNotice::TransportClosed { cause, .. } = notice.as_mut()
                    {
                        *cause = TransportCause::Error(error(ErrorCode::InternalError));
                    }
                    let bytes = self.machine.notices.iter().find(|l| l.ticket == ticket).map_or(0, |l| l.bytes);
                    let mut credits = Vec::new();
                    let mut state = match self.shared.state.lock() {
                        Ok(s) => s,
                        Err(_) => {
                            drop(notice);
                            self.shared.force();
                            return;
                        }
                    };
                    match notice.as_ref() {
                        ClientNotice::RequestFinished { request_id, .. } => {
                            if let Some(p) = state.pending.iter_mut().find(|p| &p.id == request_id) {
                                p.done = true;
                                if let Some(c) = p.completion.take() {
                                    credits.push(c);
                                }
                            }
                        }
                        ClientNotice::TransportClosed { .. } => {
                            if let Some(c) = state.transport_credit.take() {
                                credits.push(c);
                            }
                            if let Some(c) = self.tail_credit.take() {
                                credits.push(c);
                            }
                        }
                        ClientNotice::TurnClosed { .. } => {
                            if let Some(c) = state.close_credit.take() {
                                credits.push(c);
                            }
                        }
                        _ => {}
                    }
                    let primary = match (&route, notice.as_ref()) {
                        (InboundRoute::Response(id), ClientNotice::RequestFinished { request_id, .. }) => id == request_id,
                        (InboundRoute::Ready, ClientNotice::RuntimeReady(_)) => true,
                        (InboundRoute::Proposal(a), ClientNotice::TurnFinishedProposal { scope: b, .. }) => a == b,
                        (InboundRoute::Event(a), ClientNotice::Event { scope: b, .. }) => a == b,
                        _ => false,
                    };
                    if primary && let Some(c) = credit.take() {
                        credits.push(c);
                    }
                    let held = credits.iter().map(InboundCredit::bytes).sum::<usize>();
                    if bytes > held {
                        let control = self.machine.notices.iter().find(|l| l.ticket == ticket).is_some_and(|l| l.control);
                        let extra =
                            if control { self.shared.incoming.reserve_control(bytes - held) } else { self.shared.incoming.reserve(bytes - held) };
                        match extra {
                            Ok(c) => credits.push(c),
                            Err(_) => {
                                drop(state);
                                drop(notice);
                                drop(credits);
                                self.fail(now);
                                return;
                            }
                        }
                    }
                    state.notices.push_back(NoticeCell { ticket, notice: *notice, bytes, credits });
                    drop(state);
                    (self.shared.repaint)();
                }
                MachineAction::Rejected { request_id, error } => {
                    if let Some(id) = request_id {
                        let mut state = match self.shared.state.lock() {
                            Ok(s) => s,
                            Err(_) => {
                                self.shared.force();
                                return;
                            }
                        };
                        if let Some(p) = state.pending.iter_mut().find(|p| p.id == id && !p.done) {
                            p.done = true;
                            let scope = p.scope.clone();
                            let credit = p.completion.take();
                            let bytes = credit.as_ref().map_or(0, InboundCredit::bytes);
                            if state.turn.as_ref().is_some_and(|t| !t.bound.load(Ordering::Acquire) && scope == Scope::Turn(t.scope.clone())) {
                                state.turn = None;
                            }
                            state.notices.push_back(NoticeCell {
                                ticket: 0,
                                notice: ClientNotice::RequestFinished { request_id: id, scope, result: Err(error) },
                                bytes,
                                credits: credit.into_iter().collect(),
                            });
                        }
                        drop(state);
                        (self.shared.repaint)();
                    } else if !(progress_input && error.code == ErrorCode::InvalidRequest)
                        && !matches!(self.machine.state, State::Stopped | State::TransportError)
                    {
                        self.fail(now);
                        return;
                    }
                }
                MachineAction::Coalesced { request_id, .. } => {
                    if let Ok(mut state) = self.shared.state.lock() {
                        state.pending.retain(|p| p.id != request_id);
                    }
                }
                MachineAction::StopTree => {
                    // The machine's five-second cancel boundary already chose
                    // physical fallback. Do not add a second shutdown RPC/window.
                    self.process.shutdown(false);
                }
            }
        }
    }
}
