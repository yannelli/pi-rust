//! Upstream `test/differential.test.ts`: `RemoteExecutionEnv` against the local env (upstream `NodeExecutionEnv`) on
//! the same machine, for random operation sequences, special files, odd names, and the read and bash tools.

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::Context;
use pi_durable::DurableResult;
use pi_durable::env::{
    FileInfo, FileResult, LineRange, LocalEnvOptions, LocalExecutionEnv, RemoveOptions, SharedEnv, ShellOutputSkip,
    ShellOutputWindow,
};
use pi_durable::harness::ToolDiagnostic;
use pi_durable::tools::{
    BashToolOptions, ImageModel, ReadToolOptions, ToolCallApi, execute_bash, execute_read, image_model_of,
};
use pi_env::{Connection, RemoteExecutionEnv, RemoteExecutionEnvOptions};
use serde_json::{Value, json};
use support::ctx;

struct Pair {
    local: SharedEnv,
    remote: SharedEnv,
    roots: [tempfile::TempDir; 2],
}

fn pair(connection: &Connection) -> Pair {
    let local_root = tempfile::Builder::new().prefix("pi-env-local-").tempdir().unwrap();
    let remote_root = tempfile::Builder::new().prefix("pi-env-remote-").tempdir().unwrap();
    let path = |dir: &tempfile::TempDir| dir.path().to_string_lossy().into_owned();
    let local: SharedEnv = Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(path(&local_root))));
    let options = RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:test", path(&remote_root));
    let remote: SharedEnv = Arc::new(RemoteExecutionEnv::new(options));
    Pair { local, remote, roots: [local_root, remote_root] }
}

/// mulberry32, as upstream seeds its sequences.
fn random(seed: u32) -> impl FnMut() -> f64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x6d2b79f5);
        let mut t = state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        ((t ^ (t >> 14)) as f64) / 4_294_967_296.0
    }
}

fn result<T>(value: FileResult<T>, convert: impl FnOnce(T) -> Value) -> Value {
    match value {
        Ok(value) => json!({ "ok": true, "value": convert(value) }),
        Err(error) => json!({ "ok": false, "code": error.code.as_str() }),
    }
}

fn info(info: &FileInfo) -> Value {
    json!({ "name": info.name, "path": info.path, "kind": info.kind, "size": info.size, "mtimeMs": "mtime" })
}

fn bytes(bytes: &[u8]) -> Value {
    json!({ "bytes": bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>() })
}

/// A result with the environment's root replaced, for comparison.
fn normalize(value: &Value, root: &tempfile::TempDir) -> String {
    let text = value.to_string();
    let root = root.path().to_string_lossy().into_owned();
    let real = std::fs::canonicalize(&root).map(|path| path.to_string_lossy().into_owned()).unwrap_or(root.clone());
    // Windows canonical paths carry a `\\?\` prefix that canonical_path results do not.
    let real = real.strip_prefix(r"\\?\").map_or(real.clone(), str::to_owned);
    let escaped = |path: &str| json!(path).to_string().trim_matches('"').to_string();
    text.replace(&escaped(&real), "<root>").replace(&escaped(&root), "<root>")
}

const NAMES: [&str; 8] = ["a.txt", "b", "dir", "dir/c.txt", "dir/sub", "dir/sub/d.bin", "missing", "a.txt/x"];

fn contents() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        b"hello\n".to_vec(),
        "\u{feff}bom\r\nline".as_bytes().to_vec(),
        vec![0xe2, 0x82, 0x0a, 0xff, 0xef, 0xbb, 0xbf],
        "x".repeat(70_000).into_bytes(),
        // Crosses write chunks, read chunks and multi-byte characters split between them.
        "é".repeat(300_001).into_bytes(),
    ]
}

#[derive(Debug, Clone)]
enum Operation {
    WriteFile(&'static str, usize),
    AppendFile(&'static str, usize),
    ReadTextFile(&'static str),
    ReadBinaryFile(&'static str),
    ReadTextLines(&'static str, usize),
    FileInfo(&'static str),
    Exists(&'static str),
    ListDir(&'static str),
    CreateDir(&'static str, bool),
    Remove(&'static str, bool, bool),
    RenameFile(&'static str, &'static str),
    TruncateFile(&'static str, u64),
    FlushFile(&'static str),
    CanonicalPath(&'static str),
    OpenBinaryReader(&'static str, bool, u64, u64),
}

fn random_operation(next: &mut impl FnMut() -> f64) -> Operation {
    let index = |next: &mut dyn FnMut() -> f64, len: usize| (next() * len as f64) as usize;
    let name = |next: &mut dyn FnMut() -> f64| NAMES[index(next, NAMES.len())];
    let content = |next: &mut dyn FnMut() -> f64| index(next, 6);
    let flag = |next: &mut dyn FnMut() -> f64| next() < 0.5;
    let small = |next: &mut dyn FnMut() -> f64, limit: usize| index(next, limit) as u64;
    match index(next, 15) {
        0 => Operation::WriteFile(name(next), content(next)),
        1 => Operation::AppendFile(name(next), content(next)),
        2 => Operation::ReadTextFile(name(next)),
        3 => Operation::ReadBinaryFile(name(next)),
        4 => Operation::ReadTextLines(name(next), small(next, 4) as usize),
        5 => Operation::FileInfo(name(next)),
        6 => Operation::Exists(name(next)),
        7 => Operation::ListDir(name(next)),
        8 => Operation::CreateDir(name(next), flag(next)),
        9 => Operation::Remove(name(next), flag(next), flag(next)),
        10 => Operation::RenameFile(name(next), name(next)),
        11 => Operation::TruncateFile(name(next), small(next, 10)),
        12 => Operation::FlushFile(name(next)),
        13 => Operation::CanonicalPath(name(next)),
        _ => Operation::OpenBinaryReader(name(next), flag(next), small(next, 8), small(next, 8)),
    }
}

async fn run(env: &SharedEnv, operation: &Operation, contents: &[Vec<u8>]) -> Value {
    let c = ctx();
    match *operation {
        Operation::WriteFile(n, i) => result(env.write_file(n, &contents[i], &c).await, |()| Value::Null),
        Operation::AppendFile(n, i) => result(env.append_file(n, &contents[i], &c).await, |()| Value::Null),
        Operation::ReadTextFile(n) => result(env.read_text_file(n, &c).await, Value::from),
        Operation::ReadBinaryFile(n) => result(env.read_binary_file(n, &c).await, |b| bytes(&b)),
        Operation::ReadTextLines(n, m) => result(env.read_text_lines(n, Some(m), &c).await, |lines| json!(lines)),
        Operation::FileInfo(n) => result(env.file_info(n, &c).await, |i| info(&i)),
        Operation::Exists(n) => result(env.exists(n, &c).await, Value::from),
        Operation::ListDir(n) => result(env.list_dir(n, &c).await, |list| list.iter().map(info).collect()),
        Operation::CreateDir(n, r) => result(env.create_dir(n, r, &c).await, |()| Value::Null),
        Operation::Remove(n, recursive, force) => {
            result(env.remove(n, RemoveOptions { recursive, force }, &c).await, |()| Value::Null)
        }
        Operation::RenameFile(a, b) => result(env.rename_file(a, b, &c).await, |()| Value::Null),
        Operation::TruncateFile(n, size) => result(env.truncate_file(n, size, &c).await, |()| Value::Null),
        Operation::FlushFile(n) => result(env.flush_file(n, &c).await, |()| Value::Null),
        Operation::CanonicalPath(n) => result(env.canonical_path(n, &c).await, Value::from),
        Operation::OpenBinaryReader(n, no_follow, offset, length) => {
            let reader = match env.open_binary_reader(n, no_follow, &c).await {
                Ok(reader) => reader,
                Err(error) => return result::<()>(Err(error), |()| Value::Null),
            };
            let read = result(reader.read(offset, length, &c).await, |b| bytes(&b));
            let scan = result(reader.scan_lines(LineRange::default(), &c).await, |scan| json!(scan));
            reader.close(&c).await;
            json!([read, scan])
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gives_the_same_results_for_random_file_operation_sequences() {
    let connection = support::connection();
    let contents = contents();
    for seed in 1..=60 {
        let mut next = random(seed);
        let pair = pair(&connection);
        for step in 0..25 {
            let operation = random_operation(&mut next);
            let expected = normalize(&run(&pair.local, &operation, &contents).await, &pair.roots[0]);
            let actual = normalize(&run(&pair.remote, &operation, &contents).await, &pair.roots[1]);
            assert_eq!(actual, expected, "seed {seed} step {step} {operation:?}");
        }
    }
    connection.close();
}

/// FIFOs and `/dev/null` exist on Unix only.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_devices_fifos_and_directories_like_node() {
    async fn results(env: &SharedEnv, root: &tempfile::TempDir) -> String {
        let c = ctx();
        let fifo = root.path().join("fifo").to_string_lossy().into_owned();
        let argv = pi_durable::env::ShellCommand::Argv(vec!["mkfifo".into(), fifo.clone()]);
        let made = env.exec(argv, Default::default(), &c).await.expect("mkfifo");
        assert_eq!(made.exit_code, 0, "mkfifo failed");
        env.create_dir("dir", true, &c).await.expect("dir");
        // A writer for each blocking open of the FIFO.
        let write = |text: &str| {
            let script = "printf '%s' \"$1\" > \"$2\"";
            std::process::Command::new("sh").args(["-c", script, "sh", text, &fifo]).spawn().expect("writer")
        };
        let mut writers = vec![write("fifo text\n")];
        let fifo_text = result(env.read_text_file("fifo", &c).await, Value::from);
        writers.push(write("fifo bytes"));
        let fifo_bytes = result(env.read_binary_file("fifo", &c).await, |b| bytes(&b));
        writers.push(write("x"));
        let fifo_lines = result(env.read_text_lines("fifo", None, &c).await, |lines| json!(lines));
        for mut writer in writers {
            let _ = writer.wait();
        }
        let dir_reader = env.open_text_line_reader("dir", &c).await;
        let (opened, dir_line) = match &dir_reader {
            Ok(reader) => {
                let line = reader.read_line(&c).await;
                (true, result(line, |line| json!(line.map(|line| (line.text, line.terminated)))))
            }
            Err(_) => (false, Value::Null),
        };
        if let Ok(reader) = dir_reader {
            reader.close(&c).await;
        }
        let value = json!({
            "nullText": result(env.read_text_file("/dev/null", &c).await, Value::from),
            "nullBytes": result(env.read_binary_file("/dev/null", &c).await, |b| bytes(&b)),
            "nullLines": result(env.read_text_lines("/dev/null", None, &c).await, |lines| json!(lines)),
            "fifoText": fifo_text,
            "fifoBytes": fifo_bytes,
            "fifoLines": fifo_lines,
            "dirReaderOpened": opened,
            "dirLine": dir_line,
            "dirText": result(env.read_text_file("dir", &c).await, Value::from),
            "dirBytes": result(env.read_binary_file("dir", &c).await, |b| bytes(&b)),
        });
        normalize(&value, root)
    }
    let connection = support::connection();
    let pair = pair(&connection);
    assert_eq!(results(&pair.remote, &pair.roots[1]).await, results(&pair.local, &pair.roots[0]).await);
    connection.close();
}

/// macOS file systems refuse names that are not UTF-8.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_names_that_are_not_utf8_like_node() {
    use std::os::unix::ffi::OsStrExt;
    let connection = support::connection();
    let pair = pair(&connection);
    for root in &pair.roots {
        for name in [&[0x62][..], &[0x61, 0xff], &[0xc3, 0xa9], &[0x61, 0xfe, 0x7a], &[0x7a]] {
            std::fs::write(root.path().join(std::ffi::OsStr::from_bytes(name)), "x").unwrap();
        }
    }
    async fn results(env: &SharedEnv, root: &tempfile::TempDir) -> String {
        let c = ctx();
        let mut pages = Vec::new();
        if let Ok(reader) = env.open_dir_reader(".", &c).await {
            loop {
                let page = reader.next(2, &c).await;
                let done = page.as_ref().map_or(true, |page| page.done);
                pages.push(result(
                    page,
                    |page| json!({ "entries": page.entries.iter().map(info).collect::<Vec<_>>(), "done": page.done }),
                ));
                if done {
                    break;
                }
            }
            reader.close(&c).await;
        }
        let list = result(env.list_dir(".", &c).await, |list| list.iter().map(info).collect());
        normalize(&json!({ "list": list, "pages": pages }), root)
    }
    assert_eq!(results(&pair.remote, &pair.roots[1]).await, results(&pair.local, &pair.roots[0]).await);
    connection.close();
}

/// A tool call api without a Harness (upstream's fake `ToolExecutionApi`).
struct FakeApi {
    env: SharedEnv,
    output: Mutex<String>,
}

#[async_trait]
impl ToolCallApi for FakeApi {
    fn env(&self) -> Option<SharedEnv> {
        Some(self.env.clone())
    }

    fn output_window(&self) -> Option<ShellOutputWindow> {
        None
    }

    fn output(&self, chunk: &str, _skipped: Option<ShellOutputSkip>) -> DurableResult<()> {
        self.output.lock().unwrap().push_str(chunk);
        Ok(())
    }

    fn retained_output(&self) -> DurableResult<(String, bool)> {
        Ok((self.output.lock().unwrap().clone(), false))
    }

    fn diagnostic(&self, _diagnostic: ToolDiagnostic) -> DurableResult<()> {
        Ok(())
    }

    async fn image_model(&self, _context: &Context) -> DurableResult<ImageModel> {
        Ok(image_model_of(None))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gives_the_same_read_and_bash_tool_results() {
    let connection = support::connection();
    let pair = pair(&connection);
    let files: [(&str, Vec<u8>); 5] = [
        ("text.txt", b"one\ntwo\n\nthree".to_vec()),
        ("big.txt", "line\n".repeat(3000).into_bytes()),
        ("long.txt", format!("{}\nend", "é".repeat(40_000)).into_bytes()),
        ("bom.txt", "\u{feff}hello".as_bytes().to_vec()),
        ("bad.txt", vec![0x61, 0xe2, 0x82, 0x0a, 0x62]),
    ];
    for (path, content) in &files {
        pair.local.write_file(path, content, &ctx()).await.unwrap();
        pair.remote.write_file(path, content, &ctx()).await.unwrap();
    }
    async fn call(env: &SharedEnv, bash: bool, args: &Value, root: &tempfile::TempDir) -> String {
        let api = Arc::new(FakeApi { env: env.clone(), output: Mutex::default() });
        let args = args.as_object().cloned().unwrap();
        let result = if bash {
            execute_bash(&BashToolOptions::default(), args, api.clone(), ctx()).await
        } else {
            execute_read(&ReadToolOptions::default(), args, api.clone(), ctx()).await
        };
        // Only whether the tool failed: the local env reports Rust's io messages, the daemon Node's.
        let result = match result {
            Ok(result) => format!("{result:?}"),
            Err(_) => "error".to_string(),
        };
        let output = api.output.lock().unwrap().clone();
        normalize(&json!({ "result": result, "output": output }), root)
    }
    let cases = [
        (false, json!({ "path": "text.txt" })),
        (false, json!({ "path": "text.txt", "offset": 2, "limit": 1 })),
        (false, json!({ "path": "big.txt" })),
        (false, json!({ "path": "long.txt" })),
        (false, json!({ "path": "bom.txt" })),
        (false, json!({ "path": "bad.txt" })),
        (false, json!({ "path": "missing.txt" })),
        (true, json!({ "command": "cat text.txt; echo err >&2; exit 0" })),
        (true, json!({ "command": "exit 3" })),
        (true, json!({ "command": "printf 'a\\nb'; ls" })),
    ];
    for (bash, args) in cases {
        let expected = call(&pair.local, bash, &args, &pair.roots[0]).await;
        let actual = call(&pair.remote, bash, &args, &pair.roots[1]).await;
        assert_eq!(actual, expected, "{args}");
    }
    connection.close();
}
