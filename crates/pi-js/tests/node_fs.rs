mod common;
mod node_common;

use common::*;

#[tokio::test]
async fn file_and_directory_timestamps_match_real_node() {
    const SCRIPT: &str = r#"
const fs = require("node:fs");
const mark = (text) => fs.writeFileSync("stage", text);
mark("create paths");
fs.writeFileSync("file", "unchanged");
fs.mkdirSync("directory");
const out = {};
for (const path of ["file", "directory"]) {
  const times = () => { const s = fs.statSync(path); return [s.atimeMs, s.mtimeMs]; };
  mark(`${path}: sync`);
  fs.utimesSync(path, new Date(1000005), new Date(2000005));
  const sync = times();
  mark(`${path}: callback`);
  await new Promise((resolve, reject) => fs.utimes(path, new Date(3000007), 4000.125, (error) => error ? reject(error) : resolve()));
  const callback = times();
  mark(`${path}: promises`);
  await fs.promises.utimes(path, 5000.25, new Date(6000009));
  out[path] = { sync, callback, promise: times() };
}
out.contents = fs.readFileSync("file", "utf8");
out.directory = fs.statSync("directory").isDirectory();
mark("done");
return out;
"#;
    let (_node_dir, node_root) = tempdir();
    let Some((node, _capture)) = node_common::node_runtime(&node_root) else {
        assert!(std::env::var_os("CI").is_none(), "CI must run the real Node timestamp control");
        return;
    };
    let expected = run_cjs_within(&node, &node_root, SCRIPT, 30).await;
    let (_quickjs_dir, quickjs_root) = tempdir();
    let (quickjs, _capture) = runtime(&quickjs_root);
    let actual = run_cjs_within(&quickjs, &quickjs_root, SCRIPT, 30).await;
    assert_eq!(actual, expected);
}

// Expected values come from running the same script under Node 22 in an empty directory.
const FS_SCRIPT: &str = r##"
const fs = require("node:fs");
const fsp = require("fs/promises");
const path = require("path");
const out = {};
fs.mkdirSync("work/a/b", { recursive: true });
out.mkdirAgain = fs.mkdirSync("work/a/b", { recursive: true });
fs.writeFileSync("work/a/hello.txt", "héllo");
fs.appendFileSync("work/a/hello.txt", Buffer.from(" world"));
out.read = [fs.readFileSync("work/a/hello.txt", "utf8"), fs.readFileSync("work/a/hello.txt").length];
const stat = fs.statSync("work/a/hello.txt");
out.stat = [stat.isFile(), stat.isDirectory(), stat.size, stat.mtime instanceof Date, fs.statSync("work/a").isDirectory()];
out.exists = [fs.existsSync("work/a/hello.txt"), fs.existsSync("work/nope"), fs.existsSync()];
out.noEntry = fs.statSync("work/nope", { throwIfNoEntry: false });
try { fs.readFileSync("work/nope.txt"); } catch (e) { out.enoent = [e.code, e.syscall, e.path, e.message]; }
try { fs.mkdirSync("work/a"); } catch (e) { out.eexist = [e.code, e.message]; }
out.readdir = fs.readdirSync("work/a").sort();
out.dirents = fs.readdirSync("work/a", { withFileTypes: true }).map((d) => [d.name, d.isDirectory(), d.isFile()]).sort();
fs.writeFileSync("work/a/b/deep.txt", "d");
out.recursive = fs.readdirSync("work", { recursive: true }).map((p) => p.split(path.sep).join("/")).sort();
fs.renameSync("work/a/hello.txt", "work/a/renamed.txt");
fs.copyFileSync("work/a/renamed.txt", "work/copy.txt");
try { fs.copyFileSync("work/a/renamed.txt", "work/copy.txt", fs.constants.COPYFILE_EXCL); } catch (e) { out.excl = e.code; }
fs.cpSync("work/a", "work/cp", { recursive: true });
out.cp = fs.readdirSync("work/cp", { recursive: true }).map((p) => p.split(path.sep).join("/")).sort();
fs.symlinkSync("copy.txt", "work/link.txt");
out.link = [fs.readlinkSync("work/link.txt"), fs.lstatSync("work/link.txt").isSymbolicLink(), fs.readFileSync("work/link.txt", "utf8")];
out.realpath = path.basename(fs.realpathSync("work/link.txt"));
const fd = fs.openSync("work/fd.txt", "w+");
out.fdWrite = [fs.writeSync(fd, "abcdef"), fs.writeSync(fd, Buffer.from("XY"), 0, 2, 1)];
const buf = Buffer.alloc(4);
out.fdRead = [fs.readSync(fd, buf, 0, 4, 2), buf.toString(), fs.fstatSync(fd).size];
fs.ftruncateSync(fd, 3);
fs.closeSync(fd);
out.truncated = fs.readFileSync("work/fd.txt", "utf8");
try { fs.closeSync(fd); } catch (e) { out.ebadf = e.code; }
fs.chmodSync("work/copy.txt", 0o600);
out.mode = (fs.statSync("work/copy.txt").mode & 0o777).toString(8);
fs.utimesSync("work/copy.txt", 1000, 2000);
out.utimes = [fs.statSync("work/copy.txt").mtimeMs, fs.statSync("work/copy.txt").atime.getTime()];
const tmp = fs.mkdtempSync("work/tmp-");
out.mkdtemp = [path.basename(tmp).length, fs.statSync(tmp).isDirectory()];
fs.rmSync("work/cp", { recursive: true });
try { fs.rmSync("work/a"); } catch (e) { out.rmDir = e.code; }
fs.rmSync("work/nope", { force: true });
fs.unlinkSync("work/link.txt");
out.afterRm = fs.readdirSync("work").filter((n) => !n.startsWith("tmp-")).sort();
out.cb = await new Promise((resolve) => fs.readFile("work/copy.txt", "utf8", (err, data) => resolve([err, data])));
out.cbErr = await new Promise((resolve) => fs.stat("work/missing", (err) => resolve(err.code)));
out.existsCb = await new Promise((resolve) => fs.exists("work/copy.txt", resolve));
await fsp.writeFile("work/p.json", JSON.stringify({ a: 1 }));
out.promises = [JSON.parse(await fsp.readFile("work/p.json", "utf8")), (await fsp.readdir("work")).includes("p.json")];
await fsp.mkdir("work/pdir/x", { recursive: true });
await fsp.rm("work/pdir", { recursive: true, force: true });
try { await fsp.access("work/pdir"); } catch (e) { out.access = e.code; }
const handle = await fsp.open("work/handle.txt", "w+");
await handle.write("line1\nline2\n");
const { bytesRead, buffer } = await handle.read(Buffer.alloc(5), 0, 5, 0);
out.handle = [bytesRead, buffer.toString(), (await handle.stat()).size];
await handle.close();
const lines = [];
for await (const line of (await fsp.open("work/handle.txt")).readLines()) lines.push(line);
out.lines = lines;
const chunks = [];
await new Promise((resolve, reject) => fs.createReadStream("work/handle.txt", { start: 2, end: 7, encoding: "utf8" }).on("data", (c) => chunks.push(c)).on("end", resolve).on("error", reject));
out.readStream = chunks.join("");
await new Promise((resolve) => { const w = fs.createWriteStream("work/ws.txt"); w.write("a"); w.write(Buffer.from("b")); w.end("c", resolve); });
out.writeStream = fs.readFileSync("work/ws.txt", "utf8");
const dir = fs.opendirSync("work/a");
const names = [];
for await (const entry of dir) names.push(entry.name);
out.opendir = names.sort();
return out;
"##;

const FS_EXPECTED: &str = r##"{"read":["héllo world",12],"stat":[true,false,12,true,true],"exists":[true,false,false],"enoent":["ENOENT","open","work/nope.txt","ENOENT: no such file or directory, open 'work/nope.txt'"],"eexist":["EEXIST","EEXIST: file already exists, mkdir 'work/a'"],"readdir":["b","hello.txt"],"dirents":[["b",true,false],["hello.txt",false,true]],"recursive":["a","a/b","a/b/deep.txt","a/hello.txt"],"excl":"EEXIST","cp":["b","b/deep.txt","renamed.txt"],"link":["copy.txt",true,"héllo world"],"realpath":"copy.txt","fdWrite":[6,2],"fdRead":[4,"Ydef",6],"truncated":"aXY","ebadf":"EBADF","mode":"600","utimes":[2000000,1000000],"mkdtemp":[10,true],"rmDir":"ERR_FS_EISDIR","afterRm":["a","copy.txt","fd.txt"],"cb":[null,"héllo world"],"cbErr":"ENOENT","existsCb":true,"promises":[{"a":1},true],"access":"ENOENT","handle":[5,"line1",12],"lines":["line1","line2"],"readStream":"ne1\nli","writeStream":"abc","opendir":["b","renamed.txt"]}"##;

#[tokio::test]
#[cfg_attr(windows, ignore = "uses symlinks and Unix permission bits")]
async fn fs_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, FS_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(FS_EXPECTED).unwrap();
    assert_eq!(actual, expected);
}

// fchmod, fchown, lutimes, lchmod, and FileHandle chmod and chown. Expected values come from Node 22.
const FS_META_SCRIPT: &str = r##"
const fs = require("fs");
const out = {};
fs.writeFileSync("f.txt", "hello");
const fd = fs.openSync("f.txt", "r+");
fs.fchmodSync(fd, 0o640);
out.fchmod = (fs.statSync("f.txt").mode & 0o777).toString(8);
await new Promise((resolve) => fs.fchmod(fd, "600", resolve));
out.fchmodCb = (fs.statSync("f.txt").mode & 0o777).toString(8);
const st = fs.statSync("f.txt");
fs.fchownSync(fd, st.uid, st.gid);
out.fchownCb = await new Promise((resolve) => fs.fchown(fd, st.uid, st.gid, resolve));
fs.closeSync(fd);
try { fs.fchmodSync(fd, 0o600); } catch (e) { out.fchmodClosed = [e.code, e.syscall, e.message]; }
try { fs.fchmodSync(3, "abc"); } catch (e) { out.badMode = [e.code, e.message]; }
try { fs.fchownSync(3, "a", 1); } catch (e) { out.badUid = [e.code, e.message]; }
fs.symlinkSync("f.txt", "link");
fs.lutimesSync("link", 1000, 2000);
out.lutimes = [fs.lstatSync("link").mtimeMs, fs.lstatSync("link").atimeMs, fs.statSync("f.txt").mtimeMs === 2000000];
await fs.promises.lutimes("link", new Date(5000), 6);
out.lutimesPromise = [fs.lstatSync("link").mtimeMs, fs.lstatSync("link").atimeMs];
await new Promise((resolve) => fs.lutimes("link", 7, 8.5, resolve));
out.lutimesCb = fs.lstatSync("link").mtimeMs;
try { fs.lutimesSync("missing", 1, 2); } catch (e) { out.lutimesMissing = [e.code, e.syscall, e.message]; }
const handle = await fs.promises.open("f.txt", "r+");
await handle.chmod(0o644);
out.handleChmod = (fs.statSync("f.txt").mode & 0o777).toString(8);
out.handleChown = await handle.chown(st.uid, st.gid);
await handle.close();
try { await handle.chmod(0o600); } catch (e) { out.handleClosed = e.code; }
if (process.platform === "darwin") {
  fs.lchmodSync("link", 0o700);
  out.lchmod = typeof fs.lchmod === "function" && (fs.lstatSync("link").mode & 0o777) === 0o700;
} else {
  out.lchmod = typeof fs.lchmod === "undefined" && typeof fs.lchmodSync === "undefined";
  try { await fs.promises.lchmod("link", 0o700); } catch (e) { out.lchmod = out.lchmod && e.code === "ERR_METHOD_NOT_IMPLEMENTED" && e.message === "The lchmod() method is not implemented"; }
}
return out;
"##;

const FS_META_EXPECTED: &str = r##"{"fchmod":"640","fchmodCb":"600","fchownCb":null,"fchmodClosed":["EBADF","fchmod","EBADF: bad file descriptor, fchmod"],"badMode":["ERR_INVALID_ARG_VALUE","The argument 'mode' must be a 32-bit unsigned integer or an octal string. Received 'abc'"],"badUid":["ERR_INVALID_ARG_TYPE","The \"uid\" argument must be of type number. Received type string ('a')"],"lutimes":[2000000,1000000,false],"lutimesPromise":[6000,5000],"lutimesCb":8500,"lutimesMissing":["ENOENT","lutime","ENOENT: no such file or directory, lutime 'missing'"],"handleChmod":"644","handleClosed":"EBADF","lchmod":true}"##;

#[tokio::test]
#[cfg_attr(windows, ignore = "uses symlinks and Unix permission bits")]
async fn fs_fd_metadata_and_lutimes_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, FS_META_SCRIPT).await;
    assert_same_keys(&actual, &serde_json::from_str(FS_META_EXPECTED).unwrap());
}

// statfs, openAsBlob, and FileHandle.readableWebStream. Expected values come from Node 22.
const FS_STATFS_SCRIPT: &str = r##"
const fs = require("fs");
const util = require("util");
const out = {};
fs.writeFileSync("f.txt", "hello world");
const stats = fs.statfsSync(".");
out.statfs = [util.inspect(stats).split(" ")[0], Object.keys(stats), Object.values(stats).every((v) => typeof v === "number" && v >= 0), stats.bsize > 0, stats.blocks >= stats.bfree];
const big = fs.statfsSync("f.txt", { bigint: true });
out.statfsBig = Object.values(big).map((v) => typeof v);
try { fs.statfsSync("missing"); } catch (e) { out.statfsMissing = [e.code, e.syscall, e.message, e.path]; }
out.statfsCb = await new Promise((resolve) => fs.statfs(".", (err, s) => resolve([err, typeof s.bavail])));
out.statfsPromise = typeof (await fs.promises.statfs(".", { bigint: true })).ffree;
const blob = await fs.openAsBlob("f.txt", { type: "text/plain" });
out.blob = [blob.constructor.name, blob.size, blob.type, await blob.text(), await blob.slice(6).text()];
try { fs.openAsBlob("missing"); } catch (e) { out.blobMissing = [e.name, e.code, e.message]; }
try { fs.openAsBlob("f.txt", { type: 5 }); } catch (e) { out.blobType = [e.code, e.message]; }
try { fs.openAsBlob(5); } catch (e) { out.blobPath = [e.code, e.message]; }
const handle = await fs.promises.open("f.txt");
const stream = handle.readableWebStream();
const chunks = [];
for await (const chunk of stream) chunks.push([chunk.constructor.name, Buffer.from(chunk).toString()]);
out.webStream = [stream.constructor.name, chunks];
try { handle.readableWebStream(); } catch (e) { out.webLocked = [e.code, e.message]; }
await handle.close();
try { handle.readableWebStream(); } catch (e) { out.webClosed = [e.code, e.message]; }
return out;
"##;

const FS_STATFS_EXPECTED: &str = r##"{"statfs":["StatFs",["type","bsize","blocks","bfree","bavail","files","ffree"],true,true,true],"statfsBig":["bigint","bigint","bigint","bigint","bigint","bigint","bigint"],"statfsMissing":["ENOENT","statfs","ENOENT: no such file or directory, statfs 'missing'","missing"],"statfsCb":[null,"number"],"statfsPromise":"bigint","blob":["Blob",11,"text/plain","hello world","world"],"blobMissing":["TypeError","ERR_INVALID_ARG_VALUE","Unable to open file as blob"],"blobType":["ERR_INVALID_ARG_TYPE","The \"options.type\" property must be of type string. Received type number (5)"],"blobPath":["ERR_INVALID_ARG_TYPE","The \"path\" argument must be of type string or an instance of Buffer or URL. Received type number (5)"],"webStream":["ReadableStream",[["Uint8Array","hello world"]]],"webLocked":["ERR_INVALID_STATE","Invalid state: The FileHandle is locked"],"webClosed":["ERR_INVALID_STATE","Invalid state: The FileHandle is closed"]}"##;

#[tokio::test]
async fn fs_statfs_blob_and_web_stream_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, FS_STATFS_SCRIPT).await;
    assert_same_keys(&actual, &serde_json::from_str(FS_STATFS_EXPECTED).unwrap());
}
