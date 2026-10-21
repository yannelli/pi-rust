// internal/readline_keys: emitKeypressEvents and the escape sequence parser behind it, ported
// from Node's internal/readline/utils.js and emitKeypressEvents.js.
const { StringDecoder } = require("string_decoder");

const kEscape = "\x1b";
const kKeypressDecoder = Symbol("keypress-decoder");
const kEscapeDecoder = Symbol("escape-decoder");
const ESCAPE_CODE_TIMEOUT = 500;

function charLengthLeft(str, i) {
  if (i <= 0) return 0;
  if ((i > 1 && str.codePointAt(i - 2) >= 0x10000) || str.codePointAt(i - 1) >= 0x10000) return 2;
  return 1;
}

function charLengthAt(str, i) {
  if (str.length <= i) return 1;
  return str.codePointAt(i) >= 0x10000 ? 2 : 1;
}

// Key names by escape code; entries with a second field also set that modifier.
const CODES = {
  "[P": "f1", "[Q": "f2", "[R": "f3", "[S": "f4", OP: "f1", OQ: "f2", OR: "f3", OS: "f4",
  "[11~": "f1", "[12~": "f2", "[13~": "f3", "[14~": "f4", "[200~": "paste-start", "[201~": "paste-end",
  "[[A": "f1", "[[B": "f2", "[[C": "f3", "[[D": "f4", "[[E": "f5",
  "[15~": "f5", "[17~": "f6", "[18~": "f7", "[19~": "f8", "[20~": "f9", "[21~": "f10", "[23~": "f11", "[24~": "f12",
  "[A": "up", "[B": "down", "[C": "right", "[D": "left", "[E": "clear", "[F": "end", "[H": "home",
  OA: "up", OB: "down", OC: "right", OD: "left", OE: "clear", OF: "end", OH: "home",
  "[1~": "home", "[2~": "insert", "[3~": "delete", "[4~": "end", "[5~": "pageup", "[6~": "pagedown",
  "[[5~": "pageup", "[[6~": "pagedown", "[7~": "home", "[8~": "end",
  "[a": ["up", "shift"], "[b": ["down", "shift"], "[c": ["right", "shift"], "[d": ["left", "shift"], "[e": ["clear", "shift"],
  "[2$": ["insert", "shift"], "[3$": ["delete", "shift"], "[5$": ["pageup", "shift"], "[6$": ["pagedown", "shift"],
  "[7$": ["home", "shift"], "[8$": ["end", "shift"],
  Oa: ["up", "ctrl"], Ob: ["down", "ctrl"], Oc: ["right", "ctrl"], Od: ["left", "ctrl"], Oe: ["clear", "ctrl"],
  "[2^": ["insert", "ctrl"], "[3^": ["delete", "ctrl"], "[5^": ["pageup", "ctrl"], "[6^": ["pagedown", "ctrl"],
  "[7^": ["home", "ctrl"], "[8^": ["end", "ctrl"], "[Z": ["tab", "shift"],
};

const SIMPLE = { "\r": "return", "\n": "enter", "\t": "tab", "\b": "backspace", "\x7f": "backspace", [kEscape]: "escape", " ": "space" };

function* emitKeys(stream) {
  for (;;) {
    let ch = yield;
    let s = ch;
    let escaped = false;
    const key = { sequence: null, name: undefined, ctrl: false, meta: false, shift: false };
    if (ch === kEscape) {
      escaped = true;
      s += ch = yield;
      if (ch === kEscape) s += ch = yield;
    }
    if (escaped && (ch === "O" || ch === "[")) {
      let code = ch;
      let modifier = 0;
      if (ch === "O") {
        s += ch = yield;
        if (ch >= "0" && ch <= "9") {
          modifier = (ch >> 0) - 1;
          s += ch = yield;
        }
        code += ch;
      } else {
        s += ch = yield;
        if (ch === "[") {
          code += ch;
          s += ch = yield;
        }
        const cmdStart = s.length - 1;
        if (ch >= "0" && ch <= "9") {
          s += ch = yield;
          if (ch >= "0" && ch <= "9") {
            s += ch = yield;
            if (ch >= "0" && ch <= "9") s += ch = yield;
          }
        }
        if (ch === ";") {
          s += ch = yield;
          if (ch >= "0" && ch <= "9") s += yield;
        }
        const cmd = s.slice(cmdStart);
        let match;
        if ((match = /^(?:(\d\d?)(?:;(\d))?([~^$])|(\d{3}~))$/.exec(cmd))) {
          if (match[4]) code += match[4];
          else {
            code += match[1] + match[3];
            modifier = (match[2] || 1) - 1;
          }
        } else if ((match = /^((\d;)?(\d))?([A-Za-z])$/.exec(cmd))) {
          code += match[4];
          modifier = (match[3] || 1) - 1;
        } else {
          code += cmd;
        }
      }
      key.ctrl = !!(modifier & 4);
      key.meta = !!(modifier & 10);
      key.shift = !!(modifier & 1);
      key.code = code;
      const entry = CODES[code];
      if (Array.isArray(entry)) {
        key.name = entry[0];
        key[entry[1]] = true;
      } else {
        key.name = entry || "undefined";
      }
    } else if (SIMPLE[ch] !== undefined) {
      key.name = SIMPLE[ch];
      key.meta = escaped;
    } else if (!escaped && ch <= "\x1a") {
      key.name = String.fromCharCode(ch.charCodeAt(0) + "a".charCodeAt(0) - 1);
      key.ctrl = true;
    } else if (/^[0-9A-Za-z]$/.test(ch)) {
      key.name = ch.toLowerCase();
      key.shift = /^[A-Z]$/.test(ch);
      key.meta = escaped;
    } else if (escaped) {
      key.name = ch.length ? undefined : "escape";
      key.meta = true;
    }
    key.sequence = s;
    if (s.length !== 0 && (key.name !== undefined || escaped)) stream.emit("keypress", escaped ? undefined : s, key);
    else if (charLengthAt(s, 0) === s.length) stream.emit("keypress", s, key);
  }
}

// Make `stream` emit 'keypress' events for its 'data' while anything listens for them.
function emitKeypressEvents(stream, iface = {}) {
  if (stream[kKeypressDecoder]) return;
  stream[kKeypressDecoder] = new StringDecoder("utf8");
  stream[kEscapeDecoder] = emitKeys(stream);
  stream[kEscapeDecoder].next();
  const triggerEscape = () => stream[kEscapeDecoder].next("");
  const { escapeCodeTimeout = ESCAPE_CODE_TIMEOUT } = iface;
  let timeoutId;
  function onData(input) {
    if (stream.listenerCount("keypress") === 0) {
      stream.removeListener("data", onData);
      stream.on("newListener", onNewListener);
      return;
    }
    const string = typeof input === "string" ? input : stream[kKeypressDecoder].write(input);
    if (!string) return;
    clearTimeout(timeoutId);
    iface.isCompletionEnabled = false;
    let length = 0;
    for (const character of string) {
      length += character.length;
      if (length === string.length) iface.isCompletionEnabled = true;
      try {
        stream[kEscapeDecoder].next(character);
        if (length === string.length && character === kEscape) timeoutId = setTimeout(triggerEscape, escapeCodeTimeout);
      } catch (error) {
        stream[kEscapeDecoder] = emitKeys(stream);
        stream[kEscapeDecoder].next();
        throw error;
      }
    }
  }
  function onNewListener(event) {
    if (event !== "keypress") return;
    stream.on("data", onData);
    stream.removeListener("newListener", onNewListener);
  }
  if (stream.listenerCount("keypress") > 0) stream.on("data", onData);
  else stream.on("newListener", onNewListener);
}

module.exports = { emitKeypressEvents, charLengthAt, charLengthLeft };
