//! node:punycode (RFC 3492 samples), node:sea, and node:trace_events.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, body).await
}

#[tokio::test]
async fn punycode_matches_rfc_3492_samples() {
    let actual = run(r#"
const warnings = [];
process.on("warning", (w) => warnings.push(w.code));
const punycode = require("punycode");
// RFC 3492 section 7.1 samples (A) Arabic, (B) Chinese, (D) Czech, (L) Japanese, (S) ASCII.
const samples = [
  "ليهمابتكلموشعربي؟",
  "他们为什么不说中文",
  "Pročprostěnemluvíčesky",
  "3年B組金八先生",
  "-> $1.00 <-",
];
const encoded = samples.map((s) => punycode.encode(s));
let error;
try { punycode.decode("\x81-"); } catch (e) { error = [e.name, e.message]; }
await new Promise((resolve) => setImmediate(resolve));
return {
  encoded, decoded: encoded.map((e, i) => punycode.decode(e) === samples[i]),
  ascii: [punycode.toASCII("mañana.com"), punycode.toASCII("джpумлатест.bрфa"), punycode.toASCII("user@mañana.com")],
  unicode: [punycode.toUnicode("xn--maana-pta.com"), punycode.toUnicode("example.com")],
  ucs2: [punycode.ucs2.decode("𝌆a"), punycode.ucs2.encode([0x1d306, 97])],
  version: punycode.version, error, warnings,
};
"#)
    .await;
    assert_eq!(
        actual["encoded"],
        json!([
            "egbpdaj6bu4bxfgehfvwxn",
            "ihqwcrb4cv8a8dqg056pqjye",
            "Proprostnemluvesky-uyb24dma41a",
            "3B-ww4c5e180e575a65lsy2b",
            "-> $1.00 <--"
        ])
    );
    assert_eq!(actual["decoded"], json!([true, true, true, true, true]));
    assert_eq!(
        actual["ascii"],
        json!(["xn--maana-pta.com", "xn--p-8sbkgc5ag7bhce.xn--ba-lmcq", "user@xn--maana-pta.com"])
    );
    assert_eq!(actual["unicode"], json!(["mañana.com", "example.com"]));
    assert_eq!(actual["ucs2"], json!([[0x1d306, 97], "\u{1d306}a"]));
    assert_eq!(actual["version"], "2.1.0");
    assert_eq!(actual["error"], json!(["RangeError", "Illegal input >= 0x80 (not a basic code point)"]));
    assert_eq!(actual["warnings"], json!(["DEP0040"]));
}

#[tokio::test]
async fn sea_and_trace_events_behave_as_plain_node() {
    let actual = run(r#"
const sea = require("node:sea");
const trace = require("trace_events");
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return [e.name, e.code, e.message]; } };
const tracing = trace.createTracing({ categories: ["node.perf", "v8"] });
const before = [tracing.enabled, tracing.categories, trace.getEnabledCategories()];
tracing.enable();
const during = [tracing.enabled, trace.getEnabledCategories(), require("util").inspect(tracing)];
tracing.disable();
return {
  sea: [Object.keys(sea), sea.isSea(), err(() => sea.getAsset("a")), err(() => sea.getRawAsset("a")).slice(1, 2)],
  before, during, after: trace.getEnabledCategories() === undefined,
  errors: [err(() => trace.createTracing({ categories: [] })), err(() => trace.createTracing({})), err(() => trace.createTracing())],
};
"#)
    .await;
    assert_eq!(
        actual["sea"],
        json!([
            ["isSea", "getAsset", "getRawAsset", "getAssetAsBlob", "getAssetKeys"],
            false,
            [
                "Error",
                "ERR_NOT_IN_SINGLE_EXECUTABLE_APPLICATION",
                "Operation cannot be invoked when not in a single-executable application"
            ],
            ["ERR_NOT_IN_SINGLE_EXECUTABLE_APPLICATION"]
        ])
    );
    assert_eq!(actual["before"], json!([false, "node.perf,v8", null]));
    assert_eq!(
        actual["during"],
        json!([true, "node.perf,v8", "Tracing { enabled: true, categories: 'node.perf,v8' }"])
    );
    assert_eq!(actual["after"], true);
    assert_eq!(
        actual["errors"],
        json!([
            ["TypeError", "ERR_TRACE_EVENTS_CATEGORY_REQUIRED", "At least one category is required"],
            [
                "TypeError",
                "ERR_INVALID_ARG_TYPE",
                "The \"options.categories\" property must be an instance of Array. Received undefined"
            ],
            [
                "TypeError",
                "ERR_INVALID_ARG_TYPE",
                "The \"options\" argument must be of type object. Received undefined"
            ]
        ])
    );
}
