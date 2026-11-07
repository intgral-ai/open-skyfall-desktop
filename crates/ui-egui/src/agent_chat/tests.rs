use super::*;
use std::sync::mpsc::channel;

/// Original, std-only pipe peer. Each caller compiles once for its scenario set;
/// the owned directory must outlive every process using the executable.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct PeerProgram {
    pub directory: std::path::PathBuf,
    pub executable: std::path::PathBuf,
    processes: std::cell::RefCell<Vec<super::process::ProcessHandle>>,
    runtime_pools: std::cell::RefCell<Vec<std::sync::Arc<super::runtime_client::RuntimeAdmission>>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl PeerProgram {
    pub(super) fn track(&self, handle: super::process::ProcessHandle) {
        let mut processes = self.processes.borrow_mut();
        processes.retain(|p| p.try_cleanup_result() != Some(Ok(())));
        processes.push(handle);
    }
    fn track_runtime(&self, pool: std::sync::Arc<super::runtime_client::RuntimeAdmission>) {
        let mut pools = self.runtime_pools.borrow_mut();
        if !pools.iter().any(|p| std::sync::Arc::ptr_eq(p, &pool)) {
            pools.push(pool);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for PeerProgram {
    fn drop(&mut self) {
        // Also preserve teardown-before-unlink during a failed assertion/unwind.
        // A failed cleanup retains the owned directory instead of deleting files
        // still in use or pretending teardown was established.
        let mut cleaned = true;
        for process in self.processes.get_mut() {
            process.shutdown(true);
            cleaned &= process.try_cleanup_result() == Some(Ok(()));
        }
        // RuntimeClient Drop is nonblocking. Fixture teardown is off-frame and
        // waits on the REAL admission permit's cleanup fact before unlinking.
        for pool in self.runtime_pools.get_mut() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if pool.lower_limit(1).is_ok() {
                    let launch = super::runtime_client::RuntimeLaunch {
                        executable: self.directory.join("not-an-executable"),
                        args: vec![],
                        cwd: self.directory.clone(),
                        host_version: "1".into(),
                        limits: super::mastra_protocol::Limits::default(),
                    };
                    let services = mastra_worker_support::services(pool.clone(), |_, _| {});
                    if super::runtime_client::RuntimeClient::start(launch, services, std::sync::Arc::new(|| {}))
                        .err()
                        .is_some_and(|e| e.code == super::mastra_protocol::ErrorCode::TransportError)
                    {
                        break;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    cleaned = false;
                    break;
                }
                std::thread::yield_now();
            }
        }
        if cleaned {
            if let Err(error) = std::fs::remove_dir_all(&self.directory) {
                eprintln!("peer directory cleanup failed: {error}");
            }
        } else {
            eprintln!("peer directory retained: owned process cleanup was not established");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn mastra_peer_program() -> Result<PeerProgram, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir();
    let mut directory = None;
    for _ in 0..128 {
        let candidate = root.join(format!("solidcraft-mastra-peer-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                directory = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("create peer directory: {e}")),
        }
    }
    let directory = directory.ok_or("could not allocate owned peer directory")?.canonicalize().map_err(|e| e.to_string())?;
    let executable = directory.join(format!("exact runtime peer{}", std::env::consts::EXE_SUFFIX));
    let program =
        PeerProgram { directory, executable, processes: std::cell::RefCell::new(Vec::new()), runtime_pools: std::cell::RefCell::new(Vec::new()) };
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/mastra_runtime_peer.rs");
    let output = std::process::Command::new(compiler)
        .arg("--edition=2024")
        .arg(source)
        .arg("-o")
        .arg(&program.executable)
        .output()
        .map_err(|e| format!("the Rust peer compiler is required: {e}"))?;
    if !output.status.success() {
        return Err(format!("Rust peer compilation failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(program)
}

#[cfg(not(target_arch = "wasm32"))]
mod mastra_worker_support {
    pub use super::mastra_state_support::*;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, sync_channel};
    pub use std::sync::{Arc, Mutex};

    pub struct TestClock;
    impl Clock for TestClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
    }
    pub struct WorkerGate {
        armed: Arc<std::sync::atomic::AtomicBool>,
        entered: Receiver<()>,
        release: std::sync::mpsc::SyncSender<()>,
    }
    impl WorkerGate {
        pub fn install(services: &mut RuntimeServices) -> Self {
            struct GateClock {
                armed: Arc<std::sync::atomic::AtomicBool>,
                entered: std::sync::mpsc::SyncSender<()>,
                release: Mutex<Receiver<()>>,
            }
            impl Clock for GateClock {
                fn now(&self) -> Instant {
                    if self.armed.swap(false, std::sync::atomic::Ordering::AcqRel) {
                        let _ = self.entered.send(());
                        let _ = self.release.lock().unwrap().recv();
                    }
                    Instant::now()
                }
            }
            let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (entered, receive) = sync_channel(1);
            let (release, barrier) = sync_channel(1);
            services.clock = Arc::new(GateClock { armed: armed.clone(), entered, release: Mutex::new(barrier) });
            Self { armed, entered: receive, release }
        }
        pub fn block(&self, r: &mut Running) {
            self.armed.store(true, std::sync::atomic::Ordering::Release);
            r.client.drain(0, 0);
            self.entered.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        pub fn release(&self) {
            let _ = self.release.try_send(());
        }
    }
    impl Drop for WorkerGate {
        fn drop(&mut self) {
            self.release();
        }
    }
    pub struct TestIds(AtomicU64);
    impl IdSource for TestIds {
        fn next_id(&self) -> Result<Id, WireError> {
            Ok(id(&format!("id{}", self.0.fetch_add(1, Ordering::Relaxed) + 1)))
        }
    }
    pub struct Lifecycle;
    impl ScopeLifecycle for Lifecycle {
        fn close_turn(&self, _: &TurnScope, _: CloseReason) {}
        fn retire_session(&self, _: &Id) {}
    }
    pub struct Dispatch(pub Box<dyn Fn(CallbackCall, ReplyPort) + Send + Sync>);
    impl HostDispatch for Dispatch {
        fn dispatch(&self, call: CallbackCall, reply: ReplyPort) {
            (self.0)(call, reply);
        }
    }
    pub fn services(pool: Arc<RuntimeAdmission>, dispatch: impl Fn(CallbackCall, ReplyPort) + Send + Sync + 'static) -> RuntimeServices {
        RuntimeServices {
            clock: Arc::new(TestClock),
            ids: Arc::new(TestIds(AtomicU64::new(0))),
            admission: pool,
            lifecycle: Arc::new(Lifecycle),
            dispatch: Arc::new(Dispatch(Box::new(dispatch))),
        }
    }
    pub fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn step(op: &str, value: Value) -> String {
        format!("{op} {}\n", hex(format!("{value}\n").as_bytes()))
    }
    pub fn request(method: &str, name: &str, scope: Scope, params: Value) -> Value {
        json!({"protocolVersion":"1.0","kind":"request","requestId":name,"scope":scope,"method":method,"params":params})
    }
    pub fn bootstrap(limits: &Limits) -> String {
        let hello = request("runtime.hello", "h:id2", Scope::Runtime, json!({"supportedVersions":["1.0"],"hostVersion":"1","limits":limits}));
        let mut result = fixture("runtime.hello").1;
        result["limits"] = json!(limits);
        result["capabilities"]["hostModelCalls"] = json!(true);
        result["capabilities"]["hostAgentTasks"] = json!(true);
        step("EXPECT_HEX", hello)
            + &step("WRITE_HEX", response("h:id2", Scope::Runtime, result))
            + &step("WRITE_HEX", event(1, "runtime.ready", Some("h:id2"), Scope::Runtime, json!({"runtimeInstanceId":"i"})))
    }
    pub fn session_script() -> String {
        step("EXPECT_HEX", request("session.start", "h:id3", Scope::Session(id("s")), fixture("session.start").0))
            + &step("WRITE_HEX", response("h:id3", Scope::Session(id("s")), fixture("session.start").1))
    }
    pub fn host_request(method: &str) -> HostRequest {
        let frame = request(
            method,
            "h:fixture",
            if method == "session.start" || method == "session.reset" { Scope::Session(id("s")) } else { scope() },
            fixture(method).0,
        );
        let DecodeResult::Frame(Frame::Request { body: RequestBody::Host(request), .. }) =
            decode_frame(format!("{frame}\n").as_bytes(), DecodePhase::Negotiated, Sender::Host, &Limits::default()).unwrap()
        else {
            panic!("host fixture")
        };
        request
    }
    pub struct Running {
        pub client: RuntimeClient,
        pub wake: Receiver<()>,
    }
    pub fn start(program: &super::PeerProgram, name: &str, script: &str, limits: Limits, services: RuntimeServices) -> Result<Running, ClientError> {
        let path = program.directory.join(name);
        std::fs::write(&path, script).unwrap();
        let (wake, receive) = sync_channel(1);
        let pool = services.admission.clone();
        let client = RuntimeClient::start(
            RuntimeLaunch {
                executable: program.executable.clone(),
                args: vec![path.into_os_string()],
                cwd: program.directory.clone(),
                host_version: "1".into(),
                limits,
            },
            services,
            Arc::new(move || {
                let _ = wake.try_send(());
            }),
        )?;
        program.track_runtime(pool);
        Ok(Running { client, wake: receive })
    }
    pub fn until(r: &mut Running, mut predicate: impl FnMut(&ClientNotice) -> bool) -> Vec<ClientNotice> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut all = Vec::new();
        loop {
            let batch = r.client.drain(usize::MAX, usize::MAX);
            let done = batch.iter().any(&mut predicate);
            let closed = batch.iter().any(|n| matches!(n, ClientNotice::TransportClosed { .. }));
            all.extend(batch);
            if done {
                return all;
            }
            assert!(!closed, "transport closed before expected notice: {all:?}");
            r.wake
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("worker channel watchdog: {e:?}; notices={all:?}"));
        }
    }
    pub fn ready(r: &mut Running) {
        until(r, |n| matches!(n, ClientNotice::RuntimeReady(_)));
    }
    pub fn submit(r: &Running, method: &str, s: Scope) -> RequestId {
        let sender = r.client.sender();
        let prepared = sender.prepare_request(s, host_request(method), Instant::now() + Duration::from_secs(60)).unwrap();
        sender.try_submit(prepared).unwrap()
    }
    pub fn session(r: &mut Running) {
        let id = submit(r, "session.start", Scope::Session(id("s")));
        until(r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&id));
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_nested_callback() {
    use mastra_worker_support::*;
    use serde_json::json;
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let callback = request("model.generate", "r:model", scope(), fixture("model.generate").0);
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", callback)
        + &step("EXPECT_HEX", response("r:model", scope(), fixture("model.generate").1))
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + "EXPECT_EOF\n";
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let mut r = start(
        &program,
        "nested",
        &script,
        l,
        services(Arc::new(RuntimeAdmission::new(8).unwrap()), move |call, reply| {
            assert!(matches!(call.call, HostCall::ModelGenerate(_)));
            assert!(!call.cancellation.is_cancelled());
            sent.send(call.token.clone()).unwrap();
            // All managed input is destroyed before the final token/port guard.
            drop(call);
            let result: ModelGenerateResult = serde_json::from_value(fixture("model.generate").1).unwrap();
            reply.finish(Ok(HostReply::ModelGenerate(result))).unwrap();
        }),
    )
    .unwrap();
    ready(&mut r);
    session(&mut r);
    let turn = submit(&r, "turn.start", scope());
    let notices = until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
    assert!(!notices.iter().any(|n| matches!(n, ClientNotice::TransportClosed { .. })));
    drop(received.recv_timeout(Duration::from_secs(10)).unwrap());
    let _ = json!(null);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_host_runtime_instance_cap() {
    use mastra_worker_support::*;
    struct Frozen(Instant);
    impl Clock for Frozen {
        fn now(&self) -> Instant {
            self.0
        }
    }
    let program = mastra_peer_program().unwrap();
    let pool = Arc::new(RuntimeAdmission::new(8).unwrap());
    let now = Instant::now();
    let make_services = || {
        let mut s = services(pool.clone(), |_, _| {});
        s.clock = Arc::new(Frozen(now));
        s
    };
    let mut clients = Vec::new();
    for n in 0..8 {
        let marker = program.directory.join(format!("cap-started-{n}"));
        clients.push(
            start(
                &program,
                &format!("cap{n}"),
                &format!("MARK {}\nHOLD_INPUT {}\nEXPECT_EOF\n", marker.display(), program.directory.join("cap-never-release").display()),
                Limits::default(),
                make_services(),
            )
            .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !(0..8).all(|n| program.directory.join(format!("cap-started-{n}")).exists()) {
        assert!(Instant::now() < deadline, "eight real peers must reach their barrier");
        std::thread::yield_now();
    }
    let ninth = program.directory.join("ninth-was-spawned");
    assert_eq!(
        start(
            &program,
            "ninth",
            &format!("MARK {}\nHOLD_INPUT {}\n", ninth.display(), program.directory.join("ninth-hold").display()),
            Limits::default(),
            make_services()
        )
        .err()
        .unwrap()
        .code,
        ErrorCode::ResourceExhausted
    );
    assert!(!ninth.exists(), "ninth reservation is refused BEFORE OS spawn");
    assert_eq!(pool.lower_limit(7).unwrap_err().code, ErrorCode::ResourceExhausted);
    drop(clients);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_lower_instance_cap_rejects_only_new_startup() {
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    let pool = Arc::new(RuntimeAdmission::new(8).unwrap());
    let hold = format!("HOLD_INPUT {}\n", program.directory.join("lower-never-release").display());
    let a = start(&program, "lower-existing-a", &hold, Limits::default(), services(pool.clone(), |_, _| {})).unwrap();
    let b = start(&program, "lower-existing-b", &hold, Limits::default(), services(pool.clone(), |_, _| {})).unwrap();
    let offered = Limits::default();
    let returned = Limits { runtime_instances: 1, ..offered.clone() };
    let mut script = bootstrap(&returned);
    let expected =
        request("runtime.hello", "h:id2", Scope::Runtime, serde_json::json!({"supportedVersions":["1.0"],"hostVersion":"1","limits":offered}));
    let first_line_end = script.find('\n').unwrap() + 1;
    script.replace_range(..first_line_end, &step("EXPECT_HEX", expected));
    script.push_str("EXPECT_EOF\n");
    let mut c = start(&program, "lower-rejected", &script, offered, services(pool.clone(), |_, _| {})).unwrap();
    let notices = until(&mut c, |n| matches!(n, ClientNotice::TransportClosed { .. }));
    assert!(!notices.iter().any(|n| matches!(n, ClientNotice::RuntimeReady(_))));
    assert_eq!(
        notices.iter().filter(|n| matches!(n,ClientNotice::RequestFinished {request_id,result:Err(_),..} if request_id.as_str()=="h:id2")).count(),
        1,
        "rejected startup still consumes its original hello completion exactly once"
    );
    assert_eq!(pool.lower_limit(1).unwrap_err().code, ErrorCode::ResourceExhausted, "existing reservations must not be killed to claim a lower cap");
    drop((a, b));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_equal_instance_cap_is_applied_before_ready() {
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    let pool = Arc::new(RuntimeAdmission::new(8).unwrap());
    let limits = Limits { runtime_instances: 1, ..Limits::default() };
    let release = program.directory.join("equal-cap-first-release");
    let script = bootstrap(&limits)
        + &format!("HOLD_INPUT {}\n", release.display())
        + &session_script()
        + &format!("HOLD_INPUT {}\n", program.directory.join("equal-cap-first-live").display());
    let mut first = start(&program, "equal-cap-first", &script, limits, services(pool.clone(), |_, _| {})).unwrap();
    let notices = until(&mut first, |notice| matches!(notice, ClientNotice::RuntimeReady(_)));
    assert!(notices.iter().any(|notice| matches!(notice, ClientNotice::RuntimeReady(info) if info.limits.runtime_instances == 1)));

    let spawned = program.directory.join("equal-cap-second-spawned");
    let second_script = format!("MARK {}\nHOLD_INPUT {}\n", spawned.display(), program.directory.join("equal-cap-second-hold").display());
    match start(&program, "equal-cap-second", &second_script, Limits::default(), services(pool.clone(), |_, _| {})) {
        Err(error) => assert_eq!(error.code, ErrorCode::ResourceExhausted),
        Ok(second) => {
            // Make the old implementation's real OS spawn observable, rather
            // than treating an untimed missing marker as evidence of refusal.
            let watchdog = Instant::now() + Duration::from_secs(10);
            while !spawned.exists() {
                assert!(Instant::now() < watchdog, "accepted second peer never reached its spawn barrier");
                std::thread::yield_now();
            }
            drop(second);
            panic!("a Ready runtime with accepted cap=1 allowed a second real OS spawn");
        }
    }
    assert!(!spawned.exists(), "ResourceExhausted must precede OS spawn");
    std::fs::write(release, b"continue first runtime").unwrap();
    session(&mut first); // The accepted first runtime remains live and usable.
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_equal_instance_cap_conflict_rejects_only_new_startup() {
    use mastra_worker_support::*;
    struct Resume(std::sync::mpsc::SyncSender<()>);
    impl Drop for Resume {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }
    let program = mastra_peer_program().unwrap();
    let pool = Arc::new(RuntimeAdmission::new(8).unwrap());
    let release_a = program.directory.join("equal-cap-old-a-release");
    let release_b = program.directory.join("equal-cap-old-b-release");
    let old_script = |release: &std::path::Path| {
        bootstrap(&Limits::default())
            + &format!("HOLD_INPUT {}\n", release.display())
            + &session_script()
            + &format!("HOLD_INPUT {}\n", program.directory.join("equal-cap-old-still-live").display())
    };
    let mut a = start(&program, "equal-cap-old-a", &old_script(&release_a), Limits::default(), services(pool.clone(), |_, _| {})).unwrap();
    let mut b = start(&program, "equal-cap-old-b", &old_script(&release_b), Limits::default(), services(pool.clone(), |_, _| {})).unwrap();
    ready(&mut a);
    ready(&mut b); // Two actual, live, already-negotiated host reservations.

    let limits = Limits { runtime_instances: 1, ..Limits::default() };
    let hello_seen = program.directory.join("equal-cap-conflict-hello-observed");
    let mut script = bootstrap(&limits);
    let end = script.find('\n').unwrap() + 1;
    script.insert_str(end, &format!("MARK {}\n", hello_seen.display()));
    script.push_str(&format!("HOLD_INPUT {}\n", program.directory.join("equal-cap-new-held-live").display()));
    let path = program.directory.join("equal-cap-conflict");
    std::fs::write(&path, script).unwrap();
    let (wake, receive) = std::sync::mpsc::sync_channel(1);
    let (entered, observed) = std::sync::mpsc::sync_channel(1);
    let (resume, barrier) = std::sync::mpsc::sync_channel(1);
    let resume = Resume(resume);
    let barrier = Mutex::new(barrier);
    let first_repaint = std::sync::atomic::AtomicBool::new(true);
    let client = RuntimeClient::start(
        RuntimeLaunch {
            executable: program.executable.clone(),
            args: vec![path.into_os_string()],
            cwd: program.directory.clone(),
            host_version: "1".into(),
            limits,
        },
        services(pool.clone(), |_, _| {}),
        Arc::new(move || {
            let _ = wake.try_send(());
            if first_repaint.swap(false, std::sync::atomic::Ordering::AcqRel) {
                entered.send(()).unwrap();
                let _ = barrier.lock().unwrap().recv();
            }
        }),
    )
    .unwrap();
    program.track_runtime(pool.clone());
    let mut rejected = Running { client, wake: receive };
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(hello_seen.exists(), "the cap is checked only AFTER a real successful hello, not on the unnegotiated launch offer");
    let mut notices = rejected.client.drain(usize::MAX, usize::MAX);
    // The worker is held in its first notice callback, before StopTree. The
    // child is held by its protocol barrier, so cleanup cannot yet be Ok.
    assert_eq!(
        pool.lower_limit(2).unwrap_err().code,
        ErrorCode::ResourceExhausted,
        "the new runtime must retain its original permit before owned cleanup"
    );
    drop(resume);
    notices.extend(until(&mut rejected, |notice| matches!(notice, ClientNotice::RuntimeReady(_) | ClientNotice::TransportClosed { .. })));
    assert!(
        !notices.iter().any(|notice| matches!(notice, ClientNotice::RuntimeReady(_))),
        "a conflicting accepted cap must not publish Ready: {notices:?}"
    );
    assert_eq!(
        notices
            .iter()
            .filter(|notice| matches!(notice, ClientNotice::RequestFinished { request_id, result: Err(_), .. } if request_id.as_str() == "h:id2"))
            .count(),
        1
    );
    assert_eq!(notices.iter().filter(|notice| matches!(notice, ClientNotice::TransportClosed { .. })).count(), 1);
    let watchdog = Instant::now() + Duration::from_secs(15);
    loop {
        match pool.lower_limit(2) {
            Ok(()) => break,
            Err(error) => {
                assert_eq!(error.code, ErrorCode::ResourceExhausted);
                assert!(Instant::now() < watchdog, "original owner never established cleanup");
                std::thread::yield_now();
            }
        }
    }
    std::fs::write(release_a, b"continue original a").unwrap();
    std::fs::write(release_b, b"continue original b").unwrap();
    session(&mut a);
    session(&mut b); // Rejecting this startup did not kill either old instance.
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_launch_and_drop_cleanup() {
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    let pool = Arc::new(RuntimeAdmission::new(1).unwrap());
    let bad = RuntimeLaunch {
        executable: "relative".into(),
        args: vec![],
        cwd: program.directory.clone(),
        host_version: "1".into(),
        limits: Limits::default(),
    };
    assert_eq!(RuntimeClient::start(bad, services(pool.clone(), |_, _| {}), Arc::new(|| {})).err().unwrap().code, ErrorCode::TransportError);
    let helper = program.directory.join("worker-owned-helper");
    let marker = program.directory.join("worker-helper-pid");
    std::fs::write(&helper, format!("HOLD_INPUT {}\n", program.directory.join("helper-never-release").display())).unwrap();
    let script = format!(
        "SPAWN_HELPER_TRACKED {} {}\nHOLD_INPUT {}\n",
        hex(helper.to_string_lossy().as_bytes()),
        hex(marker.to_string_lossy().as_bytes()),
        program.directory.join("worker-never-release").display()
    );
    let r = start(&program, "drop", &script, Limits::default(), services(pool.clone(), |_, _| {})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let helper_pid: u32 = std::fs::read_to_string(marker).unwrap().parse().unwrap();
    drop(r);
    // Actual cleanup must release this reservation; lower_limit cannot fake it.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let launch = RuntimeLaunch {
            executable: program.directory.join("never-an-executable"),
            args: vec![],
            cwd: program.directory.clone(),
            host_version: "1".into(),
            limits: Limits::default(),
        };
        match RuntimeClient::start(launch, services(pool.clone(), |_, _| {}), Arc::new(|| {})) {
            Err(e) if e.code == ErrorCode::TransportError => break,
            Err(e) => {
                assert_eq!(e.code, ErrorCode::ResourceExhausted);
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            Ok(_) => panic!("invalid launch"),
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{helper_pid}/stat")) {
        assert!(stat.rsplit(')').next().is_some_and(|s| s.trim_start().starts_with('Z')), "worker Drop left a live descendant");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = helper_pid;
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_eof_and_replacement() {
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    for (name, ending) in [("eof", "EXIT 0\n"), ("partial", "WRITE_HEX 7b\nEXIT 0\n"), ("malformed", "WRITE_HEX 7b7d0a\nEXIT 0\n")] {
        let l = Limits::default();
        let mut r = start(&program, name, &(bootstrap(&l) + ending), l, services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {})).unwrap();
        let notices = until(&mut r, |n| matches!(n, ClientNotice::TransportClosed { .. }));
        assert_eq!(notices.iter().filter(|n| matches!(n, ClientNotice::TransportClosed { .. })).count(), 1);
        assert!(r.client.drain(usize::MAX, usize::MAX).is_empty());
        assert_eq!(
            r.client.sender().prepare_request(Scope::Session(id("s")), host_request("session.start"), Instant::now()).unwrap_err().code,
            ErrorCode::TransportError
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_backpressure_preserves_control() {
    use mastra_worker_support::*;
    struct CheckedClock {
        ui: std::thread::ThreadId,
        deny: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Clock for CheckedClock {
        fn now(&self) -> Instant {
            assert!(!(self.deny.load(std::sync::atomic::Ordering::Acquire) && std::thread::current().id() == self.ui), "UI called Clock");
            Instant::now()
        }
    }
    struct CheckedIds {
        ui: std::thread::ThreadId,
        deny: Arc<std::sync::atomic::AtomicBool>,
        next: std::sync::atomic::AtomicU64,
    }
    impl IdSource for CheckedIds {
        fn next_id(&self) -> Result<Id, WireError> {
            assert!(!(self.deny.load(std::sync::atomic::Ordering::Acquire) && std::thread::current().id() == self.ui), "UI called IdSource");
            Ok(id(&format!("id{}", self.next.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1)))
        }
    }
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", request("model.generate", "r:block", scope(), fixture("model.generate").0))
        + "EXPECT_EOF\n";
    let (entered, observed) = std::sync::mpsc::sync_channel(1);
    let (release, barrier) = std::sync::mpsc::sync_channel(1);
    let barrier = Mutex::new(barrier);
    let held = Arc::new(Mutex::new(None));
    let store = held.clone();
    let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
        *store.lock().unwrap() = Some((call, port));
        entered.send(()).unwrap();
        barrier.lock().unwrap().recv().unwrap();
    });
    let deny = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ui = std::thread::current().id();
    services.clock = Arc::new(CheckedClock { ui, deny: deny.clone() });
    services.ids = Arc::new(CheckedIds { ui, deny: deny.clone(), next: std::sync::atomic::AtomicU64::new(0) });
    let mut r = start(&program, "backpressure", &script, l, services).unwrap();
    ready(&mut r);
    session(&mut r);
    submit(&r, "turn.start", scope());
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    let sender = r.client.sender();
    let event =
        || HostEvent::ConnectionState(ConnectionStateEvent { connection_id: id("c"), state: ConnectionState::Available, auth_methods: vec![] });
    let mut prepared = Vec::new();
    for _ in 0..33 {
        prepared.push(sender.prepare_event(scope(), event(), None).unwrap());
    }
    let refused = sender.prepare_event(scope(), event(), None).unwrap();
    deny.store(true, std::sync::atomic::Ordering::Release);
    // A live callback's ready drop-fallback and any unconsumed Written metadata
    // retain their ORIGINAL data slots; they are not a free second queue.
    let mut admitted = 0;
    for p in prepared {
        match sender.try_emit(p) {
            Ok(()) => admitted += 1,
            Err(e) => {
                assert_eq!(e.code, ErrorCode::ResourceExhausted);
                break;
            }
        }
    }
    assert!((1..=32).contains(&admitted));
    assert_eq!(sender.try_emit(refused).unwrap_err().code, ErrorCode::ResourceExhausted);
    let Scope::Turn(s) = scope() else { panic!() };
    sender.cancel_turn(&s, CancelReason::User).unwrap();
    assert!(sender.cancel_turn(&s, CancelReason::Deadline).is_ok(), "repeated cancellation coalesces without replacing the original User cause");
    assert!(held.lock().unwrap().as_ref().unwrap().0.cancellation.is_cancelled(), "local Stop does not wait for dispatcher or pipe");
    assert_eq!(
        sender.shutdown(ShutdownReason::AppExit).unwrap_err().code,
        ErrorCode::ResourceExhausted,
        "full control cell forces cleanup without reopening cancellation"
    );
    assert!(r.client.drain(usize::MAX, 1).is_empty());
    release.send(()).unwrap();
    drop(held.lock().unwrap().take());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_inbound_credit_tracks_async_owners() {
    use mastra_worker_support::*;
    use serde_json::json;
    struct Owner {
        payload: HostCall,
        scope: Scope,
        cancellation: Cancellation,
        // Last guard, AFTER all managed input/header fields. No Drop obstructs moves.
        guard: CallbackToken,
    }
    let program = mastra_peer_program().unwrap();
    for mode in ["normal", "refusal", "fallback", "diagnostic_eof"] {
        for retain in [false, true] {
            let name = format!("owner-{mode}-{retain}");
            let marker = program.directory.join(format!("{name}-replied"));
            let release = program.directory.join(format!("{name}-release"));
            let l = Limits { frame_bytes: 8192, queue_bytes: 16384, control_lane_bytes: 4096, text_bytes: 8192, ..Limits::default() };
            let mut params = fixture("model.generate").0;
            params["messages"][0]["content"][0]["text"] = json!("x".repeat(7000));
            params["options"]["timeoutSeconds"] = json!(60);
            let explicit = WireError {
                code: ErrorCode::AuthRequired,
                message: "拒绝".repeat(300),
                retry: Retry::Never,
                details: ErrorDetails::Empty(EmptyDetails {}),
            };
            let first_reply = if matches!(mode, "normal" | "diagnostic_eof") {
                response("r:first", scope(), fixture("model.generate").1)
            } else {
                let error = if mode == "refusal" {
                    explicit.clone()
                } else {
                    WireError {
                        code: ErrorCode::InternalError,
                        message: "Runtime transport closed.".into(),
                        retry: Retry::Never,
                        details: ErrorDetails::Empty(EmptyDetails {}),
                    }
                };
                json!({"protocolVersion":"1.0","kind":"response","requestId":"r:first","scope":scope(),"outcome":"error","error":error})
            };
            let mut script = bootstrap(&l)
                + &session_script()
                + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
                + &step("WRITE_HEX", request("model.generate", "r:first", scope(), params.clone()))
                + &step("EXPECT_HEX", first_reply)
                + &format!("MARK {}\nHOLD_INPUT {}\n", marker.display(), release.display())
                + &step("WRITE_HEX", request("model.generate", "r:second", scope(), params))
                + &step("EXPECT_HEX", response("r:second", scope(), fixture("model.generate").1))
                + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
                + "EXPECT_EOF\n";
            if mode == "diagnostic_eof" {
                let hold = format!("HOLD_INPUT {}\n", release.display());
                let end = script.find(&hold).unwrap() + hold.len();
                script.truncate(end);
                script.push_str(&"STDERR_HEX 78 400\nSTDERR_HEX 0a\n".repeat(40));
                script.push_str("EXIT 0\n");
            }
            let (owned, received) = std::sync::mpsc::sync_channel(1);
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls = count.clone();
            let mut r = start(
                &program,
                &name,
                &script,
                l,
                services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
                    let n = calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    if n == 0 {
                        // Actual trait adapter moves the ORIGINAL payload and guard,
                        // then RETURNS. Port completion does not consume this owner.
                        let CallbackCall { call: payload, scope, cancellation, token: guard, .. } = call;
                        owned.send(Owner { payload, scope, cancellation, guard }).unwrap();
                        match mode {
                            "normal" | "diagnostic_eof" => {
                                port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(fixture("model.generate").1).unwrap()))).unwrap()
                            }
                            "refusal" => port.finish(Err(explicit.clone())).unwrap(),
                            _ => drop(port),
                        }
                    } else {
                        drop(call);
                        port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(fixture("model.generate").1).unwrap()))).unwrap();
                    }
                }),
            )
            .unwrap();
            ready(&mut r);
            session(&mut r);
            submit(&r, "turn.start", scope());
            let owner = received.recv_timeout(Duration::from_secs(10)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !marker.exists() {
                assert!(Instant::now() < deadline, "peer reply barrier");
                std::thread::yield_now();
            }
            assert_eq!(owner.scope, scope());
            assert!(matches!(&owner.payload,HostCall::ModelGenerate(p) if p.messages.len()==1));
            assert!(!owner.cancellation.is_cancelled());
            let _ = &owner.guard;
            let mut held = Some(owner);
            if !retain {
                drop(held.take());
            }
            std::fs::write(release, b"go").unwrap();
            if retain || mode == "diagnostic_eof" {
                let notices = until(&mut r, |n| matches!(n, ClientNotice::TransportClosed { .. }));
                assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 1, "retained async input prevents an independent Q refill");
                if let Some(held) = &held {
                    assert!(held.cancellation.is_cancelled());
                }
                if mode == "diagnostic_eof" {
                    let tail = notices
                        .iter()
                        .find_map(|n| if let ClientNotice::TransportClosed { stderr_tail, .. } = n { Some(stderr_tail) } else { None })
                        .unwrap();
                    assert!(!tail.is_empty());
                    assert!(tail.iter().map(String::len).sum::<usize>() < 16000, "diagnostics respect already-owned input/info, not a second Q");
                }
                assert_eq!(notices.iter().filter(|n| matches!(n, ClientNotice::TransportClosed { .. })).count(), 1);
                drop(held.take());
                assert!(r.client.drain(usize::MAX, usize::MAX).is_empty());
            } else {
                let notices =
                    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id.as_str()=="h:id4"));
                assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 2, "actual input+guard destruction permits the next large call");
                assert!(!notices.iter().any(|n| matches!(n, ClientNotice::TransportClosed { .. })));
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_written_is_not_execution() {
    use super::process::{AgentProcess, PipeLane, ProcessEvent, ProcessIoLimits, ProcessLaunch};
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    let release = program.directory.join("execution-release");
    let executed = program.directory.join("execution-marker");
    let path = program.directory.join("written-not-executed");
    std::fs::write(
        &path,
        format!("HOLD_INPUT {}\nEXPECT_HEX 7b7d0a\nMARK {}\nWRITE_HEX 6f6b0a\nEXPECT_EOF\n", release.display(), executed.display()),
    )
    .unwrap();
    let (wake, events) = std::sync::mpsc::sync_channel(8);
    let mut p = AgentProcess::spawn_strict(
        &ProcessLaunch { executable: program.executable.clone(), args: vec![path.into_os_string()], cwd: program.directory.clone() },
        ProcessIoLimits {
            frame_bytes: 8192,
            queue_bytes: 16384,
            control_lane_bytes: 4096,
            stderr_lines: 40,
            stderr_line_scalars: 400,
            stderr_bytes: 65536,
        },
        move || {
            let _ = wake.try_send(());
        },
    )
    .unwrap();
    program.track(p.handle());
    p.try_send_tracked(77, b"{}\n".to_vec(), PipeLane::Data).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match p.try_recv() {
            Ok(Some(ProcessEvent::Written(ticket))) => {
                assert_eq!(ticket, 77);
                break;
            }
            Ok(None) => {
                events.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(!executed.exists(), "local Written while peer has not read/parsed/executed");
    std::fs::write(release, b"go").unwrap();
    loop {
        match p.try_recv() {
            Ok(Some(ProcessEvent::Frame(frame))) => {
                assert_eq!(&*frame, b"ok\n");
                break;
            }
            Ok(None) => {
                events.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(executed.exists());
    p.shutdown(true);
    assert_eq!(p.handle().try_cleanup_result(), Some(Ok(())));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_prepared_is_bound_to_native_client() {
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let script = bootstrap(&l) + &session_script() + "EXPECT_EOF\n";
    let mut old = start(&program, "prep-old", &script, l.clone(), services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {})).unwrap();
    ready(&mut old);
    session(&mut old);
    let prepared = old.client.sender().prepare_request(scope(), host_request("turn.start"), Instant::now() + Duration::from_secs(60)).unwrap();
    let mut new = start(&program, "prep-new", &script, l, services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {})).unwrap();
    ready(&mut new);
    session(&mut new);
    // Injected visible IDs intentionally repeat. The native client capability
    // must nevertheless differ; no old frame may reach the replacement pipe.
    assert_eq!(new.client.sender().try_submit(prepared).unwrap_err().code, ErrorCode::InvalidRequest);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_inbound_credit_cancel_retire_and_generation() {
    use mastra_worker_support::*;
    use serde_json::json;
    let program = mastra_peer_program().unwrap();
    for reset in [false, true] {
        let l = Limits { frame_bytes: 8192, queue_bytes: 16384, control_lane_bytes: 4096, text_bytes: 8192, ..Limits::default() };
        let mut params = fixture("model.generate").0;
        params["messages"][0]["content"][0]["text"] = json!("x".repeat(7000));
        params["options"]["timeoutSeconds"] = json!(60);
        let first = program.directory.join(format!("control-first-{reset}"));
        let release = program.directory.join(format!("control-release-{reset}"));
        let method = if reset { "session.reset" } else { "turn.cancel" };
        let control_scope = if reset { Scope::Session(id("s")) } else { scope() };
        let script = bootstrap(&l)
            + &session_script()
            + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
            + &step("WRITE_HEX", request("model.generate", "r:first", scope(), params.clone()))
            + &step("EXPECT_HEX", response("r:first", scope(), fixture("model.generate").1))
            + &format!("MARK {}\n", first.display())
            + &step("EXPECT_HEX", request(method, "h:id5", control_scope.clone(), fixture(method).0))
            + &step("WRITE_HEX", response("h:id5", control_scope, fixture(method).1))
            + &format!("HOLD_INPUT {}\n", release.display())
            + &step("WRITE_HEX", request("model.generate", "r:retired", scope(), params.clone()))
            + "EXPECT_EOF\n";
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let mut old = start(
            &program,
            &format!("control-old-{reset}"),
            &script,
            l.clone(),
            services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
                send.send(call).unwrap();
                port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(fixture("model.generate").1).unwrap()))).unwrap();
            }),
        )
        .unwrap();
        ready(&mut old);
        session(&mut old);
        submit(&old, "turn.start", scope());
        let held = received.recv_timeout(Duration::from_secs(10)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !first.exists() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let sender = old.client.sender();
        let stale = sender
            .prepare_event(
                scope(),
                HostEvent::ConnectionState(ConnectionStateEvent { connection_id: id("c"), state: ConnectionState::Available, auth_methods: vec![] }),
                None,
            )
            .unwrap();
        if reset {
            let p = sender.prepare_request(Scope::Session(id("s")), host_request(method), Instant::now() + Duration::from_secs(60)).unwrap();
            sender.reset(p).unwrap();
        } else {
            let Scope::Turn(s) = scope() else { panic!() };
            sender.cancel_turn(&s, CancelReason::User).unwrap();
        }
        assert!(held.cancellation.is_cancelled(), "actual Cancellation closes BEFORE worker/control admission");
        let notices = until(&mut old, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id.as_str()=="h:id5"));
        assert_eq!(notices.iter().filter(|n| matches!(n, ClientNotice::TurnClosed { .. })).count(), 1);
        assert_eq!(sender.try_emit(stale).unwrap_err().code, ErrorCode::InvalidRequest);
        std::fs::write(release, b"go").unwrap();
        until(&mut old, |n| matches!(n, ClientNotice::TransportClosed { .. }));
        // A replacement has a NEW budget even with repeated visible IDs. Old
        // input remains held while the replacement admits its first large call.
        let next_release = program.directory.join(format!("next-release-{reset}"));
        let script = bootstrap(&l)
            + &session_script()
            + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
            + &step("WRITE_HEX", request("model.generate", "r:first", scope(), params.clone()))
            + &format!("HOLD_INPUT {}\n", next_release.display())
            + &step("WRITE_HEX", request("model.generate", "r:second", scope(), params))
            + "EXPECT_EOF\n";
        let (send, next) = std::sync::mpsc::sync_channel(1);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let mut new = start(
            &program,
            &format!("control-new-{reset}"),
            &script,
            l,
            services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
                count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                send.send((call, port)).unwrap();
            }),
        )
        .unwrap();
        ready(&mut new);
        session(&mut new);
        submit(&new, "turn.start", scope());
        let (new_call, new_port) = next.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(held);
        std::fs::write(next_release, b"go").unwrap();
        until(&mut new, |n| matches!(n, ClientNotice::TransportClosed { .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Acquire), 1, "old refund must not fund the new generation's second frame");
        assert!(new_call.cancellation.is_cancelled());
        assert_eq!(
            new_port
                .finish(Err(WireError {
                    code: ErrorCode::Cancelled,
                    message: "late".into(),
                    retry: Retry::Never,
                    details: ErrorDetails::Empty(EmptyDetails {})
                }))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        drop(new_call);
        assert!(new.client.drain(usize::MAX, usize::MAX).is_empty());
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_shutdown_writes_before_closing_input() {
    use mastra_worker_support::*;
    use serde_json::json;
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("runtime.shutdown", "h:id4", Scope::Runtime, json!({"reason":"app_exit"})))
        + "EXPECT_EOF\n"
        + &step("WRITE_HEX", response("h:id4", Scope::Runtime, fixture("runtime.shutdown").1))
        + "EXIT 0\n";
    let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {});
    let gate = WorkerGate::install(&mut services);
    let mut r = start(&program, "graceful", &script, l, services).unwrap();
    ready(&mut r);
    session(&mut r);
    gate.block(&mut r);
    r.client.sender().shutdown(ShutdownReason::AppExit).unwrap();
    gate.release();
    let notices = until(&mut r, |n| matches!(n, ClientNotice::TransportClosed { .. }));
    assert!(notices.iter().any(
        |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(RuntimeReply::RuntimeShutdown(_)),..} if request_id.as_str()=="h:id4")
    ));
    assert!(notices.iter().any(|n| matches!(n, ClientNotice::TransportClosed { cause: TransportCause::ExpectedEof, .. })));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_stop_before_worker_admission_never_writes_turn() {
    use mastra_worker_support::*;
    struct ClockBarrier {
        armed: Arc<std::sync::atomic::AtomicBool>,
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Clock for ClockBarrier {
        fn now(&self) -> Instant {
            if self.armed.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            Instant::now()
        }
    }
    let program = mastra_peer_program().unwrap();
    for cancel_request in [false, true] {
        let l = Limits::default();
        let script = bootstrap(&l) + &session_script() + "EXPECT_EOF\n";
        let (entered, observed) = std::sync::mpsc::sync_channel(1);
        let (release, barrier) = std::sync::mpsc::sync_channel(1);
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {});
        services.clock = Arc::new(ClockBarrier { armed: armed.clone(), entered, release: Mutex::new(barrier) });
        let mut r = start(&program, &format!("stop-before-admission-{cancel_request}"), &script, l, services).unwrap();
        ready(&mut r);
        session(&mut r);
        let sender = r.client.sender();
        let turn = sender.prepare_request(scope(), host_request("turn.start"), Instant::now() + Duration::from_secs(60)).unwrap();
        armed.store(true, std::sync::atomic::Ordering::Release);
        let request_id = sender.try_submit(turn).unwrap();
        observed.recv_timeout(Duration::from_secs(10)).unwrap();
        let Scope::Turn(s) = scope() else { panic!() };
        if cancel_request {
            sender.cancel_request(&request_id).unwrap();
        } else {
            sender.cancel_turn(&s, CancelReason::User).unwrap();
        }
        release.send(()).unwrap();
        let notices = until(&mut r, |n| matches!(n, ClientNotice::TurnClosed { .. }));
        assert!(
            notices.iter().any(|n| matches!(n, ClientNotice::TurnClosed { cause: CloseReason::User, .. })),
            "Stop wins before any pipe admission: {notices:?}"
        );
        let mut all = notices;
        if !all
            .iter()
            .any(|n| matches!(n,ClientNotice::RequestFinished {request_id:id,result:Err(e),..} if id==&request_id&&e.code==ErrorCode::Cancelled))
        {
            all.extend(until(
                &mut r,
                |n| matches!(n,ClientNotice::RequestFinished {request_id:id,result:Err(e),..} if id==&request_id&&e.code==ErrorCode::Cancelled),
            ));
        }
        assert!(!all.iter().any(|n| matches!(n, ClientNotice::TransportClosed { .. })));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match sender.complete_turn(&s, TurnTerminal::Cancelled) {
                Ok(()) => break,
                Err(e) => {
                    assert_eq!(e.code, ErrorCode::ResourceExhausted);
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_os_ids_use_native_entropy() {
    use mastra_worker_support::*;
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..32 {
        let id = OsIds.next_id().unwrap();
        assert_eq!(id.as_str().len(), 32);
        assert!(id.as_str().bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert!(ids.insert(id.as_str().to_owned()));
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_large_frame_fits_without_a_double_frame_floor() {
    use mastra_worker_support::*;
    use serde_json::json;
    let program = mastra_peer_program().unwrap();
    let l = Limits { frame_bytes: 8192, queue_bytes: 12288, control_lane_bytes: 4096, text_bytes: 8192, ..Limits::default() };
    let mut params = fixture("turn.start").0;
    params["parts"][0]["text"] = json!("x".repeat(7000));
    let wire = request("turn.start", "h:id4", scope(), params.clone());
    assert!(format!("{wire}\n").len() <= 8192);
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", wire)
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + "EXPECT_EOF\n";
    let mut r = start(&program, "large-frame", &script, l, services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {})).unwrap();
    ready(&mut r);
    session(&mut r);
    let sender = r.client.sender();
    let p = sender
        .prepare_request(scope(), HostRequest::TurnStart(Box::new(serde_json::from_value(params).unwrap())), Instant::now() + Duration::from_secs(60))
        .unwrap();
    let id = sender.try_submit(p).unwrap();
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&id));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_id_failure_is_visible_without_counter_fallback() {
    use mastra_worker_support::*;
    struct FailingIds(std::sync::atomic::AtomicUsize);
    impl IdSource for FailingIds {
        fn next_id(&self) -> Result<Id, WireError> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                Ok(id("generation"))
            } else {
                Err(WireError {
                    code: ErrorCode::InternalError,
                    message: "redacted".into(),
                    retry: Retry::Never,
                    details: ErrorDetails::Empty(EmptyDetails {}),
                })
            }
        }
    }
    let program = mastra_peer_program().unwrap();
    let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {});
    services.ids = Arc::new(FailingIds(std::sync::atomic::AtomicUsize::new(0)));
    let mut r = start(&program, "id-failed", "EXPECT_EOF\n", Limits::default(), services).unwrap();
    let notices = until(&mut r, |n| matches!(n, ClientNotice::TransportClosed { .. }));
    assert!(
        notices.iter().any(|n| matches!(n,ClientNotice::TransportClosed {cause:TransportCause::Error(e),..} if e.code==ErrorCode::InternalError)),
        "ID-source failure remains visible, with no substitute hello ID: {notices:?}"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_lifecycle_retirement_precedes_notices() {
    use mastra_worker_support::*;
    struct Hooks {
        sender: Mutex<Option<RuntimeSender>>,
        close: std::sync::atomic::AtomicBool,
        retired: std::sync::atomic::AtomicBool,
    }
    impl Hooks {
        fn unlocked(&self) {
            let sender = self.sender.lock().unwrap().clone().unwrap();
            assert_ne!(
                sender.cancel_request(&rid("h:never-admitted")).unwrap_err().code,
                ErrorCode::ResourceExhausted,
                "lifecycle hook must run without mailbox lock"
            );
        }
    }
    impl ScopeLifecycle for Hooks {
        fn close_turn(&self, _: &TurnScope, reason: CloseReason) {
            assert_eq!(reason, CloseReason::Reset);
            self.unlocked();
            self.close.store(true, std::sync::atomic::Ordering::Release);
        }
        fn retire_session(&self, _: &Id) {
            self.unlocked();
            self.retired.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + &step("EXPECT_HEX", request("session.reset", "h:id5", Scope::Session(id("s")), fixture("session.reset").0))
        + &step("WRITE_HEX", response("h:id5", Scope::Session(id("s")), fixture("session.reset").1))
        + "EXPECT_EOF\n";
    let pool = Arc::new(RuntimeAdmission::new(1).unwrap());
    let hooks = Arc::new(Hooks {
        sender: Mutex::new(None),
        close: std::sync::atomic::AtomicBool::new(false),
        retired: std::sync::atomic::AtomicBool::new(false),
    });
    let mut services = services(pool.clone(), |_, _| {});
    services.lifecycle = hooks.clone();
    let gate = WorkerGate::install(&mut services);
    let path = program.directory.join("lifecycle-order");
    std::fs::write(&path, script).unwrap();
    let (wake, receive) = std::sync::mpsc::sync_channel(1);
    let observed = hooks.clone();
    let violation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed = violation.clone();
    let client = RuntimeClient::start(
        RuntimeLaunch {
            executable: program.executable.clone(),
            args: vec![path.into_os_string()],
            cwd: program.directory.clone(),
            host_version: "1".into(),
            limits: l,
        },
        services,
        Arc::new(move || {
            if observed.close.load(std::sync::atomic::Ordering::Acquire) && !observed.retired.load(std::sync::atomic::Ordering::Acquire) {
                failed.store(true, std::sync::atomic::Ordering::Release);
            }
            let _ = wake.try_send(());
        }),
    )
    .unwrap();
    program.track_runtime(pool);
    let mut r = Running { client, wake: receive };
    *hooks.sender.lock().unwrap() = Some(r.client.sender());
    ready(&mut r);
    session(&mut r);
    let turn = submit(&r, "turn.start", scope());
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
    let sender = r.client.sender();
    let p = sender.prepare_request(Scope::Session(id("s")), host_request("session.reset"), Instant::now() + Duration::from_secs(60)).unwrap();
    gate.block(&mut r);
    let reset = sender.reset(p).unwrap();
    gate.release();
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&reset));
    assert!(hooks.retired.load(std::sync::atomic::Ordering::Acquire));
    assert!(!violation.load(std::sync::atomic::Ordering::Acquire), "retirement must finish before the first following notice/repaint");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_cancel_deadline_stops_tree_without_an_extra_shutdown_window() {
    use mastra_worker_support::*;
    use serde_json::json;
    struct VirtualClock {
        base: Instant,
        seconds: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Clock for VirtualClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_secs(self.seconds.load(std::sync::atomic::Ordering::Acquire))
        }
    }
    struct IdBarrier {
        next: std::sync::atomic::AtomicU64,
        cancel: std::sync::mpsc::SyncSender<()>,
    }
    impl IdSource for IdBarrier {
        fn next_id(&self) -> Result<Id, WireError> {
            let n = self.next.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            if n == 5 {
                self.cancel.send(()).unwrap();
            }
            Ok(id(&format!("id{n}")))
        }
    }
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let marker = program.directory.join("large-writer-started");
    let auth: Vec<_> = (0..4).map(|n| json!({"id":format!("auth{n}"),"name":"test","description":"x".repeat(200000)})).collect();
    let data = json!({"connectionId":"c","state":"available","authMethods":auth});
    let wire = event(1, "connection.state", None, scope(), data.clone());
    let raw = format!("{wire}\n").into_bytes();
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + &format!(
            "EXPECT_HEX {}\nMARK {}\nHOLD_INPUT {}\n",
            hex(&raw[..128]),
            marker.display(),
            program.directory.join("never-read-large-remainder").display()
        );
    let seconds = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (issued, cancel) = std::sync::mpsc::sync_channel(1);
    let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {});
    services.clock = Arc::new(VirtualClock { base: Instant::now(), seconds: seconds.clone() });
    services.ids = Arc::new(IdBarrier { next: std::sync::atomic::AtomicU64::new(0), cancel: issued });
    let mut r = start(&program, "cancel-deadline", &script, l, services).unwrap();
    ready(&mut r);
    session(&mut r);
    let turn = submit(&r, "turn.start", scope());
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
    let sender = r.client.sender();
    let event = HostEvent::ConnectionState(serde_json::from_value(data).unwrap());
    let p = sender.prepare_event(scope(), event, None).unwrap();
    sender.try_emit(p).unwrap();
    let watchdog = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(Instant::now() < watchdog);
        std::thread::yield_now();
    }
    let Scope::Turn(s) = scope() else { panic!() };
    sender.cancel_turn(&s, CancelReason::User).unwrap();
    cancel.recv_timeout(Duration::from_secs(10)).unwrap();
    seconds.store(5, std::sync::atomic::Ordering::Release);
    r.client.drain(0, 0);
    // Virtual time deliberately NEVER reaches eight. The ORIGINAL owner must
    // stop the tree at the five-second ack boundary, not start another RPC wait.
    let notices = until(&mut r, |n| matches!(n, ClientNotice::TransportClosed { .. }));
    assert!(notices.iter().any(|n| matches!(n, ClientNotice::TurnClosed { cause: CloseReason::User, .. })));
    assert!(notices.iter().any(
        |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Err(e),..} if request_id.as_str()=="h:id5"&&e.code==ErrorCode::Timeout)
    ));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_stderr_pressure_preserves_ready_callbacks() {
    use mastra_worker_support::*;
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let mut script = bootstrap(&l) + &session_script() + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0));
    for n in 0..8 {
        let id = format!("r:pressure{n}");
        script.push_str("STDERR_HEX 780a 1048576\n");
        script.push_str(&step("WRITE_HEX", request("model.generate", &id, scope(), fixture("model.generate").0)));
        script.push_str(&step("EXPECT_HEX", response(&id, scope(), fixture("model.generate").1)));
    }
    script.push_str(&step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1)));
    script.push_str("EXPECT_EOF\n");
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = count.clone();
    let mut r = start(
        &program,
        "stderr-pressure",
        &script,
        l,
        services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
            drop(call);
            calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(fixture("model.generate").1).unwrap()))).unwrap();
        }),
    )
    .unwrap();
    ready(&mut r);
    session(&mut r);
    let turn = submit(&r, "turn.start", scope());
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
    assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 8);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_coalesced_cancel_reuses_original_identity() {
    use mastra_worker_support::*;
    struct ClockGate {
        armed: Arc<std::sync::atomic::AtomicBool>,
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Clock for ClockGate {
        fn now(&self) -> Instant {
            if self.armed.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            Instant::now()
        }
    }
    struct FiveIds(Arc<std::sync::atomic::AtomicUsize>);
    impl IdSource for FiveIds {
        fn next_id(&self) -> Result<Id, WireError> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            if n > 5 {
                return Err(WireError {
                    code: ErrorCode::InternalError,
                    message: "no more entropy".into(),
                    retry: Retry::Never,
                    details: ErrorDetails::Empty(EmptyDetails {}),
                });
            }
            Ok(id(&format!("id{n}")))
        }
    }
    let program = mastra_peer_program().unwrap();
    let l = Limits::default();
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + &step("EXPECT_HEX", request("turn.cancel", "h:id5", scope(), fixture("turn.cancel").0))
        + &step("WRITE_HEX", response("h:id5", scope(), fixture("turn.cancel").1))
        + &format!("HOLD_INPUT {}\n", program.directory.join("coalesce-hold").display());
    let (entered, observed) = std::sync::mpsc::sync_channel(1);
    let (release, barrier) = std::sync::mpsc::sync_channel(1);
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {});
    services.clock = Arc::new(ClockGate { armed: armed.clone(), entered, release: Mutex::new(barrier) });
    services.ids = Arc::new(FiveIds(count.clone()));
    let mut r = start(&program, "coalesce", &script, l, services).unwrap();
    ready(&mut r);
    session(&mut r);
    let turn = submit(&r, "turn.start", scope());
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
    let sender = r.client.sender();
    let Scope::Turn(s) = scope() else { panic!() };
    armed.store(true, std::sync::atomic::Ordering::Release);
    r.client.drain(0, 0);
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    let first = sender.cancel_turn(&s, CancelReason::User);
    release.send(()).unwrap();
    first.unwrap();
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id.as_str()=="h:id5"));
    armed.store(true, std::sync::atomic::Ordering::Release);
    r.client.drain(0, 0);
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    let repeated = sender.cancel_turn(&s, CancelReason::Deadline);
    release.send(()).unwrap();
    assert_eq!(repeated.unwrap(), Some(rid("h:id5")));
    assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 5, "coalescing does not ask the ID source for a new identity");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_callback_cancel_reuses_reserved_completion_cell() {
    use mastra_worker_support::*;
    use serde_json::json;
    let program = mastra_peer_program().unwrap();
    let l = Limits { pending_requests: 1, ..Limits::default() };
    let marker = program.directory.join("callback-cancel-read");
    let release = program.directory.join("callback-cancel-release");
    let mut params = fixture("model.generate").0;
    params["options"]["timeoutSeconds"] = json!(60);
    let cancelled = json!({"protocolVersion":"1.0","kind":"response","requestId":"r:first","scope":scope(),"outcome":"error","error":{"code":"CANCELLED","message":"Runtime request was cancelled.","retry":"never","details":{}}});
    let ack = response("r:cancel", scope(), fixture("request.cancel").1);
    let barrier_event = json!({"connectionId":"c","state":"available","authMethods":[]});
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", request("model.generate", "r:first", scope(), params.clone()))
        + &step("WRITE_HEX", request("request.cancel", "r:cancel", scope(), json!({"targetRequestId":"r:first"})))
        + &format!(
            "EXPECT_PAIR_HEX {} {}\nMARK {}\nHOLD_INPUT {}\n",
            hex(format!("{cancelled}\n").as_bytes()),
            hex(format!("{ack}\n").as_bytes()),
            marker.display(),
            release.display()
        )
        + &step("EXPECT_HEX", event(1, "connection.state", None, scope(), barrier_event.clone()))
        + &step("WRITE_HEX", request("model.generate", "r:second", scope(), params))
        + &step("EXPECT_HEX", response("r:second", scope(), fixture("model.generate").1))
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + "EXPECT_EOF\n";
    let (send, received) = std::sync::mpsc::sync_channel(1);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = count.clone();
    let mut r = start(
        &program,
        "callback-cancel",
        &script,
        l,
        services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                send.send((call, port)).unwrap();
            } else {
                drop(call);
                port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(fixture("model.generate").1).unwrap()))).unwrap();
            }
        }),
    )
    .unwrap();
    ready(&mut r);
    session(&mut r);
    submit(&r, "turn.start", scope());
    let (call, port) = received.recv_timeout(Duration::from_secs(10)).unwrap();
    let watchdog = Instant::now() + Duration::from_secs(10);
    while !call.cancellation.is_cancelled() {
        assert!(Instant::now() < watchdog);
        std::thread::yield_now();
    }
    drop(call);
    assert_eq!(
        port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(fixture("model.generate").1).unwrap()))).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    while !marker.exists() {
        assert!(Instant::now() < watchdog);
        std::thread::yield_now();
    }
    let sender = r.client.sender();
    loop {
        let p = sender.prepare_event(scope(), HostEvent::ConnectionState(serde_json::from_value(barrier_event.clone()).unwrap()), None).unwrap();
        match sender.try_emit(p) {
            Ok(()) => break,
            Err(e) => {
                assert_eq!(e.code, ErrorCode::ResourceExhausted);
                assert!(Instant::now() < watchdog);
                std::thread::yield_now();
            }
        }
    }
    std::fs::write(release, b"go").unwrap();
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id.as_str()=="h:id4"));
    assert_eq!(
        count.load(std::sync::atomic::Ordering::Acquire),
        2,
        "real service completion releases the old action slot without a second wire result"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_streaming_works_with_one_negotiated_data_slot() {
    use mastra_worker_support::*;
    use serde_json::json;
    let program = mastra_peer_program().unwrap();
    let l = Limits { pending_requests: 1, ..Limits::default() };
    let mut params = fixture("model.generate").0;
    params["options"]["delivery"] = json!("streaming");
    params["options"]["timeoutSeconds"] = json!(60);
    let mut result = fixture("model.generate").1;
    result["delivery"] = json!("streaming");
    let script = bootstrap(&l)
        + &session_script()
        + &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), fixture("turn.start").0))
        + &step("WRITE_HEX", request("model.generate", "r:stream", scope(), params))
        + &step("EXPECT_HEX", event(1, "model.delta", Some("r:stream"), scope(), json!({"index":0,"text":"hi"})))
        + &step("EXPECT_HEX", response("r:stream", scope(), result.clone()))
        + &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1))
        + "EXPECT_EOF\n";
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let mut r = start(
        &program,
        "one-slot-stream",
        &script,
        l,
        services(Arc::new(RuntimeAdmission::new(1).unwrap()), move |call, port| {
            let emitted = port.emit(CallProgress::ModelDelta(ModelDelta { index: 0, text: "hi".into() }));
            let ok = emitted.is_ok();
            sent.send(emitted).unwrap();
            drop(call);
            if ok {
                port.finish(Ok(HostReply::ModelGenerate(serde_json::from_value(result.clone()).unwrap()))).unwrap();
            }
        }),
    )
    .unwrap();
    ready(&mut r);
    session(&mut r);
    let turn = submit(&r, "turn.start", scope());
    assert!(
        received.recv_timeout(Duration::from_secs(10)).unwrap().is_ok(),
        "a dormant callback completion cell must not invent a two-data-slot streaming floor"
    );
    until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_peer_wall_deadline_controls_and_zero_budget() {
    use mastra_worker_support::*;
    use serde_json::json;
    struct Virtual {
        base: Instant,
        seconds: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Clock for Virtual {
        fn now(&self) -> Instant {
            self.base + Duration::from_secs(self.seconds.load(std::sync::atomic::Ordering::Acquire))
        }
    }
    let program = mastra_peer_program().unwrap();
    for wall in [0, 5] {
        let l = Limits::default();
        let mut params = fixture("turn.start").0;
        params["budgets"]["wallTimeSeconds"] = json!(wall);
        let mut script = bootstrap(&l) + &session_script();
        if wall != 0 {
            script += &step("EXPECT_HEX", request("turn.start", "h:id4", scope(), params.clone()));
            script += &step("WRITE_HEX", response("h:id4", scope(), fixture("turn.start").1));
            script += &step("EXPECT_HEX", request("turn.cancel", "h:id5", scope(), json!({"reason":"deadline"})));
            script += &step("WRITE_HEX", response("h:id5", scope(), json!({"state":"terminal"})));
        }
        script.push_str("EXPECT_EOF\n");
        let seconds = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut services = services(Arc::new(RuntimeAdmission::new(1).unwrap()), |_, _| {});
        services.clock = Arc::new(Virtual { base: Instant::now(), seconds: seconds.clone() });
        let mut r = start(&program, &format!("wall-{wall}"), &script, l, services).unwrap();
        ready(&mut r);
        session(&mut r);
        let sender = r.client.sender();
        let p = sender
            .prepare_request(
                scope(),
                HostRequest::TurnStart(Box::new(serde_json::from_value(params).unwrap())),
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
        let turn = sender.try_submit(p).unwrap();
        if wall == 0 {
            let notices = until(
                &mut r,
                |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Err(e),..} if request_id==&turn&&e.code==ErrorCode::Timeout),
            );
            assert!(notices.iter().any(|n| matches!(n, ClientNotice::TurnClosed { cause: CloseReason::Deadline, .. })));
            assert!(!notices.iter().any(|n| matches!(n, ClientNotice::TransportClosed { .. })));
        } else {
            until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id==&turn));
            seconds.store(5, std::sync::atomic::Ordering::Release);
            r.client.drain(0, 0);
            let notices = until(&mut r, |n| matches!(n,ClientNotice::RequestFinished {request_id,result:Ok(_),..} if request_id.as_str()=="h:id5"));
            assert_eq!(notices.iter().filter(|n| matches!(n, ClientNotice::TurnClosed { cause: CloseReason::Deadline, .. })).count(), 1);
        }
    }
}

// These fixtures are literal protocol examples, not values made by the codec under test.
// State-machine tests use actual decoded wire frames and virtual monotonic time.
#[cfg(not(target_arch = "wasm32"))]
mod mastra_state_support {
    pub use super::super::mastra_protocol::*;
    pub use super::super::runtime_client::*;
    use serde_json::{Value, json};
    pub use std::time::{Duration, Instant};

    pub fn id(text: &str) -> Id {
        Id::parse(text).unwrap()
    }
    pub fn rid(text: &str) -> RequestId {
        RequestId::parse(text, if text.starts_with("h:") { Sender::Host } else { Sender::Runtime }).unwrap()
    }
    pub fn scope() -> Scope {
        Scope::Turn(TurnScope { session_id: id("s"), turn_id: id("t"), workflow_run_id: id("w") })
    }
    pub fn fixture(method: &str) -> (Value, Value) {
        super::mastra_codec_catalog().0.into_iter().find(|x| x.0 == method).map(|x| (x.1, x.2)).unwrap()
    }
    pub fn machine(limits: Limits, now: Instant) -> RuntimeMachine {
        let mut m = RuntimeMachine::new(id("g"), limits, now).unwrap();
        m.apply(
            MachineInput::Services {
                generation: id("g"),
                services: ServiceCapabilities {
                    connection_describe: true,
                    model_generate: true,
                    agent_run: true,
                    validation: false,
                    streaming: false,
                },
            },
            now,
        );
        m
    }
    pub fn submit(m: &mut RuntimeMachine, method: &str, name: &str, scope: Scope, now: Instant) -> Vec<MachineAction> {
        let params = fixture(method).0;
        submit_params(m, method, name, scope, params, now)
    }
    pub fn submit_params(m: &mut RuntimeMachine, method: &str, name: &str, scope: Scope, params: Value, now: Instant) -> Vec<MachineAction> {
        let mut wire = super::mastra_codec_request(method, params, true);
        wire["scope"] = serde_json::to_value(&scope).unwrap();
        wire["requestId"] = json!(name);
        let phase = if method == "runtime.hello" { DecodePhase::Bootstrap } else { DecodePhase::Negotiated };
        let decoded = decode_frame(format!("{wire}\n").as_bytes(), phase, Sender::Host, &Limits::default()).unwrap();
        let DecodeResult::Frame(Frame::Request { body: RequestBody::Host(request), .. }) = decoded else { panic!("host request") };
        m.apply(MachineInput::Submit { generation: id("g"), request_id: rid(name), scope, request, deadline: now + Duration::from_secs(300) }, now)
    }
    pub fn peer(m: &mut RuntimeMachine, value: Value, now: Instant) -> Vec<MachineAction> {
        let phase = if value["kind"] == "response" { DecodePhase::Bootstrap } else { DecodePhase::Negotiated };
        let decoded = decode_frame(format!("{value}\n").as_bytes(), phase, Sender::Runtime, &Limits::default())
            .unwrap_or_else(|_| panic!("invalid peer fixture: {value}"));
        m.apply(MachineInput::Peer { generation: id("g"), sender: Sender::Runtime, decoded }, now)
    }
    pub fn response(name: &str, scope: Scope, result: Value) -> Value {
        json!({"protocolVersion":"1.0","kind":"response","requestId":name,"scope":scope,"outcome":"ok","result":result})
    }
    pub fn event(sequence: u32, name: &str, cause: Option<&str>, scope: Scope, data: Value) -> Value {
        json!({"protocolVersion":"1.0","kind":"event","sequence":sequence,"event":name,"requestId":cause,"scope":scope,"data":data})
    }
    pub fn call(m: &mut RuntimeMachine, method: &str, name: &str, now: Instant) -> Vec<MachineAction> {
        let mut wire = super::mastra_codec_request(method, fixture(method).0, false);
        wire["requestId"] = json!(name);
        wire["scope"] = serde_json::to_value(scope()).unwrap();
        peer(m, wire, now)
    }
    pub fn release(m: &mut RuntimeMachine, actions: &[MachineAction], now: Instant) {
        for action in actions {
            match action {
                MachineAction::Send { ticket, .. } => {
                    m.apply(MachineInput::Written { generation: id("g"), ticket: *ticket }, now);
                }
                MachineAction::Notice { ticket, .. } => {
                    m.apply(MachineInput::NoticeConsumed { generation: id("g"), ticket: *ticket }, now);
                }
                _ => {}
            }
        }
    }
    pub fn notices(actions: &[MachineAction]) -> Vec<&ClientNotice> {
        actions.iter().filter_map(|a| if let MachineAction::Notice { notice, .. } = a { Some(notice.as_ref()) } else { None }).collect()
    }
    pub fn frames(actions: &[MachineAction]) -> Vec<Value> {
        actions
            .iter()
            .filter_map(|a| if let MachineAction::Send { bytes, .. } = a { Some(serde_json::from_slice(bytes).unwrap()) } else { None })
            .collect()
    }
    pub fn token(actions: &[MachineAction]) -> CallbackToken {
        actions.iter().find_map(|a| if let MachineAction::Dispatch(call) = a { Some(call.token.clone()) } else { None }).expect("dispatch")
    }
    pub fn fatal(actions: &[MachineAction]) -> bool {
        notices(actions).iter().any(|n| matches!(n, ClientNotice::TransportClosed { cause: TransportCause::Error(_), .. }))
    }
    pub fn ready(limits: Limits, now: Instant) -> RuntimeMachine {
        let mut m = machine(limits.clone(), now);
        let a = submit_params(
            &mut m,
            "runtime.hello",
            "h:hello",
            Scope::Runtime,
            json!({"supportedVersions":["2.0","1.0","0.9"],"hostVersion":"1","limits":limits}),
            now,
        );
        release(&mut m, &a, now);
        let mut hello = fixture("runtime.hello").1;
        hello["limits"] = serde_json::to_value(limits).unwrap();
        hello["capabilities"]["hostModelCalls"] = json!(true);
        hello["capabilities"]["hostAgentTasks"] = json!(true);
        let a = peer(&mut m, response("h:hello", Scope::Runtime, hello), now);
        assert!(!fatal(&a));
        release(&mut m, &a, now);
        let a = peer(&mut m, event(1, "runtime.ready", Some("h:hello"), Scope::Runtime, json!({"runtimeInstanceId":"i"})), now);
        assert!(notices(&a).iter().any(|n| matches!(n, ClientNotice::RuntimeReady(_))));
        release(&mut m, &a, now);
        let a = submit(&mut m, "session.start", "h:session", Scope::Session(id("s")), now);
        assert_eq!(frames(&a).len(), 1);
        release(&mut m, &a, now);
        let a = peer(&mut m, response("h:session", Scope::Session(id("s")), fixture("session.start").1), now);
        assert!(!fatal(&a));
        release(&mut m, &a, now);
        m
    }
    pub fn turn(limits: Limits, now: Instant) -> RuntimeMachine {
        let mut m = ready(limits, now);
        let a = submit(&mut m, "turn.start", "h:turn", scope(), now);
        assert_eq!(frames(&a).len(), 1);
        release(&mut m, &a, now);
        m
    }
}

// Mutation caught: loose version matching, ready/hello identity drift, or refusal overwritten by EOF.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_exact_hello_and_capabilities() {
    use mastra_state_support::*;
    let now = Instant::now();
    for selected in ["1.0.0", "2.0", "0.9"] {
        let mut m = machine(Limits::default(), now);
        let a = submit(&mut m, "runtime.hello", "h:hello", Scope::Runtime, now);
        release(&mut m, &a, now);
        let mut result = fixture("runtime.hello").1;
        result["selectedVersion"] = json!(selected);
        assert!(fatal(&peer(&mut m, response("h:hello", Scope::Runtime, result), now)));
    }
    let mut m = machine(Limits::default(), now);
    let a = submit(&mut m, "runtime.hello", "h:hello", Scope::Runtime, now);
    release(&mut m, &a, now);
    let mut hello = fixture("runtime.hello").1;
    hello["limits"]["textBytes"] = json!(1);
    let a = peer(&mut m, response("h:hello", Scope::Runtime, hello), now);
    release(&mut m, &a, now);
    let a = peer(&mut m, event(1, "runtime.ready", Some("h:hello"), Scope::Runtime, json!({"runtimeInstanceId":"other"})), now);
    assert!(fatal(&a));
    let mut m = machine(Limits::default(), now);
    let a = submit(&mut m, "runtime.hello", "h:hello", Scope::Runtime, now);
    release(&mut m, &a, now);
    let refusal = json!({"protocolVersion":"9.0","kind":"response","requestId":"h:hello","scope":Scope::Runtime,"outcome":"error","error":{"code":"INCOMPATIBLE_VERSION","message":"No common version.","retry":"never","details":{}}});
    let a = peer(&mut m, refusal, now);
    assert!(notices(&a).iter().any(|n| matches!(n,ClientNotice::RequestFinished {result:Err(e),..} if e.code==ErrorCode::IncompatibleVersion)));
    release(&mut m, &a, now);
    let a = submit(&mut m, "session.start", "h:bad", Scope::Session(id("s")), now);
    assert!(frames(&a).is_empty());
    let a = m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now);
    assert!(notices(&a).iter().any(|n| matches!(n, ClientNotice::TransportClosed { cause: TransportCause::ExpectedEof, .. })));
    assert!(!notices(&a).iter().any(|n| matches!(n,ClientNotice::RequestFinished {request_id,..} if request_id.as_str()=="h:hello")));
    let mut m = ready(Limits::default(), now);
    let a = submit(&mut m, "runtime.shutdown", "h:stop", Scope::Runtime, now);
    release(&mut m, &a, now);
    let a = peer(&mut m, response("h:stop", Scope::Runtime, fixture("runtime.shutdown").1), now);
    release(&mut m, &a, now);
    let a = m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now);
    assert!(notices(&a).iter().any(|n| matches!(n, ClientNotice::TransportClosed { cause: TransportCause::ExpectedEof, .. })));
    let mut m = ready(Limits::default(), now);
    assert!(fatal(&m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now)));
    let mut m = ready(Limits::default(), now);
    let a = submit(&mut m, "turn.start", "h:turn", scope(), now);
    release(&mut m, &a, now);
    // Availability is injected, never inferred from runtime readiness/authentication.
    let a = call(&mut m, "validation.status", "r:v", now);
    assert_eq!(frames(&a)[0]["error"]["code"], "UNSUPPORTED_CAPABILITY");
    assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
}

// Mutation caught: binding a turn only after acknowledgement deadlocks nested model callbacks.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_nested_callback_before_acceptance() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let a = call(&mut m, "model.generate", "r:model", now);
    assert!(matches!(a.as_slice(), [MachineAction::Dispatch(_)]));
    let token = token(&a);
    let reply = decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap();
    let a = m.apply(MachineInput::CallbackFinished { token, result: Box::new(Ok(reply)) }, now);
    assert_eq!(frames(&a)[0]["requestId"], "r:model");
    assert!(!notices(&a).iter().any(|n| matches!(n,ClientNotice::RequestFinished {request_id,..} if request_id.as_str()=="h:turn")));
    release(&mut m, &a, now);
    let a = peer(&mut m, response("h:turn", scope(), fixture("turn.start").1), now);
    assert!(matches!(notices(&a).as_slice(), [ClientNotice::RequestFinished { result: Ok(RuntimeReply::TurnStart(_)), .. }]));
}

// Mutation caught: trusting scope/retirement before exact sender, method, sequence and cause.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_correlation_and_sequences() {
    use mastra_state_support::*;
    let now = Instant::now();
    for wire in [
        response("h:unknown", scope(), json!({"state":"accepted"})),
        response("h:turn", Scope::Session(id("s")), json!({"state":"accepted"})),
        response("h:turn", scope(), json!({"state":"ready","workflowId":"w"})),
    ] {
        let mut m = turn(Limits::default(), now);
        assert!(fatal(&peer(&mut m, wire, now)));
    }
    let mut m = turn(Limits::default(), now);
    let a = peer(&mut m, response("h:turn", scope(), json!({"state":"accepted"})), now);
    release(&mut m, &a, now);
    assert!(fatal(&peer(&mut m, response("h:turn", scope(), json!({"state":"accepted"})), now)));
    for seq in [1, 2] {
        let mut m = turn(Limits::default(), now);
        let e = event(2, "workflow.text", Some("h:turn"), scope(), json!({"text":"a","channel":"progress"}));
        let a = peer(&mut m, e, now);
        release(&mut m, &a, now);
        assert!(fatal(&peer(&mut m, event(seq, "workflow.text", Some("h:turn"), scope(), json!({"text":"a","channel":"progress"})), now)));
    }
    let mut m = turn(Limits::default(), now);
    assert!(fatal(&peer(&mut m, event(2, "workflow.text", Some("r:wrong"), scope(), json!({"text":"a","channel":"progress"})), now)));
    let mut m = turn(Limits::default(), now);
    let decoded = decode_frame(
        format!("{}\n", response("h:turn", scope(), json!({"state":"accepted"}))).as_bytes(),
        DecodePhase::Negotiated,
        Sender::Runtime,
        &Limits::default(),
    )
    .unwrap();
    assert!(m.apply(MachineInput::Peer { generation: id("old"), sender: Sender::Runtime, decoded: decoded.clone() }, now).is_empty());
    let a = m.apply(MachineInput::Peer { generation: id("g"), sender: Sender::Runtime, decoded }, now);
    assert!(!fatal(&a));
    release(&mut m, &a, now);
    let decoded = decode_frame(
        format!("{}\n", event(2, "workflow.text", Some("h:turn"), scope(), json!({"text":"a","channel":"progress"}))).as_bytes(),
        DecodePhase::Negotiated,
        Sender::Runtime,
        &Limits::default(),
    )
    .unwrap();
    assert!(fatal(&m.apply(MachineInput::Peer { generation: id("g"), sender: Sender::Host, decoded }, now)));
    let mut m = turn(Limits::default(), now);
    let mut unknown = super::tests::mastra_codec_request("future.method", json!({}), false);
    unknown["scope"] = serde_json::to_value(scope()).unwrap();
    unknown["requestId"] = json!("r:unknown");
    let a = peer(&mut m, unknown, now);
    assert_eq!(frames(&a)[0]["error"]["code"], "METHOD_NOT_FOUND");
    release(&mut m, &a, now);
    assert!(call(&mut m, "model.generate", "r:valid", now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
}

// Mutation caught: unbounded pending/queued actions, control starvation, ID eviction and sequence wrap.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_queue_pending_and_ledger_reserves() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let mut first = None;
    for n in 0..32 {
        let a = call(&mut m, "model.generate", &format!("r:m{n}"), now);
        assert_eq!(a.iter().filter(|a| matches!(a, MachineAction::Dispatch(_))).count(), 1);
        if n == 0 {
            first = Some(token(&a));
        }
    }
    let a = call(&mut m, "model.generate", "r:overflow", now);
    assert_eq!(frames(&a)[0]["error"]["code"], "RESOURCE_EXHAUSTED");
    assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    release(&mut m, &a, now);
    let cancel = json!({"protocolVersion":"1.0","kind":"request","scope":scope(),"requestId":"r:cancel","method":"request.cancel","params":{"targetRequestId":"r:m0"}});
    let a = peer(&mut m, cancel, now);
    assert!(frames(&a).iter().any(|v| v["requestId"] == "r:cancel" && v["outcome"] == "ok"));
    release(&mut m, &a, now);
    let a = call(&mut m, "model.generate", "r:still-held", now);
    assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))), "cancellation must not release an in-flight owned action");
    assert_eq!(frames(&a)[0]["error"]["code"], "RESOURCE_EXHAUSTED", "cancelled dispatch keeps its owned payload reservation until completion");
    release(&mut m, &a, now);
    let reply = decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap();
    let a = m.apply(MachineInput::CallbackFinished { token: first.unwrap(), result: Box::new(Ok(reply)) }, now);
    assert!(frames(&a).is_empty());
    assert!(call(&mut m, "model.generate", "r:released", now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    let limits = Limits { pending_requests: 1, request_ledger_entries: 20, ..Limits::default() };
    let mut m = turn(limits, now);
    for n in 0..5 {
        let a = call(&mut m, "model.generate", &format!("r:x{n}"), now);
        let token = token(&a);
        let reply = decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap();
        let a = m.apply(MachineInput::CallbackFinished { token, result: Box::new(Ok(reply)) }, now);
        release(&mut m, &a, now);
    }
    let a = call(&mut m, "model.generate", "r:ledger-full", now);
    assert_eq!(frames(&a)[0]["error"]["code"], "RESOURCE_EXHAUSTED");
    release(&mut m, &a, now);
    assert!(fatal(&call(&mut m, "model.generate", "r:x0", now)), "completed IDs never evicted/reused");
    let limits = Limits { frame_bytes: 4096, queue_bytes: 8192, control_lane_bytes: 4096, ..Limits::default() };
    let mut m = turn(limits, now);
    let connection =
        HostEvent::ConnectionState(ConnectionStateEvent { connection_id: id("c"), state: ConnectionState::Unknown, auth_methods: vec![] });
    let mut sent = 0;
    loop {
        let a = m.apply(MachineInput::Emit { generation: id("g"), scope: scope(), event: connection.clone(), cause: None }, now);
        if frames(&a).is_empty() {
            assert!(a.iter().any(|a| matches!(a,MachineAction::Rejected {error,..} if error.code==ErrorCode::ResourceExhausted)));
            break;
        }
        sent += 1;
        assert!(sent <= 32);
    }
    let a = submit(&mut m, "turn.cancel", "h:cancel", scope(), now);
    assert_eq!(frames(&a).len(), 1, "reserved bytes and slot survive retained writes");
    let mut m = turn(Limits::default(), now);
    let a = peer(&mut m, event(2_147_483_647, "workflow.text", Some("h:turn"), scope(), json!({"text":"end","channel":"progress"})), now);
    assert!(a.iter().any(|a| matches!(a, MachineAction::StopTree)), "sequence exhaustion requests shutdown, never wraps");
}

// Mutation caught: forged streaming, callback relabeling, or terminal proposals freeing next-turn admission.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_host_events_and_buffered_delta_rejection() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let a = m.apply(
        MachineInput::Emit {
            generation: id("g"),
            scope: scope(),
            event: HostEvent::ConnectionState(ConnectionStateEvent {
                connection_id: id("c"),
                state: ConnectionState::AuthRequired,
                auth_methods: vec![],
            }),
            cause: Some(rid("h:turn")),
        },
        now,
    );
    assert_eq!(frames(&a)[0]["sequence"], 1);
    assert_eq!(frames(&a)[0]["scope"], serde_json::to_value(scope()).unwrap());
    assert_eq!(frames(&a)[0]["requestId"], "h:turn");
    release(&mut m, &a, now);
    let a = call(&mut m, "model.generate", "r:model", now);
    let model = token(&a);
    let a =
        m.apply(MachineInput::Progress { token: model, event: CallProgress::ModelDelta(ModelDelta { index: 0, text: "not streaming".into() }) }, now);
    assert!(frames(&a).is_empty());
    assert!(a.iter().any(|a| matches!(a,MachineAction::Rejected {error,..} if error.code==ErrorCode::UnsupportedCapability)));
    let a = call(&mut m, "agent.run", "r:agent", now);
    let agent = token(&a);
    let progress = CallProgress::AgentProgress(AgentProgress {
        agent_session_id: id("as"),
        content: AgentProgressContent::Text(ProgressText { text: "real progress".into() }),
    });
    let a = m.apply(MachineInput::Progress { token: agent.clone(), event: progress.clone() }, now);
    assert_eq!(frames(&a)[0]["sequence"], 2);
    assert_eq!(frames(&a)[0]["requestId"], "r:agent");
    assert_eq!(frames(&a)[0]["scope"], serde_json::to_value(scope()).unwrap());
    release(&mut m, &a, now);
    let proposal = fixture("turn.start").0["binding"]["document"].clone();
    let finished =
        json!({"state":"validated_success","reason":"proposal only","document":proposal,"changes":"unknown","validationRunId":null,"report":null});
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), finished.clone()), now);
    assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::TerminalProposal, .. })));
    assert_eq!(notices(&a).iter().filter(|n| matches!(n, ClientNotice::TurnFinishedProposal { .. })).count(), 1);
    assert!(notices(&a).iter().any(|n|matches!(n,ClientNotice::TurnFinishedProposal {proposal,..} if proposal.state==TurnTerminal::ValidatedSuccess && proposal.changes==Changes::Unknown)));
    release(&mut m, &a, now);
    let mut second = finished.clone();
    second["state"] = json!("cancelled");
    let a = peer(&mut m, event(3, "turn.finished", Some("h:turn"), scope(), second), now);
    assert!(!fatal(&a));
    assert!(!notices(&a).iter().any(|n| matches!(n, ClientNotice::TurnFinishedProposal { .. })));
    let a = m.apply(MachineInput::Progress { token: agent.clone(), event: progress.clone() }, now);
    assert!(frames(&a).is_empty());
    let a = submit(&mut m, "turn.start", "h:next", scope(), now);
    assert!(frames(&a).is_empty());
    let Scope::Turn(turn_scope) = scope() else { panic!() };
    let mut wrong = turn_scope.clone();
    wrong.turn_id = id("wrong");
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: wrong, terminal: TurnTerminal::TaskFailed }, now);
    let a = submit(&mut m, "turn.start", "h:next2", scope(), now);
    assert!(frames(&a).is_empty());
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: turn_scope, terminal: TurnTerminal::TaskFailed }, now);
    let mut next = scope();
    if let Scope::Turn(t) = &mut next {
        t.turn_id = id("next");
    }
    let a = submit(&mut m, "turn.start", "h:next3", next.clone(), now);
    assert_eq!(frames(&a).len(), 1);
    release(&mut m, &a, now);
    let mut wire = mastra_codec_request("agent.run", fixture("agent.run").0, false);
    wire["scope"] = json!(next);
    wire["requestId"] = json!("r:new-agent");
    let a = peer(&mut m, wire, now);
    let next_token = token(&a);
    let a = m.apply(MachineInput::Progress { token: agent, event: progress.clone() }, now);
    assert!(frames(&a).is_empty(), "retired progress never takes the new callback's scope");
    let a = m.apply(MachineInput::Progress { token: next_token, event: progress }, now);
    assert_eq!(frames(&a)[0]["scope"], json!(next));
    assert_eq!(frames(&a)[0]["requestId"], "r:new-agent");
}

// Mutation caught: generation checks after accounting, ignored overflow, or retired IDs pardoned as unknown.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_generation_retirement_and_overflow() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let old = decode_frame(
        format!("{}\n", event(2_147_483_647, "workflow.text", Some("h:turn"), scope(), json!({"text":"stale","channel":"progress"}))).as_bytes(),
        DecodePhase::Negotiated,
        Sender::Runtime,
        &Limits::default(),
    )
    .unwrap();
    assert!(m.apply(MachineInput::Peer { generation: id("old"), sender: Sender::Runtime, decoded: old }, now).is_empty());
    let a = peer(&mut m, event(2, "workflow.text", Some("h:turn"), scope(), json!({"text":"current","channel":"progress"})), now);
    assert!(!fatal(&a));
    release(&mut m, &a, now);
    let mut p = fixture("request.cancel").0;
    p["targetRequestId"] = json!("h:turn");
    let a = submit_params(&mut m, "request.cancel", "h:cancel", scope(), p, now);
    assert!(notices(&a).iter().any(|n| matches!(n,ClientNotice::RequestFinished {result:Err(e),..} if e.code==ErrorCode::Cancelled)));
    release(&mut m, &a, now);
    let a = peer(&mut m, response("h:turn", scope(), json!({"state":"accepted"})), now);
    assert!(!fatal(&a));
    assert!(notices(&a).is_empty());
    assert!(fatal(&peer(&mut m, response("h:not-cancelled", scope(), json!({"state":"accepted"})), now)));
    let mut m = turn(Limits::default(), now);
    let finished = json!({"state":"task_failed","reason":"no result","document":{"documentId":"d","revision":"1"},"changes":"unknown","validationRunId":null,"report":null});
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), finished), now);
    release(&mut m, &a, now);
    let a = peer(&mut m, event(3, "workflow.text", Some("h:unknown"), scope(), json!({"text":"not excused","channel":"progress"})), now);
    assert!(fatal(&a));
    let limits = Limits { frame_bytes: 4096, queue_bytes: 8192, control_lane_bytes: 4096, ..Limits::default() };
    let mut m = turn(limits, now);
    let mut seen_close = false;
    for seq in 2..80 {
        let a = peer(&mut m, event(seq, "workflow.text", Some("h:turn"), scope(), json!({"text":"a".repeat(1000),"channel":"progress"})), now);
        if fatal(&a) {
            seen_close = true;
            break;
        }
    }
    assert!(seen_close, "unconsumed notices must close visibly, not be silently dropped");
}

// Mutation caught: duplicated control consumes the reserve, and replies after their clamped deadline succeed.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_control_coalescing_and_callback_deadline() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let a = submit(&mut m, "turn.cancel", "h:cancel1", scope(), now);
    release(&mut m, &a, now);
    let a = submit(&mut m, "turn.cancel", "h:cancel2", scope(), now);
    assert!(matches!(a.as_slice(),[MachineAction::Coalesced {existing_request_id,..}] if existing_request_id.as_str()=="h:cancel1"));
    let mut m = turn(Limits::default(), now);
    let a = call(&mut m, "model.generate", "r:deadline", now);
    let token = token(&a);
    let reply = decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap();
    let a = m.apply(MachineInput::CallbackFinished { token, result: Box::new(Ok(reply)) }, now + Duration::from_secs(1));
    assert_eq!(frames(&a)[0]["error"]["code"], "TIMEOUT");
}

// Mutation caught: treating a completed CANCELLED service as an action still in flight.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_finished_cancelled_callback_releases_owned_action() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
    let a = call(&mut m, "model.generate", "r:first", now);
    let token = token(&a);
    let cancelled = WireError {
        code: ErrorCode::Cancelled,
        message: "Service finished cancelled.".into(),
        retry: Retry::Never,
        details: ErrorDetails::Empty(EmptyDetails {}),
    };
    let a = m.apply(MachineInput::CallbackFinished { token, result: Box::new(Err(cancelled)) }, now);
    assert_eq!(frames(&a)[0]["error"]["code"], "CANCELLED");
    release(&mut m, &a, now);
    let a = call(&mut m, "model.generate", "r:next", now);
    assert!(
        a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))),
        "a finished service must release its owned action even when its result is CANCELLED"
    );
}

// I1: an ended action still owns its callback slot until its reply is physically written.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_reply_slot_requires_action_end_and_written() {
    use mastra_state_support::*;
    let now = Instant::now();
    for cancelled in [false, true] {
        let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
        let a = call(&mut m, "model.generate", "r:first", now);
        let token = token(&a);
        let result = if cancelled {
            Err(WireError {
                code: ErrorCode::Cancelled,
                message: "Ended.".into(),
                retry: Retry::Never,
                details: ErrorDetails::Empty(EmptyDetails {}),
            })
        } else {
            Ok(decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap())
        };
        let reply = m.apply(MachineInput::CallbackFinished { token, result: Box::new(result) }, now);
        assert_eq!(frames(&reply).len(), 1);
        let a = call(&mut m, "model.generate", "r:too-early", now);
        assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))), "reply still owns its physical write slot");
    }
    // Both orders of a logical cancellation must retain the other half of ownership.
    for written_first in [false, true] {
        let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
        let a = call(&mut m, "model.generate", "r:first", now);
        let token = token(&a);
        let replies = peer(
            &mut m,
            json!({"protocolVersion":"1.0","kind":"request","requestId":"r:cancel","scope":scope(),"method":"request.cancel","params":{"targetRequestId":"r:first"}}),
            now,
        );
        assert!(!fatal(&replies));
        if written_first {
            release(&mut m, &replies, now);
        } else {
            let result = decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap();
            m.apply(MachineInput::CallbackFinished { token: token.clone(), result: Box::new(Ok(result)) }, now);
        }
        let a = call(&mut m, "model.generate", "r:too-early", now);
        assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))), "both action end and Written are required");
    }
    let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
    let a = call(&mut m, "model.generate", "r:first", now);
    let result = decode_host_reply("model.generate", fixture("model.generate").1, &Limits::default()).unwrap();
    let a = m.apply(MachineInput::CallbackFinished { token: token(&a), result: Box::new(Ok(result)) }, now);
    release(&mut m, &a, now);
    assert!(call(&mut m, "model.generate", "r:after-write", now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
}

// I2: valid lower limits do not acquire an undeclared 4096-byte startup floor.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_lower_legal_limits_allow_bootstrap() {
    use mastra_state_support::*;
    let now = Instant::now();
    let limits = Limits { frame_bytes: 3000, queue_bytes: 6000, control_lane_bytes: 3000, ..Limits::default() };
    assert!(validate_limits(&limits).is_ok());
    let mut m = machine(limits.clone(), now);
    let a = submit_params(
        &mut m,
        "runtime.hello",
        "h:hello",
        Scope::Runtime,
        json!({"supportedVersions":["1.0"],"hostVersion":"1","limits":limits}),
        now,
    );
    assert_eq!(frames(&a).len(), 1, "small hello fits the negotiated data capacity");
    let mut m = ready(limits, now);
    assert_eq!(frames(&submit(&mut m, "turn.start", "h:turn", scope(), now)).len(), 1);
}

// Mutation caught: silently losing the original call on notice overflow, or unbounded event slots.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_completion_cells_and_event_slots() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    for seq in 2..34 {
        let a = peer(&mut m, event(seq, "workflow.text", Some("h:turn"), scope(), json!({"text":"x","channel":"progress"})), now);
        assert!(!fatal(&a));
    }
    let a = peer(&mut m, event(34, "workflow.text", Some("h:turn"), scope(), json!({"text":"overflow","channel":"progress"})), now);
    assert!(fatal(&a));
    assert!(notices(&a).iter().any(|n|matches!(n,ClientNotice::RequestFinished {request_id,result:Err(e),..} if request_id.as_str()=="h:turn" && e.code==ErrorCode::TransportError)));
    let mut m = turn(Limits::default(), now);
    let a = submit(&mut m, "turn.cancel", "h:control", scope(), now);
    release(&mut m, &a, now);
    let a = peer(
        &mut m,
        json!({"protocolVersion":"1.0","kind":"response","requestId":"h:control","scope":scope(),"outcome":"error","error":{"code":"INTERNAL_ERROR","message":"x".repeat(2048),"retry":"never","details":{}}}),
        now,
    );
    assert!(!fatal(&a));
    assert!(notices(&a).iter().any(|n|matches!(n,ClientNotice::RequestFinished {request_id,result:Err(e),..} if request_id.as_str()=="h:control" && e.code==ErrorCode::InternalError && e.message=="x".repeat(2048))));
    let mut m = ready(Limits::default(), now);
    let a = submit(&mut m, "runtime.shutdown", "h:shutdown", Scope::Runtime, now);
    release(&mut m, &a, now);
    let a = submit(&mut m, "runtime.shutdown", "h:again", Scope::Runtime, now);
    assert!(matches!(a.as_slice(),[MachineAction::Coalesced {existing_request_id,..}] if existing_request_id.as_str()=="h:shutdown"));
    let mut m = turn(Limits::default(), now);
    let mut unknown = mastra_codec_request("future.method", json!({}), false);
    unknown["scope"] = json!(Scope::Runtime);
    unknown["requestId"] = json!("r:future");
    let a = peer(&mut m, unknown, now);
    assert!(!fatal(&a));
    assert_eq!(frames(&a)[0]["error"]["code"], "METHOD_NOT_FOUND");
}

// I3: control completions use available negotiated storage, not a private 1KiB wire limit.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_control_completion_preserves_original_error() {
    use mastra_state_support::*;
    let now = Instant::now();
    for limits in [Limits::default(), Limits { frame_bytes: 3000, queue_bytes: 6000, control_lane_bytes: 3000, ..Limits::default() }] {
        let mut m = turn(limits, now);
        let a = submit(&mut m, "turn.cancel", "h:control", scope(), now);
        release(&mut m, &a, now);
        let a = peer(
            &mut m,
            json!({"protocolVersion":"1.0","kind":"response","requestId":"h:control","scope":scope(),"outcome":"error","error":{"code":"INTERNAL_ERROR","message":"x".repeat(2048),"retry":"never","details":{"operation":"o".repeat(500)}}}),
            now,
        );
        assert!(!fatal(&a), "a legal original error fits available data/control storage");
        assert!(notices(&a).iter().any(|n|matches!(n,ClientNotice::RequestFinished {request_id,result:Err(e),..} if request_id.as_str()=="h:control" && e.code==ErrorCode::InternalError && e.message=="x".repeat(2048))));
        let params = json!({"targetRequestId":"h:turn"});
        let early = submit_params(&mut m, "request.cancel", "h:early", scope(), params.clone(), now);
        assert!(frames(&early).is_empty(), "unconsumed completion retains the semantic control slot even in data storage");
        release(&mut m, &a, now);
        assert_eq!(frames(&submit_params(&mut m, "request.cancel", "h:after-consume", scope(), params, now)).len(), 1);
    }
}

// I4: valid cancel envelopes with unknown targets return a recoverable method error.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_cancel_errors_preserve_session_and_control_reserve() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let a = peer(
        &mut m,
        json!({"protocolVersion":"1.0","kind":"request","requestId":"r:bad-cancel","scope":scope(),"method":"request.cancel","params":{"targetRequestId":"r:missing"}}),
        now,
    );
    assert!(!fatal(&a));
    assert_eq!(frames(&a)[0]["error"]["code"], "INVALID_REQUEST");
    assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    release(&mut m, &a, now);
    assert!(call(&mut m, "model.generate", "r:valid-after-error", now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    for name in ["r:cancel1", "r:cancel2"] {
        let a = peer(
            &mut m,
            json!({"protocolVersion":"1.0","kind":"request","requestId":name,"scope":scope(),"method":"request.cancel","params":{"targetRequestId":"r:valid-after-error"}}),
            now,
        );
        assert!(!fatal(&a), "idempotent cancel does not replace or release the first physical reply");
        assert!(frames(&a).iter().any(|f| f["requestId"] == name && f["outcome"] == "ok"));
    }
}

// I5: session run queries reach injected authorization without requiring a turn.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_session_validation_queries_do_not_require_turn() {
    use mastra_state_support::*;
    fn session(now: Instant, native: bool, advertised: bool) -> RuntimeMachine {
        let mut m = RuntimeMachine::new(id("g"), Limits::default(), now).unwrap();
        m.apply(
            MachineInput::Services { generation: id("g"), services: ServiceCapabilities { validation: native, ..ServiceCapabilities::default() } },
            now,
        );
        let a = submit(&mut m, "runtime.hello", "h:hello", Scope::Runtime, now);
        release(&mut m, &a, now);
        let mut hello = fixture("runtime.hello").1;
        hello["capabilities"]["snapshotValidation"] = json!(advertised);
        let a = peer(&mut m, response("h:hello", Scope::Runtime, hello), now);
        release(&mut m, &a, now);
        let a = peer(&mut m, event(1, "runtime.ready", Some("h:hello"), Scope::Runtime, json!({"runtimeInstanceId":"i"})), now);
        release(&mut m, &a, now);
        let a = submit(&mut m, "session.start", "h:session", Scope::Session(id("s")), now);
        release(&mut m, &a, now);
        let a = peer(&mut m, response("h:session", Scope::Session(id("s")), fixture("session.start").1), now);
        assert!(!fatal(&a));
        release(&mut m, &a, now);
        m
    }
    let now = Instant::now();
    for finishing in [false, true] {
        let mut m = session(now, true, true);
        if finishing {
            let mut params = fixture("turn.start").0;
            params["budgets"]["wallTimeSeconds"] = json!(0);
            let a = submit_params(&mut m, "turn.start", "h:turn", scope(), params, now);
            release(&mut m, &a, now);
            let a = peer(
                &mut m,
                event(
                    2,
                    "turn.finished",
                    Some("h:turn"),
                    scope(),
                    json!({"state":"task_failed","reason":"no result","document":{"documentId":"d","revision":"1"},"changes":"unknown","validationRunId":null,"report":null}),
                ),
                now,
            );
            release(&mut m, &a, now);
        }
        for method in ["validation.status", "validation.report", "validation.cancel"] {
            let name = format!("r:{method}");
            let mut wire = mastra_codec_request(method, fixture(method).0, false);
            wire["requestId"] = json!(name);
            wire["scope"] = json!(Scope::Session(id("s")));
            let a = peer(&mut m, wire, now);
            assert!(
                a.iter().any(|a| matches!(a,MachineAction::Dispatch(c) if c.scope==Scope::Session(id("s")) && c.call.method()==method)),
                "session query must reach service: finishing={finishing}, method={method}, actions={a:?}"
            );
            let reply = decode_host_reply(method, fixture(method).1, &Limits::default()).unwrap();
            let a = m.apply(MachineInput::CallbackFinished { token: token(&a), result: Box::new(Ok(reply)) }, now);
            assert!(!fatal(&a));
            release(&mut m, &a, now);
        }
    }
    for (native, advertised) in [(false, true), (true, false)] {
        let mut m = session(now, native, advertised);
        let mut wire = mastra_codec_request("validation.status", fixture("validation.status").0, false);
        wire["scope"] = json!(Scope::Session(id("s")));
        let a = peer(&mut m, wire, now);
        assert_eq!(frames(&a)[0]["error"]["code"], "UNSUPPORTED_CAPABILITY");
        assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    }
    let mut m = session(now, true, true);
    let mut wire = mastra_codec_request("validation.status", fixture("validation.status").0, false);
    wire["scope"] = json!(Scope::Session(id("other")));
    let a = peer(&mut m, wire, now);
    assert!(fatal(&a));
    assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
}

// I6: refusal closes work admission, but only matching trusted completion releases the turn.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_rejected_turn_start_waits_for_trusted_completion() {
    use mastra_state_support::*;
    let now = Instant::now();
    let refusal = json!({"protocolVersion":"1.0","kind":"response","requestId":"h:turn","scope":scope(),"outcome":"error","error":{"code":"INVALID_REQUEST","message":"Cannot accept this turn.","retry":"never","details":{}}});
    let mut m = turn(Limits::default(), now);
    let early = call(&mut m, "model.generate", "r:before-ack", now);
    assert!(early.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    let a = peer(&mut m, refusal.clone(), now);
    assert!(!fatal(&a));
    assert!(matches!(a.first(), Some(MachineAction::CloseTurn { .. })), "refusal must close the scope before delivering its completion");
    release(&mut m, &a, now);
    assert!(!call(&mut m, "model.generate", "r:after-refusal", now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
    let mut m = turn(Limits::default(), now);
    let a = peer(&mut m, refusal.clone(), now);
    release(&mut m, &a, now);
    let Scope::Turn(current) = scope() else { panic!() };
    let next = TurnScope { turn_id: id("next"), ..current.clone() };
    assert!(frames(&submit(&mut m, "turn.start", "h:too-soon", Scope::Turn(next.clone()), now)).is_empty());
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: next.clone(), terminal: TurnTerminal::TaskFailed }, now);
    assert!(frames(&submit(&mut m, "turn.start", "h:still-too-soon", Scope::Turn(next.clone()), now)).is_empty());
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: current, terminal: TurnTerminal::TaskFailed }, now);
    assert_eq!(frames(&submit(&mut m, "turn.start", "h:next", Scope::Turn(next), now)).len(), 1);
    let mut m = turn(Limits::default(), now);
    let proposal = json!({"state":"validated_success","reason":"proposal only","document":{"documentId":"d","revision":"1"},"changes":"unknown","validationRunId":null,"report":null});
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal.clone()), now);
    assert!(
        notices(&a).iter().any(|n| matches!(n,ClientNotice::TurnFinishedProposal {proposal,..} if proposal.state==TurnTerminal::ValidatedSuccess))
    );
    release(&mut m, &a, now);
    let a = peer(&mut m, refusal, now);
    assert!(!fatal(&a));
    assert!(!a.iter().any(|a| matches!(a, MachineAction::CloseTurn { .. })));
    release(&mut m, &a, now);
    let mut second = proposal;
    second["state"] = json!("cancelled");
    let a = peer(&mut m, event(3, "turn.finished", Some("h:turn"), scope(), second), now);
    assert!(!fatal(&a));
    assert!(!notices(&a).iter().any(|n| matches!(n, ClientNotice::TurnFinishedProposal { .. })));
}

// Mutation caught: charging the tiny runtime.ready frame, not the owned RuntimeInfo notice.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_ready_notice_owns_its_byte_reservation() {
    use mastra_state_support::*;
    let now = Instant::now();
    let limits = Limits { frame_bytes: 8000, queue_bytes: 12096, control_lane_bytes: 4096, ..Limits::default() };
    let mut m = machine(limits.clone(), now);
    let a = submit_params(
        &mut m,
        "runtime.hello",
        "h:hello",
        Scope::Runtime,
        json!({"supportedVersions":["1.0"],"hostVersion":"1","limits":limits}),
        now,
    );
    release(&mut m, &a, now);
    let mut hello = fixture("runtime.hello").1;
    hello["limits"] = json!(limits);
    hello["runtimeVersion"] = json!("");
    let fixed_bytes = format!("{}\n", response("h:hello", Scope::Runtime, hello.clone())).len();
    // Leave only 64 data bytes, genuinely too little for a session completion;
    // a 4096-byte private reservation is not part of the protocol.
    hello["runtimeVersion"] = json!("v".repeat(7936 - fixed_bytes));
    let a = peer(&mut m, response("h:hello", Scope::Runtime, hello), now);
    assert!(!fatal(&a));
    release(&mut m, &a, now);
    let ready = peer(&mut m, event(1, "runtime.ready", Some("h:hello"), Scope::Runtime, json!({"runtimeInstanceId":"i"})), now);
    assert!(!fatal(&ready));
    let a = submit(&mut m, "session.start", "h:before-consume", Scope::Session(id("s")), now);
    assert!(frames(&a).is_empty());
    assert!(a.iter().any(|a| matches!(a,MachineAction::Rejected {error,..} if error.code==ErrorCode::ResourceExhausted)));
    release(&mut m, &ready, now);
    let a = submit(&mut m, "session.start", "h:after-consume", Scope::Session(id("s")), now);
    assert_eq!(frames(&a).len(), 1);
}

// Full duplex permits terminal proposals before their start acknowledgement. Use
// successive trusted host completions to fill all 32 *host* pending calls without
// inventing concurrent turns or exposing the machine's private admission state.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_state_host_pending_has_an_independent_control_reserve() {
    use mastra_state_support::*;
    let now = Instant::now();
    let mut m = ready(Limits::default(), now);
    for n in 0..32 {
        let s = TurnScope { session_id: id("s"), turn_id: id(&format!("t{n}")), workflow_run_id: id(&format!("w{n}")) };
        let request = format!("h:turn{n}");
        let a = submit(&mut m, "turn.start", &request, Scope::Turn(s.clone()), now);
        assert_eq!(frames(&a).len(), 1);
        release(&mut m, &a, now);
        let proposal = json!({"state":"task_failed","reason":"no modeling result","document":{"documentId":"d","revision":"1"},"changes":"unknown","validationRunId":null,"report":null});
        let a = peer(&mut m, event(n + 2, "turn.finished", Some(&request), Scope::Turn(s.clone()), proposal), now);
        assert!(!fatal(&a));
        release(&mut m, &a, now);
        m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: s, terminal: TurnTerminal::TaskFailed }, now);
    }
    let a = submit(&mut m, "turn.start", "h:33", scope(), now);
    assert!(frames(&a).is_empty());
    assert!(a.iter().any(|a| matches!(a,MachineAction::Rejected {error,..} if error.code==ErrorCode::ResourceExhausted)));
    let a = submit(&mut m, "runtime.shutdown", "h:control-reserve", Scope::Runtime, now);
    assert_eq!(frames(&a).len(), 1);
}

#[cfg(not(target_arch = "wasm32"))]
mod mastra_retirement_support {
    pub use super::mastra_state_support::*;
    use serde_json::{Value, json};

    pub fn submit_at(
        m: &mut RuntimeMachine,
        generation: &str,
        name: &str,
        scope: Scope,
        request: HostRequest,
        deadline: Instant,
        now: Instant,
    ) -> Vec<MachineAction> {
        m.apply(MachineInput::Submit { generation: id(generation), request_id: rid(name), scope, request, deadline }, now)
    }
    pub fn host_request(method: &str, params: Value) -> HostRequest {
        let wire = super::mastra_codec_request(method, params, true);
        let decoded = decode_frame(format!("{wire}\n").as_bytes(), DecodePhase::Negotiated, Sender::Host, &Limits::default()).unwrap();
        let DecodeResult::Frame(Frame::Request { body: RequestBody::Host(request), .. }) = decoded else { panic!("host fixture") };
        request
    }
    pub fn callback(m: &mut RuntimeMachine, method: &str, name: &str, scope: Scope, params: Value, now: Instant) -> Vec<MachineAction> {
        peer(m, json!({"protocolVersion":"1.0","kind":"request","requestId":name,"scope":scope,"method":method,"params":params}), now)
    }
    pub fn cancellation(actions: &[MachineAction]) -> Cancellation {
        actions.iter().find_map(|a| if let MachineAction::Dispatch(c) = a { Some(c.cancellation.clone()) } else { None }).unwrap()
    }
    pub fn finish(m: &mut RuntimeMachine, token: CallbackToken, method: &str, now: Instant) -> Vec<MachineAction> {
        let result = decode_host_reply(method, fixture(method).1, &Limits::default()).unwrap();
        m.apply(MachineInput::CallbackFinished { token, result: Box::new(Ok(result)) }, now)
    }
    pub fn proposal() -> Value {
        json!({"state":"validated_success","reason":"unverified proposal","document":{"documentId":"d","revision":"1"},"changes":"unknown","validationRunId":null,"report":null})
    }
    pub fn closed(actions: &[MachineAction], reason: CloseReason) -> usize {
        notices(actions).iter().filter(|n| matches!(n, ClientNotice::TurnClosed { cause, .. } if *cause == reason)).count()
    }
    pub fn turn_with_owned_callback(wall_seconds: u32, now: Instant) -> (RuntimeMachine, Vec<MachineAction>) {
        let mut m = ready(Limits::default(), now);
        let mut params = fixture("turn.start").0;
        params["budgets"]["wallTimeSeconds"] = json!(wall_seconds);
        let a = submit_params(&mut m, "turn.start", "h:turn", scope(), params, now);
        release(&mut m, &a, now);
        let a = peer(&mut m, response("h:turn", scope(), json!({"state":"accepted"})), now);
        release(&mut m, &a, now);
        let mut params = fixture("agent.run").0;
        params["timeoutSeconds"] = json!(300);
        let work = callback(&mut m, "agent.run", "r:owned", scope(), params, now);
        assert!(work.iter().any(|a| matches!(a, MachineAction::Dispatch(_))));
        (m, work)
    }
    pub fn permission() -> CallProgress {
        CallProgress::AgentProgress(AgentProgress {
            agent_session_id: id("agent"),
            content: AgentProgressContent::Permission(PermissionProgress {
                permission_id: id("permission"),
                title: "Late permission".into(),
                state: PermissionState::Waiting,
            }),
        })
    }
    pub fn peer_generation(m: &mut RuntimeMachine, generation: &str, value: Value, now: Instant) -> Vec<MachineAction> {
        let decoded = decode_frame(format!("{value}\n").as_bytes(), DecodePhase::Negotiated, Sender::Runtime, &Limits::default()).unwrap();
        m.apply(MachineInput::Peer { generation: id(generation), sender: Sender::Runtime, decoded }, now)
    }
    pub fn bootstrap(generation: &str, instance: &str, limits: Limits, now: Instant) -> RuntimeMachine {
        let mut m = RuntimeMachine::new(id(generation), limits.clone(), now).unwrap();
        m.apply(
            MachineInput::Services {
                generation: id(generation),
                services: ServiceCapabilities { agent_run: true, ..ServiceCapabilities::default() },
            },
            now,
        );
        let request = HostRequest::RuntimeHello(RuntimeHelloParams {
            supported_versions: vec!["1.0".into()],
            host_version: "1".into(),
            limits: limits.clone(),
        });
        let a = submit_at(&mut m, generation, "h:hello", Scope::Runtime, request, now + Duration::from_secs(10), now);
        for action in a {
            if let MachineAction::Send { ticket, .. } = action {
                m.apply(MachineInput::Written { generation: id(generation), ticket }, now);
            }
        }
        let mut result = fixture("runtime.hello").1;
        result["runtimeInstanceId"] = json!(instance);
        result["capabilities"]["hostAgentTasks"] = json!(true);
        result["limits"] = json!(limits);
        let a = peer_generation(&mut m, generation, response("h:hello", Scope::Runtime, result), now);
        for action in a {
            if let MachineAction::Notice { ticket, .. } = action {
                m.apply(MachineInput::NoticeConsumed { generation: id(generation), ticket }, now);
            }
        }
        let a = peer_generation(
            &mut m,
            generation,
            event(1, "runtime.ready", Some("h:hello"), Scope::Runtime, json!({"runtimeInstanceId":instance})),
            now,
        );
        for action in a {
            if let MachineAction::Notice { ticket, .. } = action {
                m.apply(MachineInput::NoticeConsumed { generation: id(generation), ticket }, now);
            }
        }
        m
    }
    pub fn replacement(now: Instant) -> RuntimeMachine {
        let mut m = bootstrap("g2", "i2", Limits { pending_requests: 1, ..Limits::default() }, now);
        let request =
            HostRequest::SessionStart(SessionStartParams { workflow_id: id("w"), instructions: "new".into(), bridge: RequiredNullable(None) });
        let a = submit_at(&mut m, "g2", "h:session", Scope::Session(id("s2")), request, now + Duration::from_secs(30), now);
        for action in a {
            if let MachineAction::Send { ticket, .. } = action {
                m.apply(MachineInput::Written { generation: id("g2"), ticket }, now);
            }
        }
        let a = peer_generation(&mut m, "g2", response("h:session", Scope::Session(id("s2")), fixture("session.start").1), now);
        for action in a {
            if let MachineAction::Notice { ticket, .. } = action {
                m.apply(MachineInput::NoticeConsumed { generation: id("g2"), ticket }, now);
            }
        }
        m
    }
}

// Break caught: reset publishes before trusted retirement, or an old token/identity
// can revive history, complete a replacement turn, or consume its leases/timers.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_old_generation_cannot_finish_new_turn() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    let mut m = turn(Limits::default(), now);
    let connection =
        HostEvent::ConnectionState(ConnectionStateEvent { connection_id: id("c"), state: ConnectionState::Available, auth_methods: vec![] });
    let a = m.apply(MachineInput::Emit { generation: id("g"), scope: Scope::Session(id("s")), event: connection.clone(), cause: None }, now);
    assert_eq!(frames(&a)[0]["sequence"], 1);
    release(&mut m, &a, now);
    let work = call(&mut m, "agent.run", "r:old", now);
    let old_token = token(&work);
    let cancelled = cancellation(&work);
    let a = submit_params(
        &mut m,
        "session.reset",
        "h:reset",
        Scope::Session(id("s")),
        json!({"nextSessionId":"new","instructions":"fresh history"}),
        now,
    );
    assert!(cancelled.is_cancelled(), "the actual shared Cancellation must be signalled synchronously");
    assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::Reset, .. })));
    assert!(matches!(a.get(1), Some(MachineAction::RetireSession(s)) if s.as_str() == "s"), "retire history before output");
    release(&mut m, &a, now);
    let next = TurnScope { session_id: id("new"), turn_id: id("t"), workflow_run_id: id("w") };
    assert!(frames(&submit(&mut m, "turn.start", "h:before-reset-ack", Scope::Turn(next.clone()), now)).is_empty());
    assert!(frames(&submit(&mut m, "turn.start", "h:old-session", scope(), now)).is_empty());
    assert!(
        frames(&submit(&mut m, "session.start", "h:session-before-reset-ack", Scope::Session(id("other")), now)).is_empty(),
        "a pending reset cannot be bypassed by session.start"
    );
    assert!(finish(&mut m, old_token.clone(), "agent.run", now).is_empty(), "physical completion cannot publish after retirement");
    let a = peer(&mut m, response("h:reset", Scope::Session(id("s")), json!({"state":"ready","sessionId":"new"})), now);
    assert!(!fatal(&a));
    assert!(notices(&a).iter().any(|n| matches!(n, ClientNotice::RequestFinished {scope: Scope::Session(s), result: Ok(RuntimeReply::SessionReset(p)), ..} if s.as_str()=="s" && p.session_id.as_str()=="new")));
    assert!(!a.iter().any(|a| matches!(a, MachineAction::RetireSession(_))), "retirement happens at reset admission, not its reply");
    release(&mut m, &a, now);
    let a = m.apply(MachineInput::Emit { generation: id("g"), scope: Scope::Session(id("new")), event: connection, cause: None }, now);
    assert_eq!(frames(&a)[0]["sequence"], 2, "host sequence is transport lifetime, not reset lifetime");
    release(&mut m, &a, now);
    let a = submit(&mut m, "turn.start", "h:new", Scope::Turn(next.clone()), now);
    assert_eq!(frames(&a).len(), 1);
    release(&mut m, &a, now);
    assert!(frames(&submit(&mut m, "turn.start", "h:turn", Scope::Turn(next.clone()), now)).is_empty(), "transport ledger never resets");
    for sequence in [2, 3] {
        let a = peer(&mut m, event(sequence, "turn.finished", Some("h:turn"), scope(), proposal()), now);
        assert!(a.is_empty(), "old terminal is discarded, not accepted for the new visible turn IDs");
    }
    assert!(frames(&m.apply(MachineInput::Progress { token: old_token.clone(), event: permission() }, now)).is_empty());
    assert!(finish(&mut m, old_token, "agent.run", now).is_empty());
    let a =
        peer(&mut m, event(4, "workflow.text", Some("h:new"), Scope::Turn(next.clone()), json!({"text":"new session","channel":"progress"})), now);
    assert!(!fatal(&a));
    release(&mut m, &a, now);
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(1200));
    assert_eq!(closed(&a, CloseReason::Deadline), 1, "old activity must not clear the replacement turn timer");

    // A different transport can reuse visible request/turn names, but never its
    // native generation, runtime instance or session identity. No replay occurs.
    let mut fresh = replacement(now);
    let next = TurnScope { session_id: id("s2"), turn_id: id("t"), workflow_run_id: id("w") };
    let request = host_request("turn.start", fixture("turn.start").0);
    let a = submit_at(&mut fresh, "g2", "h:turn", Scope::Turn(next.clone()), request, now + Duration::from_secs(300), now);
    assert_eq!(frames(&a).len(), 1);
    for a in a {
        if let MachineAction::Send { ticket, .. } = a {
            fresh.apply(MachineInput::Written { generation: id("g2"), ticket }, now);
        }
    }
    let mut wire = mastra_codec_request("agent.run", fixture("agent.run").0, false);
    wire["requestId"] = json!("r:old");
    wire["scope"] = json!(next);
    let work2 = peer_generation(&mut fresh, "g2", wire, now);
    let fresh_token = token(&work2);
    let old_work = call(&mut turn(Limits::default(), now), "agent.run", "r:old", now);
    let old = token(&old_work);
    assert!(finish(&mut fresh, old.clone(), "agent.run", now).is_empty());
    assert!(fresh.apply(MachineInput::Progress { token: old, event: permission() }, now).is_empty());
    for input in [
        MachineInput::Eof { generation: id("g"), stderr_tail: vec![] },
        MachineInput::InputFailed { generation: id("g") },
        MachineInput::CompleteTurn { generation: id("g"), scope: next.clone(), terminal: TurnTerminal::ValidatedSuccess },
    ] {
        assert!(fresh.apply(input, now + Duration::from_secs(600)).is_empty(), "stale native input cannot even tick replacement timers");
    }
    let reply = finish(&mut fresh, fresh_token, "agent.run", now);
    assert_eq!(frames(&reply).len(), 1, "only the real replacement token can publish");
    for a in &reply {
        if let MachineAction::Send { ticket, .. } = a {
            fresh.apply(MachineInput::Written { generation: id("g"), ticket: *ticket }, now);
        }
    }
    let mut wire = mastra_codec_request("agent.run", fixture("agent.run").0, false);
    wire["requestId"] = json!("r:new");
    wire["scope"] = json!(next);
    assert!(
        !peer_generation(&mut fresh, "g2", wire, now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))),
        "old generation cannot clear the new reply ticket"
    );
}

// Break caught: publishing a proposal as acceptance, replacing the first closure,
// or signalling user cancellation on otherwise active host verification.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_success_proposal_is_not_host_acceptance() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    let Scope::Turn(current) = scope() else { panic!() };
    let next = TurnScope { turn_id: id("next"), ..current.clone() };
    let mut m = turn(Limits::default(), now);
    let work = call(&mut m, "agent.run", "r:verify", now);
    let verification = cancellation(&work);
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal()), now);
    assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::TerminalProposal, .. })));
    assert!(!verification.is_cancelled(), "proposal is not user cancellation of host verification");
    assert_eq!(closed(&a, CloseReason::User), 0);
    assert_eq!(notices(&a).iter().filter(|n| matches!(n, ClientNotice::TurnFinishedProposal { .. })).count(), 1);
    release(&mut m, &a, now);
    assert!(frames(&submit(&mut m, "turn.start", "h:too-early", Scope::Turn(next.clone()), now)).is_empty());
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: next.clone(), terminal: TurnTerminal::ValidatedSuccess }, now);
    assert!(frames(&submit(&mut m, "turn.start", "h:still-early", Scope::Turn(next.clone()), now)).is_empty());
    assert!(peer(&mut m, event(3, "turn.finished", Some("h:turn"), scope(), proposal()), now).is_empty());
    assert!(finish(&mut m, token(&work), "agent.run", now).is_empty());
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: current, terminal: TurnTerminal::TaskFailed }, now);
    assert_eq!(frames(&submit(&mut m, "turn.start", "h:next", Scope::Turn(next), now)).len(), 1);

    let mut m = turn(Limits::default(), now);
    let work = call(&mut m, "agent.run", "r:cancelled", now);
    let cancel = cancellation(&work);
    let a = submit(&mut m, "turn.cancel", "h:cancel", scope(), now);
    assert!(cancel.is_cancelled());
    assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::User, .. })), "closure must precede wire output");
    assert_eq!(closed(&a, CloseReason::User), 1);
    assert_eq!(frames(&a).iter().filter(|v| v["requestId"] == "r:cancelled" && v["error"]["code"] == "CANCELLED").count(), 1);
    release(&mut m, &a, now);
    for sequence in [2, 3] {
        assert!(peer(&mut m, event(sequence, "turn.finished", Some("h:turn"), scope(), proposal()), now).is_empty());
    }
    assert!(finish(&mut m, token(&work), "agent.run", now).is_empty());
    assert!(
        m.apply(MachineInput::Tick, now + Duration::from_secs(1200)).iter().all(|a| !matches!(a, MachineAction::CloseTurn { .. })),
        "first closure is immutable"
    );
}

// Break caught: off-by-one expiry, only checking deadlines on completion, treating
// TIMEOUT as explicit cancellation, or extending timers on repeated Stop/ack.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_deadlines_and_cancel_ack() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    let epsilon = Duration::from_nanos(1);
    let mut m = machine(Limits::default(), now);
    let a = submit(&mut m, "runtime.hello", "h:hello", Scope::Runtime, now);
    release(&mut m, &a, now);
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(10) - epsilon).is_empty());
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(10));
    assert!(fatal(&a));
    assert!(
        notices(&a).iter().any(|n| matches!(n, ClientNotice::RequestFinished { result: Err(e), .. } if e.code == ErrorCode::Timeout)),
        "handshake expiry is TIMEOUT before startup failure"
    );

    for (requested, seconds) in [(7, 7), (300, 30)] {
        let mut m = bootstrap("g", "i", Limits::default(), now);
        let request = host_request("session.start", fixture("session.start").0);
        let a = submit_at(&mut m, "g", "h:session", Scope::Session(id("s")), request, now + Duration::from_secs(requested), now);
        release(&mut m, &a, now);
        assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(seconds) - epsilon).is_empty());
        let a = peer(&mut m, response("h:session", Scope::Session(id("s")), fixture("session.start").1), now + Duration::from_secs(seconds));
        assert!(fatal(&a), "session start failure ends attempted startup, including exact-deadline replies");
        assert_eq!(
            notices(&a).iter().filter(|n| matches!(n, ClientNotice::RequestFinished { result: Err(e), .. } if e.code == ErrorCode::Timeout)).count(),
            1
        );
    }

    let mut m = turn(Limits::default(), now);
    let work = call(&mut m, "model.generate", "r:timeout", now);
    let cancel = cancellation(&work);
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(1) - epsilon).is_empty());
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(1));
    assert_eq!(frames(&a)[0]["error"]["code"], "TIMEOUT");
    assert!(cancel.is_cancelled());
    assert!(finish(&mut m, token(&work), "model.generate", now + Duration::from_secs(1)).is_empty());

    for (limits, timeout, start, expected) in [
        (Limits::default(), 1000, 0, 600),
        (Limits { call_default_seconds: 2, call_max_seconds: 2, ..Limits::default() }, 1000, 0, 2),
        (Limits::default(), 300, 1199, 1200),
    ] {
        for method in ["model.generate", "agent.run"] {
            let mut m = turn(limits.clone(), now);
            let a = peer(&mut m, response("h:turn", scope(), json!({"state":"accepted"})), now);
            release(&mut m, &a, now);
            let mut params = fixture(method).0;
            if method == "model.generate" {
                params["options"]["timeoutSeconds"] = json!(timeout);
            } else {
                params["timeoutSeconds"] = json!(timeout);
            }
            let work = callback(&mut m, method, "r:clamped", scope(), params, now + Duration::from_secs(start));
            assert!(work.iter().any(|a| matches!(a, MachineAction::Dispatch(c) if c.deadline == now + Duration::from_secs(expected))));
            assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(expected) - epsilon).is_empty());
            let a = m.apply(MachineInput::Tick, now + Duration::from_secs(expected));
            if start == 1199 {
                assert!(
                    matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::Deadline, .. })),
                    "wall retirement precedes the clamped call timeout reply"
                );
            }
            assert_eq!(frames(&a).iter().filter(|v| v["requestId"] == "r:clamped" && v["error"]["code"] == "TIMEOUT").count(), 1);
            assert!(cancellation(&work).is_cancelled());
        }
    }

    let mut m = turn(Limits::default(), now);
    let a = submit(&mut m, "turn.cancel", "h:cancel", scope(), now);
    release(&mut m, &a, now);
    let a = submit(&mut m, "turn.cancel", "h:again", scope(), now + Duration::from_secs(4));
    assert!(matches!(a.as_slice(), [MachineAction::Coalesced { .. }]));
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(5) - epsilon).is_empty());
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(5));
    assert_eq!(
        a.iter().filter(|a| matches!(a, MachineAction::StopTree)).count(),
        1,
        "unacknowledged Stop invokes the existing tree stop exactly at 5s"
    );
    assert!(!fatal(&m.apply(MachineInput::Tick, now + Duration::from_secs(8) - epsilon)));
    assert!(fatal(&m.apply(MachineInput::Tick, now + Duration::from_secs(8))), "teardown has only a further 3s");

    let mut m = turn(Limits::default(), now);
    let a = submit(&mut m, "turn.cancel", "h:cancel", scope(), now);
    release(&mut m, &a, now);
    let a = peer(&mut m, response("h:cancel", scope(), json!({"state":"cancelling"})), now + Duration::from_secs(5) - epsilon);
    release(&mut m, &a, now);
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(5)).is_empty(), "ack clears only its acknowledgement timer");
    assert!(peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(5)).is_empty());
    assert!(matches!(
        submit(&mut m, "turn.cancel", "h:after-ack", scope(), now + Duration::from_secs(6)).as_slice(),
        [MachineAction::Coalesced { .. }]
    ));

    let mut m = turn(Limits::default(), now);
    let a = submit(&mut m, "turn.cancel", "h:cancel", scope(), now);
    release(&mut m, &a, now);
    let a = peer(&mut m, response("h:cancel", scope(), json!({"state":"terminal"})), now + Duration::from_secs(5));
    assert!(a.iter().any(|a| matches!(a, MachineAction::StopTree)), "ack at the exact deadline is too late");

    let mut m = turn(Limits::default(), now);
    let a = peer(&mut m, response("h:turn", scope(), json!({"state":"accepted"})), now);
    release(&mut m, &a, now);
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(1200) - epsilon).is_empty());
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(1200));
    assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::Deadline, .. })));
    assert_eq!(closed(&a, CloseReason::Deadline), 1, "eventual host terminal maps wall closure to budget_exhausted, not user cancelled");

    let mut m = turn(Limits::default(), now);
    let a = m.apply(MachineInput::Shutdown { generation: id("g") }, now);
    assert_eq!(a.iter().filter(|a| matches!(a, MachineAction::StopTree)).count(), 1);
    assert!(
        m.apply(MachineInput::Shutdown { generation: id("g") }, now + Duration::from_secs(2)).is_empty(),
        "repeated shutdown neither repeats cleanup nor extends teardown"
    );
    assert!(!fatal(&m.apply(MachineInput::Tick, now + Duration::from_secs(3) - epsilon)));
    assert!(fatal(&m.apply(MachineInput::Tick, now + Duration::from_secs(3))));
}

// Break caught: EOF/read/write/protocol failure loses or duplicates a pending
// completion, retires history after notification, or accepts an unterminated frame.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_eof_resolves_every_call_once() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    for mode in 0..4 {
        let mut m = turn(Limits::default(), now);
        let work = call(&mut m, "agent.run", "r:running", now);
        let cancel = cancellation(&work);
        let input = match mode {
            0 => MachineInput::Eof { generation: id("g"), stderr_tail: vec!["bounded diagnostic".into()] },
            1 => MachineInput::InputFailed { generation: id("g") },
            2 => MachineInput::WriteFailed { generation: id("g") },
            _ => {
                let partial = response("h:turn", scope(), json!({"state":"accepted"})).to_string();
                assert!(decode_frame(partial.as_bytes(), DecodePhase::Negotiated, Sender::Runtime, &Limits::default()).is_err());
                MachineInput::InputFailed { generation: id("g") }
            }
        };
        let a = m.apply(input, now);
        assert!(cancel.is_cancelled());
        assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: CloseReason::TransportLoss, .. })));
        assert!(matches!(a.get(1), Some(MachineAction::RetireSession(_))), "trusted retirement precedes every completion/cleanup notice");
        assert_eq!(notices(&a).iter().filter(|n| matches!(n, ClientNotice::RequestFinished {request_id,result:Err(e),..} if request_id.as_str()=="h:turn" && e.code==ErrorCode::TransportError)).count(), 1);
        assert_eq!(closed(&a, CloseReason::TransportLoss), 1);
        assert!(fatal(&a));
        assert!(m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now).is_empty());
        assert!(m.apply(MachineInput::WriteFailed { generation: id("g") }, now).is_empty());
        assert!(finish(&mut m, token(&work), "agent.run", now).is_empty());
        assert!(frames(&submit(&mut m, "session.start", "h:no-replay", Scope::Session(id("fresh")), now)).is_empty());
    }
    let mut m = ready(Limits::default(), now);
    for n in 0..3 {
        let s = TurnScope { session_id: id("s"), turn_id: id(&format!("t{n}")), workflow_run_id: id(&format!("w{n}")) };
        let a = submit(&mut m, "turn.start", &format!("h:pending{n}"), Scope::Turn(s.clone()), now);
        release(&mut m, &a, now);
        let a = peer(&mut m, event(n + 2, "turn.finished", Some(&format!("h:pending{n}")), Scope::Turn(s.clone()), proposal()), now);
        release(&mut m, &a, now);
        m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: s, terminal: TurnTerminal::TaskFailed }, now);
    }
    let a = m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now);
    assert_eq!(
        notices(&a).iter().filter(|n| matches!(n, ClientNotice::RequestFinished {result: Err(e), ..} if e.code == ErrorCode::TransportError)).count(),
        3,
        "all outstanding identities, not just the current turn, resolve once"
    );
    assert!(m.apply(MachineInput::InputFailed { generation: id("g") }, now).is_empty());
    // A closure reply can itself discover a failed/full physical pipe. Neither
    // shutdown entry point may overwrite that failure with Stopping or emit its
    // previously reserved shutdown request after transport termination.
    for local in [false, true] {
        let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
        let work = call(&mut m, "agent.run", "r:owned", now);
        let event =
            HostEvent::ConnectionState(ConnectionStateEvent { connection_id: id("c"), state: ConnectionState::Available, auth_methods: vec![] });
        let a = m.apply(MachineInput::Emit { generation: id("g"), scope: scope(), event, cause: None }, now);
        assert_eq!(frames(&a).len(), 1); // Deliberately not Written.
        let a = if local {
            m.apply(MachineInput::Shutdown { generation: id("g") }, now)
        } else {
            submit(&mut m, "runtime.shutdown", "h:shutdown", Scope::Runtime, now)
        };
        assert!(fatal(&a));
        assert!(cancellation(&work).is_cancelled());
        assert!(!frames(&a).iter().any(|v| v["requestId"] == "h:shutdown"), "no reserved request can be published after failure");
        assert!(m.apply(MachineInput::InputFailed { generation: id("g") }, now).is_empty(), "shutdown cannot reopen a failed transport");
    }
    let mut m = ready(Limits::default(), now);
    let a = submit(&mut m, "runtime.shutdown", "h:shutdown", Scope::Runtime, now);
    release(&mut m, &a, now);
    let a = peer(&mut m, response("h:shutdown", Scope::Runtime, json!({"state":"stopping"})), now);
    release(&mut m, &a, now);
    let a = m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now);
    assert!(!fatal(&a));
    assert!(notices(&a).iter().any(|n| matches!(n, ClientNotice::TransportClosed { cause: TransportCause::ExpectedEof, .. })));
}

// Break caught: one sender cancels the opposite sender's request, or timeout/
// logical cancellation frees the actual running callback before finish + Written.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_request_cancel_is_sender_scoped() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
    let work = call(&mut m, "model.generate", "r:work", now);
    let cancel = cancellation(&work);
    let a = submit_at(
        &mut m,
        "g",
        "h:wrong-sender",
        scope(),
        HostRequest::RequestCancel(RequestCancelParams { target_request_id: rid("r:work") }),
        now + Duration::from_secs(300),
        now,
    );
    assert!(matches!(a.as_slice(), [MachineAction::Rejected { error, .. }] if error.code == ErrorCode::InvalidRequest));
    assert!(!cancel.is_cancelled());
    let a = callback(&mut m, "request.cancel", "r:wrong-sender", scope(), json!({"targetRequestId":"h:turn"}), now);
    assert_eq!(frames(&a)[0]["error"]["code"], "INVALID_REQUEST");
    assert!(!fatal(&a));
    release(&mut m, &a, now);
    let a = callback(&mut m, "request.cancel", "r:cancel", scope(), json!({"targetRequestId":"r:work"}), now);
    assert!(cancel.is_cancelled(), "real Cancellation clone observes irrevocable logical cancellation");
    assert_eq!(frames(&a).iter().filter(|v| v["requestId"] == "r:work" && v["error"]["code"] == "CANCELLED").count(), 1);
    release(&mut m, &a, now);
    let a = call(&mut m, "model.generate", "r:still-full", now);
    assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))), "logical cancel cannot release a running service's slot");
    release(&mut m, &a, now);
    assert!(finish(&mut m, token(&work), "model.generate", now).is_empty());
    assert!(cancel.is_cancelled());
    assert!(call(&mut m, "model.generate", "r:after-physical-exit", now).iter().any(|a| matches!(a, MachineAction::Dispatch(_))));

    for written_first in [false, true] {
        let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
        let work = call(&mut m, "model.generate", "r:timed-out", now);
        let timeout = m.apply(MachineInput::Tick, now + Duration::from_secs(1));
        assert_eq!(frames(&timeout)[0]["error"]["code"], "TIMEOUT");
        if written_first {
            release(&mut m, &timeout, now + Duration::from_secs(1));
        } else {
            assert!(finish(&mut m, token(&work), "model.generate", now + Duration::from_secs(1)).is_empty());
        }
        let a = call(&mut m, "model.generate", "r:still-owned", now + Duration::from_secs(1));
        assert!(!a.iter().any(|a| matches!(a, MachineAction::Dispatch(_))), "timeout also retains both physical ownership halves");
        if !written_first {
            assert!(fatal(&a), "a second reply cannot bypass the one physically occupied data frame slot");
        }
        // A full physical lane fails closed, so prove the successful both-halves
        // path in an independent machine, without first poisoning its transport.
        let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
        let work = call(&mut m, "model.generate", "r:timed-out", now);
        let timeout = m.apply(MachineInput::Tick, now + Duration::from_secs(1));
        if written_first {
            release(&mut m, &timeout, now + Duration::from_secs(1));
        } else {
            assert!(finish(&mut m, token(&work), "model.generate", now + Duration::from_secs(1)).is_empty());
        }
        if written_first {
            assert!(finish(&mut m, token(&work), "model.generate", now + Duration::from_secs(1)).is_empty());
        } else {
            release(&mut m, &timeout, now + Duration::from_secs(1));
        }
        let released = call(&mut m, "model.generate", "r:released", now + Duration::from_secs(1));
        assert!(released.iter().any(|a| matches!(a, MachineAction::Dispatch(_))), "written_first={written_first}: {released:?}");
    }
}

// I1: closing work admission for an unaccepted proposal must not suppress a
// subsequent real Stop, replace the first interruption, or extend its timers.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_proposal_then_stop_cancels_once() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    let (mut m, work) = turn_with_owned_callback(5, now);
    let cancel = cancellation(&work);
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(1));
    assert!(matches!(a.as_slice(), [MachineAction::CloseTurn { reason: CloseReason::TerminalProposal, .. }, MachineAction::Notice { .. }]));
    assert!(!cancel.is_cancelled());
    release(&mut m, &a, now + Duration::from_secs(1));
    let stopped = submit(&mut m, "turn.cancel", "h:stop", scope(), now + Duration::from_secs(2));
    assert!(cancel.is_cancelled(), "proposal is not accepted host terminal: later Stop must signal the actual Dispatch Cancellation");
    assert!(
        matches!(
            stopped.as_slice(),
            [MachineAction::CloseTurn { reason: CloseReason::User, .. }, MachineAction::Notice { .. }, MachineAction::Send { .. }]
        ),
        "trusted user closure must precede the one notice and wire cancel: {stopped:?}"
    );
    assert_eq!(closed(&stopped, CloseReason::User), 1);
    release(&mut m, &stopped, now + Duration::from_secs(2));
    let repeated = submit(&mut m, "turn.cancel", "h:repeat", scope(), now + Duration::from_secs(3));
    assert!(matches!(repeated.as_slice(), [MachineAction::Coalesced { existing_request_id, .. }] if existing_request_id.as_str()=="h:stop"));
    assert!(peer(&mut m, event(3, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(3)).is_empty());
    assert!(finish(&mut m, token(&work), "agent.run", now + Duration::from_secs(3)).is_empty());
    let Scope::Turn(current) = scope() else { panic!() };
    let next = TurnScope { turn_id: id("next"), ..current.clone() };
    m.apply(
        MachineInput::CompleteTurn { generation: id("g"), scope: next.clone(), terminal: TurnTerminal::ValidatedSuccess },
        now + Duration::from_secs(3),
    );
    assert!(frames(&submit(&mut m, "turn.start", "h:too-soon", Scope::Turn(next.clone()), now + Duration::from_secs(3))).is_empty());
    m.apply(MachineInput::CompleteTurn { generation: id("g"), scope: current, terminal: TurnTerminal::Cancelled }, now + Duration::from_secs(3));
    let a = submit(&mut m, "turn.start", "h:next", Scope::Turn(next), now + Duration::from_secs(3));
    assert_eq!(frames(&a).len(), 1, "only matching local host completion unlocks");
    release(&mut m, &a, now + Duration::from_secs(3));
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(5)).is_empty(), "the retired turn's original deadline cannot close the new turn");
    assert!(cancel.is_cancelled());

    let (mut m, work) = turn_with_owned_callback(10, now);
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(1));
    release(&mut m, &a, now + Duration::from_secs(1));
    let a = submit(&mut m, "turn.cancel", "h:stop", scope(), now + Duration::from_secs(2));
    release(&mut m, &a, now + Duration::from_secs(2));
    assert!(cancellation(&work).is_cancelled());
    assert!(matches!(submit(&mut m, "turn.cancel", "h:repeat", scope(), now + Duration::from_secs(6)).as_slice(), [MachineAction::Coalesced { .. }]));
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(7) - Duration::from_nanos(1)).is_empty());
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(7));
    assert_eq!(a.iter().filter(|a| matches!(a, MachineAction::StopTree)).count(), 1, "the original 2+5s acknowledgement deadline is unchanged");
    assert!(!a.iter().any(|a| matches!(a, MachineAction::CloseTurn { .. })), "shutdown fallback cannot replace first actual User interruption");
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(10));
    assert!(fatal(&a));
    assert!(!a.iter().any(|a| matches!(a, MachineAction::CloseTurn { .. })), "wall and transport loss cannot overwrite User");
}

// I1: Finishing while awaiting host acceptance is still inside the original wall
// budget. Reset/shutdown/transport loss also remain real interruptions after a proposal.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_proposal_keeps_original_deadline_and_interruptions() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    let (mut m, work) = turn_with_owned_callback(5, now);
    let cancel = cancellation(&work);
    let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(4));
    release(&mut m, &a, now + Duration::from_secs(4));
    assert!(!cancel.is_cancelled());
    assert!(m.apply(MachineInput::Tick, now + Duration::from_secs(5) - Duration::from_nanos(1)).is_empty());
    assert!(!cancel.is_cancelled());
    let a = m.apply(MachineInput::Tick, now + Duration::from_secs(5));
    assert!(
        matches!(a.as_slice(), [MachineAction::CloseTurn { reason: CloseReason::Deadline, .. }, MachineAction::Notice { .. }]),
        "proposal does not suspend/restart the original wall budget: {a:?}"
    );
    assert!(cancel.is_cancelled());
    assert_eq!(closed(&a, CloseReason::Deadline), 1);
    release(&mut m, &a, now + Duration::from_secs(5));
    assert!(peer(&mut m, event(3, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(5)).is_empty());
    let a = submit(&mut m, "turn.cancel", "h:late-stop", scope(), now + Duration::from_secs(5));
    assert!(matches!(a.as_slice(), [MachineAction::Send { .. }]), "a later user Stop cannot replace the actual Deadline closure");
    let a = m.apply(MachineInput::Shutdown { generation: id("g") }, now + Duration::from_secs(5));
    assert!(!a.iter().any(|a| matches!(a, MachineAction::CloseTurn { .. })));

    for reason in [CloseReason::Reset, CloseReason::Shutdown, CloseReason::TransportLoss] {
        let (mut m, work) = turn_with_owned_callback(10, now);
        let cancel = cancellation(&work);
        let a = peer(&mut m, event(2, "turn.finished", Some("h:turn"), scope(), proposal()), now + Duration::from_secs(1));
        release(&mut m, &a, now + Duration::from_secs(1));
        assert!(!cancel.is_cancelled());
        let at = now + Duration::from_secs(2);
        let a = match reason {
            CloseReason::Reset => {
                submit_params(&mut m, "session.reset", "h:reset", Scope::Session(id("s")), json!({"nextSessionId":"new","instructions":"reset"}), at)
            }
            CloseReason::Shutdown => submit(&mut m, "runtime.shutdown", "h:shutdown", Scope::Runtime, at),
            _ => m.apply(MachineInput::InputFailed { generation: id("g") }, at),
        };
        assert!(cancel.is_cancelled());
        assert!(matches!(a.first(), Some(MachineAction::CloseTurn { reason: actual, .. }) if *actual==reason));
        assert!(matches!(a.get(1), Some(MachineAction::RetireSession(s)) if s.as_str()=="s"));
        assert_eq!(closed(&a, reason), 1);
        let repeated = m.apply(MachineInput::InputFailed { generation: id("g") }, at);
        assert!(!repeated.iter().any(|a| matches!(a, MachineAction::CloseTurn { .. })));
        assert!(finish(&mut m, token(&work), "agent.run", at).is_empty());
    }
}

// I2: target cancellation may discover a full physical data lane. That fatal
// transition must end control processing before an ack/ticket can be produced.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_retirement_failed_cancel_reply_cannot_ack_after_transport_closed() {
    use mastra_retirement_support::*;
    let now = Instant::now();
    for written_first in [false, true] {
        let mut m = turn(Limits { pending_requests: 1, ..Limits::default() }, now);
        let work = call(&mut m, "agent.run", "r:owned", now);
        let cancel = cancellation(&work);
        let event =
            HostEvent::ConnectionState(ConnectionStateEvent { connection_id: id("c"), state: ConnectionState::Available, auth_methods: vec![] });
        let occupied = m.apply(MachineInput::Emit { generation: id("g"), scope: scope(), event, cause: None }, now);
        assert!(matches!(occupied.as_slice(), [MachineAction::Send { ticket: 7, .. }])); // Not Written.
        let failed = callback(&mut m, "request.cancel", "r:control", scope(), json!({"targetRequestId":"r:owned"}), now);
        assert!(cancel.is_cancelled(), "the actual running action is signalled before the fatal actions are returned");
        assert!(
            matches!(
                failed.as_slice(),
                [
                    MachineAction::CloseTurn { reason: CloseReason::TransportLoss, .. },
                    MachineAction::RetireSession(_),
                    MachineAction::Notice { ticket: 8, .. },
                    MachineAction::Notice { ticket: 9, .. },
                    MachineAction::Notice { ticket: 10, .. },
                    MachineAction::StopTree,
                ]
            ),
            "retire first, complete once, stop last: no late Send/new reply ticket: {failed:?}"
        );
        assert!(matches!(notices(&failed).as_slice(), [
            ClientNotice::TurnClosed { cause: CloseReason::TransportLoss, .. },
            ClientNotice::RequestFinished { request_id, result: Err(e), .. },
            ClientNotice::TransportClosed { cause: TransportCause::Error(_), .. },
        ] if request_id.as_str()=="h:turn" && e.code==ErrorCode::TransportError));
        assert!(frames(&failed).is_empty(), "neither the failed target response nor a successful control ack can be enqueued");
        assert!(m.apply(MachineInput::InputFailed { generation: id("g") }, now).is_empty());
        assert!(m.apply(MachineInput::WriteFailed { generation: id("g") }, now).is_empty());
        // Logical retirement did not physically finish either owner. The old
        // frame's accurate Written and real CallbackFinished may arrive in
        // either order, without publishing or reopening anything.
        if written_first {
            assert!(m.apply(MachineInput::Written { generation: id("g"), ticket: 7 }, now).is_empty());
            assert!(finish(&mut m, token(&work), "agent.run", now).is_empty());
        } else {
            assert!(finish(&mut m, token(&work), "agent.run", now).is_empty());
            assert!(m.apply(MachineInput::Written { generation: id("g"), ticket: 7 }, now).is_empty());
        }
        assert!(finish(&mut m, token(&work), "agent.run", now).is_empty());
        assert!(m.apply(MachineInput::Written { generation: id("g"), ticket: 7 }, now).is_empty());
        assert!(m.apply(MachineInput::Eof { generation: id("g"), stderr_tail: vec![] }, now).is_empty());
        let progress = m.apply(MachineInput::Progress { token: token(&work), event: permission() }, now);
        assert!(matches!(progress.as_slice(), [MachineAction::Rejected { error, .. }] if error.code==ErrorCode::InvalidRequest));
        assert!(frames(&submit(&mut m, "session.start", "h:no-restart", Scope::Session(id("new")), now)).is_empty());
        assert!(cancel.is_cancelled());
    }
}

type MastraMethodCase = (String, Value, Value, bool);

// Task7 contract consumers must fail on a missing artifact; ordinary tests never record it.
#[cfg(not(target_arch = "wasm32"))]
fn mastra_bundle_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/mastra-runtime/v1")
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_schema_matches_rust() {
    mastra_bundle::schema_agreement().unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_fixture_agreement() {
    mastra_bundle::fixture_agreement(&mastra_bundle_path()).unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_transcript_agreement() {
    mastra_bundle::transcript_agreement(&mastra_bundle_path()).unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_manifest_is_exact() {
    mastra_bundle::manifest_agreement(&mastra_bundle_path()).unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_coverage_is_complete() {
    mastra_bundle::coverage_agreement(&mastra_bundle_path()).unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
fn mastra_schema_accepts(schema_ref: &str, value: &Value) -> Result<bool, String> {
    mastra_bundle::Evaluator::new(&mastra_bundle_path())?.accepts(schema_ref, value)
}

#[cfg(not(target_arch = "wasm32"))]
fn mastra_canonical_bytes(value: &Value) -> Result<Vec<u8>, String> {
    serde_json_canonicalizer::to_vec(value).map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "explicit contract recording only; requires SOLIDCRAFT_MASTRA_RECORD=1"]
fn mastra_bundle_record() {
    assert_eq!(std::env::var("SOLIDCRAFT_MASTRA_RECORD").as_deref(), Ok("1"));
    mastra_bundle::record().unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_generated_contract_is_sound() {
    mastra_bundle::contract_review().unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_jcs_uses_rfc8785() {
    let numbers: Value = serde_json::from_str("[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001,-0.0]").unwrap();
    assert_eq!(mastra_canonical_bytes(&numbers).unwrap(), b"[333333333.3333333,1e+30,4.5,0.002,1e-27,0]");
    let keys: Value =
        serde_json::from_str(r#"{"\u20ac":"Euro","\r":"CR","\ufb33":"Hebrew","1":"One","\ud83d\ude00":"Emoji","\u0080":"Control","\u00f6":"Latin"}"#)
            .unwrap();
    assert_eq!(
        String::from_utf8(mastra_canonical_bytes(&keys).unwrap()).unwrap(),
        "{\"\\r\":\"CR\",\"1\":\"One\",\"\u{80}\":\"Control\",\"ö\":\"Latin\",\"€\":\"Euro\",\"😀\":\"Emoji\",\"דּ\":\"Hebrew\"}"
    );
    let identities = json!({"revision":"18446744073709551615","digest":format!("sha256:{}","a".repeat(64))});
    let canonical = mastra_canonical_bytes(&identities).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&canonical).unwrap(), identities);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn mastra_bundle_mutations_fail_closed() {
    mastra_bundle::mutations().unwrap();
}

// Test-only implementations describe the ACTUAL wire types, not a second DTO set.
#[cfg(not(target_arch = "wasm32"))]
mod mastra_bundle {
    use super::super::mastra_protocol::*;
    use super::super::runtime_client::*;
    use super::{mastra_bundle_path, mastra_canonical_bytes, mastra_codec_catalog, mastra_codec_request, mastra_codec_scope};
    use schemars::{JsonSchema, Schema, SchemaGenerator};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use std::borrow::Cow;
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    type Check<T = ()> = Result<T, String>;
    fn require(ok: bool, why: impl Into<String>) -> Check {
        if ok { Ok(()) } else { Err(why.into()) }
    }
    fn bytes(value: &Value) -> Vec<u8> {
        let mut b = serde_json::to_vec_pretty(value).unwrap();
        b.push(b'\n');
        b
    }
    fn line(value: &Value) -> Vec<u8> {
        let mut b = serde_json::to_vec(value).unwrap();
        b.push(b'\n');
        b
    }
    fn read(root: &Path, path: &str) -> Check<Vec<u8>> {
        safe_path(path)?;
        std::fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))
    }
    fn value(root: &Path, path: &str) -> Check<Value> {
        serde_json::from_slice(&read(root, path)?).map_err(|e| format!("{path}: {e}"))
    }
    fn digest(b: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(b))
    }
    fn reference(name: &str) -> Value {
        json!({"$ref":format!("#/$defs/{name}")})
    }
    fn object(properties: Value, required: &[&str]) -> Value {
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
    fn string_pattern(pattern: &str) -> Schema {
        json!({"type":"string","pattern":format!("^(?:{pattern})$(?![\\s\\S])")}).try_into().unwrap()
    }
    // Canonical decimal <= max, expressed lexically; no JSON floating point bound.
    fn decimal_pattern(max: u64) -> String {
        let m = max.to_string();
        let mut alternatives = vec!["0".to_owned(), format!("[1-9][0-9]{{0,{}}}", m.len() - 2), m.clone()];
        for (i, digit) in m.bytes().enumerate() {
            let low = if i == 0 { b'1' } else { b'0' };
            if digit > low {
                alternatives.push(format!("{}[{}-{}][0-9]{{{}}}", &m[..i], low as char, (digit - 1) as char, m.len() - i - 1));
            }
        }
        alternatives.join("|")
    }
    macro_rules! primitive {
        ($ty:ty, $body:expr) => {
            impl JsonSchema for $ty {
                fn schema_name() -> Cow<'static, str> {
                    stringify!($ty).into()
                }
                fn json_schema(_: &mut SchemaGenerator) -> Schema {
                    $body
                }
            }
        };
    }
    primitive!(Id, string_pattern("[A-Za-z0-9][A-Za-z0-9._:-]{0,127}"));
    primitive!(Digest, string_pattern("sha256:[0-9a-f]{64}"));
    primitive!(Revision, string_pattern(&decimal_pattern(u64::MAX)));
    primitive!(RequestId, string_pattern("[hr]:[A-Za-z0-9._:-]{0,126}"));
    primitive!(Timestamp, {
        let mut s = string_pattern("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\\.[0-9]{3}Z");
        s.insert("format".into(), json!("date-time"));
        s
    });
    primitive!(SubmissionKey, string_pattern("[0-9]+:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}"));
    impl<T: JsonSchema> JsonSchema for RequiredNullable<T> {
        fn schema_name() -> Cow<'static, str> {
            format!("RequiredNullable_{}", T::schema_name()).into()
        }
        fn json_schema(g: &mut SchemaGenerator) -> Schema {
            g.subschema_for::<Option<T>>()
        }
        // Deliberately NOT schemars' optional-field marker: the field is required.
    }
    impl JsonSchema for Scope {
        fn schema_name() -> Cow<'static, str> {
            "Scope".into()
        }
        fn json_schema(g: &mut SchemaGenerator) -> Schema {
            let id = serde_json::to_value(g.subschema_for::<Id>()).unwrap();
            let scopes = [(json!(null), json!(null), json!(null)), (id.clone(), json!(null), json!(null)), (id.clone(), id.clone(), id)];
            json!({"anyOf":scopes.into_iter().map(|(s,t,w)| object(json!({"sessionId":if s.is_null(){json!({"type":"null"})}else{s},"turnId":if t.is_null(){json!({"type":"null"})}else{t},"workflowRunId":if w.is_null(){json!({"type":"null"})}else{w}}), &["sessionId","turnId","workflowRunId"])).collect::<Vec<_>>()}).try_into().unwrap()
        }
    }
    impl JsonSchema for WireError {
        fn schema_name() -> Cow<'static, str> {
            "WireError".into()
        }
        fn json_schema(g: &mut SchemaGenerator) -> Schema {
            let mut s = object(
                json!({"code":g.subschema_for::<ErrorCode>(),"message":{"type":"string","maxLength":2048},"retry":g.subschema_for::<Retry>(),"details":{"type":"object"}}),
                &["code", "message", "retry", "details"],
            );
            let mut conditions = Vec::new();
            macro_rules! branch { ($ty:ty, $($code:literal),+ ) => {
                conditions.push(json!({"if":{"properties":{"code":{"enum":[$($code),+]}}},"then":{"properties":{"details":{"anyOf":[g.subschema_for::<EmptyDetails>(),g.subschema_for::<$ty>()]}}}}));
            }; }
            branch!(SupportedDetails, "INCOMPATIBLE_VERSION");
            branch!(FieldMethodDetails, "INVALID_REQUEST", "METHOD_NOT_FOUND");
            branch!(CapabilityDetails, "UNSUPPORTED_CAPABILITY");
            branch!(ConnectionDetails, "AUTH_REQUIRED", "CONNECTION_CHANGED", "TASK_FAILED", "INVALID_PROPOSAL");
            branch!(PermissionDetails, "PERMISSION_DENIED");
            branch!(ModelErrorDetails, "MODEL_ERROR", "MALFORMED_RESPONSE");
            branch!(StaleDetails, "STALE_DOCUMENT");
            branch!(CandidateDetails, "DEPENDENCY_CHANGED", "INVALID_CANDIDATE");
            branch!(ProfileDetails, "PROFILE_UNSUPPORTED", "REPORT_MISMATCH");
            branch!(ReferenceDetails, "NOT_READY", "NOT_FOUND");
            branch!(SubmissionDetails, "IDEMPOTENCY_CONFLICT", "REQUEST_EXPIRED");
            branch!(ResourceDetails, "RESOURCE_EXHAUSTED");
            branch!(OperationDetails, "TIMEOUT", "CANCELLED", "TRANSPORT_ERROR", "OUTCOME_UNKNOWN", "INTERNAL_ERROR");
            s["allOf"] = json!(conditions);
            s.try_into().unwrap()
        }
    }
    impl JsonSchema for AgentProgress {
        fn schema_name() -> Cow<'static, str> {
            "AgentProgress".into()
        }
        fn json_schema(g: &mut SchemaGenerator) -> Schema {
            let mut variants = Vec::new();
            macro_rules! progress { ($ty:ty,$kind:literal) => {
                variants.push(object(json!({"agentSessionId":g.subschema_for::<Id>(),"kind":{"const":$kind},"content":g.subschema_for::<$ty>()}), &["agentSessionId","kind","content"]));
            }; }
            progress!(ProgressText, "text");
            progress!(ToolProgress, "tool");
            progress!(WorkflowPlan, "plan");
            progress!(PermissionProgress, "permission");
            json!({"oneOf":variants}).try_into().unwrap()
        }
    }
    fn nullable_without_null(s: &mut Value) {
        if let Some(list) = s.get_mut("type").and_then(Value::as_array_mut) {
            list.retain(|v| v != "null");
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(list) = s.get_mut(key).and_then(Value::as_array_mut) {
                list.retain(|v| v.get("type") != Some(&json!("null")));
            }
        }
    }
    // Refinements supplement serde derivation, never change the wire acceptance implementation.
    fn refinements(defs: &mut Value) {
        fn integer_caps(s: &mut Value) {
            if s.get("type") == Some(&json!("integer")) || s.get("type").and_then(Value::as_array).is_some_and(|a| a.contains(&json!("integer"))) {
                let cap = s.get("maximum").and_then(Value::as_u64).unwrap_or(u64::MAX).min(2_147_483_647);
                s["maximum"] = json!(cap);
            }
            match s {
                Value::Object(m) => {
                    for child in m.values_mut() {
                        integer_caps(child);
                    }
                }
                Value::Array(a) => {
                    for child in a {
                        integer_caps(child);
                    }
                }
                _ => (),
            }
        }
        integer_caps(defs);
        for name in ["FieldMethodDetails", "CandidateDetails", "ProfileDetails", "ReferenceDetails", "OperationDetails", "ModelErrorDetails"] {
            for prop in defs[name]["properties"].as_object_mut().unwrap().values_mut() {
                nullable_without_null(prop);
            }
        }
        defs["Limits"]["properties"]["storeBytes"] = serde_json::to_value(string_pattern(&decimal_pattern(8_589_934_592))).unwrap();
        defs["Limits"]["properties"]["storeBytes"]["not"] = json!({"const":"0"});
        for (key, cap) in
            [("modelCalls", 20), ("agentTasks", 4), ("cadCommands", 64), ("repairs", 2), ("validationRuns", 3), ("wallTimeSeconds", 1200)]
        {
            defs["Budgets"]["properties"][key]["maximum"] = json!(cap);
        }
        defs["RuntimeHelloResult"]["properties"]["selectedVersion"] = json!({"const":"1.0"});
        for name in ["ModelOptions", "AgentRunParams"] {
            defs[name]["properties"]["timeoutSeconds"]["minimum"] = json!(1);
        }
        for (field, constraint) in [
            ("maxOutputTokens", json!({"type":"integer","minimum":1,"maximum":131072})),
            ("temperature", json!({"type":"number","minimum":0,"maximum":2})),
            ("reasoningEffort", json!({"type":"string","maxLength":64})),
        ] {
            defs["ModelOptions"]["properties"][field] = json!({"anyOf":[constraint,{"type":"null"}]});
        }
        for (name, field, cap) in [
            ("ModelGenerateParams", "messages", 128),
            ("ModelGenerateParams", "tools", 64),
            ("ModelMessage", "toolCalls", 32),
            ("ModelGenerateResult", "toolCalls", 32),
            ("WorkflowPlan", "steps", 64),
            ("SnapshotManifest", "inputs", 256),
            ("CheckProfile", "checks", 128),
            ("ValidationReport", "checks", 128),
            ("CheckResult", "evidence", 64),
            ("ParameterRequirements", "requirements", 64),
            ("GeometryMeasurements", "measurements", 64),
        ] {
            defs[name]["properties"][field]["maxItems"] = json!(cap);
        }
        for (name, field) in [("CheckProfile", "checks"), ("ParameterRequirements", "requirements"), ("GeometryMeasurements", "measurements")] {
            defs[name]["properties"][field]["minItems"] = json!(1);
        }
        // JSON Schema counts characters, not UTF-8 bytes. Byte caps remain a separate decoder layer.
        defs["ProgressText"]["properties"]["text"]["maxLength"] = json!(262144);
        for name in ["WorkflowText", "ModelDelta"] {
            defs[name]["properties"]["text"]["maxLength"] = json!(262144);
        }
        for variant in defs["ContentBlock"]["oneOf"].as_array_mut().unwrap() {
            if variant["properties"]["type"]["const"] == "text" {
                variant["properties"]["text"]["maxLength"] = json!(262144);
            } else {
                let media =
                    if variant["properties"]["type"]["const"] == "image" { json!(["image/png", "image/jpeg"]) } else { json!(["application/pdf"]) };
                variant["properties"]["artifact"] =
                    json!({"allOf":[reference("ArtifactRef"),{"properties":{"mediaType":{"enum":media},"sizeBytes":{"maximum":4194304}}}]});
            }
        }
        // Individual message roles have shape conditions; cross-message identity is semantic.
        defs["ModelMessage"]["allOf"] = json!([
            {"if":{"properties":{"role":{"const":"tool"}}},"then":{"properties":{"toolCallId":reference("Id"),"toolCalls":{"maxItems":0}}},"else":{"properties":{"toolCallId":{"type":"null"}}}},
            {"if":{"properties":{"role":{"not":{"const":"assistant"}}}},"then":{"properties":{"toolCalls":{"maxItems":0}}}}
        ]);
        for name in ["ModelGenerateResult", "AgentRunResult"] {
            defs[name]["properties"]["content"]["items"] = json!({"allOf":[reference("ContentBlock"),{"properties":{"type":{"const":"text"}}}]});
        }
    }

    pub fn schema() -> Value {
        let mut g = schemars::generate::SchemaSettings::draft2020_12().into_generator();
        macro_rules! types { ($($t:ty),+ $(,)?) => { $(let _ = g.subschema_for::<$t>();)+ }; }
        types!(
            Scope,
            DocumentStamp,
            Limits,
            Budgets,
            WireError,
            RuntimeHelloParams,
            RuntimeHelloResult,
            SessionStartParams,
            SessionStartResult,
            TurnStartParams,
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
            ModelGenerateParams,
            ModelGenerateResult,
            AgentRunParams,
            AgentRunResult,
            ValidationSubmitParams,
            ValidationSubmitResult,
            ValidationStatusParams,
            ValidationStatusResult,
            ValidationReportParams,
            ValidationReportResult,
            ValidationCancelParams,
            ValidationCancelResult,
            RuntimeReady,
            ConnectionStateEvent,
            WorkflowStage,
            WorkflowText,
            WorkflowPlan,
            WorkflowTool,
            AgentProgress,
            ModelDelta,
            TurnFinished,
            SnapshotManifest,
            CheckProfile,
            WriteOutcome,
            ParameterRequirements,
            GeometryMeasurements
        );
        let mut defs = Value::Object(g.definitions().clone());
        refinements(&mut defs);
        let mut requests = Vec::new();
        let mut results = Vec::new();
        let mut event_variants = Vec::new();
        for (method, params, result) in method_types() {
            let mut p = json!({"protocolVersion":{"const":"1.0"},"kind":{"const":"request"},"scope":scope_schema(method),"requestId":reference("RequestId"),"extensions":{"type":"object"},"method":{"const":method},"params":reference(params)});
            requests.push(object(p.take(), &["protocolVersion", "kind", "scope", "requestId", "method", "params"]));
            results.push(reference(result));
        }
        for (event, data) in event_types() {
            event_variants.push(object(json!({"protocolVersion":{"const":"1.0"},"kind":{"const":"event"},"scope":scope_schema(event),"extensions":{"type":"object"},"requestId":{"anyOf":[reference("Id"),{"type":"null"}]},"sequence":{"type":"integer","minimum":1,"maximum":2147483647},"event":{"const":event},"data":reference(data)}), &["protocolVersion","kind","scope","sequence","event","requestId","data"]));
        }
        let ok = object(
            json!({"protocolVersion":{"const":"1.0"},"kind":{"const":"response"},"scope":reference("Scope"),"extensions":{"type":"object"},"requestId":reference("RequestId"),"outcome":{"const":"ok"},"result":{"anyOf":results}}),
            &["protocolVersion", "kind", "scope", "requestId", "outcome", "result"],
        );
        let error = object(
            json!({"protocolVersion":{"const":"1.0"},"kind":{"const":"response"},"scope":reference("Scope"),"extensions":{"type":"object"},"requestId":reference("RequestId"),"outcome":{"const":"error"},"error":reference("WireError")}),
            &["protocolVersion", "kind", "scope", "requestId", "outcome", "error"],
        );
        defs["Request"] = json!({"anyOf":requests});
        defs["Response"] = json!({"anyOf":[ok,error]}); // Cancellation successes overlap: NOT oneOf.
        defs["Event"] = json!({"oneOf":event_variants});
        defs["Frame"] = json!({"oneOf":[reference("Request"),reference("Response"),reference("Event")]});
        let mut bootstrap = defs["Request"]["anyOf"][0].clone();
        bootstrap["properties"]["protocolVersion"] = json!({"type":"string"});
        let mut refusal = defs["Response"]["anyOf"][1].clone();
        refusal["properties"]["protocolVersion"] = json!({"type":"string"});
        refusal["properties"]["scope"] = scope_schema("runtime.hello");
        refusal["properties"]["error"] = json!({"allOf":[reference("WireError"),{"properties":{"code":{"const":"INCOMPATIBLE_VERSION"}}}]});
        defs["BootstrapFrame"] = json!({"anyOf":[bootstrap,defs["Response"]["anyOf"][0],refusal]});
        json!({"$schema":"https://json-schema.org/draft/2020-12/schema","$id":"urn:solidcraft:mastra-runtime:1.0","title":"SolidCraft Mastra protocol 1.0","$ref":"#/$defs/Frame","$defs":defs})
    }
    fn scope_schema(method: &str) -> Value {
        let branch = match method {
            "runtime.hello" | "runtime.shutdown" | "runtime.ready" => Some(0),
            "session.start" | "session.reset" => Some(1),
            "request.cancel" => None,
            "connection.describe" | "validation.status" | "validation.report" | "validation.cancel" | "connection.state" => {
                return json!({"anyOf":[{"$ref":"#/$defs/Scope/anyOf/1"},{"$ref":"#/$defs/Scope/anyOf/2"}]});
            }
            _ => Some(2),
        };
        branch.map_or_else(|| reference("Scope"), |i| json!({"$ref":format!("#/$defs/Scope/anyOf/{i}")}))
    }
    fn method_types() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            ("runtime.hello", "RuntimeHelloParams", "RuntimeHelloResult"),
            ("session.start", "SessionStartParams", "SessionStartResult"),
            ("turn.start", "TurnStartParams", "TurnStartResult"),
            ("turn.cancel", "TurnCancelParams", "TurnCancelResult"),
            ("request.cancel", "RequestCancelParams", "RequestCancelResult"),
            ("session.reset", "SessionResetParams", "SessionResetResult"),
            ("runtime.shutdown", "RuntimeShutdownParams", "RuntimeShutdownResult"),
            ("connection.describe", "ConnectionDescribeParams", "ConnectionDescribeResult"),
            ("model.generate", "ModelGenerateParams", "ModelGenerateResult"),
            ("agent.run", "AgentRunParams", "AgentRunResult"),
            ("validation.submit", "ValidationSubmitParams", "ValidationSubmitResult"),
            ("validation.status", "ValidationStatusParams", "ValidationStatusResult"),
            ("validation.report", "ValidationReportParams", "ValidationReportResult"),
            ("validation.cancel", "ValidationCancelParams", "ValidationCancelResult"),
        ]
    }
    fn event_types() -> Vec<(&'static str, &'static str)> {
        vec![
            ("runtime.ready", "RuntimeReady"),
            ("connection.state", "ConnectionStateEvent"),
            ("workflow.stage", "WorkflowStage"),
            ("workflow.text", "WorkflowText"),
            ("workflow.plan", "WorkflowPlan"),
            ("workflow.tool", "WorkflowTool"),
            ("agent.progress", "AgentProgress"),
            ("model.delta", "ModelDelta"),
            ("turn.finished", "TurnFinished"),
        ]
    }
    fn local_refs(s: &Value, root: &Value) -> Check {
        match s {
            Value::Object(map) => {
                if let Some(r) = map.get("$ref") {
                    let r = r.as_str().ok_or("non-string ref")?;
                    require(r.starts_with("#/") && root.pointer(&r[1..]).is_some(), format!("non-local/unresolved ref {r}"))?;
                }
                for child in map.values() {
                    local_refs(child, root)?;
                }
            }
            Value::Array(a) => {
                for child in a {
                    local_refs(child, root)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    pub struct Evaluator {
        root: Value,
        validators: HashMap<String, jsonschema::Validator>,
    }
    impl Evaluator {
        pub fn new(path: &Path) -> Check<Self> {
            let root = value(path, "protocol.schema.json")?;
            local_refs(&root, &root)?;
            require(root["$schema"] == "https://json-schema.org/draft/2020-12/schema", "not Draft2020-12")?;
            Ok(Self { root, validators: HashMap::new() })
        }
        pub fn accepts(&mut self, schema_ref: &str, value: &Value) -> Check<bool> {
            require(schema_ref.starts_with("#/$defs/") && self.root.pointer(&schema_ref[1..]).is_some(), "unknown/nonlocal schemaRef")?;
            if !self.validators.contains_key(schema_ref) {
                let mut schema = self.root.clone();
                schema["$ref"] = json!(schema_ref);
                let v = jsonschema::options()
                    .with_draft(jsonschema::Draft::Draft202012)
                    .should_validate_formats(true)
                    .offline()
                    .build(&schema)
                    .map_err(|e| e.to_string())?;
                self.validators.insert(schema_ref.to_owned(), v);
            }
            Ok(self.validators[schema_ref].is_valid(value))
        }
    }
    pub fn schema_agreement() -> Check {
        let generated = schema();
        require(read(&mastra_bundle_path(), "protocol.schema.json")? == bytes(&generated), "stale schema vs actual Rust DTO metadata")?;
        local_refs(&generated, &generated)?;
        let cancel = json!({"protocolVersion":"1.0","kind":"response","scope":mastra_codec_scope("turn.cancel"),"requestId":"h:cancel","outcome":"ok","result":{"state":"cancelling"}});
        require(super::mastra_schema_accepts("#/$defs/Frame", &cancel)?, "overlapping cancellation rejected")?;
        require(decode_runtime_reply("turn.cancel", cancel["result"].clone(), &Limits::default()).is_ok(), "turn cancellation shape")?;
        require(decode_runtime_reply("request.cancel", cancel["result"].clone(), &Limits::default()).is_ok(), "request cancellation shape")?;
        require(
            decode_runtime_reply("session.start", cancel["result"].clone(), &Limits::default()).is_err(),
            "method-neutral shape bypassed method-specific decoder",
        )?;
        Ok(())
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Case {
        id: String,
        path: String,
        kind: String,
        schema_ref: Option<String>,
        phase: String,
        sender: String,
        shape_expected: String,
        handling_expected: String,
        error_code: Option<String>,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Catalog {
        schema_version: String,
        cases: Vec<Case>,
    }
    #[derive(Default)]
    struct Plan {
        files: BTreeMap<String, Vec<u8>>,
        cases: BTreeMap<String, Case>,
    }
    impl Plan {
        fn raw(
            &mut self,
            id: &str,
            path: &str,
            raw: Vec<u8>,
            kind: &str,
            schema: Option<&str>,
            shape: &str,
            handling: &str,
            code: Option<&str>,
            host: bool,
        ) {
            assert!(self.files.insert(path.to_owned(), raw).is_none(), "duplicate fixture {path}");
            assert!(
                self.cases
                    .insert(
                        id.to_owned(),
                        Case {
                            id: id.to_owned(),
                            path: path.to_owned(),
                            kind: kind.to_owned(),
                            schema_ref: schema.map(|s| format!("#/$defs/{s}")),
                            phase: if kind == "frame" || kind == "bytes" { "negotiated" } else { "not_applicable" }.to_owned(),
                            sender: if host { "host" } else { "runtime" }.to_owned(),
                            shape_expected: shape.to_owned(),
                            handling_expected: handling.to_owned(),
                            error_code: code.map(str::to_owned)
                        }
                    )
                    .is_none()
            );
        }
        fn frame(&mut self, id: &str, value: Value, shape: bool, handling: &str, host: bool) {
            let path = format!("fixtures/{}/{id}.ndjson", if handling == "accept" { "valid" } else { "invalid" });
            let code = match handling {
                "invalid_request" => Some("INVALID_REQUEST"),
                "method_not_found" => Some("METHOD_NOT_FOUND"),
                "transport_error" => Some("TRANSPORT_ERROR"),
                _ => None,
            };
            self.raw(id, &path, line(&value), "frame", Some("Frame"), if shape { "valid" } else { "invalid" }, handling, code, host);
        }
        fn data(&mut self, id: &str, value: Value, schema: &str, shape: bool, valid: bool) {
            let path = format!("fixtures/{}/{id}.json", if valid { "valid" } else { "invalid" });
            self.raw(
                id,
                &path,
                bytes(&value),
                "data",
                Some(schema),
                if shape { "valid" } else { "invalid" },
                if valid { "accept" } else { "invalid_request" },
                if valid { None } else { Some("INVALID_REQUEST") },
                true,
            );
        }
        fn catalog(&self) -> Value {
            serde_json::to_value(Catalog { schema_version: "1.0".to_owned(), cases: self.cases.values().cloned().collect() }).unwrap()
        }
    }
    fn missing_nulls(value: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if child.is_null() {
                        let mut v = value.clone();
                        v.as_object_mut().unwrap().remove(key);
                        out.push(v);
                    }
                    for replacement in missing_nulls(child) {
                        let mut v = value.clone();
                        v[key] = replacement;
                        out.push(v);
                    }
                }
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    for replacement in missing_nulls(child) {
                        let mut v = value.clone();
                        v[i] = replacement;
                        out.push(v);
                    }
                }
            }
            _ => (),
        }
        out
    }
    fn error_details(code: &str) -> Value {
        match code {
            "INCOMPATIBLE_VERSION" => json!({"supported":["1.0"]}),
            "INVALID_REQUEST" | "METHOD_NOT_FOUND" => json!({"field":"params","method":"model.generate"}),
            "UNSUPPORTED_CAPABILITY" => json!({"capability":"tools"}),
            "AUTH_REQUIRED" | "CONNECTION_CHANGED" | "TASK_FAILED" | "INVALID_PROPOSAL" => json!({"connectionId":"c"}),
            "MODEL_ERROR" | "MALFORMED_RESPONSE" => json!({"connectionId":"c","httpStatus":429}),
            "PERMISSION_DENIED" => json!({"permissionId":"permission"}),
            "STALE_DOCUMENT" => json!({"expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":null}),
            "DEPENDENCY_CHANGED" | "INVALID_CANDIDATE" => json!({"candidateId":"candidate","dependencyId":"dependency"}),
            "PROFILE_UNSUPPORTED" | "REPORT_MISMATCH" => json!({"runId":"run","profileId":"profile"}),
            "NOT_READY" | "NOT_FOUND" => json!({"runId":"run","artifactId":"artifact"}),
            "IDEMPOTENCY_CONFLICT" | "REQUEST_EXPIRED" => json!({"submissionRequestId":"submission"}),
            "RESOURCE_EXHAUSTED" => json!({"resource":"pendingRequests","limit":32}),
            _ => json!({"operation":"request.cancel"}),
        }
    }
    fn error_value(code: &str, details: Value) -> Value {
        json!({"code":code,"message":"Redacted diagnostic.","retry":"never","details":details})
    }
    fn error_codes() -> [&'static str; 26] {
        [
            "INCOMPATIBLE_VERSION",
            "INVALID_REQUEST",
            "METHOD_NOT_FOUND",
            "UNSUPPORTED_CAPABILITY",
            "AUTH_REQUIRED",
            "CONNECTION_CHANGED",
            "PERMISSION_DENIED",
            "MODEL_ERROR",
            "MALFORMED_RESPONSE",
            "TASK_FAILED",
            "INVALID_PROPOSAL",
            "STALE_DOCUMENT",
            "DEPENDENCY_CHANGED",
            "INVALID_CANDIDATE",
            "PROFILE_UNSUPPORTED",
            "REPORT_MISMATCH",
            "NOT_READY",
            "NOT_FOUND",
            "IDEMPOTENCY_CONFLICT",
            "REQUEST_EXPIRED",
            "RESOURCE_EXHAUSTED",
            "TIMEOUT",
            "CANCELLED",
            "TRANSPORT_ERROR",
            "OUTCOME_UNKNOWN",
            "INTERNAL_ERROR",
        ]
    }
    fn plan() -> Plan {
        let mut p = Plan::default();
        let (methods, events) = mastra_codec_catalog();
        for (method, params, result, host) in &methods {
            let base = mastra_codec_request(method, params.clone(), *host);
            p.frame(&format!("request-{method}"), base.clone(), true, "accept", *host);
            let reply =
                json!({"protocolVersion":"1.0","kind":"response","scope":base["scope"],"requestId":base["requestId"],"outcome":"ok","result":result});
            p.frame(&format!("response-{method}"), reply.clone(), true, "accept", !*host);
            p.data(&format!("result-{method}"), result.clone(), method_types().iter().find(|x| x.0 == method).unwrap().2, true, true);
            let err = json!({"protocolVersion":"1.0","kind":"response","scope":base["scope"],"requestId":base["requestId"],"outcome":"error","error":error_value("INVALID_REQUEST",json!({}))});
            p.frame(&format!("response-error-{method}"), err, true, "accept", !*host);
            let mut unknown = base.clone();
            unknown["params"]["undeclared"] = json!(true);
            p.frame(&format!("unknown-param-{method}"), unknown, false, "invalid_request", *host);
            for field in params.as_object().unwrap().keys() {
                let mut v = base.clone();
                v["params"].as_object_mut().unwrap().remove(field);
                p.frame(&format!("missing-param-{method}-{field}"), v, false, "invalid_request", *host);
            }
            for (i, v) in missing_nulls(&base).into_iter().enumerate() {
                // Missing scope identity is an envelope violation, not a method error.
                let scope_ok = serde_json::from_value::<Scope>(v["scope"].clone()).is_ok();
                p.frame(
                    &format!("missing-null-request-{method}-{i:02}"),
                    v,
                    false,
                    if scope_ok { "invalid_request" } else { "transport_error" },
                    *host,
                );
            }
            let result_schema = method_types().iter().find(|x| x.0 == method).unwrap().2;
            for field in result.as_object().unwrap().keys() {
                let mut v = result.clone();
                v.as_object_mut().unwrap().remove(field);
                p.data(&format!("missing-result-{method}-{field}"), v, result_schema, false, false);
            }
            let mut v = result.clone();
            v["undeclared"] = json!(true);
            p.data(&format!("unknown-result-{method}"), v, result_schema, false, false);
            for (i, v) in missing_nulls(result).into_iter().enumerate() {
                p.data(&format!("missing-null-result-{method}-{i:02}"), v, result_schema, false, false);
            }
            let mut unknown = base.clone();
            unknown["method"] = json!("future.method");
            p.frame(&format!("unknown-method-{method}"), unknown, false, "method_not_found", *host);
            if method != "request.cancel" {
                p.frame(&format!("wrong-direction-{method}"), base, true, "transport_error", !*host);
            }
        }
        for (event, data, host) in events {
            let base = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope(&event),"sequence":1,"event":event,"requestId":null,"data":data});
            p.frame(&format!("event-{event}"), base.clone(), true, "accept", host);
            p.frame(&format!("event-direction-{event}"), base.clone(), true, "transport_error", !host);
            for field in data.as_object().unwrap().keys() {
                let mut v = base.clone();
                v["data"].as_object_mut().unwrap().remove(field);
                p.frame(&format!("missing-event-{event}-{field}"), v, false, "transport_error", host);
            }
            for (i, v) in missing_nulls(&base).into_iter().enumerate() {
                p.frame(&format!("missing-null-event-{event}-{i:02}"), v, false, "transport_error", host);
            }
            let mut v = base.clone();
            v["data"]["undeclared"] = json!(0);
            p.frame(&format!("unknown-event-data-{event}"), v, false, "transport_error", host);
            let mut v = base;
            v["event"] = json!("future.event");
            p.frame(&format!("unknown-event-{event}"), v, false, "transport_error", host);
        }
        for code in error_codes() {
            for (branch, details) in [("empty", json!({})), ("nonempty", error_details(code))] {
                let e = error_value(code, details);
                p.data(&format!("error-{code}-{branch}"), e.clone(), "WireError", true, true);
                if branch == "nonempty" {
                    for key in e["details"].as_object().unwrap().keys() {
                        let mut v = e.clone();
                        v["details"][key] = Value::Null;
                        if !e["details"][key].is_null() {
                            p.data(&format!("error-{code}-null-{key}"), v, "WireError", false, false);
                        }
                        let mut v = e.clone();
                        v["details"].as_object_mut().unwrap().remove(key);
                        let valid = matches!(
                            code,
                            "INVALID_REQUEST"
                                | "METHOD_NOT_FOUND"
                                | "DEPENDENCY_CHANGED"
                                | "INVALID_CANDIDATE"
                                | "PROFILE_UNSUPPORTED"
                                | "REPORT_MISMATCH"
                                | "NOT_READY"
                                | "NOT_FOUND"
                                | "TIMEOUT"
                                | "CANCELLED"
                                | "TRANSPORT_ERROR"
                                | "OUTCOME_UNKNOWN"
                                | "INTERNAL_ERROR"
                        ) || (matches!(code, "MODEL_ERROR" | "MALFORMED_RESPONSE") && key == "httpStatus");
                        let empty = v["details"].as_object().unwrap().is_empty();
                        p.data(&format!("error-{code}-omit-{key}"), v, "WireError", valid || empty, valid || empty);
                    }
                    if code == "STALE_DOCUMENT" {
                        let mut v = e.clone();
                        v["details"]["current"] = json!({"documentId":"doc-A","revision":"18446744073709551615"});
                        p.data("error-STALE_DOCUMENT-current-stamp", v, "WireError", true, true);
                    }
                }
                let mut bad = e.clone();
                bad["details"]["undeclared"] = json!(true);
                p.data(&format!("error-{code}-{branch}-unknown"), bad, "WireError", false, false);
                for key in ["code", "message", "retry", "details"] {
                    let mut bad = e.clone();
                    bad.as_object_mut().unwrap().remove(key);
                    p.data(&format!("error-{code}-{branch}-missing-{key}"), bad, "WireError", false, false);
                }
            }
        }
        p.data("error-future-code", error_value("FUTURE", json!({})), "WireError", false, false);
        for (id, raw, valid) in [
            ("document-stamp-zero", r#"{"documentId":"doc-A","revision":"0"}"#, true),
            ("document-stamp-unsafe-js", r#"{"documentId":"doc-A","revision":"9007199254740993"}"#, true),
            ("document-stamp-max-u64", r#"{"documentId":"doc-A","revision":"18446744073709551615"}"#, true),
            ("document-stamp-numeric", r#"{"documentId":"doc-A","revision":0}"#, false),
            ("document-stamp-leading-zero", r#"{"documentId":"doc-A","revision":"00"}"#, false),
            ("document-stamp-overflow", r#"{"documentId":"doc-A","revision":"18446744073709551616"}"#, false),
        ] {
            p.raw(
                id,
                &format!("fixtures/{}/{id}.json", if valid { "valid" } else { "invalid" }),
                raw.as_bytes().to_vec(),
                "data",
                Some("DocumentStamp"),
                if valid { "valid" } else { "invalid" },
                if valid { "accept" } else { "invalid_request" },
                if valid { None } else { Some("INVALID_REQUEST") },
                true,
            );
        }
        for (i, n) in [0, 9_007_199_254_740_993, u64::MAX].into_iter().enumerate() {
            let raw = format!(r#"{{"document_id":"doc-A","revision":{n}}}"#);
            p.raw(
                &format!("native-stamp-{i}"),
                &format!("fixtures/valid/native-stamp-{i}.json"),
                raw.into_bytes(),
                "data",
                None,
                "not_applicable",
                "accept",
                None,
                true,
            );
        }
        for (id, v) in [("id-newline", json!("a\n")), ("id-unicode", json!("é")), ("id-overlength", json!("x".repeat(129)))] {
            p.data(id, v, "Id", false, false);
        }
        p.data("id-max-length", json!("x".repeat(128)), "Id", true, true);
        // `$` alone can match before a final LF. These exercise the actual published
        // schema/evaluator and actual primitive parsers, not a regex-description check.
        for (name, good) in [
            ("Id", "a".to_owned()),
            ("RequestId", "h:call".to_owned()),
            ("Revision", "1".to_owned()),
            ("Digest", format!("sha256:{}", "a".repeat(64))),
            ("Timestamp", "2026-11-05T12:00:00.000Z".to_owned()),
            ("SubmissionKey", "1793876400000:12345678-1234-4234-8234-123456789abc".to_owned()),
        ] {
            p.data(&format!("whole-string-{name}-valid"), json!(good), name, true, true);
            for (suffix, label) in [("\n", "lf"), ("\r", "cr"), (" ", "space"), ("é", "non-ascii")] {
                p.data(&format!("whole-string-{name}-{label}"), json!(format!("{good}{suffix}")), name, false, false);
            }
        }
        for (label, revision) in [("leading-zero", "01"), ("non-ascii", "１"), ("overflow", "18446744073709551616")] {
            p.data(&format!("whole-string-Revision-{label}-standalone"), json!(revision), "Revision", false, false);
        }
        p.data("whole-string-document-id-lf", json!({"documentId":"a\n","revision":"1"}), "DocumentStamp", false, false);
        p.data("whole-string-scope-id-lf", json!({"sessionId":"a\n","turnId":null,"workflowRunId":null}), "Scope", false, false);
        let mut bad_id = mastra_codec_request("session.start", json!({"workflowId":"w","instructions":"free text","bridge":null}), true);
        bad_id["requestId"] = json!("h:call\n");
        p.frame("whole-string-envelope-id-lf", bad_id, false, "transport_error", true);
        let mut bad_scope = mastra_codec_request("session.start", json!({"workflowId":"w","instructions":"free text","bridge":null}), true);
        bad_scope["scope"]["sessionId"] = json!("a\n");
        p.frame("whole-string-envelope-scope-lf", bad_scope, false, "transport_error", true);
        p.frame(
            "whole-string-parameter-id-lf",
            mastra_codec_request("session.start", json!({"workflowId":"a\n","instructions":"free text","bridge":null}), true),
            false,
            "invalid_request",
            true,
        );
        p.frame("whole-string-free-text-lf",json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope("workflow.text"),"sequence":1,"requestId":null,"event":"workflow.text","data":{"text":"a\n","channel":"answer"}}),true,"accept",false);
        p.data("revision-numeric-unsafe", json!(9_007_199_254_740_993_u64), "Revision", false, false);
        p.data("revision-numeric-max", json!(u64::MAX), "Revision", false, false);
        let control = mastra_codec_request("turn.cancel", json!({"reason":"user"}), true);
        for field in ["protocolVersion", "kind", "scope", "requestId", "method", "params"] {
            let mut v = control.clone();
            v.as_object_mut().unwrap().remove(field);
            p.frame(&format!("envelope-missing-{field}"), v, false, "transport_error", true);
        }
        for (field, v) in [
            ("protocolVersion", json!("1.0.0")),
            ("extra", json!(0)),
            ("extensions", Value::Null),
            ("requestId", json!("r:wrong")),
            ("scope", json!({"sessionId":"s","turnId":"t","workflowRunId":null})),
            ("params", Value::Null),
        ] {
            let mut bad = control.clone();
            bad[field] = v;
            // Direction prefix is checked by codec; neutral schema cannot infer sender.
            p.frame(&format!("envelope-invalid-{field}"), bad, field == "requestId", "transport_error", true);
        }
        let raw = String::from_utf8(line(&control)).unwrap();
        for (id, raw) in [
            ("bytes-invalid-utf8", vec![0xff, b'\n']),
            ("bytes-duplicate-root", raw.replacen('{', "{\"kind\":\"request\",", 1).into_bytes()),
            ("bytes-duplicate-nested", raw.replace("\"params\":{", "\"params\":{\"reason\":\"user\",").into_bytes()),
            ("bytes-duplicate-escaped", raw.replacen('{', "{\"extensions\":{\"x\":1,\"\\u0078\":2},", 1).into_bytes()),
            ("bytes-no-lf", raw.trim_end_matches('\n').as_bytes().to_vec()),
            ("bytes-crlf", raw.replace('\n', "\r\n").into_bytes()),
            ("bytes-bom", format!("\u{feff}{raw}").into_bytes()),
            ("bytes-multiple-lines", format!("{raw}{raw}").into_bytes()),
            ("bytes-infinite", raw.replacen('{', "{\"extensions\":{\"n\":1e400},", 1).into_bytes()),
        ] {
            let parseable = matches!(id, "bytes-no-lf" | "bytes-crlf");
            p.raw(
                id,
                &format!("fixtures/invalid/{id}.bin"),
                raw,
                "bytes",
                if parseable { Some("Frame") } else { None },
                if parseable { "valid" } else { "not_applicable" },
                "transport_error",
                Some("TRANSPORT_ERROR"),
                true,
            );
        }
        for (depth, valid) in [(64, true), (65, false)] {
            let mut nested = json!(0);
            for _ in 2..depth {
                nested = json!([nested]);
            }
            let mut v = control.clone();
            v["extensions"] = json!({"value":nested});
            p.frame(&format!("depth-{depth}"), v, true, if valid { "accept" } else { "transport_error" }, true);
        }
        for (size, valid) in [(8_388_608, true), (8_388_609, false)] {
            let mut v = control.clone();
            v["extensions"] = json!({"padding":""});
            let overhead = line(&v).len();
            v["extensions"]["padding"] = json!("x".repeat(size - overhead));
            p.frame(&format!("frame-bytes-{size}"), v, true, if valid { "accept" } else { "transport_error" }, true);
        }
        let model = methods.iter().find(|x| x.0 == "model.generate").unwrap().1.clone();
        for (id, path, v, shape, valid) in [
            ("timeout-zero", "/options/timeoutSeconds", json!(0), false, false),
            ("integer-overflow", "/options/timeoutSeconds", json!(2147483648_u64), false, false),
            ("integer-fraction", "/options/timeoutSeconds", json!(1.5), false, false),
            ("output-tokens-overflow", "/options/maxOutputTokens", json!(131073), false, false),
            ("temperature-overflow", "/options/temperature", json!(2.01), false, false),
            ("reasoning-ascii-overflow", "/options/reasoningEffort", json!("x".repeat(65)), false, false),
            ("reasoning-byte-overflow", "/options/reasoningEffort", json!("é".repeat(33)), true, false),
            ("reasoning-byte-boundary", "/options/reasoningEffort", json!("é".repeat(32)), true, true),
            ("text-byte-boundary", "/messages/0/content/0/text", json!("é".repeat(131072)), true, true),
            ("text-byte-overflow", "/messages/0/content/0/text", json!("é".repeat(131073)), true, false),
            ("tool-json-boundary", "/tools/0/inputSchema", json!({"s":"x".repeat(65528)}), true, true),
            ("tool-json-overflow", "/tools/0/inputSchema", json!({"s":"x".repeat(65529)}), true, false),
            ("messages-overflow", "/messages", json!(vec![model["messages"][0].clone(); 129]), false, false),
            ("tools-overflow", "/tools", json!(vec![model["tools"][0].clone(); 65]), false, false),
        ] {
            let mut params = model.clone();
            *params.pointer_mut(path).unwrap() = v;
            p.frame(id, mastra_codec_request("model.generate", params, false), shape, if valid { "accept" } else { "invalid_request" }, false);
        }
        let assistant =
            json!({"role":"assistant","content":[],"toolCalls":[{"id":"call","name":"measure","arguments":{"width":-1.25}}],"toolCallId":null});
        let tool = json!({"role":"tool","content":[{"type":"text","text":"result"}],"toolCalls":[],"toolCallId":"call"});
        for (id, messages, valid) in [
            ("tool-identity-valid", json!([assistant.clone(), tool.clone()]), true),
            ("tool-identity-orphan", json!([tool.clone()]), false),
            ("tool-identity-duplicate", json!([assistant.clone(), tool.clone(), tool.clone()]), false),
            ("tool-identity-pending", json!([assistant]), false),
        ] {
            let mut params = model.clone();
            params["messages"] = messages;
            p.frame(id, mastra_codec_request("model.generate", params, false), true, if valid { "accept" } else { "invalid_request" }, false);
        }
        for (size, shape, valid) in [(2048, true, true), (2049, false, false)] {
            let mut e = error_value("INTERNAL_ERROR", json!({}));
            e["message"] = json!("x".repeat(size));
            p.data(&format!("error-ascii-{size}"), e, "WireError", shape, valid);
        }
        let mut e = error_value("INTERNAL_ERROR", json!({}));
        e["message"] = json!("é".repeat(1025));
        p.data("error-utf8-byte-overflow", e, "WireError", true, false);
        for (key, limit) in serde_json::to_value(Limits::default()).unwrap().as_object().unwrap() {
            let mut v = serde_json::to_value(Limits::default()).unwrap();
            v[key] = if key == "storeBytes" { json!("8589934593") } else { json!(limit.as_u64().unwrap() + 1) };
            p.data(&format!("limit-overflow-{key}"), v, "Limits", false, false);
            let mut v = serde_json::to_value(Limits::default()).unwrap();
            v[key] = if key == "storeBytes" { json!("0") } else { json!(0) };
            p.data(&format!("limit-zero-{key}"), v, "Limits", false, false);
        }
        p.data("limits-default", json!(Limits::default()), "Limits", true, true);
        p.data("budgets-default", json!(Budgets::default()), "Budgets", true, true);
        p.data(
            "budgets-zero",
            json!({"modelCalls":0,"agentTasks":0,"cadCommands":0,"repairs":0,"validationRuns":0,"wallTimeSeconds":0}),
            "Budgets",
            true,
            true,
        );
        for (key, limit) in serde_json::to_value(Budgets::default()).unwrap().as_object().unwrap() {
            let mut v = serde_json::to_value(Budgets::default()).unwrap();
            v[key] = json!(limit.as_u64().unwrap() + 1);
            p.data(&format!("budget-overflow-{key}"), v, "Budgets", false, false);
        }
        let mut hello = mastra_codec_request("runtime.hello", methods[0].1.clone(), true);
        hello["protocolVersion"] = json!("future");
        p.raw(
            "bootstrap-future-offer",
            "fixtures/valid/bootstrap-future-offer.ndjson",
            line(&hello),
            "frame",
            Some("BootstrapFrame"),
            "valid",
            "accept",
            None,
            true,
        );
        p.cases.get_mut("bootstrap-future-offer").unwrap().phase = "bootstrap".to_owned();
        let refusal = json!({"protocolVersion":"future","kind":"response","scope":mastra_codec_scope("runtime.hello"),"requestId":"h:hello","outcome":"error","error":error_value("INCOMPATIBLE_VERSION",json!({"supported":["1.0"]}))});
        p.raw(
            "bootstrap-exact-refusal",
            "fixtures/valid/bootstrap-exact-refusal.ndjson",
            line(&refusal),
            "frame",
            Some("BootstrapFrame"),
            "valid",
            "accept",
            None,
            false,
        );
        p.cases.get_mut("bootstrap-exact-refusal").unwrap().phase = "bootstrap".to_owned();
        let mut caps = methods[0].2.clone();
        caps["capabilities"]["futureCapability"] = json!(true);
        p.data("unknown-capability", caps, "RuntimeHelloResult", false, false);
        for code in ["MODEL_ERROR", "MALFORMED_RESPONSE"] {
            let mut e = error_value(code, error_details(code));
            e["details"]["httpStatus"] = json!(2147483648_u64);
            p.data(&format!("error-{code}-integer-overflow"), e, "WireError", false, false);
        }
        for (kind, content) in [
            ("text", json!({"text":"progress"})),
            ("tool", json!({"callId":"call","title":"Measure","state":"running"})),
            ("plan", json!({"steps":[{"id":"step","title":"Measure","state":"pending"}]})),
        ] {
            let frame = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope("agent.progress"),"sequence":1,"requestId":null,"event":"agent.progress","data":{"agentSessionId":"as","kind":kind,"content":content}});
            p.frame(&format!("agent-progress-{kind}"), frame, true, "accept", true);
        }
        let d = json!({"documentId":"doc-A","revision":"9007199254740993"});
        let hash = format!("sha256:{}", "a".repeat(64));
        let manifest = json!({"schemaVersion":"1.0","candidateId":"candidate","source":d,"capturedAt":"2026-11-05T12:00:00.000Z","formatVersion":"solidcraft/fixture","engineVersion":"1","inputs":[{"path":"design.sc","kind":"design","dependencyId":"design","dependencyVersion":null,"digest":hash,"sizeBytes":1}]});
        p.data("snapshot-manifest-wire-only", manifest.clone(), "SnapshotManifest", true, true);
        let mut bad = manifest;
        bad["inputs"][0]["path"] = json!("../live.sc");
        p.data("snapshot-manifest-path-traversal", bad, "SnapshotManifest", true, false);
        let profile = json!({"schemaVersion":"1.0","profileId":"profile","version":"1","checks":[{"checkId":"input.integrity","kind":"input.integrity","required":true,"parameters":{}}]});
        p.data("check-profile-wire-only", profile, "CheckProfile", true, true);
        p.data("write-outcome-unknown-wire-only",json!({"state":"unknown","request":{"bridgeId":"bridge","mcpSessionId":"mcp","requestId":"request"},"before":d,"after":null,"completed":null,"rollback":{"requested":false,"attempted":false,"restored":null,"undone":null},"expectation":null,"error":null}),"WriteOutcome",true,true);
        issue8_plan(&mut p);
        transcript_plan(&mut p);
        p
    }

    fn native_data(schema: &str, v: Value) -> bool {
        macro_rules! parse {
            ($t:ty) => {
                serde_json::from_value::<$t>(v).is_ok()
            };
        }
        match schema {
            "Id" => parse!(Id),
            "Revision" => parse!(Revision),
            "RequestId" => parse!(RequestId),
            "Digest" => parse!(Digest),
            "Timestamp" => parse!(Timestamp),
            "SubmissionKey" => parse!(SubmissionKey),
            "Scope" => parse!(Scope),
            "DocumentStamp" => parse!(DocumentStamp),
            "WireError" => parse!(WireError),
            "SnapshotManifest" => serde_json::from_value::<SnapshotManifest>(v).is_ok_and(|x| x.validate(&Limits::default()).is_ok()),
            "CheckProfile" => serde_json::from_value::<CheckProfile>(v).is_ok_and(|x| x.validate(&Limits::default()).is_ok()),
            "WriteOutcome" => serde_json::from_value::<WriteOutcome>(v).is_ok_and(|x| x.validate(&Limits::default()).is_ok()),
            "Limits" => serde_json::from_value::<Limits>(v).is_ok_and(|x| validate_limits(&x).is_ok()),
            "Budgets" => serde_json::from_value::<Budgets>(v).is_ok_and(|x| validate_budgets(&x).is_ok()),
            name => {
                let Some((method, _, _)) = method_types().into_iter().find(|x| x.2 == name) else { return false };
                let host = mastra_codec_catalog().0.into_iter().find(|x| x.0 == method).unwrap().3;
                if host {
                    decode_runtime_reply(method, v, &Limits::default()).is_ok()
                } else {
                    decode_host_reply(method, v, &Limits::default()).is_ok()
                }
            }
        }
    }
    // The copied #8 proposal is represented here as original literal carriers and thin
    // transformations. No product test reads the ignored SDD or treats these as IPC frames.
    fn guard_raw(p: &mut Plan, id: &str, raw: String) {
        p.raw(id, &format!("fixtures/valid/{id}.json"), raw.into_bytes(), "data", None, "not_applicable", "accept", None, true);
    }
    fn issue8_plan(p: &mut Plan) {
        let stamps = vec![
            r#"{"documentId":"doc-A","revision":"0"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"9007199254740993"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"18446744073709551615"}"#.to_owned(),
            r#"{"documentId":"A0._:-z","revision":"0"}"#.to_owned(),
            "null".to_owned(),
            "0".to_owned(),
            "true".to_owned(),
            "[]".to_owned(),
            r#""stamp""#.to_owned(),
            "{}".to_owned(),
            r#"{"documentId":"doc-A"}"#.to_owned(),
            r#"{"revision":"0"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"0","extra":true}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":0}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":null}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":true}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":[]}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":{}}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":""}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"00"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"01"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"+1"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"-1"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"1e3"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"1.0"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"18446744073709551616"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"99999999999999999999"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"100000000000000000000"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":" 1"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"1 "}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"1\n"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"\t1"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"１"}"#.to_owned(),
            r#"{"documentId":"doc-A","revision":"١"}"#.to_owned(),
            r#"{"documentId":"","revision":"0"}"#.to_owned(),
            r#"{"documentId":"-bad","revision":"0"}"#.to_owned(),
            r#"{"documentId":"a/b","revision":"0"}"#.to_owned(),
            r#"{"documentId":"é","revision":"0"}"#.to_owned(),
            r#"{"documentId":null,"revision":"0"}"#.to_owned(),
            r#"{"documentId":1,"revision":"0"}"#.to_owned(),
            format!(r#"{{"documentId":"{}","revision":"0"}}"#, "x".repeat(128)),
            format!(r#"{{"documentId":"{}","revision":"0"}}"#, "x".repeat(129)),
        ];
        for (i, stamp) in stamps.into_iter().enumerate() {
            if [3, 40].contains(&i) {
                p.raw(
                    &format!("document-stamp-parser-{:02}", i + 1),
                    &format!("fixtures/valid/document-stamp-parser-{:02}.json", i + 1),
                    stamp.as_bytes().to_vec(),
                    "data",
                    Some("DocumentStamp"),
                    "valid",
                    "accept",
                    None,
                    true,
                );
            }
            guard_raw(p, &format!("guard-parser-{:02}", i + 1), format!(r#"{{"expectedDocument":{stamp}}}"#));
        }
        for (label, message, field) in [
            ("object", "expectedDocument must be an object", "expectedDocument"),
            ("fields", "expectedDocument requires only documentId and revision", "expectedDocument"),
            ("revision", "revision must be a canonical decimal u64 string", "expectedDocument.revision"),
            ("id", "documentId must be a valid ASCII Id of at most 128 bytes", "expectedDocument.documentId"),
        ] {
            let message = serde_json::to_string(message).unwrap();
            let field = serde_json::to_string(field).unwrap();
            let error = format!(r#"{{"code":"INVALID_REQUEST","message":{message},"retry":"never","details":{{"field":{field}}}}}"#);
            guard_raw(p, &format!("guard-parser-refusal-{label}-control"), format!(r#"{{"ok":false,"error":{message},"errorDetails":{error}}}"#));
            guard_raw(p, &format!("guard-parser-refusal-{label}-mcp"), format!(r#"{{"error":{error},"message":{message}}}"#));
        }
        let controls = [
            (
                "matching-execute",
                r#"{"id":1,"method":"engine.execute","params":{"command":"parameters.change","params":{"name":"width","expression":"20 mm"},"expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
                r#"{"id":1,"ok":true,"result":{"name":"width","expression":"20 mm","value":20.0,"display_value":20.0,"recomputed":0,"restored":0,"errors":[]},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}"#,
            ),
            (
                "matching-batch",
                r#"{"id":2,"method":"engine.batch","params":{"commands":[{"command":"parameters.change","params":{"name":"width","expression":"20 mm"}},{"command":"parameters.change","params":{"name":"width","expression":"30 mm"}}],"expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
                r#"{"id":2,"ok":true,"result":{"completed":2,"results":[{"name":"width","expression":"20 mm","value":20.0,"display_value":20.0,"recomputed":0,"restored":0,"errors":[]},{"name":"width","expression":"30 mm","value":30.0,"display_value":30.0,"recomputed":0,"restored":0,"errors":[]}],"undone":0},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}"#,
            ),
            (
                "stale",
                r#"{"id":3,"method":"engine.execute","params":{"command":"parameters.change","params":{"name":"width","expression":"20 mm"},"expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
                r#"{"id":3,"ok":false,"error":"The document changed before this write could apply.","errorDetails":{"code":"STALE_DOCUMENT","message":"The document changed before this write could apply.","retry":"never","details":{"expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":{"documentId":"doc-A","revision":"9007199254740994"}}}}"#,
            ),
            (
                "closed",
                r#"{"id":4,"method":"engine.execute","params":{"command":"parameters.list","expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
                r#"{"id":4,"ok":false,"error":"stale","errorDetails":{"code":"STALE_DOCUMENT","message":"stale","retry":"never","details":{"expected":{"documentId":"doc-A","revision":"9007199254740993"},"current":null}}}"#,
            ),
            (
                "unsupported",
                r#"{"id":5,"method":"engine.execute","params":{"command":"file.new","expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
                r#"{"id":5,"ok":false,"error":"This execution path does not support guarded requests.","errorDetails":{"code":"UNSUPPORTED_CAPABILITY","message":"This execution path does not support guarded requests.","retry":"never","details":{"capability":"file.new"}}}"#,
            ),
            (
                "primitive-wrapper",
                r#"{"id":6,"method":"engine.execute","params":{"command":"parameters.list","expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
                r#"{"id":6,"ok":true,"result":7,"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}}"#,
            ),
        ];
        for (label, req, rep) in controls {
            guard_raw(p, &format!("guard-control-{label}-request"), req.to_owned());
            guard_raw(p, &format!("guard-control-{label}-reply"), rep.to_owned());
            let request: Value = serde_json::from_str(req).unwrap();
            let response: Value = serde_json::from_str(rep).unwrap();
            let id = request["id"].as_u64().unwrap();
            let arguments = req.split_once(",\"params\":").unwrap().1.strip_suffix('}').unwrap();
            guard_raw(
                p,
                &format!("guard-mcp-{label}-request"),
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{}","arguments":{arguments}}}}}"#,
                    if label == "matching-batch" { "batch" } else { "execute" }
                ),
            );
            let body = if response["ok"] == false {
                format!(
                    r#"{{"error":{},"message":{}}}"#,
                    rep.split_once(",\"errorDetails\":").unwrap().1.strip_suffix('}').unwrap(),
                    serde_json::to_string(&response["error"]).unwrap()
                )
            } else if label == "matching-batch" {
                let parts = rep.split_once(",\"result\":").unwrap().1;
                let (result, stamp) = parts.rsplit_once(",\"documentStamp\":").unwrap();
                format!(
                    r#"{{"ok":true,{},"documentStamp":{}}}"#,
                    result.strip_prefix('{').unwrap().strip_suffix('}').unwrap(),
                    stamp.strip_suffix('}').unwrap()
                )
            } else {
                format!("{{{}}}", rep.split_once(",\"ok\":true,").unwrap().1.strip_suffix('}').unwrap())
            };
            guard_raw(p, &format!("guard-mcp-{label}-reply"), mcp_reply(id, &body, None, response["ok"] == false));
        }
        for (label, raw) in [
            (
                "original-request",
                r#"{"id":31,"method":"engine.execute","params":{"command":"parameters.list","expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}"#,
            ),
            (
                "initial-reply",
                r#"{"id":31,"ok":false,"running":true,"error":"timeout: `engine.execute` (parameters.list) is still running in the app (it only reads: nothing changes); ask `control.pending` for its answer"}"#,
            ),
            ("request", r#"{"id":32,"method":"control.pending","params":{}}"#),
            (
                "reply",
                r#"{"id":32,"ok":true,"result":{"pending":31,"method":"engine.execute","done":true,"reply":{"ok":true,"result":7,"documentStamp":{"documentId":"doc-A","revision":"18446744073709551615"}}}}"#,
            ),
        ] {
            guard_raw(p, &format!("guard-control-pending-{label}"), raw.to_owned());
        }
        for (label, raw) in [
            (
                "success-primitive-sibling",
                r#"{"id":31,"ok":true,"result":7,"documentStamp":{"documentId":"doc-A","revision":"18446744073709551615"}}"#,
            ),
            ("success-null-sibling", r#"{"ok":true,"result":null,"documentStamp":{"documentId":"doc-A","revision":"18446744073709551615"}}"#),
            ("success-array-sibling", r#"{"ok":true,"result":[1,"x"],"documentStamp":{"documentId":"doc-A","revision":"18446744073709551615"}}"#),
            ("missing-stamp", r#"{"ok":true,"result":7}"#),
            ("numeric-stamp-revision", r#"{"ok":true,"result":7,"documentStamp":{"documentId":"doc-A","revision":18446744073709551615}}"#),
            ("missing-result", r#"{"ok":true,"documentStamp":{"documentId":"doc-A","revision":"18446744073709551615"}}"#),
            ("native-command-error", r#"{"ok":false,"error":"native command failed"}"#),
        ] {
            guard_raw(p, &format!("guard-decode-{label}"), raw.to_owned());
        }
        guard_raw(
            p,
            "guard-mcp-pending-request",
            r#"{"jsonrpc":"2.0","id":107,"method":"tools/call","params":{"name":"pending","arguments":{}}}"#.to_owned(),
        );
        guard_raw(p,"guard-mcp-pending-original-request",r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"execute","arguments":{"command":"parameters.list","expect":{"bodies":0},"expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}}"#.to_owned());
        let body = r#"{"pending":"`engine.execute` (parameters.list)","done":true,"result":{"result":7,"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"}},"expectation":null,"note":"Tool-level expectation checks were not executed for this pending request; the original effect facts are retained."}"#;
        guard_raw(p, "guard-mcp-pending-reply", mcp_reply(107, body, None, false));
        guard_raw(p,"guard-mcp-expectation-failed-request",r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"execute","arguments":{"command":"parameters.change","params":{"name":"width","expression":"20 mm"},"expect":{"bodies":1},"expectedDocument":{"documentId":"doc-A","revision":"9007199254740993"}}}}"#.to_owned());
        let body = r#"{"result":{"name":"width","expression":"20 mm","value":20.0,"display_value":20.0,"recomputed":0,"restored":0,"errors":[]},"documentStamp":{"documentId":"doc-A","revision":"9007199254740994"},"expectation":{"ok":false,"measured":{"volume_mm3":0.0,"bodies":0,"bbox":null},"expected":{"bodies":1},"mismatch":["bodies: measured 0, expected 1"]}}"#;
        guard_raw(
            p,
            "guard-mcp-expectation-failed-reply",
            mcp_reply(
                8,
                body,
                Some(
                    "expectation not met: bodies: measured 0, expected 1. The completed command's result and document stamp are retained; no rollback was attempted.",
                ),
                true,
            ),
        );
    }
    fn mcp_reply(id: u64, body: &str, note: Option<&str>, is_error: bool) -> String {
        let v: Value = serde_json::from_str(body).unwrap();
        let text = serde_json::to_string(&serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let prefix = note.map(|n| format!(r#"{{"type":"text","text":{}}},"#, serde_json::to_string(n).unwrap())).unwrap_or_default();
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{prefix}{{"type":"text","text":{text}}}],"structuredContent":{body}{}}}}}"#,
            if is_error { ",\"isError\":true" } else { "" }
        )
    }
    fn guard_adapter(name: &str, v: &Value, evaluator: &mut Evaluator) -> Check {
        if let Some(index) = name.strip_prefix("guard-parser-").filter(|s| s.len() == 2) {
            let expected = matches!(index, "01" | "02" | "03" | "04" | "41");
            let stamp = &v["expectedDocument"];
            require(serde_json::from_value::<DocumentStamp>(stamp.clone()).is_ok() == expected, format!("{name}: native stamp accept"))?;
            require(evaluator.accepts("#/$defs/DocumentStamp", stamp)? == expected, format!("{name}: stamp shape accept"))?;
            return Ok(());
        }
        // Raw control/MCP envelopes are NEVER passed to decode_frame or the Frame schema.
        fn components(v: &Value, evaluator: &mut Evaluator, numeric_bad: bool) -> Check {
            match v {
                Value::Object(m) => {
                    for (key, child) in m {
                        if ["documentStamp", "expectedDocument"].contains(&key.as_str()) {
                            let ok = serde_json::from_value::<DocumentStamp>(child.clone()).is_ok();
                            require(ok != numeric_bad, "guarded stamp component acceptance")?;
                            require(evaluator.accepts("#/$defs/DocumentStamp", child)? == ok, "guarded stamp schema/native disagreement")?;
                        } else if key == "errorDetails" || (key == "error" && child.get("code").is_some()) {
                            require(
                                serde_json::from_value::<WireError>(child.clone()).is_ok() && evaluator.accepts("#/$defs/WireError", child)?,
                                "guarded structured error component",
                            )?;
                        } else {
                            components(child, evaluator, numeric_bad)?;
                        }
                    }
                }
                Value::Array(a) => {
                    for child in a {
                        components(child, evaluator, numeric_bad)?;
                    }
                }
                _ => (),
            }
            Ok(())
        }
        components(v, evaluator, name == "guard-decode-numeric-stamp-revision")?;
        if name == "guard-control-pending-reply" {
            require(
                v["id"] == 32 && v["result"]["pending"] == 31 && v["result"]["reply"].get("id").is_none() && v["result"]["reply"]["result"] == 7,
                "raw pending outer identity/nested primitive carrier changed",
            )?;
        }
        if name == "guard-control-pending-initial-reply" {
            require(
                v["running"] == true && v["error"].as_str().is_some_and(|s| s.ends_with("ask `control.pending` for its answer")),
                "pending timeout truncated",
            )?;
        }
        if name == "guard-mcp-expectation-failed-reply" {
            require(
                v["result"]["isError"] == true
                    && v["result"]["structuredContent"]["expectation"]["ok"] == false
                    && v["result"]["structuredContent"].get("documentStamp").is_some(),
                "expectation failure lost applied stamp/effect facts",
            )?;
        }
        if name == "guard-mcp-pending-reply" {
            require(
                v["id"] == 107 && v["result"]["structuredContent"]["expectation"].is_null() && v["result"]["structuredContent"]["done"] == true,
                "MCP pending facts changed",
            )?;
        }
        Ok(())
    }
    fn catalog(root: &Path) -> Check<Catalog> {
        serde_json::from_slice(&read(root, "cases.json")?).map_err(|e| e.to_string())
    }
    pub fn fixture_agreement(root: &Path) -> Check {
        let mut evaluator = Evaluator::new(root)?;
        for c in catalog(root)?.cases {
            if c.kind == "transcript" {
                continue;
            }
            let raw = read(root, &c.path)?;
            if let Some(schema) = &c.schema_ref {
                let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("{} parse: {e}", c.id))?;
                require(
                    evaluator.accepts(schema, &v)? == (c.shape_expected == "valid"),
                    format!("{}: schema acceptance mismatch ({})", c.id, c.shape_expected),
                )?;
            } else {
                require(c.shape_expected == "not_applicable", format!("{}: missing shape layer", c.id))?;
            }
            let (handling, code) = if c.kind == "data" {
                let v: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
                let ok = if c.id.starts_with("guard-") {
                    guard_adapter(&c.id, &v, &mut evaluator)?;
                    true
                } else if c.id.starts_with("native-stamp-") {
                    let native = v["revision"].as_u64().ok_or("native revision lost")?;
                    let stamp =
                        document_stamp_from_native(v["document_id"].as_str().ok_or("native document id")?, native).map_err(|e| e.to_string())?;
                    require(document_stamp_to_native(&stamp) == ("doc-A".to_owned(), native), "native stamp adapter lost identity")?;
                    let decimal = native.to_string();
                    require(serde_json::to_value(stamp).unwrap()["revision"].as_str() == Some(decimal.as_str()), "wire revision became numeric")?;
                    true
                } else {
                    native_data(c.schema_ref.as_deref().unwrap().trim_start_matches("#/$defs/"), v)
                };
                (if ok { "accept" } else { "invalid_request" }, if ok { None } else { Some("INVALID_REQUEST".to_owned()) })
            } else {
                let phase = if c.phase == "bootstrap" { DecodePhase::Bootstrap } else { DecodePhase::Negotiated };
                let sender = if c.sender == "host" { Sender::Host } else { Sender::Runtime };
                match decode_frame(&raw, phase, sender, &Limits::default()) {
                    Ok(DecodeResult::Frame(frame)) => {
                        // Accepted raw values survive the actual wire encoder and genuine JCS comparison.
                        let encoded = encode_frame(&frame, &Limits::default()).map_err(|e| e.to_string())?;
                        let a: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
                        let b: Value = serde_json::from_slice(&encoded).map_err(|e| e.to_string())?;
                        require(
                            mastra_canonical_bytes(&a)? == mastra_canonical_bytes(&b)?,
                            format!("{}: serialization changed identity/value", c.id),
                        )?;
                        ("accept", None)
                    }
                    Ok(DecodeResult::RequestError { error, .. }) => {
                        let code = serde_json::to_value(error.code).unwrap().as_str().unwrap().to_owned();
                        (if error.code == ErrorCode::MethodNotFound { "method_not_found" } else { "invalid_request" }, Some(code))
                    }
                    Err(e) => ("transport_error", Some(serde_json::to_value(e.wire_error().code).unwrap().as_str().unwrap().to_owned())),
                }
            };
            require(
                handling == c.handling_expected && code == c.error_code,
                format!("{}: handling {handling}/{code:?} != {}/{:?}", c.id, c.handling_expected, c.error_code),
            )?;
        }
        Ok(())
    }
    fn safe_path(path: &str) -> Check {
        require(
            !path.is_empty() && !path.contains(['\\', '\0', ':']) && path.split('/').all(|s| !s.is_empty() && s != "." && s != ".."),
            format!("unsafe bundle path {path:?}"),
        )
    }
    fn inventory(root: &Path) -> Check<BTreeMap<String, Vec<u8>>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Check {
            require(!std::fs::symlink_metadata(dir).map_err(|e| e.to_string())?.file_type().is_symlink(), "symlink directory")?;
            for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
                let path = entry.map_err(|e| e.to_string())?.path();
                let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                require(!meta.file_type().is_symlink(), "symlink artifact")?;
                if meta.is_dir() {
                    walk(root, &path, out)?;
                } else {
                    require(meta.is_file(), "non-file artifact")?;
                    let name = path.strip_prefix(root).map_err(|e| e.to_string())?.to_str().ok_or("non-UTF8 path")?.to_owned();
                    safe_path(&name)?;
                    if name != "manifest.json" {
                        out.insert(name, std::fs::read(&path).map_err(|e| e.to_string())?);
                    }
                }
            }
            Ok(())
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out)?;
        Ok(out)
    }
    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Entry {
        path: String,
        size_bytes: u64,
        digest: String,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Manifest {
        schema_version: String,
        protocol_version: String,
        files: Vec<Entry>,
    }
    fn manifest(files: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
        bytes(
            &serde_json::to_value(Manifest {
                schema_version: "1.0".to_owned(),
                protocol_version: "1.0".to_owned(),
                files: files.iter().map(|(path, raw)| Entry { path: path.clone(), size_bytes: raw.len() as u64, digest: digest(raw) }).collect(),
            })
            .unwrap(),
        )
    }
    pub fn manifest_agreement(root: &Path) -> Check {
        let raw = read(root, "manifest.json")?;
        let m: Manifest = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        require(m.schema_version == "1.0" && m.protocol_version == "1.0", "manifest version")?;
        let files = inventory(root)?;
        let mut previous: Option<&str> = None;
        for e in &m.files {
            safe_path(&e.path)?;
            require(e.path != "manifest.json" && previous.is_none_or(|p| p.as_bytes() < e.path.as_bytes()), "duplicate/unsorted/self manifest path")?;
            previous = Some(&e.path);
            let raw = files.get(&e.path).ok_or_else(|| format!("missing manifest file {}", e.path))?;
            require(raw.len() as u64 == e.size_bytes && digest(raw) == e.digest, format!("size/digest mismatch {}", e.path))?;
        }
        require(files.len() == m.files.len(), "extra/unlisted artifact")?;
        require(raw == manifest(&files), "non-deterministic manifest encoding")?;
        println!("bundle manifest {} ({} entries)", digest(&raw), m.files.len());
        Ok(())
    }
    pub fn coverage_agreement(root: &Path) -> Check {
        let c = catalog(root)?;
        require(c.schema_version == "1.0" && !c.cases.is_empty(), "empty/wrong catalog")?;
        let expected = plan();
        require(value(root, "cases.json")? == expected.catalog(), "catalog missing/changed coverage")?;
        for (path, raw) in &expected.files {
            require(read(root, path)? == *raw, format!("changed exact corpus/transcript bytes: {path}"))?;
        }
        let mut seen = BTreeSet::new();
        let mut last = "";
        for case in &c.cases {
            require(last.as_bytes() < case.id.as_bytes() && seen.insert(&case.path), "unsorted/duplicate case identity/path")?;
            last = &case.id;
            require(["frame", "data", "transcript", "bytes"].contains(&case.kind.as_str()), "unknown case kind")?;
            require(["valid", "invalid", "not_applicable"].contains(&case.shape_expected.as_str()), "unknown shape expectation")?;
            require(
                ["accept", "invalid_request", "method_not_found", "transport_error", "transcript"].contains(&case.handling_expected.as_str()),
                "unknown handling expectation",
            )?;
        }
        for (method, _, _, _) in mastra_codec_catalog().0 {
            for prefix in ["request-", "response-", "response-error-", "result-"] {
                require(c.cases.iter().any(|c| c.id == format!("{prefix}{method}")), format!("missing {prefix}{method}"))?;
            }
        }
        for (event, _, _) in mastra_codec_catalog().1 {
            require(c.cases.iter().any(|c| c.id == format!("event-{event}")), format!("missing {event}"))?;
        }
        for code in error_codes() {
            for branch in ["empty", "nonempty"] {
                require(c.cases.iter().any(|c| c.id == format!("error-{code}-{branch}")), format!("missing {code}/{branch}"))?;
            }
        }
        println!("catalog: {} distinct persisted cases (shape, byte and transcript layers remain separate)", c.cases.len());
        Ok(())
    }
    pub fn record() -> Check {
        require(std::env::var("SOLIDCRAFT_MASTRA_RECORD").as_deref() == Ok("1"), "record permission missing")?;
        let root = mastra_bundle_path();
        let p = plan();
        let catalog = bytes(&p.catalog());
        let mut files = p.files;
        files.insert("cases.json".to_owned(), catalog);
        files.insert("protocol.schema.json".to_owned(), bytes(&schema()));
        files.insert("README.md".to_owned(), read(&root, "README.md")?);
        if root.exists() {
            for existing in inventory(&root)?.keys() {
                require(files.contains_key(existing), format!("recorder refuses unowned artifact {existing}"))?;
            }
        }
        for (path, raw) in &files {
            safe_path(path)?;
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(target, raw).map_err(|e| e.to_string())?;
        }
        std::fs::write(root.join("manifest.json"), manifest(&files)).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn id(s: &str) -> Id {
        Id::parse(s).unwrap()
    }
    fn request(method: &str, request_id: &str, params: Value) -> Value {
        let mut v = mastra_codec_request(method, params, request_id.starts_with("h:"));
        v["requestId"] = json!(request_id);
        v
    }
    fn reply(request_id: &str, method: &str, result: Value) -> Value {
        json!({"protocolVersion":"1.0","kind":"response","scope":mastra_codec_scope(method),"requestId":request_id,"outcome":"ok","result":result})
    }
    fn send(v: Value, control: bool) -> Value {
        json!({"action":"send","lane":if control {"control"}else{"data"},"frame":v})
    }
    fn finished(request_id: &str, method: &str, result: Value) -> Value {
        json!({"action":"finished","requestId":request_id,"scope":mastra_codec_scope(method),"method":method,"result":result})
    }
    fn closed(reason: &str) -> Vec<Value> {
        vec![
            json!({"action":"close_turn","scope":mastra_codec_scope("turn.start"),"reason":reason}),
            json!({"action":"turn_closed","scope":mastra_codec_scope("turn.start"),"cause":reason}),
        ]
    }
    fn runtime_error(code: &str) -> Value {
        json!({"code":code,"message":match code {"TIMEOUT"=>"Runtime request deadline expired.","CANCELLED"=>"Runtime request was cancelled.","RESOURCE_EXHAUSTED"=>"Runtime admission is full.",_=>"Runtime transport closed."},"retry":"never","details":{}})
    }
    fn error_reply(request_id: &str, method: &str, code: &str) -> Value {
        json!({"protocolVersion":"1.0","kind":"response","scope":mastra_codec_scope(method),"requestId":request_id,"outcome":"error","error":runtime_error(code)})
    }
    struct Trace<'a> {
        plan: &'a mut Plan,
        id: &'static str,
        limits: Limits,
        steps: Vec<Value>,
    }
    impl Trace<'_> {
        fn wire(&mut self, tag: &str, v: Value, actions: Vec<Value>) {
            let path = format!("fixtures/valid/trace-{}-{:03}.ndjson", self.id, self.steps.len());
            assert!(self.plan.files.insert(path.clone(), line(&v)).is_none());
            self.steps.push(
                json!({"tag":tag,"path":path,"phase":if v["method"]=="runtime.hello"||(v["kind"]=="response"&&v["requestId"]=="h:hello") {"bootstrap"}else{"negotiated"}}),
            );
            self.expect(actions);
        }
        fn expect(&mut self, actions: Vec<Value>) {
            self.steps.push(json!({"tag":"expect_actions","actions":actions}));
        }
        fn complete(&mut self, op: &str, actions: Vec<Value>) {
            self.steps.push(json!({"tag":"host_complete","operation":op}));
            self.expect(actions);
        }
        fn prefix(&mut self, acknowledge_turn: bool) {
            let catalog = mastra_codec_catalog().0;
            let instance = if self.id == "reset-replacement" && !self.steps.is_empty() { "i2" } else { "i" };
            let get = |name: &str| catalog.iter().find(|x| x.0 == name).unwrap();
            let hello = request("runtime.hello", "h:hello", json!({"supportedVersions":["1.0"],"hostVersion":"1","limits":self.limits}));
            self.wire("host_request", hello.clone(), vec![send(hello, false)]);
            let mut hello_result = get("runtime.hello").2.clone();
            hello_result["runtimeInstanceId"] = json!(instance);
            hello_result["limits"] = json!(self.limits);
            hello_result["capabilities"]["hostModelCalls"] = json!(true);
            hello_result["capabilities"]["hostAgentTasks"] = json!(true);
            self.wire(
                "runtime_frame",
                reply("h:hello", "runtime.hello", hello_result.clone()),
                vec![finished("h:hello", "runtime.hello", hello_result)],
            );
            self.wire("runtime_frame",json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope("runtime.ready"),"sequence":1,"requestId":"h:hello","event":"runtime.ready","data":{"runtimeInstanceId":instance}}),vec![json!({"action":"ready","runtimeInstanceId":instance})]);
            let session = request("session.start", "h:session", get("session.start").1.clone());
            self.wire("host_request", session.clone(), vec![send(session, false)]);
            self.wire(
                "runtime_frame",
                reply("h:session", "session.start", get("session.start").2.clone()),
                vec![finished("h:session", "session.start", get("session.start").2.clone())],
            );
            let turn = request("turn.start", "h:turn", get("turn.start").1.clone());
            self.wire("host_request", turn.clone(), vec![send(turn, false)]);
            if acknowledge_turn {
                self.wire(
                    "runtime_frame",
                    reply("h:turn", "turn.start", json!({"state":"accepted"})),
                    vec![finished("h:turn", "turn.start", json!({"state":"accepted"}))],
                );
            }
        }
        fn callback(&mut self, name: &str, request_id: &str) {
            let params = mastra_codec_catalog().0.into_iter().find(|x| x.0 == name).unwrap().1;
            self.wire(
                "runtime_frame",
                request(name, request_id, params.clone()),
                vec![json!({"action":"dispatch","scope":mastra_codec_scope(name),"method":name,"params":params,"deadlineMs":1000,"cancelled":false})],
            );
        }
        fn callback_finish(&mut self, name: &str, request_id: &str, expect: bool) {
            let result = mastra_codec_catalog().0.into_iter().find(|x| x.0 == name).unwrap().2;
            let path = format!("fixtures/valid/trace-{}-reply-{}.json", self.id, self.steps.len());
            self.plan.files.insert(path.clone(), bytes(&result));
            self.steps.push(json!({"tag":"host_complete","operation":"callback","requestId":request_id,"path":path}));
            self.expect(if expect { vec![send(reply(request_id, name, result), false)] } else { vec![] });
        }
        fn tick(&mut self, ms: u64, actions: Vec<Value>) {
            self.steps.push(json!({"tag":"advance_ms","milliseconds":ms}));
            self.expect(actions);
        }
        fn finish(self) {
            let path = format!("transcripts/{}.json", self.id);
            let v = json!({"schemaVersion":"1.0","id":self.id,"initialLimits":self.limits,"steps":self.steps});
            self.plan.raw(&format!("transcript-{}", self.id), &path, bytes(&v), "transcript", None, "not_applicable", "transcript", None, true);
        }
    }
    fn fatal_actions(pending: Option<&str>) -> Vec<Value> {
        let mut a = vec![
            json!({"action":"close_turn","scope":mastra_codec_scope("turn.start"),"reason":"TransportLoss"}),
            json!({"action":"retire_session","sessionId":"s"}),
            json!({"action":"turn_closed","scope":mastra_codec_scope("turn.start"),"cause":"TransportLoss"}),
        ];
        if let Some(id) = pending {
            a.push(json!({"action":"finished","requestId":id,"scope":mastra_codec_scope("turn.start"),"error":"TRANSPORT_ERROR"}));
        }
        a.extend([json!({"action":"transport_closed","cause":"TRANSPORT_ERROR"}), json!({"action":"stop_tree"})]);
        a
    }
    fn transcript_plan(p: &mut Plan) {
        let mut t = Trace { plan: p, id: "nested-callback", limits: Limits::default(), steps: vec![] };
        t.prefix(false);
        t.callback("model.generate", "r:call");
        t.callback_finish("model.generate", "r:call", true);
        t.wire(
            "runtime_frame",
            reply("h:turn", "turn.start", json!({"state":"accepted"})),
            vec![finished("h:turn", "turn.start", json!({"state":"accepted"}))],
        );
        t.finish();
        let mut t = Trace { plan: p, id: "cancel-late-terminal", limits: Limits::default(), steps: vec![] };
        t.prefix(true);
        let cancel = request("turn.cancel", "h:cancel", json!({"reason":"user"}));
        let mut a = closed("User");
        a.push(send(cancel.clone(), true));
        t.wire("host_request", cancel, a);
        for sequence in [2, 3] {
            t.wire("runtime_frame",json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope("turn.start"),"sequence":sequence,"requestId":"h:turn","event":"turn.finished","data":{"state":"validated_success","reason":"runtime proposal is not host acceptance","document":{"documentId":"d","revision":"18446744073709551615"},"changes":"unknown","validationRunId":null,"report":null}}),vec![]);
        }
        t.wire(
            "runtime_frame",
            reply("h:cancel", "turn.cancel", json!({"state":"cancelling"})),
            vec![finished("h:cancel", "turn.cancel", json!({"state":"cancelling"}))],
        );
        t.tick(5000, vec![]);
        t.finish();
        let mut t = Trace { plan: p, id: "eof-once", limits: Limits::default(), steps: vec![] };
        t.prefix(false);
        t.callback("agent.run", "r:call");
        t.complete("eof", fatal_actions(Some("h:turn")));
        t.callback_finish("agent.run", "r:call", false);
        t.complete("eof", vec![]);
        t.finish();
        let mut t = Trace { plan: p, id: "callback-timeout", limits: Limits::default(), steps: vec![] };
        t.prefix(true);
        t.callback("model.generate", "r:call");
        t.tick(999, vec![]);
        t.tick(1, vec![send(error_reply("r:call", "model.generate", "TIMEOUT"), false)]);
        t.callback_finish("model.generate", "r:call", false);
        t.finish();
        let mut t = Trace { plan: p, id: "backpressure-control", limits: Limits { pending_requests: 1, ..Limits::default() }, steps: vec![] };
        t.prefix(true);
        t.callback("model.generate", "r:call");
        let params = mastra_codec_catalog().0.into_iter().find(|x| x.0 == "model.generate").unwrap().1;
        t.wire(
            "runtime_frame",
            request("model.generate", "r:busy", params),
            vec![send(error_reply("r:busy", "model.generate", "RESOURCE_EXHAUSTED"), false)],
        );
        t.wire(
            "runtime_frame",
            request("request.cancel", "r:cancel", json!({"targetRequestId":"r:call"})),
            vec![
                send(error_reply("r:call", "model.generate", "CANCELLED"), false),
                send(reply("r:cancel", "request.cancel", json!({"state":"terminal"})), true),
            ],
        );
        t.callback_finish("model.generate", "r:call", false);
        t.finish();
        let mut t = Trace { plan: p, id: "reset-replacement", limits: Limits::default(), steps: vec![] };
        t.prefix(true);
        t.callback("agent.run", "r:call");
        let reset = request("session.reset", "h:reset", json!({"nextSessionId":"next","instructions":"reset"}));
        let mut a = closed("Reset");
        a.insert(1, json!({"action":"retire_session","sessionId":"s"}));
        a.push(send(error_reply("r:call", "agent.run", "CANCELLED"), false));
        a.push(send(reset.clone(), false));
        t.wire("host_request", reset, a);
        t.steps.push(json!({"tag":"host_complete","operation":"replace","generation":"g2"}));
        t.expect(vec![]);
        t.callback_finish("agent.run", "r:call", false);
        let late = reply("h:turn", "turn.start", json!({"state":"accepted"}));
        let path = "fixtures/valid/trace-reset-old-generation.ndjson";
        t.plan.files.insert(path.to_owned(), line(&late));
        t.steps.push(json!({"tag":"runtime_frame","path":path,"generation":"g","phase":"negotiated"}));
        t.expect(vec![]);
        t.prefix(true);
        let path = "fixtures/valid/trace-retired-permission.json";
        t.plan.files.insert(path.to_owned(),bytes(&json!({"agentSessionId":"old-agent","kind":"permission","content":{"permissionId":"old-permission","title":"Retired","state":"waiting"}})));
        t.steps.push(json!({"tag":"host_complete","operation":"progress","requestId":"r:call","path":path}));
        t.expect(vec![]);
        t.finish();
        let mut t = Trace { plan: p, id: "method-neutral-correlation", limits: Limits::default(), steps: vec![] };
        t.prefix(false);
        t.wire("runtime_frame", reply("h:turn", "turn.start", json!({"state":"cancelling"})), fatal_actions(Some("h:turn")));
        t.finish();
        let mut t = Trace { plan: p, id: "strict-input-flood", limits: Limits::default(), steps: vec![] };
        t.prefix(true);
        t.steps.push(json!({"tag":"runtime_frame","path":"fixtures/invalid/bytes-no-lf.bin","phase":"negotiated"}));
        t.expect(fatal_actions(None));
        t.finish();
        let mut t = Trace { plan: p, id: "cancel-coalescing-deadline", limits: Limits::default(), steps: vec![] };
        t.prefix(true);
        let cancel = request("turn.cancel", "h:cancel", json!({"reason":"user"}));
        let mut a = closed("User");
        a.push(send(cancel.clone(), true));
        t.wire("host_request", cancel, a);
        t.wire(
            "host_request",
            request("turn.cancel", "h:again", json!({"reason":"user"})),
            vec![json!({"action":"coalesced","requestId":"h:again","existingRequestId":"h:cancel"})],
        );
        t.tick(4999, vec![]);
        t.tick(
            1,
            vec![
                json!({"action":"retire_session","sessionId":"s"}),
                json!({"action":"finished","requestId":"h:cancel","scope":mastra_codec_scope("turn.cancel"),"error":"TIMEOUT"}),
                json!({"action":"stop_tree"}),
            ],
        );
        t.tick(3000, vec![json!({"action":"transport_closed","cause":"TRANSPORT_ERROR"})]);
        t.tick(1, vec![]);
        t.finish();
        let mut t = Trace { plan: p, id: "turn-wall-deadline", limits: Limits::default(), steps: vec![] };
        t.prefix(true);
        t.tick(1200000, closed("Deadline"));
        t.finish();
    }
    fn host_call_value(call: &HostCall) -> Value {
        match call {
            HostCall::ConnectionDescribe(x) => json!(x),
            HostCall::ModelGenerate(x) => json!(x),
            HostCall::AgentRun(x) => json!(x),
            HostCall::ValidationSubmit(x) => json!(x),
            HostCall::ValidationStatus(x) => json!(x),
            HostCall::ValidationReport(x) => json!(x),
            HostCall::ValidationCancel(x) => json!(x),
        }
    }
    fn runtime_reply_value(reply: &RuntimeReply) -> Value {
        match reply {
            RuntimeReply::RuntimeHello(x) => json!(x),
            RuntimeReply::SessionStart(x) => json!(x),
            RuntimeReply::TurnStart(x) => json!(x),
            RuntimeReply::TurnCancel(x) => json!(x),
            RuntimeReply::RequestCancel(x) => json!(x),
            RuntimeReply::SessionReset(x) => json!(x),
            RuntimeReply::RuntimeShutdown(x) => json!(x),
        }
    }
    fn summary(a: &MachineAction, start: Instant) -> Value {
        match a {
            MachineAction::Send { bytes, lane, .. } => {
                send(serde_json::from_slice(bytes).unwrap(), matches!(lane, super::super::process::PipeLane::Control))
            }
            MachineAction::Dispatch(c) => {
                json!({"action":"dispatch","scope":c.scope,"method":c.call.method(),"params":host_call_value(&c.call),"deadlineMs":c.deadline.duration_since(start).as_millis() as u64,"cancelled":c.cancellation.is_cancelled()})
            }
            MachineAction::CloseTurn { scope, reason } => {
                json!({"action":"close_turn","scope":Scope::Turn(scope.clone()),"reason":format!("{reason:?}")})
            }
            MachineAction::RetireSession(id) => json!({"action":"retire_session","sessionId":id}),
            MachineAction::StopTree => json!({"action":"stop_tree"}),
            MachineAction::Rejected { request_id, error } => json!({"action":"rejected","requestId":request_id,"error":error.code}),
            MachineAction::Coalesced { request_id, existing_request_id } => {
                json!({"action":"coalesced","requestId":request_id,"existingRequestId":existing_request_id})
            }
            MachineAction::Notice { notice, .. } => match notice.as_ref() {
                ClientNotice::RuntimeReady(i) => json!({"action":"ready","runtimeInstanceId":i.runtime_instance_id}),
                ClientNotice::RequestFinished { request_id, scope, result } => match result {
                    Ok(r) => json!({"action":"finished","requestId":request_id,"scope":scope,"method":r.method(),"result":runtime_reply_value(r)}),
                    Err(e) => json!({"action":"finished","requestId":request_id,"scope":scope,"error":e.code}),
                },
                ClientNotice::TurnClosed { scope, cause } => {
                    json!({"action":"turn_closed","scope":Scope::Turn(scope.clone()),"cause":format!("{cause:?}")})
                }
                ClientNotice::TransportClosed { cause, .. } => {
                    json!({"action":"transport_closed","cause":match cause {TransportCause::ExpectedEof=>"expected_eof",TransportCause::Error(_)=>"TRANSPORT_ERROR"}})
                }
                ClientNotice::TurnFinishedProposal { scope, proposal } => json!({"action":"proposal","scope":scope,"proposal":proposal}),
                ClientNotice::Event { scope, event } => json!({"action":"event","scope":scope,"event":event.method()}),
            },
        }
    }
    fn machine(generation: &Id, limits: Limits, now: Instant) -> Check<RuntimeMachine> {
        let mut m = RuntimeMachine::new(generation.clone(), limits, now).map_err(|e| format!("{e:?}"))?;
        require(
            m.apply(
                MachineInput::Services {
                    generation: generation.clone(),
                    services: ServiceCapabilities {
                        connection_describe: true,
                        model_generate: true,
                        agent_run: true,
                        validation: true,
                        streaming: false,
                    },
                },
                now,
            )
            .is_empty(),
            "unexpected initial services actions",
        )?;
        Ok(m)
    }
    fn replay(root: &Path, path: &str) -> Check {
        let trace = value(root, path)?;
        require(
            trace.as_object().is_some_and(|m| m.len() == 4) && trace["schemaVersion"] == "1.0" && trace["id"].is_string(),
            "invalid transcript envelope",
        )?;
        let limits: Limits = serde_json::from_value(trace["initialLimits"].clone()).map_err(|e| e.to_string())?;
        let start = Instant::now();
        let mut now = start;
        let mut generation = id("g");
        let mut m = machine(&generation, limits.clone(), now)?;
        let mut pending = Vec::<MachineAction>::new();
        let mut tokens = BTreeMap::<String, (CallbackToken, String)>::new();
        let steps = trace["steps"].as_array().ok_or("transcript steps")?;
        require(!steps.is_empty(), "empty transcript")?;
        for (i, step) in steps.iter().enumerate() {
            let generation_input = step["generation"].as_str().map(id).unwrap_or_else(|| generation.clone());
            let actions = match step["tag"].as_str().ok_or("step tag")? {
                "expect_actions" => {
                    let actual = json!(pending.iter().map(|a| summary(a, start)).collect::<Vec<_>>());
                    require(actual == step["actions"], format!("{path} step {i}:\nactual {actual}\nexpected {}", step["actions"]))?;
                    for action in pending.drain(..) {
                        let release = match action {
                            MachineAction::Send { ticket, .. } => Some(MachineInput::Written { generation: generation.clone(), ticket }),
                            MachineAction::Notice { ticket, .. } => Some(MachineInput::NoticeConsumed { generation: generation.clone(), ticket }),
                            _ => None,
                        };
                        if let Some(input) = release {
                            require(m.apply(input, now).is_empty(), "release generated unaccounted actions")?;
                        }
                    }
                    vec![]
                }
                "advance_ms" => {
                    now = now.checked_add(Duration::from_millis(step["milliseconds"].as_u64().ok_or("milliseconds")?)).ok_or("clock overflow")?;
                    m.apply(MachineInput::Tick, now)
                }
                "host_request" | "runtime_frame" => {
                    let raw = read(root, step["path"].as_str().ok_or("frame path")?)?;
                    let host = step["tag"] == "host_request";
                    let phase = if step["phase"] == "bootstrap" { DecodePhase::Bootstrap } else { DecodePhase::Negotiated };
                    if host {
                        let DecodeResult::Frame(Frame::Request { header, body: RequestBody::Host(request) }) =
                            decode_frame(&raw, phase, Sender::Host, &limits).map_err(|e| e.to_string())?
                        else {
                            return Err("not a typed host request".to_owned());
                        };
                        m.apply(
                            MachineInput::Submit {
                                generation: generation_input,
                                request_id: header.request_id,
                                scope: header.scope,
                                request,
                                deadline: now + Duration::from_secs(300),
                            },
                            now,
                        )
                    } else {
                        let decoded = decode_frame(&raw, phase, Sender::Runtime, &limits);
                        let request_id = serde_json::from_slice::<Value>(&raw).ok().and_then(|v| v["requestId"].as_str().map(str::to_owned));
                        let a = match decoded {
                            Ok(decoded) => {
                                m.apply(MachineInput::Peer { generation: generation_input.clone(), sender: Sender::Runtime, decoded }, now)
                            }
                            Err(_) => m.apply(MachineInput::InputFailed { generation: generation_input }, now),
                        };
                        for action in &a {
                            if let MachineAction::Dispatch(c) = action {
                                tokens.insert(request_id.clone().ok_or("callback identity")?, (c.token.clone(), c.call.method().to_owned()));
                            }
                        }
                        a
                    }
                }
                "host_complete" => match step["operation"].as_str().ok_or("completion operation")? {
                    "callback" => {
                        let (token, method) = tokens.get(step["requestId"].as_str().ok_or("completion identity")?).ok_or("unknown callback token")?;
                        let result = decode_host_reply(method, value(root, step["path"].as_str().ok_or("reply path")?)?, &limits)
                            .map_err(|e| e.to_string())?;
                        m.apply(MachineInput::CallbackFinished { token: token.clone(), result: Box::new(Ok(result)) }, now)
                    }
                    "progress" => {
                        let (token, _) = tokens.get(step["requestId"].as_str().ok_or("progress identity")?).ok_or("unknown progress token")?;
                        let event: AgentProgress =
                            serde_json::from_value(value(root, step["path"].as_str().ok_or("progress path")?)?).map_err(|e| e.to_string())?;
                        m.apply(MachineInput::Progress { token: token.clone(), event: CallProgress::AgentProgress(event) }, now)
                    }
                    "eof" => m.apply(MachineInput::Eof { generation: generation_input, stderr_tail: vec![] }, now),
                    "replace" => {
                        require(pending.is_empty(), "replacement with unconsumed actions")?;
                        generation = step["generation"].as_str().map(id).ok_or("replacement generation")?;
                        m = machine(&generation, limits.clone(), now)?;
                        vec![]
                    }
                    _ => return Err("unknown completion operation".to_owned()),
                },
                _ => return Err("unknown transcript tag".to_owned()),
            };
            pending.extend(actions);
        }
        require(pending.is_empty(), "transcript ends with unverified actions")
    }
    pub fn transcript_agreement(root: &Path) -> Check {
        let traces = catalog(root)?.cases.into_iter().filter(|c| c.kind == "transcript").collect::<Vec<_>>();
        require(!traces.is_empty(), "no actual-machine transcripts")?;
        let failures = traces.iter().filter_map(|t| replay(root, &t.path).err()).collect::<Vec<_>>();
        require(failures.is_empty(), failures.join("\n\n"))
    }
    pub fn contract_review() -> Check {
        let p = plan();
        let root = schema();
        local_refs(&root, &root)?;
        let mut eval = Evaluator { root, validators: HashMap::new() };
        for c in p.cases.values() {
            if let Some(schema) = &c.schema_ref {
                let v: Value = serde_json::from_slice(&p.files[&c.path]).map_err(|e| format!("{}: {e}", c.id))?;
                require(eval.accepts(schema, &v)? == (c.shape_expected == "valid"), format!("intended shape {} != {}", c.id, c.shape_expected))?;
            }
        }
        println!(
            "Reviewed generated contract: {} persisted cases, {} files; Draft2020-12 evaluator with offline/local refs",
            p.cases.len(),
            p.files.len()
        );
        Ok(())
    }
    struct TempBundle(PathBuf);
    impl TempBundle {
        fn copy() -> Check<Self> {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "solidcraft-mastra-bundle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).map_err(|e| e.to_string())?;
            let owner = Self(root);
            let mut files = inventory(&mastra_bundle_path())?;
            files.insert("manifest.json".to_owned(), read(&mastra_bundle_path(), "manifest.json")?);
            for (path, raw) in files {
                let target = owner.0.join(path);
                std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
                std::fs::write(target, raw).map_err(|e| e.to_string())?;
            }
            Ok(owner)
        }
        fn write(&self, path: &str, v: &Value) -> Check {
            std::fs::write(self.0.join(path), bytes(v)).map_err(|e| e.to_string())
        }
        fn reseal(&self) -> Check {
            std::fs::write(self.0.join("manifest.json"), manifest(&inventory(&self.0)?)).map_err(|e| e.to_string())
        }
    }
    impl Drop for TempBundle {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                eprintln!("owned temp bundle cleanup {}: {e}", self.0.display());
            }
        }
    }
    pub fn mutations() -> Check {
        for mutation in ["add", "omit", "change", "digest", "unsafe", "duplicate"] {
            let t = TempBundle::copy()?;
            match mutation {
                "add" => std::fs::write(t.0.join("extra.json"), b"{}".as_slice()).map_err(|e| e.to_string())?,
                "omit" => std::fs::remove_file(t.0.join("fixtures/valid/document-stamp-unsafe-js.json")).map_err(|e| e.to_string())?,
                "change" => std::fs::write(t.0.join("fixtures/valid/document-stamp-unsafe-js.json"), b"{}".as_slice()).map_err(|e| e.to_string())?,
                name => {
                    let mut m = value(&t.0, "manifest.json")?;
                    match name {
                        "digest" => m["files"][0]["digest"] = json!(format!("sha256:{}", "0".repeat(64))),
                        "unsafe" => m["files"][0]["path"] = json!("../escape"),
                        "duplicate" => {
                            let duplicate = m["files"][0].clone();
                            m["files"].as_array_mut().unwrap().insert(0, duplicate);
                        }
                        _ => return Err("unknown manifest mutation".to_owned()),
                    };
                    t.write("manifest.json", &m)?;
                }
            }
            require(manifest_agreement(&t.0).is_err(), format!("manifest mutation accepted: {mutation}"))?;
        }
        #[cfg(unix)]
        {
            let t = TempBundle::copy()?;
            std::os::unix::fs::symlink(t.0.join("README.md"), t.0.join("alias.json")).map_err(|e| e.to_string())?;
            require(manifest_agreement(&t.0).is_err(), "symlink accepted")?;
        }
        let t = TempBundle::copy()?;
        let path = "transcripts/cancel-late-terminal.json";
        replay(&t.0, path)?;
        let mut trace = value(&t.0, path)?;
        let mut changed = false;
        for step in trace["steps"].as_array_mut().unwrap() {
            if let Some(actions) = step.get_mut("actions").and_then(Value::as_array_mut) {
                for action in actions {
                    if action["action"] == "turn_closed" {
                        action["cause"] = json!("TerminalProposal");
                        changed = true;
                    }
                }
            }
        }
        require(changed, "terminal mutation did not target a real assertion")?;
        t.write(path, &trace)?;
        t.reseal()?;
        manifest_agreement(&t.0)?;
        require(replay(&t.0, path).is_err(), "re-sealed false terminal transcript accepted")?;
        let t = TempBundle::copy()?;
        fixture_agreement(&t.0)?;
        t.write("fixtures/valid/document-stamp-unsafe-js.json", &json!({"documentId":"doc-A","revision":9_007_199_254_740_993_u64}))?;
        t.reseal()?;
        manifest_agreement(&t.0)?;
        require(fixture_agreement(&t.0).is_err(), "re-sealed numeric revision accepted")?;
        Ok(())
    }
}

type MastraEventCase = (String, Value, bool);
fn mastra_codec_catalog() -> (Vec<MastraMethodCase>, Vec<MastraEventCase>) {
    let artifact = json!({"artifactId":"a","digest":format!("sha256:{}", "0".repeat(64)),"sizeBytes":1,"mediaType":"application/json"});
    let document = json!({"documentId":"d","revision":"18446744073709551615"});
    let profile = json!({"profileId":"p","version":"1","digest":artifact["digest"]});
    let candidate =
        json!({"candidateId":"c","source":document,"candidateDigest":artifact["digest"],"manifestDigest":artifact["digest"],"artifact":artifact});
    let capabilities = json!({"modelGenerate":false,"agentRun":false,"tools":false,"images":false,"documents":false,"structuredOutput":false,"modelSelection":false,"usage":false,"delivery":"buffered"});
    let report = json!({"schemaVersion":"1.0","runId":"v","candidateId":"c","candidateDigest":artifact["digest"],"manifestDigest":artifact["digest"],"profile":profile,"runner":{"runnerId":"r","version":"1","engineVersion":"1"},"state":"cancelled","startedAt":null,"finishedAt":"2026-11-05T12:00:00.000Z","checks":[],"error":null});
    let text = json!([{"type":"text","text":"hi"}]);
    let methods = vec![
        ("runtime.hello", json!({"supportedVersions":["1.0"],"hostVersion":"1","limits":super::mastra_protocol::Limits::default()}), json!({"selectedVersion":"1.0","runtimeInstanceId":"i","runtimeVersion":"1","capabilities":{"workflowIds":["w"],"hostModelCalls":false,"hostAgentTasks":false,"guardedExecute":false,"guardedBatch":false,"snapshotValidation":false},"limits":super::mastra_protocol::Limits::default()}), true),
        ("session.start", json!({"workflowId":"w","instructions":"hi","bridge":null}), json!({"state":"ready","workflowId":"w"}), true),
        ("turn.start", json!({"binding":{"workflowId":"w","connectionId":"c","modelId":"m","document":document,"profile":profile},"parts":text,"context":{"selection":[{"x":-1.25,"large":9007199254740992_u64}],"requirements":null},"budgets":super::mastra_protocol::Budgets::default()}), json!({"state":"accepted"}), true),
        ("turn.cancel", json!({"reason":"user"}), json!({"state":"cancelling"}), true),
        ("request.cancel", json!({"targetRequestId":"h:"}), json!({"state":"terminal"}), true),
        ("session.reset", json!({"nextSessionId":"next","instructions":"hi"}), json!({"state":"ready","sessionId":"next"}), true),
        ("runtime.shutdown", json!({"reason":"app_exit"}), json!({"state":"stopping"}), true),
        ("connection.describe", json!({"connectionId":"c","modelId":"m"}), json!({"state":"available","capabilities":capabilities,"authMethods":[]}), false),
        ("model.generate", json!({"connectionId":"c","modelId":"m","messages":[{"role":"user","content":text,"toolCalls":[],"toolCallId":null}],"tools":[{"name":"measure","description":"exact","inputSchema":{"type":"object","properties":{"x":{"minimum":-1.25}}}}],"options":{"delivery":"buffered","timeoutSeconds":1,"maxOutputTokens":null,"temperature":null,"reasoningEffort":null,"outputSchema":null},"continuation":null}), json!({"content":text,"toolCalls":[{"id":"call","name":"measure","arguments":{"x":-1.25,"n":9007199254740992_u64}}],"finishReason":"tool_calls","usage":null,"continuation":null,"delivery":"buffered"}), false),
        ("agent.run", json!({"connectionId":"c","modelId":"m","task":text,"context":{"document":document,"requirements":null,"delegationMode":"proposal_only"},"timeoutSeconds":1}), json!({"agentSessionId":"as","state":"completed","content":text,"stopReason":"stop","usage":null}), false),
        ("validation.submit", json!({"candidate":candidate,"profile":profile,"requestId":"1793876400000:12345678-1234-4234-8234-123456789abc"}), json!({"runId":"v","state":"queued","candidate":candidate,"profile":profile}), false),
        ("validation.status", json!({"runId":"v"}), json!({"runId":"v","state":"running","candidate":candidate,"profile":profile,"updatedAt":"2026-11-05T12:00:00.000Z","reportAvailable":false}), false),
        ("validation.report", json!({"runId":"v"}), json!({"report":report,"reference":artifact}), false),
        ("validation.cancel", json!({"runId":"v"}), json!({"runId":"v","state":"cancelled"}), false),
    ].into_iter().map(|(m,p,r,h)| (m.to_owned(),p,r,h)).collect();
    let events = vec![
        ("runtime.ready", json!({"runtimeInstanceId":"i"}), false),
        (
            "connection.state",
            json!({"connectionId":"c","state":"auth_required","authMethods":[{"id":"sign-in","name":"Sign in","description":"Host UI"}]}),
            true,
        ),
        ("workflow.stage", json!({"stageId":"s","title":"Build","state":"running"}), false),
        ("workflow.text", json!({"text":"hello","channel":"answer"}), false),
        ("workflow.plan", json!({"steps":[{"id":"s","title":"Build","state":"pending"}]}), false),
        ("workflow.tool", json!({"callId":"c","name":"measure","state":"completed","output":null}), false),
        (
            "agent.progress",
            json!({"agentSessionId":"as","kind":"permission","content":{"permissionId":"p","title":"Allow?","state":"waiting"}}),
            true,
        ),
        ("model.delta", json!({"index":0,"text":"hello"}), true),
        (
            "turn.finished",
            json!({"state":"cancelled","reason":"user","document":document,"changes":"unknown","validationRunId":null,"report":null}),
            false,
        ),
    ]
    .into_iter()
    .map(|(e, d, h)| (e.to_owned(), d, h))
    .collect();
    (methods, events)
}

fn mastra_codec_scope(method: &str) -> Value {
    match method {
        "runtime.hello" | "runtime.shutdown" | "runtime.ready" => json!({"sessionId":null,"turnId":null,"workflowRunId":null}),
        "session.start" | "session.reset" | "connection.describe" => json!({"sessionId":"s","turnId":null,"workflowRunId":null}),
        _ => json!({"sessionId":"s","turnId":"t","workflowRunId":"w"}),
    }
}

fn mastra_codec_request(method: &str, params: Value, host: bool) -> Value {
    json!({"protocolVersion":"1.0","kind":"request","scope":mastra_codec_scope(method),"requestId":if host {"h:"} else {"r:"},"method":method,"params":params})
}

fn mastra_codec_decode(value: &Value, host: bool) -> Result<super::mastra_protocol::DecodeResult, super::mastra_protocol::ProtocolViolation> {
    use super::mastra_protocol::*;
    decode_frame(format!("{value}\n").as_bytes(), DecodePhase::Negotiated, if host { Sender::Host } else { Sender::Runtime }, &Limits::default())
}

// Mutation caught: accepting serde's duplicate-key collapse, or missing == nullable.
#[test]
fn mastra_codec_duplicate_keys_and_required_nulls() {
    use super::mastra_protocol::*;
    let (methods, events) = mastra_codec_catalog();
    let good = mastra_codec_request("turn.cancel", json!({"reason":"user"}), true);
    let raw = good.to_string();
    for text in [
        raw.replacen('{', "{\"kind\":\"request\",", 1),
        raw.replacen("\"params\":{", "\"params\":{\"reason\":\"user\",", 1),
        raw.replacen('{', "{\"extensions\":{\"x\":{\"z\":1,\"z\":2}},", 1),
        raw.replacen('{', "{\"extensions\":{\"x\":1,\"\\u0078\":2},", 1),
    ] {
        assert!(
            decode_frame(format!("{text}\n").as_bytes(), DecodePhase::Negotiated, Sender::Host, &Limits::default()).is_err(),
            "duplicate accepted: {text}"
        );
    }
    let model = &methods.iter().find(|m| m.0 == "model.generate").unwrap().1;
    let mut request = mastra_codec_request("model.generate", model.clone(), false);
    request["params"]["messages"][0]["role"] = json!("assistant");
    request["params"]["messages"][0]["toolCalls"] = json!([{"id":"c","name":"m","arguments":{"x":1}}]);
    let duplicate = request.to_string().replace("\"arguments\":{\"x\":1}", "\"arguments\":{\"x\":1,\"x\":2}");
    assert!(decode_frame(format!("{duplicate}\n").as_bytes(), DecodePhase::Negotiated, Sender::Runtime, &Limits::default()).is_err());
    fn remove_nulls(value: &Value) -> Vec<Value> {
        let mut cases = Vec::new();
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if child.is_null() {
                        let mut changed = value.clone();
                        changed.as_object_mut().unwrap().remove(key);
                        cases.push(changed);
                    }
                    for nested in remove_nulls(child) {
                        let mut changed = value.clone();
                        changed[key] = nested;
                        cases.push(changed);
                    }
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    for nested in remove_nulls(child) {
                        let mut changed = value.clone();
                        changed[i] = nested;
                        cases.push(changed);
                    }
                }
            }
            _ => (),
        }
        cases
    }
    for (method, params, result, host) in methods {
        let request = mastra_codec_request(&method, params, host);
        assert!(matches!(mastra_codec_decode(&request, host), Ok(DecodeResult::Frame(_))), "valid {method}");
        for missing in remove_nulls(&request) {
            assert!(!matches!(mastra_codec_decode(&missing, host), Ok(DecodeResult::Frame(_))), "missing nullable {method}: {missing}");
        }
        for missing in remove_nulls(&result) {
            let decoded = if host {
                decode_runtime_reply(&method, missing, &Limits::default()).map(|_| ())
            } else {
                decode_host_reply(&method, missing, &Limits::default()).map(|_| ())
            };
            assert!(decoded.is_err(), "missing result nullable {method}");
        }
    }
    for (event, data, host) in events {
        let frame = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope(&event),"sequence":1,"event":event,"requestId":null,"data":data});
        assert!(matches!(mastra_codec_decode(&frame, host), Ok(DecodeResult::Frame(_))), "valid event {event}");
        for missing in remove_nulls(&frame) {
            assert!(mastra_codec_decode(&missing, host).is_err(), "missing event nullable {event}");
        }
    }
}

// Mutation caught: lossy UTF-8, extra framing, counting depth/bytes without root/LF.
#[test]
fn mastra_codec_byte_and_depth_boundaries() {
    use super::mastra_protocol::*;
    let good = mastra_codec_request("turn.cancel", json!({"reason":"user"}), true);
    let line = format!("{good}\n");
    for bytes in [
        vec![0xff, b'\n'],
        format!("\u{feff}{line}").into_bytes(),
        good.to_string().into_bytes(),
        b"\n".to_vec(),
        b"[]\n".to_vec(),
        b"null\n".to_vec(),
        format!("{good}{good}\n").into_bytes(),
        format!("{good}\n\n").into_bytes(),
        format!("{good}\r\n").into_bytes(),
    ] {
        assert!(decode_frame(&bytes, DecodePhase::Negotiated, Sender::Host, &Limits::default()).is_err(), "invalid frame accepted");
    }
    for number in ["NaN", "Infinity", "1e400", "-1e400"] {
        let raw = line.replacen('{', &format!("{{\"extensions\":{{\"n\":{number}}},"), 1);
        assert!(decode_frame(raw.as_bytes(), DecodePhase::Negotiated, Sender::Host, &Limits::default()).is_err());
    }
    for (depth, valid) in [(64, true), (65, false)] {
        // root + extensions + nested arrays = depth.
        let mut value = json!(0);
        for _ in 2..depth {
            value = json!([value]);
        }
        let mut frame = good.clone();
        frame["extensions"] = json!({"value":value});
        assert_eq!(mastra_codec_decode(&frame, true).is_ok(), valid, "depth {depth}");
    }
    for (length, valid) in [(8_388_608, true), (8_388_609, false)] {
        let mut frame = good.clone();
        frame["extensions"] = json!({"padding":""});
        let overhead = frame.to_string().len() + 1;
        frame["extensions"]["padding"] = json!("x".repeat(length - overhead));
        let line = format!("{frame}\n");
        assert_eq!(line.len(), length);
        assert_eq!(decode_frame(line.as_bytes(), DecodePhase::Negotiated, Sender::Host, &Limits::default()).is_ok(), valid, "bytes {length}");
    }
}

// Mutation caught: treating bad params as fatal, or a bad identity as recoverable.
#[test]
fn mastra_codec_request_errors_preserve_transport() {
    use super::mastra_protocol::*;
    let good = mastra_codec_request("session.start", json!({"workflowId":"w","instructions":"hi","bridge":null}), true);
    for (field, value, code) in [
        ("params", json!({}), "INVALID_REQUEST"),
        ("method", json!("future.method"), "METHOD_NOT_FOUND"),
        ("params", json!({"workflowId":"/bad","instructions":"hi","bridge":null}), "INVALID_REQUEST"),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        let DecodeResult::RequestError { header, error } = mastra_codec_decode(&bad, true).unwrap() else {
            panic!("not a recoverable request error")
        };
        assert_eq!(header.request_id.as_str(), "h:");
        assert_eq!(serde_json::to_value(error).unwrap()["code"], code);
    }
    for (field, value) in [
        ("requestId", json!("r:wrong")),
        ("requestId", json!("/bad")),
        ("scope", json!({"sessionId":"s","turnId":"t","workflowRunId":null})),
        ("protocolVersion", json!("2.0")),
        ("extensions", Value::Null),
        ("extra", json!(0)),
        ("params", Value::Null),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        let error = mastra_codec_decode(&bad, true).unwrap_err();
        assert_eq!(serde_json::to_value(error.wire_error()).unwrap()["code"], "TRANSPORT_ERROR");
    }
    let mut bootstrap = mastra_codec_request("runtime.hello", mastra_codec_catalog().0[0].1.clone(), true);
    bootstrap["protocolVersion"] = json!("2.0");
    assert!(decode_frame(format!("{bootstrap}\n").as_bytes(), DecodePhase::Bootstrap, Sender::Host, &Limits::default()).is_ok());
    assert!(mastra_codec_decode(&good, false).is_err());
    let mut bad_scope = good.clone();
    bad_scope["scope"] = mastra_codec_scope("runtime.hello");
    assert!(mastra_codec_decode(&bad_scope, true).is_err());
}

// Mutation caught: omitted variants, unchecked raw result delivery, permissive errors.
#[test]
fn mastra_codec_all_variants_and_error_details() {
    use super::mastra_protocol::*;
    let (methods, events) = mastra_codec_catalog();
    assert_eq!(methods.len(), 14);
    assert_eq!(events.len(), 9);
    for (method, params, result, host) in methods {
        let request = mastra_codec_request(&method, params, host);
        let DecodeResult::Frame(frame) = mastra_codec_decode(&request, host).unwrap() else { panic!("{method}") };
        assert_eq!(serde_json::from_slice::<Value>(&encode_frame(&frame, &Limits::default()).unwrap()).unwrap(), request);
        if host {
            assert!(decode_runtime_reply(&method, result.clone(), &Limits::default()).is_ok(), "{method}");
        } else {
            assert!(decode_host_reply(&method, result.clone(), &Limits::default()).is_ok(), "{method}");
        }
        let response = json!({"protocolVersion":"1.0","kind":"response","scope":request["scope"],"requestId":request["requestId"],"outcome":"ok","result":result});
        let DecodeResult::Frame(frame) = mastra_codec_decode(&response, !host).unwrap() else { panic!("response") };
        assert_eq!(serde_json::from_slice::<Value>(&encode_frame(&frame, &Limits::default()).unwrap()).unwrap(), response);
        let mut unknown = request;
        unknown["params"]["undeclared"] = json!(true);
        assert!(matches!(mastra_codec_decode(&unknown, host), Ok(DecodeResult::RequestError { .. })), "unknown param {method}");
    }
    for (event, data, host) in events {
        let value = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope(&event),"sequence":1,"requestId":null,"event":event,"data":data});
        let DecodeResult::Frame(frame) = mastra_codec_decode(&value, host).unwrap() else { panic!("{event}") };
        assert_eq!(serde_json::from_slice::<Value>(&encode_frame(&frame, &Limits::default()).unwrap()).unwrap(), value);
        assert!(mastra_codec_decode(&value, !host).is_err(), "wrong event direction {event}");
    }
    let codes = [
        "INCOMPATIBLE_VERSION",
        "INVALID_REQUEST",
        "METHOD_NOT_FOUND",
        "UNSUPPORTED_CAPABILITY",
        "AUTH_REQUIRED",
        "CONNECTION_CHANGED",
        "PERMISSION_DENIED",
        "MODEL_ERROR",
        "MALFORMED_RESPONSE",
        "TASK_FAILED",
        "INVALID_PROPOSAL",
        "STALE_DOCUMENT",
        "DEPENDENCY_CHANGED",
        "INVALID_CANDIDATE",
        "PROFILE_UNSUPPORTED",
        "REPORT_MISMATCH",
        "NOT_READY",
        "NOT_FOUND",
        "IDEMPOTENCY_CONFLICT",
        "REQUEST_EXPIRED",
        "RESOURCE_EXHAUSTED",
        "TIMEOUT",
        "CANCELLED",
        "TRANSPORT_ERROR",
        "OUTCOME_UNKNOWN",
        "INTERNAL_ERROR",
    ];
    assert_eq!(codes.len(), 26);
    for code in codes {
        let base = json!({"code":code,"message":"safe","retry":"never","details":{}});
        let parsed = serde_json::from_value::<WireError>(base.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), base);
        let details = match code {
            "INCOMPATIBLE_VERSION" => json!({"supported":["1.0"]}),
            "INVALID_REQUEST" | "METHOD_NOT_FOUND" => json!({"field":"x","method":"m"}),
            "UNSUPPORTED_CAPABILITY" => json!({"capability":"tools"}),
            "AUTH_REQUIRED" | "CONNECTION_CHANGED" | "MODEL_ERROR" | "MALFORMED_RESPONSE" | "TASK_FAILED" | "INVALID_PROPOSAL" => {
                json!({"connectionId":"c"})
            }
            "PERMISSION_DENIED" => json!({"permissionId":"p"}),
            "STALE_DOCUMENT" => json!({"expected":{"documentId":"d","revision":"18446744073709551615"},"current":null}),
            "DEPENDENCY_CHANGED" | "INVALID_CANDIDATE" => json!({"candidateId":"c"}),
            "PROFILE_UNSUPPORTED" | "REPORT_MISMATCH" => json!({"profileId":"p"}),
            "NOT_READY" | "NOT_FOUND" => json!({"artifactId":"a"}),
            "IDEMPOTENCY_CONFLICT" | "REQUEST_EXPIRED" => json!({"submissionRequestId":"s"}),
            "RESOURCE_EXHAUSTED" => json!({"resource":"r","limit":1}),
            _ => json!({"operation":"request.cancel"}),
        };
        let mut nonempty = base.clone();
        nonempty["details"] = details;
        assert!(serde_json::from_value::<WireError>(nonempty.clone()).is_ok(), "{code}");
        let keys: Vec<_> = nonempty["details"].as_object().unwrap().keys().cloned().collect();
        for key in keys {
            if nonempty["details"][&key].is_null() {
                continue;
            }
            let mut bad = nonempty.clone();
            bad["details"][key] = Value::Null;
            assert!(serde_json::from_value::<WireError>(bad).is_err(), "optional null {code}");
        }
        nonempty["details"]["unknown"] = json!(0);
        assert!(serde_json::from_value::<WireError>(nonempty).is_err(), "{code}");
        let mut oversized = base;
        oversized["message"] = json!("é".repeat(1025));
        assert!(serde_json::from_value::<WireError>(oversized).is_err());
    }
    assert!(serde_json::from_value::<WireError>(json!({"code":"FUTURE","message":"safe","retry":"never","details":{}})).is_err());
}

#[test]
fn mastra_codec_field_limits_and_model_rounds() {
    use super::mastra_protocol::*;
    let catalog = mastra_codec_catalog().0;
    let params = catalog.iter().find(|m| m.0 == "model.generate").unwrap().1.clone();
    for (pointer, value) in [
        ("/options/timeoutSeconds", json!(0)),
        ("/options/timeoutSeconds", json!(2147483648_u64)),
        ("/options/maxOutputTokens", json!(0)),
        ("/options/maxOutputTokens", json!(131073)),
        ("/options/temperature", json!(-0.01)),
        ("/options/temperature", json!(2.01)),
        ("/options/reasoningEffort", json!("é".repeat(33))),
        ("/messages/0/content/0/text", json!("é".repeat(131073))),
        ("/messages", json!(vec![params["messages"][0].clone(); 129])),
        ("/tools", json!(vec![params["tools"][0].clone(); 65])),
        ("/tools/0/inputSchema", json!({"s":"x".repeat(65529)})),
        ("/options/outputSchema", json!({"s":"x".repeat(65529)})),
    ] {
        let mut bad = params.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            matches!(mastra_codec_decode(&mastra_codec_request("model.generate", bad, false), false), Ok(DecodeResult::RequestError { .. })),
            "accepted {pointer}"
        );
    }
    let mut edge = params.clone();
    edge["options"]["temperature"] = json!(2);
    edge["options"]["maxOutputTokens"] = json!(131072);
    edge["options"]["reasoningEffort"] = json!("é".repeat(32));
    edge["options"]["outputSchema"] = json!({"s":"x".repeat(65528)});
    assert!(matches!(mastra_codec_decode(&mastra_codec_request("model.generate", edge, false), false), Ok(DecodeResult::Frame(_))));
    let assistant = json!({"role":"assistant","content":[],"toolCalls":[{"id":"call","name":"m","arguments":{}}],"toolCallId":null});
    let tool = json!({"role":"tool","content":[{"type":"text","text":"result"}],"toolCalls":[],"toolCallId":"call"});
    for (messages, valid) in [
        (json!([assistant.clone(), tool.clone()]), true),
        (json!([tool.clone()]), false),
        (json!([assistant.clone(), tool.clone(), tool.clone()]), false),
        (json!([assistant.clone(),{"role":"user","content":[],"toolCalls":[],"toolCallId":null}]), false),
        (json!([{ "role":"user","content":[],"toolCalls":[{"id":"call","name":"m","arguments":{}}],"toolCallId":null}]), false),
        (json!([{ "role":"assistant","content":[],"toolCalls":[],"toolCallId":"call"}]), false),
    ] {
        let mut p = params.clone();
        p["messages"] = messages;
        assert_eq!(matches!(mastra_codec_decode(&mastra_codec_request("model.generate", p, false), false), Ok(DecodeResult::Frame(_))), valid);
    }
    let mut result = catalog.iter().find(|m| m.0 == "model.generate").unwrap().2.clone();
    result["usage"] = json!({"inputTokens":null,"outputTokens":null,"totalTokens":null});
    for key in ["inputTokens", "outputTokens", "totalTokens"] {
        let mut missing = result.clone();
        missing["usage"].as_object_mut().unwrap().remove(key);
        assert!(decode_host_reply("model.generate", missing, &Limits::default()).is_err());
        let mut large = result.clone();
        large["usage"][key] = json!(2147483648_u64);
        assert!(decode_host_reply("model.generate", large, &Limits::default()).is_err());
    }
    for name in ["model.generate", "agent.run"] {
        let mut r = catalog.iter().find(|m| m.0 == name).unwrap().2.clone();
        r["content"] = json!([{"type":"image","artifact":{"artifactId":"a","digest":format!("sha256:{}","0".repeat(64)),"sizeBytes":1,"mediaType":"image/png"}}]);
        assert!(decode_host_reply(name, r, &Limits::default()).is_err());
    }
}

#[test]
fn mastra_codec_referenced_data_and_attachment_bounds() {
    use super::mastra_protocol::*;
    let digest = format!("sha256:{}", "0".repeat(64));
    let manifest = json!({"schemaVersion":"1.0","candidateId":"c","source":{"documentId":"d","revision":"18446744073709551615"},"capturedAt":"2024-02-29T23:59:59.999Z","formatVersion":"1","engineVersion":"1","inputs":[{"path":"design.sc","kind":"design","dependencyId":"d","dependencyVersion":null,"digest":digest,"sizeBytes":1}]});
    let parsed: SnapshotManifest = serde_json::from_value(manifest.clone()).unwrap();
    assert!(parsed.validate(&Limits::default()).is_ok());
    for path in ["/a.sc", "a/../b.sc", "a//b.sc", "a\\b.sc", "./a.sc", &"a".repeat(241)] {
        let mut bad = manifest.clone();
        bad["inputs"][0]["path"] = json!(path);
        assert!(serde_json::from_value::<SnapshotManifest>(bad).unwrap().validate(&Limits::default()).is_err(), "{path}");
    }
    let mut missing = manifest.clone();
    missing["inputs"][0].as_object_mut().unwrap().remove("dependencyVersion");
    assert!(serde_json::from_value::<SnapshotManifest>(missing).is_err());
    for text in ["2023-02-29T12:00:00.000Z", "2026-11-05T12:00:00Z", "2026-11-05T12:00:00.000+00:00", "2026-11-05T24:00:00.000Z"] {
        assert!(Timestamp::parse(text).is_err());
    }
    let write = json!({"state":"unknown","request":{"bridgeId":"b","mcpSessionId":"s","requestId":"r"},"before":{"documentId":"d","revision":"18446744073709551615"},"after":null,"completed":null,"rollback":{"requested":false,"attempted":false,"restored":null,"undone":null},"expectation":null,"error":null});
    assert!(serde_json::from_value::<WriteOutcome>(write.clone()).is_ok());
    for key in ["after", "completed", "expectation", "error"] {
        let mut bad = write.clone();
        bad.as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<WriteOutcome>(bad).is_err());
    }
    for key in ["restored", "undone"] {
        let mut bad = write.clone();
        bad["rollback"].as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<WriteOutcome>(bad).is_err());
    }
    let mut params = mastra_codec_catalog().0.iter().find(|m| m.0 == "agent.run").unwrap().1.clone();
    let artifact = json!({"artifactId":"a","digest":digest,"sizeBytes":4194304,"mediaType":"image/png"});
    for (task, valid) in [
        (json!([{"type":"image","artifact":artifact.clone()}]), true),
        (json!(vec![json!({"type":"image","artifact":artifact.clone()}); 5]), false),
        (json!([{"type":"document","name":"a","artifact":artifact.clone()}]), false),
    ] {
        params["task"] = task;
        assert_eq!(
            matches!(mastra_codec_decode(&mastra_codec_request("agent.run", params.clone(), false), false), Ok(DecodeResult::Frame(_))),
            valid
        );
    }
    params["task"] = json!([{"type":"image","artifact":artifact}]);
    params["task"][0]["artifact"]["sizeBytes"] = json!(4194305);
    assert!(matches!(mastra_codec_decode(&mastra_codec_request("agent.run", params, false), false), Ok(DecodeResult::RequestError { .. })));
}

#[test]
fn mastra_codec_local_encoding_and_progress_are_strict() {
    use super::mastra_protocol::*;
    let params = mastra_codec_catalog().0.iter().find(|m| m.0 == "model.generate").unwrap().1.clone();
    let request = mastra_codec_request("model.generate", params, false);
    let DecodeResult::Frame(mut frame) = mastra_codec_decode(&request, false).unwrap() else { panic!("frame") };
    if let Frame::Request { body: RequestBody::Runtime(HostCall::ModelGenerate(p)), .. } = &mut frame {
        p.options.temperature = RequiredNullable(Some(f64::NAN));
    }
    assert!(encode_frame(&frame, &Limits::default()).is_err(), "do not silently serialize NaN as null");
    for (kind, content) in [
        ("text", json!({"text":"hi"})),
        ("tool", json!({"callId":"c","title":"Measure","state":"completed"})),
        ("plan", json!({"steps":[{"id":"s","title":"Build","state":"pending"}]})),
        ("permission", json!({"permissionId":"p","title":"Allow?","state":"denied"})),
    ] {
        let good = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope("agent.progress"),"sequence":1,"requestId":"r:c","event":"agent.progress","data":{"agentSessionId":"as","kind":kind,"content":content}});
        assert!(mastra_codec_decode(&good, true).is_ok());
        let mut bad = good.clone();
        bad["data"]["content"]["extra"] = json!(0);
        assert!(mastra_codec_decode(&bad, true).is_err());
        let mut bad = good;
        bad["data"]["kind"] = json!("future");
        assert!(mastra_codec_decode(&bad, true).is_err());
    }
    let mut response = json!({"protocolVersion":"1.0","kind":"response","scope":mastra_codec_scope("turn.cancel"),"requestId":"h:c","outcome":"ok","result":{"state":"cancelling"}});
    let DecodeResult::Frame(Frame::Response { body: ResponseBody::Ok(pending), .. }) = mastra_codec_decode(&response, false).unwrap() else {
        panic!("pending")
    };
    assert!(pending.decode_runtime("session.start", &Limits::default()).is_err(), "origin method required");
    response["method"] = json!("turn.cancel");
    assert!(mastra_codec_decode(&response, false).is_err());
    let mut control = mastra_codec_request("request.cancel", json!({"targetRequestId":"r:_1"}), false);
    assert!(matches!(mastra_codec_decode(&control, false), Ok(DecodeResult::Frame(_))));
    control["params"]["targetRequestId"] = json!("h:_1");
    assert!(matches!(mastra_codec_decode(&control, false), Ok(DecodeResult::RequestError { .. })));
    for details in [json!({"expected":{"documentId":"d","revision":"0"}}), json!({"current":null})] {
        assert!(serde_json::from_value::<WireError>(json!({"code":"STALE_DOCUMENT","message":"safe","retry":"never","details":details})).is_err());
    }
    let limits = Limits { tool_calls: 1, ..Limits::default() };
    let mut result = mastra_codec_catalog().0.iter().find(|m| m.0 == "model.generate").unwrap().2.clone();
    result["toolCalls"] = json!([{"id":"c1","name":"m","arguments":{}},{"id":"c2","name":"m","arguments":{}}]);
    assert!(decode_host_reply("model.generate", result, &limits).is_err());
}

#[test]
fn mastra_codec_profiles_and_control_capacity() {
    use super::mastra_protocol::*;
    let profile = json!({"schemaVersion":"1.0","profileId":"p","version":"1","checks":[{"checkId":"p","kind":"parameters.required","required":true,"parameters":{"requirements":[{"name":"width","value":-1.25,"absTolerance":0.01}]}}]});
    let parsed: CheckProfile = serde_json::from_value(profile.clone()).unwrap();
    assert!(parsed.validate(&Limits::default()).is_ok());
    for parameters in [
        json!({"requirements":[]}),
        json!({"requirements":[{"name":"width","value":1,"absTolerance":-0.01}]}),
        json!({"requirements":[{"name":"width","value":1,"absTolerance":0,"extra":true}]}),
    ] {
        let mut p = profile.clone();
        p["checks"][0]["parameters"] = parameters;
        assert!(serde_json::from_value::<CheckProfile>(p).unwrap().validate(&Limits::default()).is_err());
    }
    let long = "x".repeat(128);
    let response = json!({"protocolVersion":"1.0","kind":"response","scope":{"sessionId":long,"turnId":long,"workflowRunId":long},"requestId":format!("h:{}","x".repeat(126)),"outcome":"error","error":{"code":"CANCELLED","message":"x".repeat(2048),"retry":"never","details":{"operation":"request.cancel"}}});
    let DecodeResult::Frame(frame) = mastra_codec_decode(&response, false).unwrap() else { panic!("control") };
    assert_eq!(encode_control_frame(&frame, &Limits::default()).unwrap().len(), 2792);
    assert_eq!(minimum_control_frame_bytes(), 2792);
    for code in [ErrorCode::InvalidRequest, ErrorCode::ResourceExhausted] {
        let wire = json!({"protocolVersion":"1.0","kind":"response","scope":mastra_codec_scope("turn.cancel"),"requestId":"r:cancel","outcome":"error","error":{"code":code,"message":"Invalid cancel state.","retry":"never","details":{}}});
        let DecodeResult::Frame(frame) = mastra_codec_decode(&wire, true).unwrap() else { panic!("control method error") };
        let limits = Limits { frame_bytes: 3000, queue_bytes: 6000, control_lane_bytes: 3000, ..Limits::default() };
        let bytes = encode_control_frame(&frame, &limits).unwrap();
        assert!(bytes.len() <= 3000);
        assert!(matches!(decode_frame(&bytes, DecodePhase::Negotiated, Sender::Host, &limits), Ok(DecodeResult::Frame(_))));
    }
    let mut escaped = response;
    escaped["error"]["message"] = json!("\u{0001}".repeat(2048));
    let DecodeResult::Frame(frame) = mastra_codec_decode(&escaped, false).unwrap() else { panic!("peer error") };
    assert!(encode_frame(&frame, &Limits::default()).unwrap().len() > 4096);
    assert!(encode_control_frame(&frame, &Limits::default()).is_err(), "remote escaped text is not a 4096 byte guarantee");
}

#[test]
fn mastra_codec_nested_caps_extensions_and_redaction() {
    use super::mastra_protocol::*;
    let (methods, _) = mastra_codec_catalog();
    let mut model = methods.iter().find(|m| m.0 == "model.generate").unwrap().1.clone();
    let mut call = json!({"id":"c","name":"measure","arguments":{"x":"x".repeat(65528)}});
    let mut result = methods.iter().find(|m| m.0 == "model.generate").unwrap().2.clone();
    result["toolCalls"] = json!([call.clone()]);
    assert!(decode_host_reply("model.generate", result.clone(), &Limits::default()).is_ok());
    call["arguments"]["x"] = json!("x".repeat(65529));
    result["toolCalls"] = json!([call]);
    assert!(decode_host_reply("model.generate", result, &Limits::default()).is_err());
    model["messages"][0]["content"][0]["text"] = json!("é".repeat(131072));
    assert!(matches!(mastra_codec_decode(&mastra_codec_request("model.generate", model, false), false), Ok(DecodeResult::Frame(_))));
    let base = mastra_codec_request("turn.cancel", json!({"reason":"user"}), true);
    for (extensions, valid) in [
        (json!({"negative":-1.25,"large":9007199254740992_u64,"toolCalls":[{"not":"a call"}]}), true),
        (json!([]), false),
        (json!(1), false),
        (Value::Null, false),
    ] {
        let mut value = base.clone();
        value["extensions"] = extensions;
        assert_eq!(mastra_codec_decode(&value, true).is_ok(), valid);
    }
    let mut invalid = base;
    invalid["params"]["reason"] = json!("private-provider-body-secret");
    let DecodeResult::RequestError { error, .. } = mastra_codec_decode(&invalid, true).unwrap() else { panic!("params") };
    assert!(!format!("{error:?} {error} {}", serde_json::to_string(&error).unwrap()).contains("private-provider-body-secret"));
    let mut plan = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope("workflow.plan"),"sequence":1,"requestId":null,"event":"workflow.plan","data":{"steps":(0..64).map(|i|json!({"id":format!("s{i}"),"title":"step","state":"pending"})).collect::<Vec<_>>()}});
    assert!(mastra_codec_decode(&plan, false).is_ok());
    plan["data"]["steps"].as_array_mut().unwrap().push(json!({"id":"s64","title":"step","state":"pending"}));
    assert!(mastra_codec_decode(&plan, false).is_err());
    let error: WireError = serde_json::from_value(
        json!({"code":"MODEL_ERROR","message":"peer-supplied-secret","retry":"read_only","details":{"connectionId":"c","httpStatus":2147483647}}),
    )
    .unwrap();
    assert!(!format!("{error:?} {error}").contains("peer-supplied-secret"));
    for details in [
        json!({"connectionId":"c","httpStatus":2147483648_u64}),
        json!({"connectionId":"c","httpStatus":null}),
        json!({"connectionId":"c","httpStatus":-1}),
    ] {
        assert!(serde_json::from_value::<WireError>(json!({"code":"MODEL_ERROR","message":"safe","retry":"never","details":details})).is_err());
    }
}

#[test]
fn mastra_codec_bootstrap_refusal_preserves_version_string() {
    use super::mastra_protocol::*;
    let value = json!({"protocolVersion":"future","kind":"response","scope":mastra_codec_scope("runtime.hello"),"requestId":"h:","outcome":"error","error":{"code":"INCOMPATIBLE_VERSION","message":"No common version.","retry":"never","details":{"supported":["1.0"]}}});
    let bytes = format!("{value}\n").into_bytes();
    let DecodeResult::Frame(frame) = decode_frame(&bytes, DecodePhase::Bootstrap, Sender::Runtime, &Limits::default()).unwrap() else {
        panic!("refusal")
    };
    assert_eq!(serde_json::from_slice::<Value>(&encode_frame(&frame, &Limits::default()).unwrap()).unwrap(), value);
    assert!(decode_frame(&bytes, DecodePhase::Negotiated, Sender::Runtime, &Limits::default()).is_err());
}

#[test]
fn mastra_codec_text_limit_lowering_preserves_control_metadata() {
    use super::mastra_protocol::*;
    let offered = Limits::default();
    let returned = Limits { text_bytes: 1, ..offered.clone() };
    assert!(validate_limits(&returned).is_ok());
    let limits = negotiate_limits(&offered, &returned).unwrap();
    assert_eq!(limits.text_bytes, 1, "do not mask the bug by raising a negotiated minimum");
    let long_id = "x".repeat(128);
    let long_request_id = format!("h:{}", "_".repeat(126));
    for (method, params) in [
        ("turn.cancel", json!({"reason":"user"})),
        ("turn.cancel", json!({"reason":"reset"})),
        ("turn.cancel", json!({"reason":"deadline"})),
        ("turn.cancel", json!({"reason":"shutdown"})),
        ("request.cancel", json!({"targetRequestId":long_request_id})),
        ("runtime.shutdown", json!({"reason":"app_exit"})),
        ("runtime.shutdown", json!({"reason":"reconfigure"})),
        ("runtime.shutdown", json!({"reason":"protocol_error"})),
    ] {
        let mut value = mastra_codec_request(method, params, true);
        value["requestId"] = json!(long_request_id);
        if method != "runtime.shutdown" {
            value["scope"] = json!({"sessionId":long_id,"turnId":long_id,"workflowRunId":long_id});
        }
        let bytes = format!("{value}\n").into_bytes();
        let DecodeResult::Frame(frame) = decode_frame(&bytes, DecodePhase::Negotiated, Sender::Host, &offered).unwrap() else {
            panic!("default control")
        };
        assert!(encode_control_frame(&frame, &limits).is_ok(), "typed {method} must still encode at textBytes=1");
        let decoded = decode_frame(&bytes, DecodePhase::Negotiated, Sender::Host, &limits).unwrap();
        assert!(matches!(decoded, DecodeResult::Frame(_)), "{method} metadata is not a text block: {decoded:?}");
    }
    let (methods, events) = mastra_codec_catalog();
    for (method, mut params, mut result, host) in methods {
        // Only real text blocks are shortened. Instructions, labels, versions,
        // states, model IDs, media types and digest strings keep their own rules.
        let body_path = match method.as_str() {
            "turn.start" => Some("/parts/0/text"),
            "model.generate" => Some("/messages/0/content/0/text"),
            "agent.run" => Some("/task/0/text"),
            _ => None,
        };
        if let Some(path) = body_path {
            *params.pointer_mut(path).unwrap() = json!("x");
        }
        if matches!(method.as_str(), "model.generate" | "agent.run") {
            result["content"][0]["text"] = json!("x");
        }
        if method == "runtime.hello" {
            params["limits"] = json!(returned);
            result["limits"] = json!(returned);
        }
        if method == "turn.start" {
            params["binding"]["document"]["documentId"] = json!(long_id);
            params["binding"]["profile"]["profileId"] = json!(long_id);
            params["binding"]["profile"]["version"] = json!("1.0.0-stable");
        }
        let value = mastra_codec_request(&method, params, host);
        let bytes = format!("{value}\n").into_bytes();
        let sender = if host { Sender::Host } else { Sender::Runtime };
        let DecodeResult::Frame(frame) = decode_frame(&bytes, DecodePhase::Negotiated, sender, &limits).unwrap() else {
            panic!("metadata incorrectly capped in {method}")
        };
        assert_eq!(serde_json::from_slice::<Value>(&encode_frame(&frame, &limits).unwrap()).unwrap(), value, "{method}");
        if host {
            assert!(decode_runtime_reply(&method, result, &limits).is_ok(), "reply {method}");
        } else {
            assert!(decode_host_reply(&method, result, &limits).is_ok(), "reply {method}");
        }
    }
    for (event, mut data, host) in events {
        if matches!(event.as_str(), "workflow.text" | "model.delta") {
            data["text"] = json!("x");
        }
        let value = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope(&event),"sequence":1,"requestId":null,"event":event,"data":data});
        let sender = if host { Sender::Host } else { Sender::Runtime };
        let DecodeResult::Frame(frame) = decode_frame(format!("{value}\n").as_bytes(), DecodePhase::Negotiated, sender, &limits).unwrap() else {
            panic!("event")
        };
        assert_eq!(serde_json::from_slice::<Value>(&encode_frame(&frame, &limits).unwrap()).unwrap(), value, "event {event}");
    }
    assert!(Id::parse(&long_id).is_ok());
    assert!(Id::parse(&"x".repeat(129)).is_err());
    assert!(Digest::parse(&format!("sha256:{}", "a".repeat(64))).is_ok());
    assert!(Digest::parse(&format!("sha256:{}", "a".repeat(65))).is_err());
}

#[test]
fn mastra_codec_text_limit_checks_typed_bodies_without_opaque_loopholes() {
    use super::mastra_protocol::*;
    let limits = negotiate_limits(&Limits::default(), &Limits { text_bytes: 1, ..Limits::default() }).unwrap();
    let (methods, _) = mastra_codec_catalog();
    for (method, path) in [("turn.start", "/parts/0/text"), ("model.generate", "/messages/0/content/0/text"), ("agent.run", "/task/0/text")] {
        for (text, valid) in [("x", true), ("é", false)] {
            let mut params = methods.iter().find(|m| m.0 == method).unwrap().1.clone();
            *params.pointer_mut(path).unwrap() = json!(text);
            if method == "model.generate" {
                params["tools"][0]["inputSchema"] = json!({"text":"long opaque schema string","type":"object"});
                params["options"]["outputSchema"] = json!({"content":[{"type":"text","text":"not a protocol block"}]});
            }
            let host = method == "turn.start";
            let value = mastra_codec_request(method, params, host);
            let sender = if host { Sender::Host } else { Sender::Runtime };
            let bytes = format!("{value}\n").into_bytes();
            let decoded = decode_frame(&bytes, DecodePhase::Negotiated, sender, &limits).unwrap();
            assert_eq!(matches!(decoded, DecodeResult::Frame(_)), valid, "{method}: {text:?}");
            if let DecodeResult::RequestError { error, .. } = decoded {
                assert_eq!(serde_json::to_value(error).unwrap()["code"], "INVALID_REQUEST");
            }
            let DecodeResult::Frame(frame) = decode_frame(&bytes, DecodePhase::Negotiated, sender, &Limits::default()).unwrap() else {
                panic!("default")
            };
            assert_eq!(encode_frame(&frame, &limits).is_ok(), valid, "encode {method}: {text:?}");
        }
    }
    for name in ["model.generate", "agent.run"] {
        for (text, valid) in [("x", true), ("é", false)] {
            let mut result = methods.iter().find(|m| m.0 == name).unwrap().2.clone();
            result["content"][0]["text"] = json!(text);
            assert_eq!(decode_host_reply(name, result.clone(), &limits).is_ok(), valid, "result {name}");
            let typed = decode_host_reply(name, result, &Limits::default()).unwrap();
            assert_eq!(ResponseBody::host_ok(typed, &limits).is_ok(), valid, "typed result {name}");
        }
    }
    for (event, data, host) in [
        ("workflow.text", json!({"text":"x","channel":"answer"}), false),
        ("model.delta", json!({"index":0,"text":"x"}), true),
        ("agent.progress", json!({"agentSessionId":"as","kind":"text","content":{"text":"x"}}), true),
    ] {
        for (text, valid) in [("x", true), ("é", false)] {
            let mut data = data.clone();
            if event == "agent.progress" {
                data["content"]["text"] = json!(text);
            } else {
                data["text"] = json!(text);
            }
            let value = json!({"protocolVersion":"1.0","kind":"event","scope":mastra_codec_scope(event),"sequence":1,"requestId":null,"event":event,"data":data});
            let bytes = format!("{value}\n").into_bytes();
            let sender = if host { Sender::Host } else { Sender::Runtime };
            assert_eq!(decode_frame(&bytes, DecodePhase::Negotiated, sender, &limits).is_ok(), valid, "event {event}");
            let DecodeResult::Frame(frame) = decode_frame(&bytes, DecodePhase::Negotiated, sender, &Limits::default()).unwrap() else {
                panic!("event")
            };
            assert_eq!(encode_frame(&frame, &limits).is_ok(), valid, "encode event {event}");
        }
    }
    let mut opaque = methods.iter().find(|m| m.0 == "model.generate").unwrap().2.clone();
    opaque["content"][0]["text"] = json!("x");
    opaque["toolCalls"][0]["arguments"] = json!({"type":"text","text":"long opaque argument","x":-1.25});
    assert!(decode_host_reply("model.generate", opaque, &limits).is_ok());
    let too_long: WorkflowText = serde_json::from_value(json!({"text":"é","channel":"progress"})).unwrap();
    assert!(too_long.validate(&limits).is_err(), "direct DTO validation also enforces its text field");
}

#[test]
fn mastra_codec_text_limit_keeps_error_and_reasoning_bounds_independent() {
    use super::mastra_protocol::*;
    let limits = negotiate_limits(&Limits::default(), &Limits { text_bytes: 1, ..Limits::default() }).unwrap();
    let (methods, _) = mastra_codec_catalog();
    let mut model = methods.iter().find(|m| m.0 == "model.generate").unwrap().1.clone();
    model["messages"][0]["content"][0]["text"] = json!("x");
    for (bytes, valid) in [(64, true), (65, false)] {
        model["options"]["reasoningEffort"] = json!("x".repeat(bytes));
        let value = mastra_codec_request("model.generate", model.clone(), false);
        assert_eq!(
            matches!(decode_frame(format!("{value}\n").as_bytes(), DecodePhase::Negotiated, Sender::Runtime, &limits), Ok(DecodeResult::Frame(_))),
            valid,
            "reasoning {bytes}"
        );
    }
    for (bytes, valid) in [(2048, true), (2049, false)] {
        let mut result = methods.iter().find(|m| m.0 == "validation.report").unwrap().2.clone();
        result["report"]["error"] = json!({"code":"CANCELLED","message":"x".repeat(bytes),"retry":"never","details":{"operation":"request.cancel"}});
        assert_eq!(decode_host_reply("validation.report", result, &limits).is_ok(), valid, "nested error {bytes}");
    }
}

#[test]
fn mastra_values_request_id_parser_uses_whole_id_and_sender() {
    use super::mastra_protocol::{RequestId, Sender};
    for (sender, prefix, other_prefix) in [(Sender::Host, "h:", "r:"), (Sender::Runtime, "r:", "h:")] {
        for suffix in ["_1", ".1", ":1", "-1", ""] {
            let text = format!("{prefix}{suffix}");
            let parsed = RequestId::parse(&text, sender);
            assert!(parsed.is_ok(), "whole Id {text:?} must be accepted: {parsed:?}");
            assert_eq!(parsed.unwrap().as_str(), text);
            assert!(RequestId::parse(&format!("{other_prefix}{suffix}"), sender).is_err(), "wrong sender");
        }
        let longest = format!("{prefix}{}", "_".repeat(126));
        assert_eq!(longest.len(), 128);
        assert_eq!(RequestId::parse(&longest, sender).unwrap().as_str(), longest);
        let oversized = format!("{prefix}{}", "_".repeat(127));
        assert_eq!(oversized.len(), 129);
        assert!(RequestId::parse(&oversized, sender).is_err());
        for text in ["call", "h:/secret", "r:bad space", "h:é", "r:\n", "H:_1", "r_1"] {
            assert!(RequestId::parse(text, sender).is_err(), "{text:?}");
        }
    }
}

#[test]
fn mastra_values_request_id_serde_uses_whole_id() {
    use super::mastra_protocol::RequestId;
    for prefix in ["h:", "r:"] {
        for suffix in ["_1", ".1", ":1", "-1", "", &"_".repeat(126)] {
            let text = format!("{prefix}{suffix}");
            let parsed = serde_json::from_str::<RequestId>(&serde_json::to_string(&text).unwrap());
            assert!(parsed.is_ok(), "serde must accept whole Id {text:?}: {parsed:?}");
            let id = parsed.unwrap();
            assert_eq!(id.as_str(), text);
            assert_eq!(serde_json::to_value(id).unwrap(), json!(text));
        }
        let oversized = format!("{prefix}{}", "_".repeat(127));
        assert!(serde_json::from_str::<RequestId>(&serde_json::to_string(&oversized).unwrap()).is_err());
    }
    for value in [json!("call"), json!("h:/secret"), json!("r:bad space"), json!("H:_1"), json!("r_1"), json!(0), Value::Null] {
        assert!(serde_json::from_value::<RequestId>(value).is_err());
    }
}

#[test]
fn mastra_values_revision_round_trips() {
    use super::mastra_protocol::Revision;
    for (text, native) in [("0", 0), ("9007199254740993", 9_007_199_254_740_993), ("18446744073709551615", u64::MAX)] {
        let revision = Revision::parse(text).unwrap();
        assert_eq!(revision.as_u64(), native);
        assert_eq!(serde_json::to_value(revision).unwrap(), json!(text));
        assert_eq!(serde_json::from_value::<Revision>(json!(text)).unwrap(), Revision::from_u64(native));
    }
    for text in ["", "00", "01", "-1", "+1", " 1", "1 ", "1.0", "1e2", "１２", "18446744073709551616"] {
        assert!(Revision::parse(text).is_err(), "{text:?}");
        assert!(serde_json::from_value::<Revision>(json!(text)).is_err(), "{text:?}");
    }
    for value in [json!(0), json!(9_007_199_254_740_993_u64), json!(u64::MAX), json!(-1), json!(1.0), Value::Null] {
        assert!(serde_json::from_value::<Revision>(value).is_err());
    }
}

#[test]
fn mastra_values_identity_and_scope() {
    use super::mastra_protocol::{Digest, Id, RequestId, Scope, Sender, TurnScope};
    for text in ["A", "0", "a.b_c:d-e", &"x".repeat(128)] {
        let id = Id::parse(text).unwrap();
        assert_eq!(id.as_str(), text);
        assert_eq!(serde_json::to_value(&id).unwrap(), json!(text));
        assert_eq!(serde_json::from_value::<Id>(json!(text)).unwrap(), id);
    }
    for text in ["", ".a", "_a", ":a", "-a", "a/b", "a b", "a\n", "é", &"x".repeat(129)] {
        assert!(Id::parse(text).is_err(), "{text:?}");
        assert!(serde_json::from_value::<Id>(json!(text)).is_err());
    }
    for (sender, good, bad) in [(Sender::Host, "h:call", "r:call"), (Sender::Runtime, "r:call", "h:call")] {
        let id = RequestId::parse(good, sender).unwrap();
        assert_eq!(id.as_str(), good);
        assert_eq!(serde_json::to_value(&id).unwrap(), json!(good));
        assert_eq!(serde_json::from_value::<RequestId>(json!(good)).unwrap(), id);
        for text in [bad, "call", "h:/secret"] {
            assert!(RequestId::parse(text, sender).is_err(), "{text:?}");
        }
    }
    for text in ["call", "h:/secret"] {
        assert!(serde_json::from_value::<RequestId>(json!(text)).is_err());
    }
    let digest_text = format!("sha256:{}", "0123456789abcdef".repeat(4));
    let digest = Digest::parse(&digest_text).unwrap();
    assert_eq!(digest.bytes(), &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(4).as_slice());
    assert_eq!(serde_json::to_value(&digest).unwrap(), json!(digest_text));
    assert_eq!(serde_json::from_value::<Digest>(json!(digest_text)).unwrap(), digest);
    for text in [
        digest_text.to_uppercase(),
        format!("sha256:{}", "A".repeat(64)),
        "0".repeat(64),
        format!("sha256:{}", "0".repeat(63)),
        format!("sha256:{}", "0".repeat(65)),
        format!("sha256:{}g", "0".repeat(63)),
    ] {
        assert!(Digest::parse(&text).is_err());
    }
    for (scope, expected) in [
        (Scope::Runtime, json!({"sessionId":null,"turnId":null,"workflowRunId":null})),
        (Scope::Session(Id::parse("s").unwrap()), json!({"sessionId":"s","turnId":null,"workflowRunId":null})),
        (
            Scope::Turn(TurnScope {
                session_id: Id::parse("s").unwrap(),
                turn_id: Id::parse("t").unwrap(),
                workflow_run_id: Id::parse("w").unwrap(),
            }),
            json!({"sessionId":"s","turnId":"t","workflowRunId":"w"}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&scope).unwrap(), expected);
        assert_eq!(serde_json::from_value::<Scope>(expected.clone()).unwrap(), scope);
        for key in ["sessionId", "turnId", "workflowRunId"] {
            let mut missing = expected.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<Scope>(missing).is_err(), "missing {key}");
        }
    }
    for (session, turn, workflow) in
        [(None, Some("t"), None), (None, None, Some("w")), (None, Some("t"), Some("w")), (Some("s"), Some("t"), None), (Some("s"), None, Some("w"))]
    {
        assert!(serde_json::from_value::<Scope>(json!({"sessionId":session,"turnId":turn,"workflowRunId":workflow})).is_err());
    }
    for raw in [
        r#"{"turnId":null,"workflowRunId":null}"#,
        r#"{"sessionId":null,"workflowRunId":null}"#,
        r#"{"sessionId":null,"turnId":null}"#,
        r#"{"sessionId":null,"turnId":null,"workflowRunId":null,"extra":0}"#,
        r#"{"sessionId":null,"sessionId":"s","turnId":null,"workflowRunId":null}"#,
        r#"{"sessionId":null,"turnId":null,"turnId":null,"workflowRunId":null}"#,
        r#"{"sessionId":null,"turnId":null,"workflowRunId":null,"workflowRunId":null}"#,
    ] {
        assert!(serde_json::from_str::<Scope>(raw).is_err(), "{raw}");
    }
}

#[test]
fn mastra_values_negotiation_cannot_raise_or_disable_limits() {
    use super::mastra_protocol::{
        Budgets, CONTROL_FRAME_BYTES, Limits, minimum_control_frame_bytes, negotiate_limits, validate_budgets, validate_limits,
    };
    let offered = Limits::default();
    assert!(validate_limits(&offered).is_ok());
    assert_eq!(negotiate_limits(&offered, &offered).unwrap(), offered);
    let encoded = serde_json::to_value(&offered).unwrap();
    // Independent contract ceilings: a wrong default must not move the tested boundary too.
    let ceilings: Value = serde_json::from_str(
        r#"{
        "frameBytes":8388608,"jsonDepth":64,"queueBytes":16777216,"controlLaneBytes":65536,
        "pendingRequests":32,"controlSlots":1,"requestLedgerEntries":4096,
        "runtimeInstances":8,"sessions":1,"activeTurns":1,
        "stderrLines":40,"stderrLineScalars":400,"stderrBytes":65536,
        "modelMessages":128,"toolDefinitions":64,"toolCalls":32,"textBytes":262144,"toolJsonBytes":65536,
        "attachments":4,"attachmentBytes":4194304,"attachmentTotalBytes":16777216,
        "historyBytes":16777216,"historyMessages":128,
        "continuationHandles":32,"continuationBytes":16777216,"continuationTtlSeconds":86400,
        "httpPerConnection":2,"httpPerHost":8,"cadWritesInFlight":1,"cadReadsInFlight":4,
        "handshakeSeconds":10,"sessionStartSeconds":30,"callDefaultSeconds":300,"callMaxSeconds":600,
        "cancelAckSeconds":5,"shutdownSeconds":3,
        "validationWorkers":2,"validationQueue":8,"validationSeconds":600,"validationQueueSeconds":300,
        "snapshotBytes":1073741824,"snapshotInputs":256,"manifestBytes":1048576,
        "reportBytes":4194304,"checks":128,"evidenceArtifacts":64,"evidenceBytes":16777216,"evidenceTotalBytes":67108864,
        "storeBytes":"8589934592","jobRecords":10000,"retentionSeconds":604800
    }"#,
    )
    .unwrap();
    assert_eq!(encoded, ceilings);
    for (key, value) in ceilings.as_object().unwrap() {
        let ceiling = if key == "storeBytes" { 8_589_934_592 } else { value.as_u64().unwrap() };
        for invalid in [0, ceiling + 1] {
            let mut changed = encoded.clone();
            changed[key] = if key == "storeBytes" { json!(invalid.to_string()) } else { json!(invalid) };
            let limits: Limits = serde_json::from_value(changed).unwrap();
            assert!(validate_limits(&limits).is_err(), "{key}={invalid}");
            assert!(negotiate_limits(&offered, &limits).is_err());
            assert!(negotiate_limits(&limits, &offered).is_err(), "invalid offer {key}");
        }
        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<Limits>(missing).is_err(), "missing {key}");
    }
    for invalid in [json!(8_589_934_592_u64), json!("08589934592"), json!(-1), Value::Null] {
        let mut changed = encoded.clone();
        changed["storeBytes"] = invalid;
        assert!(serde_json::from_value::<Limits>(changed).is_err());
    }
    for (key, value) in [
        ("queueBytes", json!(65_535)),
        ("queueBytes", json!(8_454_143)),
        ("requestLedgerEntries", json!(74)),
        ("pendingRequests", json!(u32::MAX)),
        ("callMaxSeconds", json!(299)),
        ("httpPerHost", json!(1)),
        ("attachmentTotalBytes", json!(4_194_303)),
        ("evidenceTotalBytes", json!(16_777_215)),
    ] {
        let mut changed = encoded.clone();
        changed[key] = value;
        assert!(validate_limits(&serde_json::from_value(changed).unwrap()).is_err(), "{key}");
    }
    let mut lowered = offered.clone();
    lowered.pending_requests = 1;
    lowered.request_ledger_entries = 13; // Strictly more than 2 * (1 + 1) + 8.
    lowered.frame_bytes = 3000;
    lowered.control_lane_bytes = 3000;
    lowered.queue_bytes = 6000;
    // Cancellation error: 232 fixed JSON/LF bytes + four 128-byte IDs + 2048 text bytes.
    assert_eq!(minimum_control_frame_bytes(), 2792);
    assert!(minimum_control_frame_bytes() <= CONTROL_FRAME_BYTES);
    assert!(validate_limits(&lowered).is_ok(), "do not impose an arbitrary 4096-byte minimum");
    assert_eq!(negotiate_limits(&offered, &lowered).unwrap(), lowered);
    assert!(negotiate_limits(&lowered, &offered).is_err(), "return cannot raise even a below-ceiling offer");
    lowered.frame_bytes = 2792;
    lowered.control_lane_bytes = 2792;
    lowered.queue_bytes = 5584;
    assert!(validate_limits(&lowered).is_ok());
    lowered.frame_bytes = 2791;
    assert!(validate_limits(&lowered).is_err());
    lowered.frame_bytes = 2792;
    lowered.control_lane_bytes = 2791;
    assert!(validate_limits(&lowered).is_err());
    assert!(serde_json::from_str::<Limits>(&format!(r#"{{"frameBytes":1,{}"#, &serde_json::to_string(&offered).unwrap()[1..])).is_err());
    let mut unknown = encoded;
    unknown["extra"] = json!(1);
    assert!(serde_json::from_value::<Limits>(unknown).is_err());

    let budgets = Budgets::default();
    assert!(validate_budgets(&budgets).is_ok());
    let encoded = serde_json::to_value(budgets).unwrap();
    assert_eq!(encoded, json!({"modelCalls":20,"agentTasks":4,"cadCommands":64,"repairs":2,"validationRuns":3,"wallTimeSeconds":1200}));
    let mut zero = encoded.clone();
    for (key, value) in encoded.as_object().unwrap() {
        let mut raised = encoded.clone();
        raised[key] = json!(value.as_u64().unwrap() + 1);
        assert!(validate_budgets(&serde_json::from_value(raised).unwrap()).is_err(), "{key}");
        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<Budgets>(missing).is_err(), "missing {key}");
        zero[key] = json!(0);
    }
    let zero: Budgets = serde_json::from_value(zero).unwrap();
    assert!(validate_budgets(&zero).is_ok(), "zero is admitted, not an invalid request");
    assert_eq!(zero.wall_time_seconds, 0, "later turn execution must see immediate exhaustion, not a default");
    for invalid in [json!(-1), json!(1.5), json!("1"), Value::Null] {
        let mut malformed = encoded.clone();
        malformed["modelCalls"] = invalid;
        assert!(serde_json::from_value::<Budgets>(malformed).is_err());
    }
    assert!(
        serde_json::from_str::<Budgets>(
            r#"{"modelCalls":0,"modelCalls":1,"agentTasks":0,"cadCommands":0,"repairs":0,"validationRuns":0,"wallTimeSeconds":0}"#
        )
        .is_err()
    );
}

#[test]
fn mastra_values_native_stamp_adapter_has_no_numeric_loss() {
    use super::mastra_protocol::{DocumentStamp, document_stamp_from_native, document_stamp_to_native};
    for revision in [0, 9_007_199_254_740_993, u64::MAX] {
        let stamp = document_stamp_from_native("doc:original", revision).unwrap();
        assert_eq!(document_stamp_to_native(&stamp), ("doc:original".to_owned(), revision));
        let encoded = serde_json::to_value(&stamp).unwrap();
        assert_eq!(encoded, json!({"documentId":"doc:original","revision":revision.to_string()}));
        let decoded: DocumentStamp = serde_json::from_value(encoded).unwrap();
        assert_eq!(document_stamp_to_native(&decoded), ("doc:original".to_owned(), revision));
    }
    let native_error = document_stamp_from_native("/private/secret", 0).unwrap_err();
    assert_eq!(
        serde_json::to_value(&native_error).unwrap(),
        json!({"code":"INTERNAL_ERROR","message":"Internal protocol value error.","retry":"never","details":{}})
    );
    let supplied_error = super::mastra_protocol::Id::parse("/private/secret").unwrap_err();
    assert_eq!(
        serde_json::to_value(&supplied_error).unwrap(),
        json!({"code":"INVALID_REQUEST","message":"Invalid protocol value.","retry":"never","details":{}})
    );
    assert!(!format!("{native_error:?} {native_error} {supplied_error:?} {supplied_error}").contains("secret"));
    for raw in [
        r#"{"documentId":"doc"}"#,
        r#"{"revision":"0"}"#,
        r#"{"documentId":"doc","revision":0}"#,
        r#"{"documentId":"doc","revision":"0","revision":"1"}"#,
        r#"{"documentId":"doc","revision":"0","extra":null}"#,
    ] {
        assert!(serde_json::from_str::<DocumentStamp>(raw).is_err());
    }
}

fn transcript_clipboard_harness(layout: crate::shell::LayoutMode) -> crate::scenario::Harness {
    let mut h = narrow_chat();
    h.app.ui.layout = layout;
    h.app.preferences.agent_chat_width = 500.0;
    h.app.agent_chat.items.push(Item::Agent { thought: false, text: "Transcript clipboard sentinel".into() });
    h.app.run("solid.box", json!({"length": 2, "width": 2, "height": 2, "body_name": "Cube"})).unwrap();
    h.app.run("select.set", json!({"items": [{"type": "body", "name": "Cube"}]})).unwrap();
    h.frames(4);
    h
}

fn transcript_text_position(h: &crate::scenario::Harness) -> egui::Pos2 {
    let mut at = None;
    for clipped in &h.shapes {
        crate::test_support::each_shape(&clipped.shape, &mut |shape| {
            if let egui::Shape::Text(text) = shape
                && text.galley.text() == "Transcript clipboard sentinel"
            {
                at = Some(text.pos + egui::vec2(15.0, 7.0));
            }
        });
    }
    at.expect("the assistant message is visible")
}

fn transcript_click(h: &mut crate::scenario::Harness, pos: egui::Pos2) {
    h.frame_with(vec![
        egui::Event::PointerMoved(pos),
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
    ]);
    h.frame_with(vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
}

#[test]
fn transcript_copy_and_cut_do_not_touch_the_cad_clipboard() {
    for layout in [crate::shell::LayoutMode::Docked, crate::shell::LayoutMode::Floating] {
        let mut h = transcript_clipboard_harness(layout);
        let at = transcript_text_position(&h);
        transcript_click(&mut h, at);
        transcript_click(&mut h, at);
        assert!(h.ctx.plugin::<egui::text_selection::LabelSelectionState>().lock().has_selection(), "select real label text");
        assert!(!h.ctx.egui_wants_keyboard_input(), "a selectable label is not a text editor");
        // Moving the mouse away does not transfer the keyboard back to the model.
        h.frame_with(vec![egui::Event::PointerMoved(egui::pos2(500.0, 500.0))]);
        let undo = h.app.session.undo.len();
        h.frame_with(vec![egui::Event::Copy]);
        assert!(h.output.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(text) if text == "Transcript")), "copy selected text");
        assert!(h.app.copied_bodies.is_empty(), "{layout:?}: copying text must not copy a CAD body");
        h.frame_with(vec![egui::Event::Cut]);
        assert!(h.app.cut.is_empty(), "{layout:?}: cutting read-only text must not cut a CAD body");
        assert_eq!(h.app.session.undo.len(), undo);
    }
}

#[test]
fn transcript_paste_does_not_insert_a_previously_copied_body() {
    for layout in [crate::shell::LayoutMode::Docked, crate::shell::LayoutMode::Floating] {
        let mut h = transcript_clipboard_harness(layout);
        h.frame_with(vec![egui::Event::Copy]);
        assert!(!h.app.copied_bodies.is_empty(), "prime the real CAD clipboard");
        let at = transcript_text_position(&h);
        transcript_click(&mut h, at);
        transcript_click(&mut h, at);
        let undo = h.app.session.undo.len();
        h.frame_with(vec![egui::Event::Paste("text from another app".into())]);
        assert_eq!(h.app.session.model.state().bodies.len(), 1, "{layout:?}: text paste must not paste CAD geometry");
        assert_eq!(h.app.session.undo.len(), undo);
        // A deliberate click outside the panel returns ownership to CAD.
        transcript_click(&mut h, egui::pos2(700.0, 500.0));
        h.frame_with(vec![egui::Event::Paste("SolidCraft bodies".into())]);
        assert_eq!(h.app.session.model.state().bodies.len(), 2, "{layout:?}: CAD paste still works after returning to the model");
    }
}

#[test]
fn transcript_key_events_do_not_run_cad_shortcuts() {
    for layout in [crate::shell::LayoutMode::Docked, crate::shell::LayoutMode::Floating] {
        let mut h = transcript_clipboard_harness(layout);
        let at = transcript_text_position(&h);
        transcript_click(&mut h, at);
        transcript_click(&mut h, at);
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::COMMAND,
        };
        h.frame_with(vec![key(egui::Key::C)]);
        assert!(h.app.copied_bodies.is_empty(), "{layout:?}: key-event copy also belongs to the chat");
        h.frame_with(vec![key(egui::Key::X)]);
        assert!(h.app.cut.is_empty(), "{layout:?}: key-event cut also belongs to the chat");
        assert!(crate::edit_clipboard::copy(&mut h.app));
        h.frame_with(vec![key(egui::Key::V)]);
        assert_eq!(h.app.session.model.state().bodies.len(), 1, "{layout:?}: key-event paste stays out of CAD");
        // A quick tap can have no modifiers held by the end of the frame. It must not
        // fall through to the plain A (Appearance) shortcut while selecting chat text.
        h.frame_with(vec![key(egui::Key::A)]);
        assert!(h.app.dialog.is_none(), "{layout:?}: selecting text must not open Appearance");
    }
}

#[test]
fn transcript_shortcut_ownership_ends_when_the_panel_is_hidden() {
    for hidden in ["closed", "minimized", "disabled"] {
        let mut h = transcript_clipboard_harness(crate::shell::LayoutMode::Floating);
        h.frame_with(vec![egui::Event::Copy]);
        let at = transcript_text_position(&h);
        transcript_click(&mut h, at);
        transcript_click(&mut h, at);
        match hidden {
            "closed" => h.app.agent_chat.open = false,
            "minimized" => h.app.preferences.agent_chat_minimized = true,
            _ => h.app.preferences.ai_features = false,
        }
        h.frame_with(vec![egui::Event::Paste("SolidCraft bodies".into())]);
        assert_eq!(h.app.session.model.state().bodies.len(), 2, "{hidden}: the invisible panel must not retain CAD keys");
    }
}

fn chat() -> AgentChat {
    let mut c = AgentChat::default();
    c.state = Some(State::Ready);
    c
}

#[cfg(not(target_arch = "wasm32"))]
fn app_with_discovered_login() -> SolveApp {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.detection = Some(discover::Detection {
        found: vec![discover::Found {
            id: "qwen",
            name: "Qwen Code",
            argv: vec!["qwen".into(), "--acp".into()],
            version: None,
            login: discover::Login::LooksLoggedIn,
            problem: None,
            install: "npm install -g @qwen-code/qwen-code",
        }],
        ..Default::default()
    });
    app.agent_chat.running_id = "qwen".into();
    app
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn agent_login_file_does_not_claim_a_verified_sign_in() {
    let app = app_with_discovered_login();
    assert_eq!(agents::current(&app).unwrap().login, "login_found");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn agent_login_follows_chat_authentication_results() {
    let mut app = app_with_discovered_login();
    app.preferences.agents.push(agents::AgentConfig { id: "other".into(), name: "Other agent".into(), ..Default::default() });
    app.agent_chat.on_event(Event::AuthRequired { methods: vec![] });
    assert_eq!(agents::current(&app).unwrap().login, "signed_out");
    assert_eq!(agents::entries(&app).iter().find(|e| e.id == "other").unwrap().login, "");
    app.agent_chat.settings.tests.insert("qwen".into(), settings::Test::Done(Err(backend::TestError::AuthRequired)));
    app.agent_chat.on_event(Event::Ready { agent: "Qwen Code".into(), images: false });
    assert_eq!(agents::current(&app).unwrap().login, "signed_in");
    assert!(!app.agent_chat.settings.tests.contains_key("qwen"), "a newer chat clears the stale test error");
    app.agent_chat.on_event(Event::AuthRequired { methods: vec![] });
    assert_eq!(agents::current(&app).unwrap().login, "signed_out");
}

#[cfg(unix)]
#[test]
fn agent_login_test_results_override_discovery() {
    // A real local ACP subprocess: initialize succeeds, session/new rejects the expired login.
    let script = r#"
read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fake","version":"1"}}}'
read -r session
printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"credentials expired"}}'
while IFS= read -r line; do :; done
"#;
    let argv = vec!["sh".into(), "-c".into(), script.into()];
    let result = acp_client::test_agent(&argv, &std::env::temp_dir());
    assert_eq!(result, Err(backend::TestError::AuthRequired), "keep the protocol error, not translated text");
    let mut app = app_with_discovered_login();
    let complete = |app: &mut SolveApp, result| {
        let (tx, rx) = channel();
        app.agent_chat.settings.tests.insert("qwen".into(), settings::Test::Running(rx));
        tx.send(result).unwrap();
        frame(app);
    };
    complete(&mut app, result);
    assert_eq!(agents::current(&app).unwrap().login, "signed_out");
    complete(&mut app, Err(backend::TestError::Other("connection failed".into())));
    assert_eq!(agents::current(&app).unwrap().login, "signed_out", "transport errors aren't login evidence");
    let success = script.replace(r#""error":{"code":-32000,"message":"credentials expired"}"#, r#""result":{"sessionId":"test"}"#);
    let result = acp_client::test_agent(&["sh".into(), "-c".into(), success], &std::env::temp_dir());
    assert!(result.is_ok(), "{result:?}");
    complete(&mut app, result);
    assert_eq!(agents::current(&app).unwrap().login, "signed_in");
    complete(&mut app, Err(backend::TestError::Other("connection failed".into())));
    assert_eq!(agents::current(&app).unwrap().login, "signed_in");
    app.agent_chat.on_event(Event::AuthRequired { methods: vec![] });
    frame(&mut app);
    assert_eq!(agents::current(&app).unwrap().login, "signed_out", "an old test cannot overwrite newer chat evidence");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn agent_login_refresh_returns_to_unverified_discovery() {
    let mut app = app_with_discovered_login();
    app.agent_chat.on_event(Event::AuthRequired { methods: vec![] });
    app.agent_chat.settings.tests.insert("qwen".into(), settings::Test::Done(Err(backend::TestError::AuthRequired)));
    let (tx, rx) = channel();
    app.agent_chat.detecting = Some(rx);
    tx.send(app.agent_chat.detection.clone().unwrap()).unwrap();
    frame(&mut app);
    assert_eq!(agents::current(&app).unwrap().login, "login_found");
    assert!(!app.agent_chat.settings.tests.contains_key("qwen"));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_sign_in_proven_in_one_tab_holds_in_every_tab() {
    // #9293: discovery found no login file before the user signed in from the panel. The chat's
    // proof of the sign-in belongs to the window, so a new tab doesn't ask for it again.
    let mut app = app_with_discovered_login();
    if let Some(d) = app.agent_chat.detection.as_mut() {
        d.found[0].login = discover::Login::NotLoggedIn;
    }
    assert_eq!(assistant_mark(&app), AssistantMark::NeedsAuth);
    app.agent_chat.on_event(Event::Ready { agent: "Qwen Code".into(), images: false });
    assert_eq!(agents::current(&app).unwrap().login, "signed_in");

    crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
    assert_eq!(app.agent_chat.state(), State::Off, "a new tab starts its own conversation");
    assert_eq!(agents::current(&app).unwrap().login, "signed_in");
    assert_eq!(assistant_mark(&app), AssistantMark::Ready, "no sign-in mark in the new tab");

    // Evidence from the new tab is the newest, also back in the first one.
    app.agent_chat.running_id = "qwen".into();
    app.agent_chat.on_event(Event::AuthRequired { methods: vec![] });
    crate::documents::switch(&mut app, 0);
    assert_eq!(agents::current(&app).unwrap().login, "signed_out");
    app.agent_chat.on_event(Event::Ready { agent: "Qwen Code".into(), images: false });

    // A replacement design starts a fresh conversation, not a fresh sign-in.
    crate::documents::reset_chat(&mut app);
    assert_eq!(agents::current(&app).unwrap().login, "signed_in");
    crate::documents::close(&mut app, 0, true);
    assert_eq!(agents::current(&app).unwrap().login, "signed_in", "closing a tab keeps the window's evidence");
}

#[test]
fn streamed_text_joins_and_tool_updates_change_their_call() {
    let mut c = chat();
    for e in [
        Event::Text { thought: false, text: "Mak".into() },
        Event::Text { thought: false, text: "ing a box".into() },
        Event::Tool {
            id: "t".into(),
            title: Some("batch".into()),
            kind: Some("other".into()),
            status: Some(ToolStatus::Pending),
            input: None,
            output: None,
        },
        Event::Tool { id: "t".into(), title: None, kind: None, status: Some(ToolStatus::Completed), input: None, output: Some("ok".into()) },
    ] {
        c.on_event(e);
    }
    assert_eq!(c.items.len(), 2);
    assert!(matches!(&c.items[0], Item::Agent { text, .. } if text == "Making a box"));
    assert!(matches!(&c.items[1], Item::Tool { title, status: Some(ToolStatus::Completed), output: Some(o), .. } if title == "batch" && o == "ok"));
}

#[test]
fn a_sign_in_request_keeps_the_question_for_after_the_login() {
    let mut c = chat();
    c.last = Some(("make a cube".into(), false, None, vec![attach::Attachment::imported("motor.step".into())]));
    c.state = Some(State::Busy);
    assert!(c.on_event(Event::TurnEnded { reason: "auth".into() }).is_some());
    c.on_event(Event::AuthRequired { methods: vec![] });
    assert_eq!(c.state(), State::NeedsAuth);
    let (text, image, _, files) = c.queued.clone().expect("queued for after the login");
    assert_eq!((text.as_str(), image), ("make a cube", false));
    assert_eq!(files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["motor.step"], "its files go again too");
}

#[test]
fn a_plan_update_replaces_the_turns_plan() {
    let mut c = chat();
    c.push(Item::User { text: "go".into(), image: false, pictures: Vec::new(), files: Vec::new(), context: Context::default() });
    c.on_event(Event::Plan(vec![("a".into(), "pending".into())]));
    c.on_event(Event::Text { thought: false, text: "working".into() });
    c.on_event(Event::Plan(vec![("a".into(), "completed".into())]));
    let plans: Vec<_> = c.items.iter().filter(|i| matches!(i, Item::Plan(_))).collect();
    assert_eq!(plans.len(), 1);
    assert!(matches!(plans[0], Item::Plan(p) if p[0].1 == "completed"));
}

#[test]
fn a_permission_is_answered_once() {
    let mut c = chat();
    let opts = vec![PermissionOption { id: "allow".into(), name: "Allow".into(), kind: "allow_once".into() }];
    c.push(Item::Permission { key: 5, title: "batch".into(), options: opts, answer: None });
    assert_eq!(c.waiting(), 1);
    c.answer(0, Some("allow"));
    c.answer(0, None);
    assert!(matches!(&c.items[0], Item::Permission { answer: Some(a), .. } if a == "Allow"));
    assert_eq!(c.waiting(), 0);
}

/// An app with the chat's endpoint wired to a channel the test feeds, and a message in progress.
fn app_in_a_turn() -> (SolveApp, Sender<ControlRequest>) {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    // The default mode: modelling runs, files ask.
    assert_eq!(app.preferences.ai_permissions, agents::Mode::Edits);
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    app.agent_chat.state = Some(State::Busy);
    start_turn(&mut app, "two boxes".into(), false, None, Vec::new());
    (app, tx)
}

fn ask(tx: &Sender<ControlRequest>, method: &str, params: Value) -> Receiver<Value> {
    let (req, rx) = ControlRequest::new(method, params);
    tx.send(req).unwrap();
    rx
}

fn frame(app: &mut SolveApp) {
    let ctx = crate::fonts::test_context();
    poll(app, &ctx);
    app.drain_control(&ctx);
}

fn boxed(x: f64) -> Value {
    json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10, "corner": [x, 0, 0]}})
}

/// Everything the agent changed in one message undoes as one step, and the conversation offers
/// that undo while nothing else has happened since.
#[test]
fn a_messages_changes_undo_as_one_step() {
    let (mut app, tx) = app_in_a_turn();
    let before = app.session.doc.clone();
    let depth = app.session.undo.len();
    let replies: Vec<_> = [0.0, 20.0, 40.0].into_iter().map(|x| ask(&tx, "engine.execute", boxed(x))).collect();
    frame(&mut app);
    for r in replies {
        assert_eq!(r.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    }
    assert_eq!(app.session.undo.len(), depth + 3);
    assert_eq!(app.agent_chat.turn.as_ref().map(|t| t.changes), Some(3));
    app.agent_chat.on_event(Event::TurnEnded { reason: String::new() });
    end_turn(&mut app);
    assert_eq!(app.session.undo.len(), depth + 1, "one step");
    assert!(app.session.undo.last().is_some_and(|s| s.label.contains("two boxes")));
    let n = app.agent_chat.items.iter().rposition(|i| matches!(i, Item::Changes { steps: 3, .. })).expect("the changes are listed");
    undo_changes(&mut app, n);
    assert!(Arc::ptr_eq(&app.session.doc, &before), "back to the design before the message");
    assert!(matches!(app.agent_chat.items[n], Item::Changes { undone: true, .. }));
}

/// Saving waits for Allow or Deny; Deny answers the agent without running it, Allow runs it.
#[test]
fn file_actions_wait_for_the_user() {
    let (mut app, tx) = app_in_a_turn();
    let dir = solidcraft_engine::io::vfs::temp_dir().join(format!("sc-agent-chat-approve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.solidcraft");
    let save = json!({"command": "file.save", "params": {"path": path.to_string_lossy()}, "allow_paths": [dir.to_string_lossy()]});
    let denied = ask(&tx, "engine.execute", save.clone());
    frame(&mut app);
    assert_eq!(app.agent_chat.waiting(), 1);
    assert!(denied.try_recv().is_err(), "nothing answered before the user decides");
    let id = app.agent_chat.held[0].id;
    approve(&mut app, id, false);
    let r = denied.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(r["error"].as_str().is_some_and(|e| e.contains("declined")), "{r}");
    assert!(!path.exists());
    let allowed = ask(&tx, "engine.execute", save);
    frame(&mut app);
    let id = app.agent_chat.held[0].id;
    approve(&mut app, id, true);
    frame(&mut app);
    let r = allowed.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert!(path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Crash reports and app methods never run for the agent, requests outside a message don't
/// either, and a message stops taking changes at the limit.
#[test]
fn the_gate_refuses_and_limits() {
    let (mut app, tx) = app_in_a_turn();
    assert_eq!(app.preferences.agent_chat_turn_limit, 0, "no limit by default");
    app.preferences.agent_chat_turn_limit = 2;
    let crash = ask(&tx, "engine.execute", json!({"command": "diagnostics.crash_reports", "params": {}}));
    let quit = ask(&tx, "app.quit", json!({"force": true}));
    let ok: Vec<_> = (0..2).map(|i| ask(&tx, "engine.execute", boxed(f64::from(i) * 20.0))).collect();
    let over = ask(&tx, "engine.execute", boxed(80.0));
    let read = ask(&tx, "document.inspect", json!({}));
    frame(&mut app);
    let get = |r: Receiver<Value>| r.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
    assert!(get(crash)["error"].as_str().is_some_and(|e| e.contains("crash reports")));
    assert!(get(quit)["error"].as_str().is_some_and(|e| e.contains("not available")));
    for r in ok {
        assert_eq!(get(r)["ok"], true);
    }
    assert!(get(over)["error"].as_str().is_some_and(|e| e.contains("limit of 2")));
    assert_eq!(get(read)["ok"], true, "reads go on past the limit");
    end_turn(&mut app);
    let late = ask(&tx, "engine.execute", boxed(100.0));
    frame(&mut app);
    assert!(get(late)["error"].as_str().is_some_and(|e| e.contains("no message in progress")));
}

#[test]
fn merging_finds_the_turns_start_when_the_history_was_trimmed() {
    // Distinct designs: what counts is which `Arc` a snapshot holds.
    let doc = |_: usize| Arc::new(solidcraft_engine::doc::Document::default());
    let snap = |d: &Arc<solidcraft_engine::doc::Document>| solidcraft_engine::Snapshot {
        label: String::new(),
        doc: d.clone(),
        active_sketch: None,
        pending_moves: Default::default(),
        visibility: Default::default(),
    };
    let docs: Vec<_> = (0..6).map(doc).collect();
    // The turn began at depth 3 with docs[3]; the history then lost its oldest step.
    let mut undo: Vec<_> = docs[1..6].iter().map(snap).collect();
    assert_eq!(merge_undo(&mut undo, &docs[3], 3, "Agent"), 3);
    assert_eq!(undo.len(), 3);
    assert!(Arc::ptr_eq(&undo[2].doc, &docs[3]) && undo[2].label == "Agent");
    // A start that is no longer in the history: nothing is merged.
    let mut undo: Vec<_> = docs[1..6].iter().map(snap).collect();
    assert_eq!(merge_undo(&mut undo, &doc(9), 3, "Agent"), 0);
    assert_eq!(undo.len(), 5);
}

/// People who don't want AI never see it: with "Show AI features" off there is no button, the
/// shortcut does nothing, no panel draws, nothing is looked for and nothing starts, even when
/// asked over the control channel. Turned on, the button is there; discovery waits for the panel.
#[test]
fn with_ai_features_off_nothing_of_the_chat_shows_or_runs() {
    use crate::scenario::{handle_at, rect_of};
    let mut h = crate::scenario::Harness::new();
    let shortcut = json!({"key": "A", "shift": true, "cmd": true});
    h.app.preferences.ai_features = false;
    h.frames(3);
    assert!(handle_at("titlebar:assistant").is_none(), "no Assistant button");
    h.call("ui.key", shortcut.clone());
    h.frames(3);
    h.call("ui.agent_chat", json!({"open": true, "send": "hello", "connect": true}));
    h.frames(3);
    assert!(!h.app.agent_chat.open && rect_of("agent_chat:panel").is_none(), "no panel");
    assert!(h.app.agent_chat.detection.is_none() && h.app.agent_chat.detecting.is_none(), "nothing looked for");
    assert!(h.app.agent_chat.backend.is_none() && h.app.agent_chat.tools_port.is_none(), "nothing started");
    // The View settings menu has no chat entry either.
    if let Some(view) = handle_at("nav:view") {
        h.call("ui.click", json!({"x": view.x, "y": view.y}));
        h.frames(3);
        assert!(handle_at("view:agent_chat").is_none(), "no menu entry");
        h.call("ui.click", json!({"x": view.x, "y": view.y}));
    }
    // On (the default): the button shows; discovery waits until the panel opens.
    h.app.preferences.ai_features = true;
    h.frames(3);
    assert!(handle_at("titlebar:assistant").is_some(), "the Assistant button");
    assert!(h.app.agent_chat.detection.is_none() && h.app.agent_chat.detecting.is_none(), "nothing looked for yet");
    h.call("ui.key", shortcut.clone());
    h.frames(3);
    assert!(h.app.agent_chat.open && rect_of("agent_chat:panel").is_some(), "Ctrl+Shift+A opens the panel");
    assert!(h.app.agent_chat.detection.is_some() || h.app.agent_chat.detecting.is_some(), "the panel looks for agents");
    assert!(h.app.agent_chat.backend.is_none(), "but starts none until a chat begins");
    // Without the toolbar button the shortcut still works.
    h.app.preferences.ai_toolbar_button = false;
    h.call("ui.key", shortcut);
    h.frames(3);
    assert!(handle_at("titlebar:assistant").is_none() && !h.app.agent_chat.open);
}

#[test]
fn ai_features_and_the_toolbar_button_are_on_by_default() {
    let p = crate::prefs::Prefs::default();
    assert!(p.ai_features && p.ai_toolbar_button);
}

/// Pop out puts the chat in its own window (a native window on the desktop, which can go to
/// another screen; embedded here, where the test context has one viewport), and Dock puts it back
/// in the side panel. The control channel does the same.
#[test]
fn the_chat_pops_out_into_its_own_window_and_docks_back() {
    use crate::scenario::{handle_at, rect_of};
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    assert!(rect_of("agent_chat:panel").is_some() && rect_of("agent_chat:window").is_none(), "docked by default");
    assert!(!h.app.preferences.agent_chat_popped);
    let pop = handle_at("agent_chat:pop_out").expect("a pop-out button in the header");
    h.call("ui.click", json!({"x": pop.x, "y": pop.y}));
    h.frames(3);
    assert!(h.app.preferences.agent_chat_popped);
    assert!(rect_of("agent_chat:panel").is_none(), "the side panel is gone");
    assert!(rect_of("agent_chat:window").is_some(), "the chat is in its own window");
    assert_eq!(status(&h.app)["popped"], json!(true));
    // A window has its own minimise; the strip and the side drag are the docked panel's.
    assert!(handle_at("agent_chat:minimize").is_none() && handle_at("agent_chat:grip").is_none());
    let dock = handle_at("agent_chat:dock").expect("a dock button in the popped-out header");
    h.call("ui.click", json!({"x": dock.x, "y": dock.y}));
    h.frames(3);
    assert!(!h.app.preferences.agent_chat_popped);
    assert!(rect_of("agent_chat:panel").is_some() && rect_of("agent_chat:window").is_none(), "back in the side panel");
    h.call("ui.agent_chat", json!({"popped": true}));
    h.frames(3);
    assert!(rect_of("agent_chat:window").is_some() && rect_of("agent_chat:panel").is_none());
    // Closing the chat keeps where it lives: it opens popped out again.
    h.call("ui.agent_chat", json!({"open": false}));
    h.frames(2);
    assert!(rect_of("agent_chat:window").is_none());
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    assert!(rect_of("agent_chat:window").is_some());
}

/// The saved place is given to the window once, when it opens; after that the system owns it.
/// Given every frame, it chased the system's own rounding: a window that shook a pixel up and
/// down (686, 687, 686…) on a Retina screen.
#[test]
fn the_window_place_is_given_once_when_it_opens() {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.preferences.agent_chat_window = Some([1967.0, 160.0, 569.0, 686.0]);
    let first = panel::window_builder(&app);
    assert_eq!(first.position, Some(egui::pos2(1967.0, 160.0)));
    assert_eq!(first.inner_size, Some(egui::vec2(569.0, 686.0)));
    app.agent_chat.window_placed = true;
    app.preferences.agent_chat_window = Some([1967.0, 160.0, 569.0, 687.0]);
    let later = panel::window_builder(&app);
    assert_eq!((later.position, later.inner_size), (None, None), "nothing to move or resize once it is open");
    assert!(later.title.is_some());
    // Docked or closed, the next pop-out opens at the saved place again.
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true, "popped": true}));
    h.frames(2);
    assert!(h.app.agent_chat.window_placed);
    h.call("ui.agent_chat", json!({"popped": false}));
    h.frames(2);
    assert!(!h.app.agent_chat.window_placed);
    h.call("ui.agent_chat", json!({"popped": true}));
    h.frames(2);
    h.call("ui.agent_chat", json!({"open": false}));
    h.frames(2);
    assert!(!h.app.agent_chat.window_placed);
}

/// macOS shows the text cursor only in the active window: the chat's own window becomes active
/// when the pointer comes over its text box from the app's main window, and never takes the focus
/// from another app.
#[test]
fn the_chat_window_takes_the_focus_for_its_text_box_from_the_main_window_only() {
    assert!(panel::take_focus_for_text(true, true, Some(false), Some(true)));
    assert!(!panel::take_focus_for_text(true, true, Some(false), Some(false)), "another app is active");
    assert!(!panel::take_focus_for_text(true, true, Some(true), Some(false)), "already active");
    assert!(!panel::take_focus_for_text(true, false, Some(false), Some(true)), "not over the text box");
    assert!(!panel::take_focus_for_text(false, true, Some(false), Some(true)), "docked: the main window shows it");
    assert!(!panel::take_focus_for_text(true, true, None, Some(true)), "unknown: leave it");
}

/// Whether the chat is popped out, and its window's place and size, are preferences; ones saved
/// before they existed load docked, and an unusable saved place is ignored.
#[test]
fn the_popped_out_window_and_its_place_are_remembered() {
    use crate::prefs::Prefs;
    let mut p = Prefs::default();
    assert!(!p.agent_chat_popped && p.agent_chat_window.is_none());
    p.agent_chat_popped = true;
    p.agent_chat_window = Some([-1600.0, 80.0, 420.0, 700.0]);
    let back: Prefs = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert!(back.agent_chat_popped);
    assert_eq!(back.agent_chat_window, Some([-1600.0, 80.0, 420.0, 700.0]), "a place on a screen left of the main one is kept");
    let mut old = serde_json::to_value(Prefs::default()).unwrap();
    old.as_object_mut().unwrap().remove("agent_chat_popped");
    old.as_object_mut().unwrap().remove("agent_chat_window");
    let old: Prefs = serde_json::from_value(old).unwrap();
    assert!(!old.agent_chat_popped && old.agent_chat_window.is_none());
    assert_eq!(panel::window_place(Some([10.0, 20.0, 400.0, 600.0])), Some(([10.0, 20.0], [400.0, 600.0])));
    for bad in [[f32::NAN, 0.0, 400.0, 600.0], [0.0, 0.0, 10.0, 600.0], [0.0, 0.0, 400.0, f32::INFINITY], [1e9, 0.0, 400.0, 600.0]] {
        assert_eq!(panel::window_place(Some(bad)), None, "{bad:?}");
    }
}

/// "Ask every time": a modelling command waits for Allow; "Always allow" lets the same command
/// through from then on for this agent. "Allow everything": even a save runs without a question.
#[test]
fn permission_modes_decide_what_asks() {
    let (mut app, tx) = app_in_a_turn();
    app.agent_chat.running_id = "claude".into();
    app.preferences.ai_permissions = agents::Mode::Ask;
    let first = ask(&tx, "engine.execute", boxed(0.0));
    frame(&mut app);
    assert_eq!(app.agent_chat.waiting(), 1, "a modelling command asks");
    let id = app.agent_chat.held[0].id;
    approve_always(&mut app, id);
    frame(&mut app);
    assert_eq!(first.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    assert!(agents::always_allowed(&app.preferences, "claude", "solid.box"));
    let second = ask(&tx, "engine.execute", boxed(20.0));
    frame(&mut app);
    assert_eq!(app.agent_chat.waiting(), 0, "remembered");
    assert_eq!(second.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    // Another agent keeps asking.
    app.agent_chat.running_id = "codex".into();
    let _third = ask(&tx, "engine.execute", boxed(40.0));
    frame(&mut app);
    assert_eq!(app.agent_chat.waiting(), 1);
    decline_all(&mut app, "test");
    // Allow everything, set for this agent only.
    app.preferences.agent_permissions.insert("codex".into(), agents::Mode::All);
    let dir = solidcraft_engine::io::vfs::temp_dir().join(format!("sc-agent-chat-all-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("b.solidcraft");
    let save = ask(
        &tx,
        "engine.execute",
        json!({"command": "file.save", "params": {"path": path.to_string_lossy()}, "allow_paths": [dir.to_string_lossy()]}),
    );
    frame(&mut app);
    frame(&mut app);
    assert_eq!(app.agent_chat.waiting(), 0);
    assert_eq!(save.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    // Crash reports stay off limits whatever the mode.
    let crash = ask(&tx, "engine.execute", json!({"command": "diagnostics.crash_reports"}));
    frame(&mut app);
    assert_eq!(crash.recv_timeout(std::time::Duration::from_secs(5)).unwrap()["ok"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent's own questions: SolidCraft's tools are allowed (its gate checks them), an earlier
/// "Always allow" answers by itself, and both leave a note; anything else is asked.
#[test]
fn the_agents_own_questions_are_answered_by_the_mode_or_asked() {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.running_id = "claude".into();
    let opts = || {
        vec![
            PermissionOption { id: "allow".into(), name: "Allow".into(), kind: "allow_once".into() },
            PermissionOption { id: "always".into(), name: "Always".into(), kind: "allow_always".into() },
            PermissionOption { id: "no".into(), name: "Reject".into(), kind: "reject_once".into() },
        ]
    };
    on_permission(&mut app, 1, "mcp__solidcraft-app__batch".into(), Some("other"), opts());
    assert!(matches!(app.agent_chat.items.last(), Some(Item::Notice { text, .. }) if text.contains("mcp__solidcraft-app__batch")));
    on_permission(&mut app, 2, "Run ls".into(), None, opts());
    assert!(matches!(app.agent_chat.items.last(), Some(Item::Permission { answer: None, .. })));
    let n = app.agent_chat.items.len() - 1;
    answer_permission(&mut app, n, Some("always"));
    assert!(agents::always_allowed(&app.preferences, "claude", "Run ls"));
    on_permission(&mut app, 3, "Run ls".into(), None, opts());
    assert!(matches!(app.agent_chat.items.last(), Some(Item::Notice { .. })), "remembered");
}

/// A title that only mentions SolidCraft is no tool of SolidCraft's: in every mode but Allow
/// everything the user is asked, and the mode still answers when it says to.
#[test]
fn a_title_that_mentions_solidcraft_goes_through_the_users_mode() {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.running_id = "codex".into();
    let opts = || vec![PermissionOption { id: "allow".into(), name: "Allow".into(), kind: "allow_once".into() }];
    let titles = [
        ("run solidcraft-cli; ls", Some("execute")),
        ("run solidcraft-cli; ls", None),
        ("Edit ~/.local/share/solidcraft/agent-chat/work/notes.txt", Some("edit")),
        ("mcp__solidcraft__batch", Some("other")),
        ("mcp__solidcraft-app__batch", Some("execute")),
    ];
    for mode in [agents::Mode::Ask, agents::Mode::Edits] {
        app.preferences.agent_permissions.insert("codex".into(), mode);
        for (title, kind) in titles {
            on_permission(&mut app, 1, title.into(), kind, opts());
            assert!(matches!(app.agent_chat.items.last(), Some(Item::Permission { answer: None, .. })), "{mode:?}: {title}");
            app.agent_chat.items.clear();
        }
        // The chat's own tool stays a single question, the gate's.
        on_permission(&mut app, 2, "mcp__solidcraft-app__batch".into(), Some("other"), opts());
        assert!(matches!(app.agent_chat.items.last(), Some(Item::Notice { .. })), "{mode:?}");
        app.agent_chat.items.clear();
    }
    app.preferences.agent_permissions.insert("codex".into(), agents::Mode::All);
    on_permission(&mut app, 3, titles[0].0.into(), titles[0].1, opts());
    assert!(matches!(app.agent_chat.items.last(), Some(Item::Notice { .. })), "Allow everything answers it");
}

// ---- Messages typed while the agent works ----

use std::sync::Mutex;

/// A backend that records what the panel asks of it, and reports what the test pushes.
#[derive(Clone, Default)]
struct Script {
    calls: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<Event>>>,
    /// Every prompt in full (its text and pictures).
    prompts: Arc<Mutex<Vec<Vec<PromptPart>>>>,
}

impl Script {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn prompts(&self) -> Vec<Vec<PromptPart>> {
        self.prompts.lock().unwrap().clone()
    }
    fn report(&self, e: Event) {
        self.events.lock().unwrap().push(e);
    }
}

impl ChatBackend for Script {
    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
    fn prompt(&mut self, parts: &[PromptPart]) {
        let text = parts.iter().find_map(|p| if let PromptPart::Text(t) = p { Some(t.clone()) } else { None }).unwrap_or_default();
        self.calls.lock().unwrap().push(format!("prompt: {text}"));
        self.prompts.lock().unwrap().push(parts.to_vec());
    }
    fn cancel(&mut self) {
        self.calls.lock().unwrap().push("cancel".into());
    }
    fn new_session(&mut self) {
        self.calls.lock().unwrap().push("new_session".into());
    }
    fn set_model(&mut self, model: &str) {
        self.calls.lock().unwrap().push(format!("set_model: {model}"));
    }
    fn answer_permission(&mut self, key: u64, option: Option<&str>) {
        self.calls.lock().unwrap().push(format!("permission: {key} {option:?}"));
    }
    fn set_config_option(&mut self, id: &str, value: &str) {
        self.calls.lock().unwrap().push(format!("set_config_option: {id}={value}"));
    }
    fn authenticate(&mut self, _: &str) {}
    fn shutdown(&mut self, _: bool) {}
}

/// An app whose agent is scripted and ready.
fn app_with_script() -> (SolveApp, Script, egui::Context) {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    let script = Script::default();
    app.agent_chat.backend = Some(Box::new(script.clone()));
    app.agent_chat.state = Some(State::Ready);
    (app, script, crate::fonts::test_context())
}

fn ended(script: &Script, reason: &str) {
    script.report(Event::TurnEnded { reason: reason.into() });
}

#[test]
fn document_tabs_keep_their_conversations_and_backend_context() {
    let (mut app, first, ctx) = app_with_script();
    app.agent_chat.open = true;
    app.agent_chat.control_starter = Some(Box::new(|_| Some(12345)));
    app.agent_chat.tools_port = Some(12345);
    app.agent_chat.settings.add = Some(settings::AddState::default());
    app.agent_chat.agent_id = Some("first-agent".into());
    app.agent_chat.input = "first draft".into();
    app.agent_chat.attach_view = true;
    app.agent_chat.add_file(attach::Attachment::imported("sample.step".into()));
    send(&mut app, &ctx, "first request".into());
    send(&mut app, &ctx, "first follow-up".into());

    crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
    assert!(app.agent_chat.items.is_empty() && app.agent_chat.input.is_empty());
    assert!(app.agent_chat.pending.is_empty() && app.agent_chat.attachments.is_empty());
    assert!(app.agent_chat.backend.is_none() && app.agent_chat.agent_id.is_none());
    assert!(app.agent_chat.tools_port.is_none() && app.agent_chat.tools_rx.is_none());
    assert!(!app.agent_chat.attach_view);
    assert!(app.agent_chat.open && app.agent_chat.control_starter.is_some());
    assert!(app.agent_chat.settings.add.is_some(), "settings belong to the window");
    assert!(first.calls().iter().any(|c| c == "cancel"));

    let second = Script::default();
    app.agent_chat.backend = Some(Box::new(second.clone()));
    app.agent_chat.state = Some(State::Ready);
    app.agent_chat.input = "second draft".into();
    send(&mut app, &ctx, "second request".into());
    ended(&second, "");
    poll(&mut app, &ctx);
    ended(&first, "cancelled");
    crate::documents::switch(&mut app, 0);
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.input, "first draft");
    assert_eq!(app.agent_chat.agent_id.as_deref(), Some("first-agent"));
    assert_eq!(app.agent_chat.tools_port, Some(12345), "the endpoint belongs to its conversation");
    assert!(app.agent_chat.items.iter().any(|i| matches!(i, Item::User { files, .. } if files.iter().any(|name| name == "sample.step"))));
    assert_eq!(texts(&app), ["first follow-up"]);
    assert!(app.agent_chat.stopped, "returning must not send the stopped queue");
    assert_eq!(first.prompts().len(), 1);
    assert!(app.agent_chat.items.iter().any(|i| matches!(i, Item::User { text, .. } if text == "first request")));
    assert!(!app.agent_chat.items.iter().any(|i| matches!(i, Item::User { text, .. } if text == "second request")));
    assert!(app.agent_chat.attach_view);

    // Closing an earlier tab shifts indices, but the surviving backend and draft stay together.
    crate::documents::close(&mut app, 0, true);
    assert_eq!(app.docs.active, 0);
    assert_eq!(app.agent_chat.input, "second draft");
    assert!(!app.agent_chat.stopped, "leaving an idle conversation does not stop a turn");
    send(&mut app, &ctx, "second continuation".into());
    assert_eq!(second.prompts().len(), 2, "the same backend retains this tab's context");
    crate::documents::close(&mut app, 0, true);
    assert!(app.agent_chat.items.is_empty() && app.agent_chat.backend.is_none());
    assert!(app.agent_chat.open && app.agent_chat.control_starter.is_some());
}

#[test]
fn leaving_during_startup_or_sign_in_keeps_the_unsent_message_for_send() {
    for state in [State::Starting, State::NeedsAuth] {
        let (mut app, script, ctx) = app_with_script();
        app.agent_chat.state = Some(state);
        app.agent_chat.add_file(attach::Attachment::imported("sample.step".into()));
        send(&mut app, &ctx, "waiting request".into());
        assert!(script.prompts().is_empty());
        assert!(app.agent_chat.queued.is_some());
        crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
        // Both events may have been buffered while the original tab was hidden.
        script.report(Event::AuthRequired { methods: Vec::new() });
        script.report(Event::Ready { agent: "scripted".into(), images: false });
        crate::documents::switch(&mut app, 0);
        poll(&mut app, &ctx);
        assert!(script.prompts().is_empty(), "returning must not send a startup or sign-in retry");
        assert!(app.agent_chat.queued.is_none());
        assert_eq!(texts(&app), ["waiting request"]);
        assert_eq!(app.agent_chat.pending[0].files[0].name, "sample.step");
        send_now(&mut app, &ctx);
        assert_eq!(script.prompts().len(), 1, "an explicit Send resumes the message");
        assert_eq!(app.agent_chat.items.iter().filter(|i| matches!(i, Item::User { text, .. } if text == "waiting request")).count(), 1);
    }
}

#[test]
fn a_late_sign_in_retry_or_permission_cannot_restart_a_departed_turn() {
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "first request".into());
    crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
    script.report(Event::Permission {
        key: 7,
        title: "mcp__solidcraft-app__batch".into(),
        kind: None,
        options: vec![PermissionOption { id: "allow".into(), name: "Allow".into(), kind: "allow_once".into() }],
    });
    ended(&script, "auth");
    script.report(Event::AuthRequired { methods: Vec::new() });
    script.report(Event::Ready { agent: "scripted".into(), images: false });
    crate::documents::switch(&mut app, 0);
    poll(&mut app, &ctx);
    assert!(script.calls().iter().any(|c| c == "permission: 7 None"), "late permission is cancelled even for our own tools");
    assert_eq!(app.agent_chat.waiting(), 0);
    assert_eq!(script.prompts().len(), 1, "a cancelled turn must not retry after sign-in");
    assert!(app.agent_chat.turn.is_none() && app.agent_chat.queued.is_none());
}

/// Exercise the chat endpoint's gate and the real dispatch paths, including the animated
/// pointer and asynchronous file loads. The scripted backend never needs a model account.
fn chat_tool(app: &mut SolveApp, tx: &Sender<ControlRequest>, ctx: &egui::Context, method: &str, params: Value) -> Value {
    let reply = ask(tx, method, params);
    pump_tools(app);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        app.drain_control(ctx);
        app.now += 1.0;
        crate::agent_cursor::step(app, ctx);
        crate::file_load::tick(app, ctx);
        app.poll_control_jobs(ctx);
        match reply.try_recv() {
            Ok(response) => return response,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("the chat's tool reply was dropped"),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        assert!(std::time::Instant::now() < deadline, "the chat's tool did not finish");
        std::thread::yield_now();
    }
}

#[test]
fn an_agents_new_design_keeps_its_conversation_and_continues() {
    // #7526: the old reset disconnected the backend inside its own file.new request.
    for (method, params, animate) in [
        ("engine.execute", json!({"command": "file.new"}), false),
        ("engine.execute", json!({"command": "FILE.NEW"}), false),
        ("engine.execute", json!({"command": "file.new"}), true),
        ("engine.batch", json!({"commands": [{"command": "file.new"}, boxed(0.0)]}), false),
        ("engine.batch", json!({"commands": [{"command": "file.new"}, boxed(0.0)]}), true),
    ] {
        let (mut app, script, ctx) = app_with_script();
        let (tx, rx) = channel();
        app.agent_chat.tools_rx = Some(rx);
        app.preferences.ai_permissions = agents::Mode::All;
        app.run("solid.box", boxed(0.0)["params"].clone()).unwrap();
        send(&mut app, &ctx, "create another design".into());
        let active = app.agent_chat.turn.as_ref().unwrap().active.clone();
        app.ui.agent_cursor.show = animate;
        crate::scenario::publish_handle("toolbar:file.new", egui::pos2(80.0, 100.0));
        crate::scenario::publish_handle("toolbar:solid.box", egui::pos2(100.0, 100.0));
        let response = chat_tool(&mut app, &tx, &ctx, method, params);
        assert_eq!(response["ok"], true, "{response}");
        assert!(app.agent_chat.backend.is_some(), "the agent must receive its own replacement's reply");
        assert!(active.load(std::sync::atomic::Ordering::Relaxed));
        assert!(app.agent_chat.items.iter().any(|i| matches!(i, Item::User { text, .. } if text == "create another design")));
        assert!(!script.calls().iter().any(|call| matches!(call.as_str(), "cancel" | "new_session")));
        let response = chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(30.0));
        assert_eq!(response["ok"], true, "the same agent continues: {response}");
        assert_eq!(app.agent_chat.turn.as_ref().unwrap().changes, 2, "a replacement does not reset the turn's change budget");
        let count = if method == "engine.batch" { 2 } else { 1 };
        assert_eq!(app.session.doc.features.len(), count);
        ended(&script, "");
        poll(&mut app, &ctx);
        assert_eq!(app.session.undo.len(), 1, "only the new design's edits form this turn's undo step");
        app.run("edit.undo", json!({})).unwrap();
        assert!(app.session.doc.features.is_empty(), "undo must not resurrect the previous design");
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn an_agents_opened_design_keeps_its_conversation_and_undo_ownership() {
    for blank in [true, false] {
        let dir = std::env::temp_dir().join(format!("solidcraft-chat-open-{}-{blank}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("next.solidcraft");
        let mut next = solidcraft_engine::Session::default();
        next.execute("solid.box", &boxed(100.0)["params"]).unwrap();
        next.execute("file.save_as", &json!({"path": path})).unwrap();
        let (mut app, script, ctx) = app_with_script();
        let (tx, rx) = channel();
        app.agent_chat.tools_rx = Some(rx);
        app.preferences.ai_permissions = agents::Mode::All;
        if !blank {
            app.run("solid.box", boxed(0.0)["params"].clone()).unwrap();
            app.run("file.save_as", json!({"path": dir.join("first.solidcraft")})).unwrap();
        }
        send(&mut app, &ctx, "open another design and add a box".into());
        let active = app.agent_chat.turn.as_ref().unwrap().active.clone();
        if !blank {
            assert_eq!(chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(20.0))["ok"], true);
            assert_eq!(chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(40.0))["ok"], true);
        }
        let response =
            chat_tool(&mut app, &tx, &ctx, "engine.execute", json!({"command": "doc.open", "params": {"path": path}, "allow_paths": [dir]}));
        assert_eq!(response["ok"], true, "{response}");
        assert!(app.agent_chat.backend.is_some(), "opening the file must carry the initiating chat");
        assert!(active.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(app.docs.count(), if blank { 1 } else { 2 });
        if !blank {
            let response = chat_tool(
                &mut app,
                &tx,
                &ctx,
                "engine.execute",
                json!({
                    "command": "doc.open", "params": {"path": dir.join("first.solidcraft")}, "allow_paths": [dir]
                }),
            );
            assert_eq!(response["ok"], false, "another tab must keep its own conversation: {response}");
            assert!(response["error"].as_str().unwrap().contains("already open in another tab"));
            assert_eq!(app.docs.active, 1);
        }
        assert_eq!(chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(130.0))["ok"], true);
        assert_eq!(chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(150.0))["ok"], true);
        assert_eq!(app.session.doc.features.len(), 3);
        ended(&script, "");
        poll(&mut app, &ctx);
        assert!(!script.calls().iter().any(|call| matches!(call.as_str(), "cancel" | "new_session")));
        assert_eq!(app.session.undo.len(), 1);
        app.run("edit.undo", json!({})).unwrap();
        assert_eq!(app.session.doc.features.len(), 1, "the opened design's existing body survives undo");
        if !blank {
            crate::documents::switch(&mut app, 0);
            assert_eq!(app.session.doc.features.len(), 3);
            assert_eq!(app.session.undo.len(), 2, "the first design keeps its own grouped agent edits");
            app.run("edit.undo", json!({})).unwrap();
            assert_eq!(app.session.doc.features.len(), 1);
            assert!(app.agent_chat.backend.is_none(), "the endpoint moved with the conversation");
            crate::documents::switch(&mut app, 1);
            assert!(app.agent_chat.backend.is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn an_agents_failed_new_design_batch_restores_its_undo_boundary() {
    let (mut app, script, ctx) = app_with_script();
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    app.preferences.ai_permissions = agents::Mode::All;
    app.run("solid.box", boxed(0.0)["params"].clone()).unwrap();
    send(&mut app, &ctx, "try another design".into());
    assert_eq!(chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(20.0))["ok"], true);
    let before = app.session.doc.clone();
    let response = chat_tool(
        &mut app,
        &tx,
        &ctx,
        "engine.batch",
        json!({"commands": [
            {"command": "file.new"},
            {"command": "sketch.line", "params": {"points": [[0, 0], [10, 10]]}}
        ]}),
    );
    assert_eq!(response["result"]["completed"], 1, "the new design ran before the failure: {response}");
    assert_eq!(response["result"]["rolled_back"], true);
    assert!(Arc::ptr_eq(&app.session.doc, &before));
    assert!(app.agent_chat.backend.is_some());
    assert_eq!(chat_tool(&mut app, &tx, &ctx, "engine.execute", boxed(40.0))["ok"], true);
    ended(&script, "");
    poll(&mut app, &ctx);
    assert_eq!(app.session.undo.len(), 2, "edits before and after the failed batch share one turn");
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(app.session.doc.features.len(), 1);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn an_agents_cancelled_open_cannot_replace_the_design_later() {
    let dir = std::env::temp_dir().join(format!("solidcraft-chat-cancel-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("next.solidcraft");
    solidcraft_engine::Session::default().execute("file.save_as", &json!({"path": path})).unwrap();
    let (mut app, script, ctx) = app_with_script();
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    app.preferences.ai_permissions = agents::Mode::All;
    app.run("solid.box", boxed(0.0)["params"].clone()).unwrap();
    let original = app.session.doc.clone();
    send(&mut app, &ctx, "open another design".into());
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    app.file_load.worker_gate = Some((started_tx, release_rx));
    let reply = ask(&tx, "engine.execute", json!({"command": "doc.open", "params": {"path": path}, "allow_paths": [dir]}));
    pump_tools(&mut app);
    app.drain_control(&ctx);
    started_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    cancel(&mut app);
    ended(&script, "cancelled");
    poll(&mut app, &ctx);
    release_tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !app.file_load.idle() && std::time::Instant::now() < deadline {
        crate::file_load::tick(&mut app, &ctx);
        std::thread::yield_now();
    }
    assert!(app.file_load.idle());
    assert_eq!(reply.try_recv().unwrap()["ok"], false, "a stopped turn cannot finish its pending open");
    assert!(Arc::ptr_eq(&app.session.doc, &original));
    assert_eq!(app.docs.count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn programmatic_replacement_clears_the_conversation_and_invalidates_its_requests() {
    for command in ["file.new", "FILE.NEW"] {
        let (mut app, script, ctx) = app_with_script();
        let (tx, rx) = channel();
        app.agent_chat.tools_rx = Some(rx);
        send(&mut app, &ctx, "first request".into());
        let queued = ask(&tx, "engine.execute", boxed(0.0));
        pump_tools(&mut app);
        app.run(command, json!({})).unwrap();
        app.drain_control(&ctx);
        assert_eq!(queued.try_recv().unwrap()["ok"], false);
        assert!(app.agent_chat.items.is_empty() && app.agent_chat.backend.is_none());
        assert!(app.session.doc.features.is_empty() && app.session.undo.is_empty());
        assert!(script.calls().iter().any(|c| c == "cancel"));
    }
}

#[test]
fn a_script_resets_chat_only_after_a_replacement_succeeds() {
    for (steps, replaced) in [
        (json!([{"command": "missing.command"}, {"command": "file.new"}]), false),
        (json!([{"command": "file.new"}]), true),
        (json!([{"id": "FILE.NEW"}]), true),
        (json!([{"command": "file.new"}, {"command": "missing.command"}]), true),
    ] {
        for params in [steps.clone(), json!({"commands": steps})] {
            let (mut app, _script, ctx) = app_with_script();
            send(&mut app, &ctx, "first request".into());
            let (req, reply) = ControlRequest::new("engine.script", params);
            app.run_control(&ctx, req);
            assert!(reply.try_recv().is_ok());
            assert_eq!(app.agent_chat.backend.is_none(), replaced);
            assert_eq!(app.agent_chat.items.is_empty(), replaced);
            assert_eq!(app.agent_chat.turn.is_none(), replaced);
        }
    }
}

#[test]
fn leaving_a_tab_refuses_computation_and_export_replies() {
    // The export deliberately omits its path: even a worker's error must be replaced by
    // cancellation after leaving, without writing a file or echoing into another document.
    for params in [boxed(0.0), json!({"command": "file.export", "params": {}})] {
        let (mut app, tx) = app_in_a_turn();
        let ctx = crate::fonts::test_context();
        app.apply_off_thread = Some(std::time::Duration::ZERO);
        app.preferences.agent_off_thread = true;
        app.preferences.ai_permissions = agents::Mode::All;
        let reply = ask(&tx, "engine.execute", params);
        pump_tools(&mut app);
        app.drain_control(&ctx);
        assert!(app.control_job.is_some() || !app.control_answering.is_empty(), "the worker result waits for a later frame");
        crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
        let incoming = app.session.doc.clone();
        let log = app.session.log.clone();
        app.poll_control_jobs(&ctx);
        let response = reply.try_recv().expect("cancellation must not wait for the worker");
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().is_some_and(|e| e.contains("document tab was left")));
        assert!(app.control_job.is_none() && app.control_answering.is_empty());
        assert!(Arc::ptr_eq(&app.session.doc, &incoming));
        assert_eq!(app.session.log, log);
        assert!(app.session.undo.is_empty());
        crate::documents::switch(&mut app, 0);
        assert!(app.session.doc.features.is_empty() && app.session.undo.is_empty());
    }
}

#[test]
fn leaving_a_tab_refuses_admitted_commands_animations_and_screenshots() {
    for animate in [false, true] {
        let (mut app, tx) = app_in_a_turn();
        let ctx = crate::fonts::test_context();
        app.ui.agent_cursor.show = animate;
        crate::scenario::publish_handle("toolbar:solid.box", egui::pos2(100.0, 100.0));
        app.ui.agent_cursor.speed = crate::agent_cursor::Speed::Times(1.0);
        let change = ask(&tx, "engine.execute", boxed(0.0));
        let shot = ask(&tx, "ui.screenshot", json!({}));
        pump_tools(&mut app);
        // Capture a request already waiting for a frame, as well as a queued command.
        let screenshot = app.control_held.pop_back().unwrap();
        app.run_control(&ctx, screenshot);
        assert_eq!(app.pending_shots.len(), 1);
        if animate {
            app.drain_control(&ctx);
            assert!(app.agent.busy(), "the command is waiting on cursor animation");
        }
        crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
        let other = app.session.doc.clone();
        app.drain_control(&ctx);
        crate::agent_cursor::step(&mut app, &ctx);
        app.collect_screenshots(&ctx);
        for reply in [change, shot] {
            let response = reply.try_recv().expect("cancelled requests answer without running");
            assert_eq!(response["ok"], false);
            assert!(response["error"].as_str().is_some_and(|e| e.contains("document tab was left")));
        }
        assert!(Arc::ptr_eq(&app.session.doc, &other));
        assert!(app.session.undo.is_empty() && app.pending_shots.is_empty());
        assert!(app.queued_shots.is_empty(), "a cancelled screenshot must not capture the incoming tab");
        crate::documents::switch(&mut app, 0);
        assert!(app.session.doc.features.is_empty() && app.session.undo.is_empty());
        // Adopting into a blank tab replaces the document and clears its old conversation.
        app.agent_chat.input = "discarded draft".into();
        crate::documents::adopt(&mut app, solidcraft_engine::Session::default());
        assert!(app.agent_chat.input.is_empty() && app.agent_chat.items.is_empty());
    }
}

#[test]
fn late_events_approvals_and_undo_stay_with_the_originating_document() {
    let (mut app, first, ctx) = app_with_script();
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    send(&mut app, &ctx, "make two blocks".into());
    for x in [0.0, 20.0] {
        let reply = ask(&tx, "engine.execute", boxed(x));
        frame(&mut app);
        assert_eq!(reply.try_recv().unwrap()["ok"], true);
    }
    assert_eq!(app.session.undo.len(), 2);
    app.preferences.ai_permissions = agents::Mode::Ask;
    let held = ask(&tx, "engine.execute", boxed(40.0));
    pump_tools(&mut app);
    assert_eq!(app.agent_chat.waiting(), 1);
    crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
    assert_eq!(held.try_recv().unwrap()["ok"], false);
    assert_eq!(app.agent_chat.waiting(), 0);
    first.report(Event::Text { thought: false, text: "Stopped in the first design".into() });
    ended(&first, "cancelled");
    let late = ask(&tx, "engine.execute", boxed(60.0));
    frame(&mut app);
    assert!(app.agent_chat.items.is_empty());
    assert!(app.session.doc.features.is_empty() && app.session.undo.is_empty());
    assert!(matches!(late.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));

    crate::documents::switch(&mut app, 0);
    frame(&mut app);
    assert_eq!(late.try_recv().unwrap()["ok"], false);
    assert_eq!(app.session.doc.features.len(), 2);
    assert_eq!(app.session.undo.len(), 1, "the outgoing turn merged only its own undo");
    let changes = app.agent_chat.items.iter().position(|i| matches!(i, Item::Changes { steps: 2, .. })).unwrap();
    assert!(app.agent_chat.items.iter().any(|i| matches!(i, Item::Agent { text, .. } if text == "Stopped in the first design")));
    undo_changes(&mut app, changes);
    assert!(app.session.doc.features.is_empty());
    crate::documents::switch(&mut app, 1);
    app.preferences.ai_features = false;
    poll(&mut app, &ctx);
    crate::documents::switch(&mut app, 0);
    assert!(app.agent_chat.backend.is_none(), "disabling AI stops hidden conversations too");
}

fn texts(app: &SolveApp) -> Vec<String> {
    app.agent_chat.pending.iter().map(|p| p.text.clone()).collect()
}

/// While the agent works a message is queued, not dropped and not sent; it is not in the
/// transcript as a sent message, but waits as a pending one that can be edited and removed.
#[test]
fn a_message_typed_while_busy_is_queued() {
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "make a box".into());
    assert_eq!(app.agent_chat.state(), State::Busy);
    send(&mut app, &ctx, "then a cylinder".into());
    send(&mut app, &ctx, "   ".into());
    send(&mut app, &ctx, "and a sphere".into());
    assert_eq!(texts(&app), ["then a cylinder", "and a sphere"]);
    assert_eq!(script.calls(), ["prompt: make a box"], "nothing else was sent");
    let users = app.agent_chat.items.iter().filter(|i| matches!(i, Item::User { .. })).count();
    assert_eq!(users, 1, "queued messages are not sent messages yet");
    // Remove one; edit the other.
    remove_pending(&mut app, 0);
    assert_eq!(texts(&app), ["and a sphere"]);
    app.agent_chat.pending[0].text = "and a cone".into();
    assert_eq!(app.agent_chat.waiting(), 0);
    assert!(status(&app)["pending"][0]["text"] == "and a cone");
}

/// When the turn ends normally the queue goes as the next prompt, all messages in order in one.
#[test]
fn the_queue_is_sent_when_the_turn_ends() {
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "first".into());
    send(&mut app, &ctx, "second".into());
    send(&mut app, &ctx, "third".into());
    ended(&script, "");
    poll(&mut app, &ctx);
    assert_eq!(script.calls(), ["prompt: first", "prompt: second\n\nthird"]);
    assert!(app.agent_chat.pending.is_empty());
    assert_eq!(app.agent_chat.state(), State::Busy, "the next turn is under way");
    assert!(matches!(app.agent_chat.items.last(), Some(Item::User { text, .. }) if text == "second\n\nthird"));
    // Nothing queued: the end of the turn just ends it.
    ended(&script, "");
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Ready);
    assert_eq!(script.calls().len(), 2);
}

/// Stop does not send the queue by itself; it stays, with a Send that sends it later.
#[test]
fn stop_keeps_the_queue() {
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "first".into());
    send(&mut app, &ctx, "later".into());
    cancel(&mut app);
    assert_eq!(script.calls(), ["prompt: first", "cancel"]);
    ended(&script, "cancelled");
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Ready);
    assert_eq!(texts(&app), ["later"], "still queued");
    assert_eq!(script.calls().len(), 2, "not sent");
    // Send (the bubble's button) sends it, without the interruption note.
    send_now(&mut app, &ctx);
    assert_eq!(script.calls(), ["prompt: first", "cancel", "prompt: later"]);
    assert!(app.agent_chat.pending.is_empty());
}

#[test]
fn an_unresponsive_cancel_exit_keeps_the_queue_and_completed_changes() {
    let (mut app, script, ctx) = app_with_script();
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    let before = app.session.doc.clone();
    send(&mut app, &ctx, "two boxes".into());
    let replies: Vec<_> = [0.0, 20.0].into_iter().map(|x| ask(&tx, "engine.execute", boxed(x))).collect();
    frame(&mut app);
    for reply in replies {
        assert_eq!(reply.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    }
    let completed = app.session.doc.clone();
    let statuses = [Some(ToolStatus::Completed), Some(ToolStatus::Failed), Some(ToolStatus::Pending), Some(ToolStatus::InProgress), None];
    for (id, status) in statuses.into_iter().enumerate() {
        script.report(Event::Tool { id: id.to_string(), title: Some("search".into()), kind: None, status, input: None, output: None });
    }
    poll(&mut app, &ctx);
    send(&mut app, &ctx, "later".into());
    cancel(&mut app);
    script.report(Event::Exited { tail: vec![tr("ui.agent_chat_cancel_timeout").to_string()] });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Exited);
    assert!(app.agent_chat.backend.is_none() && app.agent_chat.turn.is_none());
    assert_eq!(texts(&app), ["later"]);
    assert_eq!(script.prompts().len(), 1, "disconnect must not send the queue");
    assert!(Arc::ptr_eq(&app.session.doc, &completed), "completed changes stay in the design");
    let statuses: Vec<_> = app
        .agent_chat
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Tool { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        [Some(ToolStatus::Completed), Some(ToolStatus::Failed), Some(ToolStatus::Failed), Some(ToolStatus::Failed), Some(ToolStatus::Failed)]
    );
    let changes = app.agent_chat.items.iter().rposition(|i| matches!(i, Item::Changes { steps: 2, .. })).expect("the completed changes are grouped");
    undo_changes(&mut app, changes);
    assert!(Arc::ptr_eq(&app.session.doc, &before), "the completed changes still undo together");
}

/// Send now: cancel first, the prompt only after the agent has ended the cancelled turn, with a
/// note that the previous step was interrupted (shown without it in the conversation).
#[test]
fn send_now_cancels_then_sends_with_a_note() {
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "build a gearbox".into());
    send(&mut app, &ctx, "use 12 teeth instead".into());
    send_now(&mut app, &ctx);
    assert_eq!(script.calls(), ["prompt: build a gearbox", "cancel"], "cancel alone, the prompt waits for the end");
    poll(&mut app, &ctx);
    assert_eq!(script.calls().len(), 2);
    ended(&script, "cancelled");
    poll(&mut app, &ctx);
    assert_eq!(script.calls(), ["prompt: build a gearbox".to_string(), "cancel".into(), format!("prompt: {INTERRUPT_NOTE}\n\nuse 12 teeth instead")]);
    assert!(matches!(app.agent_chat.items.last(), Some(Item::User { text, .. }) if text == "use 12 teeth instead"));
    // Stop after Send now wins: nothing more is sent by itself.
    send(&mut app, &ctx, "one more".into());
    send_now(&mut app, &ctx);
    cancel(&mut app);
    ended(&script, "cancelled");
    poll(&mut app, &ctx);
    assert_eq!(texts(&app), ["one more"]);
}

/// The steered message also goes after a turn that ended by itself just before the cancel arrived.
#[test]
fn send_now_after_the_turn_already_ended_still_sends() {
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "a".into());
    send(&mut app, &ctx, "b".into());
    send_now(&mut app, &ctx);
    ended(&script, "");
    poll(&mut app, &ctx);
    assert_eq!(script.calls().last().map(String::as_str), Some("prompt: b"), "no note: nothing was interrupted");
}

/// What an interrupted turn changed undoes as one step, and the steered message starts its own.
#[test]
fn an_interrupted_turns_changes_undo_as_one_step() {
    let (mut app, script, ctx) = app_with_script();
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    let before = app.session.doc.clone();
    let depth = app.session.undo.len();
    send(&mut app, &ctx, "two boxes".into());
    let replies: Vec<_> = [0.0, 20.0].into_iter().map(|x| ask(&tx, "engine.execute", boxed(x))).collect();
    frame(&mut app);
    for r in replies {
        assert_eq!(r.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    }
    send(&mut app, &ctx, "stop, make it one".into());
    send_now(&mut app, &ctx);
    ended(&script, "cancelled");
    poll(&mut app, &ctx);
    assert_eq!(app.session.undo.len(), depth + 1, "the interrupted message is one step");
    let n = app.agent_chat.items.iter().rposition(|i| matches!(i, Item::Changes { steps: 2, .. })).expect("its changes are listed");
    // The steered message is on its own turn.
    assert!(app.agent_chat.turn.as_ref().is_some_and(|t| t.label.contains("stop, make it one") && t.changes == 0));
    undo_changes(&mut app, n);
    assert!(Arc::ptr_eq(&app.session.doc, &before));
}

/// What the user changes by hand between two messages is told to the agent before the next one,
/// with the parameters and features it touched (#10854): the agent built a hole on a parameter,
/// the user changed it, and the agent picked the old rim. Its own changes, and a design it has not
/// seen yet (a new chat), bring no note.
#[test]
fn a_change_by_hand_between_messages_is_told_to_the_agent() {
    use solidcraft_engine::doc::Param;
    let (mut app, script, ctx) = app_with_script();
    let (tx, rx) = channel();
    app.agent_chat.tools_rx = Some(rx);
    app.agent_chat.attach_selection = false;
    let expr = |app: &SolveApp| app.session.doc.params.iter().find(|p| p.name == "w").map(|p| p.expr.clone()).unwrap();

    send(&mut app, &ctx, "a box and a parameter w".into());
    let replies = [
        ask(&tx, "engine.execute", json!({"command": "parameters.add", "params": {"name": "w", "expression": "10", "unit": "mm"}})),
        ask(&tx, "engine.execute", boxed(0.0)),
    ];
    frame(&mut app);
    for r in replies {
        assert_eq!(r.recv_timeout(std::time::Duration::from_secs(30)).unwrap()["ok"], true);
    }
    ended(&script, "");
    poll(&mut app, &ctx);
    send(&mut app, &ctx, "thanks".into());
    assert_eq!(script.calls().last().map(String::as_str), Some("prompt: thanks"), "the agent made those changes itself");
    ended(&script, "");
    poll(&mut app, &ctx);

    let old = expr(&app);
    app.run("parameters.change", json!({"name": "w", "expression": "25"})).unwrap();
    app.run("solid.box", json!({"length": 5, "width": 5, "height": 5, "corner": [30, 0, 0]})).unwrap();
    let added = app.session.doc.features.last().map(|f| f.name.clone()).unwrap();
    send(&mut app, &ctx, "chamfer it".into());
    let sent = text_of(script.prompts().last().unwrap());
    // The new box may bring parameters of its own after `w`.
    let new = expr(&app);
    assert!(sent.starts_with(&format!("[The user changed the design by hand since your last reply (parameters: w {old} -> {new}")), "{sent}");
    assert!(sent.contains(&format!("; features: {added} (new)). Read it again before")), "{sent}");
    assert!(sent.ends_with("chat.]\n\nchamfer it"), "{sent}");
    assert!(
        matches!(app.agent_chat.items.iter().rev().find(|i| matches!(i, Item::User { .. })), Some(Item::User { text, .. }) if text == "chamfer it")
    );
    script.report(Event::Text { thought: false, text: "done".into() });
    ended(&script, "");
    poll(&mut app, &ctx);
    send(&mut app, &ctx, "and again".into());
    assert_eq!(script.calls().last().map(String::as_str), Some("prompt: and again"), "told once");
    ended(&script, "");
    poll(&mut app, &ctx);

    // The note's other parts, and its bound on what it lists.
    let seen = app.session.doc.as_ref().clone();
    assert!(changed_note(&seen, &seen.clone()).is_none());
    let mut rolled = seen.clone();
    rolled.marker = Some(0);
    let first = seen.features.first().map(|f| f.name.clone()).unwrap();
    assert!(changed_note(&seen, &rolled).unwrap().contains(&format!("(the History marker is now before {first}, so it")));
    let mut many = seen.clone();
    let w = seen.params.iter().find(|p| p.name == "w").cloned().unwrap();
    many.params.extend((0..20).map(|i| Param { name: format!("p{i}"), ..w.clone() }));
    assert!(changed_note(&seen, &many).unwrap().contains(&format!(", p11 = {} (new) and 8 more)", w.expr)));

    // A new chat starts a session that has seen nothing.
    app.run("parameters.change", json!({"name": "w", "expression": "30"})).unwrap();
    new_chat(&mut app);
    app.agent_chat.state = Some(State::Ready);
    send(&mut app, &ctx, "hello".into());
    assert_eq!(script.calls().last().map(String::as_str), Some("prompt: hello"));
}

/// The same with a real agent process speaking ACP: `session/cancel` goes out, the agent answers
/// the held prompt with the cancelled stop reason, and only then does the queued text go as a new
/// `session/prompt`.
#[cfg(unix)]
#[test]
fn send_now_with_an_acp_agent_sends_cancel_then_the_prompt() {
    let dir = std::env::temp_dir().join(format!("sc-agent-chat-steer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("received.log");
    // A fake agent: answers initialize and session/new; holds a prompt whose text says "slow"
    // until it gets session/cancel (then answers it "cancelled"); answers other prompts at once.
    let script = r#"
log="$1"; held=""
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      case "$line" in
        *slow*) held="$id" ;;
        *) printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id" ;;
      esac ;;
    *'"method":"session/cancel"'*)
      if [ -n "$held" ]; then printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$held"; held=""; fi ;;
  esac
done
"#;
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string(), "fake-agent".to_string(), log.to_string_lossy().into_owned()];
    let start = backend::StartContext { cwd: dir.clone(), bridge: None, tools: None, wake: Box::new(|| {}) };
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.backend = Some(Box::new(acp_client::AcpBackend::start(&argv, start).unwrap()));
    app.agent_chat.state = Some(State::Starting);
    let ctx = crate::fonts::test_context();
    let until = |app: &mut SolveApp, what: &str, done: &dyn Fn(&SolveApp) -> bool| {
        let t = std::time::Instant::now();
        while !done(app) {
            assert!(t.elapsed() < std::time::Duration::from_secs(20), "timed out waiting for {what}");
            poll(app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    until(&mut app, "the agent to be ready", &|a| a.agent_chat.state() == State::Ready);
    send(&mut app, &ctx, "slow job".into());
    send(&mut app, &ctx, "do this instead".into());
    assert_eq!(texts(&app), ["do this instead"]);
    send_now(&mut app, &ctx);
    until(&mut app, "the steered turn to finish", &|a| a.agent_chat.state() == State::Ready && a.agent_chat.pending.is_empty());
    let methods: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let m = v["method"].as_str()?.to_string();
            let text = v.pointer("/params/prompt/0/text").and_then(Value::as_str).unwrap_or("");
            Some(if text.is_empty() { m } else { format!("{m}: {text}") })
        })
        .collect();
    assert_eq!(
        methods,
        [
            "initialize".to_string(),
            "session/new".into(),
            "session/prompt: slow job".into(),
            "session/cancel".into(),
            format!("session/prompt: {INTERRUPT_NOTE}\n\ndo this instead"),
        ]
    );
    disconnect(&mut app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// New chat: the running message is stopped, the transcript and the queue are cleared, a fresh
/// session starts on the same agent, the mode stays, and the design's undo history is untouched.
#[test]
fn new_chat_clears_everything_and_keeps_the_agent_and_mode() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.modes = vec![("plan".into(), "Plan".into()), ("code".into(), "Code".into())];
    app.agent_chat.mode = "plan".into();
    app.agent_chat.agent_id = Some("claude".into());
    let depth = app.session.undo.len();
    assert!(!new_chat_needs_confirm(&app));
    send(&mut app, &ctx, "first".into());
    send(&mut app, &ctx, "queued".into());
    assert!(new_chat_needs_confirm(&app), "a running message and a queue ask first");
    new_chat(&mut app);
    assert_eq!(script.calls(), ["prompt: first", "cancel", "new_session"]);
    assert!(app.agent_chat.items.is_empty() && app.agent_chat.pending.is_empty());
    assert_eq!(app.agent_chat.state(), State::Starting);
    assert_eq!(app.agent_chat.agent_id.as_deref(), Some("claude"));
    // The cancelled message's late end is not reported; the fresh session is ready again.
    ended(&script, "cancelled");
    script.report(Event::Modes { current: "code".into(), available: vec![] });
    script.report(Event::Ready { agent: "fake".into(), images: false });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Ready);
    assert!(app.agent_chat.items.is_empty(), "no notice about the old message");
    assert_eq!(app.agent_chat.mode, "plan", "the mode is kept");
    assert_eq!(app.session.undo.len(), depth);
    assert!(!new_chat_needs_confirm(&app));
}

/// With a real ACP agent, New chat sends `session/new` on the same process (no restart).
#[cfg(unix)]
#[test]
fn new_chat_with_an_acp_agent_opens_a_new_session_on_the_same_process() {
    let dir = std::env::temp_dir().join(format!("sc-agent-chat-new-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("received.log");
    let script = r#"
log="$1"; n=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"agentInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"session/new"'*) n=$((n+1)); printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s%s"}}\n' "$id" "$n" ;;
    *'"method":"session/prompt"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id" ;;
  esac
done
"#;
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string(), "fake-agent".to_string(), log.to_string_lossy().into_owned()];
    let start = backend::StartContext { cwd: dir.clone(), bridge: None, tools: None, wake: Box::new(|| {}) };
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.backend = Some(Box::new(acp_client::AcpBackend::start(&argv, start).unwrap()));
    app.agent_chat.state = Some(State::Starting);
    let ctx = crate::fonts::test_context();
    let until = |app: &mut SolveApp, done: &dyn Fn(&SolveApp) -> bool| {
        let t = std::time::Instant::now();
        while !done(app) {
            assert!(t.elapsed() < std::time::Duration::from_secs(20), "timed out");
            poll(app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    send(&mut app, &ctx, "hello".into());
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    assert!(!app.agent_chat.items.is_empty());
    let pid = app.agent_chat.backend.as_ref().and_then(|b| b.pid());
    new_chat(&mut app);
    assert!(app.agent_chat.items.is_empty());
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    assert_eq!(app.agent_chat.backend.as_ref().and_then(|b| b.pid()), pid, "same process");
    send(&mut app, &ctx, "again".into());
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    let log_text = std::fs::read_to_string(&log).unwrap();
    let methods: Vec<String> =
        log_text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| v["method"].as_str().map(String::from)).collect();
    assert_eq!(methods, ["initialize", "session/new", "session/prompt", "session/new", "session/prompt"]);
    assert!(log_text.lines().last().is_some_and(|l| l.contains("\"s2\"")), "the prompt goes to the new session: {log_text}");
    disconnect(&mut app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #9811: an agent that offers the standard `systemPrompt` field gets the note in it, on the first
/// session and on the one New chat opens, with the custom instructions set before it (#8220).
#[cfg(unix)]
#[test]
fn an_acp_agent_offering_the_standard_field_gets_the_system_note_in_every_session() {
    let dir = std::env::temp_dir().join(format!("sc-agent-chat-note-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("received.log");
    let script = r#"
log="$1"; n=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"agentCapabilities":{"sessionCapabilities":{"systemPrompt":{}}},"agentInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"session/new"'*) n=$((n+1)); printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s%s"}}\n' "$id" "$n" ;;
  esac
done
"#;
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string(), "fake-agent".to_string(), log.to_string_lossy().into_owned()];
    let start = backend::StartContext { cwd: dir.clone(), bridge: None, tools: None, wake: Box::new(|| {}) };
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.backend = Some(Box::new(acp_client::AcpBackend::start(&argv, start).unwrap()));
    app.agent_chat.state = Some(State::Starting);
    let ctx = crate::fonts::test_context();
    let until = |app: &mut SolveApp, done: &dyn Fn(&SolveApp) -> bool| {
        let t = std::time::Instant::now();
        while !done(app) {
            assert!(t.elapsed() < std::time::Duration::from_secs(20), "timed out");
            poll(app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    app.preferences.agent_instructions = "Print flat side down.".into();
    new_chat(&mut app);
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    let sessions: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["method"] == "session/new")
        .collect();
    assert_eq!(sessions.len(), 2);
    let notes = [acp::SYSTEM_NOTE.to_string(), acp::system_note("Print flat side down.")];
    for (session, note) in sessions.iter().zip(&notes) {
        assert_eq!(session.pointer("/params/systemPrompt/content/0/text").and_then(Value::as_str), Some(note.as_str()));
    }
    disconnect(&mut app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #8220: Preferences → AI shows the Custom instructions field; `ui.agent_chat` sets them (cut at
/// the limit) and reports them; Connect sends them in the endpoint's system note, and an edit
/// reaches the conversation New chat opens.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn custom_instructions_reach_the_system_note_of_each_new_session() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let log = seen.clone();
    let http: backend::HttpFn = Arc::new(move |r: &backend::HttpRequest| {
        if r.url.ends_with("/chat/completions") {
            log.lock().unwrap().push(serde_json::from_slice(r.body.as_deref().unwrap_or(b"null")).unwrap());
            return Ok((200, json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}).to_string().into_bytes()));
        }
        Ok((200, b"{\"data\":[]}".to_vec()))
    });
    let mut h = crate::scenario::Harness::new();
    h.app.home.open = false;
    h.app.services.agent_http = Some(http);
    crate::prefs::open_at(&mut h.app, crate::prefs::PAGE_ASSISTANT);
    h.frames(4);
    assert!(crate::scenario::rect_of("prefs:agent_instructions").is_some(), "the field is on the AI page");
    h.app.preferences.ai_features = true;
    h.ctx.memory_mut(|m| m.request_focus(egui::Id::new("prefs_agent_instructions")));
    h.frames(2);
    h.step(&json!({"text": "M3"})).unwrap();
    h.frames(2);
    assert_eq!(h.app.preferences.agent_instructions, "M3", "typed text lands in the field");
    assert!(h.app.prefs_window.filter.is_empty(), "Preferences search does not take the typing");
    let long = "x".repeat(acp::INSTRUCTIONS_LIMIT + 5);
    assert_eq!(
        h.call("ui.agent_chat", json!({"instructions": long}))["result"]["instructions"].as_str().map(str::len),
        Some(acp::INSTRUCTIONS_LIMIT)
    );
    assert!(!h.call("ui.agent_chat", json!({"instructions": 3}))["error"].is_null());
    assert_eq!(h.call("ui.agent_chat", json!({"instructions": "Use M3 screws."}))["result"]["instructions"], "Use M3 screws.");

    h.app.preferences.agents.push(agents::AgentConfig {
        id: "api-1".into(),
        kind: agents::Kind::Api,
        base_url: "http://localhost:11434/v1".into(),
        model: "m".into(),
        ..Default::default()
    });
    h.app.agent_chat.agent_id = Some("api-1".into());
    let ctx = crate::fonts::test_context();
    let until = |app: &mut SolveApp, done: &dyn Fn(&SolveApp) -> bool| {
        let t = std::time::Instant::now();
        while !done(app) {
            assert!(t.elapsed() < std::time::Duration::from_secs(20), "timed out");
            poll(app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    let sent = |n: usize| {
        let seen = seen.clone();
        move |a: &SolveApp| a.agent_chat.state() == State::Ready && seen.lock().unwrap().len() == n
    };
    connect(&mut h.app, &ctx);
    until(&mut h.app, &|a| a.agent_chat.state() == State::Ready);
    send(&mut h.app, &ctx, "one".into());
    until(&mut h.app, &sent(1));
    // Editing the field leaves the running conversation's note alone.
    h.app.preferences.agent_instructions = "Print flat side down.".into();
    send(&mut h.app, &ctx, "two".into());
    until(&mut h.app, &sent(2));
    new_chat(&mut h.app);
    until(&mut h.app, &|a| a.agent_chat.state() == State::Ready);
    send(&mut h.app, &ctx, "three".into());
    until(&mut h.app, &sent(3));
    let notes: Vec<Value> = seen.lock().unwrap().iter().map(|body| body["messages"][0]["content"].clone()).collect();
    assert_eq!(notes, [acp::system_note("Use M3 screws."), acp::system_note("Use M3 screws."), acp::system_note("Print flat side down.")]);
    disconnect(&mut h.app);
}

fn models_reply(current: &str) -> Event {
    let m = |id: &str| acp::ModelInfo { id: id.into(), name: id.to_uppercase(), description: String::new() };
    Event::Models { current: current.into(), available: vec![m("fast"), m("smart")] }
}

/// An endpoint's listing whose ids read `vendor/model` and whose names (the descriptions here) read
/// `Vendor: Model`, as OpenRouter's, names its providers; the rows then read the model's name with
/// the id as the tooltip. One vendor is one flat list.
#[test]
fn an_endpoints_vendor_named_listing_names_its_providers() {
    let m = |id: &str, description: &str| acp::ModelInfo { id: id.into(), name: id.into(), description: description.into() };
    assert_eq!(model_provider(&m("openai/gpt-6.1-terra", "OpenAI: GPT-6.1 Terra")), Some(("OpenAI".into(), "GPT-6.1 Terra".into())));
    assert_eq!(model_provider(&m("openai/gpt-6.1-terra", "OpenAI: GPT-6.1 Terra\nmore")), Some(("OpenAI".into(), "GPT-6.1 Terra".into())));
    assert_eq!(model_provider(&m("gpt-6.1-terra", "OpenAI: GPT-6.1 Terra")), None, "no vendor in the id");
    assert_eq!(model_provider(&m("openai/gpt-6.1-terra", "OpenAI: ")), None, "no name");
    assert_eq!(model_provider(&m("openai/gpt-6.1-terra", "")), None);
    assert_eq!(
        model_provider(&m("openai/gpt-6.1-terra", "Provider: Host • current")),
        Some(("Host".into(), "openai/gpt-6.1-terra".into())),
        "an agent's own word first"
    );
    let rows = model_rows(&[m("openai/gpt-6.1-terra", "OpenAI: GPT-6.1 Terra"), m("qwen/qwen3.8-flash", "Qwen: Qwen3.8 Flash"), m("free-model", "")]);
    assert_eq!(
        rows,
        [
            ("GPT-6.1 Terra".to_string(), Some("OpenAI".to_string()), "openai/gpt-6.1-terra".to_string()),
            ("Qwen3.8 Flash".to_string(), Some("Qwen".to_string()), "qwen/qwen3.8-flash".to_string()),
            ("free-model".to_string(), Some(tr("ui.agent_chat_provider_other").to_string()), String::new()),
        ]
    );
    let rows = model_rows(&[m("openai/gpt-6.1-terra", "OpenAI: GPT-6.1 Terra"), m("openai/gpt-5.6-luna", "OpenAI: GPT-5.6 Luna")]);
    assert_eq!(rows[0], ("openai/gpt-6.1-terra".to_string(), None, "OpenAI: GPT-6.1 Terra".to_string()), "one vendor is one flat list");
}

/// Models from several providers fold under their providers inside the picker: the current model's
/// provider is open and the others folded; a header opens or folds its group; a typed search opens
/// every group with a match but one folded by hand; the provider asked for over the control channel
/// is open at the next opening.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn the_header_groups_models_by_provider_and_folds_them() {
    use crate::scenario::handle_at;
    let mut h = narrow_chat();
    let script = Script::default();
    h.app.agent_chat.backend = Some(Box::new(script.clone()));
    h.app.agent_chat.state = Some(State::Ready);
    h.app.agent_chat.running_id = "claude".into();
    let m = |id: &str, name: &str| acp::ModelInfo { id: id.into(), name: name.into(), description: String::new() };
    script.report(Event::Models {
        current: "a".into(),
        available: vec![m("a", "Host · Alpha"), m("c", "Nous Portal · Gamma"), m("b", "Host · Beta"), m("d", "Nous Portal · Delta")],
    });
    h.frames(4);
    assert!(handle_at("agent_chat:provider").is_none(), "no provider drop-down of its own");
    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:group:Host").is_some() && handle_at("agent_chat:model:group:Nous Portal").is_some());
    assert!(handle_at("agent_chat:model:a").is_some() && handle_at("agent_chat:model:b").is_some(), "the current model's provider is open");
    assert!(handle_at("agent_chat:model:c").is_none() && handle_at("agent_chat:model:d").is_none(), "the other provider is folded");
    h.step(&json!({"click": {"handle": "agent_chat:model:group:Nous Portal"}})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:c").is_some(), "a header opens its group");
    h.step(&json!({"click": {"handle": "agent_chat:model:group:Host"}})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:a").is_none(), "and folds it");
    h.step(&json!({"text": "ta"})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:b").is_none(), "a fold made by hand stays while searching, Beta or not");
    assert!(handle_at("agent_chat:model:d").is_some() && handle_at("agent_chat:model:c").is_none(), "Delta matches, Gamma does not");
    h.step(&json!({"click": {"handle": "agent_chat:model:d"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls(), ["set_model: d"]);
    assert_eq!(h.app.agent_chat.model, "d");
    assert!(handle_at("agent_chat:model:search").is_none(), "a choice closes it");

    control(&mut h.app, &h.ctx, &json!({"provider": "Host"})).unwrap();
    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:a").is_some(), "the provider asked for is open");
    assert!(handle_at("agent_chat:model:d").is_some(), "and the current model's");
}

/// Choosing a model tells the agent, shows it as current and remembers it for this agent; a model
/// the agent did not offer is ignored.
#[test]
fn choosing_a_model_sets_it_and_remembers_it() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "claude".into();
    script.report(models_reply("fast"));
    poll(&mut app, &ctx);
    assert!(models_shown(&app));
    assert_eq!(app.agent_chat.model, "fast");
    set_model(&mut app, "smart");
    set_model(&mut app, "nonsense");
    assert_eq!(script.calls(), ["set_model: smart"]);
    assert_eq!(app.agent_chat.model, "smart");
    assert_eq!(app.preferences.agent_models.get("claude").map(String::as_str), Some("smart"));
    assert_eq!(status(&app)["model"], "smart");
    // The agent switching its own model is shown too.
    script.report(Event::Models { current: "fast".into(), available: vec![] });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "fast");
    assert_eq!(app.agent_chat.models.len(), 2, "the list stays");
}

/// A scripted endpoint: its agent is configured, and it is running with these models.
#[cfg(not(target_arch = "wasm32"))]
fn endpoint_with_script() -> (SolveApp, Script, egui::Context) {
    let (mut app, script, ctx) = app_with_script();
    let endpoint = "http://localhost:11434/v1";
    app.agent_chat.running_id = "api-1".into();
    app.agent_chat.running_endpoint = endpoint.into();
    app.preferences.agents.push(agents::AgentConfig {
        id: "api-1".into(),
        kind: agents::Kind::Api,
        base_url: endpoint.into(),
        model: "fast".into(),
        ..Default::default()
    });
    script.report(models_reply("fast"));
    poll(&mut app, &ctx);
    (app, script, ctx)
}

/// A typed model goes straight to an endpoint, joins the list for the next connection and becomes
/// the agent's default.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_typed_model_goes_to_an_endpoint_and_is_remembered() {
    let (mut app, script, _ctx) = endpoint_with_script();
    assert!(custom_models_accepted(&app));
    assert!(set_custom_model(&mut app, "  openai/gpt-x "));
    assert_eq!(script.calls(), ["set_model: openai/gpt-x"]);
    assert_eq!(app.agent_chat.models.first().map(|m| m.id.as_str()), Some("openai/gpt-x"));
    assert_eq!(app.agent_chat.models_cache.get("http://localhost:11434/v1").and_then(|l| l.first()).map(|m| m.id.as_str()), Some("openai/gpt-x"));
    assert_eq!(app.preferences.agents.iter().find(|a| a.id == "api-1").unwrap().model, "openai/gpt-x");
    assert_eq!(app.preferences.agent_models.get("api-1").map(String::as_str), Some("openai/gpt-x"));
    assert_eq!(status(&app)["model"], "openai/gpt-x");
    assert_eq!(status(&app)["customModels"], true);
    let listed = app.agent_chat.models.len();
    assert!(set_custom_model(&mut app, "smart"));
    assert_eq!(app.agent_chat.models.len(), listed, "a listed id goes the usual way");
    assert_eq!(app.agent_chat.model, "smart");
}

/// A typed model is only sent when it could be a model at all, and the list stays within its cap.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_typed_model_that_cannot_be_one_is_not_sent() {
    let (mut app, script, _ctx) = endpoint_with_script();
    for text in ["", "   ", "x".repeat(MODEL_ID_CAP + 1).as_str(), "bell\u{7}", "two\nlines"] {
        assert!(!set_custom_model(&mut app, text), "{text:?}");
    }
    assert!(script.calls().is_empty());
    assert!(set_custom_model(&mut app, &"x".repeat(MODEL_ID_CAP)));
    app.agent_chat.models =
        (0..MODEL_LIST_CAP).map(|i| acp::ModelInfo { id: format!("m{i}"), name: String::new(), description: String::new() }).collect();
    assert!(set_custom_model(&mut app, "one-more"));
    assert_eq!(app.agent_chat.models.len(), MODEL_LIST_CAP);
    assert_eq!(app.agent_chat.models.first().map(|m| m.id.as_str()), Some("one-more"));
}

/// An agent with a model option gets a typed model through it. Nothing changes until it answers,
/// and a refusal leaves the old model with the agent's reason in the chat.
#[test]
fn a_typed_model_is_sent_to_a_modern_agent_and_a_rejection_keeps_the_old_one() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "modern".into();
    script.report(Event::ConfigOptions(modern_options("fast")));
    poll(&mut app, &ctx);
    assert!(custom_models_accepted(&app));
    assert!(set_custom_model(&mut app, "claude-x"));
    assert_eq!(script.calls(), ["set_config_option: some.provider.model=claude-x"]);
    assert!(app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "fast");
    assert!(!set_custom_model(&mut app, "claude-y"), "one request at a time");

    script.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Err("no such model".into()) });
    poll(&mut app, &ctx);
    assert!(!app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "fast");
    assert!(
        app.agent_chat
            .items
            .iter()
            .any(|i| matches!(i, Item::Notice { text, error: true } if text.contains("claude-x") && text.contains("no such model"))),
        "the notice names the model and the reason"
    );
    assert!(!app.preferences.agent_models.contains_key("modern"));
    assert_eq!(app.agent_chat.typed_model, None);

    // An agent that takes it says so in its snapshot, and that is what is remembered.
    assert!(set_custom_model(&mut app, "claude-x"));
    script.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("claude-x")) });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "claude-x");
    assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some("claude-x"));
}

/// A legacy agent shows a typed model at once; a refusal puts everything back and says why.
#[test]
fn a_typed_model_is_sent_to_a_legacy_agent_and_a_rejection_reverts_it() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "claude".into();
    app.preferences.agent_models.insert("claude".into(), "fast".into());
    script.report(models_reply("fast"));
    poll(&mut app, &ctx);
    assert!(set_custom_model(&mut app, "zzz"));
    assert_eq!(script.calls(), ["set_model: zzz"]);
    assert_eq!(app.agent_chat.model, "zzz");
    assert_eq!(app.preferences.agent_models.get("claude").map(String::as_str), Some("zzz"));

    script.report(Event::ModelRejected("no such model".into()));
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "fast");
    assert_eq!(app.agent_chat.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["fast", "smart"]);
    assert_eq!(app.preferences.agent_models.get("claude").map(String::as_str), Some("fast"));
    assert!(
        app.agent_chat
            .items
            .iter()
            .any(|i| matches!(i, Item::Notice { text, error: true } if text.contains("zzz") && text.contains("no such model"))),
        "the notice names the model and the reason"
    );

    // A listed model the agent refuses goes back too, and nothing was remembered before it here.
    app.preferences.agent_models.remove("claude");
    set_model(&mut app, "smart");
    script.report(Event::ModelRejected("x".repeat(MODELS_NOTE_CAP * 2)));
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "fast");
    assert_eq!(app.agent_chat.models.len(), 2);
    assert!(!app.preferences.agent_models.contains_key("claude"));
    let Some(Item::Notice { text, .. }) = app.agent_chat.items.last() else { panic!("no notice") };
    assert_eq!(text.chars().count(), MODELS_NOTE_CAP);

    // The agent confirming its model ends the wait: a later refusal has nothing to put back.
    assert!(set_custom_model(&mut app, "zzz"));
    script.report(Event::Models { current: "zzz".into(), available: vec![] });
    script.report(Event::ModelRejected("late".into()));
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "zzz");
}

/// An agent that lists no models and has no model option cannot be sent one; a stopped one neither.
#[test]
fn an_agent_without_models_takes_no_typed_model() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "other".into();
    script.report(Event::Ready { agent: "fake".into(), images: false });
    poll(&mut app, &ctx);
    assert!(!custom_models_accepted(&app));
    assert_eq!(status(&app)["customModels"], false);
    assert!(!set_custom_model(&mut app, "zzz"));
    assert!(script.calls().is_empty());
    script.report(Event::ConfigOptions(vec![]));
    poll(&mut app, &ctx);
    assert!(!custom_models_accepted(&app), "its options name no model");

    let (mut app, script, ctx) = app_with_script();
    script.report(models_reply("fast"));
    poll(&mut app, &ctx);
    assert!(custom_models_accepted(&app));
    app.agent_chat.backend = None;
    assert!(!custom_models_accepted(&app));
    assert!(!set_custom_model(&mut app, "zzz"));
}

/// The control channel's `model`: a listed one as before, an unlisted one when the agent takes
/// typed models, otherwise ignored as before.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn the_control_channel_sends_an_unlisted_model_where_it_can_go() {
    let (mut app, script, ctx) = endpoint_with_script();
    let reply = control(&mut app, &ctx, &json!({"model": "smart"})).unwrap();
    assert_eq!(reply["model"], "smart");
    let reply = control(&mut app, &ctx, &json!({"model": "unlisted"})).unwrap();
    assert_eq!(reply["model"], "unlisted");
    assert_eq!(reply["customModels"], true);
    assert_eq!(reply["modelsNote"], Value::Null);
    assert_eq!(script.calls(), ["set_model: smart", "set_model: unlisted"]);

    let (mut app, script, ctx) = app_with_script();
    let reply = control(&mut app, &ctx, &json!({"model": "unlisted"})).unwrap();
    assert_eq!(reply["model"], "");
    assert_eq!(reply["customModels"], false);
    assert!(script.calls().is_empty());
}

/// The header's picker on an agent that lists models: it searches, a row chooses, and what was typed
/// can be chosen too. The control and the popup stay in a narrow chat's window.
#[test]
fn the_header_model_picker_searches_and_takes_a_typed_model() {
    use crate::scenario::{handle_at, rect_of};
    let mut h = narrow_chat();
    let script = Script::default();
    h.app.agent_chat.backend = Some(Box::new(script.clone()));
    h.app.agent_chat.state = Some(State::Ready);
    h.app.agent_chat.running_id = "claude".into();
    script.report(models_reply("fast"));
    h.frames(4);
    let panel = rect_of("agent_chat:panel").expect("the panel");
    let control = rect_of("agent_chat:model").expect("the picker");
    assert!(control.left() >= panel.left() && control.right() <= panel.right(), "{control:?} in {panel:?}");
    assert!(handle_at("agent_chat:model:search").is_none(), "closed");

    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    for name in ["search", "fast", "smart"] {
        assert!(handle_at(&format!("agent_chat:model:{name}")).is_some(), "{name}");
    }
    assert!(handle_at("agent_chat:model:custom").is_none(), "nothing typed");
    let search = rect_of("agent_chat:model:search").expect("the search box");
    assert!(search.left() >= 0.0 && search.right() <= 1600.0, "inside the harness's window: {search:?}");

    h.step(&json!({"text": "SMA"})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:fast").is_none() && handle_at("agent_chat:model:smart").is_some());
    assert!(handle_at("agent_chat:model:custom").is_some(), "the typed text is not a listed model");
    h.step(&json!({"click": {"handle": "agent_chat:model:smart"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls(), ["set_model: smart"]);
    assert_eq!(h.app.agent_chat.model, "smart");
    assert!(handle_at("agent_chat:model:search").is_none(), "a choice closes it");

    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    h.step(&json!({"text": "zzz"})).unwrap();
    h.frames(3);
    h.step(&json!({"click": {"handle": "agent_chat:model:custom"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls(), ["set_model: smart", "set_model: zzz"]);
    assert_eq!(h.app.agent_chat.model, "zzz");
    disconnect(&mut h.app);
}

/// The picker on an agent with a model option goes by the same names. A row and a typed model both
/// go through the option, and other options stay plain dropdowns.
#[test]
fn the_header_model_option_searches_and_takes_a_typed_model() {
    use crate::scenario::handle_at;
    let mut h = narrow_chat();
    let script = Script::default();
    h.app.agent_chat.backend = Some(Box::new(script.clone()));
    h.app.agent_chat.state = Some(State::Ready);
    script.report(Event::ConfigOptions(modern_options("fast")));
    h.frames(4);
    assert_eq!(handle_at("agent_chat:model"), handle_at("agent_chat:config:some.provider.model"));

    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    for name in ["search", "fast", "smart"] {
        assert!(handle_at(&format!("agent_chat:model:{name}")).is_some(), "{name}");
        assert_eq!(handle_at(&format!("agent_chat:model:{name}")), handle_at(&format!("agent_chat:config:some.provider.model:{name}")));
    }
    h.step(&json!({"click": {"handle": "agent_chat:model:smart"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls(), ["set_config_option: some.provider.model=smart"]);
    script.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
    h.frames(3);
    assert_eq!(h.app.agent_chat.model, "smart");

    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    h.step(&json!({"text": "zzz"})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:custom").is_some());
    h.step(&json!({"click": {"handle": "agent_chat:model:custom"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls().last().map(String::as_str), Some("set_config_option: some.provider.model=zzz"));
    assert_eq!(h.app.agent_chat.model, "smart", "not until the agent says so");
    assert!(h.app.agent_chat.config_pending);
    disconnect(&mut h.app);
}

/// Nothing typed can be chosen where it could not be sent, and the reason a listing failed shows
/// in the popup.
#[test]
fn the_header_model_picker_offers_no_typed_model_it_cannot_send_and_shows_why_listing_failed() {
    use crate::scenario::handle_at;
    let mut h = narrow_chat();
    h.app.agent_chat.state = Some(State::Ready);
    h.app.agent_chat.models = vec![acp::ModelInfo { id: "fast".into(), name: "fast".into(), description: String::new() }];
    h.app.agent_chat.model = "fast".into();
    h.app.agent_chat.models_note = Some("Couldn't list models: offline".into());
    h.frames(4);
    assert!(!custom_models_accepted(&h.app));
    h.step(&json!({"click": {"handle": "agent_chat:model"}})).unwrap();
    h.frames(3);
    assert!(h.widget("Couldn't list models: offline").is_some());
    h.step(&json!({"text": "zzz"})).unwrap();
    h.frames(3);
    assert!(handle_at("agent_chat:model:search").is_some());
    assert!(handle_at("agent_chat:model:custom").is_none());
}

/// A running endpoint's card uses the chat's list and changes the chat's model.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn the_card_of_a_running_endpoint_changes_the_chats_model() {
    use crate::scenario::handle_at;
    let mut h = crate::scenario::Harness::new();
    h.app.home.open = false;
    h.app.preferences.ai_features = true;
    h.app.agent_chat.detection = Some(Default::default());
    let script = Script::default();
    h.app.agent_chat.backend = Some(Box::new(script.clone()));
    h.app.agent_chat.state = Some(State::Ready);
    h.app.agent_chat.running_id = "api-1".into();
    h.app.agent_chat.running_endpoint = "http://localhost:1/v1".into();
    h.app.preferences.agents.push(agents::AgentConfig {
        id: "api-1".into(),
        name: "Endpoint".into(),
        kind: agents::Kind::Api,
        base_url: "http://localhost:1/v1".into(),
        model: "fast".into(),
        ..Default::default()
    });
    script.report(models_reply("fast"));
    h.step(&json!({"call": "ui.prefs", "params": {"open": "AI"}})).unwrap();
    h.frames(4);
    // The permission cards put endpoint controls below the fixed page's viewport.
    for _ in 0..32 {
        let at = handle_at("prefs:ai_model:api-1").unwrap();
        let body = crate::scenario::rect_of("prefs:page_body").unwrap();
        if body.contains(at) {
            break;
        }
        h.call("ui.scroll", json!({"x":body.center().x,"y":body.center().y,"delta":if at.y < body.top() {100} else {-100}}));
        h.frames(30);
    }
    assert!(crate::scenario::rect_of("prefs:page_body").unwrap().contains(handle_at("prefs:ai_model:api-1").unwrap()));
    h.step(&json!({"click": {"handle": "prefs:ai_model:api-1"}})).unwrap();
    h.frames(3);
    assert!(handle_at("prefs:ai_model:api-1:smart").is_some());
    h.step(&json!({"click": {"handle": "prefs:ai_model:api-1:smart"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls(), ["set_model: smart"]);
    assert_eq!(h.app.agent_chat.model, "smart");
    h.step(&json!({"click": {"handle": "prefs:ai_model:api-1"}})).unwrap();
    h.frames(3);
    h.step(&json!({"text": "zzz"})).unwrap();
    h.frames(3);
    h.step(&json!({"click": {"handle": "prefs:ai_model:api-1:custom"}})).unwrap();
    h.frames(3);
    assert_eq!(script.calls(), ["set_model: smart", "set_model: zzz"]);
    assert_eq!(h.app.preferences.agents.iter().find(|a| a.id == "api-1").map(|a| a.model.as_str()), Some("zzz"));
    disconnect(&mut h.app);
}

/// A listing that fails says why and leaves last time's list, with the configured model in it; the
/// next connection starts without the note.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_listing_that_fails_says_why_and_keeps_the_list_of_last_time() {
    let (mut app, script, ctx) = app_with_script();
    let endpoint = "http://localhost:11434/v1";
    app.agent_chat.running_endpoint = endpoint.into();
    script.report(Event::ModelsNote(None));
    script.report(models_reply("fast"));
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.models_note, None);
    let ids = |app: &SolveApp| app.agent_chat.models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&app), ["fast", "smart"]);

    let only = |id: &str| acp::ModelInfo { id: id.into(), name: id.into(), description: String::new() };
    script.report(Event::ModelsNote(Some("x".repeat(MODELS_NOTE_CAP + 10))));
    script.report(Event::Models { current: "local".into(), available: vec![only("local")] });
    poll(&mut app, &ctx);
    let note = app.agent_chat.models_note.clone().unwrap();
    assert!(note.starts_with(&trf("ui.agent_chat_models_failed", &[("error", "x")])), "{note}");
    assert_eq!(note.chars().count(), MODELS_NOTE_CAP);
    assert_eq!(ids(&app), ["local", "fast", "smart"]);
    assert_eq!(app.agent_chat.model, "local");
    assert_eq!(app.agent_chat.models_cache.get(endpoint).map(Vec::len), Some(2), "the failed listing is not remembered");

    script.report(Event::ModelsNote(Some("again".into())));
    script.report(Event::Models { current: "fast".into(), available: vec![only("fast")] });
    poll(&mut app, &ctx);
    assert_eq!(ids(&app), ["local", "fast", "smart"], "a model already listed is not listed twice");

    // A real listing on an endpoint that cannot be reached.
    let mut app = app_with_endpoints();
    app.agent_chat.models_note = Some("from before".into());
    app.agent_chat.agent_id = Some("api-1".into());
    connect(&mut app, &ctx);
    assert_eq!(app.agent_chat.models_note, None);
    let start = std::time::Instant::now();
    while app.agent_chat.models.is_empty() && start.elapsed() < std::time::Duration::from_secs(10) {
        poll(&mut app, &ctx);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(app.agent_chat.models_note.as_deref().is_some_and(|note| note.contains("offline")), "{:?}", app.agent_chat.models_note);
    assert_eq!(ids(&app), ["m"]);
    assert!(models_shown(&app));
    disconnect(&mut app);
}

/// The next session puts back the model chosen last time, when the agent offers it.
#[test]
fn the_remembered_model_is_set_on_the_next_session() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "claude".into();
    app.preferences.agent_models.insert("claude".into(), "smart".into());
    script.report(models_reply("fast"));
    script.report(Event::Ready { agent: "fake".into(), images: false });
    poll(&mut app, &ctx);
    assert_eq!(script.calls(), ["set_model: smart"]);
    assert_eq!(app.agent_chat.model, "smart");
    // Already on it, or no longer offered: nothing is sent.
    script.report(models_reply("smart"));
    script.report(Event::Ready { agent: "fake".into(), images: false });
    poll(&mut app, &ctx);
    app.preferences.agent_models.insert("claude".into(), "gone".into());
    script.report(models_reply("fast"));
    script.report(Event::Ready { agent: "fake".into(), images: false });
    poll(&mut app, &ctx);
    assert_eq!(script.calls().len(), 1);
}

fn provider_models() -> Vec<acp::ModelInfo> {
    let m = |id: &str, name: &str, description: &str| acp::ModelInfo { id: id.into(), name: name.into(), description: description.into() };
    vec![
        m("nous:anthropic/opus", "Nous Portal · anthropic/opus", "Provider: Nous Portal"),
        m("nous:deepseek/v4", "Nous Portal · deepseek/v4", "Provider: Nous Portal"),
        m("custom:host:deepseek-v4.1-flash", "deepseek-v4.1-flash", "Provider: Host • current"),
        m("custom:host:haiku", "haiku", "Provider: Host"),
        m("plain", "plain", ""),
    ]
}

/// Models from several providers: the header picks a provider, then one of its models, listed
/// without the provider in front. Picking a provider alone leaves the agent's model as it is.
#[test]
fn models_from_several_providers_are_picked_by_provider_first() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "hermes".into();
    script.report(Event::Models { current: "custom:host:deepseek-v4.1-flash".into(), available: provider_models() });
    poll(&mut app, &ctx);
    let providers = model_providers(&app);
    assert_eq!(providers, [("Nous Portal".to_string(), 2), ("Host".to_string(), 2), ("Other".to_string(), 1)]);
    assert_eq!(shown_provider(&app).as_deref(), Some("Host"), "the current model's provider");
    let names = |app: &SolveApp| listed_models(app).into_iter().map(|(_, n)| n).collect::<Vec<_>>();
    assert_eq!(names(&app), ["deepseek-v4.1-flash", "haiku"]);
    set_model_provider(&mut app, "Nous Portal");
    set_model_provider(&mut app, "nonsense");
    assert_eq!(names(&app), ["anthropic/opus", "deepseek/v4"]);
    assert!(script.calls().is_empty(), "picking a provider sends nothing");
    assert_eq!(app.agent_chat.model, "custom:host:deepseek-v4.1-flash");
    let s = status(&app);
    assert_eq!(s["provider"], "Nous Portal");
    assert_eq!(s["providers"][0], json!({"name": "Nous Portal", "count": 2}));
    set_model(&mut app, "nous:deepseek/v4");
    assert_eq!(script.calls(), ["set_model: nous:deepseek/v4"]);
    assert_eq!(shown_provider(&app).as_deref(), Some("Nous Portal"));
    // The agent switching to another provider's model shows that provider.
    set_model_provider(&mut app, "Host");
    script.report(Event::Models { current: "plain".into(), available: vec![] });
    poll(&mut app, &ctx);
    assert_eq!(shown_provider(&app).as_deref(), Some("Other"));
    assert_eq!(names(&app), ["plain"]);
}

/// Models from one provider, or with none named: one list, every model as the agent names it.
#[test]
fn models_from_one_provider_keep_one_list() {
    let (mut app, script, ctx) = app_with_script();
    script.report(models_reply("fast"));
    poll(&mut app, &ctx);
    assert!(model_providers(&app).is_empty());
    assert_eq!(shown_provider(&app), None);
    assert_eq!(listed_models(&app).into_iter().map(|(_, n)| n).collect::<Vec<_>>(), ["FAST", "SMART"]);
    assert_eq!(status(&app)["provider"], Value::Null);
    let one = |name: &str| acp::ModelInfo { id: name.into(), name: format!("Only · {name}"), description: String::new() };
    script.report(Event::Models { current: "a".into(), available: vec![one("a"), one("b")] });
    poll(&mut app, &ctx);
    assert!(model_providers(&app).is_empty(), "a single provider needs no provider picker");
    assert_eq!(listed_models(&app)[0].1, "Only · a");
}

/// A provider name from the agent is bounded, and `·` or `Provider:` without a name is no provider.
#[test]
fn hostile_provider_names_are_bounded() {
    let m = |name: String, description: &str| acp::ModelInfo { id: "x".into(), name, description: description.into() };
    let long = "p".repeat(10_000);
    let (provider, name) = model_provider(&m(format!("{long} · model"), "")).unwrap();
    assert_eq!((provider.chars().count(), name.as_str()), (128, "model"));
    assert_eq!(model_provider(&m(" · model".into(), "")), None);
    assert_eq!(model_provider(&m("model".into(), "Provider:  ")), None);
    assert_eq!(model_provider(&m("model".into(), "Provider: A\nsecond line • current")).map(|p| p.0), Some("A".into()));
}

fn modern_options(model: &str) -> Vec<acp::ConfigOption> {
    vec![
        acp::ConfigOption {
            id: "some.provider.model".into(),
            name: "Model".into(),
            description: String::new(),
            category: "model".into(),
            current_value: model.into(),
            options: ["fast", "smart"]
                .into_iter()
                .map(|v| acp::ConfigChoice { value: v.into(), name: v.into(), description: String::new(), group: None })
                .collect(),
        },
        acp::ConfigOption {
            id: "custom-mode-key".into(),
            name: "Mode".into(),
            description: String::new(),
            category: "mode".into(),
            current_value: "ask".into(),
            options: ["ask", "plan"]
                .into_iter()
                .map(|v| acp::ConfigChoice { value: v.into(), name: v.into(), description: String::new(), group: None })
                .collect(),
        },
    ]
}

#[test]
fn modern_snapshots_replace_provider_choices_and_ignore_legacy_updates() {
    let (mut app, script, ctx) = app_with_script();
    script.report(Event::Models { current: "plain".into(), available: provider_models() });
    poll(&mut app, &ctx);
    set_model_provider(&mut app, "Host");
    script.report(Event::ConfigOptions(modern_options("smart")));
    script.report(Event::Models { current: "plain".into(), available: provider_models() });
    script.report(Event::Modes { current: "old-mode".into(), available: vec![("old-mode".into(), "Old mode".into())] });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "smart");
    assert_eq!(app.agent_chat.mode, "ask");
    assert!(app.agent_chat.model_provider.is_none());
    assert!(model_providers(&app).is_empty());
    assert_eq!(listed_models(&app).into_iter().map(|(m, _)| m.id).collect::<Vec<_>>(), ["fast", "smart"]);
    script.report(Event::ConfigOptions(vec![]));
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.config_options, Some(vec![]));
    assert!(app.agent_chat.models.is_empty() && app.agent_chat.modes.is_empty());
    set_model(&mut app, "plain");
    set_mode(&mut app, "old-mode");
    assert!(script.calls().is_empty(), "an empty modern snapshot never falls back to stale legacy choices");
}

#[test]
fn modern_acknowledgements_stay_with_their_document_without_resuming_its_queue() {
    let (mut app, first, ctx) = app_with_script();
    app.agent_chat.running_id = "first-modern".into();
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
    app.preferences.agent_models.insert("first-modern".into(), "smart".into());
    new_chat(&mut app);
    send(&mut app, &ctx, "first request".into());
    first.report(Event::ConfigOptions(modern_options("fast")));
    first.report(Event::Ready { agent: "first-modern".into(), images: false });
    poll(&mut app, &ctx);
    assert!(app.agent_chat.config_pending && app.agent_chat.queued.is_some());

    crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
    assert!(app.agent_chat.config_options.is_none() && !app.agent_chat.config_pending);
    let second = Script::default();
    app.agent_chat.backend = Some(Box::new(second.clone()));
    app.agent_chat.state = Some(State::Ready);
    app.agent_chat.running_id = "second-modern".into();
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
    set_model(&mut app, "smart");
    first.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
    poll(&mut app, &ctx);
    assert!(app.agent_chat.config_pending, "the hidden tab's acknowledgement cannot unlock this one");
    assert_eq!(app.agent_chat.model, "fast");
    assert!(!app.preferences.agent_models.contains_key("second-modern"));
    second.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
    poll(&mut app, &ctx);
    assert_eq!(app.preferences.agent_models.get("second-modern").map(String::as_str), Some("smart"));

    crate::documents::switch(&mut app, 0);
    assert!(app.agent_chat.config_pending, "pending configuration belongs to the conversation");
    poll(&mut app, &ctx);
    assert!(!app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "smart");
    assert_eq!(app.preferences.agent_models.get("first-modern").map(String::as_str), Some("smart"));
    assert_eq!(texts(&app), ["first request"]);
    assert!(app.agent_chat.queued.is_none() && app.agent_chat.stopped);
    assert!(first.prompts().is_empty() && second.prompts().is_empty(), "a late configuration reply cannot resume a departed tab's prompt");
}

#[test]
fn closing_a_pending_modern_chat_preserves_the_surviving_api_endpoint_and_saved_models() {
    for close_active in [false, true] {
        let (mut app, modern, ctx) = app_with_script();
        app.agent_chat.running_id = "modern".into();
        app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
        app.preferences.agent_models.insert("modern".into(), "fast".into());
        set_model(&mut app, "smart");
        assert!(app.agent_chat.config_pending);

        crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
        let endpoint = "http://127.0.0.1:12345/v1";
        let api = Script::default();
        app.agent_chat.backend = Some(Box::new(api.clone()));
        app.agent_chat.state = Some(State::Ready);
        app.agent_chat.running_id = "endpoint-agent".into();
        app.agent_chat.running_endpoint = endpoint.into();
        app.preferences.agents.push(agents::AgentConfig {
            id: "endpoint-agent".into(),
            kind: agents::Kind::Api,
            base_url: endpoint.into(),
            model: "fast".into(),
            ..Default::default()
        });
        api.report(models_reply("fast"));
        poll(&mut app, &ctx);
        set_model(&mut app, "smart");
        let cached = app.agent_chat.models_cache.get(endpoint).cloned().unwrap();
        modern.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
        modern.report(Event::ConfigOptions(vec![]));
        if close_active {
            crate::documents::switch(&mut app, 0);
            assert!(app.agent_chat.config_pending);
        }
        crate::documents::close(&mut app, 0, true);
        poll(&mut app, &ctx);
        assert_eq!(app.agent_chat.running_id, "endpoint-agent");
        assert_eq!(app.agent_chat.running_endpoint, endpoint);
        assert!(app.agent_chat.config_options.is_none() && !app.agent_chat.config_pending);
        assert_eq!(app.agent_chat.model, "smart");
        assert_eq!(app.agent_chat.models_cache.get(endpoint), Some(&cached));
        assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some("fast"));
        assert_eq!(app.preferences.agent_models.get("endpoint-agent").map(String::as_str), Some("smart"));
        assert_eq!(app.preferences.agents.iter().find(|a| a.id == "endpoint-agent").unwrap().model, "smart");
        assert_eq!(api.calls(), ["set_model: smart"], "the closed chat's events cannot reach the surviving backend");

        // Reusing the same agent in a fresh design must restore only the last confirmed model.
        crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
        assert!(app.agent_chat.config_options.is_none() && !app.agent_chat.config_pending);
        let fresh = Script::default();
        app.agent_chat.backend = Some(Box::new(fresh.clone()));
        app.agent_chat.state = Some(State::Starting);
        app.agent_chat.running_id = "modern".into();
        fresh.report(Event::ConfigOptions(modern_options("fast")));
        fresh.report(Event::Ready { agent: "modern".into(), images: false });
        poll(&mut app, &ctx);
        assert_eq!(app.agent_chat.model, "fast");
        assert!(!app.agent_chat.config_pending);
        assert!(fresh.calls().is_empty(), "an unacknowledged choice from the closed chat is not restored");
        assert_eq!(app.agent_chat.models_cache.get(endpoint), Some(&cached));
    }
}

#[test]
fn document_disconnect_paths_clear_modern_configuration_and_pending_requests() {
    let assert_cleared = |app: &SolveApp| {
        assert!(app.agent_chat.backend.is_none());
        assert!(app.agent_chat.config_options.is_none() && !app.agent_chat.config_pending);
        assert!(app.agent_chat.models.is_empty() && app.agent_chat.modes.is_empty());
        assert!(app.agent_chat.model.is_empty() && app.agent_chat.mode.is_empty());
        assert!(app.agent_chat.model_provider.is_none());
        assert_eq!(app.agent_chat.state(), State::Off);
    };
    let (mut app, _, _) = app_with_script();
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
    set_model(&mut app, "smart");
    disconnect(&mut app);
    assert_cleared(&app);

    for quitting in [false, true] {
        let (mut app, first, ctx) = app_with_script();
        app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
        set_model(&mut app, "smart");
        crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
        app.agent_chat.backend = Some(Box::new(Script::default()));
        app.agent_chat.state = Some(State::Ready);
        app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
        set_model(&mut app, "smart");
        first.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
        if quitting {
            shutdown(&mut app);
        } else {
            app.preferences.ai_features = false;
            poll(&mut app, &ctx);
        }
        assert_cleared(&app);
        crate::documents::switch(&mut app, 0);
        assert_cleared(&app);
        assert!(app.preferences.agent_models.is_empty(), "an unpolled acknowledgement is discarded with its backend");
    }
}

#[cfg(unix)]
#[test]
fn modern_config_requests_roundtrip_success_rejection_malformed_and_retry() {
    let dir = std::env::temp_dir().join(format!("sc-chat-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("requests.jsonl");
    let script = r#"
log="$1"; n=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"agentInfo":{"name":"fake"}}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1","configOptions":[{"id":"vendor.model-key","name":"Model","category":"model","type":"select","currentValue":"fast","options":[{"value":"fast","name":"Fast"},{"value":"smart","name":"Smart"}]}]}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*)
      n=$((n+1))
      case "$n" in
        1|4) v=smart; [ "$n" = 4 ] && v=fast; printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[{"id":"vendor.model-key","name":"Model","category":"model","type":"select","currentValue":"%s","options":[{"value":"fast","name":"Fast"},{"value":"smart","name":"Smart"}]}]}}\n' "$id" "$v" ;;
        2) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"rejected"}}\n' "$id" ;;
        3) printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[{"type":"select"}]}}\n' "$id" ;;
      esac ;;
  esac
done
"#;
    let argv = vec!["sh".into(), "-c".into(), script.into(), "fake".into(), log.to_string_lossy().into_owned()];
    let (wake_tx, wake_rx) = channel();
    let start = backend::StartContext {
        cwd: dir.clone(),
        bridge: None,
        tools: None,
        wake: Box::new(move || {
            let _ = wake_tx.send(());
        }),
    };
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.backend = Some(Box::new(acp_client::AcpBackend::start(&argv, start).unwrap()));
    app.agent_chat.running_id = "modern".into();
    app.agent_chat.state = Some(State::Starting);
    let ctx = crate::fonts::test_context();
    let until = |app: &mut SolveApp, done: &dyn Fn(&SolveApp) -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            poll(app, &ctx);
            if done(app) {
                return;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "timed out; status={}", status(app));
            wake_rx.recv_timeout(left).expect("backend wake");
        }
    };
    until(&mut app, &|a| a.agent_chat.state() == State::Ready);
    assert_eq!(app.agent_chat.model, "fast");
    for (value, expected, saved) in [("smart", "smart", "smart"), ("fast", "smart", "smart"), ("smart", "smart", "smart"), ("fast", "fast", "fast")] {
        set_model(&mut app, value);
        assert!(app.agent_chat.config_pending);
        until(&mut app, &|a| !a.agent_chat.config_pending);
        assert_eq!(app.agent_chat.model, expected);
        assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some(saved));
    }
    disconnect(&mut app);
    let requests: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let configs: Vec<_> = requests.iter().filter(|v| v["method"] == "session/set_config_option").collect();
    assert_eq!(configs.len(), 4);
    assert_eq!(
        configs.iter().map(|v| v["params"].clone()).collect::<Vec<_>>(),
        ["smart", "fast", "smart", "fast"].map(|value| json!({"sessionId":"s1","configId":"vendor.model-key","value":value}))
    );
    assert!(!requests.iter().any(|v| v["method"] == "session/set_model"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn modern_controls_route_by_category_and_wait_for_acknowledgement() {
    let (mut app, script, _) = app_with_script();
    app.agent_chat.running_id = "modern".into();
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
    set_model(&mut app, "smart");
    assert!(app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "fast");
    assert!(!app.preferences.agent_models.contains_key("modern"));
    assert_eq!(script.calls(), ["set_config_option: some.provider.model=smart"]);
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
    assert!(app.agent_chat.config_pending, "a notification does not acknowledge the request");
    app.agent_chat.on_event(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
    assert!(!app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "smart");
    app.preferences.agent_models.insert("modern".into(), app.agent_chat.model.clone());
    set_mode(&mut app, "ask");
    assert_eq!(script.calls().last().map(String::as_str), Some("set_config_option: custom-mode-key=ask"));
}

#[test]
fn modern_rejection_and_malformed_success_unlock_without_changing_confirmed_value() {
    let (mut app, script, _) = app_with_script();
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("fast")));
    set_model(&mut app, "smart");
    app.agent_chat.on_event(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Err("rejected".into()) });
    assert!(!app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "fast");
    set_model(&mut app, "smart");
    app.agent_chat.on_event(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Err("malformed snapshot".into()) });
    assert!(!app.agent_chat.config_pending);
    assert_eq!(app.agent_chat.model, "fast");
    assert_eq!(script.calls().len(), 2);
}

#[test]
fn modern_saved_model_restore_waits_for_ack_and_manual_confirmation_persists() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "modern".into();
    app.preferences.agent_models.insert("modern".into(), "smart".into());
    script.report(Event::ConfigOptions(modern_options("fast")));
    script.report(Event::Ready { agent: "modern".into(), images: false });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "fast");
    assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some("smart"));
    assert_eq!(script.calls(), ["set_config_option: some.provider.model=smart"]);
    script.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.model, "smart");
    assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some("smart"));
}

#[test]
fn modern_model_restore_then_mode_restore_are_acknowledged_before_queued_prompt() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "modern".into();
    app.preferences.agent_models.insert("modern".into(), "smart".into());
    app.agent_chat.restore_mode = Some("plan".into());
    app.agent_chat.queued = Some(("describe the model".into(), false, None, Vec::new()));
    script.report(Event::ConfigOptions(modern_options("fast")));
    script.report(Event::Ready { agent: "modern".into(), images: false });
    poll(&mut app, &ctx);
    assert_eq!(script.calls(), ["set_config_option: some.provider.model=smart"]);
    assert_eq!(app.agent_chat.model, "fast");
    assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some("smart"));
    script.report(Event::ConfigOptionFinished { id: "some.provider.model".into(), result: Ok(modern_options("smart")) });
    poll(&mut app, &ctx);
    assert_eq!(script.calls().get(1).map(String::as_str), Some("set_config_option: custom-mode-key=plan"));
    assert!(app.agent_chat.queued.is_some(), "prompt waits for mode confirmation");
    let mut confirmed = modern_options("smart");
    confirmed[1].current_value = "plan".into();
    script.report(Event::ConfigOptionFinished { id: "custom-mode-key".into(), result: Ok(confirmed) });
    poll(&mut app, &ctx);
    assert!(script.calls().iter().any(|s| s == "prompt: describe the model"));
    assert_eq!(app.preferences.agent_models.get("modern").map(String::as_str), Some("smart"));
}

#[test]
fn invalidated_modern_sessions_do_not_expose_stale_legacy_choices() {
    for event in [Event::AuthRequired { methods: vec![] }, Event::Exited { tail: vec![] }] {
        let (mut app, _, _) = app_with_script();
        app.agent_chat.on_event(Event::ConfigOptions(modern_options("smart")));
        app.agent_chat.config_pending = true;
        app.agent_chat.on_event(event);
        assert!(app.agent_chat.config_options.is_none() && !app.agent_chat.config_pending);
        assert!(app.agent_chat.models.is_empty() && app.agent_chat.modes.is_empty());
        assert!(app.agent_chat.model.is_empty() && app.agent_chat.mode.is_empty());
    }
    let (mut app, _, _) = app_with_script();
    app.agent_chat.on_event(Event::ConfigOptions(modern_options("smart")));
    new_chat(&mut app);
    assert_eq!(app.agent_chat.restore_mode.as_deref(), Some("ask"));
    assert!(app.agent_chat.config_options.is_none() && app.agent_chat.models.is_empty());
}

/// An agent that reports no models gets no picker; another agent's choice is not applied to it.
#[test]
fn an_agent_without_models_shows_no_picker() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.running_id = "other".into();
    app.preferences.agent_models.insert("claude".into(), "smart".into());
    script.report(Event::Ready { agent: "fake".into(), images: false });
    poll(&mut app, &ctx);
    assert!(!models_shown(&app));
    assert!(script.calls().is_empty());
    assert_eq!(status(&app)["models"], json!([]));
}

/// With a real ACP agent the choice goes out as `session/set_model` with the session and model.
#[cfg(unix)]
#[test]
fn set_model_goes_to_an_acp_agent() {
    let dir = std::env::temp_dir().join(format!("sc-agent-chat-model-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("received.log");
    let script = r#"
log="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"agentInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1","models":{"currentModelId":"fast","availableModels":[{"modelId":"fast","name":"Fast"},{"modelId":"smart","name":"Smart"}]}}}\n' "$id" ;;
    *'"method":"session/set_model"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string(), "fake-agent".to_string(), log.to_string_lossy().into_owned()];
    let start = backend::StartContext { cwd: dir.clone(), bridge: None, tools: None, wake: Box::new(|| {}) };
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.backend = Some(Box::new(acp_client::AcpBackend::start(&argv, start).unwrap()));
    app.agent_chat.state = Some(State::Starting);
    app.agent_chat.running_id = "fake".into();
    app.preferences.agent_models.insert("fake".into(), "smart".into());
    let ctx = crate::fonts::test_context();
    let t = std::time::Instant::now();
    loop {
        poll(&mut app, &ctx);
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        if logged.contains("session/set_model") {
            break;
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "no set_model: {logged}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(models_shown(&app) && app.agent_chat.model == "smart", "the remembered model was put back");
    let logged = std::fs::read_to_string(&log).unwrap();
    let set: Value = logged.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v["method"] == "session/set_model").unwrap();
    assert_eq!(set["params"], json!({"sessionId": "s1", "modelId": "smart"}));
    // A choice made in the picker goes out too.
    set_model(&mut app, "fast");
    let t = std::time::Instant::now();
    while std::fs::read_to_string(&log).unwrap_or_default().matches("session/set_model").count() < 2 {
        assert!(t.elapsed() < std::time::Duration::from_secs(20));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    disconnect(&mut app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An agent that lists `models` but does not serve the unstable `session/set_model` answers
/// "method not found": the choice stays and no error shows (#9970). Other errors still show, as
/// the refusal of the model they were about.
#[cfg(unix)]
#[test]
fn set_model_method_not_found_shows_no_error() {
    let dir = std::env::temp_dir().join(format!("sc-agent-chat-model-unserved-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("received.log");
    let script = r#"
log="$1"; n=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"agentInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1","models":{"currentModelId":"fast","availableModels":[{"modelId":"fast","name":"Fast"},{"modelId":"smart","name":"Smart"}]}}}\n' "$id" ;;
    *'"method":"session/set_model"'*)
      n=$((n+1))
      if [ "$n" = 1 ]; then printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"\\"Method not found\\": session/set_model"}}\n' "$id"
      else printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"model unavailable"}}\n' "$id"; fi ;;
  esac
done
"#;
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string(), "fake-agent".to_string(), log.to_string_lossy().into_owned()];
    let start = backend::StartContext { cwd: dir.clone(), bridge: None, tools: None, wake: Box::new(|| {}) };
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.backend = Some(Box::new(acp_client::AcpBackend::start(&argv, start).unwrap()));
    app.agent_chat.state = Some(State::Starting);
    app.agent_chat.running_id = "fake".into();
    let ctx = crate::fonts::test_context();
    let errors = |app: &SolveApp| -> Vec<String> {
        app.agent_chat.items.iter().filter_map(|i| if let Item::Notice { text, error: true } = i { Some(text.clone()) } else { None }).collect()
    };
    let t = std::time::Instant::now();
    while !(app.agent_chat.state() == State::Ready && models_shown(&app)) {
        poll(&mut app, &ctx);
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "agent not ready: {}", status(&app));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    set_model(&mut app, "smart");
    set_model(&mut app, "fast");
    // Replies come in order: once the second one's error shows, the first reply was handled too.
    let t = std::time::Instant::now();
    let refused = trf("ui.agent_chat_model_rejected", &[("model", "fast"), ("error", "model unavailable")]);
    while !errors(&app).contains(&refused) {
        poll(&mut app, &ctx);
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "no error for the second choice: {:?}", errors(&app));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(errors(&app), [refused], "method not found is no error to show");
    assert!(app.agent_chat.state().running(), "the chat goes on");
    assert_eq!(std::fs::read_to_string(&log).unwrap().matches("session/set_model").count(), 2);
    disconnect(&mut app);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Files attached to a message ----

/// A folder of files to attach: a picture and a text file.
fn attachable(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("solidcraft-chat-files-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    image::RgbaImage::from_pixel(40, 30, image::Rgba([250, 250, 250, 255])).save(d.join("target.png")).unwrap();
    std::fs::write(d.join("brief.md"), "wall 2 mm, Pi 5 inside\n").unwrap();
    d
}

fn at(d: &std::path::Path, f: &str) -> String {
    d.join(f).to_string_lossy().to_string()
}

/// Wait for the files read off the window thread to join the message.
fn settle_files(app: &mut SolveApp, ctx: &egui::Context) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !app.agent_chat.loading.is_empty() && std::time::Instant::now() < deadline {
        take_loaded(app, ctx);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    take_loaded(app, ctx);
}

/// The same with the whole app running frames.
fn settle_chat(h: &mut crate::scenario::Harness) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !h.app.agent_chat.loading.is_empty() && std::time::Instant::now() < deadline {
        h.frames(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.frames(2);
}

fn text_of(parts: &[PromptPart]) -> String {
    parts.iter().filter_map(|p| if let PromptPart::Text(t) = p { Some(t.clone()) } else { None }).collect()
}

fn pictures_in(parts: &[PromptPart]) -> usize {
    parts.iter().filter(|p| matches!(p, PromptPart::Image { .. })).count()
}

/// Files attached to a message go with it: the pictures as images, the text files in its text,
/// and every file by name; the message lists them in the conversation.
#[test]
fn a_message_carries_its_files() {
    let d = attachable("carry");
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.images = true;
    control(&mut app, &ctx, &json!({"attach": at(&d, "target.png")})).unwrap();
    control(&mut app, &ctx, &json!({"attach": at(&d, "brief.md")})).unwrap();
    settle_files(&mut app, &ctx);
    assert_eq!(status(&app)["attachments"], json!(["target.png", "brief.md"]));
    assert!(control(&mut app, &ctx, &json!({"attach": at(&d, "missing.png")})).is_err());
    send(&mut app, &ctx, "make the case like this".into());
    let sent = script.prompts();
    assert_eq!(sent.len(), 1);
    let text = text_of(&sent[0]);
    assert!(text.starts_with("make the case like this"), "{text}");
    assert!(text.contains("Attached pictures: target.png.") && text.contains("wall 2 mm, Pi 5 inside"), "{text}");
    assert_eq!(sent[0].iter().filter(|p| matches!(p, PromptPart::Image { mime, .. } if mime == "image/png")).count(), 1);
    assert!(app.agent_chat.attachments.is_empty(), "the files went with the message");
    // The picture shows as a thumbnail with the viewer (its name is the note); the text file by name.
    assert_eq!(status(&app)["items"][0]["files"], json!(["brief.md"]));
    assert_eq!(status(&app)["items"][0]["pictures"], 1);
    assert!(matches!(&app.agent_chat.items[0], Item::User { pictures, .. } if pictures.first().is_some_and(|p| p.note == "target.png" && !p.view)));
    let _ = std::fs::remove_dir_all(&d);
}

/// Review of #1744: a picture was decoded on the window thread, so a large one froze the window.
/// It is read off it now: the message waits for it (Send waits too), the files keep the order they
/// were attached in, and a refused picture says why later, in the app's language.
#[test]
fn pictures_are_read_off_the_window_thread() {
    let d = attachable("worker");
    std::fs::write(d.join("fake.png"), b"not a picture at all").unwrap();
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.images = true;
    control(&mut app, &ctx, &json!({"attach": at(&d, "target.png")})).unwrap();
    control(&mut app, &ctx, &json!({"attach": at(&d, "brief.md")})).unwrap();
    assert_eq!(status(&app)["loading"], json!(["target.png"]), "the picture is still being read");
    send(&mut app, &ctx, "like this".into());
    assert!(script.prompts().is_empty(), "Send waits for the files");
    assert_eq!(app.agent_chat.input, "like this", "and keeps the words");
    settle_files(&mut app, &ctx);
    assert_eq!(status(&app)["attachments"], json!(["target.png", "brief.md"]), "in the order they were attached");
    let words = std::mem::take(&mut app.agent_chat.input);
    send(&mut app, &ctx, words);
    assert_eq!(script.prompts().len(), 1);
    {
        let _ja = crate::i18n::scope("ja");
        control(&mut app, &ctx, &json!({"attach": at(&d, "fake.png")})).unwrap();
    }
    settle_files(&mut app, &ctx);
    assert!(
        matches!(app.agent_chat.items.last(), Some(Item::Notice { text, error: true }) if text == "fake.png: 読み込めない画像です"),
        "a refused picture says why, in the language it was attached in"
    );
    assert!(app.agent_chat.attachments.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// Review of #1744: the prompt cap applied to the typed text only, so eight 256 KB text files
/// took a message to about 2 MB. The whole message stays within it now, every file still named.
#[test]
fn a_message_with_large_text_files_stays_within_the_prompt_cap() {
    let d = attachable("cap");
    let (mut app, script, ctx) = app_with_script();
    for i in 0..attach::MAX_FILES {
        let path = d.join(format!("notes{i}.txt"));
        std::fs::write(&path, "y".repeat(250 * 1024)).unwrap();
        control(&mut app, &ctx, &json!({"attach": path.to_string_lossy()})).unwrap();
    }
    send(&mut app, &ctx, "read these".into());
    let text = text_of(&script.prompts()[0]);
    assert!(text.chars().count() <= MAX_PROMPT, "{} characters", text.chars().count());
    assert!((0..attach::MAX_FILES).all(|i| text.contains(&format!("`notes{i}.txt`"))), "every file is named");
    let _ = std::fs::remove_dir_all(&d);
}

/// A message sent before the agent is ready keeps its files until it goes; an agent that takes
/// no pictures gets none, and the message says which were left out. Files alone make a message,
/// and its undo step is named after the first file.
#[test]
fn files_wait_with_their_message_and_pictures_skip_an_agent_that_takes_none() {
    let d = attachable("wait");
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.state = Some(State::Starting);
    attach_path(&mut app, &at(&d, "target.png")).unwrap();
    settle_files(&mut app, &ctx);
    send(&mut app, &ctx, "  ".into());
    assert!(script.prompts().is_empty(), "not sent before the agent is ready");
    assert_eq!(status(&app)["items"][0]["user"], "", "no words, just the file");
    script.report(Event::Ready { agent: "Test".into(), images: false });
    poll(&mut app, &ctx);
    let sent = script.prompts();
    assert_eq!(sent.len(), 1, "sent once the agent is ready");
    assert_eq!(pictures_in(&sent[0]), 0, "no pictures to an agent that takes none");
    assert!(text_of(&sent[0]).contains("not sent"), "{}", text_of(&sent[0]));
    assert_eq!(app.agent_chat.turn.as_ref().map(|t| t.label.as_str()), Some("target.png"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn at_most_eight_files_on_a_message() {
    let d = attachable("eight");
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    for _ in 0..attach::MAX_FILES {
        attach_path(&mut app, &at(&d, "brief.md")).unwrap();
    }
    assert!(attach_path(&mut app, &at(&d, "brief.md")).unwrap_err().contains('8'));
    assert!(attach_rgba(&mut app, 2, 2, &[9; 16]).is_err());
    assert_eq!(app.agent_chat.attachments.len(), attach::MAX_FILES);
    let ctx = egui::Context::default();
    assert!(control(&mut app, &ctx, &json!({"remove_file": 8})).is_err(), "no ninth file to take off");
    control(&mut app, &ctx, &json!({"remove_file": 0})).unwrap();
    assert_eq!(app.agent_chat.attachments.len(), attach::MAX_FILES - 1);
    let _ = std::fs::remove_dir_all(&d);
}

/// While the agent works, a message queues with its files: the queue shows them, Stop keeps
/// them, and they go with the queue.
#[test]
fn a_queued_message_keeps_its_files_through_stop_and_sends_them() {
    let d = attachable("queue");
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.images = true;
    send(&mut app, &ctx, "make a box".into());
    attach_path(&mut app, &at(&d, "target.png")).unwrap();
    settle_files(&mut app, &ctx);
    send(&mut app, &ctx, "like this".into());
    attach_path(&mut app, &at(&d, "brief.md")).unwrap();
    send(&mut app, &ctx, String::new());
    assert!(app.agent_chat.attachments.is_empty(), "the files moved to the queue");
    assert_eq!(status(&app)["pending"][0]["files"], json!(["target.png"]));
    assert_eq!(status(&app)["pending"][1]["files"], json!(["brief.md"]));
    cancel(&mut app);
    ended(&script, "cancelled");
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.pending.len(), 2, "Stop keeps the queue and its files");
    send_now(&mut app, &ctx);
    let sent = script.prompts();
    let last = sent.last().expect("the queue went");
    let text = text_of(last);
    assert!(text.starts_with("like this") && !text.contains("\n\n\n\n"), "a message of files alone adds no empty paragraph: {text:?}");
    assert!(text.contains("Attached pictures: target.png.") && text.contains("wall 2 mm"), "{text}");
    assert_eq!(pictures_in(last), 1);
    let sent = status(&app)["items"].as_array().and_then(|i| i.last()).cloned().unwrap_or_default();
    assert_eq!((sent["files"].clone(), sent["pictures"].clone()), (json!(["brief.md"]), json!(1)));
    let _ = std::fs::remove_dir_all(&d);
}

/// Queued messages go as one prompt with at most eight files; the rest are named, not sent.
#[test]
fn the_queue_sends_at_most_eight_files_and_names_the_rest() {
    let d = attachable("queue-cap");
    let (mut app, script, ctx) = app_with_script();
    send(&mut app, &ctx, "first".into());
    for n in 0..3 {
        for _ in 0..4 {
            attach_path(&mut app, &at(&d, "brief.md")).unwrap();
        }
        send(&mut app, &ctx, format!("batch {n}"));
    }
    ended(&script, "");
    poll(&mut app, &ctx);
    let sent = script.prompts();
    assert_eq!(sent.len(), 2);
    assert_eq!(text_of(&sent[1]).matches("Attached file `brief.md`").count(), attach::MAX_FILES);
    let told = app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, .. } if text.contains("brief.md") && text.contains('8')));
    assert!(told, "the files left out are named");
    let _ = std::fs::remove_dir_all(&d);
}

/// New chat clears the files waiting on the chips too, and asks first when there are some.
#[test]
fn new_chat_clears_the_files_waiting_and_asks_first() {
    let d = attachable("new");
    let (mut app, _script, _ctx) = app_with_script();
    assert!(!new_chat_needs_confirm(&app));
    attach_path(&mut app, &at(&d, "brief.md")).unwrap();
    assert!(new_chat_needs_confirm(&app), "files would be lost");
    new_chat(&mut app);
    assert!(app.agent_chat.attachments.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// A harness with the chat open at its narrowest and nothing to look for.
fn narrow_chat() -> crate::scenario::Harness {
    let mut h = crate::scenario::Harness::new();
    h.app.preferences.agent_chat_width = panel::MIN_WIDTH;
    h.app.agent_chat.open = true;
    h.app.agent_chat.detection = Some(Default::default());
    h
}

/// The rows `a_selection_held_at_the_edge_scrolls_and_keeps_its_start` draws that are in view:
/// each row's number, where its text is, and the conversation's view.
fn rows_in_view(h: &crate::scenario::Harness) -> Vec<(usize, egui::Rect, egui::Rect)> {
    let mut rows = Vec::new();
    for clipped in &h.shapes {
        crate::test_support::each_shape(&clipped.shape, &mut |shape| {
            if let egui::Shape::Text(text) = shape
                && let Some(n) = text.galley.text().strip_prefix("Parts list row ").and_then(|n| n.parse::<usize>().ok())
            {
                let at = egui::Rect::from_min_size(text.pos, text.galley.size());
                if clipped.clip_rect.intersects(at) {
                    rows.push((n, at, clipped.clip_rect));
                }
            }
        });
    }
    rows.sort_by_key(|r| r.0);
    rows
}

/// #11527: a text selection held at the conversation's edge scrolls it, and keeps its start once
/// that has scrolled out of view, so Copy takes the hidden rows too. Resting there with no button
/// down, or after letting go, scrolls nothing.
#[test]
fn a_selection_held_at_the_edge_scrolls_and_keeps_its_start() {
    let mut h = narrow_chat();
    for n in 0..80 {
        h.app.agent_chat.items.push(Item::Agent { thought: false, text: format!("Parts list row {n}") });
    }
    h.frames(4);
    let rows = rows_in_view(&h);
    let (&(top, _, view), &(last, last_at, _)) = (rows.first().expect("rows in view"), rows.last().expect("rows in view"));
    assert_eq!(last, 79, "the conversation shows its end");
    let edge = egui::pos2(last_at.left() + 10.0, view.top() + 4.0);
    let press = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    h.frame_with(vec![egui::Event::PointerMoved(edge)]);
    h.frames(20);
    assert_eq!(rows_in_view(&h)[0].0, top, "no button down: the pointer at the edge scrolls nothing");

    // Start the selection at the end of the last row and hold it at the top edge.
    let start = last_at.right_center() - egui::vec2(1.0, 0.0);
    h.frame_with(vec![egui::Event::PointerMoved(start), press(start, true)]);
    h.frame_with(vec![egui::Event::PointerMoved(edge)]);
    for _ in 0..600 {
        if !rows_in_view(&h).iter().any(|r| r.0 == last) {
            break;
        }
        h.frame();
    }
    let rows = rows_in_view(&h);
    assert!(!rows.iter().any(|r| r.0 == last), "holding the selection at the top scrolls the last row out of view");
    let reached = rows[0].0;
    assert!(reached < top, "row {reached} is in view, it was {top}");
    h.frame_with(vec![press(edge, false)]);
    let after = rows_in_view(&h)[0].0;
    h.frames(20);
    assert_eq!(rows_in_view(&h)[0].0, after, "letting go stops the scrolling");
    assert!(h.ctx.plugin::<egui::text_selection::LabelSelectionState>().lock().has_selection(), "the selection keeps its start out of view");

    h.frame_with(vec![egui::Event::Copy]);
    let copied = h
        .output
        .commands
        .iter()
        .find_map(|c| if let egui::OutputCommand::CopyText(text) = c { Some(text.clone()) } else { None })
        .expect("Copy takes the selection");
    // The selection's moving end is under the pointer, in the first row or two in view.
    for n in [reached + 2, 78] {
        assert!(copied.contains(&format!("Parts list row {n}\n")), "row {n} is copied whole: {copied:?}");
    }
    assert!(copied.trim_end().ends_with("Parts list row 79"), "the selection ends where it started, out of view: {copied:?}");
}

/// #11527: only a drag towards the edge scrolls. Holding a press on the line at the top edge, a
/// double click there, or a drag from there into the conversation leaves it where it is.
#[test]
fn a_press_at_the_edge_without_a_drag_towards_it_scrolls_nothing() {
    let mut h = narrow_chat();
    // One paragraph taller than the conversation: every point in view is on its text.
    h.app.agent_chat.items.push(Item::Agent { thought: false, text: "Bill of materials ".repeat(400) });
    h.frames(4);
    // Where the answer's text starts, the conversation's view, and the first line wholly in view
    // (egui itself scrolls a pressed line that is cut off at the edge into view).
    let answer = |h: &crate::scenario::Harness| {
        let mut found = None;
        for clipped in &h.shapes {
            crate::test_support::each_shape(&clipped.shape, &mut |shape| {
                if let egui::Shape::Text(text) = shape
                    && text.galley.text().starts_with("Bill of materials")
                    && clipped.clip_rect.intersects(egui::Rect::from_min_size(text.pos, text.galley.size()))
                {
                    let line = text.galley.rows.iter().map(|r| r.rect().translate(text.pos.to_vec2())).find(|r| r.top() >= clipped.clip_rect.top());
                    found = Some((text.pos.y, clipped.clip_rect, line.expect("a line wholly in view")));
                }
            });
        }
        found.expect("the answer is in view")
    };
    let (rest, view, line) = answer(&h);
    let press = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    let edge = egui::pos2(view.left() + 40.0, line.center().y);
    assert!(edge.y < view.top() + 32.0, "the first line in view is by the edge");
    h.frame_with(vec![egui::Event::PointerMoved(edge), press(edge, true)]);
    h.frames(30);
    assert_eq!(answer(&h).0, rest, "a press held at the edge scrolls nothing");
    h.frame_with(vec![press(edge, false)]);
    for _ in 0..2 {
        h.frame_with(vec![press(edge, true)]);
        h.frame_with(vec![press(edge, false)]);
    }
    h.frames(10);
    assert_eq!(answer(&h).0, rest, "a double click at the edge scrolls nothing");
    let inside = edge + egui::vec2(0.0, 20.0);
    h.frame_with(vec![egui::Event::PointerMoved(edge), press(edge, true)]);
    h.frame_with(vec![egui::Event::PointerMoved(inside)]);
    h.frames(30);
    assert_eq!(answer(&h).0, rest, "a drag from the edge into the conversation scrolls nothing");
    h.frame_with(vec![press(inside, false)]);
    // Towards the edge it does.
    let middle = view.center();
    h.frame_with(vec![egui::Event::PointerMoved(middle), press(middle, true)]);
    h.frame_with(vec![egui::Event::PointerMoved(edge)]);
    h.frames(30);
    assert!(answer(&h).0 > rest, "a drag to the top edge scrolls the answer down");
    h.frame_with(vec![press(edge, false)]);
}

/// #10172: before a connection, what the panel draws under the agent switcher (the Agent row
/// with Add agent and Refresh) keeps the panel's side margin, docked and floating: nothing starts
/// at the panel's edge or is cut off at the other one.
#[test]
fn the_agent_list_keeps_the_panels_side_margin() {
    use crate::shell::LayoutMode;
    for layout in [LayoutMode::Docked, LayoutMode::Floating] {
        let mut h = narrow_chat();
        h.app.ui.layout = layout;
        h.frames(3);
        h.frame();
        let panel = crate::scenario::rect_of("agent_chat:panel").expect("the panel");
        let mut texts = Vec::new();
        for clipped in &h.shapes {
            crate::test_support::each_shape(&clipped.shape, &mut |s| {
                if let egui::Shape::Text(t) = s {
                    texts.push((t.galley.text().to_string(), egui::Rect::from_min_size(t.pos, t.galley.rect.size())));
                }
            });
        }
        let at = |label: &str| texts.iter().find(|(text, r)| text == label && panel.contains(r.center())).map(|(_, r)| *r);
        let margin = crate::theme::Tokens::get().space_12 - 0.5;
        let agent = at(tr("ui.agent_chat_pick")).expect("the Agent row");
        assert!(agent.left() >= panel.left() + margin, "{layout:?}: Agent at {agent:?} in {panel:?}");
        let refresh = at(tr("ui.agent_chat_refresh")).expect("Refresh");
        assert!(refresh.right() <= panel.right() - margin, "{layout:?}: Refresh at {refresh:?} in {panel:?}");
    }
}

/// Every chip and the text box sit inside the panel.
fn assert_inside_the_panel(chips: usize) {
    use crate::scenario::rect_of;
    let panel = rect_of("agent_chat:panel").expect("the panel");
    let mut rects: Vec<(String, egui::Rect)> =
        (0..chips).map(|i| (format!("file:{i}"), rect_of(&format!("agent_chat:file:{i}")).expect("a chip"))).collect();
    rects.push(("input".into(), rect_of("agent_chat:input").expect("the text box")));
    for (name, r) in rects {
        assert!(r.left() >= panel.left() && r.right() <= panel.right(), "{name} {r:?} in {panel:?}");
    }
}

/// Files for the next message show as chips above the text box (a picture's thumbnail, the name,
/// a × that takes it off), inside the chat, and Send sends files with no text.
#[test]
fn attached_files_show_as_chips_that_can_be_taken_off_and_sent_alone() {
    use crate::scenario::handle_at;
    let d = attachable("chips");
    let mut h = narrow_chat();
    let script = Script::default();
    h.app.agent_chat.backend = Some(Box::new(script.clone()));
    h.app.agent_chat.state = Some(State::Ready);
    h.app.agent_chat.images = true;
    h.frames(2);
    h.call("ui.agent_chat", json!({"attach": at(&d, "target.png")}));
    h.call("ui.agent_chat", json!({"attach": at(&d, "brief.md")}));
    settle_chat(&mut h);
    assert!(handle_at("agent_chat:file:0").is_some() && handle_at("agent_chat:file:1").is_some(), "a chip per file");
    assert_inside_the_panel(2);
    let x = handle_at("agent_chat:file_remove:0").expect("a × on the chip");
    h.call("ui.click", json!({"x": x.x, "y": x.y}));
    h.frames(2);
    assert_eq!(status(&h.app)["attachments"], json!(["brief.md"]));
    let send = handle_at("agent_chat:send").expect("Send");
    h.call("ui.click", json!({"x": send.x, "y": send.y}));
    h.frames(2);
    assert_eq!(script.prompts().len(), 1, "files alone make a message");
    assert!(h.app.agent_chat.attachments.is_empty() && handle_at("agent_chat:file:0").is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// Review of #1744: chips' thumbnails were cached by the picture's address, so a new picture at a
/// freed address could show an old one. Each file has an id of its own, never reused, and the
/// cache follows the files on the message.
#[test]
fn each_file_has_its_own_id_and_thumbnail() {
    let d = attachable("ids");
    image::RgbaImage::from_pixel(20, 20, image::Rgba([10, 200, 10, 255])).save(d.join("green.png")).unwrap();
    let mut h = narrow_chat();
    h.frames(2);
    h.call("ui.agent_chat", json!({"attach": at(&d, "target.png")}));
    h.call("ui.agent_chat", json!({"attach": at(&d, "green.png")}));
    settle_chat(&mut h);
    let first: Vec<u64> = h.app.agent_chat.attachments.iter().map(|f| f.id).collect();
    assert!(first.len() == 2 && first[0] != first[1], "{first:?}");
    let mut cached: Vec<u64> = h.app.agent_chat.thumbs.keys().copied().collect();
    cached.sort_unstable();
    assert_eq!(cached, first, "a thumbnail per file, by its id");
    h.call("ui.agent_chat", json!({"remove_file": 0}));
    h.call("ui.agent_chat", json!({"attach": at(&d, "target.png")}));
    settle_chat(&mut h);
    let now: Vec<u64> = h.app.agent_chat.attachments.iter().map(|f| f.id).collect();
    assert!(!now.contains(&first[0]), "a removed file's id is not used again: {now:?}");
    let mut cached: Vec<u64> = h.app.agent_chat.thumbs.keys().copied().collect();
    cached.sort_unstable();
    let mut want = now.clone();
    want.sort_unstable();
    assert_eq!(cached, want, "the cache follows the files on the message");
    let _ = std::fs::remove_dir_all(&d);
}

/// The + button: "Choose a file…" asks the app's file picker for the types that can be attached
/// (not the design types Open shows) and attaches what it answers; "Paste a picture" attaches
/// the clipboard's picture (here given as RGBA, as the system does).
#[test]
fn the_plus_button_attaches_a_chosen_file_or_a_pasted_picture() {
    use crate::scenario::handle_at;
    let d = attachable("plus");
    let mut h = narrow_chat();
    let chosen = at(&d, "brief.md");
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let seen = asked.clone();
    h.app.services.pick_file = Some(Box::new(move |_label: &str, exts: &[&str]| {
        *seen.borrow_mut() = exts.iter().map(|e| e.to_string()).collect();
        let p = chosen.clone();
        Box::pin(async move { Some(p) })
    }));
    h.frames(2);
    let plus = handle_at("agent_chat:add").expect("a + button");
    h.call("ui.click", json!({"x": plus.x, "y": plus.y}));
    h.frames(2);
    let choose = handle_at("agent_chat:add_file").expect("Choose a file… in its menu");
    assert!(handle_at("agent_chat:paste_picture").is_some(), "Paste a picture in its menu");
    h.call("ui.click", json!({"x": choose.x, "y": choose.y}));
    h.frames(4);
    assert_eq!(status(&h.app)["attachments"], json!(["brief.md"]));
    let exts = asked.borrow().clone();
    for ext in ["png", "jpg", "gif", "webp", "svg", "md", "step", "stl"] {
        assert!(exts.iter().any(|e| e == ext), "{ext} in {exts:?}");
    }
    assert!(!exts.iter().any(|e| e == "solidcraft"));
    attach_rgba(&mut h.app, 4, 3, &[200; 48]).unwrap();
    attach_rgba(&mut h.app, 4, 3, &[200; 47]).unwrap();
    settle_chat(&mut h);
    assert!(
        h.app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, error: true } if text.starts_with("pasted picture 2.png:"))),
        "not a picture: it says so once it has been read"
    );
    assert_eq!(status(&h.app)["attachments"], json!(["brief.md", "pasted picture 1.png"]));
    let _ = std::fs::remove_dir_all(&d);
}

#[derive(Debug)]
struct DroppedPath(std::path::PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

fn drop_files(h: &mut crate::scenario::Harness, at: egui::Pos2, files: &[std::path::PathBuf]) {
    h.drop_files(at, files.iter().map(|p| Arc::new(DroppedPath(p.clone())) as egui::DroppedFileHandle).collect());
}

/// A tetrahedron, as a small ASCII STL.
const TETRA_STL: &str = "solid t
facet normal 0 0 -1
 outer loop
  vertex 0 0 0
  vertex 0 10 0
  vertex 10 0 0
 endloop
endfacet
facet normal 0 -1 0
 outer loop
  vertex 0 0 0
  vertex 10 0 0
  vertex 0 0 10
 endloop
endfacet
facet normal -1 0 0
 outer loop
  vertex 0 0 0
  vertex 0 0 10
  vertex 0 10 0
 endloop
endfacet
facet normal 1 1 1
 outer loop
  vertex 10 0 0
  vertex 0 10 0
  vertex 0 0 10
 endloop
endfacet
endsolid t
";

/// Review of #1744: a CAD file's import wasn't waited on, so a failed or queued-out import still
/// told the agent "I added x to the design". The file joins the message once its import is in;
/// a failed import says why and the agent is told nothing of it.
#[test]
fn a_cad_file_joins_the_message_once_its_import_is_in() {
    let d = attachable("cad");
    std::fs::write(d.join("block.stl"), TETRA_STL).unwrap();
    std::fs::write(d.join("broken.stl"), [0u8, 1, 2]).unwrap();
    let mut h = narrow_chat();
    let script = Script::default();
    h.app.agent_chat.backend = Some(Box::new(script.clone()));
    h.app.agent_chat.state = Some(State::Ready);
    h.frames(2);
    attach_path(&mut h.app, &at(&d, "block.stl")).unwrap();
    assert_eq!(status(&h.app)["loading"], json!(["block.stl"]), "importing, not yet on the message");
    assert_eq!(status(&h.app)["attachments"], json!([]));
    settle_chat(&mut h);
    assert_eq!(status(&h.app)["attachments"], json!(["block.stl"]), "in, and on the message");
    h.call("ui.agent_chat", json!({"attach": at(&d, "broken.stl")}));
    settle_chat(&mut h);
    assert!(
        h.app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, error: true } if text.starts_with("broken.stl:"))),
        "a failed import says why"
    );
    assert_eq!(status(&h.app)["attachments"], json!(["block.stl"]), "and is not on the message");
    h.call("ui.agent_chat", json!({"send": "fit around this"}));
    h.frames(2);
    let text = text_of(&script.prompts()[0]);
    assert!(text.contains("`block.stl` to the design") && !text.contains("broken.stl"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Review of #1744: taking an imported part's chip off leaves the part in the design, and its ×
/// says so; any other file's × just takes it off.
#[test]
fn an_imported_parts_chip_says_the_part_stays() {
    assert_eq!(panel::remove_tip(&attach::Attachment::imported("motor.step".into())), tr("ui.agent_chat_remove_imported"));
    assert!(tr("ui.agent_chat_remove_imported").contains("stays in the design"));
    let picture = attach::from_rgba("a.png".into(), 2, 2, &[9; 16]).unwrap();
    assert_eq!(panel::remove_tip(&picture), tr("ui.agent_chat_remove_file"));
}

/// Files dropped on the chat attach to the next message, and the app doesn't open them as well
/// (while they are dragged over it, the chat says so); dropped on the 3D view they are not the
/// chat's (the app handles them as before), nor when the chat is closed.
#[test]
fn files_dropped_on_the_chat_attach_and_the_app_does_not_open_them() {
    use crate::scenario::rect_of;
    let d = attachable("drop");
    let mut h = narrow_chat();
    h.call("ui.resize", json!({"width": 1600, "height": 1000}));
    h.frames(3);
    let panel = rect_of("agent_chat:panel").expect("docked");
    // While files are dragged over the chat, it says they will attach; over the 3D view, not.
    let hovered = || vec![egui::HoveredFile { path: Some(d.join("target.png")), mime: String::new() }];
    h.drag_files(panel.center(), hovered(), Vec::new());
    assert!(rect_of("agent_chat:drop_hint").is_some_and(|r| panel.contains(r.center())), "the hint over the chat");
    h.drag_files(egui::pos2(300.0, 500.0), hovered(), Vec::new());
    assert!(rect_of("agent_chat:drop_hint").is_none(), "no hint over the 3D view");
    drop_files(&mut h, panel.center(), &[d.join("target.png"), d.join("brief.md")]);
    settle_chat(&mut h);
    assert_eq!(status(&h.app)["attachments"], json!(["target.png", "brief.md"]));
    assert!(!h.app.file_load.busy(), "the app doesn't open them as well");
    drop_files(&mut h, egui::pos2(300.0, 500.0), &[d.join("brief.md")]);
    assert_eq!(status(&h.app)["attachments"].as_array().map(Vec::len), Some(2), "over the 3D view: not the chat's");
    // Folded to its strip, a drop on the strip is the chat's too.
    h.call("ui.agent_chat", json!({"minimized": true}));
    h.frames(3);
    let strip = h.app.agent_chat.drop_rect.expect("the strip takes drops");
    assert!(strip.width() < 60.0, "{strip:?}");
    drop_files(&mut h, strip.center(), &[d.join("brief.md")]);
    assert_eq!(status(&h.app)["attachments"].as_array().map(Vec::len), Some(3));
    // Closed: nothing is the chat's, wherever the pointer was.
    h.app.agent_chat.open = false;
    h.frames(2);
    assert!(h.app.agent_chat.drop_rect.is_none());
    drop_files(&mut h, strip.center(), &[d.join("brief.md")]);
    assert_eq!(status(&h.app)["attachments"].as_array().map(Vec::len), Some(3));
    let _ = std::fs::remove_dir_all(&d);
}

/// The chat popped out into its own window takes drops too, and says so while files are dragged
/// over it (#2812). Here the window floats inside the app, as it does where the system can't open
/// one: a drop on the window is the chat's, one elsewhere is not.
#[test]
fn files_dropped_on_the_popped_out_chat_attach() {
    use crate::scenario::rect_of;
    let d = attachable("popdrop");
    let mut h = narrow_chat();
    h.call("ui.resize", json!({"width": 1600, "height": 1000}));
    h.app.preferences.agent_chat_popped = true;
    h.frames(4);
    let window = rect_of("agent_chat:window").expect("popped out");
    let hovered = || vec![egui::HoveredFile { path: Some(d.join("target.png")), mime: String::new() }];
    h.drag_files(window.center(), hovered(), Vec::new());
    assert!(rect_of("agent_chat:drop_hint").is_some_and(|r| window.contains(r.center())), "the hint over the window");
    let outside = egui::pos2(5.0, 500.0);
    assert!(!window.contains(outside));
    h.drag_files(outside, hovered(), Vec::new());
    assert!(rect_of("agent_chat:drop_hint").is_none(), "no hint away from it");
    drop_files(&mut h, window.center(), &[d.join("target.png"), d.join("brief.md")]);
    settle_chat(&mut h);
    assert_eq!(status(&h.app)["attachments"], json!(["target.png", "brief.md"]));
    assert!(!h.app.file_load.busy(), "the app doesn't open them as well");
    drop_files(&mut h, outside, &[d.join("brief.md")]);
    assert_eq!(status(&h.app)["attachments"].as_array().map(Vec::len), Some(2), "away from the window: not the chat's");
    let _ = std::fs::remove_dir_all(&d);
}

/// Many files and a long message in a narrow chat: the chips wrap onto more rows and the text box
/// keeps to the chat (a row of chips wider than the chat would widen everything under it).
#[test]
fn chips_and_a_long_message_wrap_inside_a_narrow_chat() {
    use crate::scenario::handle_at;
    let d = attachable("wrap");
    let mut h = narrow_chat();
    for _ in 0..6 {
        h.call("ui.agent_chat", json!({"attach": at(&d, "target.png")}));
    }
    h.app.agent_chat.input = "a separate design next to the robot body, the main version, with these measures for the second one".into();
    settle_chat(&mut h);
    assert_inside_the_panel(6);
    let (first, second) = (handle_at("agent_chat:file:0").unwrap(), handle_at("agent_chat:file:1").unwrap());
    assert!(second.y > first.y + 10.0, "at 280 px the second chip goes on the next row: {first:?} {second:?}");
    let _ = std::fs::remove_dir_all(&d);
}

/// An agent's text is Markdown: a code block shows with its Copy button and a link is a button
/// of its own, instead of the raw markers.
#[test]
fn an_agents_markdown_is_drawn_as_blocks() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    h.app.agent_chat.items.push(Item::Agent {
        thought: false,
        text: "# Result\n\n- a **bold** point\n- another\n\n```\nlet x = 1;\n```\n\nSee [the docs](https://example.com/a).".into(),
    });
    h.frames(3);
    assert!(h.widget("Copy").is_some(), "the code block has its Copy button");
    assert!(h.widget("↗ example.com").is_some(), "the link is a button that names where it goes");
    // A hostile reply draws too (no panic, bounded work).
    h.app.agent_chat.items.push(Item::Agent { thought: false, text: "*".repeat(50_000) + &"[".repeat(5_000) + &"`a".repeat(5_000) });
    h.frames(3);
}

// ---- Attach selection (#2286) ----

/// A 40 x 30 x 20 block, its first face and first edge as the selection would hold them, its body
/// and its feature.
fn box_selection(app: &mut SolveApp) -> Vec<solidcraft_engine::Sel> {
    use solidcraft_engine::Sel;
    app.run("solid.box", json!({"length": 40, "width": 30, "height": 20})).expect("a box");
    let face = app.session.execute("model.faces", &json!({"body": "Body1"})).expect("faces")["faces"][0].clone();
    let edge = app.session.execute("model.edges", &json!({"body": "Body1"})).expect("edges")["edges"][0].clone();
    let at = |v: &Value| solidcore_record::Vec3::new(v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0), v[2].as_f64().unwrap_or(0.0));
    vec![
        Sel::Face { body: "Body1".into(), index: face["index"].as_u64().unwrap_or(0) as usize, point: at(&face["point"]) },
        Sel::Edge { body: "Body1".into(), index: edge["index"].as_u64().unwrap_or(0) as usize, point: at(&edge["mid"]) },
        Sel::Feature { id: app.session.doc.features.first().map(|f| f.id).expect("a feature") },
        Sel::Body { name: "Body1".into() },
    ]
}

/// With Attach selection on, the message goes with what is selected: the agent gets it as text
/// after the message, the conversation shows the message as typed with the items as chips.
#[test]
fn an_attached_selection_goes_as_text_for_the_agent_and_as_chips_in_the_conversation() {
    let (mut app, script, ctx) = app_with_script();
    app.session.selection = box_selection(&mut app);
    // Off: the message is just the message.
    send(&mut app, &ctx, "plain".into());
    assert_eq!(script.calls(), ["prompt: plain"]);
    ended(&script, "");
    poll(&mut app, &ctx);

    app.agent_chat.attach_selection = true;
    send(&mut app, &ctx, "round this".into());
    let prompt = script.calls().pop().expect("a prompt");
    assert!(prompt.starts_with("prompt: round this\n\n[Selected in SolidCraft"), "{prompt}");
    for want in ["Elements:", "- Flat face ", "planar", "area ", "- Edge ", "length ", "name ", "Features:", "feature id", "Bodies:", "- Body1"] {
        assert!(prompt.contains(want), "{want:?} missing from {prompt}");
    }
    let Some(Item::User { text, context, .. }) = app.agent_chat.items.last() else { panic!("a sent message") };
    assert_eq!(text, "round this", "the conversation shows what was typed");
    assert_eq!(context.items.len(), 4);
    let kinds: Vec<_> = context.items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [context::Kind::Element, context::Kind::Element, context::Kind::Feature, context::Kind::Body]);
    // Nothing selected: nothing is added, and the box stays ticked.
    ended(&script, "");
    poll(&mut app, &ctx);
    app.session.selection.clear();
    send(&mut app, &ctx, "nothing".into());
    assert_eq!(script.calls().pop().as_deref(), Some("prompt: nothing"));
}

/// A queued message keeps the selection of the moment it was typed; queued messages go as one
/// prompt with all their items.
#[test]
fn queued_messages_keep_the_selection_they_were_typed_with() {
    use solidcraft_engine::Sel;
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.attach_selection = true;
    send(&mut app, &ctx, "first".into());
    app.session.selection = vec![Sel::Body { name: "Alpha".into() }];
    send(&mut app, &ctx, "second".into());
    app.session.selection = vec![Sel::Body { name: "Beta".into() }];
    send(&mut app, &ctx, "third".into());
    assert_eq!(app.agent_chat.pending.iter().map(|p| p.context.total()).collect::<Vec<_>>(), [1, 1]);
    ended(&script, "");
    poll(&mut app, &ctx);
    let prompt = script.calls().pop().expect("the queue went");
    assert!(prompt.starts_with("prompt: second\n\nthird\n\n[Selected"), "{prompt}");
    assert!(prompt.contains("- Alpha") && prompt.contains("- Beta"), "{prompt}");
    let Some(Item::User { context, .. }) = app.agent_chat.items.last() else { panic!("a sent message") };
    assert_eq!(context.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Alpha", "Beta"]);
}

/// A large selection is capped and says how much is left out; nothing in it can break the prompt.
#[test]
fn a_huge_selection_is_counted_not_dropped_silently() {
    use solidcraft_engine::Sel;
    let mut session = solidcraft_engine::Session::default();
    let selection: Vec<Sel> = (0..100).map(|i| Sel::Body { name: format!("Body{i}") }).collect();
    let context = context::describe(&mut session, &selection);
    assert_eq!((context.items.len(), context.omitted, context.total()), (context::MAX_ITEMS, 100 - context::MAX_ITEMS, 100));
    assert!(context.prompt_block().contains("and 36 more, not listed"));
    // Two such messages together stay capped.
    let mut both = context.clone();
    both.merge(context);
    assert_eq!((both.items.len(), both.total()), (context::MAX_ITEMS, 200));
    // Unknown bodies, faces and features are described by what is known, without panicking.
    let odd = [
        Sel::Face { body: "Nope".into(), index: usize::MAX, point: solidcore_record::Vec3::new(f64::NAN, f64::INFINITY, 0.0) },
        Sel::Edge { body: "Nope".into(), index: 7, point: solidcore_record::Vec3::new(0.0, 0.0, 0.0) },
        Sel::Feature { id: u64::MAX },
        Sel::Plane { name: "XY".into() },
        Sel::SketchCurve { id: "c1".into(), sketch: None },
    ];
    let described = context::describe(&mut session, &odd);
    assert_eq!(described.items.len(), 5);
    assert!(described.prompt_block().contains("Other:"));
}

/// The checkbox shows the number selected, and the chips of a sent message are drawn grouped.
#[test]
fn the_selection_checkbox_and_the_chips_are_drawn() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    assert!(h.widget("Attach selection").is_some(), "the checkbox is there, with nothing selected");
    h.app.session.selection = box_selection(&mut h.app);
    h.frames(3);
    assert!(h.widget("Attach selection (4)").is_some(), "it counts what is selected");
    let context = context::describe_selection(&mut h.app.session);
    h.app.agent_chat.items.push(Item::User { text: "look".into(), image: false, pictures: Vec::new(), context, files: Vec::new() });
    h.frames(3);
    for heading in ["Elements 2", "Features 1", "Bodies 1"] {
        assert!(h.widget(heading).is_some(), "{heading} is drawn");
    }
}

#[test]
fn assistant_marks_follow_chat_login_turns_and_approvals() {
    use AssistantMark::*;
    let mark = |state, login, configured| assistant_mark_for(state, login, configured, 0, false, false);
    assert_eq!(mark(State::Off, "", false), Quiet);
    assert_eq!(mark(State::Off, "signed_in", false), Quiet);
    for login in ["", "unknown"] {
        assert_eq!(mark(State::Off, login, true), Quiet);
    }
    for login in ["signed_in", "key"] {
        assert_eq!(mark(State::Off, login, true), Ready);
    }
    assert_eq!(mark(State::Off, "signed_out", true), NeedsAuth);
    for login in ["", "signed_out", "unknown", "signed_in"] {
        for (state, expected) in [
            (State::Ready, Ready),
            (State::Busy, Busy),
            (State::Starting, Busy),
            (State::NeedsAuth, NeedsAuth),
            (State::Exited, Stopped),
            (State::Failed, Stopped),
        ] {
            assert_eq!(mark(state, login, true), expected);
            assert_eq!(assistant_mark_for(state, login, true, 12, false, false), Waiting(12));
        }
    }
    assert_eq!(assistant_mark_for(State::Ready, "signed_in", true, 0, true, false), Stopped);
    assert_eq!(assistant_mark_for(State::Ready, "signed_in", true, 0, false, true), Stopped);
}

/// An app whose endpoint agents answer at once (offline), with `api-1` and `api-2` added.
#[cfg(not(target_arch = "wasm32"))]
fn app_with_endpoints() -> SolveApp {
    let http: backend::HttpFn = Arc::new(|_| Err("offline".into()));
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services { agent_http: Some(http), ..Default::default() });
    for id in ["api-1", "api-2"] {
        app.preferences.agents.push(agents::AgentConfig {
            id: id.into(),
            kind: agents::Kind::Api,
            base_url: "http://localhost:11434/v1".into(),
            model: "m".into(),
            ..Default::default()
        });
    }
    app
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn the_agent_started_last_connects_at_start_once_the_agents_are_known() {
    let ctx = crate::fonts::test_context();
    let mut app = app_with_endpoints();
    // Off by default: a launch starts nothing and leaves the panel closed.
    begin(&mut app);
    poll(&mut app, &ctx);
    assert!(!app.agent_chat.open && app.agent_chat.backend.is_none() && app.agent_chat.detecting.is_none());

    app.preferences.agent_connect_at_start = true;
    app.preferences.agent_chat_open_at_start = true;
    app.preferences.agent_last = "api-2".into();
    begin(&mut app);
    assert!(app.agent_chat.open);
    // Nothing waits for discovery: the agent starts when it is in, in whichever tab is shown.
    let (found, detecting) = channel();
    app.agent_chat.detecting = Some(detecting);
    poll(&mut app, &ctx);
    assert!(app.agent_chat.backend.is_none());
    crate::documents::open_tab(&mut app, solidcraft_engine::Session::default()).unwrap();
    found.send(discover::Detection::default()).unwrap();
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.agent_id.as_deref(), Some("api-2"), "the agent started last, not the default");
    assert_eq!(app.agent_chat.state(), State::Starting);
    assert!(app.agent_chat.backend.is_some());
    // A design opened over the launch's blank one keeps the agent: nothing was asked yet.
    crate::documents::adopt(&mut app, solidcraft_engine::Session::default());
    assert!(app.agent_chat.backend.is_some() && app.agent_chat.items.is_empty());
    assert_eq!(app.docs.count(), 2);
    // Once per launch.
    disconnect(&mut app);
    poll(&mut app, &ctx);
    assert!(app.agent_chat.backend.is_none());
    // Connecting another agent makes it the one started last.
    app.agent_chat.agent_id = Some("api-1".into());
    connect(&mut app, &ctx);
    assert_eq!(app.preferences.agent_last, "api-1");
    assert!(status(&app)["lastAgent"] == "api-1" && status(&app)["connectAtStart"] == true);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_connection_that_never_gets_ready_says_connection_failed_and_offers_retry() {
    let ctx = crate::fonts::test_context();
    // The agent started last is gone: the chat says so, and no other agent starts by itself.
    let mut app = app_with_endpoints();
    app.preferences.agent_connect_at_start = true;
    app.preferences.agent_last = "claude".into();
    app.agent_chat.detection = Some(discover::Detection::default());
    begin(&mut app);
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Failed);
    assert!(app.agent_chat.backend.is_none() && app.agent_chat.agent_id.is_none());
    assert!(matches!(app.agent_chat.items.last(), Some(Item::Notice { text, error: true }) if text.contains("claude")));
    assert!(!State::Failed.running(), "the Connect button reads Retry, not Stop");
    assert_eq!(status(&app)["state"], "failed");

    // An agent that cannot start.
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.preferences.agents.push(agents::AgentConfig { id: "custom-1".into(), command: "solidcraft-no-such-agent-7009".into(), ..Default::default() });
    connect(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Failed);
    assert!(app.preferences.agent_last.is_empty(), "only an agent that started is remembered");

    // A found agent that failed its version check (#9034): the picker says it doesn't start, and
    // connecting says why and how to install it again, besides the system's own message.
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    let reason = "This version of opencode.exe is not compatible with the version of Windows you're running.";
    app.agent_chat.detection = Some(discover::Detection {
        found: vec![discover::Found {
            id: "opencode",
            name: "OpenCode",
            argv: vec!["solidcraft-no-such-agent-9034".into(), "acp".into()],
            version: None,
            login: discover::Login::Unknown,
            problem: Some(reason.into()),
            install: "npm install -g opencode-ai",
        }],
        ..Default::default()
    });
    let entry = agents::current(&app).unwrap();
    assert!(entry.detail().contains(reason), "{}", entry.detail());
    connect(&mut app, &ctx);
    assert_eq!(app.agent_chat.state(), State::Failed);
    let told = |needle: &str| app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, error: true } if text.contains(needle)));
    assert!(told(reason) && told("npm install -g opencode-ai") && told("OpenCode"), "{:?}", app.agent_chat.items);
    assert_eq!(status(&app)["agents"][0]["problem"], reason);
    // One that answered has nothing to say.
    let mut fine = entry.clone();
    fine.problem = None;
    assert_eq!(fine.problem_hint(), None);

    // No agent at all: the picker opens on what to install.
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.agent_chat.detection = Some(discover::Detection::default());
    connect(&mut app, &ctx);
    assert_eq!((app.agent_chat.state(), app.agent_chat.picking), (State::Failed, Some(true)));

    // An agent that ends while starting failed to connect; one that ends later stopped.
    let mut c = chat();
    c.state = Some(State::Starting);
    c.on_event(Event::Error("initialize failed".into()));
    c.on_event(Event::Exited { tail: Vec::new() });
    assert_eq!(c.state(), State::Failed);
    let mut c = chat();
    c.on_event(Event::Exited { tail: Vec::new() });
    assert_eq!(c.state(), State::Exited);
}

#[test]
fn assistant_click_and_shortcut_toggle_the_same_panel_without_a_connection() {
    let mut h = crate::scenario::Harness::new();
    h.frames(3);
    assert_eq!(assistant_mark(&h.app), AssistantMark::Quiet);
    assert!(crate::scenario::handle_at("toolbar:ai").is_none());
    h.step(&json!({"click":{"handle":"titlebar:assistant"}})).unwrap();
    assert!(h.app.agent_chat.open);
    h.step(&json!({"key":"A", "shift":true,"cmd":true})).unwrap();
    assert!(!h.app.agent_chat.open);
    assert!(h.app.agent_chat.backend.is_none());
}

/// A long message scrolls inside its box: the buttons under the box stay in the window (#2224).
/// Measured against the panel's bottom once the agents on this machine have been looked for:
/// what that finds (and the status it reports) arrives in its own time and may change what is
/// around the panel, which is not what this checks.
#[test]
fn a_long_message_keeps_the_buttons_in_the_window() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.resize", json!({"width": 1600.0, "height": 1000.0}));
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(8);
    let began = std::time::Instant::now();
    while h.app.agent_chat.detecting.is_some() && began.elapsed() < std::time::Duration::from_secs(30) {
        h.frame();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.frames(4);
    let bottom = |h: &crate::scenario::Harness| f64::from(h.app.agent_chat.panel_rect.map(|r| r.bottom()).expect("the panel is drawn"));
    let resting: Vec<_> = ["Send", "Attach view"]
        .into_iter()
        .map(|name| {
            let [_, y] = h.widget(name).unwrap_or_else(|| panic!("{name} is drawn"));
            (name, bottom(&h) - y)
        })
        .collect();
    h.app.agent_chat.input = "Please make the plate thicker and then add a fillet on every outside edge.\n".repeat(60);
    h.frames(8);
    let window = 1000.0;
    for (name, rest) in resting {
        let [_, y] = h.widget(name).unwrap_or_else(|| panic!("{name} is drawn"));
        assert!(y < window, "{name} is at {y}, below the window's bottom at {window}");
        // The box is pinned to the bottom and grows upwards: the buttons stay where they were
        // above the panel's bottom.
        let above = bottom(&h) - y;
        assert!((above - rest).abs() < 1.0, "{name} moved from {rest} to {above} above the panel's bottom");
    }
}

fn test_png(colour: [u8; 4], width: u32) -> Vec<u8> {
    let mut png = Vec::new();
    let image = image::RgbaImage::from_pixel(width, 4, image::Rgba(colour));
    image::DynamicImage::ImageRgba8(image).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).expect("a PNG");
    png
}

/// A message's pictures show as thumbnails; a click opens the viewer on that one, where Next, the
/// arrow keys and Esc work (#2224). A message whose picture is not taken yet keeps a plain caption.
#[test]
fn a_messages_pictures_open_in_a_viewer() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.app.agent_chat.items.push(Item::User {
        text: "waiting".into(),
        image: true,
        pictures: Vec::new(),
        files: Vec::new(),
        context: Context::default(),
    });
    h.frames(3);
    let [x, y] = h.widget("View attached").expect("the plain caption is drawn");
    h.call("ui.click", json!({"x": x, "y": y}));
    h.frames(3);
    assert!(h.app.agent_chat.gallery.is_none(), "no picture yet: nothing opens");

    h.app.agent_chat.items.clear();
    let pictures = vec![Picture::new(test_png([200, 30, 30, 255], 8), "first view"), Picture::new(test_png([30, 30, 200, 255], 16), "")];
    let ours = pictures.clone();
    h.app.agent_chat.items.push(Item::User { text: "look".into(), image: true, pictures, files: Vec::new(), context: Context::default() });
    h.frames(2);
    // Decoded in the background: let both finish before looking for thumbnails.
    for picture in &ours {
        assert!(until_decoded(&mut h, picture), "the picture was decoded");
    }
    assert!(h.widget("View attached").is_none(), "thumbnails replace the caption");
    let point = |name: &str| {
        let p = crate::scenario::handle_at(name).unwrap_or_else(|| panic!("{name} is drawn"));
        json!({"x": p.x, "y": p.y})
    };
    // Hovering is not clicking.
    let second = point("agent_chat:picture:0:1");
    h.call("ui.move", second.clone());
    h.frames(3);
    assert!(h.app.agent_chat.gallery.is_none());
    h.call("ui.click", second);
    h.frames(4);
    assert_eq!(h.app.agent_chat.gallery.as_ref().map(|g| g.at), Some(1), "the click opens the viewer on that picture");
    assert!(crate::scenario::rect_of("agent_chat:gallery_picture").is_some(), "the picture is drawn big");
    // Next wraps round; the arrow keys step too.
    let next = point("agent_chat:gallery_next");
    h.call("ui.click", next);
    h.frames(3);
    assert_eq!(h.app.agent_chat.gallery.as_ref().map(|g| g.at), Some(0));
    h.call("ui.key", json!({"key": "ArrowRight"}));
    h.frames(3);
    assert_eq!(h.app.agent_chat.gallery.as_ref().map(|g| g.at), Some(1));
    let state = h.call("ui.agent_chat", json!({}));
    assert_eq!(state["result"]["items"][0]["pictures"], 2);
    h.call("ui.key", json!({"key": "Escape"}));
    h.frames(3);
    assert!(h.app.agent_chat.gallery.is_none(), "Esc closes the viewer");
    // New chat closes it with the conversation.
    h.app.agent_chat.gallery = Some(Gallery { pictures: vec![Picture::new(test_png([0, 0, 0, 255], 4), "")], at: 0 });
    super::new_chat(&mut h.app);
    assert!(h.app.agent_chat.gallery.is_none());
}

/// A message's picture is kept on exactly the message it was taken for: not an earlier one that
/// also wanted a picture, not one that has it already, and nothing that isn't such a message.
#[test]
fn the_picture_goes_to_the_message_it_was_taken_for() {
    let mut chat = AgentChat::default();
    chat.push(Item::User { text: "no picture".into(), image: false, pictures: Vec::new(), files: Vec::new(), context: Context::default() });
    chat.push(Item::User {
        text: "first".into(),
        image: true,
        pictures: vec![Picture::new(vec![1], "")],
        files: Vec::new(),
        context: Context::default(),
    });
    chat.push(Item::User { text: "second".into(), image: true, pictures: Vec::new(), files: Vec::new(), context: Context::default() });
    chat.push(Item::User { text: "third".into(), image: true, pictures: Vec::new(), files: Vec::new(), context: Context::default() });
    let sizes = |chat: &AgentChat| -> Vec<usize> {
        chat.items
            .iter()
            .filter_map(|i| match i {
                Item::User { pictures, .. } => Some(pictures.iter().map(|p| p.bytes.len()).sum()),
                _ => None,
            })
            .collect()
    };
    // The picture of the third message goes to the third, though the second still waits for one.
    chat.keep_picture(Some(3), vec![2, 3]);
    assert_eq!(sizes(&chat), [0, 1, 0, 2]);
    // The same message again (it was sent again): nothing is added, nor moved to an older message.
    chat.keep_picture(Some(3), vec![9; 5]);
    assert_eq!(sizes(&chat), [0, 1, 0, 2]);
    // A message that did not want one, no message, or a place that is gone.
    chat.keep_picture(Some(0), vec![1]);
    chat.keep_picture(None, vec![1]);
    chat.keep_picture(Some(99), vec![1]);
    assert_eq!(sizes(&chat), [0, 1, 0, 2]);
    chat.keep_picture(Some(2), vec![0; super::MAX_PICTURE_BYTES + 1]);
    assert_eq!(sizes(&chat), [0, 1, 0, 2], "an oversized picture is not kept");
}

/// A message sent again after a sign-in is still the message it was: its turn points at its own
/// place in the conversation, so its picture is kept there, once, and never on an older message that
/// wanted one (#2224 review). The picture itself can't be drawn without a renderer in a test, so the
/// place a turn is started for is what is checked.
#[test]
fn a_message_sent_again_after_sign_in_stays_the_message_it_was() {
    let (mut app, script, ctx) = app_with_script();
    app.agent_chat.images = true;
    // An older message that wanted a picture and has none.
    app.agent_chat.push(Item::User { text: "older".into(), image: true, pictures: Vec::new(), files: Vec::new(), context: Context::default() });
    app.agent_chat.attach_view = true;
    send(&mut app, &ctx, "the new one".into());
    let place = app.agent_chat.items.len() - 1;
    assert_eq!(app.agent_chat.last, Some(("the new one".into(), true, Some(place), Vec::new())), "the turn knows its message");
    // The agent asked for a sign-in; after it the message goes again, as it is done on a sign-in.
    ended(&script, "");
    poll(&mut app, &ctx);
    app.agent_chat.queued = app.agent_chat.last.take();
    script.report(Event::Ready { agent: "fake".into(), images: true });
    poll(&mut app, &ctx);
    assert_eq!(app.agent_chat.last, Some(("the new one".into(), true, Some(place), Vec::new())), "sent again, still for the same message");
    assert_eq!(app.agent_chat.items.iter().filter(|i| matches!(i, Item::User { .. })).count(), 2, "not a new message");
    // Its picture goes there, once; the older message gets none.
    app.agent_chat.keep_picture(Some(place), test_png([1, 2, 3, 255], 4));
    app.agent_chat.keep_picture(Some(place), test_png([1, 2, 3, 255], 4));
    let counts: Vec<usize> =
        app.agent_chat.items.iter().filter_map(|i| if let Item::User { pictures, .. } = i { Some(pictures.len()) } else { None }).collect();
    assert_eq!(counts, [0, 1]);
}

/// The picture viewer opens over the chat's own window too, not only over the docked panel (#1745).
#[test]
fn the_viewer_opens_in_the_popped_out_window() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.app.preferences.agent_chat_popped = true;
    h.app.agent_chat.items.push(Item::User {
        text: "look".into(),
        image: true,
        pictures: vec![Picture::new(test_png([200, 30, 30, 255], 8), "first view")],
        context: Context::default(),
        files: Vec::new(),
    });
    h.frames(2);
    let Item::User { pictures, .. } = &h.app.agent_chat.items[0] else { panic!("the message") };
    let first = pictures[0].clone();
    assert!(until_decoded(&mut h, &first), "the picture was decoded");
    let at = crate::scenario::handle_at("agent_chat:picture:0:0").expect("the thumbnail is drawn in the window");
    h.call("ui.click", json!({"x": at.x, "y": at.y}));
    h.frames(4);
    assert!(h.app.agent_chat.gallery.is_some(), "the click opened the viewer");
    assert!(crate::scenario::rect_of("agent_chat:gallery_picture").is_some(), "and it is drawn");
}

// ---- Pick context (#2286) ----

/// Put `sels` into the open picker's inputs the way clicks would: each in the input that takes it.
fn pick_into_dialog(app: &mut SolveApp, sels: &[solidcraft_engine::Sel]) {
    use solidcraft_engine::Sel;
    let mut d = app.dialog.take().expect("the picker is open");
    for sel in sels {
        d.active = match sel {
            Sel::Face { .. } | Sel::Edge { .. } | Sel::Vertex { .. } => 0,
            Sel::Feature { .. } => 1,
            _ => 2,
        };
        d.pick(&app.session, sel.clone());
    }
    app.dialog = Some(d);
}

/// Pick context opens the picker with Elements, Features and Bodies; OK puts what was picked in the
/// tray above the message box, as chips with an ×; × takes one out; Cancel keeps what was there.
#[test]
fn the_picker_fills_a_tray_of_chips_that_can_be_trimmed() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    let sels = box_selection(&mut h.app);
    h.frames(3);
    let [x, y] = h.widget("Pick context").expect("the button is there");
    h.call("ui.click", json!({"x": x, "y": y}));
    h.frames(3);
    assert!(matches!(h.app.dialog.as_ref().map(|d| &d.kind), Some(crate::dialogs::Kind::ChatContext)), "the picker opened");
    assert_eq!(h.app.dialog.as_ref().map(|d| d.inputs.iter().map(|i| i.label).collect::<Vec<_>>()), Some(vec!["Elements", "Features", "Bodies"]));
    pick_into_dialog(&mut h.app, &sels);
    h.frames(3);
    let [x, y] = h.widget("OK").expect("OK is offered");
    h.call("ui.click", json!({"x": x, "y": y}));
    h.frames(4);
    assert!(h.app.dialog.is_none(), "OK closed the picker");
    assert_eq!(h.app.agent_chat.picked.len(), 4);
    assert_eq!(h.app.agent_chat.picked_context.items.len(), 4);
    for heading in ["Elements 2", "Features 1", "Bodies 1"] {
        assert!(h.widget(heading).is_some(), "{heading} is drawn in the tray");
    }
    // × takes the body out: the tray and the picks stay in step.
    let cross = crate::scenario::handle_at("agent_chat:picked_remove:3").expect("every chip has an ×");
    h.call("ui.click", json!({"x": cross.x, "y": cross.y}));
    h.frames(4);
    assert_eq!(h.app.agent_chat.picked.len(), 3);
    assert!(h.app.agent_chat.picked_context.items.iter().all(|i| i.kind != context::Kind::Body));
    // Reopened, the picker holds what is in the tray; Cancel leaves it alone.
    start_pick(&mut h.app);
    assert_eq!(h.app.dialog.as_ref().map(|d| d.inputs.iter().map(|i| i.items.len()).collect::<Vec<_>>()), Some(vec![2, 1, 0]));
    pick_into_dialog(&mut h.app, &[solidcraft_engine::Sel::Body { name: "Body1".into() }]);
    crate::dialogs::cancel(&mut h.app);
    assert!(h.app.dialog.is_none());
    assert_eq!(h.app.agent_chat.picked.len(), 3, "Cancel keeps what was picked before");
    // The control channel opens it too and reports what is picked.
    let reply = h.call("ui.agent_chat", json!({"pick": true}));
    assert!(matches!(h.app.dialog.as_ref().map(|d| &d.kind), Some(crate::dialogs::Kind::ChatContext)));
    assert_eq!(reply["result"]["picked"].as_array().map(Vec::len), Some(3));
    crate::dialogs::cancel(&mut h.app);
    // While another dialog is open, the button does nothing.
    h.app.start("solid.box");
    let before = h.app.dialog.is_some();
    start_pick(&mut h.app);
    assert!(before && !matches!(h.app.dialog.as_ref().map(|d| &d.kind), Some(crate::dialogs::Kind::ChatContext)));
}

/// What was picked goes with the next message, once, together with an attached selection that does
/// not repeat it.
#[test]
fn the_picks_go_with_the_next_message_once() {
    let (mut app, script, ctx) = app_with_script();
    let sels = box_selection(&mut app);
    start_pick(&mut app);
    pick_into_dialog(&mut app, &sels[2..]); // the feature and the body
    let d = app.dialog.take().expect("the picker");
    pick_done(&mut app, &d).expect("OK");
    assert_eq!(app.agent_chat.picked_context.items.len(), 2);

    // The same body is also selected and Attach selection is on: it is listed once.
    app.session.selection = vec![sels[3].clone(), sels[0].clone()];
    app.agent_chat.attach_selection = true;
    send(&mut app, &ctx, "look at these".into());
    let prompt = script.calls().pop().expect("a prompt");
    assert!(prompt.starts_with("prompt: look at these\n\n[Selected in SolidCraft"), "{prompt}");
    assert!(prompt.contains("3 item(s)"), "two picked and one more selected: {prompt}");
    assert_eq!(prompt.matches("- Body1").count(), 1, "{prompt}");
    let Some(Item::User { context, .. }) = app.agent_chat.items.last() else { panic!("a sent message") };
    assert_eq!(context.items.len(), 3);
    assert!(app.agent_chat.picked.is_empty() && app.agent_chat.picked_context.is_empty(), "the tray is empty again");
    // The next message carries nothing of it.
    ended(&script, "");
    poll(&mut app, &ctx);
    app.agent_chat.attach_selection = false;
    send(&mut app, &ctx, "and now?".into());
    assert_eq!(script.calls().pop().as_deref(), Some("prompt: and now?"));
}

/// Two clicks on two rims are two things (the chat counts and describes what was clicked). The
/// cylinder is a circle drawn as two half arcs, extruded: one wall, each rim one closed edge.
#[test]
fn a_rim_clicked_once_is_one_item_not_its_pieces() {
    use solidcraft_engine::Sel;
    let (mut app, script, ctx) = app_with_script();
    app.run("sketch.create", json!({"plane": "XY"})).expect("a sketch");
    app.run("sketch.arc.three_point", json!({"start": [10, 0], "end": [-10, 0], "through": [0, 10]})).expect("an arc");
    app.run("sketch.arc.three_point", json!({"start": [-10, 0], "end": [10, 0], "through": [0, -10]})).expect("an arc");
    app.run("sketch.finish", json!({})).expect("the sketch");
    app.run("solid.extrude", json!({"distance": 20})).expect("a cylinder");
    let (name, top, bottom, mid_top, mid_bottom) = {
        let st = crate::open_meshing::session_world(&app.session);
        let body = &st.bodies[0];
        let mesh = body.mesh();
        let rim_at = |z: f64| mesh.edges.iter().position(|e| e.len() > 3 && e.iter().all(|p| (p.z - z).abs() < 1e-6)).expect("a rim");
        let (top, bottom) = (rim_at(20.0), rim_at(0.0));
        (body.name.clone(), top, bottom, crate::viewport::polyline_mid(&mesh.edges[top]), crate::viewport::polyline_mid(&mesh.edges[bottom]))
    };
    crate::viewport::select(&mut app, Some(Sel::Edge { body: name.clone(), index: top, point: mid_top }), false, false);
    crate::viewport::select(&mut app, Some(Sel::Edge { body: name, index: bottom, point: mid_bottom }), true, false);
    assert_eq!(app.session.selection.len(), 2, "two clicks selected the two rims");
    assert_eq!(selected_count(&mut app), 2, "the checkbox counts the two clicks");

    app.agent_chat.attach_selection = true;
    send(&mut app, &ctx, "these two rims".into());
    let prompt = script.calls().pop().expect("a prompt");
    assert!(prompt.contains("2 item(s)"), "{prompt}");
    assert!(!prompt.contains("pieces)"), "{prompt}");
    // A rim of radius 10 is about 62.8 mm round (the kernel's own figure), the whole circle.
    assert_eq!(prompt.matches("Edge 62.").count(), 2, "{prompt}");
    let Some(Item::User { context, .. }) = app.agent_chat.items.last() else { panic!("a sent message") };
    assert_eq!(context.items.len(), 2);
    assert_eq!(context.total(), 2);
    // The detail names the index the tools take.
    let indices = [top, bottom].map(|i| format!("index {i}, "));
    assert!(context.items.iter().zip(&indices).all(|(item, index)| item.detail.contains(index)), "{:?}", context.items);
}

/// The same for a surface the kernel splits at a seam: one click on the cylinder's wall is one item.
#[test]
fn a_split_wall_clicked_once_is_one_item() {
    use solidcraft_engine::Sel;
    let (mut app, script, ctx) = app_with_script();
    app.run("solid.cylinder", json!({"radius": 10, "height": 20})).expect("a cylinder");
    let wall = app.session.execute("model.faces", &json!({"body": "Body1"})).expect("faces")["faces"]
        .as_array()
        .and_then(|faces| faces.iter().find(|f| f["cylinder"]["radius"].is_number()).cloned())
        .expect("the wall");
    let at = &wall["point"];
    let point = solidcore_record::Vec3::new(at[0].as_f64().unwrap_or(0.0), at[1].as_f64().unwrap_or(0.0), at[2].as_f64().unwrap_or(0.0));
    crate::viewport::select(
        &mut app,
        Some(Sel::Face { body: "Body1".into(), index: wall["index"].as_u64().unwrap_or(0) as usize, point }),
        false,
        false,
    );
    assert!(!app.session.selection.is_empty());
    assert_eq!(selected_count(&mut app), 1, "one click, however many patches it selected: {:?}", app.session.selection);
    app.agent_chat.attach_selection = true;
    send(&mut app, &ctx, "this wall".into());
    let prompt = script.calls().pop().expect("a prompt");
    assert!(prompt.contains("1 item(s)") && prompt.contains("Round face r10"), "{prompt}");
}

// ---- Attached pictures in the shared viewer ----

/// A small picture encoded as `format`.
fn encoded(format: image::ImageFormat, width: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let image = image::RgbaImage::from_pixel(width, 4, image::Rgba([40, 160, 90, 255]));
    let image = if format == image::ImageFormat::Jpeg {
        image::DynamicImage::ImageRgb8(image::DynamicImage::ImageRgba8(image).to_rgb8())
    } else {
        image::DynamicImage::ImageRgba8(image)
    };
    image.write_to(&mut std::io::Cursor::new(&mut out), format).expect("encoded");
    out
}

/// Frames until a picture is decoded (it is, in the background, on first draw), at most two seconds.
fn until_decoded(h: &mut crate::scenario::Harness, picture: &Picture) -> bool {
    for _ in 0..200 {
        h.frames(1);
        if picture.decode.texture.get().is_some() {
            h.frames(2);
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Pictures in any format the attachments take are drawn: JPEG and GIF as well as PNG, decoded off
/// the window thread (the first frames show a spinner), one that isn't a picture says so.
#[test]
fn attached_pictures_of_any_format_are_decoded_in_the_background() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    for (format, width) in [(image::ImageFormat::Png, 8), (image::ImageFormat::Jpeg, 16), (image::ImageFormat::Gif, 12)] {
        let picture = Picture::new(encoded(format, width), format!("photo.{format:?}"));
        h.app.agent_chat.items.clear();
        h.app.agent_chat.items.push(Item::User {
            text: "look".into(),
            image: false,
            pictures: vec![picture.clone()],
            files: Vec::new(),
            context: Context::default(),
        });
        h.frames(2);
        assert!(until_decoded(&mut h, &picture), "{format:?} was decoded");
        let texture = picture.decode.texture.get().and_then(|t| t.as_ref()).unwrap_or_else(|| panic!("{format:?} is a texture"));
        assert_eq!(texture.size()[0], width as usize, "{format:?} keeps its width");
        assert!(crate::scenario::handle_at("agent_chat:picture:0:0").is_some(), "{format:?} has its thumbnail");
    }
    // Bytes that are no picture: the name where the thumbnail would be, and nothing breaks.
    let broken = Picture::new(b"not a picture at all".to_vec(), "fake.png");
    h.app.agent_chat.items.clear();
    h.app.agent_chat.items.push(Item::User {
        text: "look".into(),
        image: false,
        pictures: vec![broken.clone()],
        files: Vec::new(),
        context: Context::default(),
    });
    assert!(until_decoded(&mut h, &broken));
    assert!(matches!(broken.decode.texture.get(), Some(None)), "it could not be read");
    assert!(h.widget("fake.png").is_some(), "its name stands in for the thumbnail");
}

/// A picture attached as a file goes to the viewer like the view's own picture, named in it, and
/// the view's picture is added beside it rather than replacing it, once.
#[test]
fn an_attached_picture_and_the_views_picture_share_the_viewer() {
    let mut chat = AgentChat::default();
    let attached = Picture::shared(Arc::new(encoded(image::ImageFormat::Jpeg, 16)), "photo.jpg", false);
    chat.push(Item::User { text: "see".into(), image: true, pictures: vec![attached], files: Vec::new(), context: Context::default() });
    chat.keep_picture(Some(0), encoded(image::ImageFormat::Png, 8));
    let Some(Item::User { pictures, .. }) = chat.items.last() else { panic!("a sent message") };
    assert_eq!(pictures.iter().map(|p| (p.note.as_str(), p.view)).collect::<Vec<_>>(), [("photo.jpg", false), ("", true)]);
    // A second view picture is not added to the same message.
    chat.keep_picture(Some(0), encoded(image::ImageFormat::Png, 8));
    let Some(Item::User { pictures, .. }) = chat.items.last() else { panic!("a sent message") };
    assert_eq!(pictures.len(), 2);
}

/// Settings › AI › Add agent opens its dialog from the Home tab too (#3483, #3513).
#[test]
fn add_agent_opens_its_dialog_from_home() {
    use crate::scenario::handle_at;
    let mut h = crate::scenario::Harness::new();
    h.call("ui.resize", json!({"width": 1600.0, "height": 1000.0}));
    h.call("ui.home", json!({"open": true}));
    h.call("ui.prefs", json!({"open": "AI"}));
    h.frames(4);
    assert!(h.app.home.open, "Home is showing");
    let add = handle_at("prefs:ai_add").expect("Add agent is drawn");
    h.call("ui.click", json!({"x": add.x, "y": add.y}));
    h.frames(4);
    assert!(h.app.agent_chat.settings.add.is_some(), "Add agent opened its dialog");
    assert!(handle_at("ai_add:add").is_some(), "the dialog is drawn over Home");
}

/// `ui.agent_chat` `add_agent` takes an endpoint's request timeout (rounded and clamped to 30 s
/// ..= 1 h, 300 s when left out) and the state reports it.
#[test]
fn add_agent_takes_a_request_timeout_and_the_state_reports_it() {
    let ctx = egui::Context::default();
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    let timeout = |app: &mut SolveApp, id: &str| {
        let state = control(app, &ctx, &json!({})).unwrap();
        state["agents"].as_array().unwrap().iter().find(|a| a["id"] == id).map(|a| a["request_timeout_s"].clone())
    };
    let add = |app: &mut SolveApp, t: Value| {
        let mut a = json!({"id": "slow", "name": "Slow", "kind": "api", "base_url": "http://localhost:11434/v1", "model": "m"});
        if !t.is_null() {
            a["request_timeout_s"] = t;
        }
        control(app, &ctx, &json!({"add_agent": a}))
    };
    add(&mut app, json!(1200)).unwrap();
    assert_eq!(timeout(&mut app, "slow"), Some(json!(1200)));
    assert_eq!(app.preferences.agents.iter().find(|a| a.id == "slow").unwrap().request_timeout_s, 1200);
    add(&mut app, json!(99999.7)).unwrap();
    assert_eq!(timeout(&mut app, "slow"), Some(json!(3600)));
    add(&mut app, json!(-3)).unwrap();
    assert_eq!(timeout(&mut app, "slow"), Some(json!(30)));
    add(&mut app, Value::Null).unwrap();
    assert_eq!(timeout(&mut app, "slow"), Some(json!(300)));
    assert!(add(&mut app, json!("long")).is_err());
    control(&mut app, &ctx, &json!({"add_agent": {"id": "acp", "name": "A", "command": "x --acp"}})).unwrap();
    assert_eq!(timeout(&mut app, "acp"), Some(Value::Null), "only endpoints have a request timeout");
}

#[test]
fn no_tool_turn_reports_empty_reply_and_unperformed_edit() {
    for (prompt, reply) in [
        ("Hello", ""),
        ("Hello", "   "),
        ("Make a box", "Here is how to make a box."),
        ("Can you rename the body?", "I cannot rename bodies."),
        ("Could you please make a change to the current shape", "Here is the command."),
        ("Sketch a rectangle", r#"{"name":"batch","arguments":{"commands":[]}}"#),
    ] {
        let (mut app, _, _) = app_with_script();
        app.agent_chat.tools_port = Some(12345);
        start_turn(&mut app, prompt.into(), false, None, vec![]);
        if !reply.is_empty() {
            app.agent_chat.on_event(Event::Text { thought: false, text: reply.into() });
        }
        app.agent_chat.on_event(Event::TurnEnded { reason: String::new() });
        end_turn(&mut app);
        let expected = tr("ui.agent_chat_no_tool_call");
        assert!(app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, error: false } if text == expected)), "{prompt}");
        assert_eq!(status(&app)["turnWarning"], expected);
        assert!(app.session.undo.is_empty());
        start_turn(&mut app, "What is a fillet?".into(), false, None, vec![]);
        assert!(status(&app)["turnWarning"].is_null());
        assert!(app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, .. } if text == expected)));
        new_chat(&mut app);
        assert!(status(&app)["turnWarning"].is_null());
        assert!(app.agent_chat.items.is_empty());
    }
}

#[test]
fn no_tool_turn_warning_does_not_follow_a_tool_call() {
    let (mut app, _, _) = app_with_script();
    app.agent_chat.tools_port = Some(12345);
    start_turn(&mut app, "Make a box".into(), false, None, vec![]);
    app.agent_chat.on_event(Event::Tool {
        id: "t".into(),
        title: Some("batch".into()),
        kind: None,
        status: Some(ToolStatus::Failed),
        input: None,
        output: None,
    });
    app.agent_chat.on_event(Event::TurnEnded { reason: String::new() });
    end_turn(&mut app);
    assert!(status(&app).get("turnWarning").is_some());
    assert!(status(&app)["turnWarning"].is_null());
    assert!(!app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { .. })));
}

#[test]
fn no_tool_turn_warning_leaves_answers_and_incomplete_turns_quiet() {
    for (prompt, reply, reason, tools) in [
        ("What is a fillet?", "A fillet rounds an edge.", "", true),
        ("How do I make a box?", "Use the Box command.", "", true),
        ("Make a box", "", "cancelled", true),
        ("Make a box", "", "error", true),
        ("Make a box", "", "", false),
    ] {
        let (mut app, _, _) = app_with_script();
        app.agent_chat.tools_port = tools.then_some(12345);
        start_turn(&mut app, prompt.into(), false, None, vec![]);
        app.agent_chat.on_event(Event::Text { thought: false, text: reply.into() });
        app.agent_chat.on_event(Event::TurnEnded { reason: reason.into() });
        end_turn(&mut app);
        assert!(status(&app).get("turnWarning").is_some());
        assert!(status(&app)["turnWarning"].is_null(), "{prompt}: {reason}");
        assert!(!app.agent_chat.items.iter().any(|i| matches!(i, Item::Notice { text, .. } if text == tr("ui.agent_chat_no_tool_call"))));
    }
}

/// A turn that ends with nothing shown because the reply hit its length limit, or the model
/// declined, says so instead of blaming tool support (#9495).
#[test]
fn a_cut_off_or_declined_reply_says_so_instead_of_blaming_tools() {
    for (reason, said) in [("max_tokens", "ui.agent_chat_reply_cut_off"), ("refusal", "ui.agent_chat_reply_declined")] {
        let (mut app, _, _) = app_with_script();
        app.agent_chat.tools_port = Some(12345);
        start_turn(&mut app, "Make a box".into(), false, None, vec![]);
        app.agent_chat.on_event(Event::TurnEnded { reason: reason.into() });
        end_turn(&mut app);
        assert!(status(&app)["turnWarning"].is_null(), "{reason}");
        let notices: Vec<&str> = app
            .agent_chat
            .items
            .iter()
            .filter_map(|i| if let Item::Notice { text, error: false } = i { Some(text.as_str()) } else { None })
            .collect();
        assert_eq!(notices, [tr(said)], "{reason}");
    }
}

// ---- Tool calls in words (#1457) ----

fn in_mm() -> summary::Units<'static> {
    summary::Units { units: "mm", decimals: 2 }
}

fn said(title: &str, input: Value, output: Option<&str>) -> Option<String> {
    summary::summary(title, Some(&input), output, true, in_mm())
}

/// The issue's examples: a card says what the call did, from the command's label, its main
/// parameters and what it returned.
#[test]
fn tool_calls_read_as_what_they_did() {
    let extrude = json!({"command": "solid.extrude", "params": {"sketch": "Sketch1", "distance": 10}});
    assert_eq!(said("execute: solid.extrude", extrude.clone(), None).as_deref(), Some("Extrude Sketch1 by 10 mm"));
    // As an ACP agent names the bridged tool, with the body the result made.
    let made = r#"{"feature": 4, "name": "Extrude1", "created_bodies": ["Body2"], "changed_bodies": []}"#;
    assert_eq!(said("mcp__solidcraft-app__execute", extrude, Some(made)).as_deref(), Some("Extrude Sketch1 by 10 mm → Body2"));
    let edges = json!([[20, 0, 20], [20, 30, 20], [0, 15, 20], [40, 15, 20]]);
    let batch = json!({"commands": [
        {"command": "solid.box", "params": {"length": 40, "width": 30, "height": 20}},
        {"command": "solid.fillet", "params": {"edges": edges, "radius": 4}},
    ]});
    assert_eq!(said("batch: solid.box, solid.fillet", batch, None).as_deref(), Some("Box 40 × 30 × 20 mm, then Fillet 4 edges (r 4 mm)"));
    let measured = r#"{"body_count": 1, "bodies": [{"name": "Block", "volume_mm3": 23620.4, "bbox": {"min": [0, 0, 0], "max": [40, 30, 20]}}]}"#;
    assert_eq!(said("measure", json!({"body": "Block"}), Some(measured)).as_deref(), Some("Measured Block: 40 × 30 × 20 mm, 23.6 cm³"));
    assert_eq!(said("save", json!({"path": "C:\\designs\\block.solidcraft"}), None).as_deref(), Some("Saved block.solidcraft"));
    // Still running, or failed: what it does, not what it did.
    let running = summary::summary("save", Some(&json!({"path": "/home/me/block.solidcraft"})), None, false, in_mm());
    assert_eq!(running.as_deref(), Some("Save block.solidcraft"));
    // Lengths in the design's units; an expression as written; one edge is one.
    let inches = summary::Units { units: "in", decimals: 3 };
    let one = json!({"command": "solid.fillet", "params": {"edges": [[0, 0, 0]], "radius": 2.54}});
    assert_eq!(summary::summary("execute: solid.fillet", Some(&one), None, true, inches).as_deref(), Some("Fillet 1 edge (r 0.1 in)"));
    let expr = json!({"command": "solid.extrude", "params": {"sketch": "Sketch1", "distance": "2 * depth"}});
    assert_eq!(said("execute: solid.extrude", expr, None).as_deref(), Some("Extrude Sketch1 by 2 * depth"));
    // A long batch names its first steps and counts the rest.
    let steps: Vec<Value> = (0..6).map(|_| json!({"command": "edit.undo"})).collect();
    assert_eq!(said("batch: edit.undo", json!({"commands": steps}), None).as_deref(), Some("Undo, then Undo, then Undo, then Undo, and 2 more"));
}

/// Paths in a card never show their folders (the owner's "way too much"): the screenshot is just
/// "Screenshot", files are their names, temporary files are not named, and the full text stays in
/// Details and the tooltip.
#[test]
fn tool_cards_show_file_names_never_paths() {
    let shot = r"ui.screenshot: C:\Users\bherbruck\AppData\Local\Temp\solidcraft-mcp-8608-b4c7b64\shot-0.png";
    assert_eq!(said(shot, json!({}), None).as_deref(), Some("Screenshot"));
    assert_eq!(said("mcp__solidcraft-app__ui.screenshot", json!({}), None).as_deref(), Some("Screenshot"));
    // The fallback title (another agent's tools) is cut to file names; temp files drop out.
    assert_eq!(summary::plain_title(shot), "ui.screenshot");
    assert_eq!(summary::plain_title(r"Read C:\Users\Jane Doe\Documents\part.step"), "Read part.step");
    assert_eq!(summary::plain_title(r"Write D:/work/out/block.solidcraft now"), "Write block.solidcraft now");
    assert_eq!(summary::plain_title(r"Open \\nas\share\parts\bracket.step"), "Open bracket.step");
    assert_eq!(summary::plain_title("Read /home/bherbruck/designs/part.step"), "Read part.step");
    assert_eq!(summary::plain_title("Edit ~/designs/part.step."), "Edit part.step.");
    assert_eq!(summary::plain_title("cat \"/home/me/a b.txt\" | head"), "cat \"a b.txt\" | head");
    assert_eq!(summary::plain_title("Read /tmp/claude-1000/scratch/out.txt"), "Read");
    assert_eq!(summary::plain_title("ui.render: /var/folders/xy/T/shot.png"), "ui.render");
    assert_eq!(summary::plain_title("Run mcp__x__y with a/b and /"), "Run mcp__x__y with a/b and /");
    // SolidCraft's own file tools: the name alone; a temp file has none.
    assert_eq!(
        said("export", json!({"path": "/home/me/cad/part.step"}), None).as_deref().map(|s| s.ends_with("part.step") && !s.contains('/')),
        Some(true)
    );
    assert_eq!(
        said("open", json!({"path": r"C:\cad\block.solidcraft"}), None).as_deref().map(|s| s.ends_with(" block.solidcraft") && !s.contains('\\')),
        Some(true)
    );
    assert_eq!(said("save", json!({"path": "/tmp/x/block.solidcraft"}), None).as_deref(), Some("Save"));
    assert_eq!(said("save", json!({"path": r"C:\Users\me\AppData\Local\Temp\a.solidcraft"}), Some("")).as_deref(), Some("Save"));
}

/// Another agent's own tools, an unknown command and malformed or hostile calls keep the card's
/// title (no words), and nothing panics or grows without bound.
#[test]
fn unknown_or_malformed_tool_calls_keep_their_title_and_never_panic() {
    for title in ["Read /etc/hosts", "mcp__github__create_issue", "mcp__github__save", "save /tmp/x.txt", "", "  ", "__", "mcp____"] {
        assert_eq!(said(title, json!({"path": "x.solidcraft", "command": "solid.box"}), None), None, "{title}");
    }
    assert_eq!(said("execute: solid.nope", json!({"command": "solid.nope"}), None), None, "an unknown command");
    assert_eq!(summary::summary("execute: solid.box", None, None, true, in_mm()), None, "arguments that did not parse");
    let long = "x".repeat(100_000);
    let many: Vec<Value> = (0..10_000).map(|i| json!({"command": format!("solid.{i}"), "params": {"distance": i}})).collect();
    let hostile = [
        json!(null),
        json!([1, 2, 3]),
        json!("text"),
        json!({"command": 5, "params": "x"}),
        json!({"command": "solid.box", "params": {"length": "", "width": [], "height": {"a": 1}}}),
        json!({"command": "solid.box", "params": {"length": 1e308, "width": -1e308, "height": 0}}),
        json!({"command": "solid.extrude", "params": {"sketch": long, "distance": long, "edges": [long], "angle": 1e300}}),
        json!({"command": "solid.extrude", "params": {"sketch": "a\nb\u{0}\u{202e}c", "distance": -0.0}}),
        json!({"commands": many}),
        json!({"commands": [null, 1, {"command": "batch", "params": {"commands": [{"command": "solid.box"}]}}, {"id": "save"}]}),
        json!({"body": ["Block"], "path": {"x": 1}, "name": 3, "value": null}),
    ];
    let outputs = [
        None,
        Some(""),
        Some("{"),
        Some("[]"),
        Some(r#"{"bodies": [{"bbox": {"min": [1], "max": "x"}}]}"#),
        Some(r#"{"created_bodies": [1, null, "B"]}"#),
    ];
    for tool in ["execute", "batch", "measure", "save", "export", "open", "new_design", "set_parameter", "mcp__solidcraft__batch"] {
        for input in &hostile {
            for output in outputs {
                for done in [true, false] {
                    if let Some(s) = summary::summary(tool, Some(input), output, done, in_mm()) {
                        assert!(s.chars().count() <= 240 && !s.contains('\n'), "{tool}: {s}");
                    }
                }
            }
        }
    }
    let huge = format!("{{\"bodies\": [{}]}}", "{\"name\": \"B\"},".repeat(20_000));
    assert_eq!(said("measure", json!({}), Some(&huge)).as_deref(), Some("Measure"), "a result too large is not read");
}

/// The card's title is the summary; the raw arguments and result wait behind Details until it is
/// opened. A call with no words keeps its title.
#[test]
fn a_tool_card_shows_its_summary_and_keeps_the_json_behind_details() {
    use crate::scenario::handle_at;
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    let input = json!({"command": "solid.extrude", "params": {"sketch": "Sketch1", "distance": 10}});
    let output = r#"{"feature": 4, "name": "Extrude1", "created_bodies": []}"#;
    let n = h.app.agent_chat.items.len();
    h.app.agent_chat.items.push(Item::Tool {
        id: "t".into(),
        title: "execute: solid.extrude".into(),
        kind: String::new(),
        status: Some(ToolStatus::Completed),
        input: Some(input.clone()),
        output: Some(output.into()),
    });
    h.app.agent_chat.items.push(Item::Tool {
        id: "u".into(),
        title: "mcp__github__create_issue".into(),
        kind: String::new(),
        status: Some(ToolStatus::InProgress),
        input: None,
        output: None,
    });
    h.frames(3);
    assert!(h.widget("Extrude Sketch1 by 10 mm").is_some(), "the summary is the card's title");
    assert!(h.widget("execute: solid.extrude").is_none(), "the raw title is not shown");
    assert!(h.widget("mcp__github__create_issue").is_some(), "a call with no words keeps its title");
    assert!(h.widget("Details").is_some(), "the expander is named");
    let pretty = serde_json::to_string_pretty(&input).unwrap();
    assert!(h.widget("Arguments").is_none() && h.widget(&pretty).is_none() && h.widget(output).is_none(), "the JSON waits behind Details");
    let card = handle_at(&format!("agent_chat:tool:{n}")).expect("the card");
    h.call("ui.click", json!({"x": card.x, "y": card.y}));
    h.frames(3);
    assert!(h.widget("Arguments").is_some() && h.widget(&pretty).is_some(), "the arguments unfold");
    assert!(h.widget("Result").is_some() && h.widget(output).is_some(), "and what it returned");
}

/// The card says "Screenshot"; the full temp path is only under Details.
#[test]
fn a_screenshot_card_hides_the_path_until_details() {
    use crate::scenario::handle_at;
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    let path = r"C:\Users\bherbruck\AppData\Local\Temp\solidcraft-mcp-8608-b4c7b64\shot-0.png";
    let n = h.app.agent_chat.items.len();
    h.app.agent_chat.items.push(Item::Tool {
        id: "s".into(),
        title: format!("ui.screenshot: {path}"),
        kind: String::new(),
        status: Some(ToolStatus::Completed),
        input: Some(json!({"path": path})),
        output: Some(path.into()),
    });
    h.frames(3);
    assert!(h.widget("Screenshot").is_some(), "the card says what it did");
    assert!(h.widget(path).is_none() && h.widget(&format!("ui.screenshot: {path}")).is_none(), "no path in the card");
    let card = handle_at(&format!("agent_chat:tool:{n}")).expect("the card");
    h.call("ui.click", json!({"x": card.x, "y": card.y}));
    h.frames(3);
    assert!(h.widget("Result").is_some() && h.widget(path).is_some(), "Details holds the full path");
}

/// The composer's input box fills its parent composer area on all four sides, in every layout:
/// the only inset left is the text's own padding inside the box (no second margin around it).
#[test]
fn the_composer_box_fills_its_parent_in_every_layout() {
    use crate::scenario::rect_of;
    for (layout, popped) in [("floating", false), ("docked", false), ("floating", true), ("docked", true)] {
        let mut h = narrow_chat();
        h.call("ui.resize", json!({"width": 1600, "height": 1000}));
        h.call("ui.set", json!({"layout": layout}));
        h.app.preferences.agent_chat_popped = popped;
        h.app.agent_chat.state = Some(State::Ready);
        h.frames(4);
        let footer = rect_of("agent_chat:composer_footer").unwrap_or_else(|| panic!("{layout} popped={popped}: the composer area"));
        let boxed = rect_of("agent_chat:composer_box").expect("the input box");
        let (l, r, t, b) =
            (boxed.left() - footer.left(), footer.right() - boxed.right(), boxed.top() - footer.top(), footer.bottom() - boxed.bottom());
        for (edge, gap) in [("left", l), ("right", r), ("top", t), ("bottom", b)] {
            assert!(gap.abs() <= 1.0, "{layout} popped={popped}: box {edge} edge is {gap} from its parent ({boxed:?} in {footer:?})");
        }
        let input = rect_of("agent_chat:input").expect("the text");
        assert!(input.left() > boxed.left() + 2.0, "{layout} popped={popped}: the text keeps its own padding");
        let send = rect_of("agent_chat:send").expect("Send");
        assert!(boxed.contains_rect(send), "{layout} popped={popped}: Send stays inside the box");
    }
}

/// The composer hint (idle and busy, in every language) stays on one row, even at the narrowest
/// panel, and is cut with an ellipsis instead of wrapping.
#[test]
fn the_composer_hint_stays_on_one_row_at_the_narrowest_width() {
    let mut h = narrow_chat();
    h.app.agent_chat.state = Some(State::Ready);
    h.frames(4);
    let width = crate::scenario::rect_of("agent_chat:input").expect("the text box").width();
    let ctx = crate::fonts::test_context();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/locales");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let language = path.file_stem().unwrap().to_string_lossy().into_owned();
        let _scope = crate::i18n::scope(&language);
        for key in ["ui.agent_chat_placeholder", "ui.agent_chat_placeholder_busy"] {
            let hint = crate::i18n::tr(key).to_owned();
            let mut rows = 0;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                rows = crate::ui_kit::assistant_hint_galley(ui, &hint, width - 2.0 * crate::theme::Tokens::get().form_inset).rows.len();
            });
            output.textures_delta.clear();
            assert_eq!(rows, 1, "{language} {key} at {width} wide: {hint}");
            checked += 1;
        }
    }
    assert!(checked >= 20, "every language was checked");
}

/// The room tool windows get beside the docked Assistant, in a view that came out inside out
/// (the window of #9492): the Assistant's side gives way instead of panicking in `f32::clamp`.
#[test]
fn tool_room_beside_the_docked_assistant_survives_an_inside_out_view() {
    let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
    app.ui.layout = crate::shell::LayoutMode::Floating;
    app.preferences.ai_features = true;
    app.agent_chat.open = true;
    app.preferences.agent_chat_popped = false;
    app.agent_chat.panel_rect = Some(egui::Rect::from_min_max(egui::pos2(100.0, 40.0), egui::pos2(150.0, 600.0)));
    let inside_out = egui::Rect::from_min_max(egui::pos2(264.1875, 40.0), egui::pos2(159.0, 600.0));
    for side in ["right", "left"] {
        app.preferences.agent_chat_side = side.into();
        let free = tool_space(&app, inside_out);
        assert!(free.min.x.is_finite() && free.max.x.is_finite(), "{side}: {free:?}");
    }
}

// ---- Copy message (#10675) ----

/// A chat with a multi-paragraph prompt and a Markdown reply, both drawn.
fn chat_with_copiable_messages() -> crate::scenario::Harness {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    h.app.agent_chat.items.push(Item::User {
        text: "make a plate\n\nwith two holes:\n- 6 mm\n- 8 mm".into(),
        image: false,
        pictures: Vec::new(),
        files: Vec::new(),
        context: Context::default(),
    });
    h.app.agent_chat.items.push(Item::Agent { thought: false, text: "# Result\n\nA plate with holes.\n\n```rust\nlet x = 1;\n```".into() });
    h.frames(3);
    h
}

/// The text the last frame copied to the clipboard, if any.
fn copied_text(h: &crate::scenario::Harness) -> Option<String> {
    h.output.commands.iter().rev().find_map(|c| match c {
        egui::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    })
}

fn click_at(h: &mut crate::scenario::Harness, at: egui::Pos2, button: egui::PointerButton) {
    h.frame_with(vec![egui::Event::PointerMoved(at)]);
    h.frame_with(vec![egui::Event::PointerButton { pos: at, button, pressed: true, modifiers: egui::Modifiers::NONE }]);
    h.frame_with(vec![egui::Event::PointerButton { pos: at, button, pressed: false, modifiers: egui::Modifiers::NONE }]);
}

/// The visible button on a sent prompt copies the whole prompt, as entered.
#[test]
fn the_copy_message_button_copies_the_prompt_as_entered() {
    let mut h = chat_with_copiable_messages();
    let at = crate::scenario::handle_at("agent_chat:copy:0").expect("the prompt's Copy message button");
    click_at(&mut h, at, egui::PointerButton::Primary);
    assert_eq!(copied_text(&h).as_deref(), Some("make a plate\n\nwith two holes:\n- 6 mm\n- 8 mm"), "the whole prompt, paragraphs and list items");
}

/// A prompt at the top of a short conversation is as tall as its text and its Copy message
/// button: the button sits at the bubble's foot and the reply follows straight after, instead of
/// the bubble stretching over the empty view below it.
#[test]
fn a_prompt_at_the_top_of_the_conversation_is_as_tall_as_its_content() {
    // Kept to the end: the published handles and rects are its last frame's.
    let _h = chat_with_copiable_messages();
    let button = crate::scenario::handle_at("agent_chat:copy:0").expect("the prompt's Copy message button");
    let prompt = crate::scenario::rect_of("agent_chat:msg:0").expect("the prompt's message area");
    let reply = crate::scenario::rect_of("agent_chat:msg:1").expect("the reply's message area");
    let row = crate::theme::Tokens::get().control_height;
    assert!(prompt.bottom() - button.y < row, "the bubble ends {} below its button", prompt.bottom() - button.y);
    assert!(reply.top() - button.y < 2.0 * row, "the reply starts {} below the prompt's button", reply.top() - button.y);
}

/// The visible button on a reply copies its original Markdown: heading, paragraphs, list and
/// fenced code, not the laid-out text.
#[test]
fn the_copy_message_button_copies_the_reply_as_original_markdown() {
    let mut h = chat_with_copiable_messages();
    let at = crate::scenario::handle_at("agent_chat:copy:1").expect("the reply's Copy message button");
    click_at(&mut h, at, egui::PointerButton::Primary);
    assert_eq!(copied_text(&h).as_deref(), Some("# Result\n\nA plate with holes.\n\n```rust\nlet x = 1;\n```"));
}

/// Right-clicking a message opens its menu, and its row copies the same whole reply.
#[test]
fn right_clicking_a_message_offers_copy_message() {
    let mut h = chat_with_copiable_messages();
    let msg = crate::scenario::rect_of("agent_chat:msg:1").expect("the reply's message area");
    click_at(&mut h, msg.center(), egui::PointerButton::Secondary);
    h.frames(1); // let the popup settle at its final place before clicking its row
    let item = crate::scenario::handle_at("agent_chat:copy_menu:1").expect("the right-click menu's Copy message row");
    click_at(&mut h, item, egui::PointerButton::Primary);
    assert_eq!(copied_text(&h).as_deref(), Some("# Result\n\nA plate with holes.\n\n```rust\nlet x = 1;\n```"));
}

/// A reply that is still streaming copies the stable text received so far.
#[test]
fn a_streaming_reply_copies_the_text_received_so_far() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    h.app.agent_chat.items.push(Item::Agent { thought: false, text: "half a repl".into() });
    h.frames(3);
    let at = crate::scenario::handle_at("agent_chat:copy:0").expect("the streaming reply's button");
    click_at(&mut h, at, egui::PointerButton::Primary);
    assert_eq!(copied_text(&h).as_deref(), Some("half a repl"));
}

/// A picture-only prompt has no text to copy: no button, no menu, and a thought has none
/// either — the clipboard is never overwritten with an empty string.
#[test]
fn a_picture_only_prompt_and_a_thought_have_no_copy_message_action() {
    let mut h = crate::scenario::Harness::new();
    h.call("ui.agent_chat", json!({"open": true}));
    h.frames(3);
    h.app.agent_chat.items.push(Item::User {
        text: String::new(),
        image: true,
        pictures: Vec::new(),
        files: Vec::new(),
        context: Context::default(),
    });
    h.app.agent_chat.items.push(Item::Agent { thought: true, text: "pondering".into() });
    h.frames(3);
    assert!(crate::scenario::handle_at("agent_chat:copy:0").is_none(), "a picture-only prompt hides the text-copy action");
    assert!(crate::scenario::handle_at("agent_chat:copy:1").is_none(), "a thought has no copy action");
    assert!(h.widget("Copy message").is_none(), "nothing to click");
}
