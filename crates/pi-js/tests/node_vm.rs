//! node:vm: separate contexts, timeouts, compileFunction, and values crossing contexts.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, body).await
}

#[tokio::test]
async fn vm_contexts_and_globals() {
    let actual = run(r#"
const vm = require("vm");
const sandbox = { x: 2, list: [1] };
vm.createContext(sandbox);
const out = [vm.isContext(sandbox), vm.isContext({})];
out.push(vm.runInContext("x * 21", sandbox));
vm.runInContext("var y = x + 1; z = 'set'; x = 10; list.push(2)", sandbox);
out.push(sandbox.y, sandbox.z, sandbox.x, sandbox.list.length);
sandbox.x = 7;
out.push(vm.runInContext("x + y", sandbox));
out.push(vm.runInContext("typeof require + typeof console + typeof Array", sandbox));
const arr = vm.runInNewContext("[1, 2]");
out.push(Array.isArray(arr), arr instanceof Array);
const error = (() => { try { vm.runInContext("throw new TypeError('in')", sandbox); } catch (e) { return e; } })();
out.push(error instanceof TypeError, error.name, error.message);
out.push(vm.runInThisContext("typeof setTimeout"), new vm.Script("1 + 1").runInThisContext());
const script = new vm.Script("count = (typeof count === 'number' ? count : 0) + 1");
const a = vm.createContext({}), b = vm.createContext({ count: 10 });
script.runInContext(a); script.runInContext(a); script.runInContext(b);
out.push(a.count, b.count);
try { new vm.Script("let let = 1"); } catch (e) { out.push(e.name); }
try { vm.runInContext("1", {}); } catch (e) { out.push(e.code); }
const plain = vm.createContext(vm.constants.DONT_CONTEXTIFY);
vm.runInContext("var q = 5", plain);
out.push(plain.q, vm.isContext(plain));
out.push(vm.runInNewContext("a + b", { a: 1, b: 2 }));
const fn = vm.runInNewContext("(function (o) { return o.v * 2; })");
out.push(fn({ v: 4 }));
const promise = vm.runInNewContext("Promise.resolve(3)");
out.push(await promise);
return out;
"#)
    .await;
    assert_eq!(
        actual,
        json!([
            true,
            false,
            42,
            3,
            "set",
            10,
            2,
            10,
            "undefinedundefinedfunction",
            true,
            false,
            false,
            "TypeError",
            "in",
            "function",
            2,
            2,
            11,
            "SyntaxError",
            "ERR_INVALID_ARG_TYPE",
            5,
            true,
            3,
            8,
            3
        ])
    );
}

#[tokio::test]
async fn vm_timeouts_and_compile_function() {
    let actual = run(r#"
const vm = require("vm");
const out = [];
const context = vm.createContext({});
const timed = (code, ctx) => { try { return ctx ? vm.runInContext(code, ctx, { timeout: 50 }) : vm.runInThisContext(code, { timeout: 50 }); } catch (e) { return [e.code, e.message]; } };
out.push(timed("while (true) {}", context));
out.push(timed("try { while (true) {} } catch (e) { 'caught' }", context));
out.push(timed("let n = 0; for (let i = 0; i < 1000; i++) n += i; n"));
out.push(timed("for (;;) {}"));
try { vm.runInContext("1", context, { timeout: 0 }); } catch (e) { out.push(e.code); }
const add = vm.compileFunction("return a + b + c", ["a", "b"], { contextExtensions: [{ c: 100 }] });
out.push(add(1, 2), add.length);
const inContext = vm.compileFunction("return typeof marker + ':' + marker", [], { parsingContext: vm.createContext({ marker: "ctx" }) });
out.push(inContext());
try { vm.compileFunction("return )"); } catch (e) { out.push(e.name); }
const micro = vm.createContext({ log: [] }, { microtaskMode: "afterEvaluate" });
vm.runInContext("Promise.resolve().then(() => log.push('micro')); log.push('sync')", micro);
out.push(micro.log.join(","));
try { vm.createContext({}, { microtaskMode: "never" }); } catch (e) { out.push(e.code); }
const memory = await vm.measureMemory();
out.push(typeof memory.total.jsMemoryEstimate, typeof vm.constants.DONT_CONTEXTIFY, vm.SourceTextModule);
const s = new vm.Script("1", { produceCachedData: true });
out.push(Buffer.isBuffer(s.createCachedData()), new vm.Script("1", { cachedData: s.cachedData }).cachedDataRejected);
return out;
"#)
    .await;
    let timeout = json!(["ERR_SCRIPT_EXECUTION_TIMEOUT", "Script execution timed out after 50ms"]);
    assert_eq!(
        actual,
        json!([
            timeout,
            timeout,
            499500,
            timeout,
            "ERR_OUT_OF_RANGE",
            103,
            2,
            "string:ctx",
            "SyntaxError",
            "sync,micro",
            "ERR_INVALID_ARG_VALUE",
            "number",
            "symbol",
            null,
            true,
            false
        ])
    );
}
