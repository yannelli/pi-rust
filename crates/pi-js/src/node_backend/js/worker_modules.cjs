"use strict";
// Module loading through Rust: pi's resolver (virtual modules, registered source files,
// extensionless TypeScript) and pi-js's transpiler. Node loads everything else itself.
const Module = require("node:module");
const { fileURLToPath, pathToFileURL } = require("node:url");

const OWN_DIR = pathToFileURL(__dirname).href + "/";
const NODE_22 = process.versions.node.startsWith("22.");

function parentPath(parentURL) {
  if (!parentURL || parentURL.startsWith(OWN_DIR) || !parentURL.startsWith("file:")) return null;
  return fileURLToPath(parentURL);
}

function plain(specifier) {
  return specifier.startsWith("file:") ? fileURLToPath(specifier) : specifier;
}

function isBuiltin(specifier) {
  return specifier.startsWith("node:") || Module.isBuiltin(specifier);
}

function install({ ask, answer, hooksPort, config }) {
  const resolveRust = (specifier, parent, kind) => ask({ t: "resolve", specifier, parent, kind });
  const loadRust = (filename) => answer(ask({ t: "load", path: filename }));

  if (typeof Module.registerHooks === "function") {
    Module.registerHooks({
      resolve(specifier, context, nextResolve) {
        if (isBuiltin(specifier)) return nextResolve(specifier, context);
        const kind = context.conditions?.includes("require") ? "require" : "import";
        const reply = resolveRust(plain(specifier), parentPath(context.parentURL), kind);
        if (reply.ok) return { url: reply.ok.url, shortCircuit: true };
        try {
          return nextResolve(specifier, context);
        } catch {
          return answer(reply);
        }
      },
      load(url, context, nextLoad) {
        if (!url.startsWith("file:") || url.endsWith(".node")) return nextLoad(url, context);
        const loaded = loadRust(fileURLToPath(url));
        if (!loaded) {
          // Node 22 runs CommonJS that import() loads with a require that cannot load ES
          // modules; its internal "commonjs-sync" format uses the CommonJS loader instead.
          const next = nextLoad(url, context);
          return NODE_22 && next.format === "commonjs" ? { ...next, format: "commonjs-sync" } : next;
        }
        return { format: loaded.format, source: loaded.source, shortCircuit: true };
      },
    });
    // Node 22 compiles CommonJS files that import() loads with a require that skips the hooks.
    patchResolve(resolveRust, answer);
    return;
  }

  Module.register(pathToFileURL(`${__dirname}/hooks.mjs`).href, {
    data: { port: hooksPort, ownDir: OWN_DIR },
    transferList: [hooksPort],
  });
  patchRequire(resolveRust, loadRust, answer);
}

/** CommonJS loading for Node versions without `module.registerHooks`. */
function patchRequire(resolveRust, loadRust, answer) {
  patchResolve(resolveRust, answer);
  patchExtensions(loadRust);
}

function patchResolve(resolveRust, answer) {
  const resolveFilename = Module._resolveFilename;
  Module._resolveFilename = function (request, parent, isMain, options) {
    if (isBuiltin(request)) return resolveFilename.call(this, request, parent, isMain, options);
    const reply = resolveRust(request, parent?.filename ?? null, "require");
    if (reply.ok) {
      const url = reply.ok.url;
      return url.startsWith("file:") ? fileURLToPath(url) : url;
    }
    try {
      return resolveFilename.call(this, request, parent, isMain, options);
    } catch {
      return answer(reply);
    }
  };
}

function patchExtensions(loadRust) {
  const compileWith = (fallback) =>
    function (module, filename) {
      const loaded = loadRust(filename);
      if (!loaded) return fallback.call(this, module, filename);
      if (loaded.format === "json") {
        module.exports = JSON.parse(loaded.source);
        return;
      }
      if (loaded.format !== "commonjs") {
        const error = new Error(`require() of ES module ${filename} needs Node 22.15 or later on the pi Node backend; use import()`);
        error.code = "ERR_REQUIRE_ESM";
        throw error;
      }
      module._compile(loaded.source, filename);
    };
  const extensions = Module._extensions;
  const js = extensions[".js"];
  for (const ext of [".js", ".cjs", ".mjs", ".ts", ".cts", ".mts", ".tsx", ".jsx"]) {
    extensions[ext] = compileWith(extensions[ext] ?? js);
  }
  extensions[".json"] = compileWith(extensions[".json"]);
}

module.exports = { install };
