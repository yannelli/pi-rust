//! What pi-js does not provide from Node on the QuickJS backend. Every listed module and API
//! throws an `Error` with code `ERR_PI_UNSUPPORTED` and the message "<api> is not supported in
//! pi-rust". The Node backend ([`crate::node_backend`]) runs real Node and has none of these gaps.

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
    "child_process: stdio entries that are Unix socket or pipe streams, descriptors 0 to 2 at an index above \
     2, and on Windows sockets and anything but 'ignore' above index 2; sending handles other than TCP \
     net.Socket and net.Server objects over IPC (ERR_INVALID_HANDLE_TYPE), such as Unix socket or pipe \
     servers, TLS sockets, and dgram sockets",
    "cluster: workers sharing Unix socket or named pipe servers, and TLS servers (each worker listens on its own)",
    "net: sockets and servers from a file descriptor or an existing handle, and reusePort outside Linux \
     (ENOTSUP, as in libuv)",
    "tls: pfx (no PKCS#12 parser is bundled), encrypted private keys, exportKeyingMaterial, and TLS before 1.2",
    "http: the insecureHTTPParser option (accepted and ignored)",
    "http2: performServerHandshake, altsvc and origin frames (ignored), and stream priority (h2 sends no \
     PRIORITY frames)",
    "dgram: IPv6 source-specific membership on Windows, and bind with a file descriptor",
    "process: binding, dlopen (native .node addons; supported on the Node backend), \
     setUncaughtExceptionCaptureCallback, and running code after process.exit when the embedder's on_exit \
     callback returns (the runtime is shutting down)",
    "v8: queryObjects",
    "vm: SourceTextModule, SyntheticModule, and Module (Node needs --experimental-vm-modules for them), \
     and breakOnSigint (accepted and ignored)",
    "worker_threads: worker.getHeapSnapshot, postMessageToThread, and resourceLimits other than the heap \
     and stack sizes (accepted)",
    "sqlite: createSession, applyChangeset, enableLoadExtension(true), and loadExtension (the bundled SQLite \
     has no session extension, and native extensions do not load)",
    "test: coverage (the lcov reporter)",
    "inspector: open (there is no DevTools server to attach to)",
    "WebAssembly: shared memories (Memory with shared: true, and modules using threads and atomics fail to \
     compile), and exception handling (Tag and Exception throw, JSTag is absent, and modules using it fail \
     to compile)",
];

/// Supported behavior that differs from Node.
pub const DIFFERENCES: &[&str] = &[
    "child_process: when the embedder sets an output sink, stdio 'inherit' sends a child's stdout and \
     stderr to the sink instead of the real terminal and gives its stdin no input (with the default \
     output, children inherit the real descriptors)",
    "child_process: spawnSync treats a Buffer or string stdio entry as 'pipe' (Node drops the entry and \
     shifts the later ones down)",
    "child_process.fork, and spawning process.execPath, run the script in this binary's JavaScript runner \
     (PI_JS_FORK) on the QuickJS backend, with IPC over a Unix socket or Windows named pipe; the Node \
     backend runs them on real Node",
    "fs.openAsBlob reads the whole file when it is called, so later changes to the file do not show \
     in the Blob (Node reads it lazily and fails the read if the file changed)",
    "CompressionStream and DecompressionStream reject a chunk that is not a string, TypedArray, DataView, \
     or ArrayBuffer (ERR_INVALID_ARG_TYPE, null as ERR_STREAM_NULL_VALUES) and accept an ArrayBuffer, as \
     Node 24 does; Node 22 leaves the write of such a chunk, and of an ArrayBuffer, pending",
    "tty.ReadStream works only for descriptor 0 (process.stdin); other descriptors fail with ERR_TTY_INIT_FAILED",
    "PerformanceObserver sees function, mark, and measure entries only",
    "process.exit calls the embedder's on_exit callback when one is set",
    "os.freemem and os.totalmem come from /proc, sysctl, or vm_stat; os.cpus reports zero times on \
     platforms other than Linux, macOS, and Windows",
    "readline: terminal mode has no tab completion, kill ring (Ctrl+Y), undo (Ctrl+_), or Ctrl+Z \
     suspend, and display widths come from Node's fallback width tables rather than ICU",
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
     and other OpenSSL tuning options are ignored; a session Buffer is an opaque handle that works only in \
     this process, as rustls cannot serialize sessions; SNICallback gets the server as `this` and also \
     runs for handshakes that resume a session",
    "http: the parser is httparse, so malformed messages can fail with a different HPE_* code than llhttp; \
     server headersTimeout and requestTimeout use per-connection timers instead of a periodic check, and \
     setMaxIdleHTTPParsers only validates its argument",
    "http2: sessions run on the h2 crate: ping sends an opaque payload and reports the caller's payload back, \
     settings after connect send only initialWindowSize and customSettings, the custom ones in a \
     SETTINGS frame of their own, a client's goaway sends no GOAWAY frame, received \
     data is not flow-controlled by the reader, closing a pushed stream on the client sends no RST_STREAM, \
     and session and stream state objects are approximate",
    "child_process: messages sent after a handle do not wait for the peer's NODE_HANDLE_ACK (Node queues \
     them, and resends the handle after a NODE_HANDLE_NACK), and bytes that a sent socket had read in the \
     sender but not yet emitted as 'data' are dropped instead of emitted there",
    "dgram: addMembership, dropMembership, and the source-specific variants need a bound socket (Node \
     binds an unbound one to a random port first)",
    "cluster: round-robin hands each connection to the next connected worker in turn rather than to the \
     next free one, and the primary does not report worker server errors with Node's exact errno text",
    "vm: a context is a separate QuickJS context whose globals forward to the contextified object; \
     `this` and `globalThis` inside it are that context's own global, not the object, globals the code \
     declares are copied to the object when each run ends, cached data is accepted but not used, and \
     microtaskMode 'afterEvaluate' runs every pending microtask of the runtime after the script",
    "worker_threads: a SharedArrayBuffer shares its memory with workers, but growing it changes its \
     byteLength only in the thread that grows it; a worker writes straight to the parent's output unless \
     stdout or stderr is set; \
     MessagePort is an EventEmitter with addEventListener and onmessage rather than an EventTarget; \
     worker.performance reports no event loop use; moveMessagePortToContext returns the same port",
    "zlib: deflate output can differ in bytes from Node's zlib (zlib-rs); memLevel and strategy are \
     validated but not applied; brotli error codes come from the Rust decoder",
    "sqlite: all(), get(), and run() prepare a StatementSync's SQL through the connection's statement \
     cache on each call, and iterate() prepares its own copy that the statement's iterators share",
    "test: each result prints as soon as its test finishes, so console output from tests interleaves with \
     the report differently than in Node; subtests run one at a time (concurrency is accepted and \
     ignored); --test-only, --experimental-test-coverage, and module mocks are not available; run() loads \
     test files in this runtime instead of child processes, so an ES module file runs only on its first \
     load; t.filePath, and so the default snapshot file, is the file that declared the test rather than \
     the entry file; the spec reporter prints no colors; source locations come from QuickJS stack frames, whose \
     columns can differ from V8's",
    "repl: terminal mode edits lines through readline but has no completion, previews, or colors, and \
     there is no history file or .editor mode; top-level await keeps a single const, let, or var declaration per line visible afterwards; \
     lines that arrive during an asynchronous evaluation wait for it; the context is a vm context, so \
     globalThis inside it is not the context object (see vm), and error messages come from QuickJS",
    "inspector: sessions run in this runtime and cover Runtime (evaluation, remote objects, properties, \
     calls, console notifications), HeapProfiler (an empty snapshot), Schema, and NodeRuntime; Debugger, \
     Profiler, and Network methods fail with error -32601 because QuickJS has no V8 inspector; console \
     messages from before Runtime.enable are not replayed; url() returns undefined",
    "WebAssembly: modules run on the wasmi interpreter, which is much slower than V8 and also accepts \
     memory64; messages for compile errors (other than bad headers), instantiation failures, and \
     float-to-integer overflow traps use wasmi's wording; error stacks have no Wasm frames; modules, \
     instances, and values passed as externref stay alive until the runtime ends",
    "wasi: the system calls are JavaScript over the fs module: symbolic links inside a preopened \
     directory are followed without a sandbox check, poll_oneoff reports descriptors ready at once and \
     blocks the thread for clock subscriptions, proc_raise returns ENOSYS, sockets are not available, \
     stdio descriptors report the rights of a character device when the runtime's output is not a real \
     descriptor, and returnOnExit: false exits through process.exit (so 'exit' listeners run)",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_unsupported_modules() {
        let unsupported = unsupported_modules();
        assert!(unsupported.is_empty(), "{unsupported:?}");
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
            "cluster",
            "sqlite",
            "test",
            "test/reporters",
            "repl",
            "inspector",
            "inspector/promises",
            "wasi",
        ] {
            assert!(supported.contains(&name) && !unsupported.contains(&name), "{name}");
        }
    }
}
