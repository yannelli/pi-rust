// node:module: the CommonJS Module class from the bootstrap.
const Module = internal.Module;
Module.register = () => {
  throw internal.unsupported("module.register");
};
Module.findSourceMap = () => {
  throw internal.unsupported("module.findSourceMap");
};
Module.syncBuiltinESMExports = () => {};
Module.enableCompileCache = () => ({ status: 3 });
Module.constants = { compileCacheStatus: { FAILED: 0, ENABLED: 1, ALREADY_ENABLED: 2, DISABLED: 3 } };
module.exports = Module;
