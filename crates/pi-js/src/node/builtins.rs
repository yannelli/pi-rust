//! JavaScript sources of the built-in modules, compiled on first `require`.

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Function, Object};

use super::set_fn;
use crate::error::JsError;
use crate::resolve::{NODE_BUILTINS, PREFIX_ONLY};
use crate::runtime::convert::throw;

/// `(name, source)`. Names are Node's without `node:`; `internal/` names are private helpers.
const SOURCES: &[(&str, &str)] = &[
    ("pi:host", include_str!("../js/host.js")),
    ("path", include_str!("../js/path.js")),
    ("internal/path_win32", include_str!("../js/path_win32.js")),
    ("path/posix", "module.exports = require('path').posix;"),
    ("path/win32", "module.exports = require('path').win32;"),
    ("events", include_str!("../js/events.js")),
    ("internal/inspect", include_str!("../js/inspect.js")),
    ("internal/deep_equal", include_str!("../js/deep_equal.js")),
    ("internal/errors", include_str!("../js/errors.js")),
    ("internal/parse_args", include_str!("../js/parse_args.js")),
    ("internal/mime", include_str!("../js/mime.js")),
    ("internal/glob", include_str!("../js/glob.js")),
    ("util", include_str!("../js/util.js")),
    ("util/types", "module.exports = require('util').types;"),
    ("sys", "module.exports = require('util');"),
    ("timers", include_str!("../js/timers.js")),
    ("timers/promises", "module.exports = require('timers').promises;"),
    ("process", include_str!("../js/process.js")),
    ("console", include_str!("../js/console.js")),
    ("internal/module_hooks", include_str!("../js/module_hooks.js")),
    ("module", include_str!("../js/module.js")),
    ("internal/histogram", include_str!("../js/histogram.js")),
    ("perf_hooks", include_str!("../js/perf_hooks.js")),
    ("buffer", include_str!("../js/buffer.js")),
    ("string_decoder", include_str!("../js/string_decoder.js")),
    ("querystring", include_str!("../js/querystring.js")),
    ("url", include_str!("../js/url.js")),
    ("internal/web", include_str!("../js/web.js")),
    ("internal/intl", include_str!("../js/intl.js")),
    ("internal/streams_common", include_str!("../js/streams_common.js")),
    ("internal/streams_readable", include_str!("../js/streams_readable.js")),
    ("internal/streams_writable", include_str!("../js/streams_writable.js")),
    ("internal/streams_pipe", include_str!("../js/streams_pipe.js")),
    ("internal/streams_duplexify", include_str!("../js/streams_duplexify.js")),
    ("stream", include_str!("../js/stream.js")),
    ("stream/promises", "module.exports = require('stream').promises;"),
    ("os", include_str!("../js/os.js")),
    ("internal/crypto_keys", include_str!("../js/crypto_keys.js")),
    ("internal/crypto_extra", include_str!("../js/crypto_extra.js")),
    ("crypto", include_str!("../js/crypto.js")),
    ("internal/fs_utils", include_str!("../js/fs_utils.js")),
    ("internal/fs_ops", include_str!("../js/fs_ops.js")),
    ("internal/fs_streams", include_str!("../js/fs_streams.js")),
    ("fs", include_str!("../js/fs.js")),
    ("fs/promises", include_str!("../js/fs_promises.js")),
    ("readline", include_str!("../js/readline.js")),
    ("readline/promises", include_str!("../js/readline_promises.js")),
    ("internal/ipc", include_str!("../js/ipc.js")),
    ("child_process", include_str!("../js/child_process.js")),
    ("internal/streams_web", include_str!("../js/streams_web.js")),
    ("internal/streams_web_bytes", include_str!("../js/streams_web_bytes.js")),
    ("internal/streams_web_writable", include_str!("../js/streams_web_writable.js")),
    ("stream/web", include_str!("../js/stream_web.js")),
    ("internal/fetch_body", include_str!("../js/fetch_body.js")),
    ("internal/fetch", include_str!("../js/fetch.js")),
    ("assert", include_str!("../js/assert.js")),
    ("assert/strict", "module.exports = require('assert').strict;"),
    ("tty", include_str!("../js/tty.js")),
    ("diagnostics_channel", include_str!("../js/diagnostics_channel.js")),
    ("stream/consumers", include_str!("../js/stream_consumers.js")),
    ("async_hooks", include_str!("../js/async_hooks.js")),
    ("domain", include_str!("../js/domain.js")),
    ("punycode", include_str!("../js/punycode.js")),
    ("sea", include_str!("../js/sea.js")),
    ("trace_events", include_str!("../js/trace_events.js")),
    ("internal/v8_serdes", include_str!("../js/v8_serdes.js")),
    ("internal/v8_deserializer", include_str!("../js/v8_deserializer.js")),
    ("v8", include_str!("../js/v8.js")),
    ("internal/zlib_constants", include_str!("../js/zlib_constants.js")),
    ("zlib", include_str!("../js/zlib.js")),
    ("internal/net_address", include_str!("../js/net_address.js")),
    ("internal/net_socket", include_str!("../js/net_socket.js")),
    ("net", include_str!("../js/net.js")),
    ("dns", include_str!("../js/dns.js")),
    ("dns/promises", "module.exports = require('dns').promises;"),
    ("dgram", include_str!("../js/dgram.js")),
    ("internal/tls_wrap", include_str!("../js/tls_wrap.js")),
    ("tls", include_str!("../js/tls.js")),
    ("internal/http_common", include_str!("../js/http_common.js")),
    ("internal/http_outgoing", include_str!("../js/http_outgoing.js")),
    ("internal/http_server", include_str!("../js/http_server.js")),
    ("internal/http_agent", include_str!("../js/http_agent.js")),
    ("internal/http_client", include_str!("../js/http_client.js")),
    ("http", include_str!("../js/http.js")),
    ("https", include_str!("../js/https.js")),
    ("internal/http2_core", include_str!("../js/http2_core.js")),
    ("internal/http2_session", include_str!("../js/http2_session.js")),
    ("internal/http2_compat", include_str!("../js/http2_compat.js")),
    ("http2", include_str!("../js/http2.js")),
    ("vm", include_str!("../js/vm.js")),
    ("internal/worker_ports", include_str!("../js/worker_ports.js")),
    ("worker_threads", include_str!("../js/worker_threads.js")),
    ("cluster", include_str!("../js/cluster.js")),
    ("sqlite", include_str!("../js/sqlite.js")),
    ("internal/test_harness", include_str!("../js/test_harness.js")),
    ("internal/test_mock", include_str!("../js/test_mock.js")),
    ("test/reporters", include_str!("../js/test_reporters.js")),
    ("test", include_str!("../js/test.js")),
    ("repl", include_str!("../js/repl.js")),
    ("inspector", include_str!("../js/inspector.js")),
    ("inspector/promises", "module.exports = require('inspector').promises;"),
    ("internal/wasm_values", include_str!("../js/wasm_values.js")),
    ("internal/webassembly", include_str!("../js/webassembly.js")),
    ("internal/wasi_fds", include_str!("../js/wasi_fds.js")),
    ("internal/wasi_calls", include_str!("../js/wasi_calls.js")),
    ("wasi", include_str!("../js/wasi.js")),
    (
        "constants",
        "module.exports = { ...require('os').constants.errno, ...require('os').constants.signals, ...require('os').constants.priority, ...require('internal/fs_utils').constants };",
    ),
];

/// Every module name `require` accepts, for `module.builtinModules`.
pub(crate) fn supported() -> impl Iterator<Item = &'static str> {
    SOURCES.iter().map(|(name, _)| *name).filter(|name| !name.starts_with("internal/") && *name != "pi:host")
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "compile", compile)?;
    set_fn(&object, "list", || supported().map(str::to_string).collect::<Vec<_>>())?;
    set_fn(&object, "prefixOnly", || PREFIX_ONLY.iter().map(|name| name.to_string()).collect::<Vec<_>>())?;
    Ok(object)
}

fn compile<'js>(ctx: Ctx<'js>, name: String) -> rquickjs::Result<Function<'js>> {
    let Some((_, source)) = SOURCES.iter().find(|(known, _)| *known == name) else {
        let error = if NODE_BUILTINS.contains(&name.as_str()) {
            JsError::unsupported(&format!("Node module 'node:{name}'"))
        } else {
            JsError::new(format!("No such built-in module: {name}")).with_code("ERR_UNKNOWN_BUILTIN_MODULE")
        };
        return Err(throw(&ctx, &error));
    };
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(if name.contains(':') { name.clone() } else { format!("node:{name}") });
    let wrapped = format!("(function (module, exports, require, native, internal) {{ \"use strict\"; {source}\n}})");
    let wrapped = crate::transpile::instrument_script(&wrapped).unwrap_or(wrapped);
    ctx.eval_with_options(wrapped, options)
}
