mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..count]);
        if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..count]);
    }
    Some(Request { method, path, headers, body })
}

async fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n", body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
}

async fn handle(mut stream: TcpStream) {
    let Some(request) = read_request(&mut stream).await else { return };
    let header = |name: &str| request.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    match request.path.as_str() {
        "/text" => {
            let headers =
                [("content-type", "text/plain"), ("x-test", "1"), ("set-cookie", "a=1"), ("set-cookie", "b=2")];
            respond(&mut stream, "200 OK", &headers, b"hello").await;
        }
        "/echo" => {
            let body = json!({
                "method": request.method,
                "body": String::from_utf8_lossy(&request.body),
                "contentType": header("content-type"),
                "custom": header("x-custom"),
                "userAgent": header("user-agent"),
                "accept": header("accept"),
            });
            respond(&mut stream, "200 OK", &[("content-type", "application/json")], body.to_string().as_bytes()).await;
        }
        "/redirect" => respond(&mut stream, "302 Found", &[("location", "/text")], b"").await,
        "/stream" => {
            let head = "HTTP/1.1 200 OK\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n";
            let _ = stream.write_all(head.as_bytes()).await;
            for part in ["one,", "two,", "three"] {
                let _ = stream.write_all(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes()).await;
                let _ = stream.flush().await;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        }
        "/slow" => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            respond(&mut stream, "200 OK", &[], b"late").await;
        }
        _ => respond(&mut stream, "404 Not Found", &[], b"missing").await,
    }
}

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(handle(stream));
        }
    });
    format!("http://{address}")
}

const FETCH_SCRIPT: &str = r##"
const out = {};
const r = await fetch(BASE + "/text");
out.text = [r.status, r.ok, r.statusText, r.headers.get("content-type"), r.headers.get("x-test"), r.headers.getSetCookie(),
  await r.text(), r.bodyUsed, r.url === BASE + "/text", r.redirected, r.type];
const echo = await fetch(BASE + "/echo", { method: "post", headers: { "X-Custom": "yes" }, body: JSON.stringify({ a: 1 }) });
out.echo = await echo.json();
out.form = await (await fetch(BASE + "/echo", { method: "POST", body: new URLSearchParams({ q: "a b" }) })).json();
const form = new FormData();
form.append("field", "v");
form.append("file", new Blob(["data"], { type: "text/plain" }), "f.txt");
const multipart = await (await fetch(BASE + "/echo", { method: "POST", body: form })).json();
out.multipart = [multipart.contentType.startsWith("multipart/form-data; boundary="), multipart.body.includes('name="field"\r\n\r\nv\r\n'),
  multipart.body.includes('filename="f.txt"\r\nContent-Type: text/plain\r\n\r\ndata')];
const redirected = await fetch(BASE + "/redirect");
out.redirect = [redirected.status, redirected.redirected, redirected.url === BASE + "/text", await redirected.text()];
const manual = await fetch(BASE + "/redirect", { redirect: "manual" });
out.manual = [manual.status, manual.headers.get("location")];
const streamed = await fetch(BASE + "/stream");
const reader = streamed.body.getReader();
const chunks = [];
for (;;) {
  const { value, done } = await reader.read();
  if (done) break;
  chunks.push(new TextDecoder().decode(value));
}
out.stream = [chunks.join(""), chunks.length > 1];
const controller = new AbortController();
const pending = fetch(BASE + "/slow", { signal: controller.signal });
setTimeout(() => controller.abort(), 20);
try { await pending; } catch (e) { out.abort = [e.name, e.code]; }
try { await fetch(REFUSED); } catch (e) { out.refused = [e.name, e.message, e.cause.code]; }
try { await fetch("not a url"); } catch (e) { out.badUrl = [e.name, e.message]; }
out.data = await (await fetch("data:text/plain;base64,aGk=")).text();
const response = new Response("x", { status: 201, headers: { a: "b" } });
out.response = [response.status, response.headers.get("a"), await response.clone().text(), await response.text()];
out.json = await Response.json({ k: 1 }).json();
const request = new Request(BASE + "/echo", { method: "PUT", body: "z" });
out.request = [request.method, request.url.endsWith("/echo"), await request.clone().text(), await request.text()];
out.headers = [...new Headers([["B", "2"], ["a", "1"], ["a", "3"]])];
let decoded = "";
for await (const piece of (await fetch(BASE + "/text")).body.pipeThrough(new TextDecoderStream())) decoded += piece;
out.piped = decoded;
const blob = await (await fetch(BASE + "/text")).blob();
out.blob = [blob.size, blob.type];
out.notFound = [(await fetch(BASE + "/nope")).status, (await fetch(BASE + "/nope")).ok];
return out;
"##;

#[tokio::test]
async fn fetch_works_against_a_local_server() {
    let base = serve().await;
    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let script = format!("const BASE = {base:?};\nconst REFUSED = {refused:?};\n{FETCH_SCRIPT}");
    let actual = run_cjs(&runtime, &root, &script).await;
    let expected = json!({
        "text": [200, true, "OK", "text/plain", "1", ["a=1", "b=2"], "hello", true, true, false, "basic"],
        "echo": { "method": "POST", "body": "{\"a\":1}", "contentType": "text/plain;charset=UTF-8", "custom": "yes", "userAgent": "node", "accept": "*/*" },
        "form": { "method": "POST", "body": "q=a+b", "contentType": "application/x-www-form-urlencoded;charset=UTF-8", "custom": null, "userAgent": "node", "accept": "*/*" },
        "multipart": [true, true, true],
        "redirect": [200, true, true, "hello"],
        "manual": [302, "/text"],
        "stream": ["one,two,three", true],
        "abort": ["AbortError", 20],
        "refused": ["TypeError", "fetch failed", "ECONNREFUSED"],
        "badUrl": ["TypeError", "Failed to parse URL from not a url"],
        "data": "hi",
        "response": [201, "b", "x", "x"],
        "json": { "k": 1 },
        "request": ["PUT", true, "z", "z"],
        "headers": [["a", "1, 3"], ["b", "2"]],
        "piped": "hello",
        "blob": [5, "text/plain"],
        "notFound": [404, false],
    });
    assert_eq!(actual, expected);
}
