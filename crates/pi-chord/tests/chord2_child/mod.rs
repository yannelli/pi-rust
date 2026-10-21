//! The child host of the manager tests (upstream `test/fixtures/worker.ts`). FIXTURE_MODE: normal |
//! ignore-shutdown | fatal | exit. Until hosts land it answers `call` frames itself: `echo` greets with the
//! init's greeting and its pid, `relay` returns its (packed) arguments as they came.

use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use pi_chord::host2::link::{Link, Received, ipc_link, unpack};
use pi_chord::host2::manager::IPC_ENV;
use pi_chord::host2::wire::{Call, CallResult, Frame, Payload};
use pi_chord::host2::{ChildControl, IpcLink};
use pi_chord::{ChordError, Json};
use tokio::sync::Notify;

/// Set for the grandchild: it only sleeps until it is killed.
pub const GRANDCHILD: &str = "CHORD2_FIXTURE_GRANDCHILD";
/// Set for the parent process of the inherited-lock test.
pub const PARENT: &str = "CHORD2_FIXTURE_PARENT";

pub fn grandchild() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub async fn worker() -> ExitCode {
    let mode = std::env::var("FIXTURE_MODE").unwrap_or_else(|_| "normal".into());
    let control = ChildControl::from_env().expect("parent channel");
    match mode.as_str() {
        "fatal" => {
            control.fatal(&ChordError::error("fixture refuses to start"));
            return linger(&control, 1).await;
        }
        "exit" => return ExitCode::from(3),
        _ => {}
    }
    // Before anything that owns data: the kernel lock (if the manager configured one).
    if control.lock().await.is_err() {
        return linger(&control, 1).await;
    }
    let link = ipc_link(control.channel());
    let init = control.init().await.expect("init");
    let greeting = init.get("greeting").and_then(Json::as_str).unwrap_or_default().to_string();
    serve(&link, greeting);
    if let Ok(file) = std::env::var("GRANDCHILD_FILE") {
        let program = std::env::current_exe().expect("current exe");
        let mut command = std::process::Command::new(program);
        command.env(GRANDCHILD, "1").env_remove(IPC_ENV);
        let child = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        std::fs::write(file, child.expect("grandchild").id().to_string()).expect("grandchild file");
    }
    if let Ok(say) = std::env::var("FIXTURE_SAY") {
        println!("{say}");
    }
    let stop = Arc::new(Notify::new());
    let ignore = mode == "ignore-shutdown";
    let notify = stop.clone();
    control.on_shutdown(Arc::new(move || {
        let notify = notify.clone();
        Box::pin(async move {
            if !ignore {
                notify.notify_one();
            }
        })
    }));
    control.ready();
    stop.notified().await;
    link.close(None);
    control.flush().await;
    ExitCode::SUCCESS
}

/// Lets the parent read what was sent before the process exits.
async fn linger(control: &ChildControl, code: u8) -> ExitCode {
    control.flush().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    ExitCode::from(code)
}

fn serve(link: &Arc<IpcLink>, greeting: String) {
    let weak = Arc::downgrade(link);
    let pid = std::process::id();
    link.receive(Arc::new(move |received| {
        let (Some(link), Received::Frame(Frame::Call(call))) = (weak.upgrade(), received) else { return };
        let Call { id, member, args, .. } = call;
        let value = match member.as_str() {
            "echo" => {
                let text = args.as_ref().map(|args| unpack(args).expect("args")).unwrap_or(Json::Null);
                Payload::Value(Json::from(format!("{greeting} {} from {pid}", text.as_str().unwrap_or_default())))
            }
            "relay" => args.unwrap_or(Payload::Value(Json::Null)),
            _ => Payload::Value(Json::Null),
        };
        link.send(&Frame::Result(CallResult { id, value: Some(value), error: None }));
    }));
}
