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
     (ERR_INVALID_HANDLE_TYPE, as there is no net module)",
    "process: binding, dlopen, setUncaughtExceptionCaptureCallback, and running code after process.exit when \
     the embedder's on_exit callback returns (the runtime is shutting down)",
    "stream: Duplex.from for promises and web streams; stream/web BYOB readers, \
     ReadableByteStreamController, CompressionStream, DecompressionStream",
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
    "events.EventEmitterAsyncResource and diagnostics_channel stores run without async_hooks contexts",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_unsupported_modules() {
        let unsupported = unsupported_modules();
        for name in ["http", "https", "net", "zlib", "worker_threads", "vm"] {
            assert!(unsupported.contains(&name), "{name}");
        }
        let supported = supported_modules();
        for name in ["fs", "fs/promises", "child_process", "crypto", "stream/web", "assert/strict", "readline"] {
            assert!(supported.contains(&name) && !unsupported.contains(&name), "{name}");
        }
    }
}
