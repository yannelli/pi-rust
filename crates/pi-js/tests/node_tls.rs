mod common;

use common::*;

// A self-signed P-256 certificate for localhost and 127.0.0.1, valid 2024 to 2124, made with
// `openssl req -x509` for this test. Expected values are what Node 22 prints.
const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg64px2ljToEp/t1Ym\nD+ubiZ99rB2l1SEkOI+aLR97TKehRANCAAQ84xgYgunffWWwvLaOfO8dbOATZqug\nO6FUlDKGfsDWZut72xTosaRA95OR/BAJlJ8prJYPsL3DXtJSMltLyhxb\n-----END PRIVATE KEY-----";
const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIB4zCCAYmgAwIBAgIUZiUWvZqXI7NigfNonKV2msHpJikwCgYIKoZIzj0EAwIw\nKTESMBAGA1UEAwwJbG9jYWxob3N0MRMwEQYDVQQKDApwaS1qcyB0ZXN0MCAXDTI0\nMDEwMTAwMDAwMFoYDzIxMjQwMTAxMDAwMDAwWjApMRIwEAYDVQQDDAlsb2NhbGhv\nc3QxEzARBgNVBAoMCnBpLWpzIHRlc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNC\nAAQ84xgYgunffWWwvLaOfO8dbOATZqugO6FUlDKGfsDWZut72xTosaRA95OR/BAJ\nlJ8prJYPsL3DXtJSMltLyhxbo4GMMIGJMB0GA1UdDgQWBBQ/YkByXucdZQDqX4xT\ncanf3KfrCDAfBgNVHSMEGDAWgBQ/YkByXucdZQDqX4xTcanf3KfrCDAaBgNVHREE\nEzARgglsb2NhbGhvc3SHBH8AAAEwDAYDVR0TAQH/BAIwADAdBgNVHSUEFjAUBggr\nBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZIzj0EAwIDSAAwRQIgVwoYZAHdSV0NY76/\nQrgGkefE7StUTS9udFUgj9XCxIACIQCT6yajiEoiwiY2XSRtNjv5wooePns3Ap2B\neBwd6BBY7A==\n-----END CERTIFICATE-----";

const TLS_SCRIPT: &str = r##"
const tls = require("node:tls");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const serverSide = [];
const server = tls.createServer({ key: __KEY__, cert: __CERT__, ALPNProtocols: ["h2", "http/1.1"] }, (socket) => {
  serverSide.push([socket.encrypted, socket.alpnProtocol, socket.servername, socket.getProtocol()]);
  socket.on("data", (chunk) => socket.write(chunk));
  socket.on("end", () => socket.end());
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;
const client = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: __CERT__, ALPNProtocols: ["http/1.1"] });
await once(client, "secureConnect");
out.trusted = [client.authorized, client.authorizationError, client.alpnProtocol, client.encrypted, client.getProtocol(), client.getCipher().version, typeof client.getCipher().name];
const cert = client.getPeerCertificate();
out.cert = [cert.subject, cert.issuer, cert.subjectaltname, cert.valid_from, cert.valid_to, cert.fingerprint256, cert.serialNumber, Buffer.isBuffer(cert.raw)];
client.setEncoding("utf8");
let text = "";
client.on("data", (chunk) => (text += chunk));
client.end("secret");
await once(client, "close");
out.echo = text;
const rejected = tls.connect({ port, host: "127.0.0.1", servername: "localhost" });
const error = await once(rejected, "error");
out.rejected = [error.code, error.message];
const loose = tls.connect({ port, host: "127.0.0.1", servername: "localhost", rejectUnauthorized: false });
await once(loose, "secureConnect");
out.loose = [loose.authorized, loose.authorizationError];
loose.destroy();
const wrongName = tls.connect({ port, host: "127.0.0.1", servername: "example.com", ca: __CERT__ });
const mismatch = await once(wrongName, "error");
out.mismatch = [mismatch.code, mismatch.reason];
out.identity = [
  tls.checkServerIdentity("localhost", { subjectaltname: "DNS:localhost" }),
  tls.checkServerIdentity("a.example.com", { subjectaltname: "DNS:*.example.com" }),
  tls.checkServerIdentity("x.y.example.com", { subjectaltname: "DNS:*.example.com" }).code,
];
out.constants = [tls.DEFAULT_MIN_VERSION, tls.DEFAULT_MAX_VERSION, tls.DEFAULT_ECDH_CURVE, tls.getCiphers().includes("tls_aes_128_gcm_sha256"), tls.rootCertificates.length > 100, tls.rootCertificates[0].startsWith("-----BEGIN CERTIFICATE-----")];
out.context = tls.createSecureContext({ cert: __CERT__, key: __KEY__ }) instanceof tls.SecureContext;
server.close();
await once(server, "close");
out.server = serverSide[0];
return out;
"##;

const TLS_EXPECTED: &str = r##"{"trusted":[true,null,"http/1.1",true,"TLSv1.3","TLSv1.3","string"],"cert":[{"CN":"localhost","O":"pi-js test"},{"CN":"localhost","O":"pi-js test"},"DNS:localhost, IP Address:127.0.0.1","Jan  1 00:00:00 2024 GMT","Jan  1 00:00:00 2124 GMT","FD:FE:EA:F3:65:F3:3C:07:B3:AB:3D:F9:1C:E8:0B:0E:57:84:A4:DC:8B:AD:CD:C9:56:1E:50:88:DC:42:A8:07","662516BD9A9723B36281F3689CA5769AC1E92629",true],"echo":"secret","rejected":["DEPTH_ZERO_SELF_SIGNED_CERT","self-signed certificate"],"loose":[false,"DEPTH_ZERO_SELF_SIGNED_CERT"],"mismatch":["ERR_TLS_CERT_ALTNAME_INVALID","Host: example.com. is not in the cert's altnames: DNS:localhost, IP Address:127.0.0.1"],"identity":[null,null,"ERR_TLS_CERT_ALTNAME_INVALID"],"constants":["TLSv1.2","TLSv1.3","auto",true,true,true],"context":true,"server":[true,"http/1.1","localhost","TLSv1.3"]}"##;

#[tokio::test]
async fn tls_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let script = TLS_SCRIPT.replace("__KEY__", &format!("{KEY:?}")).replace("__CERT__", &format!("{CERT:?}"));
    let actual = run_cjs(&runtime, &root, &script).await;
    assert_same_keys(&actual, &serde_json::from_str(TLS_EXPECTED).unwrap());
    let uncaught = capture.uncaught.lock().unwrap();
    assert!(
        uncaught.is_empty(),
        "{:?}",
        uncaught.iter().map(|(_, error)| error.display_with_stack()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn tls_sockets_sessions_and_sni_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let pem = |text: &str| format!("{text:?}");
    let script = include_str!("fixtures/node_tls/features.js")
        .replace("__SNI_KEY__", &pem(include_str!("fixtures/node_tls/sni_key.pem")))
        .replace("__SNI_CERT__", &pem(include_str!("fixtures/node_tls/sni_cert.pem")))
        .replace("__KEY__", &pem(include_str!("fixtures/node_http/key.pem")))
        .replace("__CERT__", &pem(include_str!("fixtures/node_http/cert.pem")));
    let actual = run_cjs_within(&runtime, &root, &script, 120).await;
    assert_same_keys(&actual, &serde_json::from_str(include_str!("fixtures/node_tls/features.json")).unwrap());
    let uncaught = capture.uncaught.lock().unwrap();
    assert!(
        uncaught.is_empty(),
        "{:?}",
        uncaught.iter().map(|(_, error)| error.display_with_stack()).collect::<Vec<_>>()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tls_over_a_unix_socket_closes_without_a_reset() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let script = include_str!("fixtures/node_tls/unix_close.js")
        .replace("__KEY__", &format!("{:?}", include_str!("fixtures/node_http/key.pem")))
        .replace("__CERT__", &format!("{:?}", include_str!("fixtures/node_http/cert.pem")));
    let actual = run_cjs_within(&runtime, &root, &script, 120).await;
    assert_same_keys(&actual, &serde_json::from_str(include_str!("fixtures/node_tls/unix_close.json")).unwrap());
    assert!(capture.uncaught.lock().unwrap().is_empty());
}
