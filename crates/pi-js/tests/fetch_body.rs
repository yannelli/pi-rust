//! Body.formData for multipart and urlencoded bodies, and fetch request bodies sent as they are
//! read. Expected values come from Node 22; the server is node:http on localhost.

mod common;

use common::*;

const FORM_SCRIPT: &str = r##"
const out = {};
const show = async (form) => {
  const entries = [];
  for (const [name, value] of form) entries.push(typeof value === "string" ? [name, value] : [name, value.constructor.name, value.name, value.type, value.size, await value.text()]);
  return entries;
};
  const fd = new FormData();
  fd.append("text", "héllo\nworld");
  fd.append("file", new Blob(["file body"], { type: "text/x-custom" }), "a.txt");
  fd.append("bin", new Blob([new Uint8Array([0, 255, 13, 10])]), "b.bin");
  fd.append("weird\"name", "v");
  const req = new Request("http://x/", { method: "POST", body: fd });
  out.roundTrip = await show(await req.formData());
  const raw = '--XyZ\r\nContent-Disposition: form-data; name="a"\r\n\r\n1\r\n--XyZ\r\nContent-Disposition: form-data; name="f"; filename="x.json"\r\nContent-Type: application/json\r\n\r\n{"k":1}\r\n--XyZ\r\nContent-Disposition: form-data; name="nofiletype"; filename="y"\r\n\r\nzz\r\n--XyZ\r\nContent-Disposition: form-data; name="b"\r\nContent-Type: text/plain\r\n\r\ntwo\r\nlines\r\n--XyZ--\r\n';
  out.raw = await show(await new Response(raw, { headers: { "content-type": 'multipart/form-data; boundary="XyZ"' } }).formData());
  out.urlencoded = await show(await new Response("a=1&b=x+y&a=%C3%A9", { headers: { "content-type": "application/x-www-form-urlencoded;charset=UTF-8" } }).formData());
  for (const [key, type, body] of [["noBoundary", "multipart/form-data", "x"], ["badType", "text/plain", "a=1"], ["noType", null, "a=1"], ["truncated", "multipart/form-data; boundary=b", "--b\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\nv"], ["noDisposition", "multipart/form-data; boundary=b", "--b\r\nX: y\r\n\r\nv\r\n--b--"], ["preamble", "multipart/form-data; boundary=b", "x\r\n--b\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\nv\r\n--b--"], ["unquoted", "multipart/form-data; boundary=b", "--b\r\nContent-Disposition: form-data; name=q\r\n\r\nv\r\n--b--"], ["crlfAround", "Multipart/Form-Data; Boundary=b", "\r\n--b\r\ncontent-disposition: form-data; name=\"q\"; filename=\"a%22b\"\r\ncontent-transfer-encoding: base64\r\n\r\ndg==\r\n--b--\r\n\r\n"], ["emptyForm", "multipart/form-data; boundary=b", "--b--"]]) {
    try {
      const headers = type ? { "content-type": type } : {};
      const r = new Response(body, { headers });
      if (!type) r.headers.delete("content-type");
      out[key] = await show(await r.formData());
    } catch (e) { out[key] = [e.name, e.message]; }
  }
  try { await new Request("http://x/", { method: "POST", body: new ReadableStream() }); } catch (e) { out.duplex = [e.name, e.message]; }
  return out;
"##;

const FORM_EXPECTED: &str = r##"{"roundTrip":[["text","héllo\r\nworld"],["file","File","a.txt","text/x-custom",9,"file body"],["bin","File","b.bin","application/octet-stream",4,"\u0000�\r\n"],["weird\"name","v"]],"raw":[["a","1"],["f","File","x.json","application/json",7,"{\"k\":1}"],["nofiletype","File","y","text/plain",2,"zz"],["b","two\r\nlines"]],"urlencoded":[["a","1"],["b","x y"],["a","é"]],"noBoundary":["TypeError","Failed to parse body as FormData."],"badType":["TypeError","Content-Type was not one of \"multipart/form-data\" or \"application/x-www-form-urlencoded\"."],"noType":["TypeError","Content-Type was not one of \"multipart/form-data\" or \"application/x-www-form-urlencoded\"."],"truncated":["TypeError","Failed to parse body as FormData."],"noDisposition":["TypeError","Failed to parse body as FormData."],"preamble":["TypeError","Failed to parse body as FormData."],"unquoted":["TypeError","Failed to parse body as FormData."],"crlfAround":[["q","File","a\"b","text/plain",1,"v"]],"emptyForm":[],"duplex":["TypeError","RequestInit: duplex option is required when sending a body."]}"##;

#[tokio::test]
async fn form_data_parsing_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, FORM_SCRIPT, 30).await;
    assert_same_keys(&actual, &serde_json::from_str(FORM_EXPECTED).unwrap());
}

// The second chunk waits until the server has the first, so a buffered body would hang.
const STREAM_SCRIPT: &str = r##"
const http = require("http");
const out = {};
let firstChunk;
const gotFirst = new Promise((resolve) => (firstChunk = resolve));
const server = http.createServer((req, res) => {
  const chunks = [];
  out.headers = [req.headers["transfer-encoding"], req.headers["content-length"], req.headers["content-type"]];
  req.on("data", (chunk) => { chunks.push(chunk.toString()); firstChunk(); });
  req.on("end", () => res.end(JSON.stringify(chunks.join(""))));
  req.on("error", () => {});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}/`;
  let step = 0;
  const body = new ReadableStream({
    async pull(controller) {
      if (step === 0) controller.enqueue(new TextEncoder().encode("first,"));
      else if (step === 1) { await gotFirst; controller.enqueue(new TextEncoder().encode("second")); }
      else controller.close();
      step++;
    },
  });
  const response = await fetch(url, { method: "POST", body, duplex: "half", headers: { "content-type": "text/plain" } });
  out.echo = await response.json();
  async function* gen() { yield new TextEncoder().encode("a"); yield new TextEncoder().encode("b"); }
  out.iterable = await (await fetch(url, { method: "PUT", body: gen(), duplex: "half" })).json();
  out.blob = await (await fetch(url, { method: "POST", body: new Blob(["blob!"]) })).json();
  out.blobHeaders = out.headers;
  try { await fetch(url, { method: "POST", body: new ReadableStream({ pull(c) { c.error(new Error("boom")); } }), duplex: "half" }); } catch (e) { out.errored = [e.name, e.message, e.cause && e.cause.message]; }
  try { new Request(url, { method: "POST", body: new ReadableStream() }); } catch (e) { out.noDuplex = [e.name, e.message]; }
  const used = new Request(url, { method: "POST", body: new ReadableStream({ start(c) { c.enqueue(new Uint8Array([104, 105])); c.close(); } }), duplex: "half" });
  out.echoRequest = await (await fetch(used)).json();
  out.bodyUsed = used.bodyUsed;
  server.close();
  return out;
"##;

const STREAM_EXPECTED: &str = r##"{"headers":["chunked",null,null],"echo":"first,second","iterable":"ab","blob":"blob!","blobHeaders":[null,"5",null],"errored":["TypeError","fetch failed","boom"],"noDuplex":["TypeError","RequestInit: duplex option is required when sending a body."],"echoRequest":"hi","bodyUsed":true}"##;

#[tokio::test]
async fn fetch_streams_request_bodies_like_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, STREAM_SCRIPT, 60).await;
    assert_same_keys(&actual, &serde_json::from_str(STREAM_EXPECTED).unwrap());
}
