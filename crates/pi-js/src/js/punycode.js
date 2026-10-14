// node:punycode (deprecated): RFC 3492 Punycode and IDNA labels, as in punycode.js 2.1.0.
process.emitWarning(
  "The `punycode` module is deprecated. Please use a userland alternative instead.",
  "DeprecationWarning",
  "DEP0040",
);

const maxInt = 2147483647;
const base = 36;
const tMin = 1;
const tMax = 26;
const skew = 38;
const damp = 700;
const initialBias = 72;
const initialN = 128;
const delimiter = "-";
const regexPunycode = /^xn--/;
const regexNonASCII = /[^\0-\x7F]/;
const regexSeparators = /[\x2E。．｡]/g;
const errors = {
  overflow: "Overflow: input needs wider integers to process",
  "not-basic": "Illegal input >= 0x80 (not a basic code point)",
  "invalid-input": "Invalid input",
};

function error(type) {
  throw new RangeError(errors[type]);
}

function mapDomain(domain, callback) {
  const parts = domain.split("@");
  let result = "";
  if (parts.length > 1) {
    result = parts[0] + "@";
    domain = parts[1];
  }
  domain = domain.replace(regexSeparators, "\x2E");
  const encoded = domain.split(".").map(callback).join(".");
  return result + encoded;
}

function ucs2decode(string) {
  const output = [];
  let counter = 0;
  const length = string.length;
  while (counter < length) {
    const value = string.charCodeAt(counter++);
    if (value >= 0xd800 && value <= 0xdbff && counter < length) {
      const extra = string.charCodeAt(counter++);
      if ((extra & 0xfc00) === 0xdc00) {
        output.push(((value & 0x3ff) << 10) + (extra & 0x3ff) + 0x10000);
      } else {
        output.push(value);
        counter--;
      }
    } else {
      output.push(value);
    }
  }
  return output;
}

const ucs2encode = (codePoints) => String.fromCodePoint(...codePoints);

function basicToDigit(codePoint) {
  if (codePoint >= 0x30 && codePoint < 0x3a) return 26 + (codePoint - 0x30);
  if (codePoint >= 0x41 && codePoint < 0x5b) return codePoint - 0x41;
  if (codePoint >= 0x61 && codePoint < 0x7b) return codePoint - 0x61;
  return base;
}

function digitToBasic(digit, flag) {
  return digit + 22 + 75 * (digit < 26) - ((flag !== 0) << 5);
}

function adapt(delta, numPoints, firstTime) {
  let k = 0;
  delta = firstTime ? Math.floor(delta / damp) : delta >> 1;
  delta += Math.floor(delta / numPoints);
  for (; delta > ((base - tMin) * tMax) >> 1; k += base) {
    delta = Math.floor(delta / (base - tMin));
  }
  return Math.floor(k + ((base - tMin + 1) * delta) / (delta + skew));
}

function decode(input) {
  const output = [];
  const inputLength = input.length;
  let i = 0;
  let n = initialN;
  let bias = initialBias;
  let basic = input.lastIndexOf(delimiter);
  if (basic < 0) basic = 0;
  for (let j = 0; j < basic; ++j) {
    if (input.charCodeAt(j) >= 0x80) error("not-basic");
    output.push(input.charCodeAt(j));
  }
  for (let index = basic > 0 ? basic + 1 : 0; index < inputLength; ) {
    const oldi = i;
    for (let w = 1, k = base; ; k += base) {
      if (index >= inputLength) error("invalid-input");
      const digit = basicToDigit(input.charCodeAt(index++));
      if (digit >= base) error("invalid-input");
      if (digit > Math.floor((maxInt - i) / w)) error("overflow");
      i += digit * w;
      const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
      if (digit < t) break;
      const baseMinusT = base - t;
      if (w > Math.floor(maxInt / baseMinusT)) error("overflow");
      w *= baseMinusT;
    }
    const out = output.length + 1;
    bias = adapt(i - oldi, out, oldi === 0);
    if (Math.floor(i / out) > maxInt - n) error("overflow");
    n += Math.floor(i / out);
    i %= out;
    output.splice(i++, 0, n);
  }
  return String.fromCodePoint(...output);
}

function encode(input) {
  const output = [];
  input = ucs2decode(input);
  const inputLength = input.length;
  let n = initialN;
  let delta = 0;
  let bias = initialBias;
  for (const currentValue of input) {
    if (currentValue < 0x80) output.push(String.fromCharCode(currentValue));
  }
  const basicLength = output.length;
  let handledCPCount = basicLength;
  if (basicLength) output.push(delimiter);
  while (handledCPCount < inputLength) {
    let m = maxInt;
    for (const currentValue of input) {
      if (currentValue >= n && currentValue < m) m = currentValue;
    }
    const handledCPCountPlusOne = handledCPCount + 1;
    if (m - n > Math.floor((maxInt - delta) / handledCPCountPlusOne)) error("overflow");
    delta += (m - n) * handledCPCountPlusOne;
    n = m;
    for (const currentValue of input) {
      if (currentValue < n && ++delta > maxInt) error("overflow");
      if (currentValue === n) {
        let q = delta;
        for (let k = base; ; k += base) {
          const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
          if (q < t) break;
          const qMinusT = q - t;
          const baseMinusT = base - t;
          output.push(String.fromCharCode(digitToBasic(t + (qMinusT % baseMinusT), 0)));
          q = Math.floor(qMinusT / baseMinusT);
        }
        output.push(String.fromCharCode(digitToBasic(q, 0)));
        bias = adapt(delta, handledCPCountPlusOne, handledCPCount === basicLength);
        delta = 0;
        ++handledCPCount;
      }
    }
    ++delta;
    ++n;
  }
  return output.join("");
}

function toUnicode(input) {
  return mapDomain(input, (string) => (regexPunycode.test(string) ? decode(string.slice(4).toLowerCase()) : string));
}

function toASCII(input) {
  return mapDomain(input, (string) => (regexNonASCII.test(string) ? "xn--" + encode(string) : string));
}

module.exports = {
  version: "2.1.0",
  ucs2: { decode: ucs2decode, encode: ucs2encode },
  decode,
  encode,
  toASCII,
  toUnicode,
};
