//! node:sqlite: DatabaseSync and StatementSync on the bundled SQLite, user functions,
//! aggregates, and backup. Expected values come from running the same scripts under Node 22.

mod common;

use common::*;

async fn check(body: &str, expected: &str) {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, body).await;
    let expected: serde_json::Value = serde_json::from_str(expected).unwrap();
    assert_same_keys(&actual, &expected);
}

const CORE: &str = r##"
const { DatabaseSync, StatementSync, constants } = require("node:sqlite");
const out = {};
const db = new DatabaseSync(":memory:");
out.types = db.prepare("select typeof(?) a, typeof(?) b, typeof(?) c, typeof(?) d, typeof(?) e, typeof(?) f").get(1, 1.5, 10n, "s", new Uint8Array([1]), null);
for (const [key, value] of [["boolErr", true], ["undefErr", undefined], ["bigErr", 2n ** 70n]]) {
  try { db.prepare("select ?").get(value); } catch (e) { out[key] = [e.name, e.code, e.message]; }
}
out.big = db.prepare("select 9007199254740991 as x").get();
try { db.prepare("select 9007199254740992 as x").get(); } catch (e) { out.tooBig = [e.name, e.code, e.message]; }
const st = db.prepare("select 9007199254740992 as x, 1 as y");
st.setReadBigInts(true);
out.readBig = Object.values(st.get()).map(String);
db.exec("create table t(id integer primary key, name text not null unique, score real)");
const ins = db.prepare("insert into t(name, score) values (:name, $score)");
out.run = [ins.run({ name: "a", score: 1 }), ins.run({ name: "b", score: 2 }), ins.run({ ":name": "c", "$score": 3 })];
try { ins.run({ name: "a", score: 1 }); } catch (e) { out.unique = [e.code, e.message, e.errcode, e.errstr]; }
try { ins.run({ name: "d", score: 1, extra: 2 }); } catch (e) { out.unknown = [e.code, e.message]; }
out.all = db.prepare("select * from t order by id").all();
out.proto = Object.getPrototypeOf(out.all[0]);
out.allArgs = db.prepare("select name from t where id > ? and score < ?").all(1, 3);
const arr = db.prepare("select id, name from t order by id");
arr.setReturnArrays(true);
out.arrays = arr.all();
out.iter = [...db.prepare("select name from t order by id desc").iterate()].map((r) => r.name);
const it = db.prepare("select id from t order by id").iterate();
out.iterSteps = [it.next(), it.return(), it.next()].map((step) => [step.done, step.value && step.value.id]);
out.columns = db.prepare("select id, name as n, 1 + 1 as two from t").columns();
const ex = db.prepare("select * from t where name = ? and score > ?");
ex.get("a", 0.5);
out.sql = [ex.sourceSQL, ex.expandedSQL];
out.tx = [db.isTransaction];
db.exec("begin");
out.tx.push(db.isTransaction);
db.exec("rollback");
out.tx.push(db.isTransaction);
out.location = [db.location(), db.location("temp")];
try { db.exec("insert into nope values (1)"); } catch (e) { out.execErr = [e.name, e.code, e.message, e.errcode, e.errstr]; }
const named = db.prepare("select :a as a, @b as b, ?3 as c");
out.partial = named.get({ a: 1 }, 7);
named.setAllowBareNamedParameters(false);
try { named.get({ a: 1, b: 2 }); } catch (e) { out.bare = [e.code, e.message]; }
out.prefixed = named.get({ ":a": 1, "@b": 2 });
try { db.prepare("select :a, $a").get({ a: 1 }); } catch (e) { out.conflict = [e.code, e.message]; }
try { db.prepare("select ?").get(1, 2); } catch (e) { out.range = [e.code, e.message, e.errcode, e.errstr]; }
out.emptyGet = db.prepare("select 1 where 0").get() === undefined;
out.blob = db.prepare("select x'0102ff' as b").get().b;
out.blobType = out.blob.constructor.name;
out.isOpen = db.isOpen;
db.close();
out.isOpen2 = db.isOpen;
try { db.close(); } catch (e) { out.closeTwice = [e.code, e.message]; }
try { ins.run({ name: "z", score: 1 }); } catch (e) { out.afterClose = [e.code, e.message]; }
try { db.prepare("select 1"); } catch (e) { out.prepClosed = [e.code, e.message]; }
const lazy = new DatabaseSync(":memory:", { open: false });
out.lazy = [lazy.isOpen];
lazy.open();
out.lazy.push(lazy.isOpen);
try { lazy.open(); } catch (e) { out.openTwice = [e.code, e.message]; }
try { new DatabaseSync(5); } catch (e) { out.badPath = [e.code, e.message]; }
try { DatabaseSync(":memory:"); } catch (e) { out.noNew = [e.code, e.message]; }
try { new StatementSync(); } catch (e) { out.illegal = [e.code, e.message]; }
try { new DatabaseSync(":memory:", { timeout: "x" }); } catch (e) { out.badTimeout = [e.code, e.message]; }
try { new DatabaseSync(":memory:", { readOnly: 1 }); } catch (e) { out.badReadOnly = [e.code, e.message]; }
try { lazy.prepare(1); } catch (e) { out.badSql = [e.code, e.message]; }
const fk = new DatabaseSync(":memory:");
fk.exec("create table p(id integer primary key); create table c(p integer references p(id))");
try { fk.exec("insert into c values (5)"); } catch (e) { out.fk = [e.message, e.errcode]; }
const nofk = new DatabaseSync(":memory:", { enableForeignKeyConstraints: false });
nofk.exec("create table p(id integer primary key); create table c(p integer references p(id)); insert into c values (5)");
out.nofk = nofk.prepare("select count(*) as n from c").get().n;
try { fk.enableLoadExtension(true); } catch (e) { out.ext = [e.code, e.message]; }
try { fk.exec('select "nope"'); } catch (e) { out.dq = e.message; }
const dq = new DatabaseSync(":memory:", { enableDoubleQuotedStringLiterals: true });
out.dqOn = dq.prepare('select "yes" as v').get().v;
const opts = new DatabaseSync(":memory:", { readBigInts: true, returnArrays: true, allowUnknownNamedParameters: true });
out.dbOptions = opts.prepare("select :a as a").all({ a: 5, b: 6 }).map((row) => row.map(String));
out.constants = constants;
out.dispose = typeof fk[Symbol.dispose];
fk[Symbol.dispose]();
out.disposed = fk.isOpen;
return JSON.parse(JSON.stringify(out, (k, v) => (typeof v === "bigint" ? `${v}n` : v instanceof Uint8Array ? Array.from(v) : v)));
"##;

const CORE_EXPECTED: &str = r##"{"types":{"a":"real","b":"real","c":"integer","d":"text","e":"blob","f":"null"},"boolErr":["TypeError","ERR_INVALID_ARG_TYPE","Provided value cannot be bound to SQLite parameter 1."],"undefErr":["TypeError","ERR_INVALID_ARG_TYPE","Provided value cannot be bound to SQLite parameter 1."],"bigErr":["TypeError","ERR_INVALID_ARG_VALUE","BigInt value is too large to bind."],"big":{"x":9007199254740991},"tooBig":["RangeError","ERR_OUT_OF_RANGE","Value is too large to be represented as a JavaScript number: 9007199254740992"],"readBig":["9007199254740992","1"],"run":[{"lastInsertRowid":1,"changes":1},{"lastInsertRowid":2,"changes":1},{"lastInsertRowid":3,"changes":1}],"unique":["ERR_SQLITE_ERROR","UNIQUE constraint failed: t.name",2067,"constraint failed"],"unknown":["ERR_INVALID_STATE","Unknown named parameter 'extra'"],"all":[{"id":1,"name":"a","score":1},{"id":2,"name":"b","score":2},{"id":3,"name":"c","score":3}],"proto":null,"allArgs":[{"name":"b"}],"arrays":[[1,"a"],[2,"b"],[3,"c"]],"iter":["c","b","a"],"iterSteps":[[false,1],[true,null],[true,null]],"columns":[{"column":"id","database":"main","name":"id","table":"t","type":"INTEGER"},{"column":"name","database":"main","name":"n","table":"t","type":"TEXT"},{"column":null,"database":null,"name":"two","table":null,"type":null}],"sql":["select * from t where name = ? and score > ?","select * from t where name = 'a' and score > 0.5"],"tx":[false,true,false],"location":[null,null],"execErr":["Error","ERR_SQLITE_ERROR","no such table: nope",1,"SQL logic error"],"partial":{"a":1,"b":null,"c":7},"bare":["ERR_INVALID_STATE","Unknown named parameter 'a'"],"prefixed":{"a":1,"b":2,"c":null},"conflict":["ERR_INVALID_STATE","Cannot create bare named parameter 'a' because of conflicting names ':a' and '$a'."],"range":["ERR_SQLITE_ERROR","column index out of range",25,"column index out of range"],"emptyGet":true,"blob":[1,2,255],"blobType":"Uint8Array","isOpen":true,"isOpen2":false,"closeTwice":["ERR_INVALID_STATE","database is not open"],"afterClose":["ERR_INVALID_STATE","statement has been finalized"],"prepClosed":["ERR_INVALID_STATE","database is not open"],"lazy":[false,true],"openTwice":["ERR_INVALID_STATE","database is already open"],"badPath":["ERR_INVALID_ARG_TYPE","The \"path\" argument must be a string, Uint8Array, or URL without null bytes."],"noNew":["ERR_CONSTRUCT_CALL_REQUIRED","Cannot call constructor without `new`"],"illegal":["ERR_ILLEGAL_CONSTRUCTOR","Illegal constructor"],"badTimeout":["ERR_INVALID_ARG_TYPE","The \"options.timeout\" argument must be an integer."],"badReadOnly":["ERR_INVALID_ARG_TYPE","The \"options.readOnly\" argument must be a boolean."],"badSql":["ERR_INVALID_ARG_TYPE","The \"sql\" argument must be a string."],"fk":["FOREIGN KEY constraint failed",787],"nofk":1,"ext":["ERR_INVALID_STATE","Cannot enable extension loading because it was disabled at database creation."],"dq":"no such column: \"nope\" - should this be a string literal in single-quotes?","dqOn":"yes","dbOptions":[["5"]],"constants":{"SQLITE_CHANGESET_OMIT":0,"SQLITE_CHANGESET_REPLACE":1,"SQLITE_CHANGESET_ABORT":2,"SQLITE_CHANGESET_DATA":1,"SQLITE_CHANGESET_NOTFOUND":2,"SQLITE_CHANGESET_CONFLICT":3,"SQLITE_CHANGESET_CONSTRAINT":4,"SQLITE_CHANGESET_FOREIGN_KEY":5},"dispose":"function","disposed":false}"##;

#[tokio::test]
async fn sqlite_statements_and_options() {
    check(CORE, CORE_EXPECTED).await;
}

const FUNCTIONS: &str = r##"
const { DatabaseSync, backup } = require("node:sqlite");
const fs = require("fs");
const path = require("path");
const out = {};
const db = new DatabaseSync(":memory:");
db.function("twice", (x) => x * 2);
db.function("concat", { varargs: true }, (...a) => a.join("-"));
db.function("kinds", { useBigIntArguments: true }, (a, b) => typeof a + typeof b);
out.fn = db.prepare("select twice(21) as t, typeof(twice(1)) as ty, concat(1, 'a', null) as c, kinds(1, 2.5) as k").get();
db.function("thrower", () => { throw new TypeError("from fn"); });
try { db.prepare("select thrower()").get(); } catch (e) { out.fnThrow = [e.name, e.message, e.code === undefined]; }
for (const [name, value] of [["bad", {}], ["big", 2n ** 70n], ["prom", Promise.resolve(1)]]) {
  db.function(name, () => value);
  try { db.prepare(`select ${name}()`).get(); } catch (e) { out[name] = [e.code, e.message, e.errcode]; }
}
db.function("i16", () => new Int16Array([1]));
db.function("undef", () => undefined);
db.function("bigok", () => 5n);
out.returns = db.prepare("select i16() as a, undef() as b, bigok() as c, typeof(bigok()) as d").get();
db.function("arity", (a, b, c) => 1);
try { db.prepare("select arity(1)").get(); } catch (e) { out.arity = [e.code, e.message]; }
db.function("nested", () => db.prepare("select 40 + 2 as v").get().v);
out.nested = db.prepare("select nested() as n").get().n;
try { db.function("f", 1); } catch (e) { out.badFn = [e.code, e.message]; }
try { db.function(1, () => {}); } catch (e) { out.badName = [e.code, e.message]; }
db.exec("create table w(v); insert into w values (1),(2),(3),(4)");
db.aggregate("sumsq", { start: 0, step: (acc, v) => acc + v * v });
db.aggregate("collect", { start: () => [], step: (acc, v) => { acc.push(v); return acc; }, result: (acc) => acc.join(",") });
db.aggregate("win", { start: 0, step: (a, v) => a + v, inverse: (a, v) => a - v, result: (a) => a * 10 });
out.agg = db.prepare("select sumsq(v) s, collect(v) c from w").get();
out.aggEmpty = db.prepare("select sumsq(v) s, collect(v) c from w where 0").get();
out.window = db.prepare("select win(v) over (order by v rows between 1 preceding and current row) as x from w").all().map((r) => r.x);
db.aggregate("thr", { start: 0, step: (a, v) => { throw new RangeError("agg"); } });
try { db.prepare("select thr(v) from w").get(); } catch (e) { out.aggThrow = [e.name, e.message]; }
try { db.aggregate("a", {}); } catch (e) { out.aggNoStart = [e.code, e.message]; }
try { db.aggregate("a", { start: 0 }); } catch (e) { out.aggNoStep = [e.code, e.message]; }
const file = path.join(__dirname, "fn-test.db");
const copy = path.join(__dirname, "fn-copy.db");
for (const name of [file, copy]) fs.rmSync(name, { force: true });
const disk = new DatabaseSync(file);
disk.exec("create table x(a); insert into x values (1), (2)");
out.fileLocation = disk.location() === file;
const progress = [];
out.backupPages = await backup(disk, copy, { rate: 1, progress: (info) => progress.push(Object.keys(info).sort().join()) });
out.progress = [...new Set(progress)];
disk.close();
const reopened = new DatabaseSync(copy, { readOnly: true });
out.copied = reopened.prepare("select count(*) as n from x").get().n;
try { reopened.exec("insert into x values (3)"); } catch (e) { out.readOnly = [e.message, e.errcode, e.errstr]; }
reopened.close();
try { new DatabaseSync(path.join(__dirname, "missing", "x.db")); } catch (e) { out.cantOpen = [e.code, e.message, e.errcode, e.errstr]; }
for (const name of [file, copy]) fs.rmSync(name, { force: true });
return JSON.parse(JSON.stringify(out, (k, v) => (typeof v === "bigint" ? `${v}n` : v instanceof Uint8Array ? Array.from(v) : v)));
"##;

const FUNCTIONS_EXPECTED: &str = r##"{"fn":{"t":42,"ty":"real","c":"1-a-","k":"bigintnumber"},"fnThrow":["TypeError","from fn",true],"bad":["ERR_SQLITE_ERROR","Returned JavaScript value cannot be converted to a SQLite value",1],"big":["ERR_SQLITE_ERROR","BigInt value is too large for SQLite",1],"prom":["ERR_SQLITE_ERROR","Asynchronous user-defined functions are not supported",1],"returns":{"a":[1,0],"b":null,"c":5,"d":"integer"},"arity":["ERR_SQLITE_ERROR","wrong number of arguments to function arity()"],"nested":42,"badFn":["ERR_INVALID_ARG_TYPE","The \"function\" argument must be a function."],"badName":["ERR_INVALID_ARG_TYPE","The \"name\" argument must be a string."],"agg":{"s":30,"c":"1,2,3,4"},"aggEmpty":{"s":0,"c":""},"window":[10,30,50,70],"aggThrow":["RangeError","agg"],"aggNoStart":["ERR_INVALID_ARG_TYPE","The \"options.start\" argument must be a function or a primitive value."],"aggNoStep":["ERR_INVALID_ARG_TYPE","The \"options.step\" argument must be a function."],"fileLocation":true,"backupPages":2,"progress":["remainingPages,totalPages"],"copied":2,"readOnly":["attempt to write a readonly database",8,"attempt to write a readonly database"],"cantOpen":["ERR_SQLITE_ERROR","unable to open database file",14,"unable to open database file"]}"##;

#[tokio::test]
async fn sqlite_functions_aggregates_and_backup() {
    check(FUNCTIONS, FUNCTIONS_EXPECTED).await;
}

#[tokio::test]
async fn sqlite_loads_only_with_the_node_prefix() {
    let body = r#"
const m = require("module");
const out = {};
for (const name of ["sqlite", "sea"]) {
  try { require(name); out[name] = "loaded"; } catch (e) { out[name] = e.code; }
}
out.isBuiltin = ["sqlite", "node:sqlite", "fs", "node:fs"].map((name) => m.isBuiltin(name));
out.listed = ["sqlite", "sea", "fs"].map((name) => m.builtinModules.includes(name));
out.loads = typeof require("node:sqlite").DatabaseSync;
out.esm = typeof (await import("node:sqlite")).DatabaseSync;
return out;
"#;
    let expected = r#"{"sqlite": "MODULE_NOT_FOUND", "sea": "MODULE_NOT_FOUND", "isBuiltin": [false, true, true, true],
        "listed": [false, false, true], "loads": "function", "esm": "function"}"#;
    check(body, expected).await;
}
