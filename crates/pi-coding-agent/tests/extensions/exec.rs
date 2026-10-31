//! `pi.exec` maxBuffer validation, combined byte cap, UTF-8 cuts, and child cleanup (upstream c466a3595).
//! Shell cases are Unix only, as upstream skips them on Windows.

use serde_json::{Value, json};

use crate::harness::{Setup, harness};

/// The returned JSON value of a JavaScript body awaited inside an extension command.
async fn value_of(body: &str) -> Value {
    let source = format!(
        r#"export default function (pi) {{
	pi.registerCommand("run", {{ handler: async (_args, ctx) => ctx.ui.notify(JSON.stringify(await (async () => {{ {body} }})())) }});
}}
"#
    );
    let h = harness(Setup { files: vec![("run.ts", source)], load: vec!["run.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let run = h.session.prompt("/run", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(30), run).await.expect("pi.exec did not return").unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let calls = h.ui.calls();
    let [call] = calls.as_slice() else { panic!("{calls:?}") };
    serde_json::from_str(call.strip_prefix("notify(info): ").expect("notify")).unwrap()
}

#[cfg(unix)]
async fn exec(script: &str, max_buffer: &str) -> Value {
    value_of(&format!("return await pi.exec(\"sh\", [\"-c\", {}], {{ maxBuffer: {max_buffer} }});", json!(script)))
        .await
}

#[cfg(unix)]
fn bytes(result: &Value) -> usize {
    result["stdout"].as_str().unwrap().len() + result["stderr"].as_str().unwrap().len()
}

#[tokio::test(flavor = "multi_thread")]
async fn max_buffer_is_validated_before_spawning_and_caps_output_on_every_platform() {
    let exe = json!(std::env::current_exe().unwrap().display().to_string());
    let result = value_of(&format!(
        r#"const max = require("buffer").constants.MAX_STRING_LENGTH;
		const out = [];
		for (const maxBuffer of [-1, 0.5, NaN, Infinity, -Infinity, max + 1]) {{
			out.push(await pi.exec("pi-command-that-does-not-exist-11360", [], {{ maxBuffer }}).then(() => "resolved", (e) => e.name));
		}}
		const capped = await pi.exec({exe}, ["--list"], {{ maxBuffer: 5 }});
		const zero = await pi.exec({exe}, ["--list"], {{ maxBuffer: 0 }});
		const whole = await pi.exec({exe}, ["--list"], {{ maxBuffer: max }});
		return {{ out, capped, zero, whole: {{ truncated: whole.truncated, code: whole.code, listed: whole.stdout.includes(": test") }} }};"#
    ))
    .await;
    assert_eq!(
        result["out"],
        json!(vec!["RangeError"; 6]),
        "a missing command resolves, so a rejection means no spawn"
    );
    for (key, length) in [("capped", 5), ("zero", 0)] {
        let capped = &result[key];
        assert_eq!(capped["stdout"].as_str().unwrap().len(), length, "{key}: {capped}");
        assert_eq!((&capped["truncated"], &capped["killed"]), (&json!(true), &json!(true)), "{key}: {capped}");
        assert_ne!(capped["code"], 0, "{key}: {capped}");
    }
    assert_eq!(result["whole"], json!({"truncated": false, "code": 0, "listed": true}));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_cap_counts_both_streams_and_stops_the_command() {
    let zero = exec("printf x", "0").await;
    assert_eq!((&zero["stdout"], &zero["truncated"], &zero["killed"]), (&json!(""), &json!(true), &json!(true)));
    assert_ne!(zero["code"], 0);
    let script =
        "printf '%0600d' 0 | tr 0 e >&2; printf '%0600d' 0 | tr 0 o; while :; do printf '%01000d' 0; sleep 0.01; done";
    let capped = exec(script, "1000").await;
    assert_eq!((bytes(&capped), &capped["truncated"], &capped["killed"]), (1000, &json!(true), &json!(true)));
    assert_ne!(capped["code"], 0);
    let fits = exec("printf '%01000d' 0 | tr 0 a", "1000").await;
    assert_eq!(fits, json!({"stdout": "a".repeat(1000), "stderr": "", "code": 0, "killed": false, "truncated": false}));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_keeps_whole_utf8_characters_and_eof_still_flushes() {
    for (redirect, key) in [("", "stdout"), (" >&2", "stderr")] {
        let euro = exec(&format!(r"printf '\342\202\254'{redirect}"), "1").await;
        assert_eq!((&euro[key], &euro["truncated"]), (&json!(""), &json!(true)), "{key}");
        let mixed = exec(&format!(r"printf 'a\342\202\254\360\237\231\202'{redirect}"), "5").await;
        assert_eq!((&mixed[key], &mixed["truncated"]), (&json!("a€"), &json!(true)), "{key}");
        assert!(bytes(&mixed) <= 5);
    }
    let eof = exec(r"printf '\342'; printf '\360' >&2", "1000").await;
    assert_eq!(
        eof,
        json!({"stdout": "\u{FFFD}", "stderr": "\u{FFFD}", "code": 0, "killed": false, "truncated": false})
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_child_that_keeps_writing_after_its_shell_stops_does_not_keep_the_call_open() {
    let result = exec("yes; echo done", "1000").await;
    assert_eq!((&result["killed"], &result["truncated"]), (&json!(true), &json!(true)));
    assert_eq!(result["stdout"], "y\n".repeat(500));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_ignores_sigterm_is_killed() {
    let result = exec("trap '' TERM; printf xx; while :; do sleep 1; done", "1").await;
    assert_eq!((&result["stdout"], &result["killed"], &result["truncated"]), (&json!("x"), &json!(true), &json!(true)));
    assert_ne!(result["code"], 0);
}

// Upstream issue 5303 (`waitForChildProcess`): a detached descendant that keeps the pipes open.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_descendant_holding_the_pipes_does_not_keep_the_call_open() {
    let result = value_of(
        r#"const started = Date.now();
		const result = await pi.exec("sh", ["-c", "sleep 5 & echo $!; echo done"]);
		const pid = Number(result.stdout.split("\n")[0]);
		if (pid > 0) {
			try { process.kill(pid); } catch {}
		}
		return { ...result, elapsed: Date.now() - started };"#,
    )
    .await;
    assert_eq!(result["stdout"].as_str().unwrap().lines().nth(1), Some("done"), "{result}");
    assert_eq!((&result["code"], &result["truncated"]), (&json!(0), &json!(false)), "{result}");
    assert!(result["elapsed"].as_u64().unwrap() < 3000, "pi.exec waited for the descendant: {result}");
}

/// A background writer gated by token files in `$1`: it starts after `release`, writes TICK<n> only after `ack<n-1>`,
/// then holds the pipes until `stop` or the directory is gone. Each wait gives up after 10 s.
#[cfg(unix)]
const LATE_WRITER: &str = r#"d=$1
alive() { [ -d "$d" ] && [ ! -e "$d/stop" ]; }
wait_token() { n=0; while alive && [ ! -e "$d/$1" ] && [ $n -lt 1000 ]; do sleep 0.01; n=$((n+1)); done; }
( wait_token release; i=1
  while [ $i -le 6 ] && alive; do echo TICK$i; wait_token ack$i; i=$((i+1)); done
  wait_token stop ) &
echo HEAD"#;

/// Runs `LATE_WRITER` through the real `pi.exec` with only its 100 ms idle grace on a virtual clock: each stdout
/// chunk advances it 90 ms before the helper re-arms, and after TICK6 the test checks the pipes at 99 and 100 ms.
#[cfg(unix)]
const LATE_OUTPUT_BODY: &str = r#"const fs = require("fs"), os = require("os"), path = require("path");
const { ChildProcess } = require("child_process");
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-exec-late-"));
const token = (name) => { try { fs.writeFileSync(path.join(dir, name), ""); } catch {} };
const real = { setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout, spawn: ChildProcess.prototype.spawn };
const timers = new Set();
const report = { chunks: [], armed: 0, tick6: false, openAt99: null, closedAt100: null };
let now = 0;
let child;
let watchdog;
const advance = (ms) => {
	now += ms;
	for (const timer of [...timers]) if (timer.at <= now && timers.delete(timer)) timer.callback();
};
try {
	globalThis.setTimeout = (callback, ms, ...rest) => {
		if (ms !== 100) return real.setTimeout(callback, ms, ...rest);
		const timer = { at: now + ms, callback: () => callback(...rest) };
		timers.add(timer);
		report.armed++;
		return timer;
	};
	globalThis.clearTimeout = (handle) => (timers.has(handle) ? timers.delete(handle) : real.clearTimeout(handle));
	ChildProcess.prototype.spawn = function (...args) {
		child = this;
		return real.spawn.apply(this, args);
	};
	let pending;
	try {
		pending = pi.exec("sh", ["-c", SCRIPT, "sh", dir], { maxBuffer: 1000 });
	} finally {
		ChildProcess.prototype.spawn = real.spawn;
	}
	let settled = false;
	await new Promise((resolve, reject) => {
		watchdog = real.setTimeout(() => reject(new Error(`watchdog: ${JSON.stringify(report)}`)), 20000);
		pending.then(() => { settled = true; if (!report.tick6) resolve(); }, reject);
		child.once("exit", () => token("release"));
		let text = "";
		let acked = 0;
		child.stdout.prependListener("data", (chunk) => {
			advance(90);
			report.chunks.push(chunk.toString());
			text += chunk.toString();
			while (acked < 6 && text.includes(`TICK${acked + 1}\n`)) token(`ack${++acked}`);
			if (acked < 6 || report.tick6) return;
			report.tick6 = true;
			real.setTimeout(() => {
				advance(99);
				report.openAt99 = !settled && !child.stdout.destroyed && !child.stderr.destroyed;
				advance(1);
				report.closedAt100 = child.stdout.destroyed && child.stderr.destroyed;
				resolve();
			}, 0);
		});
	});
	return { result: await pending, report };
} finally {
	real.clearTimeout(watchdog);
	globalThis.setTimeout = real.setTimeout;
	globalThis.clearTimeout = real.clearTimeout;
	ChildProcess.prototype.spawn = real.spawn;
	token("stop");
	fs.rmSync(dir, { recursive: true, force: true });
}"#;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn output_a_descendant_writes_after_the_exit_is_kept() {
    let value = value_of(&LATE_OUTPUT_BODY.replace("SCRIPT", &json!(LATE_WRITER).to_string())).await;
    let stdout = "HEAD\nTICK1\nTICK2\nTICK3\nTICK4\nTICK5\nTICK6\n";
    let expected = json!({"stdout": stdout, "stderr": "", "code": 0, "killed": false, "truncated": false});
    assert_eq!(value["result"], expected, "{value}");
    let report = &value["report"];
    // Open while the grace runs, closed when it ends: 6 chunks after the exit took 540 virtual ms, 90 apart.
    assert_eq!((&report["openAt99"], &report["closedAt100"]), (&json!(true), &json!(true)), "{report}");
    assert!(report["armed"].as_u64().unwrap() >= 7, "the grace restarts on the exit and each chunk: {report}");
}
