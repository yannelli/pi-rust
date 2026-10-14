//! Node APIs backed by the operating system: fs.glob, fs.cp filters, bigint Stats, fs.watch,
//! os priorities and interfaces, and process resource usage.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn fs_glob_cp_filter_and_bigint_stats() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    for file in [
        "src/a.js",
        "src/b.ts",
        "src/lib/c.js",
        "src/.hidden/d.js",
        "docs/readme.md",
        "tree/keep.txt",
        "tree/skip.log",
        "tree/sub/deep.txt",
    ] {
        write(&root, file, "x");
    }
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const fs = require("fs");
const path = require("path");
process.chdir(__dirname);
const sorted = (list) => list.map((p) => p.split(path.sep).join("/")).sort();
const sync = sorted(fs.globSync("src/**/*.js"));
const braces = sorted(fs.globSync(["src/*.{js,ts}", "docs/*"]));
const excluded = sorted(fs.globSync("src/**/*.js", { exclude: (p) => p.endsWith("c.js") }));
const cwd = sorted(fs.globSync("*.js", { cwd: path.join(__dirname, "src") }));
const dirents = fs.globSync("docs/*.md", { withFileTypes: true }).map((d) => [d.name, d.isFile(), d.parentPath === path.join(__dirname, "docs")]);
const viaCallback = await new Promise((resolve, reject) => fs.glob("src/lib/*", (err, matches) => (err ? reject(err) : resolve(sorted(matches)))));
const viaPromise = [];
for await (const entry of fs.promises.glob("**/*.md")) viaPromise.push(entry);
const absolute = fs.globSync(path.join(__dirname, "docs", "*.md")).map((p) => p === path.join(__dirname, "docs", "readme.md"));
const seen = [];
fs.cpSync("tree", "copy", { recursive: true, filter: (src, dest) => { seen.push(path.relative(__dirname, src)); return !src.endsWith(".log"); } });
await fs.promises.cp("tree", "copy2", { recursive: true, filter: async (src) => !src.endsWith("sub") });
let syncPromiseError;
try { fs.cpSync("tree", "copy3", { recursive: true, filter: async () => true }); } catch (e) { syncPromiseError = e.code; }
const big = fs.statSync("docs/readme.md", { bigint: true });
const plain = fs.statSync("docs/readme.md");
const lbig = await fs.promises.lstat("docs", { bigint: true });
return {
  sync, braces, excluded, cwd, dirents, viaCallback, viaPromise: sorted(viaPromise), absolute,
  copied: sorted(fs.readdirSync("copy", { recursive: true })), copied2: sorted(fs.readdirSync("copy2", { recursive: true })),
  seen: sorted(seen), syncPromiseError,
  big: [typeof big.size, big.size === 1n, typeof big.mtimeNs, big.mtimeMs === BigInt(Math.floor(plain.mtimeMs)), big.mtime instanceof Date, big.isFile(), lbig.isDirectory(), big instanceof fs.Stats, "mtimeNs" in plain],
  globError: (() => { try { fs.globSync(1); } catch (e) { return e.code; } })(),
};
"#,
    )
    .await;
    assert_eq!(actual["sync"], json!(["src/a.js", "src/lib/c.js"]));
    assert_eq!(actual["braces"], json!(["docs/readme.md", "src/a.js", "src/b.ts"]));
    assert_eq!(actual["excluded"], json!(["src/a.js"]));
    assert_eq!(actual["cwd"], json!(["a.js"]));
    assert_eq!(actual["dirents"], json!([["readme.md", true, true]]));
    assert_eq!(actual["viaCallback"], json!(["src/lib/c.js"]));
    assert_eq!(actual["viaPromise"], json!(["docs/readme.md"]));
    assert_eq!(actual["absolute"], json!([true]));
    assert_eq!(actual["copied"], json!(["keep.txt", "sub", "sub/deep.txt"]));
    assert_eq!(actual["copied2"], json!(["keep.txt", "skip.log"]));
    assert_eq!(actual["seen"], json!(["tree", "tree/keep.txt", "tree/skip.log", "tree/sub", "tree/sub/deep.txt"]));
    assert_eq!(actual["syncPromiseError"], "ERR_INVALID_RETURN_VALUE");
    assert_eq!(actual["big"], json!(["bigint", true, "bigint", true, true, true, true, false, false]));
    assert_eq!(actual["globError"], "ERR_INVALID_ARG_TYPE");
}
