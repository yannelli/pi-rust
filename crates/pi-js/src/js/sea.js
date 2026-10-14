// node:sea: pi-rust never runs as a Node single-executable application.
const { codeError } = require("internal/errors");

function notInSea() {
  throw codeError(Error, "ERR_NOT_IN_SINGLE_EXECUTABLE_APPLICATION", "Operation cannot be invoked when not in a single-executable application");
}

module.exports = {
  isSea: () => false,
  getAsset: (key, encoding) => notInSea(key, encoding),
  getRawAsset: (key) => notInSea(key),
  getAssetAsBlob: (key, options) => notInSea(key, options),
  getAssetKeys: () => notInSea(),
};
