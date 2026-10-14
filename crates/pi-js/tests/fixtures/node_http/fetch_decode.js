const http = require("node:http");
const zlib = require("node:zlib");
const text = "hello compressed world ";
const large = Buffer.from(text.repeat(20000));
const bodies = {
  gzip: ["gzip", zlib.gzipSync(text)],
  xgzip: ["x-gzip", zlib.gzipSync(text)],
  deflate: ["deflate", zlib.deflateSync(text)],
  raw: ["deflate", zlib.deflateRawSync(text)],
  br: ["br", zlib.brotliCompressSync(text)],
  upper: ["GZIP", zlib.gzipSync(text)],
  multi: ["gzip, br", zlib.brotliCompressSync(zlib.gzipSync(text))],
  unknown: ["gzip, zstd", Buffer.from(text)],
  bad: ["gzip", Buffer.from("not gzip at all")],
  truncated: ["gzip", zlib.gzipSync(text).subarray(0, -8)],
  large: ["gzip", zlib.gzipSync(large)],
  none: [null, Buffer.from(text)],
};
const seen = {};
const server = http.createServer((req, res) => {
  const name = req.url.slice(1);
  seen[name] = req.headers["accept-encoding"] || null;
  if (name === "nocontent") {
    res.writeHead(204, { "content-encoding": "gzip" });
    return res.end();
  }
  const [coding, body] = bodies[name] || bodies.gzip;
  const headers = { "content-type": "text/plain" };
  if (coding) headers["content-encoding"] = coding;
  if (name === "large") {
    res.writeHead(200, headers);
    for (let i = 0; i < body.length; i += 1000) res.write(body.subarray(i, i + 1000));
    return res.end();
  }
  headers["content-length"] = body.length;
  res.writeHead(200, headers);
  res.end(req.method === "HEAD" ? undefined : body);
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const out = {};
for (const name of ["gzip", "xgzip", "deflate", "raw", "br", "upper", "multi", "unknown", "truncated", "none"]) {
  const response = await fetch(`${base}/${name}`);
  out[name] = [response.headers.get("content-encoding"), response.headers.get("content-length") !== null, await response.text()];
}
const decoded = Buffer.from(await (await fetch(`${base}/large`)).arrayBuffer());
out.large = [decoded.length, decoded.equals(large)];
try {
  await (await fetch(`${base}/bad`)).text();
  out.bad = "no error";
} catch (error) {
  out.bad = [error.name, error.message, error.cause instanceof Error, error.cause.code, error.cause.errno, error.cause.message];
}
const head = await fetch(`${base}/gzip`, { method: "HEAD" });
out.head = [head.status, head.headers.get("content-encoding"), await head.text()];
const empty = await fetch(`${base}/nocontent`);
out.nocontent = [empty.status, await empty.text()];
await (await fetch(`${base}/custom`, { headers: { "Accept-Encoding": "identity" } })).text();
out.seen = [seen.gzip, seen.custom];
server.close();
return out;
