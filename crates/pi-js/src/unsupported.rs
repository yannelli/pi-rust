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
    "crypto: createCipheriv, createDecipheriv, createSign, createVerify, sign, verify, key pairs and key objects \
     beyond createSecretKey, Diffie-Hellman and ECDH, public and private encrypt, scrypt, hkdf, primes, \
     webcrypto.subtle, setFips",
    "fs: glob, lchmod, fchmod, fchown, lutimes, statfs, openAsBlob, bigint Stats, cp with a filter, \
     FileHandle chmod, chown, and readableWebStream; reading descriptor 0",
    "child_process: fork, IPC (send and disconnect), stdio entries that are streams or descriptors above 2",
    "process: reading stdin, stdin.setRawMode, cpuUsage, resourceUsage, abort, binding, dlopen, \
     setUncaughtExceptionCaptureCallback",
    "os: networkInterfaces, getPriority, setPriority",
    "stream: compose, Duplex.from for values that are not iterables; stream/web BYOB readers, \
     ReadableByteStreamController, CompressionStream, DecompressionStream",
    "fetch: Body.formData for multipart bodies, request bodies are buffered before sending",
    "util: parseArgs, getSystemErrorName, getSystemErrorMap, MIMEType, transferableAbortController, types.isProxy",
    "module: register, findSourceMap, require.extensions",
    "perf_hooks: PerformanceObserver, timerify, eventLoopUtilization, monitorEventLoopDelay",
    "misc: path.matchesGlob, buffer.transcode, buffer.resolveObjectURL, URL.createObjectURL, \
     URL.revokeObjectURL, assert.CallTracker, events.EventEmitterAsyncResource, tty.ReadStream, \
     diagnostics_channel.tracingChannel and bindStore",
];

/// Supported behavior that differs from Node.
pub const DIFFERENCES: &[&str] = &[
    "fs.watch polls every 100 ms (or options.interval) and fs.watchFile every 5007 ms by default",
    "process.stdin is empty, and stdio 'inherit' for a child's stdin gives it no input",
    "child stdout and stderr with stdio 'inherit' go to the runtime's output sink, not the real terminal",
    "process.exit calls the embedder's on_exit callback when one is set",
    "os.cpus reports no CPU times; os.freemem and os.totalmem come from /proc, sysctl, or vm_stat",
    "fetch does not decompress gzip or brotli bodies and sends no accept-encoding header",
    "readline has no raw terminal editing: lines come from the input stream as text",
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
