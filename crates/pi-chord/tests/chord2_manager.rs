//! The host manager (upstream `manager.ts`) with a scripted driver: readiness, failed starts, stops, the grace
//! period, keep-alive and the idle clock ("idleFor counts from the busy → idle transition" in
//! `test/manager.test.ts`). Control message forms and [`ChildControl`] run on a recording channel.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::host2::link::{DisconnectListener, IpcChannel, ListenerId, MessageListener, SendCallback};
use pi_chord::host2::manager::{ControlListener, StatusReport};
use pi_chord::host2::{
    ChildControl, ChordErrorCode, ControlMessage, Driver, Exit, HostManager, HostManagerOptions, Instance, Link,
    Status, memory_link_pair,
};
use pi_chord::{BoxFuture, ChordError, ChordResult, Json, json};
use tokio::sync::watch;

/// A child the test scripts: it reports what the test emits and exits when told to.
struct Fake {
    pid: u32,
    link: Arc<dyn Link>,
    listeners: Mutex<Vec<ControlListener>>,
    sent: Mutex<Vec<ControlMessage>>,
    exit: watch::Sender<Option<Exit>>,
    killed: AtomicBool,
    /// Exits with code 0 on `shutdown`.
    graceful: bool,
}

impl Fake {
    fn emit(&self, message: ControlMessage) {
        let listeners = self.listeners.lock().unwrap().clone();
        for listener in listeners {
            listener(&message);
        }
    }

    fn end(&self, code: Option<i32>, signal: Option<&str>) {
        self.exit.send_replace(Some(Exit { code, signal: signal.map(String::from) }));
    }

    fn sent(&self) -> Vec<ControlMessage> {
        self.sent.lock().unwrap().clone()
    }
}

impl Instance for Fake {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn link(&self) -> Arc<dyn Link> {
        self.link.clone()
    }

    fn exited(&self) -> BoxFuture<Exit> {
        let mut exit = self.exit.subscribe();
        Box::pin(async move { exit.wait_for(Option::is_some).await.unwrap().clone().unwrap() })
    }

    fn send(&self, message: &ControlMessage) {
        self.sent.lock().unwrap().push(message.clone());
        if *message == ControlMessage::Shutdown && self.graceful {
            self.end(Some(0), None);
        }
    }

    fn kill_tree(&self) {
        self.killed.store(true, Ordering::SeqCst);
        self.end(None, Some("SIGKILL"));
    }

    fn on_control(&self, listener: ControlListener) {
        self.listeners.lock().unwrap().push(listener);
    }
}

#[derive(Default)]
struct Script {
    spawned: Mutex<Vec<Arc<Fake>>>,
    /// Children say `ready` as soon as the manager listens.
    auto_ready: bool,
    graceful: bool,
    fail: bool,
}

impl Script {
    fn child(&self, index: usize) -> Arc<Fake> {
        self.spawned.lock().unwrap()[index].clone()
    }

    fn count(&self) -> usize {
        self.spawned.lock().unwrap().len()
    }
}

struct ScriptDriver(Arc<Script>);

impl Driver for ScriptDriver {
    fn spawn(&self) -> ChordResult<Arc<dyn Instance>> {
        let script = &self.0;
        if script.fail {
            return Err(ChordError::error("spawn failed"));
        }
        let fake = Arc::new(Fake {
            pid: 100 + script.count() as u32,
            link: memory_link_pair().0,
            listeners: Mutex::default(),
            sent: Mutex::default(),
            exit: watch::Sender::new(None),
            killed: AtomicBool::new(false),
            graceful: script.graceful,
        });
        if script.auto_ready {
            let ready = fake.clone();
            tokio::spawn(async move {
                until(|| !ready.listeners.lock().unwrap().is_empty()).await;
                ready.emit(ControlMessage::Ready);
            });
        }
        script.spawned.lock().unwrap().push(fake.clone());
        Ok(fake)
    }
}

fn scripted(script: Script, configure: impl FnOnce(&mut HostManagerOptions)) -> (HostManager, Arc<Script>) {
    let script = Arc::new(script);
    let mut options = HostManagerOptions::new(Arc::new(ScriptDriver(script.clone())));
    configure(&mut options);
    (HostManager::new(options), script)
}

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("condition not met");
}

#[tokio::test]
async fn idle_for_counts_from_the_busy_to_idle_transition() {
    let (manager, script) = scripted(Script::default(), |_| {});
    let started = tokio::spawn({
        let manager = manager.clone();
        async move { manager.start().await.map(|_| ()) }
    });
    until(|| script.count() == 1 && !script.child(0).listeners.lock().unwrap().is_empty()).await;
    assert_eq!(manager.status(), Status::Starting);
    assert!(manager.liveness().booting);
    let child = script.child(0);
    child.emit(ControlMessage::Ready);
    started.await.unwrap().unwrap();
    assert!(!manager.liveness().booting);
    child.emit(ControlMessage::Status(StatusReport { active_calls: Some(1), ..StatusReport::default() }));
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(manager.liveness().idle_for, Duration::ZERO);
    assert_eq!(manager.liveness().active_calls, 1);
    child.emit(ControlMessage::Status(StatusReport { active_calls: Some(0), ..StatusReport::default() }));
    let idle_for = manager.liveness().idle_for;
    assert!(idle_for < Duration::from_millis(60), "idle_for {idle_for:?}");
}

#[tokio::test]
async fn start_returns_the_running_child_and_stop_shuts_it_down() {
    let exits = Arc::new(Mutex::new(Vec::new()));
    let sink = exits.clone();
    let script = Script { auto_ready: true, graceful: true, ..Script::default() };
    let (manager, script) = scripted(script, |options| {
        options.on_exit = Some(Arc::new(move |exit| sink.lock().unwrap().push(exit.clone())));
    });
    assert_eq!(manager.status(), Status::Stopped);
    let first = manager.start().await.unwrap();
    assert_eq!(manager.status(), Status::Running);
    assert!(Arc::ptr_eq(&manager.start().await.unwrap(), &first));
    assert_eq!(script.count(), 1);
    manager.stop().await;
    assert_eq!(manager.status(), Status::Stopped);
    assert!(manager.instance().is_none());
    let child = script.child(0);
    assert_eq!(child.sent(), [ControlMessage::Shutdown]);
    assert!(!child.killed.load(Ordering::SeqCst));
    until(|| exits.lock().unwrap().len() == 1).await;
    assert_eq!(exits.lock().unwrap()[0], Exit { code: Some(0), signal: None });
}

#[tokio::test]
async fn a_child_that_ignores_shutdown_is_killed_after_the_grace_period() {
    let (manager, script) = scripted(Script { auto_ready: true, ..Script::default() }, |options| {
        options.grace = Duration::from_millis(50);
    });
    let instance = manager.start().await.unwrap();
    manager.stop().await;
    assert!(script.child(0).killed.load(Ordering::SeqCst));
    assert_eq!(instance.exited().await.signal.as_deref(), Some("SIGKILL"));
    assert_eq!(manager.status(), Status::Stopped);
}

#[tokio::test]
async fn a_fatal_report_fails_the_start_with_its_reason() {
    let (manager, script) = scripted(Script::default(), |_| {});
    let started = tokio::spawn({
        let manager = manager.clone();
        async move { manager.start().await.map(|_| ()) }
    });
    until(|| script.count() == 1 && !script.child(0).listeners.lock().unwrap().is_empty()).await;
    let message = "unavailable/locked: lock x is held by another process".to_string();
    script.child(0).emit(ControlMessage::Fatal { message, reason: Some("locked".into()) });
    let error = started.await.unwrap().unwrap_err();
    assert_eq!((error.code(), error.reason()), (Some(ChordErrorCode::Unavailable), Some("locked")));
    assert_eq!(
        error.to_string(),
        "unavailable/locked: child failed: unavailable/locked: lock x is held by another process"
    );
    assert!(script.child(0).killed.load(Ordering::SeqCst));
    assert_eq!(manager.status(), Status::Stopped);
    assert!(!manager.liveness().booting);
}

#[tokio::test]
async fn an_exit_or_a_timeout_before_ready_fails_the_start() {
    let (manager, script) = scripted(Script::default(), |options| options.ready_timeout = Duration::from_millis(50));
    let started = tokio::spawn({
        let manager = manager.clone();
        async move { manager.start().await.map(|_| ()) }
    });
    until(|| script.count() == 1).await;
    script.child(0).end(Some(3), None);
    let error = started.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "unavailable: child exited before ready (3)");
    assert_eq!(manager.status(), Status::Stopped);

    let error = manager.start().await.err().expect("not ready");
    assert_eq!(error.to_string(), "unavailable: child not ready in time");
    assert!(script.child(1).killed.load(Ordering::SeqCst));
    assert_eq!(manager.status(), Status::Stopped);
}

#[tokio::test]
async fn a_failed_spawn_leaves_the_manager_stopped() {
    let (manager, _script) = scripted(Script { fail: true, ..Script::default() }, |_| {});
    let error = manager.start().await.err().expect("spawn fails");
    assert_eq!(error.to_string(), "spawn failed");
    assert_eq!(manager.status(), Status::Stopped);
    assert!(!manager.liveness().booting);
}

#[tokio::test]
async fn steps_run_in_call_order_and_replace_stops_before_it_starts() {
    let script = Script { auto_ready: true, graceful: true, ..Script::default() };
    let (manager, script) = scripted(script, |_| {});
    let (first, (), second) = tokio::join!(manager.start(), manager.stop(), manager.start());
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_ne!(first.pid(), second.pid());
    assert_eq!(script.child(0).sent(), [ControlMessage::Shutdown]);
    let replaced = manager.replace().await.unwrap();
    assert_eq!(script.count(), 3);
    assert_eq!(script.child(1).sent(), [ControlMessage::Shutdown]);
    assert!(Arc::ptr_eq(&manager.instance().unwrap(), &replaced));
}

#[tokio::test]
async fn a_child_that_exits_on_its_own_is_forgotten() {
    let exits = Arc::new(AtomicU64::new(0));
    let counter = exits.clone();
    let (manager, script) = scripted(Script { auto_ready: true, ..Script::default() }, |options| {
        options.on_exit = Some(Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
    });
    manager.start().await.unwrap();
    script.child(0).end(None, Some("SIGSEGV"));
    until(|| manager.status() == Status::Stopped && exits.load(Ordering::SeqCst) == 1).await;
    assert!(manager.instance().is_none());
    manager.start().await.unwrap();
    assert_eq!(script.count(), 2);
}

#[tokio::test]
async fn keep_alive_stops_an_idle_child() {
    let script = Script { auto_ready: true, graceful: true, ..Script::default() };
    let (manager, script) = scripted(script, |options| {
        options.keep_alive = Some(Arc::new(|liveness| liveness.idle_for < Duration::from_millis(60)));
        options.check_interval = Duration::from_millis(20);
    });
    manager.start().await.unwrap();
    let child = script.child(0);
    child.emit(ControlMessage::Status(StatusReport { viewers: Some(1), ..StatusReport::default() }));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(manager.status(), Status::Running, "a child with viewers is kept");
    child.emit(ControlMessage::Status(StatusReport::default()));
    until(|| manager.status() == Status::Stopped).await;
    assert_eq!(child.sent(), [ControlMessage::Shutdown]);
}

#[test]
fn control_messages_have_upstreams_fields_and_order() {
    let cases = [
        (ControlMessage::Init { value: json!({"greeting": "hi"}) }, r#"{"type":"init","value":{"greeting":"hi"}}"#),
        (ControlMessage::Ready, r#"{"type":"ready"}"#),
        (
            ControlMessage::Fatal { message: "no".into(), reason: Some("locked".into()) },
            r#"{"type":"fatal","message":"no","reason":"locked"}"#,
        ),
        (ControlMessage::Fatal { message: "no".into(), reason: None }, r#"{"type":"fatal","message":"no"}"#),
        (ControlMessage::Shutdown, r#"{"type":"shutdown"}"#),
        (
            ControlMessage::Status(StatusReport { active_calls: Some(1), viewers: None, durable_tasks: Some(2) }),
            r#"{"type":"status","activeCalls":1,"durableTasks":2}"#,
        ),
    ];
    for (message, text) in cases {
        assert_eq!(message.to_json().to_string(), text);
        assert_eq!(message.wrap().to_string(), format!("{{\"chord2control\":{text}}}"));
        assert_eq!(ControlMessage::unwrap(&message.wrap()), Some(message));
    }
    assert_eq!(ControlMessage::from_json(&json!({"type": "other"})), None);
    assert_eq!(ControlMessage::unwrap(&json!({"chord2": {"t": "ping", "n": 1}})), None);
}

/// Records what is sent and lets the test deliver messages.
#[derive(Default)]
struct Recording {
    sent: Mutex<Vec<Json>>,
    listeners: Mutex<Vec<MessageListener>>,
}

impl Recording {
    fn deliver(&self, message: Json) {
        let listeners = self.listeners.lock().unwrap().clone();
        for listener in listeners {
            listener(&message);
        }
    }
}

impl IpcChannel for Recording {
    fn connected(&self) -> bool {
        true
    }

    fn send(&self, message: Json, _done: Option<SendCallback>) -> bool {
        self.sent.lock().unwrap().push(message);
        true
    }

    fn on_message(&self, listener: MessageListener) -> ListenerId {
        self.listeners.lock().unwrap().push(listener);
        0
    }

    fn on_disconnect(&self, _listener: DisconnectListener) -> ListenerId {
        0
    }

    fn off(&self, _id: ListenerId) {}
}

#[tokio::test]
async fn child_control_speaks_the_control_messages() {
    let channel = Arc::new(Recording::default());
    let control = ChildControl::new(channel.clone());
    channel.deliver(ControlMessage::Init { value: json!({"greeting": "hi"}) }.wrap());
    channel.deliver(ControlMessage::Init { value: json!("second") }.wrap());
    assert_eq!(control.init().await.unwrap(), json!({"greeting": "hi"}));
    control.ready();
    control.report(StatusReport { viewers: Some(2), ..StatusReport::default() });
    control.fatal(&ChordError::with_reason(ChordErrorCode::Unavailable, "locked", "lock x is held"));
    control.fatal(&ChordError::error("plain"));
    let sent: Vec<String> = channel.sent.lock().unwrap().iter().map(Json::to_string).collect();
    assert_eq!(
        sent,
        [
            r#"{"chord2control":{"type":"ready"}}"#,
            r#"{"chord2control":{"type":"status","viewers":2}}"#,
            r#"{"chord2control":{"type":"fatal","message":"unavailable/locked: lock x is held","reason":"locked"}}"#,
            r#"{"chord2control":{"type":"fatal","message":"plain"}}"#,
        ]
    );
    let shutdowns = Arc::new(AtomicU64::new(0));
    let counter = shutdowns.clone();
    control.on_shutdown(Arc::new(move || {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    }));
    channel.deliver(ControlMessage::Shutdown.wrap());
    until(|| shutdowns.load(Ordering::SeqCst) == 1).await;
    // No lock name in the environment: lock() does nothing.
    control.lock().await.unwrap();
}

#[tokio::test]
async fn child_control_needs_a_parent_channel() {
    let Err(error) = ChildControl::from_env() else { panic!("no channel was configured") };
    assert_eq!(error.code(), Some(ChordErrorCode::Unavailable));
}
