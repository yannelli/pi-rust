// pi:host: call Rust host functions registered on the runtime.
const host = native.host;
module.exports = {
  call: host.call,
  callAsync(name, ...args) {
    try {
      return Promise.resolve(host.call(name, ...args));
    } catch (error) {
      return Promise.reject(error);
    }
  },
  has: host.has,
  names: host.names,
};
