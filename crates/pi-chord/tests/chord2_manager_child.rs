//! Port of upstream `test/manager.test.ts` with real child processes (Unix). The binary has its own harness because
//! it is also the child host the tests spawn ([`chord2_child::worker`]), the grandchild that child spawns and the
//! parent process of the inherited-lock test, chosen by environment.

#[cfg(unix)]
mod chord2_child;

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    unix::main()
}

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use pi_chord::host2::link::{Received, unpack};
    use pi_chord::host2::manager::{IPC_ENV, LOCK_ENV, OutputStream};
    use pi_chord::host2::wire::{Call, CallResult, Frame, Payload};
    use pi_chord::host2::{
        ChordErrorCode, Driver, Exit, ForkDriver, ForkDriverOptions, HostManager, HostManagerOptions, Instance, Status,
        process_lock,
    };
    use pi_chord::{BoxFuture, ChordResult, Json, json};

    use super::chord2_child::{self, GRANDCHILD, PARENT};

    pub fn main() -> ExitCode {
        if std::env::var_os(GRANDCHILD).is_some() {
            chord2_child::grandchild();
        }
        if std::env::var_os(IPC_ENV).is_some() {
            return runtime().block_on(chord2_child::worker());
        }
        if std::env::var_os(PARENT).is_some() {
            return runtime().block_on(inherited_lock_parent());
        }
        run_tests()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime")
    }

    type Case = (&'static str, fn() -> BoxFuture<()>);

    fn run_tests() -> ExitCode {
        let cases: [Case; 9] = [
            ("starts_a_child_host_links_it_and_stops_it_gracefully", || Box::pin(starts_links_and_stops())),
            ("large_payloads_cross_the_ipc_link_packed_both_ways", || Box::pin(large_payloads_cross_packed())),
            ("a_child_without_a_lock_does_not_inherit_the_parents_lock", || Box::pin(no_inherited_lock())),
            ("a_child_owned_lock_admits_one_live_process_and_replace_waits_for_the_exit", || {
                Box::pin(lock_and_replace())
            }),
            ("a_graceful_exit_still_takes_the_childs_descendants_with_it", || Box::pin(graceful_exit_kills_tree())),
            ("kills_the_process_tree_when_a_child_ignores_shutdown_past_the_grace", || Box::pin(grace_kills_tree())),
            ("reports_a_fatal_or_exiting_child_as_a_failed_start", || Box::pin(fatal_child_fails_start())),
            ("stops_an_idle_child_when_keep_alive_says_so", || Box::pin(keep_alive_stops_idle_child())),
            ("taps_the_childs_output_and_messages", || Box::pin(taps_output_and_messages())),
        ];
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|arg| arg == "--list") {
            cases.iter().for_each(|(name, _)| println!("{name}: test"));
            return ExitCode::SUCCESS;
        }
        let filter = args.iter().find(|arg| !arg.starts_with('-'));
        let selected: Vec<_> =
            cases.iter().filter(|(name, _)| filter.is_none_or(|f| name.contains(f.as_str()))).collect();
        println!("\nrunning {} tests", selected.len());
        let mut failed = Vec::new();
        for (name, case) in selected {
            let outcome = std::panic::catch_unwind(|| {
                let run = async { tokio::time::timeout(Duration::from_secs(60), case()).await };
                runtime().block_on(run).expect("timed out");
            });
            println!("test {name} ... {}", if outcome.is_ok() { "ok" } else { "FAILED" });
            if outcome.is_err() {
                failed.push(*name);
            }
        }
        let verdict = if failed.is_empty() { "ok" } else { "FAILED" };
        println!("\ntest result: {verdict}. {} failed: {failed:?}\n", failed.len());
        if failed.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
    }

    fn program() -> PathBuf {
        std::env::current_exe().expect("current exe")
    }

    fn options(env: &[(&str, &str)], lock: Option<&str>) -> ForkDriverOptions {
        let mut options = ForkDriverOptions::new(program());
        let env = [("HOST_NAME", "w1")].iter().chain(env);
        options.env = env.map(|(key, value)| (key.into(), value.into())).collect();
        options.init = Some(json!({"greeting": "hi"}));
        options.lock = lock.map(String::from);
        options
    }

    fn driver(env: &[(&str, &str)], lock: Option<&str>) -> Arc<dyn Driver> {
        Arc::new(ForkDriver::new(options(env, lock)))
    }

    fn manager(driver: Arc<dyn Driver>) -> HostManager {
        HostManager::new(HostManagerOptions::new(driver))
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("chord2-{}-{name}", std::process::id()))
    }

    /// Whether `pid` runs (a zombie does not).
    fn alive(pid: u32) -> bool {
        let Some(id) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
        if rustix::process::test_kill_process(id).is_err() {
            return false;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        !stat.rsplit_once(") ").is_some_and(|(_, rest)| rest.starts_with('Z'))
    }

    async fn until(what: &str, condition: impl Fn() -> bool) {
        for _ in 0..250 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Calls `member` on the worker over the instance's link and returns the result payload.
    async fn call(instance: &Arc<dyn Instance>, id: u64, member: &str, args: Json) -> Payload {
        let link = instance.link();
        let results: Arc<Mutex<Vec<CallResult>>> = Arc::default();
        let sink = results.clone();
        link.receive(Arc::new(move |received| {
            if let Received::Frame(Frame::Result(result)) = received {
                sink.lock().unwrap().push(result);
            }
        }));
        let args = Some(Payload::Value(args));
        link.send(&Frame::Call(Call { id, hold: 1, member: member.into(), args, root: None }));
        until("a result", || results.lock().unwrap().iter().any(|result| result.id == id)).await;
        let result = results.lock().unwrap().iter().find(|result| result.id == id).cloned().unwrap();
        result.value.expect("a value")
    }

    async fn starts_links_and_stops() {
        let manager = manager(driver(&[], None));
        let instance = manager.start().await.unwrap();
        assert_eq!(manager.status(), Status::Running);
        let greeting = call(&instance, 1, "echo", json!("x")).await;
        assert_eq!(unpack(&greeting).unwrap(), json!(format!("hi x from {}", instance.pid())));
        assert!(Arc::ptr_eq(&manager.start().await.unwrap(), &instance));
        manager.stop().await;
        assert_eq!(manager.status(), Status::Stopped);
        assert_eq!(instance.exited().await, Exit { code: Some(0), signal: None });
        assert!(instance.link().closed());
    }

    async fn large_payloads_cross_packed() {
        let manager = manager(driver(&[], None));
        let instance = manager.start().await.unwrap();
        let large = Json::array((0..40).map(|index| Json::from(format!("a{index}:{}", "x".repeat(4000)))).collect());
        let relayed = call(&instance, 2, "relay", large.clone()).await;
        assert!(matches!(relayed, Payload::Packed(_)), "the worker relays the packed bytes");
        assert_eq!(unpack(&relayed).unwrap(), large);
        manager.stop().await;
    }

    /// The parent of the inherited-lock test: it runs with `CHORD2_LOCK` set, and its child must not get it.
    async fn inherited_lock_parent() -> ExitCode {
        assert!(std::env::var_os(LOCK_ENV).is_some());
        let manager = manager(driver(&[], None));
        let started = manager.start().await;
        let running = manager.status() == Status::Running;
        manager.stop().await;
        match started {
            Ok(_) if running => ExitCode::SUCCESS,
            other => {
                eprintln!("child did not start: {:?}", other.err());
                ExitCode::FAILURE
            }
        }
    }

    // A child host that spawns its own children used to pass its CHORD2_LOCK on, so they failed with "locked".
    async fn no_inherited_lock() {
        let name = format!("chord2-test-{}-inherited", std::process::id());
        let held = process_lock(&name).await.unwrap();
        let mut parent = tokio::process::Command::new(program());
        parent.env(PARENT, "1").env(LOCK_ENV, &name).env_remove(IPC_ENV);
        assert!(parent.status().await.unwrap().success());
        held.release();
    }

    struct Watched {
        base: Arc<dyn Driver>,
        pids: Arc<Mutex<Vec<u32>>>,
    }

    impl Driver for Watched {
        fn spawn(&self) -> ChordResult<Arc<dyn Instance>> {
            // Exit before replace: every earlier child is gone when the next one spawns.
            for pid in self.pids.lock().unwrap().iter() {
                assert!(!alive(*pid), "pid {pid} still alive");
            }
            let instance = self.base.spawn()?;
            self.pids.lock().unwrap().push(instance.pid());
            Ok(instance)
        }
    }

    async fn lock_and_replace() {
        let lock = format!("chord2-test-{}-lock", std::process::id());
        let pids = Arc::default();
        let first = manager(Arc::new(Watched { base: driver(&[], Some(&lock)), pids }));
        let second = manager(driver(&[], Some(&lock)));
        let a = first.start().await.unwrap();
        // Both managers race for the same lock; only the kernel decides, so the second child cannot start.
        let error = second.start().await.err().expect("locked");
        assert_eq!((error.code(), error.reason()), (Some(ChordErrorCode::Unavailable), Some("locked")), "{error}");
        let b = first.replace().await.unwrap();
        assert_ne!(a.pid(), b.pid());
        first.stop().await;
        second.start().await.unwrap();
        second.stop().await;
    }

    async fn grandchild_of(manager: &HostManager, file: &PathBuf) -> u32 {
        manager.start().await.unwrap();
        let grandchild: u32 = std::fs::read_to_string(file).unwrap().trim().parse().unwrap();
        assert!(alive(grandchild));
        grandchild
    }

    async fn graceful_exit_kills_tree() {
        let file = scratch("grandchild-graceful");
        let _ = std::fs::remove_file(&file);
        let manager = manager(driver(&[("GRANDCHILD_FILE", file.to_str().unwrap())], None));
        let grandchild = grandchild_of(&manager, &file).await;
        manager.stop().await;
        until("the grandchild's end", || !alive(grandchild)).await;
        let _ = std::fs::remove_file(&file);
    }

    async fn grace_kills_tree() {
        let file = scratch("grandchild");
        let _ = std::fs::remove_file(&file);
        let env = [("FIXTURE_MODE", "ignore-shutdown"), ("GRANDCHILD_FILE", file.to_str().unwrap())];
        let mut options = HostManagerOptions::new(driver(&env, None));
        options.grace = Duration::from_millis(300);
        let manager = HostManager::new(options);
        let grandchild = grandchild_of(&manager, &file).await;
        let instance = manager.instance().unwrap();
        manager.stop().await;
        assert_eq!(instance.exited().await.signal.as_deref(), Some("SIGKILL"));
        until("the grandchild's end", || !alive(grandchild)).await;
        let _ = std::fs::remove_file(&file);
    }

    async fn fatal_child_fails_start() {
        let manager = manager(driver(&[("FIXTURE_MODE", "fatal")], None));
        let error = manager.start().await.err().expect("fatal");
        assert!(error.to_string().contains("fixture refuses to start"), "{error}");
        assert_eq!(error.code(), Some(ChordErrorCode::Unavailable));
        assert_eq!(manager.status(), Status::Stopped);

        let manager = self::manager(driver(&[("FIXTURE_MODE", "exit")], None));
        let error = manager.start().await.err().expect("exit");
        assert!(error.to_string().contains("child exited before ready (3)"), "{error}");
        assert_eq!(manager.status(), Status::Stopped);
    }

    async fn keep_alive_stops_idle_child() {
        let exits: Arc<Mutex<Vec<Exit>>> = Arc::default();
        let sink = exits.clone();
        let mut options = HostManagerOptions::new(driver(&[], None));
        options.keep_alive =
            Some(Arc::new(|liveness| !liveness.booting && liveness.idle_for < Duration::from_millis(150)));
        options.check_interval = Duration::from_millis(50);
        options.on_exit = Some(Arc::new(move |exit| sink.lock().unwrap().push(exit.clone())));
        let manager = HostManager::new(options);
        manager.start().await.unwrap();
        until("the stop", || manager.status() == Status::Stopped && exits.lock().unwrap().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(exits.lock().unwrap().len(), 1);
    }

    async fn taps_output_and_messages() {
        let output: Arc<Mutex<Vec<u8>>> = Arc::default();
        let messages: Arc<Mutex<Vec<Json>>> = Arc::default();
        let mut options = options(&[("FIXTURE_SAY", "worker says hello")], None);
        let (out, seen) = (output.clone(), messages.clone());
        options.on_output = Some(Arc::new(move |stream, chunk| {
            if stream == OutputStream::Stdout {
                out.lock().unwrap().extend_from_slice(chunk);
            }
        }));
        options.on_message = Some(Arc::new(move |message| seen.lock().unwrap().push(message.clone())));
        let manager = manager(Arc::new(ForkDriver::new(options)));
        manager.start().await.unwrap();
        let said = || String::from_utf8_lossy(&output.lock().unwrap()).contains("worker says hello");
        until("the child's output", said).await;
        let ready = json!({"chord2control": {"type": "ready"}});
        assert!(messages.lock().unwrap().contains(&ready));
        manager.stop().await;
    }
}
