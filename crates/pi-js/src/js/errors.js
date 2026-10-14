// Node error helpers (ERR_* codes and messages) and libuv's system error table.
const platform = internal.config.platform;

function codeError(Ctor, code, message) {
  return Object.assign(new Ctor(message), { code });
}

function describe(value) {
  if (value === null || value === undefined) return String(value);
  if (typeof value === "function") return `function ${value.name}`;
  if (typeof value === "object") return value.constructor && value.constructor.name ? `an instance of ${value.constructor.name}` : "an instance of Object";
  const shown = typeof value === "string" ? `'${value.length > 28 ? value.slice(0, 25) + "..." : value}'` : String(value) + (typeof value === "bigint" ? "n" : "");
  return `type ${typeof value} (${shown})`;
}

function invalidArgType(name, expected, value) {
  const kind = name.includes(".") ? "property" : "argument";
  return codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" ${kind} must be ${expected}. Received ${describe(value)}`);
}

function invalidArgValue(name, value, reason = "is invalid") {
  const kind = name.includes(".") ? "property" : "argument";
  const shown = require("util").inspect(value);
  return codeError(TypeError, "ERR_INVALID_ARG_VALUE", `The ${kind} '${name}' ${reason}. Received ${shown}`);
}

function outOfRange(name, range, value) {
  return codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "${name}" is out of range. It must be ${range}. Received ${String(value)}`);
}

function missingArgs(...names) {
  const list = names.map((name) => `"${name}"`).join(" and ");
  return codeError(TypeError, "ERR_MISSING_ARGS", `The ${list} argument${names.length > 1 ? "s" : ""} must be specified`);
}

const validate = {
  string(value, name) {
    if (typeof value !== "string") throw invalidArgType(name, "of type string", value);
  },
  boolean(value, name) {
    if (typeof value !== "boolean") throw invalidArgType(name, "of type boolean", value);
  },
  function(value, name) {
    if (typeof value !== "function") throw invalidArgType(name, "of type function", value);
  },
  object(value, name) {
    if (value === null || typeof value !== "object" || Array.isArray(value)) throw invalidArgType(name, "of type object", value);
  },
  array(value, name) {
    if (!Array.isArray(value)) throw invalidArgType(name, "an instance of Array", value);
  },
  integer(value, name, min = Number.MIN_SAFE_INTEGER, max = Number.MAX_SAFE_INTEGER) {
    if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
    if (!Number.isInteger(value)) throw outOfRange(name, "an integer", value);
    if (value < min || value > max) throw outOfRange(name, `>= ${min} && <= ${max}`, value);
  },
};

// [name, message, Linux errno, macOS errno]; a missing errno uses libuv's own code. Windows
// always uses libuv's codes, which count down from -4093 in this order.
const UV_ERRORS = [
  ["E2BIG", "argument list too long", 7, 7], ["EACCES", "permission denied", 13, 13],
  ["EADDRINUSE", "address already in use", 98, 48], ["EADDRNOTAVAIL", "address not available", 99, 49],
  ["EAFNOSUPPORT", "address family not supported", 97, 47], ["EAGAIN", "resource temporarily unavailable", 11, 35],
  null, null, null, ["EALREADY", "connection already in progress", 114, 37], ["EBADF", "bad file descriptor", 9, 9],
  ["EBUSY", "resource busy or locked", 16, 16], ["ECANCELED", "operation canceled", 125, 89],
  ["ECHARSET", "invalid Unicode character"], ["ECONNABORTED", "software caused connection abort", 103, 53],
  ["ECONNREFUSED", "connection refused", 111, 61], ["ECONNRESET", "connection reset by peer", 104, 54],
  ["EDESTADDRREQ", "destination address required", 89, 39], ["EEXIST", "file already exists", 17, 17],
  ["EFAULT", "bad address in system call argument", 14, 14], ["EHOSTUNREACH", "host is unreachable", 113, 65],
  ["EINTR", "interrupted system call", 4, 4], ["EINVAL", "invalid argument", 22, 22], ["EIO", "i/o error", 5, 5],
  ["EISCONN", "socket is already connected", 106, 56], ["EISDIR", "illegal operation on a directory", 21, 21],
  ["ELOOP", "too many symbolic links encountered", 40, 62], ["EMFILE", "too many open files", 24, 24],
  ["EMSGSIZE", "message too long", 90, 40], ["ENAMETOOLONG", "name too long", 36, 63],
  ["ENETDOWN", "network is down", 100, 50], ["ENETUNREACH", "network is unreachable", 101, 51],
  ["ENFILE", "file table overflow", 23, 23], ["ENOBUFS", "no buffer space available", 105, 55],
  ["ENODEV", "no such device", 19, 19], ["ENOENT", "no such file or directory", 2, 2],
  ["ENOMEM", "not enough memory", 12, 12], ["ENONET", "machine is not on the network", 64],
  ["ENOSPC", "no space left on device", 28, 28], ["ENOSYS", "function not implemented", 38, 78],
  ["ENOTCONN", "socket is not connected", 107, 57], ["ENOTDIR", "not a directory", 20, 20],
  ["ENOTEMPTY", "directory not empty", 39, 66], ["ENOTSOCK", "socket operation on non-socket", 88, 38],
  ["ENOTSUP", "operation not supported on socket", 95, 45], ["EPERM", "operation not permitted", 1, 1],
  ["EPIPE", "broken pipe", 32, 32], ["EPROTO", "protocol error", 71, 100],
  ["EPROTONOSUPPORT", "protocol not supported", 93, 43], ["EPROTOTYPE", "protocol wrong type for socket", 91, 41],
  ["EROFS", "read-only file system", 30, 30], ["ESHUTDOWN", "cannot send after transport endpoint shutdown", 108, 58],
  ["ESPIPE", "invalid seek", 29, 29], ["ESRCH", "no such process", 3, 3], ["ETIMEDOUT", "connection timed out", 110, 60],
  ["ETXTBSY", "text file is busy", 26, 26], ["EXDEV", "cross-device link not permitted", 18, 18],
  ["EFBIG", "file too large", 27, 27], ["ENOPROTOOPT", "protocol not available", 92, 42],
  ["ERANGE", "result too large", 34, 34], ["ENXIO", "no such device or address", 6, 6],
  ["EMLINK", "too many links", 31, 31], ["EHOSTDOWN", "host is down", 112, 64],
  ["EREMOTEIO", "remote I/O error", 121], ["ENOTTY", "inappropriate ioctl for device", 25, 25],
  ["EFTYPE", "inappropriate file type or format", undefined, 79], ["EILSEQ", "illegal byte sequence", 84, 92],
  ["EOVERFLOW", "value too large for defined data type", 75, 84],
  ["ESOCKTNOSUPPORT", "socket type not supported", 94, 44], ["ENODATA", "no data available", 61, 96],
  ["EUNATCH", "protocol driver not attached", 49], ["ENOEXEC", "exec format error", 8, 8],
];
const EAI_ERRORS = [
  ["EAI_ADDRFAMILY", "address family not supported"], ["EAI_AGAIN", "temporary failure"],
  ["EAI_BADFLAGS", "bad ai_flags value"], ["EAI_CANCELED", "request canceled"], ["EAI_FAIL", "permanent failure"],
  ["EAI_FAMILY", "ai_family not supported"], ["EAI_MEMORY", "out of memory"], ["EAI_NODATA", "no address"],
  ["EAI_NONAME", "unknown node or service"], ["EAI_OVERFLOW", "argument buffer overflow"],
  ["EAI_SERVICE", "service not available for socket type"], ["EAI_SOCKTYPE", "socket type not supported"], null,
  ["EAI_BADHINTS", "invalid value for hints"], ["EAI_PROTOCOL", "resolved protocol is unknown"],
];

let errorMap;
function getSystemErrorMap() {
  if (errorMap) return errorMap;
  errorMap = new Map();
  const column = platform === "darwin" ? 3 : platform === "win32" ? -1 : 2;
  UV_ERRORS.forEach((entry, index) => {
    if (!entry) return;
    const errno = column > 0 && entry[column] !== undefined ? -entry[column] : -4093 + index;
    errorMap.set(errno, [entry[0], entry[1]]);
  });
  EAI_ERRORS.forEach((entry, index) => entry && errorMap.set(-3000 - index, entry));
  errorMap.set(-4094, ["UNKNOWN", "unknown error"]);
  errorMap.set(-4095, ["EOF", "end of file"]);
  return errorMap;
}

function getSystemErrorName(err) {
  if (typeof err !== "number") throw invalidArgType("err", "of type number", err);
  if (err >= 0 || !Number.isSafeInteger(err)) throw outOfRange("err", "a negative integer", err);
  const entry = getSystemErrorMap().get(err);
  return entry ? entry[0] : `Unknown system error ${err}`;
}

function getSystemErrorMessage(err) {
  if (typeof err !== "number") throw invalidArgType("err", "of type number", err);
  if (err >= 0 || !Number.isSafeInteger(err)) throw outOfRange("err", "a negative integer", err);
  const entry = getSystemErrorMap().get(err);
  return entry ? entry[1] : `Unknown system error ${err}`;
}

// The negative errno Node reports for a code name, such as -2 for ENOENT on Linux.
function errnoOf(name) {
  for (const [errno, entry] of getSystemErrorMap()) if (entry[0] === name) return errno;
  return undefined;
}

module.exports = {
  codeError,
  describe,
  invalidArgType,
  invalidArgValue,
  outOfRange,
  missingArgs,
  validate,
  getSystemErrorMap,
  getSystemErrorName,
  getSystemErrorMessage,
  errnoOf,
};
