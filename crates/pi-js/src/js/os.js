// node:os.
const nativeOs = native.os;
const { validate } = require("internal/errors");
const config = internal.config;
const platform = config.platform;

const LINUX_SIGNALS = {
  SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGIOT: 6, SIGBUS: 7, SIGFPE: 8, SIGKILL: 9,
  SIGUSR1: 10, SIGSEGV: 11, SIGUSR2: 12, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15, SIGCHLD: 17, SIGSTKFLT: 16,
  SIGCONT: 18, SIGSTOP: 19, SIGTSTP: 20, SIGTTIN: 21, SIGTTOU: 22, SIGURG: 23, SIGXCPU: 24, SIGXFSZ: 25,
  SIGVTALRM: 26, SIGPROF: 27, SIGWINCH: 28, SIGIO: 29, SIGPOLL: 29, SIGPWR: 30, SIGSYS: 31,
};
const DARWIN_SIGNALS = {
  SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGIOT: 6, SIGBUS: 10, SIGFPE: 8, SIGKILL: 9,
  SIGUSR1: 30, SIGSEGV: 11, SIGUSR2: 31, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15, SIGCHLD: 20, SIGCONT: 19,
  SIGSTOP: 17, SIGTSTP: 18, SIGTTIN: 21, SIGTTOU: 22, SIGURG: 16, SIGXCPU: 24, SIGXFSZ: 25, SIGVTALRM: 26,
  SIGPROF: 27, SIGWINCH: 28, SIGIO: 23, SIGINFO: 29, SIGSYS: 12,
};
const WINDOWS_SIGNALS = { SIGHUP: 1, SIGINT: 2, SIGILL: 4, SIGABRT: 22, SIGFPE: 8, SIGKILL: 9, SIGSEGV: 11, SIGTERM: 15, SIGBREAK: 21, SIGWINCH: 28 };

const COMMON_ERRNO = {
  EPERM: 1, ENOENT: 2, ESRCH: 3, EINTR: 4, EIO: 5, ENXIO: 6, E2BIG: 7, ENOEXEC: 8, EBADF: 9, ECHILD: 10, ENOMEM: 12,
  EACCES: 13, EFAULT: 14, EBUSY: 16, EEXIST: 17, EXDEV: 18, ENODEV: 19, ENOTDIR: 20, EISDIR: 21, EINVAL: 22,
  ENFILE: 23, EMFILE: 24, ENOTTY: 25, EFBIG: 27, ENOSPC: 28, ESPIPE: 29, EROFS: 30, EMLINK: 31, EPIPE: 32,
};
const LINUX_ERRNO = { ...COMMON_ERRNO, EAGAIN: 11, ENOTEMPTY: 39, ELOOP: 40, ENAMETOOLONG: 36, ENOSYS: 38, ETIMEDOUT: 110, ECONNREFUSED: 111, ECONNRESET: 104, EADDRINUSE: 98 };
const DARWIN_ERRNO = { ...COMMON_ERRNO, EAGAIN: 35, ENOTEMPTY: 66, ELOOP: 62, ENAMETOOLONG: 63, ENOSYS: 78, ETIMEDOUT: 60, ECONNREFUSED: 61, ECONNRESET: 54, EADDRINUSE: 48 };

const PLATFORM_SIGNALS = { win32: WINDOWS_SIGNALS, darwin: DARWIN_SIGNALS };
const signals = PLATFORM_SIGNALS[platform] || LINUX_SIGNALS;
const constants = {
  UV_UDP_REUSEADDR: 4,
  dlopen: platform === "win32" ? {} : { RTLD_LAZY: 1, RTLD_NOW: 2, RTLD_GLOBAL: 256, RTLD_LOCAL: 0 },
  errno: platform === "darwin" ? DARWIN_ERRNO : LINUX_ERRNO,
  signals: Object.freeze({ ...signals }),
  priority: { PRIORITY_LOW: 19, PRIORITY_BELOW_NORMAL: 10, PRIORITY_NORMAL: 0, PRIORITY_ABOVE_NORMAL: -7, PRIORITY_HIGH: -14, PRIORITY_HIGHEST: -20 },
};

function cpus() {
  return nativeOs.cpus().map(({ model, speed }) => ({ model, speed, times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 } }));
}

// Node reads these from the environment first; this runtime's environment is process.env.
function homedir() {
  const env = process.env;
  return (platform === "win32" ? env.USERPROFILE : env.HOME) || nativeOs.homedir();
}

function tmpdir() {
  const env = process.env;
  if (platform === "win32") {
    const dir = env.TEMP || env.TMP || `${env.SystemRoot || env.windir}\\temp`;
    return dir.length > 1 && dir.endsWith("\\") && !dir.endsWith(":\\") ? dir.slice(0, -1) : dir;
  }
  const dir = env.TMPDIR || env.TMP || env.TEMP || nativeOs.tmpdir();
  return dir.length > 1 && dir.endsWith("/") ? dir.slice(0, -1) : dir;
}

function userId(kind) {
  if (platform === "win32") return -1;
  return native.process.uid(kind) ?? -1;
}

function userInfo(options = {}) {
  const user = {
    uid: userId("uid"),
    gid: userId("gid"),
    username: nativeOs.username(),
    homedir: homedir(),
    shell: platform === "win32" ? null : nativeOs.shell() ?? null,
  };
  if (options.encoding === "buffer") {
    const { Buffer } = require("buffer");
    for (const key of ["username", "homedir", "shell"]) if (user[key] !== null) user[key] = Buffer.from(user[key]);
  }
  return user;
}

module.exports = {
  EOL: platform === "win32" ? "\r\n" : "\n",
  devNull: platform === "win32" ? "\\\\.\\nul" : "/dev/null",
  constants,
  arch: () => config.arch,
  platform: () => platform,
  type: () => nativeOs.type(),
  release: () => nativeOs.release(),
  version: () => nativeOs.version(),
  machine: () => nativeOs.machine(),
  hostname: () => nativeOs.hostname(),
  homedir,
  tmpdir,
  totalmem: () => nativeOs.totalmem(),
  freemem: () => nativeOs.freemem(),
  uptime: () => nativeOs.uptime(),
  loadavg: () => nativeOs.loadavg(),
  availableParallelism: () => nativeOs.parallelism(),
  endianness: () => "LE",
  cpus,
  userInfo,
  networkInterfaces,
  getPriority(pid = 0) {
    validate.integer(pid, "pid", -2147483648, 2147483647);
    return nativeOs.getPriority(pid);
  },
  setPriority(pid, priority) {
    if (priority === undefined) [priority, pid] = [pid, 0];
    validate.integer(pid, "pid", -2147483648, 2147483647);
    validate.integer(priority, "priority", -20, 19);
    nativeOs.setPriority(pid, priority);
  },
};

// The prefix length of a contiguous netmask, else null, as Node computes cidr.
function prefixLength(netmask, family) {
  let bits = "";
  if (family === "IPv4") bits = netmask.split(".").map((part) => Number(part).toString(2).padStart(8, "0")).join("");
  else {
    const [head, tail = ""] = netmask.split("::");
    const left = head ? head.split(":") : [];
    const right = tail ? tail.split(":") : [];
    const groups = netmask.includes("::") ? [...left, ...Array(8 - left.length - right.length).fill("0"), ...right] : left;
    bits = groups.map((group) => parseInt(group || "0", 16).toString(2).padStart(16, "0")).join("");
  }
  return /^1*0*$/.test(bits) ? bits.indexOf("0") === -1 ? bits.length : bits.indexOf("0") : null;
}

function networkInterfaces() {
  const result = {};
  for (const { name, address, netmask, family, mac, internal, scopeid } of nativeOs.networkInterfaces()) {
    const bits = prefixLength(netmask, family);
    const entry = { address, netmask, family, mac, internal, cidr: bits === null ? null : `${address}/${bits}` };
    if (family === "IPv6") entry.scopeid = scopeid;
    (result[name] ||= []).push(entry);
  }
  return result;
}
