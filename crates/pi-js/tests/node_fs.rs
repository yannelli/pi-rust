mod common;

use common::*;

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
