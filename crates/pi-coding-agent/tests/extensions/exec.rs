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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn output_a_descendant_writes_after_the_exit_is_kept() {
    let script = "(i=1; while [ $i -le 6 ]; do sleep 0.02; echo TICK$i; i=$((i+1)); done) & echo HEAD";
    let result = exec(script, "1000").await;
    let stdout = result["stdout"].as_str().unwrap();
    assert!(stdout.starts_with("HEAD\n") && stdout.contains("TICK6"), "{result}");
}
