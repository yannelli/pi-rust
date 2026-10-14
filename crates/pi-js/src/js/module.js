// node:module: the CommonJS Module class from the bootstrap, plus loader hooks and source maps.
const Module = internal.Module;
const hooks = require("internal/module_hooks");
Module.register = hooks.register;
Module.findSourceMap = hooks.findSourceMap;
Module.SourceMap = hooks.SourceMap;
Module.syncBuiltinESMExports = () => {};
Module.enableCompileCache = () => ({ status: 3 });
Module.constants = { compileCacheStatus: { FAILED: 0, ENABLED: 1, ALREADY_ENABLED: 2, DISABLED: 3 } };
module.exports = Module;
