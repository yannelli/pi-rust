// Web globals: DOMException, Event, EventTarget, AbortController, TextEncoder, structuredClone.
const encoding = native.encoding;
const LEGACY_CODES = {
  IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4, InvalidCharacterError: 5, NoModificationAllowedError: 7,
  NotFoundError: 8, NotSupportedError: 9, InvalidStateError: 11, SyntaxError: 12, InvalidModificationError: 13,
  NamespaceError: 14, InvalidAccessError: 15, TypeMismatchError: 17, SecurityError: 18, NetworkError: 19, AbortError: 20,
  URLMismatchError: 21, QuotaExceededError: 22, TimeoutError: 23, InvalidNodeTypeError: 24, DataCloneError: 25,
};

class DOMException extends Error {
  #name;
  constructor(message = "", options = "Error") {
    const name = typeof options === "object" && options !== null ? String(options.name ?? "Error") : String(options);
    super(String(message), typeof options === "object" && options !== null && "cause" in options ? { cause: options.cause } : undefined);
    this.#name = name;
  }
  get name() {
    return this.#name;
  }
  get code() {
    return LEGACY_CODES[this.#name] || 0;
  }
  get [Symbol.toStringTag]() {
    return "DOMException";
  }
}
for (const [name, code] of Object.entries(LEGACY_CODES)) {
  const constant = name.replace(/Error$/, "").replace(/([a-z])([A-Z])/g, "$1_$2").toUpperCase() + "_ERR";
  Object.defineProperty(DOMException, constant, { value: code, enumerable: true });
}

const kStop = Symbol("stop");
class Event {
  #type;
  #options;
  #canceled = false;
  #target = null;
  #current = null;
  #phase = 0;
  #dispatching = false;
  constructor(type, options = {}) {
    if (arguments.length === 0) throw new TypeError('The "type" argument must be specified');
    this.#type = String(type);
    this.#options = { bubbles: !!options.bubbles, cancelable: !!options.cancelable, composed: !!options.composed };
    this.timeStamp = globalThis.performance ? performance.now() : Date.now();
    this[kStop] = 0;
  }
  get type() {
    return this.#type;
  }
  get bubbles() {
    return this.#options.bubbles;
  }
  get cancelable() {
    return this.#options.cancelable;
  }
  get composed() {
    return this.#options.composed;
  }
  get defaultPrevented() {
    return this.#options.cancelable && this.#canceled;
  }
  get returnValue() {
    return !this.defaultPrevented;
  }
  get target() {
    return this.#target;
  }
  get srcElement() {
    return this.#target;
  }
  get currentTarget() {
    return this.#current;
  }
  get eventPhase() {
    return this.#phase;
  }
  get isTrusted() {
    return false;
  }
  get cancelBubble() {
    return this[kStop] > 0;
  }
  set cancelBubble(value) {
    if (value) this.stopPropagation();
  }
  composedPath() {
    return this.#dispatching ? [this.#current] : [];
  }
  preventDefault() {
    this.#canceled = true;
  }
  stopPropagation() {
    this[kStop] = Math.max(this[kStop], 1);
  }
  stopImmediatePropagation() {
    this[kStop] = 2;
  }
  static begin(event, target) {
    if (event.#dispatching) throw new DOMException("The event is already being dispatched", "InvalidStateError");
    event.#dispatching = true;
    event.#target = target;
    event.#current = target;
    event.#phase = 2;
  }
  static end(event) {
    event.#dispatching = false;
    event.#current = null;
    event.#phase = 0;
  }
  get [Symbol.toStringTag]() {
    return "Event";
  }
}
Object.assign(Event, { NONE: 0, CAPTURING_PHASE: 1, AT_TARGET: 2, BUBBLING_PHASE: 3 });

class CustomEvent extends Event {
  #detail;
  constructor(type, options = {}) {
    super(type, options);
    this.#detail = options.detail ?? null;
  }
  get detail() {
    return this.#detail;
  }
}

function reportListenerError(error) {
  queueMicrotask(() => {
    throw error;
  });
}

class EventTarget {
  #listeners = new Map();
  addEventListener(type, listener, options = {}) {
    if (listener === null || listener === undefined) return;
    if (typeof options === "boolean") options = { capture: options };
    const { once = false, capture = false, signal } = options;
    if (signal && signal.aborted) return;
    const list = this.#listeners.get(String(type)) || [];
    if (list.some((entry) => entry.listener === listener && entry.capture === !!capture)) return;
    const entry = { listener, capture: !!capture, once: !!once, removed: false };
    list.push(entry);
    this.#listeners.set(String(type), list);
    if (signal) signal.addEventListener("abort", () => this.removeEventListener(type, listener, options), { once: true });
  }
  removeEventListener(type, listener, options = {}) {
    const capture = typeof options === "boolean" ? options : !!options.capture;
    const list = this.#listeners.get(String(type));
    if (!list) return;
    const index = list.findIndex((entry) => entry.listener === listener && entry.capture === capture);
    if (index >= 0) {
      list[index].removed = true;
      list.splice(index, 1);
    }
  }
  dispatchEvent(event) {
    if (!(event instanceof Event)) throw new TypeError('The "event" argument must be an instance of Event');
    Event.begin(event, this);
    try {
      const list = (this.#listeners.get(event.type) || []).slice();
      for (const entry of list) {
        if (entry.removed) continue;
        if (entry.once) this.removeEventListener(event.type, entry.listener, { capture: entry.capture });
        try {
          const result = typeof entry.listener === "function" ? entry.listener.call(this, event) : entry.listener.handleEvent(event);
          if (result && typeof result.then === "function") result.then(undefined, reportListenerError);
        } catch (error) {
          reportListenerError(error);
        }
        if (event[kStop] === 2) break;
      }
    } finally {
      Event.end(event);
    }
    return !event.defaultPrevented;
  }
  get [Symbol.toStringTag]() {
    return "EventTarget";
  }
}

function abortReason(reason) {
  return reason === undefined ? new DOMException("This operation was aborted", "AbortError") : reason;
}

class AbortSignal extends EventTarget {
  #aborted = false;
  #reason = undefined;
  #onabort = null;
  #onabortListener = null;
  constructor(key) {
    super();
    if (key !== kStop) throw new TypeError("Illegal constructor");
  }
  // An event handler property joins the listener list where it is first set, as in browsers.
  get onabort() {
    return this.#onabort;
  }
  set onabort(handler) {
    this.#onabort = typeof handler === "function" ? handler : null;
    if (this.#onabort && !this.#onabortListener) {
      this.#onabortListener = (event) => {
        if (this.#onabort) this.#onabort.call(this, event);
      };
      this.addEventListener("abort", this.#onabortListener);
    }
  }
  get aborted() {
    return this.#aborted;
  }
  get reason() {
    return this.#reason;
  }
  throwIfAborted() {
    if (this.#aborted) throw this.#reason;
  }
  static abort(reason) {
    const signal = new AbortSignal(kStop);
    signal.#aborted = true;
    signal.#reason = abortReason(reason);
    return signal;
  }
  static timeout(ms) {
    const signal = new AbortSignal(kStop);
    const timer = setTimeout(() => {
      AbortSignal.fire(signal, new DOMException("The operation was aborted due to timeout", "TimeoutError"));
    }, ms);
    if (timer && typeof timer.unref === "function") timer.unref();
    return signal;
  }
  static any(signals) {
    const signal = new AbortSignal(kStop);
    for (const source of signals) {
      if (source.aborted) {
        AbortSignal.fire(signal, source.reason);
        return signal;
      }
    }
    for (const source of signals) source.addEventListener("abort", () => AbortSignal.fire(signal, source.reason), { once: true });
    return signal;
  }
  static fire(signal, reason) {
    if (signal.#aborted) return;
    signal.#aborted = true;
    signal.#reason = abortReason(reason);
    signal.dispatchEvent(new Event("abort"));
  }
  get [Symbol.toStringTag]() {
    return "AbortSignal";
  }
}

class AbortController {
  #signal = new AbortSignal(kStop);
  get signal() {
    return this.#signal;
  }
  abort(reason) {
    AbortSignal.fire(this.#signal, reason);
  }
  get [Symbol.toStringTag]() {
    return "AbortController";
  }
}

function toBytes(input) {
  if (input === undefined) return new Uint8Array(0);
  if (input instanceof ArrayBuffer) return new Uint8Array(input);
  if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  throw Object.assign(new TypeError('The "input" argument must be an instance of ArrayBuffer or ArrayBufferView'), { code: "ERR_INVALID_ARG_TYPE" });
}

class TextEncoder {
  get encoding() {
    return "utf-8";
  }
  encode(input = "") {
    return encoding.encode(String(input), "utf8");
  }
  encodeInto(input, destination) {
    const bytes = encoding.encode(String(input), "utf8");
    let written = Math.min(bytes.length, destination.length);
    while (written > 0 && written < bytes.length && (bytes[written] & 0xc0) === 0x80) written--;
    destination.set(bytes.subarray(0, written));
    const read = encoding.decode(internal.bytesToLatin1(bytes.subarray(0, written)), "utf8").length;
    return { read, written };
  }
  get [Symbol.toStringTag]() {
    return "TextEncoder";
  }
}

const DECODER_LABELS = {
  "utf-8": "utf-8", utf8: "utf-8", "unicode-1-1-utf-8": "utf-8",
  "utf-16le": "utf-16le", "utf-16": "utf-16le", ucs2: "utf-16le",
  latin1: "windows-1252", "iso-8859-1": "windows-1252", ascii: "windows-1252", "us-ascii": "windows-1252", "windows-1252": "windows-1252",
};

// Bytes at the end of `bytes` that start a UTF-8 sequence not yet complete.
function incompleteTail(bytes) {
  for (let back = 1; back <= Math.min(3, bytes.length); back++) {
    const byte = bytes[bytes.length - back];
    if ((byte & 0xc0) === 0x80) continue;
    let needed = 1;
    if (byte >= 0xf0) needed = 4;
    else if (byte >= 0xe0) needed = 3;
    else if (byte >= 0xc0) needed = 2;
    return needed > back ? back : 0;
  }
  return 0;
}

class TextDecoder {
  #encoding;
  #fatal;
  #ignoreBOM;
  #pending = new Uint8Array(0);
  #started = false;
  constructor(label = "utf-8", options = {}) {
    const name = DECODER_LABELS[String(label).trim().toLowerCase()];
    if (!name) throw Object.assign(new RangeError(`The "${label}" encoding is not supported`), { code: "ERR_ENCODING_NOT_SUPPORTED" });
    this.#encoding = name;
    this.#fatal = !!options.fatal;
    this.#ignoreBOM = !!options.ignoreBOM;
  }
  get encoding() {
    return this.#encoding;
  }
  get fatal() {
    return this.#fatal;
  }
  get ignoreBOM() {
    return this.#ignoreBOM;
  }
  decode(input, options = {}) {
    let bytes = toBytes(input);
    if (this.#pending.length) {
      const joined = new Uint8Array(this.#pending.length + bytes.length);
      joined.set(this.#pending);
      joined.set(bytes, this.#pending.length);
      bytes = joined;
    }
    const stream = !!options.stream;
    let keep = 0;
    if (stream && this.#encoding === "utf-8") keep = incompleteTail(bytes);
    else if (stream && this.#encoding === "utf-16le") keep = bytes.length % 2;
    this.#pending = bytes.slice(bytes.length - keep);
    bytes = bytes.subarray(0, bytes.length - keep);
    const first = !this.#started;
    this.#started = stream;
    const latin1 = internal.bytesToLatin1(bytes);
    if (this.#encoding === "utf-8") return encoding.utf8Decode(latin1, this.#fatal, this.#ignoreBOM || !first);
    if (this.#encoding === "windows-1252") return encoding.decode(latin1, "latin1");
    const text = encoding.decode(latin1, "utf16le");
    return first && !this.#ignoreBOM && text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
  }
  get [Symbol.toStringTag]() {
    return "TextDecoder";
  }
}

function cloneError(message) {
  return new DOMException(message, "DataCloneError");
}

function structuredClone(value, options = {}) {
  const seen = new Map();
  const transfer = new Set(options && options.transfer ? options.transfer : []);
  function clone(input) {
    if (input === null || (typeof input !== "object" && typeof input !== "function")) {
      if (typeof input === "symbol") throw cloneError(`${String(input)} could not be cloned.`);
      return input;
    }
    if (typeof input === "function") throw cloneError(`${input.toString().slice(0, 60)} could not be cloned.`);
    if (seen.has(input)) return seen.get(input);
    let output;
    if (input instanceof Date) output = new Date(input.getTime());
    else if (input instanceof RegExp) output = new RegExp(input.source, input.flags);
    else if (input instanceof ArrayBuffer) output = transfer.has(input) ? input : input.slice(0);
    else if (input instanceof DataView) output = new DataView(clone(input.buffer), input.byteOffset, input.byteLength);
    else if (ArrayBuffer.isView(input)) {
      const Constructor = input.constructor.name === "Buffer" ? Uint8Array : input.constructor;
      output = new Constructor(clone(input.buffer), input.byteOffset, input.length);
    } else if (input instanceof Boolean || input instanceof Number || input instanceof String) output = Object(input.valueOf());
    else if (input instanceof Error) {
      const Constructor = globalThis[input.name] && globalThis[input.name].prototype instanceof Error ? globalThis[input.name] : Error;
      output = new Constructor(input.message);
      if (input.stack !== undefined) output.stack = input.stack;
      if ("cause" in input) output.cause = clone(input.cause);
    } else if (input instanceof Map) {
      output = new Map();
      seen.set(input, output);
      for (const [key, item] of input) output.set(clone(key), clone(item));
      return output;
    } else if (input instanceof Set) {
      output = new Set();
      seen.set(input, output);
      for (const item of input) output.add(clone(item));
      return output;
    } else if (input instanceof Promise || input instanceof WeakMap || input instanceof WeakSet) {
      throw cloneError(`#<${input.constructor.name}> could not be cloned.`);
    } else {
      output = Array.isArray(input) ? new Array(input.length) : {};
      seen.set(input, output);
      for (const key of Object.keys(input)) output[key] = clone(input[key]);
      return output;
    }
    seen.set(input, output);
    return output;
  }
  return clone(value);
}

module.exports = {
  DOMException,
  Event,
  CustomEvent,
  EventTarget,
  AbortController,
  AbortSignal,
  TextEncoder,
  TextDecoder,
  structuredClone,
};
