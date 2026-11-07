//! The agent as a child process (desktop only): its stdout is read on a thread and handed to the
//! panel line by line, its stdin written on another so a slow agent never blocks a frame.
//!
//! Agents start helpers of their own (Node adapters start the CLI they wrap, which starts the MCP
//! servers it was given), so stopping only the program we started would leave those running. The
//! agent therefore gets a process group of its own on Unix (on Windows, `taskkill /T` walks its
//! tree), and [`AgentProcess::shutdown`] ends the whole group: stdin is closed first (an ACP agent
//! ends at end of input), then the group is asked to stop, then stopped. Everything is addressed
//! by the ids of the processes we started, never by name.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::{Duration, Instant};

#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;

const GRACE: Duration = Duration::from_millis(1200);
const TEARDOWN: Duration = Duration::from_secs(3);
const SLOTS: usize = 4096;
const ESSENTIALS: &[&str] = &["SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR"];

pub struct ProcessLaunch {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy)]
pub struct ProcessIoLimits {
    pub frame_bytes: usize,
    pub queue_bytes: usize,
    pub control_lane_bytes: usize,
    pub stderr_lines: usize,
    pub stderr_line_scalars: usize,
    pub stderr_bytes: usize,
}

impl ProcessIoLimits {
    fn validate(self) -> Result<Self, ProcessIoError> {
        if self.frame_bytes == 0
            || self.frame_bytes > 8 << 20
            || self.queue_bytes > 16 << 20
            || self.control_lane_bytes == 0
            || self.control_lane_bytes > 65536
            || !self.queue_bytes.checked_sub(self.control_lane_bytes).is_some_and(|data| self.frame_bytes <= data)
            || self.stderr_lines == 0
            || self.stderr_lines > 40
            || self.stderr_line_scalars == 0
            || self.stderr_line_scalars > 400
            || self.stderr_bytes == 0
            || self.stderr_bytes > 65536
        {
            return Err(ProcessIoError::InvalidLaunch);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeLane {
    Data,
    Control,
}

/// Fixed, redacted categories: never include executable paths, payloads or OS
/// error strings in a transport diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessIoError {
    InvalidLaunch,
    ResourceExhausted,
    OversizedFrame,
    TruncatedFrame,
    ReadFailed,
    WriteFailed,
    ThreadStartFailed,
    CleanupFailed,
    Closed,
}

impl std::fmt::Display for ProcessIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "process {self:?}")
    }
}
impl std::error::Error for ProcessIoError {}

#[derive(Debug)]
pub enum ProcessEvent {
    Frame(InboundFrame),
    Written(u64),
    IoFailed(ProcessIoError),
    Exited(Vec<String>),
}

pub enum FromAgent {
    Line(String),
    Exited(Vec<String>),
}

pub struct AgentProcess {
    child: ProcessHandle,
    to_agent: Option<Arc<Pipes>>,
    pub from_agent: Receiver<FromAgent>,
}

pub struct StrictProcess {
    child: ProcessHandle,
    io: Arc<Pipes>,
}

struct PendingWrite {
    bytes: Vec<u8>,
    lane: PipeLane,
    ticket: Option<u64>,
}

/// Logical wire ownership, not an allocator/RSS bound. One process owns one
/// pool. Refund is atomic and never re-enters pipe/mailbox locks or user code.
#[derive(Debug)]
pub(super) struct InboundPool {
    // low 32 bits: bytes; high 32 bits: frames/owned cells.
    used: AtomicU64,
    cap: AtomicU64,
    data: AtomicU64,
    data_cap: AtomicU64,
    admission: Mutex<()>,
}
impl InboundPool {
    fn new(bytes: usize, control: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicU64::new(0),
            cap: AtomicU64::new(bytes as u64),
            data: AtomicU64::new(0),
            data_cap: AtomicU64::new((bytes - control) as u64),
            admission: Mutex::new(()),
        })
    }
    pub(super) fn used(&self) -> usize {
        (self.used.load(Ordering::Acquire) & u64::from(u32::MAX)) as usize
    }
    fn add(&self, bytes: usize, slots: u64) -> Result<(), ProcessIoError> {
        let _guard = self.admission.lock().map_err(|_| ProcessIoError::Closed)?;
        self.add_locked(bytes, slots)
    }
    fn add_locked(&self, bytes: usize, slots: u64) -> Result<(), ProcessIoError> {
        let mut old = self.used.load(Ordering::Acquire);
        loop {
            if (old & u64::from(u32::MAX)) + bytes as u64 > self.cap.load(Ordering::Acquire) || (old >> 32) + slots > SLOTS as u64 {
                return Err(ProcessIoError::ResourceExhausted);
            }
            let next = old + bytes as u64 + (slots << 32);
            match self.used.compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(()),
                Err(actual) => old = actual,
            }
        }
    }
    fn release(&self, bytes: usize, slots: u64) {
        self.used.fetch_sub(bytes as u64 + (slots << 32), Ordering::AcqRel);
    }
    pub(super) fn lower(&self, bytes: usize, control: usize) -> Result<(), ProcessIoError> {
        let _guard = self.admission.lock().map_err(|_| ProcessIoError::Closed)?;
        if bytes > self.cap.load(Ordering::Acquire) as usize
            || self.used() > bytes
            || control > bytes
            || self.data.load(Ordering::Acquire) > (bytes - control) as u64
        {
            return Err(ProcessIoError::ResourceExhausted);
        }
        self.cap.store(bytes as u64, Ordering::Release);
        self.data_cap.store((bytes - control) as u64, Ordering::Release);
        Ok(())
    }
    pub(super) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<InboundCredit, ProcessIoError> {
        self.reserve_lane(bytes, false)
    }
    pub(super) fn reserve_control(self: &Arc<Self>, bytes: usize) -> Result<InboundCredit, ProcessIoError> {
        self.reserve_lane(bytes, true)
    }
    fn reserve_lane(self: &Arc<Self>, bytes: usize, control: bool) -> Result<InboundCredit, ProcessIoError> {
        let _guard = self.admission.lock().map_err(|_| ProcessIoError::Closed)?;
        self.cell(bytes, control)
    }
    pub(super) fn try_reserve(self: &Arc<Self>, bytes: usize, control: bool) -> Result<InboundCredit, ProcessIoError> {
        let _guard = self.admission.try_lock().map_err(lock_error)?;
        self.cell(bytes, control)
    }
    fn cell(self: &Arc<Self>, bytes: usize, control: bool) -> Result<InboundCredit, ProcessIoError> {
        if !control && self.data.load(Ordering::Acquire) + bytes as u64 > self.data_cap.load(Ordering::Acquire) {
            return Err(ProcessIoError::ResourceExhausted);
        }
        self.add_locked(bytes, 1)?;
        if !control {
            self.data.fetch_add(bytes as u64, Ordering::AcqRel);
        }
        Ok(InboundCredit(Arc::new(InboundOwner { pool: self.clone(), bytes, data: std::sync::atomic::AtomicBool::new(!control) })))
    }
}
#[derive(Debug)]
struct InboundOwner {
    pool: Arc<InboundPool>,
    bytes: usize,
    data: std::sync::atomic::AtomicBool,
}
impl Drop for InboundOwner {
    fn drop(&mut self) {
        if self.data.load(Ordering::Acquire) {
            self.pool.data.fetch_sub(self.bytes as u64, Ordering::AcqRel);
        }
        self.pool.release(self.bytes, 1);
    }
}
#[derive(Clone, Debug)]
pub(super) struct InboundCredit(Arc<InboundOwner>);
impl PartialEq for InboundCredit {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for InboundCredit {}
impl InboundCredit {
    pub(super) fn bytes(&self) -> usize {
        self.0.bytes
    }
    pub(super) fn classify_data(&self) -> Result<(), ProcessIoError> {
        let pool = &self.0.pool;
        let _guard = pool.admission.lock().map_err(|_| ProcessIoError::Closed)?;
        if self.0.data.load(Ordering::Acquire) {
            return Ok(());
        }
        if pool.data.load(Ordering::Acquire) + self.0.bytes as u64 > pool.data_cap.load(Ordering::Acquire) {
            return Err(ProcessIoError::ResourceExhausted);
        }
        pool.data.fetch_add(self.0.bytes as u64, Ordering::AcqRel);
        self.0.data.store(true, Ordering::Release);
        Ok(())
    }
}
#[derive(Debug)]
pub(super) struct InboundFrame {
    // Declaration order: payload BEFORE the last refund guard.
    bytes: Vec<u8>,
    credit: InboundCredit,
}
impl InboundFrame {
    pub(super) fn into_parts(self) -> (Vec<u8>, InboundCredit) {
        (self.bytes, self.credit)
    }
}
impl std::ops::Deref for InboundFrame {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
impl<T: AsRef<[u8]>> PartialEq<T> for InboundFrame {
    fn eq(&self, other: &T) -> bool {
        self.bytes == other.as_ref()
    }
}

struct PipeState {
    data: VecDeque<PendingWrite>,
    control: VecDeque<PendingWrite>,
    data_bytes: usize,
    control_bytes: usize,
    in_flight: usize,
    data_slots: usize,
    control_slots: usize,
    written: VecDeque<(u64, PipeLane)>,
    input_closed: bool,
    incoming: VecDeque<InboundFrame>,
    incoming_bytes: usize,
    stdout_peak: usize,
    stdout_read: usize,
    errors: VecDeque<ProcessIoError>,
    seen_errors: u16,
    stdout_done: bool,
    stderr_done: bool,
    writer_done: bool,
    exit_sent: bool,
    tail: StderrTail,
}

struct Pipes {
    limits: ProcessIoLimits,
    inbound: Arc<InboundPool>,
    io_started: AtomicBool,
    io_workers: AtomicU8,
    receiver_gone: AtomicBool,
    state: Mutex<PipeState>,
    ready: Condvar,
    stop: Arc<Stop>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Pipes {
    fn new(limits: ProcessIoLimits, wake: Arc<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            limits,
            inbound: InboundPool::new(limits.queue_bytes, limits.control_lane_bytes),
            io_started: AtomicBool::new(false),
            io_workers: AtomicU8::new(0),
            receiver_gone: AtomicBool::new(false),
            state: Mutex::new(PipeState {
                data: VecDeque::new(),
                control: VecDeque::new(),
                data_bytes: 0,
                control_bytes: 0,
                in_flight: 0,
                data_slots: 0,
                control_slots: 0,
                written: VecDeque::new(),
                input_closed: false,
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                stdout_peak: 0,
                stdout_read: 0,
                errors: VecDeque::new(),
                seen_errors: 0,
                stdout_done: false,
                stderr_done: false,
                writer_done: false,
                exit_sent: false,
                tail: StderrTail::new(limits),
            }),
            ready: Condvar::new(),
            stop: Arc::new(Stop { origin: Instant::now(), at: AtomicU64::new(u64::MAX) }),
            wake,
        })
    }

    fn close_locked(state: &mut PipeState) {
        state.input_closed = true;
        for write in state.data.drain(..) {
            state.data_bytes = state.data_bytes.saturating_sub(write.bytes.capacity());
            state.data_slots = state.data_slots.saturating_sub(1);
        }
        for write in state.control.drain(..) {
            state.control_bytes = state.control_bytes.saturating_sub(write.bytes.capacity());
            state.control_slots = state.control_slots.saturating_sub(1);
        }
    }
    fn discard_received_locked(state: &mut PipeState) {
        let queued = state.incoming.iter().map(|frame| frame.len()).sum::<usize>();
        state.incoming.clear();
        state.incoming_bytes = state.incoming_bytes.saturating_sub(queued);
        for (_, lane) in state.written.drain(..) {
            match lane {
                PipeLane::Data => state.data_slots = state.data_slots.saturating_sub(1),
                PipeLane::Control => state.control_slots = state.control_slots.saturating_sub(1),
            }
        }
    }

    fn close_input(&self) {
        // UI never waits for the writer or its pipe. If a worker owns the short
        // bookkeeping lock it observes Stop and performs this cancellation.
        if let Ok(mut state) = self.state.try_lock() {
            Self::close_locked(&mut state);
            if self.receiver_gone.load(Ordering::Acquire) {
                Self::discard_received_locked(&mut state);
            }
        }
        self.ready.notify_all();
    }

    fn fail(&self, error: ProcessIoError) {
        self.stop.request();
        {
            // Recover only to publish failure and cancel writes. Poison remains
            // set, so send/recv cannot resume using this transport state.
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(error) => error.into_inner(),
            };
            let bit = 1_u16 << (error as u16);
            if state.seen_errors & bit == 0 {
                state.seen_errors |= bit;
                state.errors.push_back(error);
            }
            Self::close_locked(&mut state);
        }
        self.ready.notify_all();
        (self.wake)();
    }

    fn send(&self, bytes: Vec<u8>, lane: PipeLane) -> Result<(), ProcessIoError> {
        self.enqueue(None, bytes, lane)
    }
    fn send_tracked(&self, ticket: u64, bytes: Vec<u8>, lane: PipeLane) -> Result<(), ProcessIoError> {
        self.enqueue(Some(ticket), bytes, lane)
    }
    fn enqueue(&self, ticket: Option<u64>, bytes: Vec<u8>, lane: PipeLane) -> Result<(), ProcessIoError> {
        if self.stop.requested() {
            return Err(ProcessIoError::Closed);
        }
        if bytes.is_empty()
            || bytes.last() != Some(&b'\n')
            || bytes.len() > self.limits.frame_bytes
            || (lane == PipeLane::Control && bytes.len() > 4096)
        {
            return Err(ProcessIoError::OversizedFrame);
        }
        let charge = bytes.capacity();
        // Strict tracked producers run only on the off-frame runtime worker.
        // Queue admission still never waits for capacity or OS IO. Serialize
        // the bounded bookkeeping instead of turning a stderr/read lock race
        // into a spurious physical write failure. ACP keeps its original try-lock.
        let mut state =
            if ticket.is_some() { self.state.lock().map_err(|_| ProcessIoError::Closed)? } else { self.state.try_lock().map_err(lock_error)? };
        if state.input_closed || self.stop.requested() {
            return Err(ProcessIoError::Closed);
        }
        let data_cap = self.limits.queue_bytes - self.limits.control_lane_bytes;
        if state.data_slots + state.control_slots >= SLOTS
            || (lane == PipeLane::Data && state.data_slots >= SLOTS - 1)
            || charge > self.limits.queue_bytes.saturating_sub(state.data_bytes + state.control_bytes)
            || match lane {
                PipeLane::Data => charge > data_cap.saturating_sub(state.data_bytes),
                PipeLane::Control => charge > self.limits.control_lane_bytes.saturating_sub(state.control_bytes),
            }
        {
            return Err(ProcessIoError::ResourceExhausted);
        }
        match lane {
            PipeLane::Data => {
                state.data_bytes += charge;
                state.data_slots += 1;
                state.data.push_back(PendingWrite { bytes, lane, ticket });
            }
            PipeLane::Control => {
                state.control_bytes += charge;
                state.control_slots += 1;
                state.control.push_back(PendingWrite { bytes, lane, ticket });
            }
        }
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn recv(&self) -> Result<Option<ProcessEvent>, ProcessIoError> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) if self.io_started.load(Ordering::Acquire) && self.io_workers.load(Ordering::Acquire) == 0 => {
                // Recovery ONLY after the original three IO workers destroyed
                // their buffers. Never reopen a poisoned transport or invent
                // Written. Queued raw frames are destroyed before credit drops.
                let mut state = error.into_inner();
                if state.exit_sent {
                    return Ok(None);
                }
                state.data.clear();
                state.control.clear();
                state.incoming.clear();
                state.written.clear();
                state.errors.clear();
                state.data_bytes = 0;
                state.control_bytes = 0;
                state.incoming_bytes = 0;
                state.in_flight = 0;
                state.data_slots = 0;
                state.control_slots = 0;
                state.exit_sent = true;
                return Ok(Some(ProcessEvent::Exited(vec![ProcessIoError::Closed.to_string()])));
            }
            Err(error) => return Err(lock_error(error)),
        };
        if let Some((ticket, lane)) = state.written.pop_front() {
            match lane {
                PipeLane::Data => state.data_slots -= 1,
                PipeLane::Control => state.control_slots -= 1,
            }
            return Ok(Some(ProcessEvent::Written(ticket)));
        }
        // Frames already read precede EOF/errors; a fatal prefix never becomes a frame.
        if let Some(frame) = state.incoming.pop_front() {
            state.incoming_bytes -= frame.len();
            return Ok(Some(ProcessEvent::Frame(frame)));
        }
        if let Some(error) = state.errors.pop_front() {
            return Ok(Some(ProcessEvent::IoFailed(error)));
        }
        if state.stdout_done && state.stderr_done && state.writer_done && !state.exit_sent {
            state.exit_sent = true;
            return Ok(Some(ProcessEvent::Exited(state.tail.snapshot())));
        }
        Ok(None)
    }
}

fn lock_error<T>(error: TryLockError<T>) -> ProcessIoError {
    match error {
        TryLockError::WouldBlock => ProcessIoError::ResourceExhausted,
        TryLockError::Poisoned(_) => ProcessIoError::Closed,
    }
}

/// One timestamp is shared by drop, explicit Stop, EOF and every teardown phase.
struct Stop {
    origin: Instant,
    at: AtomicU64,
}
impl Stop {
    fn request(&self) {
        let nanos = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX - 1);
        let _ = self.at.compare_exchange(u64::MAX, nanos, Ordering::AcqRel, Ordering::Acquire);
    }
    fn requested(&self) -> bool {
        self.at.load(Ordering::Acquire) != u64::MAX
    }
    fn deadline(&self) -> Instant {
        self.origin + Duration::from_nanos(self.at.load(Ordering::Acquire)) + TEARDOWN
    }
}

/// The same owner/reaper is used by the legacy ACP adapter and strict I/O.
/// A failed deadline keeps at most the child and one OS kill-helper in this
/// owner's reaper; the failure stays published, so admission cannot be released.
struct Owner {
    pid: AtomicU32,
    child: Mutex<Option<Child>>,
    io: Arc<Pipes>,
    result: Mutex<Option<Result<(), ProcessIoError>>>,
    done: Condvar,
}
#[derive(Clone)]
pub struct ProcessHandle(Arc<Owner>);

impl ProcessHandle {
    pub fn pid(&self) -> Option<u32> {
        if self.0.io.stop.requested() || self.try_cleanup_result().is_some() { None } else { Some(self.0.pid.load(Ordering::Acquire)) }
    }
    pub fn try_cleanup_result(&self) -> Option<Result<(), ProcessIoError>> {
        match self.0.result.try_lock() {
            Ok(result) => *result,
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(_)) => Some(Err(ProcessIoError::CleanupFailed)),
        }
    }
    /// `false` performs bounded bookkeeping only, never OS I/O or a thread join.
    /// `true` is for off-frame callers / application exit, with the SAME deadline.
    pub fn shutdown(&self, wait: bool) {
        self.0.io.stop.request();
        self.0.io.close_input();
        if wait {
            let deadline = self.0.io.stop.deadline();
            if let Ok(mut result) = self.0.result.lock() {
                while result.is_none() {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        *result = Some(Err(ProcessIoError::CleanupFailed));
                        break;
                    }
                    match self.0.done.wait_timeout(result, remaining) {
                        Ok((next, _)) => result = next,
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

fn detach_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
}

fn pause_until(deadline: Instant) {
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(10)));
}

fn wait_until(child: &mut Child, deadline: Instant) -> Result<bool, ProcessIoError> {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(true),
            Ok(None) if Instant::now() < deadline => pause_until(deadline),
            Ok(None) => return Ok(false),
            Err(_) => return Err(ProcessIoError::CleanupFailed),
        }
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) -> Result<(), ProcessIoError> {
    let pid = i32::try_from(pid).map_err(|_| ProcessIoError::CleanupFailed)?;
    if pid <= 1 {
        return Err(ProcessIoError::CleanupFailed);
    }
    match killpg(Pid::from_raw(pid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(_) => Err(ProcessIoError::CleanupFailed),
    }
}

/// killpg(0) includes zombies. On Linux, bounded /proc inspection distinguishes
/// terminated orphan zombies (init's responsibility) from live owned helpers.
#[cfg(unix)]
fn group_gone(pid: u32, deadline: Instant) -> bool {
    let Ok(pgid) = i32::try_from(pid) else { return false };
    if matches!(killpg(Pid::from_raw(pgid), None), Err(nix::errno::Errno::ESRCH)) {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else { return false };
        for (count, entry) in entries.enumerate() {
            if count >= 65536 || Instant::now() >= deadline {
                return false;
            }
            let Ok(entry) = entry else { return false };
            if !entry.file_name().to_string_lossy().bytes().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let file = match std::fs::File::open(entry.path().join("stat")) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return false,
            };
            let mut stat = String::new();
            if file.take(4096).read_to_string(&mut stat).is_err() {
                return false;
            }
            let Some((_, fields)) = stat.rsplit_once(')') else { return false };
            let mut fields = fields.split_whitespace();
            let state = fields.next();
            let group = fields.nth(1).and_then(|s| s.parse::<u32>().ok());
            if group == Some(pid) && !matches!(state, Some("Z" | "X")) {
                return false;
            }
        }
        true
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = deadline;
        false
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) -> Result<Child, ProcessIoError> {
    use std::os::windows::process::CommandExt;
    let root = std::env::var_os("SYSTEMROOT").ok_or(ProcessIoError::CleanupFailed)?;
    let path = PathBuf::from(root).join("System32").join("taskkill.exe");
    if !path.is_absolute() {
        return Err(ProcessIoError::CleanupFailed);
    }
    let mut cmd = Command::new(path);
    essential_env(&mut cmd);
    cmd.args(["/T", "/F", "/PID", &pid.to_string()])
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ProcessIoError::CleanupFailed)
}

/// Published deadline result and physical tree evidence are separate. Reaping
/// the leader/helper alone never proves that an unobserved tree terminated.
struct TreeCleanup {
    result: Result<(), ProcessIoError>,
    helper: Option<Child>,
    tree_confirmed: bool,
}

impl TreeCleanup {
    fn failed(helper: Option<Child>) -> Self {
        Self { result: Err(ProcessIoError::CleanupFailed), helper, tree_confirmed: false }
    }
}

/// No blocking wait/status, no phase-relative reset, and no final unbounded
/// wait. The caller retains unresolved responsibility as well as unreaped handles.
fn end_tree(child: &mut Child, deadline: Instant) -> TreeCleanup {
    let pid = child.id();
    if Instant::now() >= deadline {
        #[cfg(unix)]
        let _ = signal_group(pid, Signal::SIGKILL);
        let _ = child.kill();
        return TreeCleanup::failed(None);
    }
    let start = deadline - TEARDOWN;
    let polite = wait_until(child, (start + GRACE).min(deadline));
    #[cfg(unix)]
    {
        let term = signal_group(pid, Signal::SIGTERM);
        if !matches!(polite, Ok(true)) {
            let _ = wait_until(child, (start + GRACE + GRACE).min(deadline));
        }
        let kill = signal_group(pid, Signal::SIGKILL);
        if term.is_err() || kill.is_err() || polite.is_err() {
            let _ = child.kill();
        }
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) && group_gone(pid, deadline) {
                return TreeCleanup { result: Ok(()), helper: None, tree_confirmed: true };
            }
            pause_until(deadline);
        }
        TreeCleanup::failed(None)
    }
    #[cfg(windows)]
    {
        // Do not launch taskkill against an already reaped leader's numerical
        // PID, nor start a new helper after the common deadline. The original
        // Child remains owned, but descendants are unconfirmed in these cases.
        if !matches!(polite, Ok(false)) || Instant::now() >= deadline {
            if !matches!(polite, Ok(true)) {
                let _ = child.kill();
                let _ = wait_until(child, deadline);
            }
            return TreeCleanup::failed(None);
        }
        let mut helper = match kill_tree(pid) {
            Ok(helper) => helper,
            Err(_) => {
                let _ = child.kill();
                let _ = wait_until(child, deadline);
                return TreeCleanup::failed(None);
            }
        };
        // Reserve part of the SAME deadline to terminate/reap a stuck taskkill.
        let helper_deadline = deadline - Duration::from_millis(200);
        let helper_ok = wait_until(&mut helper, helper_deadline).is_ok_and(|done| done)
            && helper.try_wait().is_ok_and(|status| status.is_some_and(|s| s.success()));
        if !helper_ok {
            let _ = helper.kill();
            let _ = child.kill();
        }
        let child_done = wait_until(child, deadline).is_ok_and(|done| done);
        let helper_done = wait_until(&mut helper, deadline).is_ok_and(|done| done);
        // A late successful result from THIS owned helper is termination
        // evidence, but cannot rewrite a previously missed deadline as success.
        let tree_confirmed = helper.try_wait().is_ok_and(|status| status.is_some_and(|s| s.success()));
        let result = if helper_ok && child_done && helper_done && tree_confirmed { Ok(()) } else { Err(ProcessIoError::CleanupFailed) };
        TreeCleanup { result, helper: if helper_done { None } else { Some(helper) }, tree_confirmed }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, polite);
        let _ = child.kill();
        let _ = wait_until(child, deadline);
        TreeCleanup::failed(None)
    }
}

/// Shared owner decision. None means there is no observation of tree termination.
fn owner_may_finish(leader_reaped: bool, kill_helper_reaped: bool, tree_gone: Option<bool>) -> bool {
    leader_reaped && kill_helper_reaped && tree_gone == Some(true)
}

fn start_owner(io: Arc<Pipes>) -> Result<(ProcessHandle, SyncSender<()>), String> {
    let owner = Arc::new(Owner { pid: AtomicU32::new(0), child: Mutex::new(None), io, result: Mutex::new(None), done: Condvar::new() });
    // Establish the only reaper BEFORE spawning an OS child. A failed thread
    // start therefore cannot leave an unreaped child or need a fallback manager.
    let (ready, receive) = sync_channel(1);
    let worker = owner.clone();
    std::thread::Builder::new()
        .name("agent-process-owner".into())
        .spawn(move || {
            // Closing the handoff also wakes us when Command::spawn failed.
            let _ = receive.recv();
            let (child, poisoned) = match worker.child.lock() {
                Ok(mut slot) => (slot.take(), false),
                // Recover ONLY to stop/reap this owned handle, never to reopen
                // admission or continue using corrupted synchronization state.
                Err(error) => (error.into_inner().take(), true),
            };
            if poisoned {
                worker.io.fail(ProcessIoError::CleanupFailed);
            }
            let Some(mut child) = child else {
                if poisoned {
                    if let Ok(mut result) = worker.result.lock() {
                        *result = Some(Err(ProcessIoError::CleanupFailed));
                    }
                    worker.done.notify_all();
                }
                return;
            };
            while !worker.io.stop.requested() {
                match child.try_wait() {
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Ok(Some(_)) | Err(_) => worker.io.stop.request(),
                }
            }
            if let Ok(mut state) = worker.io.state.lock() {
                Pipes::close_locked(&mut state);
                if worker.io.receiver_gone.load(Ordering::Acquire) {
                    Pipes::discard_received_locked(&mut state);
                }
            }
            worker.io.ready.notify_all();
            let deadline = worker.io.stop.deadline();
            let TreeCleanup { result: cleanup, mut helper, mut tree_confirmed } = end_tree(&mut child, deadline);
            let result = if poisoned { Err(ProcessIoError::CleanupFailed) } else { cleanup };
            if result.is_err() {
                worker.io.fail(ProcessIoError::CleanupFailed);
            }
            if let Ok(mut published) = worker.result.lock()
                && published.is_none()
            {
                *published = Some(result);
            }
            worker.done.notify_all();
            (worker.io.wake)();
            if result.is_err() {
                // Bounded ownership, not an unbounded queue of detached reaping jobs.
                // Never replace CleanupFailed with success or release a permit here.
                loop {
                    let child_done = matches!(child.try_wait(), Ok(Some(_)));
                    let helper_done = match helper.as_mut() {
                        None => true,
                        Some(helper) => match helper.try_wait() {
                            Ok(Some(status)) => {
                                tree_confirmed |= status.success();
                                true
                            }
                            Ok(None) | Err(_) => false,
                        },
                    };
                    let tree_gone = if tree_confirmed {
                        Some(true)
                    } else {
                        #[cfg(unix)]
                        {
                            Some(group_gone(worker.pid.load(Ordering::Acquire), Instant::now() + Duration::from_millis(100)))
                        }
                        #[cfg(not(unix))]
                        {
                            None
                        }
                    };
                    // With no evidence, THIS owner keeps its bounded unresolved
                    // record even after both handles are reaped. Never retry a
                    // numerical PID kill here: it may now name someone else.
                    if owner_may_finish(child_done, helper_done, tree_gone) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .map_err(|_| ProcessIoError::ThreadStartFailed.to_string())?;
    Ok((ProcessHandle(owner), ready))
}

/// Incremental UTF-8 diagnostics: at most three pending bytes, one capped line,
/// and the capped tail. Oversized/unterminated lines are discarded in place.
struct StderrTail {
    limits: ProcessIoLimits,
    lines: VecDeque<String>,
    current: String,
    scalars: usize,
    bytes: usize,
    pending: Vec<u8>,
}
impl StderrTail {
    fn new(limits: ProcessIoLimits) -> Self {
        Self { limits, lines: VecDeque::new(), current: String::new(), scalars: 0, bytes: 0, pending: Vec::with_capacity(4) }
    }
    fn character(&mut self, c: char) {
        if c == '\n' {
            self.lines.push_back(std::mem::take(&mut self.current));
            self.scalars = 0;
        } else if self.scalars < self.limits.stderr_line_scalars && self.current.len() + c.len_utf8() <= self.limits.stderr_bytes {
            self.current.push(c);
            self.bytes += c.len_utf8();
            self.scalars += 1;
        }
        while self.lines.len() + usize::from(!self.current.is_empty()) > self.limits.stderr_lines || self.bytes > self.limits.stderr_bytes {
            if let Some(line) = self.lines.pop_front() {
                self.bytes -= line.len();
            } else {
                break;
            }
        }
    }
    fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.pending.push(*byte);
            loop {
                match std::str::from_utf8(&self.pending) {
                    Ok(text) => {
                        let c = text.chars().next();
                        self.pending.clear();
                        if let Some(c) = c {
                            self.character(c);
                        }
                        break;
                    }
                    Err(e) => {
                        let Some(len) = e.error_len() else { break };
                        self.pending.drain(..len);
                        self.character('\u{fffd}');
                    }
                }
            }
        }
    }
    fn finish(&mut self) {
        if !self.pending.is_empty() {
            self.pending.clear();
            self.character('\u{fffd}');
        }
    }
    fn snapshot(&self) -> Vec<String> {
        let mut lines: Vec<_> = self.lines.iter().cloned().collect();
        if !self.current.is_empty() {
            lines.push(self.current.clone());
        }
        lines
    }
}

fn read_stdout(mut stdout: impl Read, io: Arc<Pipes>) {
    let mut frame = Vec::new();
    let mut credited = 0;
    let mut block = [0_u8; 8192];
    let outcome = (|| -> Result<(), ProcessIoError> {
        loop {
            if io.receiver_gone.load(Ordering::Acquire) {
                return Err(ProcessIoError::Closed);
            }
            let available = (io.limits.frame_bytes - frame.len()).min(block.len());
            let count = match stdout.read(&mut block[..available]) {
                Ok(0) => return if frame.is_empty() { Ok(()) } else { Err(ProcessIoError::TruncatedFrame) },
                Ok(count) => count,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ProcessIoError::ReadFailed),
            };
            if let Ok(mut state) = io.state.lock() {
                state.stdout_read = state.stdout_read.saturating_add(count);
            } else {
                return Err(ProcessIoError::Closed);
            }
            let mut rest = &block[..count];
            while !rest.is_empty() {
                let size = rest.iter().position(|b| *b == b'\n').map_or(rest.len(), |i| i + 1);
                let (piece, next) = rest.split_at(size);
                if frame.len() + piece.len() > io.limits.frame_bytes {
                    return Err(ProcessIoError::OversizedFrame);
                }
                {
                    io.inbound.add(piece.len(), u64::from(frame.is_empty()))?;
                    credited += piece.len();
                    let mut state = io.state.lock().map_err(|_| ProcessIoError::Closed)?;
                    state.incoming_bytes += piece.len();
                    state.stdout_peak = state.stdout_peak.max(frame.len() + piece.len());
                }
                frame.try_reserve_exact(piece.len()).map_err(|_| ProcessIoError::ResourceExhausted)?;
                frame.extend_from_slice(piece);
                if piece.last() == Some(&b'\n') {
                    let mut state = io.state.lock().map_err(|_| ProcessIoError::Closed)?;
                    if io.receiver_gone.load(Ordering::Acquire) {
                        return Err(ProcessIoError::Closed);
                    }
                    if state.incoming.len() >= SLOTS {
                        return Err(ProcessIoError::ResourceExhausted);
                    }
                    let bytes = std::mem::take(&mut frame);
                    let credit = InboundCredit(Arc::new(InboundOwner {
                        pool: io.inbound.clone(),
                        bytes: bytes.len(),
                        data: std::sync::atomic::AtomicBool::new(false),
                    }));
                    credited = 0;
                    state.incoming.push_back(InboundFrame { bytes, credit });
                    drop(state);
                    (io.wake)();
                } else if frame.len() == io.limits.frame_bytes {
                    return Err(ProcessIoError::OversizedFrame);
                }
                rest = next;
            }
        }
    })();
    if let Err(error) = outcome {
        io.fail(error);
    }
    if let Ok(mut state) = io.state.lock() {
        state.incoming_bytes = state.incoming_bytes.saturating_sub(credited);
        state.stdout_done = true;
    }
    // Destroy the partial payload before its last credit is returned.
    drop(frame);
    if credited != 0 {
        io.inbound.release(credited, 1);
    }
    io.stop.request();
    io.close_input();
    (io.wake)();
}

fn read_stderr(mut stderr: impl Read, io: Arc<Pipes>) {
    let mut block = [0_u8; 8192];
    loop {
        match stderr.read(&mut block) {
            Ok(0) => break,
            Ok(count) => match io.state.lock() {
                Ok(mut state) => state.tail.feed(&block[..count]),
                Err(error) => {
                    // PoisonError owns the guard; release it before fail()
                    // takes this same mutex to close/cancel the transport.
                    drop(error);
                    io.fail(ProcessIoError::Closed);
                    break;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                io.fail(ProcessIoError::ReadFailed);
                break;
            }
        }
    }
    if let Ok(mut state) = io.state.lock() {
        state.tail.finish();
        state.stderr_done = true;
    }
    (io.wake)();
}

fn write_input(mut stdin: impl Write, io: Arc<Pipes>) {
    loop {
        let pending = (|| -> Result<Option<PendingWrite>, ProcessIoError> {
            let mut state = io.state.lock().map_err(|_| ProcessIoError::Closed)?;
            loop {
                if io.stop.requested() {
                    Pipes::close_locked(&mut state);
                }
                if state.input_closed {
                    return Ok(None);
                }
                if let Some(write) = state.control.pop_front().or_else(|| state.data.pop_front()) {
                    state.in_flight = write.bytes.capacity();
                    return Ok(Some(write));
                }
                state = io.ready.wait(state).map_err(|_| ProcessIoError::Closed)?;
            }
        })();
        let write = match pending {
            Ok(Some(write)) => write,
            Ok(None) => break,
            Err(error) => {
                io.fail(error);
                break;
            }
        };
        let result = stdin.write_all(&write.bytes).and_then(|()| stdin.flush());
        let charge = write.bytes.capacity();
        let lane = write.lane;
        let ticket = write.ticket;
        // The actual frame allocation dies before its byte ownership or its
        // completion can be observed by another admission/consumer.
        drop(write);
        if let Ok(mut state) = io.state.lock() {
            match lane {
                PipeLane::Data => state.data_bytes -= charge,
                PipeLane::Control => state.control_bytes -= charge,
            }
            state.in_flight = 0;
            if result.is_ok()
                && let Some(ticket) = ticket
            {
                state.written.push_back((ticket, lane));
            } else {
                match lane {
                    PipeLane::Data => state.data_slots -= 1,
                    PipeLane::Control => state.control_slots -= 1,
                }
            }
        }
        io.ready.notify_all();
        if result.is_ok() && ticket.is_some() {
            (io.wake)();
        }
        if result.is_err() {
            io.fail(ProcessIoError::WriteFailed);
            break;
        }
    }
    drop(stdin);
    if let Ok(mut state) = io.state.lock() {
        state.writer_done = true;
    }
    (io.wake)();
}

fn essential_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in ESSENTIALS {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

struct IoWorkerGuard(Arc<Pipes>);
impl Drop for IoWorkerGuard {
    fn drop(&mut self) {
        if self.0.receiver_gone.load(Ordering::Acquire) {
            let mut state = match self.0.state.lock() {
                Ok(s) => s,
                Err(e) => e.into_inner(),
            };
            Pipes::discard_received_locked(&mut state);
        }
        self.0.io_workers.fetch_sub(1, Ordering::AcqRel);
        (self.0.wake)();
    }
}

fn pipe_worker<T: Send + 'static>(name: &str, io: Arc<Pipes>, run: impl FnOnce(T, Arc<Pipes>) + Send + 'static) -> Result<SyncSender<T>, String> {
    let (send, receive) = sync_channel(1);
    io.io_started.store(true, Ordering::Release);
    io.io_workers.fetch_add(1, Ordering::AcqRel);
    let guard = IoWorkerGuard(io.clone());
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _guard = guard;
            if let Ok(pipe) = receive.recv() {
                run(pipe, io);
            }
        })
        .map_err(|_| ProcessIoError::ThreadStartFailed.to_string())?;
    Ok(send)
}

fn spawn_process(mut cmd: Command, limits: ProcessIoLimits, wake: Arc<dyn Fn() + Send + Sync>) -> Result<StrictProcess, String> {
    let limits = limits.validate().map_err(|e| e.to_string())?;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    detach_group(&mut cmd);
    let io = Pipes::new(limits, wake);
    let (handle, ready) = start_owner(io.clone())?;
    // Every fallible thread start precedes OS spawn. Returning a startup Err
    // therefore cannot release caller admission while a child is still owned.
    let send_stdin = pipe_worker::<std::process::ChildStdin>("agent-stdin", io.clone(), write_input)?;
    let send_stdout = pipe_worker::<std::process::ChildStdout>("agent-stdout", io.clone(), read_stdout)?;
    let send_stderr = pipe_worker::<std::process::ChildStderr>("agent-stderr", io.clone(), read_stderr)?;
    // No other thread touches the handoff slot until ready is sent/dropped.
    // Acquire it before spawn, so synchronization failure cannot orphan a child.
    let mut slot = handle.0.child.lock().map_err(|_| ProcessIoError::Closed.to_string())?;
    let mut child = cmd.spawn().map_err(|_| ProcessIoError::InvalidLaunch.to_string())?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    handle.0.pid.store(child.id(), Ordering::Release);
    *slot = Some(child);
    drop(slot);
    drop(ready);
    let process = StrictProcess { child: handle, io: io.clone() };
    // A post-spawn pipe anomaly remains an event on an OWNED facade, never an
    // Err that loses the cleanup handle and its caller's admission reservation.
    if let Ok(mut state) = io.state.lock() {
        state.writer_done = stdin.is_none();
        state.stdout_done = stdout.is_none();
        state.stderr_done = stderr.is_none();
    }
    if let Some(pipe) = stdin {
        if send_stdin.try_send(pipe).is_err() {
            io.fail(ProcessIoError::WriteFailed);
        }
    } else {
        io.fail(ProcessIoError::WriteFailed);
    }
    if let Some(pipe) = stdout {
        if send_stdout.try_send(pipe).is_err() {
            io.fail(ProcessIoError::ReadFailed);
        }
    } else {
        io.fail(ProcessIoError::ReadFailed);
    }
    if let Some(pipe) = stderr {
        if send_stderr.try_send(pipe).is_err() {
            io.fail(ProcessIoError::ReadFailed);
        }
    } else {
        io.fail(ProcessIoError::ReadFailed);
    }
    Ok(process)
}

impl StrictProcess {
    pub fn try_send_tracked(&self, ticket: u64, frame_including_lf: Vec<u8>, lane: PipeLane) -> Result<(), ProcessIoError> {
        self.io.send_tracked(ticket, frame_including_lf, lane)
    }
    pub(super) fn inbound(&self) -> Arc<InboundPool> {
        self.io.inbound.clone()
    }
    pub(super) fn close_input(&self) {
        // Called by the runtime worker only AFTER its tracked shutdown frame
        // was written. Start the SAME original owner/deadline while closing.
        self.child.shutdown(false);
    }
    pub fn try_recv(&self) -> Result<Option<ProcessEvent>, ProcessIoError> {
        self.io.recv()
    }
    pub fn handle(&self) -> ProcessHandle {
        self.child.clone()
    }
    pub fn shutdown(&mut self, wait: bool) {
        self.child.shutdown(wait);
    }
}
impl Drop for StrictProcess {
    fn drop(&mut self) {
        self.io.receiver_gone.store(true, Ordering::Release);
        // Nonblocking: this call try-locks only. A missed cleanup is observed by
        // the SAME original owner/IO workers, not a new reaper or a UI join.
        self.shutdown(false);
    }
}

impl AgentProcess {
    pub fn spawn_strict(launch: &ProcessLaunch, limits: ProcessIoLimits, wake: impl Fn() + Send + Sync + 'static) -> Result<StrictProcess, String> {
        if !launch.executable.is_absolute()
            || !launch.cwd.is_absolute()
            || !launch.executable.is_file()
            || launch.executable.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        {
            return Err(ProcessIoError::InvalidLaunch.to_string());
        }
        let mut cmd = Command::new(&launch.executable);
        essential_env(&mut cmd);
        cmd.args(&launch.args).current_dir(&launch.cwd);
        spawn_process(cmd, limits, Arc::new(wake))
    }

    /// Source-compatible ACP facade; only this legacy path uses discovery and
    /// lossy text conversion. Process ownership and all OS teardown are shared.
    pub fn spawn(
        argv: &[String],
        cwd: &std::path::Path,
        env_remove: &[&str],
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<AgentProcess, String> {
        let Some((program, args)) = argv.split_first() else { return Err("no agent command".into()) };
        let mut cmd = super::discover::command_for(std::path::Path::new(program));
        cmd.args(args).current_dir(cwd);
        for key in env_remove {
            cmd.env_remove(key);
        }
        let wake = Arc::new(wake);
        let limits = ProcessIoLimits {
            frame_bytes: 8 << 20,
            queue_bytes: 16 << 20,
            control_lane_bytes: 65536,
            stderr_lines: 40,
            stderr_line_scalars: 400,
            stderr_bytes: 65536,
        };
        let process = spawn_process(cmd, limits, wake.clone())?;
        let child = process.handle();
        let to_agent = Some(process.io.clone());
        // Receiver stays source-compatible. Do not put a new unbounded text
        // mailbox behind the bounded pipe; only the adapter thread may wait.
        let (tx, from_agent) = sync_channel(1);
        std::thread::Builder::new()
            .name("agent-acp-lines".into())
            .spawn(move || {
                let mut diagnostics = Vec::new();
                loop {
                    let message = match process.try_recv() {
                        Ok(Some(ProcessEvent::Frame(bytes))) => Some(FromAgent::Line(String::from_utf8_lossy(&bytes).into_owned())),
                        Ok(Some(ProcessEvent::Written(_))) => None,
                        Ok(Some(ProcessEvent::IoFailed(error))) => {
                            diagnostics.push(error.to_string());
                            None
                        }
                        Ok(Some(ProcessEvent::Exited(mut tail))) => {
                            tail.extend(diagnostics);
                            let _ = tx.send(FromAgent::Exited(tail));
                            wake();
                            break;
                        }
                        Err(ProcessIoError::ResourceExhausted) | Ok(None) => {
                            std::thread::sleep(Duration::from_millis(2));
                            None
                        }
                        Err(error) => {
                            let _ = tx.send(FromAgent::Exited(vec![error.to_string()]));
                            wake();
                            break;
                        }
                    };
                    if let Some(message) = message {
                        if tx.send(message).is_err() {
                            break;
                        }
                        wake();
                    }
                }
            })
            .map_err(|_| ProcessIoError::ThreadStartFailed.to_string())?;
        Ok(Self { child, to_agent, from_agent })
    }
    pub fn send(&self, msg: &serde_json::Value) {
        if let Some(io) = &self.to_agent {
            let mut bytes = msg.to_string().into_bytes();
            bytes.push(b'\n');
            if let Err(error) = io.send(bytes, PipeLane::Data) {
                io.fail(error);
            }
        }
    }
    pub fn pid(&self) -> Option<u32> {
        self.child.pid()
    }
    pub fn handle(&self) -> ProcessHandle {
        self.child.clone()
    }
    pub fn shutdown(&mut self, wait: bool) {
        self.to_agent = None;
        self.child.shutdown(wait);
    }
}
impl Drop for AgentProcess {
    fn drop(&mut self) {
        self.shutdown(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Restored from the already-observed Task6 missing-interface RED.
    #[test]
    fn mastra_process_tracked_written_requires_write_and_flush() {
        struct BarrierWriter {
            entered: SyncSender<&'static str>,
            proceed: Receiver<()>,
        }
        impl Write for BarrierWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert_eq!(bytes, b"original shared writer\n");
                self.entered.send("write").unwrap();
                self.proceed.recv().unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.entered.send("flush").unwrap();
                self.proceed.recv().unwrap();
                Ok(())
            }
        }
        let (entered, observed) = sync_channel(1);
        let (proceed, barrier) = sync_channel(1);
        let (wake, woke) = sync_channel(8);
        let io = Pipes::new(
            limits(),
            Arc::new(move || {
                let _ = wake.try_send(());
            }),
        );
        io.send_tracked(41, b"original shared writer\n".to_vec(), PipeLane::Data).unwrap();
        assert!(io.recv().unwrap().is_none());
        let writer_io = io.clone();
        let writer = std::thread::spawn(move || write_input(BarrierWriter { entered, proceed: barrier }, writer_io));
        assert_eq!(observed.recv_timeout(Duration::from_secs(10)).unwrap(), "write");
        assert!(io.recv().unwrap().is_none());
        proceed.send(()).unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(10)).unwrap(), "flush");
        assert!(io.recv().unwrap().is_none(), "write without flush is not Written");
        proceed.send(()).unwrap();
        woke.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(io.recv().unwrap(), Some(ProcessEvent::Written(41))));
        assert!(io.recv().unwrap().is_none());
        io.close_input();
        writer.join().unwrap();
    }

    #[test]
    fn mastra_process_tracked_failure_exit_and_metadata_are_bounded() {
        struct Failing {
            remaining: usize,
            flush: bool,
        }
        impl Write for Failing {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("private"));
                }
                let n = self.remaining.min(b.len());
                self.remaining -= n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if self.flush { Err(std::io::Error::other("private")) } else { Ok(()) }
            }
        }
        for (remaining, flush) in [(0, false), (2, false), (usize::MAX, true)] {
            let io = Pipes::new(limits(), Arc::new(|| {}));
            io.send_tracked(7, b"frame\n".to_vec(), PipeLane::Data).unwrap();
            io.send_tracked(8, b"never written\n".to_vec(), PipeLane::Data).unwrap();
            write_input(Failing { remaining, flush }, io.clone());
            assert!(matches!(io.recv().unwrap(), Some(ProcessEvent::IoFailed(ProcessIoError::WriteFailed))));
            assert!(io.recv().unwrap().is_none());
            let state = io.state.lock().unwrap();
            assert!(state.data.is_empty() && state.control.is_empty());
            assert_eq!((state.data_bytes, state.control_bytes, state.in_flight), (0, 0, 0));
        }
        // A condvar barrier watches the actual writer, not a sleep threshold.
        let io = Pipes::new(limits(), Arc::new(|| {}));
        for ticket in 1..SLOTS as u64 {
            io.send_tracked(ticket, b"x\n".to_vec(), PipeLane::Data).unwrap();
        }
        io.send_tracked(SLOTS as u64, b"c\n".to_vec(), PipeLane::Control).unwrap();
        let writer_io = io.clone();
        let writer = std::thread::spawn(move || write_input(std::io::sink(), writer_io));
        {
            let mut state = io.state.lock().unwrap();
            while !state.data.is_empty() || !state.control.is_empty() || state.in_flight != 0 {
                state = io.ready.wait_timeout(state, Duration::from_secs(10)).unwrap().0;
            }
        }
        assert_eq!(io.send_tracked(9000, b"x\n".to_vec(), PipeLane::Data), Err(ProcessIoError::ResourceExhausted));
        assert_eq!(io.send_tracked(9001, b"c\n".to_vec(), PipeLane::Control), Err(ProcessIoError::ResourceExhausted));
        let mut tickets = Vec::new();
        while let Some(event) = io.recv().unwrap() {
            let ProcessEvent::Written(ticket) = event else { panic!("not Written") };
            tickets.push(ticket);
        }
        tickets.sort_unstable();
        assert_eq!(tickets, (1..=SLOTS as u64).collect::<Vec<_>>());
        io.send_tracked(9002, b"queued\n".to_vec(), PipeLane::Data).unwrap();
        io.stop.request();
        io.close_input();
        writer.join().unwrap();
        while let Some(event) = io.recv().unwrap() {
            assert!(matches!(event, ProcessEvent::Written(9002)));
        }
    }

    #[test]
    fn mastra_process_inbound_credit_survives_dequeue() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let release = program.directory.join("credit-release");
        let l = ProcessIoLimits { frame_bytes: 8192, queue_bytes: 12288, control_lane_bytes: 4096, ..limits() };
        let script = format!("WRITE_HEX 78 8191\nWRITE_HEX 0a\nHOLD_INPUT {}\nWRITE_HEX 79 8191\nWRITE_HEX 0a\nEXPECT_EOF\n", release.display());
        let mut p = peer(&program, "credit", &script, l);
        let ProcessEvent::Frame(frame) = event(&p) else { panic!("frame") };
        assert_eq!(frame.len(), 8192);
        let (bytes, credit) = frame.into_parts();
        drop(bytes); // decode conversion does NOT release the input owner.
        assert_eq!(p.io.inbound.used(), 8192);
        let retained = credit.clone();
        drop(credit);
        assert_eq!(p.io.inbound.used(), 8192);
        std::fs::write(release, b"go").unwrap();
        assert!(ended(&mut p).0.contains(&ProcessIoError::ResourceExhausted));
        assert_eq!(p.io.inbound.used(), 8192, "cleanup cannot refund a live managed owner");
        let replacement = Pipes::new(l, Arc::new(|| {}));
        drop(retained);
        assert_eq!(p.io.inbound.used(), 0);
        assert_eq!(replacement.inbound.used(), 0, "old credit never changes a replacement budget");
    }

    #[test]
    fn mastra_process_inbound_credit_poison_cleanup_releases_only_owned_frames() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let mut p = peer(&program, "credit-poison", "WRITE_HEX 68656c640a\nWRITE_HEX 7175657565640a\nHOLD_INPUT never-created\n", limits());
        let ProcessEvent::Frame(held) = event(&p) else { panic!("held frame") };
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.io.state.lock().unwrap().incoming.is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let poison = p.io.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.state.lock().unwrap();
                panic!("intentional live IPC poison");
            })
            .join()
            .is_err()
        );
        p.shutdown(false);
        let mut exited = false;
        while Instant::now() < deadline {
            match p.try_recv() {
                Ok(Some(ProcessEvent::Exited(_))) => {
                    exited = true;
                    break;
                }
                Err(ProcessIoError::Closed) | Err(ProcessIoError::ResourceExhausted) | Ok(None) => std::thread::yield_now(),
                other => panic!("poison event {other:?}"),
            }
        }
        p.shutdown(true);
        assert_eq!(p.handle().try_cleanup_result(), Some(Ok(())));
        assert!(exited, "poison must not permanently pin completed original IO workers or their queued credit");
        assert_eq!(p.io.inbound.used(), held.len(), "cleanup refunds queued payload only, not the live external owner");
        drop(held);
        assert_eq!(p.io.inbound.used(), 0);
    }

    #[test]
    fn mastra_process_inbound_credit_drop_discards_abandoned_queue() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let p = peer(&program, "credit-drop", "WRITE_HEX 68656c640a\nWRITE_HEX 7175657565640a\nHOLD_INPUT never-created\n", limits());
        let ProcessEvent::Frame(held) = event(&p) else { panic!("held frame") };
        let io = p.io.clone();
        let handle = p.handle();
        let deadline = Instant::now() + Duration::from_secs(10);
        while io.state.lock().unwrap().incoming.is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(p);
        handle.shutdown(true);
        assert_eq!(handle.try_cleanup_result(), Some(Ok(())));
        assert_eq!(io.inbound.used(), held.len(), "an abandoned strict receiver must not pin its queued raw payload through the cleanup handle");
        drop(held);
        assert_eq!(io.inbound.used(), 0);
    }

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // `kill -0` checks that the process exists without touching it. A zombie still counts
        // on Linux, so /proc's state is checked there too.
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            return !stat.rsplit(')').next().is_some_and(|s| s.trim_start().starts_with('Z'));
        }
        Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// Start a fake agent that leaves helpers behind, read the helpers' ids from it, end it.
    #[cfg(unix)]
    fn helpers_after_shutdown(script: &str) -> Vec<u32> {
        let dir = std::env::temp_dir();
        let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        let mut p = AgentProcess::spawn(&argv, &dir, &[], || {}).unwrap();
        let mut pids = vec![p.pid().unwrap()];
        while pids.len() < 3 {
            match p.from_agent.recv_timeout(Duration::from_secs(10)).unwrap() {
                FromAgent::Line(l) => pids.push(l.trim().parse().unwrap()),
                FromAgent::Exited(tail) => panic!("the fake agent ended early: {tail:?}"),
            }
        }
        assert!(pids.iter().all(|&pid| alive(pid)), "all running before the shutdown: {pids:?}");
        p.shutdown(true);
        // Orphaned helpers are reaped by init a moment after they end.
        let start = Instant::now();
        while pids.iter().any(|&pid| alive(pid)) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        pids.into_iter().filter(|&pid| alive(pid)).collect()
    }

    /// No orphans: an agent that started helpers (as Node adapters start the CLI they wrap and
    /// its MCP servers) is ended with all of them, whether it ends politely at end of input or
    /// ignores both its input and the request to stop.
    #[test]
    #[cfg(unix)]
    fn stopping_the_agent_leaves_no_helpers_running() {
        // A polite agent: ends when its input closes; its helpers would live on by themselves.
        let polite = "sleep 300 & echo $!; sleep 300 & echo $!; cat >/dev/null";
        assert_eq!(helpers_after_shutdown(polite), Vec::<u32>::new(), "polite agent");
        // A stubborn one: ignores end of input and TERM (so do its helpers).
        let stubborn = "trap '' TERM; (trap '' TERM; sleep 300) & echo $!; (trap '' TERM; sleep 300) & echo $!; while :; do sleep 1; done";
        assert_eq!(helpers_after_shutdown(stubborn), Vec::<u32>::new(), "stubborn agent");
    }

    /// Dropping the process (closing the panel's connection any other way) ends it too.
    #[test]
    #[cfg(unix)]
    fn dropping_the_process_ends_its_group() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 300 & echo $!; wait".to_string()];
        let p = AgentProcess::spawn(&argv, &std::env::temp_dir(), &[], || {}).unwrap();
        let leader = p.pid().unwrap();
        let FromAgent::Line(l) = p.from_agent.recv_timeout(Duration::from_secs(10)).unwrap() else { panic!("no helper id") };
        let helper: u32 = l.trim().parse().unwrap();
        drop(p);
        let start = Instant::now();
        while (alive(helper) || alive(leader)) && start.elapsed() < Duration::from_secs(8) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(helper) && !alive(leader), "leader {leader} or helper {helper} still running");
    }

    fn limits() -> ProcessIoLimits {
        ProcessIoLimits {
            frame_bytes: 8 << 20,
            queue_bytes: 16 << 20,
            control_lane_bytes: 65536,
            stderr_lines: 40,
            stderr_line_scalars: 400,
            stderr_bytes: 65536,
        }
    }

    fn peer(peer: &super::super::tests::PeerProgram, name: &str, script: &str, limits: ProcessIoLimits) -> StrictProcess {
        let path = peer.directory.join(name);
        std::fs::write(&path, script).unwrap();
        let process = AgentProcess::spawn_strict(
            &ProcessLaunch { executable: peer.executable.clone(), args: vec![path.into_os_string()], cwd: peer.directory.clone() },
            limits,
            || {},
        )
        .unwrap();
        peer.track(process.handle());
        process
    }

    fn event(p: &StrictProcess) -> ProcessEvent {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            match p.try_recv() {
                Ok(Some(ProcessEvent::Written(_))) => continue,
                Ok(Some(event)) => return event,
                Ok(None) | Err(ProcessIoError::ResourceExhausted) if Instant::now() < until => std::thread::sleep(Duration::from_millis(2)),
                other => panic!("peer event watchdog: {other:?}"),
            }
        }
    }

    fn ended(p: &mut StrictProcess) -> (Vec<ProcessIoError>, Vec<String>) {
        let mut errors = Vec::new();
        loop {
            match event(p) {
                ProcessEvent::IoFailed(e) => errors.push(e),
                ProcessEvent::Exited(tail) => {
                    p.shutdown(true);
                    assert_eq!(p.handle().try_cleanup_result(), Some(Ok(())));
                    assert!(matches!(p.try_recv(), Ok(None)));
                    return (errors, tail);
                }
                other => panic!("unexpected peer output: {other:?}"),
            }
        }
    }

    // Mutation caught: draining/allocating an oversized remainder or retaining an
    // unterminated stderr line; split valid UTF-8 must not become replacement text.
    #[test]
    fn mastra_process_unterminated_streams_are_bounded() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let mut p = peer(&program, "flood", "STDERR_HEX e282ac 100000\nWRITE_HEX 78 16777216\n", limits());
        let (errors, tail) = ended(&mut p);
        assert_eq!(errors, [ProcessIoError::OversizedFrame]);
        assert_eq!(tail, ["€".repeat(400)]);
        assert!(tail.len() <= 40 && tail.iter().all(|l| l.chars().count() <= 400));
        assert!(tail.iter().map(String::len).sum::<usize>() <= 65536);
        let state = p.io.state.lock().unwrap();
        assert!(state.stdout_peak <= (8 << 20));
        assert_eq!(state.stdout_read, 8 << 20, "stop at first oversized prefix, without draining remainder");
        drop(state);
        let mut p = peer(&program, "lines", "STDERR_HEX e282ac 400\nSTDERR_HEX 0a\nSTDERR_HEX 780a 100\nWRITE_HEX 6f6b0a\nEXPECT_EOF\n", limits());
        assert!(matches!(event(&p), ProcessEvent::Frame(v) if v == b"ok\n"));
        p.shutdown(true);
        let (_, tail) = ended(&mut p);
        assert_eq!(tail, vec!["x".to_owned(); 40]);
    }

    // Mutation caught: releasing bytes on dequeue (rather than physical write),
    // starving control under data pressure, or interleaving an in-flight frame.
    #[test]
    fn mastra_process_data_and_control_bytes_are_accounted() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let release = program.directory.join("release");
        let script = format!(
            "WRITE_HEX 72656164790a\nHOLD_INPUT {}\nEXPECT_HEX 64 1048575\nEXPECT_HEX 0a\nEXPECT_HEX 63 4095\nEXPECT_HEX 0a\nEXPECT_HEX 65 1048575\nEXPECT_HEX 0a\nWRITE_HEX 646f6e650a\nEXPECT_EOF\n",
            release.display()
        );
        let mut l = limits();
        l.frame_bytes = 1 << 20;
        l.queue_bytes = (2 << 20) + 4096;
        l.control_lane_bytes = 4096;
        let mut p = peer(&program, "blocked", &script, l);
        assert!(matches!(event(&p), ProcessEvent::Frame(v) if v == b"ready\n"));
        let frame = |byte, size| {
            let mut v = vec![byte; size];
            *v.last_mut().unwrap() = b'\n';
            v
        };
        p.try_send_tracked(1, frame(b'd', 1 << 20), PipeLane::Data).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while p.io.state.lock().unwrap().in_flight == 0 {
            assert!(Instant::now() < until, "writer watchdog");
            std::thread::yield_now();
        }
        assert_eq!(p.io.state.lock().unwrap().data_bytes, 1 << 20);
        p.try_send_tracked(2, frame(b'e', 1 << 20), PipeLane::Data).unwrap();
        assert_eq!(p.try_send_tracked(3, b"x\n".to_vec(), PipeLane::Data), Err(ProcessIoError::ResourceExhausted));
        p.try_send_tracked(4, frame(b'c', 4096), PipeLane::Control).unwrap();
        assert_eq!(p.try_send_tracked(5, b"x\n".to_vec(), PipeLane::Control), Err(ProcessIoError::ResourceExhausted));
        std::fs::write(release, b"release").unwrap();
        assert!(matches!(event(&p), ProcessEvent::Frame(v) if v == b"done\n"));
        p.shutdown(true);
        assert_eq!(ended(&mut p).0, Vec::<ProcessIoError>::new());
        assert_eq!(p.io.state.lock().unwrap().data_bytes, 0);

        let mut p = peer(&program, "cancel-queued", "WRITE_HEX 72656164790a\nHOLD_INPUT never-created\n", l);
        assert!(matches!(event(&p), ProcessEvent::Frame(_)));
        p.try_send_tracked(1, frame(b'd', 1 << 20), PipeLane::Data).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while p.io.state.lock().unwrap().in_flight == 0 {
            assert!(Instant::now() < until, "blocked writer watchdog");
            std::thread::yield_now();
        }
        p.try_send_tracked(2, frame(b'e', 1 << 20), PipeLane::Data).unwrap();
        p.try_send_tracked(3, frame(b'c', 4096), PipeLane::Control).unwrap();
        p.shutdown(false);
        {
            let state = p.io.state.lock().unwrap();
            assert!(state.data.is_empty() && state.control.is_empty());
            assert_eq!(state.data_bytes, state.in_flight);
            assert_eq!(state.control_bytes, 0);
        }
        p.shutdown(true);
        assert_eq!(p.handle().try_cleanup_result(), Some(Ok(())));
        assert_eq!(ended(&mut p).0, [ProcessIoError::WriteFailed]);
    }

    // Mutation caught: lossy stdout decoding, swallowed partial EOF, swallowed
    // broken pipes, and duplicate terminal notifications.
    #[test]
    fn mastra_process_pipe_failures_and_partial_eof_are_visible() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let mut p = peer(&program, "raw", "WRITE_HEX ff0a\nWRITE_HEX e282\nEXIT 0\n", limits());
        assert!(matches!(event(&p), ProcessEvent::Frame(v) if v == [255, 10]));
        assert_eq!(ended(&mut p).0, [ProcessIoError::TruncatedFrame]);
        let release = program.directory.join("break");
        let mut p = peer(&program, "broken", &format!("WRITE_HEX 72656164790a\nHOLD_INPUT {}\nEXIT 0\n", release.display()), limits());
        assert!(matches!(event(&p), ProcessEvent::Frame(_)));
        let mut frame = vec![b'x'; 8 << 20];
        *frame.last_mut().unwrap() = b'\n';
        p.try_send_tracked(1, frame, PipeLane::Data).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while p.io.state.lock().unwrap().in_flight == 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        std::fs::write(release, b"exit").unwrap();
        assert!(ended(&mut p).0.contains(&ProcessIoError::WriteFailed));
        assert_eq!(p.try_send_tracked(2, b"x\n".to_vec(), PipeLane::Data), Err(ProcessIoError::Closed));
    }

    // Mutation caught: PATH/shim fallback, ambient credential inheritance,
    // incorrect cwd/argument splitting, repeated teardown or surviving helpers.
    #[test]
    fn mastra_process_exact_path_and_teardown() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let mut launch = ProcessLaunch { executable: "rustc".into(), args: Vec::new(), cwd: program.directory.clone() };
        assert!(AgentProcess::spawn_strict(&launch, limits(), || {}).is_err());
        launch.executable = program.directory.join("nonexistent");
        assert!(AgentProcess::spawn_strict(&launch, limits(), || {}).is_err());
        launch.executable = program.executable.clone();
        launch.cwd = ".".into();
        assert!(AgentProcess::spawn_strict(&launch, limits(), || {}).is_err());
        let helper = program.directory.join("helper");
        std::fs::write(&helper, "HOLD_INPUT never-created\n").unwrap();
        for stubborn in [false, true] {
            let script = format!(
                "SPAWN_HELPER {}\nSPAWN_HELPER {}\n{}\n",
                helper.display(),
                helper.display(),
                if stubborn { "HOLD_INPUT never-created" } else { "EXPECT_EOF" }
            );
            let mut p = peer(&program, "literal script with spaces", &script, limits());
            let h = p.handle();
            let mut pids = vec![h.pid().unwrap()];
            for _ in 0..2 {
                let ProcessEvent::Frame(v) = event(&p) else { panic!("helper pid") };
                pids.push(std::str::from_utf8(&v).unwrap().trim().parse::<u32>().unwrap());
            }
            p.shutdown(false);
            p.shutdown(true);
            p.shutdown(true);
            assert_eq!(h.try_cleanup_result(), Some(Ok(())));
            #[cfg(unix)]
            assert!(pids.iter().all(|pid| !alive(*pid)), "live descendants: {pids:?}");
            assert_eq!(ended(&mut p).0, Vec::<ProcessIoError>::new());
        }
        let p = peer(&program, "drop", &format!("SPAWN_HELPER {}\nHOLD_INPUT never-created\n", helper.display()), limits());
        let h = p.handle();
        let ProcessEvent::Frame(v) = event(&p) else { panic!("helper pid") };
        let child: u32 = std::str::from_utf8(&v).unwrap().trim().parse().unwrap();
        drop(p);
        h.shutdown(true);
        assert_eq!(h.try_cleanup_result(), Some(Ok(())));
        #[cfg(unix)]
        assert!(!alive(child));
        #[cfg(not(unix))]
        let _ = child;
    }

    // Mutation caught: accepting contradictory/oversized budgets, allocating a
    // multibyte diagnostic remainder, or treating a read error as clean EOF.
    #[test]
    fn mastra_process_limits_incremental_diagnostics_and_read_errors() {
        for bad in [
            ProcessIoLimits { frame_bytes: 0, ..limits() },
            ProcessIoLimits { frame_bytes: (8 << 20) + 1, ..limits() },
            ProcessIoLimits { queue_bytes: 1, ..limits() },
            ProcessIoLimits { control_lane_bytes: 0, ..limits() },
            ProcessIoLimits { stderr_bytes: 0, ..limits() },
        ] {
            assert!(bad.validate().is_err());
        }
        let l = ProcessIoLimits { stderr_bytes: 7, stderr_lines: 2, stderr_line_scalars: 2, ..limits() };
        let mut tail = StderrTail::new(l);
        for _ in 0..100000 {
            for byte in "€".as_bytes() {
                tail.feed(&[*byte]);
                assert!(tail.pending.len() <= 3);
            }
        }
        assert_eq!(tail.snapshot(), ["€€"]);
        tail.feed(b"\na\nb\n");
        assert_eq!(tail.snapshot(), ["a", "b"]);
        tail.feed(&[0xe2, 0x82]);
        tail.finish();
        assert_eq!(tail.snapshot(), ["b", "�"]);

        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("private path/key must not be echoed"))
            }
        }
        for stderr in [false, true] {
            let io = Pipes::new(limits(), Arc::new(|| {}));
            if stderr {
                read_stderr(Broken, io.clone());
            } else {
                read_stdout(Broken, io.clone());
            }
            assert!(matches!(io.recv(), Ok(Some(ProcessEvent::IoFailed(ProcessIoError::ReadFailed)))));
            assert!(io.recv().unwrap().is_none());
        }
    }

    // Mutation caught: acknowledging a cleanup that missed its original
    // deadline, or changing an established failure to success after reaping.
    #[test]
    fn mastra_process_expired_cleanup_is_visible_and_never_relabelled() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let script = program.directory.join("expired");
        std::fs::write(&script, "HOLD_INPUT never-created\n").unwrap();
        let mut io = Pipes::new(limits(), Arc::new(|| {}));
        Arc::get_mut(&mut io).unwrap().stop = Arc::new(Stop { origin: Instant::now() - Duration::from_secs(4), at: AtomicU64::new(u64::MAX) });
        let (handle, ready) = start_owner(io.clone()).unwrap();
        let mut command = Command::new(&program.executable);
        essential_env(&mut command);
        detach_group(&mut command);
        let child =
            command.arg(script).current_dir(&program.directory).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let pid = child.id();
        handle.0.pid.store(pid, Ordering::Release);
        *handle.0.child.lock().unwrap() = Some(child);
        #[cfg(not(unix))]
        program.track(handle.clone());
        io.stop.at.store(0, Ordering::Release);
        drop(ready);
        handle.shutdown(true);
        assert_eq!(handle.try_cleanup_result(), Some(Err(ProcessIoError::CleanupFailed)));
        #[cfg(unix)]
        {
            let until = Instant::now() + Duration::from_secs(10);
            while alive(pid) {
                assert!(Instant::now() < until, "reaper watchdog");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(handle.try_cleanup_result(), Some(Err(ProcessIoError::CleanupFailed)));
        // Unix can observe the group gone and finish this same reaper. Without
        // tree evidence (Windows expired before taskkill), retain responsibility
        // AND the fixture directory instead of asserting that the owner ended.
        #[cfg(unix)]
        {
            let until = Instant::now() + Duration::from_secs(10);
            while Arc::strong_count(&handle.0) != 1 {
                assert!(Instant::now() < until, "reaper ownership watchdog");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    // Mutation caught: accepting command shims (Command can invoke cmd.exe for
    // .cmd/.bat on Windows) instead of requiring an exact executable file.
    #[test]
    fn mastra_process_exact_path_rejects_command_shims() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let script = program.directory.join("shim-script");
        std::fs::write(&script, "EXPECT_EOF\n").unwrap();
        for extension in ["cmd", "BAT"] {
            let shim = program.directory.join(format!("runtime.{extension}"));
            std::fs::copy(&program.executable, &shim).unwrap();
            let launch = ProcessLaunch { executable: shim, args: vec![script.clone().into_os_string()], cwd: program.directory.clone() };
            assert!(AgentProcess::spawn_strict(&launch, limits(), || {}).is_err(), "command shims must be refused before OS launch");
        }
    }

    // Mutation caught: a frame-count bound consuming the control reserve even
    // when data has not consumed the byte budget (many tiny frames).
    #[test]
    fn mastra_process_control_reserve_survives_tiny_frame_slot_pressure() {
        let io = Pipes::new(limits(), Arc::new(|| {}));
        let mut refused = false;
        for _ in 0..10000 {
            match io.send(b"x\n".to_vec(), PipeLane::Data) {
                Ok(()) => {}
                Err(ProcessIoError::ResourceExhausted) => {
                    refused = true;
                    break;
                }
                other => panic!("unexpected admission: {other:?}"),
            }
        }
        assert!(refused, "bounded data slots must refuse admission");
        let mut control = vec![b'c'; 4096];
        *control.last_mut().unwrap() = b'\n';
        assert_eq!(io.send(control, PipeLane::Control), Ok(()), "a separate control slot must remain usable");
    }

    // Mutation caught: silently abandoning the owned child when synchronization
    // is poisoned; recovering ownership is only for stopping/reaping, not reuse.
    #[test]
    fn mastra_process_poisoned_ownership_fails_closed_and_reaps() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let script = program.directory.join("poisoned");
        std::fs::write(&script, "HOLD_INPUT never-created\n").unwrap();
        let io = Pipes::new(limits(), Arc::new(|| {}));
        let (handle, ready) = start_owner(io.clone()).unwrap();
        let mut command = Command::new(&program.executable);
        essential_env(&mut command);
        detach_group(&mut command);
        let child =
            command.arg(script).current_dir(&program.directory).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let pid = child.id();
        handle.0.pid.store(pid, Ordering::Release);
        *handle.0.child.lock().unwrap() = Some(child);
        let poison = handle.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.0.child.lock().unwrap();
                panic!("intentional test poison");
            })
            .join()
            .is_err()
        );
        drop(ready);
        let until = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&handle.0) != 1 {
            assert!(Instant::now() < until, "owner watchdog");
            std::thread::sleep(Duration::from_millis(5));
        }
        let event = io.recv();
        let result = handle.try_cleanup_result();
        // RED cleanup: an unfixed owner abandoned this slot. Do not leave a
        // real peer running merely because the assertion below must fail.
        let abandoned = handle.0.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(mut child) = abandoned {
            #[cfg(unix)]
            signal_group(pid, Signal::SIGKILL).unwrap();
            let _ = child.kill();
            assert_eq!(wait_until(&mut child, Instant::now() + TEARDOWN), Ok(true));
        }
        assert!(matches!(event, Ok(Some(ProcessEvent::IoFailed(ProcessIoError::CleanupFailed)))), "poison must be explicit: {event:?}");
        assert_eq!(result, Some(Err(ProcessIoError::CleanupFailed)));
    }

    // ACP's pid() is the active-connection surface, not cleanup evidence. It
    // must close immediately while the independent cleanup result stays pending.
    #[test]
    fn mastra_process_shutdown_retires_pid_without_faking_cleanup() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let mut p = peer(&program, "retired-pid", "WRITE_HEX 72656164790a\nHOLD_INPUT never-created\n", limits());
        assert!(matches!(event(&p), ProcessEvent::Frame(_)));
        let handle = p.handle();
        assert!(handle.pid().is_some());
        let pending = handle.0.result.lock().unwrap();
        p.shutdown(false);
        let retired_pid = handle.pid();
        let cleanup = handle.try_cleanup_result();
        drop(pending);
        p.shutdown(true);
        assert_eq!(handle.try_cleanup_result(), Some(Ok(())));
        assert_eq!(ended(&mut p).0, Vec::<ProcessIoError>::new());
        assert_eq!(retired_pid, None, "shutdown must retire the ACP connection immediately");
        assert_eq!(cleanup, None, "do not fabricate cleanup while the real result is pending");
    }

    // A channel watchdog precedes creation of any OS child. The RED path can
    // fail without joining a stuck reader or leaving a peer running.
    #[test]
    fn mastra_process_poisoned_pipe_state_closes_and_reaps() {
        let program = super::super::tests::mastra_peer_program().unwrap();
        let io = Pipes::new(limits(), Arc::new(|| {}));
        io.send(b"queued-data\n".to_vec(), PipeLane::Data).unwrap();
        io.send(b"queued-control\n".to_vec(), PipeLane::Control).unwrap();
        let poison = io.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.state.lock().unwrap();
                panic!("intentional PipeState test poison");
            })
            .join()
            .is_err()
        );
        let (done, completed) = sync_channel(1);
        let reader_io = io.clone();
        let reader = std::thread::spawn(move || {
            read_stderr(&b"nonempty stderr"[..], reader_io);
            let _ = done.send(());
        });
        assert!(
            completed.recv_timeout(Duration::from_secs(1)).is_ok(),
            "poisoned nonempty stderr reader did not return; no OS child has been started"
        );
        reader.join().unwrap();
        assert!(io.stop.requested());
        assert!(matches!(io.recv(), Err(ProcessIoError::Closed)), "poison must remain an explicit closed error");
        assert_eq!(io.send(b"late\n".to_vec(), PipeLane::Data), Err(ProcessIoError::Closed));
        assert!(io.state.is_poisoned(), "do not clear poison or reopen admission");
        {
            let state = io.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(state.input_closed && state.data.is_empty() && state.control.is_empty());
            assert_eq!((state.data_bytes, state.control_bytes), (0, 0));
            assert!(state.errors.contains(&ProcessIoError::Closed));
        }
        // Exercise the actual shared owner with this SAME poisoned PipeState.
        let script = program.directory.join("poisoned-pipes");
        std::fs::write(&script, "HOLD_INPUT never-created\n").unwrap();
        let (handle, ready) = start_owner(io).unwrap();
        let mut command = Command::new(&program.executable);
        essential_env(&mut command);
        detach_group(&mut command);
        let child =
            command.arg(script).current_dir(&program.directory).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let pid = child.id();
        handle.0.pid.store(pid, Ordering::Release);
        *handle.0.child.lock().unwrap() = Some(child);
        program.track(handle.clone());
        drop(ready);
        handle.shutdown(true);
        assert_eq!(handle.try_cleanup_result(), Some(Ok(())), "owned tree termination must still be established");
        #[cfg(unix)]
        assert!(!alive(pid), "poisoned stderr must not block termination/reap");
    }

    // Portable policy test, not a Windows process/OS execution claim. All of
    // these observations enter the SAME decision used by the production owner.
    #[test]
    fn mastra_process_unconfirmed_tree_retains_owner() {
        for (case, leader, helper, tree, finish) in [
            ("leader exited first; helper never started; descendants unconfirmed", true, true, None, false),
            ("tree helper failed; descendants unconfirmed", true, true, None, false),
            ("deadline expired; only leader killed; helpers remain unconfirmed", true, true, None, false),
            ("owned kill helper still needs reaping", true, false, None, false),
            ("leader still needs reaping", false, true, Some(true), false),
            ("successful tree evidence but helper not reaped", true, false, Some(true), false),
            ("group observation still reports live helpers", true, true, Some(false), false),
            ("all owned handles reaped AND tree termination established", true, true, Some(true), true),
        ] {
            assert_eq!(owner_may_finish(leader, helper, tree), finish, "{case}");
        }
    }

    // Windows-named POLICY cases: not Windows OS execution evidence on Linux.
    #[cfg(windows)]
    #[test]
    fn mastra_process_windows_policy_leader_exits_first() {
        assert!(!owner_may_finish(true, true, None));
    }

    #[cfg(windows)]
    #[test]
    fn mastra_process_windows_policy_failed_or_missing_tree_helper() {
        assert!(!owner_may_finish(true, true, None));
        assert!(!owner_may_finish(true, false, None));
    }

    #[cfg(windows)]
    #[test]
    fn mastra_process_windows_policy_expired_deadline_with_helpers() {
        assert!(!owner_may_finish(true, true, None));
        assert!(!owner_may_finish(true, true, Some(false)));
    }

    // This OS-specific gate is intentionally NOT compiled/run as Linux proof.
    #[cfg(windows)]
    #[test]
    fn mastra_process_windows_owned_child_and_helper_cleanup() {
        mastra_process_exact_path_and_teardown();
    }
}
