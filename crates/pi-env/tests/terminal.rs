//! Upstream `test/terminal.test.ts`: programs in the daemon's pseudo-terminals. Terminals are Unix only.
#![cfg(unix)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_env::{Connection, RemoteError, RemoteTerminal, TerminalOptions, open_terminal};
use support::Guard;
use tokio::sync::Notify;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(20);

fn connect() -> Guard {
    let dir = tempfile::Builder::new().prefix("pi-env-terminal-").tempdir().expect("temp dir");
    Guard { dir, connection: support::connection() }
}

fn cwd(guard: &Guard) -> String {
    guard.dir.path().to_string_lossy().into_owned()
}

/// Collects output and waits for text to appear in it.
#[derive(Clone, Default)]
struct Collector {
    chunks: Arc<Mutex<Vec<u8>>>,
    changed: Arc<Notify>,
}

impl Collector {
    fn on_data(&self) -> pi_env::DataFn {
        let (chunks, changed) = (self.chunks.clone(), self.changed.clone());
        Arc::new(move |bytes: &[u8]| {
            chunks.lock().unwrap().extend_from_slice(bytes);
            changed.notify_waiters();
        })
    }

    fn bytes(&self) -> Vec<u8> {
        self.chunks.lock().unwrap().clone()
    }

    fn contains(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.bytes()).contains(text)
    }

    async fn wait_for(&self, text: &str) {
        let waiting = async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.contains(text) {
                    return;
                }
                changed.await;
            }
        };
        timeout(LIMIT, waiting).await.unwrap_or_else(|_| {
            panic!("timed out waiting for {text:?} in {:?}", String::from_utf8_lossy(&self.bytes()))
        });
    }
}

async fn exit_code(terminal: &RemoteTerminal) -> Result<i64, RemoteError> {
    timeout(LIMIT, terminal.exited()).await.expect("the program exits")
}

fn argv(parts: &[&str]) -> Option<Vec<String>> {
    Some(parts.iter().map(|part| part.to_string()).collect())
}

async fn open(
    guard: &Guard,
    program: Option<Vec<String>>,
    on_data: pi_env::DataFn,
) -> Result<RemoteTerminal, RemoteError> {
    let mut options = TerminalOptions::new(cwd(guard), 80, 24, on_data);
    options.argv = program;
    timeout(LIMIT, open_terminal(&guard.connection, options)).await.expect("the terminal opens")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_a_program_in_a_terminal_with_input_resizing_and_its_exit_code() {
    let guard = connect();
    let output = Collector::default();
    let mut options = TerminalOptions::new(cwd(&guard), 80, 24, output.on_data());
    options.argv = argv(&["/bin/sh"]);
    options.env = Some([("PS1".into(), "$ ".into()), ("TERM".into(), "xterm-256color".into())].into());
    let terminal = timeout(LIMIT, open_terminal(&guard.connection, options)).await.expect("opens").expect("terminal");
    assert!(terminal.pid > 0);
    terminal.write("test -t 0 && echo IS-A-TTY; stty size; echo $TERM\n");
    output.wait_for("IS-A-TTY").await;
    output.wait_for("24 80").await;
    output.wait_for("xterm-256color").await;
    terminal.resize(100, 30);
    terminal.write("stty size\n");
    output.wait_for("30 100").await;
    terminal.write("exit 3\n");
    assert_eq!(exit_code(&terminal).await.expect("exit code"), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_the_job_in_the_foreground_not_the_shell_at_its_prompt() {
    let guard = connect();
    let output = Collector::default();
    // Interactive, for job control: the shell puts each job in front of the terminal.
    let mut options = TerminalOptions::new(cwd(&guard), 80, 24, output.on_data());
    options.argv = argv(&["/bin/sh", "-i"]);
    options.env = Some([("PS1".into(), "$ ".into()), ("TERM".into(), "xterm-256color".into())].into());
    let terminal = timeout(LIMIT, open_terminal(&guard.connection, options)).await.expect("opens").expect("terminal");
    terminal.write("echo READY\n");
    output.wait_for("READY").await;
    assert_eq!(terminal.foreground().await.expect("foreground"), None);
    terminal.write("sleep 100\n");
    // The job is in front before it execs, so its name reads "sh" for a moment.
    let named = cfg!(any(target_os = "linux", target_os = "macos"));
    let ready = |job: &Option<pi_env::TerminalForeground>| {
        job.as_ref().is_some_and(|job| !named || job.name.as_deref() == Some("sleep"))
    };
    let mut foreground = terminal.foreground().await.expect("foreground");
    for _ in 0..100 {
        if ready(&foreground) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        foreground = terminal.foreground().await.expect("foreground");
    }
    let foreground = foreground.expect("a job in front");
    assert!(foreground.pid > 0 && foreground.pid != terminal.pid);
    if named {
        assert_eq!(foreground.name.as_deref(), Some("sleep"));
    }
    terminal.close();
    assert_eq!(exit_code(&terminal).await.expect_err("aborted").code, "aborted");
    assert_eq!(terminal.foreground().await.expect_err("ended").code, "EBADF");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivers_raw_bytes_not_decoded_text() {
    let guard = connect();
    let output = Collector::default();
    let terminal = open(&guard, argv(&["sh", "-c", "printf 'a\\377b'"]), output.on_data()).await.expect("terminal");
    assert_eq!(exit_code(&terminal).await.expect("exit code"), 0);
    assert_eq!(output.bytes(), [0x61, 0xff, 0x62]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_input_in_order() {
    let guard = connect();
    let on_data: pi_env::DataFn = Arc::new(|_| {});
    let terminal = open(&guard, argv(&["sh", "-c", "head -c 40000 > input.txt"]), on_data).await.expect("terminal");
    let mut text = String::new();
    for index in 0..4000 {
        let piece = format!("{index:09}\n");
        text.push_str(&piece);
        terminal.write(&piece);
    }
    assert_eq!(exit_code(&terminal).await.expect("exit code"), 0);
    assert_eq!(std::fs::read_to_string(guard.dir.path().join("input.txt")).expect("input.txt"), text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kills_the_program_on_close() {
    let guard = connect();
    let terminal = open(&guard, argv(&["sleep", "100"]), Arc::new(|_| {})).await.expect("terminal");
    terminal.close();
    let error = exit_code(&terminal).await.expect_err("aborted");
    assert_eq!(error.code, "aborted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fails_to_open_in_a_missing_directory() {
    let guard = connect();
    let missing = guard.dir.path().join("missing").to_string_lossy().into_owned();
    let options = TerminalOptions::new(missing, 80, 24, Arc::new(|_| {}));
    let error = timeout(LIMIT, open_terminal(&guard.connection, options)).await.expect("settles").err();
    assert_eq!(error.map(|error| error.code), Some("spawn_error".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_sizes_outside_the_accepted_range() {
    let guard = connect();
    let options = TerminalOptions::new(cwd(&guard), 0, 24, Arc::new(|_| {}));
    let error = timeout(LIMIT, open_terminal(&guard.connection, options)).await.expect("settles").err();
    assert_eq!(error.map(|error| error.code), Some("EINVAL".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_writes_to_a_terminal_that_does_not_exist() {
    let guard = connect();
    let mut json = pi_env::Json::new();
    json.insert("target".into(), 999.into());
    let connection: &Connection = &guard.connection;
    let error = timeout(LIMIT, connection.request("ptyWrite", json, Default::default())).await.expect("settles");
    assert_eq!(error.err().map(|error| error.code), Some("EBADF".to_string()));
}
