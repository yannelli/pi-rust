//! What pi-js does not provide from Node. Every listed module and API throws an `Error` with
//! code `ERR_PI_UNSUPPORTED` and the message "<api> is not supported in pi-rust".

use crate::resolve::NODE_BUILTINS;

/// Built-in modules that throw on `require` or `import` (with or without `node:`).
pub fn unsupported_modules() -> Vec<&'static str> {
    let supported: Vec<&str> = crate::node::supported_modules().collect();
    NODE_BUILTINS.iter().copied().filter(|name| !supported.contains(name)).collect()
}

/// Built-in modules that load, in Node's naming without the `node:` prefix.
pub fn supported_modules() -> Vec<&'static str> {
    crate::node::supported_modules().collect()
}

/// APIs inside supported modules that throw the unsupported error.
pub const UNSUPPORTED_APIS: &[&str] = &[
    "crypto: ciphers outside AES (CBC, CTR, ECB, CFB, OFB, GCM, CCM, OCB), DES, 3DES, and ChaCha20-Poly1305 \
     (for example aes-128-cfb8, aes-*-xts, camellia, bf, rc4), AES-GCM IVs longer than 16 bytes other than \
     32 or 64, AES-OCB IVs other than 8, 12, or 15 bytes and tags other than 8, 12, or 16, RSA-OAEP labels \
     that are not UTF-8, DSA sizes other than 1024/160, 2048/224, 2048/256, and 3072/256, private key encryption \
     with ciphers other than AES-CBC and 3DES-CBC (or PBES1), webcrypto.subtle except digest, setFips",
    "fs: lchmod, fchmod, fchown, lutimes, statfs, openAsBlob, FileHandle chmod, chown, and readableWebStream",
    "child_process: stdio entries that are streams or descriptors above 2, sending handles over IPC \
     (ERR_INVALID_HANDLE_TYPE)",
    "net: sockets and servers from a file descriptor or an existing handle, reusePort",
    "tls: TLSSocket over an existing socket or stream (and tls.connect's socket option), SNICallback, \
     addContext, pfx, encrypted private keys, session resumption, exportKeyingMaterial, and TLS before 1.2",
    "http: the insecureHTTPParser option (accepted and ignored)",
    "http2: server push (pushStream and createPushResponse fail with ERR_HTTP2_PUSH_DISABLED), \
     performServerHandshake, altsvc and origin frames (ignored), custom settings, and stream priority",
    "dgram: addSourceSpecificMembership, dropSourceSpecificMembership, and bind with a file descriptor",
    "process: binding, dlopen, setUncaughtExceptionCaptureCallback, and running code after process.exit when \
     the embedder's on_exit callback returns (the runtime is shutting down)",
    "stream: Duplex.from for promises and web streams; stream/web BYOB readers, \
     ReadableByteStreamController, CompressionStream, DecompressionStream",
    "v8: queryObjects",
    "vm: SourceTextModule, SyntheticModule, and Module (Node needs --experimental-vm-modules for them), \
     and breakOnSigint (accepted and ignored)",
    "worker_threads: worker.getHeapSnapshot, postMessageToThread, and resourceLimits other than the heap \
     and stack sizes (accepted)",
    "fetch: Body.formData for multipart bodies, request bodies are buffered before sending",
];

/// Supported behavior that differs from Node.
pub const DIFFERENCES: &[&str] = &[
    "stdio 'inherit' for a child's stdin gives it no input",
    "child_process.fork, and spawning process.execPath, run the script in this binary's JavaScript runner \
     (PI_JS_FORK); the IPC channel is a Unix socket or Windows named pipe, not an inherited descriptor",
    "tty.ReadStream works only for descriptor 0 (process.stdin); other descriptors fail with ERR_TTY_INIT_FAILED",
    "PerformanceObserver sees function, mark, and measure entries only",
    "child stdout and stderr with stdio 'inherit' go to the runtime's output sink, not the real terminal",
    "process.exit calls the embedder's on_exit callback when one is set",
    "os.cpus reports no CPU times; os.freemem and os.totalmem come from /proc, sysctl, or vm_stat",
    "fetch does not decompress gzip or brotli bodies and sends no accept-encoding header",
    "readline has no raw terminal editing: lines come from the input stream as text",
    "module.register hooks run on the importing thread: a hook's promise must settle from microtasks alone, \
     and hooks apply to import, not require",
    "async_hooks: createHook sees AsyncResource objects only, not timers, promises, or I/O; async context \
     follows awaits through a source rewrite, and an await with a store set takes one extra microtask",
    "v8: heap statistics come from QuickJS memory use, heap snapshots have no nodes, setFlagsFromString \
     and promiseHooks do nothing, and strings with only Latin-1 characters always count as one-byte",
    "dns: lookup filters getaddrinfo results by family and hints after the call, and every failure other \
     than a temporary one is ENOTFOUND; the resolve family uses hickory-resolver instead of c-ares, \
     never uses search domains, and Resolver.setLocalAddress does nothing",
    "tls: servers emit secureConnection but no connection event before the handshake; getCertificate \
     returns null and getX509Certificate and getPeerX509Certificate return undefined; ciphers, ecdhCurve, \
     and other OpenSSL tuning options are ignored",
    "http: the parser is httparse, so malformed messages can fail with a different HPE_* code than llhttp; \
     server headersTimeout and requestTimeout use per-connection timers instead of a periodic check, and \
     setMaxIdleHTTPParsers only validates its argument",
    "http2: sessions run on the h2 crate: ping sends an opaque payload and reports the caller's payload back, \
     settings after connect apply only initialWindowSize, remoteSettings shows defaults except \
     maxConcurrentStreams, a client's goaway sends no GOAWAY frame, received data is not flow-controlled \
     by the reader, and session and stream state objects are approximate",
    "net: connecting to several looked-up addresses tries them one after another, not in parallel",
    "vm: a context is a separate QuickJS context whose globals forward to the contextified object; \
     `this` and `globalThis` inside it are that context's own global, not the object, globals the code \
     declares are copied to the object when each run ends, cached data is accepted but not used, and \
     microtaskMode 'afterEvaluate' runs every pending microtask of the runtime after the script",
    "worker_threads: a SharedArrayBuffer is copied, not shared, because each worker is a separate QuickJS \
     runtime; a worker writes straight to the parent's output unless stdout or stderr is set; \
     MessagePort is an EventEmitter with addEventListener and onmessage rather than an EventTarget; \
     worker.performance reports no event loop use; moveMessagePortToContext returns the same port",
    "zlib: deflate output can differ in bytes from Node's zlib (zlib-rs); memLevel and strategy are \
     validated but not applied; brotli error codes come from the Rust decoder",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_unsupported_modules() {
        let unsupported = unsupported_modules();
        for name in ["inspector", "repl"] {
            assert!(unsupported.contains(&name), "{name}");
        }
        let supported = supported_modules();
        for name in [
            "fs",
            "fs/promises",
            "child_process",
            "crypto",
            "stream/web",
            "assert/strict",
            "readline",
            "zlib",
            "v8",
            "net",
            "http",
            "https",
            "http2",
            "vm",
            "worker_threads",
        ] {
            assert!(supported.contains(&name) && !unsupported.contains(&name), "{name}");
        }
    }
}
