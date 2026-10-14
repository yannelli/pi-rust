// util.MIMEType and util.MIMEParams, following Node's WHATWG MIME sniffing parser.
const NOT_HTTP_TOKEN_CODE_POINT = /[^!#$%&'*+\-.^_`|~A-Za-z0-9]/;
const NOT_HTTP_QUOTED_STRING_CODE_POINT = /[^\t\u0020-~\u0080-\u00FF]/;
const END_BEGINNING_WHITESPACE = /[^\r\n\t ]|$/;
const START_ENDING_WHITESPACE = /[\r\n\t ]*$/;
const EQUALS_SEMICOLON_OR_END = /[;=]|$/;
const QUOTED_VALUE_PATTERN = /^(?:([\\]$)|[\\][\s\S]|[^"])*(?:(")|$)/u;

function invalidSyntax(production, str, index) {
  const at = index === -1 ? "" : ` at ${index}`;
  return Object.assign(new TypeError(`The MIME syntax for a ${production} in "${str}" is invalid${at}`), { code: "ERR_INVALID_MIME_SYNTAX" });
}

function toASCIILower(str) {
  return str.replace(/[A-Z]/g, (c) => String.fromCharCode(c.charCodeAt(0) + 32));
}

function checkToken(production, value) {
  const index = value.search(NOT_HTTP_TOKEN_CODE_POINT);
  if (value.length === 0 || index !== -1) throw invalidSyntax(production, value, index);
  return toASCIILower(value);
}

function parseTypeAndSubtype(str) {
  let position = str.search(END_BEGINNING_WHITESPACE);
  const typeEnd = str.indexOf("/", position);
  const trimmedType = typeEnd === -1 ? str.slice(position) : str.slice(position, typeEnd);
  const invalidTypeIndex = trimmedType.search(NOT_HTTP_TOKEN_CODE_POINT);
  if (trimmedType === "" || invalidTypeIndex !== -1 || typeEnd === -1) throw invalidSyntax("type", str, invalidTypeIndex);
  position = typeEnd + 1;
  const subtypeEnd = str.indexOf(";", position);
  const rawSubtype = subtypeEnd === -1 ? str.slice(position) : str.slice(position, subtypeEnd);
  position += rawSubtype.length + (subtypeEnd === -1 ? 0 : 1);
  const trimmedSubtype = rawSubtype.slice(0, rawSubtype.search(START_ENDING_WHITESPACE));
  const invalidSubtypeIndex = trimmedSubtype.search(NOT_HTTP_TOKEN_CODE_POINT);
  if (trimmedSubtype === "" || invalidSubtypeIndex !== -1) throw invalidSyntax("subtype", str, invalidSubtypeIndex);
  return [toASCIILower(trimmedType), toASCIILower(trimmedSubtype), position];
}

function removeBackslashes(str) {
  let out = "";
  let i;
  for (i = 0; i < str.length - 1; i++) {
    if (str[i] === "\\") out += str[++i];
    else out += str[i];
  }
  if (i === str.length - 1) out += str[i];
  return out;
}

function encodeValue(value) {
  if (value.length === 0) return '""';
  if (value.search(NOT_HTTP_TOKEN_CODE_POINT) === -1) return value;
  return `"${value.replace(/["\\]/g, (c) => `\\${c}`)}"`;
}

function parseParams(str, data) {
  let position = 0;
  const end = str.search(START_ENDING_WHITESPACE);
  while (position < end) {
    position += str.slice(position).search(END_BEGINNING_WHITESPACE);
    const afterName = str.slice(position).search(EQUALS_SEMICOLON_OR_END) + position;
    const name = toASCIILower(str.slice(position, afterName));
    position = afterName;
    if (position < end) {
      const char = str.charAt(position);
      position += 1;
      if (char === ";") continue;
    }
    if (position >= end) break;
    let value;
    if (str.charAt(position) === '"') {
      position += 1;
      const match = QUOTED_VALUE_PATTERN.exec(str.slice(position));
      position += match[0].length;
      const inside = match[1] || match[2] ? match[0].slice(0, -1) : match[0];
      value = removeBackslashes(inside) + (match[1] ? "\\" : "");
    } else {
      const valueEnd = str.indexOf(";", position);
      const raw = valueEnd === -1 ? str.slice(position) : str.slice(position, valueEnd);
      position += raw.length;
      value = raw.slice(0, raw.search(START_ENDING_WHITESPACE));
      if (value === "") continue;
    }
    if (name !== "" && name.search(NOT_HTTP_TOKEN_CODE_POINT) === -1 && value.search(NOT_HTTP_QUOTED_STRING_CODE_POINT) === -1 && !data.has(name)) {
      data.set(name, value);
    }
    position++;
  }
}

let pendingParams = null;

class MIMEParams {
  #data = new Map();
  #source = null;

  constructor() {
    this.#source = pendingParams;
    pendingParams = null;
  }

  #parse() {
    if (this.#source === null) return this.#data;
    parseParams(this.#source, this.#data);
    this.#source = null;
    return this.#data;
  }

  delete(name) {
    this.#parse().delete(name);
  }

  get(name) {
    const data = this.#parse();
    return data.has(name) ? data.get(name) : null;
  }

  has(name) {
    return this.#parse().has(name);
  }

  set(name, value) {
    const data = this.#parse();
    name = `${name}`;
    value = `${value}`;
    const nameIndex = name.search(NOT_HTTP_TOKEN_CODE_POINT);
    if (name.length === 0 || nameIndex !== -1) throw invalidSyntax("parameter name", name, nameIndex);
    const valueIndex = value.search(NOT_HTTP_QUOTED_STRING_CODE_POINT);
    if (valueIndex !== -1) throw invalidSyntax("parameter value", value, valueIndex);
    data.set(name, value);
  }

  *entries() {
    yield* this.#parse().entries();
  }

  *keys() {
    yield* this.#parse().keys();
  }

  *values() {
    yield* this.#parse().values();
  }

  toString() {
    return Array.from(this.#parse(), ([key, value]) => `${key}=${encodeValue(value)}`).join(";");
  }

  toJSON() {
    return this.toString();
  }

  [Symbol.iterator]() {
    return this.entries();
  }
}

class MIMEType {
  #type;
  #subtype;
  #params;

  constructor(input) {
    const str = `${input}`;
    const [type, subtype, position] = parseTypeAndSubtype(str);
    this.#type = type;
    this.#subtype = subtype;
    pendingParams = str.slice(position);
    this.#params = new MIMEParams();
  }

  get type() {
    return this.#type;
  }

  set type(value) {
    this.#type = checkToken("type", `${value}`);
  }

  get subtype() {
    return this.#subtype;
  }

  set subtype(value) {
    this.#subtype = checkToken("subtype", `${value}`);
  }

  get essence() {
    return `${this.#type}/${this.#subtype}`;
  }

  get params() {
    return this.#params;
  }

  toString() {
    const params = this.#params.toString();
    return `${this.#type}/${this.#subtype}${params ? `;${params}` : ""}`;
  }

  toJSON() {
    return this.toString();
  }
}

module.exports = { MIMEType, MIMEParams };
