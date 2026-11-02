"use strict";
// Module namespaces. Node keys modules by path, so a module in a namespace stands at an alias path
// of its own and Node loads it as usual; its code sees the original location (see modules.rs).
const Module = require("node:module");
const path = require("node:path");
const { randomUUID } = require("node:crypto");
const { fileURLToPath, pathToFileURL } = require("node:url");

const QUERY = "pi-ns";
const ENTRY = "pi-ns-entry:";
const NODE_22 = process.versions.node.startsWith("22.");

function isBuiltin(specifier) {
  return specifier.startsWith("node:") || Module.isBuiltin(specifier);
}

function unsupported(message) {
  const error = new Error(`${message} is not supported in pi-rust`);
  error.code = "ERR_PI_UNSUPPORTED";
  return error;
}

function aliasOf(url) {
  if (typeof url !== "string" || !url.startsWith("file:")) return null;
  const parsed = new URL(url);
  return parsed.search ? null : fileURLToPath(parsed);
}

function create({ resolveRust, loadRust, answer }) {
  const entries = new Map();
  const modules = new Map();
  const aliases = new Map();
  const dirs = new Map();
  const views = new Map();
  const anchors = new WeakMap();

  /** `{ ns, path }` for the alias URL of a module in a namespace, else `null`. */
  function infoOf(url) {
    const alias = aliasOf(url);
    return (alias !== null && modules.get(alias)) || null;
  }

  function record(resolved) {
    if (!resolved.ns) return resolved.url;
    const alias = fileURLToPath(resolved.url);
    modules.set(alias, { ns: resolved.ns, path: resolved.path });
    dirs.set(path.dirname(alias), path.dirname(resolved.path));
    let own = aliases.get(resolved.ns);
    if (!own) aliases.set(resolved.ns, (own = new Map()));
    own.set(resolved.path, alias);
    return pathToFileURL(alias).href;
  }

  /** The URL in a Rust resolve reply; a module in a namespace gets Node's URL for its alias path. */
  function located(reply) {
    return record(answer(reply));
  }

  /** A specifier whose resolution is `url`, for one `import()`. */
  function entry(url) {
    const token = randomUUID();
    entries.set(token, url);
    return ENTRY + token;
  }

  function resolveEntry(specifier) {
    const token = specifier.slice(ENTRY.length);
    const url = entries.get(token);
    entries.delete(token);
    if (url === undefined) {
      const error = new Error(`Cannot find module '${specifier}'`);
      error.code = "ERR_MODULE_NOT_FOUND";
      throw error;
    }
    return { url, shortCircuit: true };
  }

  /** Rust's `import_namespaced`: `specifier` resolves from the cwd into `namespace`. */
  function importInto(specifier, namespace) {
    if (typeof Module.registerHooks !== "function") throw unsupported("A module namespace on Node before 22.15");
    const url = located(resolveRust(specifier, null, "import", namespace));
    return import(infoOf(url) ? entry(url) : url);
  }

  /** `require.resolve` from `filename` in `namespace`: original paths, never alias paths. */
  function resolvePath(request, filename, namespace) {
    if (isBuiltin(request)) return request;
    const resolved = answer(resolveRust(request, filename, "require", namespace));
    if (resolved.ns) return (record(resolved), resolved.path);
    return resolved.url.startsWith("file:") ? fileURLToPath(resolved.url) : resolved.url;
  }

  /** `import.meta.resolve` of `filename` in `namespace`: a module of the namespace has its query. */
  function metaResolve(filename, namespace) {
    return function resolve(specifier) {
      const resolved = answer(resolveRust(String(specifier), filename, "import", namespace));
      if (!resolved.ns) return resolved.url;
      record(resolved);
      return `${pathToFileURL(resolved.path).href}?${QUERY}=${resolved.ns}`;
    };
  }

  /** `require.cache` in `namespace`: Node's cache with the namespace's modules at their own paths. */
  function cacheView(namespace) {
    let view = views.get(namespace);
    if (view) return view;
    const key = (name) => aliases.get(namespace)?.get(name) ?? name;
    const shown = (name) => {
      const info = modules.get(name);
      return info ? (info.ns === namespace ? info.path : null) : name;
    };
    const keyed = (trap, own) => (target, name, ...rest) =>
      typeof name === "string" ? own(key(name), ...rest) : Reflect[trap](target, name, ...rest);
    view = new Proxy(Object.create(null), {
      get: keyed("get", (name) => Module._cache[name]),
      has: keyed("has", (name) => name in Module._cache),
      set: keyed("set", (name, value) => ((Module._cache[name] = value), true)),
      deleteProperty: keyed("deleteProperty", (name) => delete Module._cache[name]),
      defineProperty: keyed("defineProperty", (name, descriptor) => ((Module._cache[name] = descriptor.value), true)),
      getOwnPropertyDescriptor: keyed("getOwnPropertyDescriptor", (name) =>
        name in Module._cache
          ? { value: Module._cache[name], writable: true, enumerable: true, configurable: true }
          : undefined),
      ownKeys: (target) => [
        ...new Set(Object.keys(Module._cache).map(shown).filter((name) => name !== null)),
        ...Reflect.ownKeys(target).filter((name) => typeof name === "symbol"),
      ],
    });
    views.set(namespace, view);
    return view;
  }

  /** Where Node's loader sees a namespaced module: at its alias, keeping `module`'s own fields. */
  function anchorOf(module, alias) {
    const own = (value) => ({ value, writable: true, configurable: true });
    const anchor = Object.create(module, { filename: own(alias), path: own(path.dirname(alias)) });
    // Node takes a parent's hook URL from its filename once (cjs/loader.js:1052-1055) and keeps it.
    Module.prototype.require.call(anchor, "node:module");
    delete anchor.filename;
    anchors.set(anchor, module);
    return anchor;
  }

  /** Each child Node created under an anchor gets the anchor's module as its `parent` again. */
  function reparent(module) {
    if (module && anchors.has(module.parent)) module.parent = anchors.get(module.parent);
  }

  /** Node caches a CommonJS child before its load hook runs (cjs/loader.js:1293), so the hook
   *  gives it its real parent before its code runs. */
  function reparentLoading(url) {
    if (url.startsWith("file:")) reparent(Module._cache[fileURLToPath(url)]);
  }

  /** The `require` of CommonJS `module`, whose original path is `filename`, in `namespace`. Node's
   *  loader sees `module` at its alias; its public fields show the original location. */
  function requireOf(module, filename, namespace) {
    const alias = aliases.get(namespace)?.get(filename);
    if (alias === undefined) throw new Error(`${filename} is not a module of namespace ${namespace}`);
    const anchor = anchorOf(module, alias);
    const require = function require(id) {
      try {
        return Module.prototype.require.call(anchor, id);
      } finally {
        module.children.forEach(reparent);
      }
    };
    if (module.id === alias) module.id = filename;
    module.filename = filename;
    module.path = path.dirname(filename);
    module.paths = Module._nodeModulePaths(module.path);
    module.require = require;
    require.resolve = (request) => resolvePath(request, filename, namespace);
    require.resolve.paths = (request) => (isBuiltin(request) ? null : Module._nodeModulePaths(path.dirname(filename)));
    require.main = process.mainModule;
    require.extensions = Module._extensions;
    require.cache = cacheView(namespace);
    return require;
  }

  /** A free `require` in the ES module of `namespace` whose original path is `filename`. */
  function requireIn(filename, namespace) {
    const module = new Module(filename, null);
    module.loaded = true;
    return requireOf(module, filename, namespace);
  }

  /** The original location of `target` when Node's `createRequire` would read it as an alias or
   *  a file in an alias directory (cjs/loader.js:1946-1996), else `target` with its native errors. */
  function original(target) {
    let file = target;
    try {
      if (target instanceof URL || (typeof target === "string" && !path.isAbsolute(target))) {
        file = fileURLToPath(new URL(target));
      }
    } catch {
      return target;
    }
    if (typeof file !== "string") return target;
    const directory = file.endsWith("/") || file.endsWith(path.sep);
    const resolved = path.resolve(file);
    const info = directory ? undefined : modules.get(resolved);
    if (info) return info.path;
    const at = directory ? resolved : path.dirname(resolved);
    const dir = dirs.get(at);
    if (dir === undefined || dir === at) return target;
    return directory ? dir + path.sep : path.join(dir, path.basename(resolved));
  }

  const nativeCreateRequire = Module.createRequire;
  Module.createRequire = function createRequire(filename) {
    return nativeCreateRequire(original(filename));
  };
  Module.syncBuiltinESMExports();

  /** A path Node's loader asks for without hooks (`commonjs-sync` evaluation) that is an alias. */
  function isAlias(filename) {
    return modules.has(filename);
  }

  /** The load hook's answer for a module in a namespace. On Node 22, CommonJS that `import` loads
   *  runs through Node's CommonJS loader, as `require(esm)` graphs run it, not its own `require`. */
  function loadHook(info, context) {
    const loaded = answer(loadRust(info.path, info.ns));
    const imported = !context.conditions?.includes("require");
    const format = NODE_22 && imported && loaded.format === "commonjs" ? "commonjs-sync" : loaded.format;
    return { format, source: loaded.source, shortCircuit: true };
  }

  globalThis[Symbol.for("pi.namespaces")] = { requireOf, requireIn, metaResolve };
  return {
    infoOf, located, resolveEntry, importInto, loadHook, isAlias, reparentLoading,
    isEntry: (specifier) => specifier.startsWith(ENTRY),
  };
}

module.exports = { create };
