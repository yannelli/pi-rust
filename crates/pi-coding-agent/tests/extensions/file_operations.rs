//! SDK file-tool factories with an extension-owned filesystem.

use std::time::Duration;

use serde_json::{Value, json};

use crate::extensions::harness::{Setup, harness};

const FILE_TOOLS: &str = r#"
import {
  createEditTool, createEditToolDefinition, createReadTool, createReadToolDefinition, createWriteTool, createWriteToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { join, dirname } from "node:path";
export default function(pi) {
  pi.registerCommand("virtual-files", { handler: async (_, ctx) => {
    const path = join(ctx.cwd, "nested", "record.txt");
    const files = new Map([[path, "virtual original"]]);
    const calls = [];
    const operations = {
      files,
      async access(p) { if (!this.files.has(p)) throw Object.assign(new Error("missing"), { code: "ENOENT" }); },
      async readFile(p) { return Buffer.from(this.files.get(p)); },
      async writeFile(p, text) { this.files.set(p, text); },
      async mkdir(dir) { calls.push(dir === dirname(path)); },
    };
    const options = { operations };
    const params = { path: "nested/record.txt" };
    const factoryCwd = join(ctx.cwd, "unused");
    const execute = (factory, args) => factory(factoryCwd, options).execute("call", args, undefined, undefined, ctx);
    const before = await execute(createReadToolDefinition, params);
    await execute(createWriteTool, { ...params, content: "abc\r\nRésumé\r\n" });
    const edited = await execute(createEditToolDefinition, { ...params, edits: [{ oldText: "abc", newText: "def" }] });
    const after = await execute(createReadTool, params);
    const accessError = await execute(createEditTool, { path: "missing", oldText: "x", newText: "y" })
      .then(() => null, e => e.message);
    const image = await createReadTool(ctx.cwd, { autoResizeImages: false, operations: {
      access: async () => {}, readFile: async () => Buffer.from([0, 255, 1]), detectImageMimeType: async () => "image/png",
    } }).execute("image", { path: "remote.png" });
    const denied = Object.assign(new Error("store is read-only"), { code: "EROFS" });
    operations.writeFile = async () => { throw denied; };
    const propagated = await execute(createWriteToolDefinition, { ...params, content: "denied" })
      .then(() => false, e => e === denied && e.code === "EROFS");
    pi.appendEntry("virtual-files", { before: before.structuredContent, after: after.structuredContent,
      firstChangedLine: edited.details.firstChangedLine, mkdir: calls, accessError, propagated, image: image.content[1] });
  } });
  pi.registerCommand("abort-file", { handler: async (_, ctx) => {
    const abort = new AbortController();
    let entered, release;
    const started = new Promise(resolve => { entered = resolve; });
    const stalled = new Promise(resolve => { release = resolve; });
    const tool = createReadTool(ctx.cwd, { operations: {
      access: async () => { entered(); await stalled; },
      readFile: async () => { throw new Error("read after abort"); },
    } });
    const result = tool.execute("cancel", { path: "virtual" }, abort.signal).then(() => null, e => e.message);
    await started;
    abort.abort();
    try { pi.appendEntry("aborted", { error: await result }); }
    finally { release(); }
  } });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn file_factories_forward_operations_and_keep_host_files_out_of_the_virtual_workspace() {
    let setup = Setup {
        files: vec![("files.ts", FILE_TOOLS.to_owned())],
        load: vec!["files.ts"],
        root_files: vec![("work/nested/record.txt", "original on disk".into())],
        ..Setup::default()
    };
    let h = harness(setup).await;
    for command in ["/virtual-files", "/abort-file"] {
        tokio::time::timeout(Duration::from_secs(25), h.session.prompt(command, Vec::new()))
            .await
            .expect("file operation completes")
            .unwrap();
    }
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let data = |kind: &str| {
        entries
            .iter()
            .find(|entry| entry["customType"] == kind)
            .map(|entry| entry["data"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(
        data("virtual-files"),
        json!({
            "before": "virtual original", "after": "def\r\nRésumé\r\n", "firstChangedLine": 1,
            "mkdir": [true, true], "accessError": "Could not edit file: missing. Error code: ENOENT.", "propagated": true,
            "image": {"type": "image", "mimeType": "image/png", "data": "AP8B"},
        })
    );
    assert_eq!(data("aborted"), json!({"error": "Operation aborted"}));
    assert_eq!(std::fs::read(h.cwd.join("nested/record.txt")).unwrap(), b"original on disk");
    assert!(h.session.undo_candidates().is_empty());
}
