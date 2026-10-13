// Deep equality shared by assert.deepEqual, assert.deepStrictEqual, and util.isDeepStrictEqual.
function isPrimitive(value) {
  return value === null || (typeof value !== "object" && typeof value !== "function");
}

function looseEqual(a, b) {
  // eslint-disable-next-line eqeqeq
  return a == b || (Number.isNaN(a) && Number.isNaN(b));
}

function deepEqual(a, b, strict, memo) {
  if (strict ? Object.is(a, b) : a === b) return true;
  if (isPrimitive(a) || isPrimitive(b)) return !strict && isPrimitive(a) && isPrimitive(b) && looseEqual(a, b);
  if (strict && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
  const tagA = Object.prototype.toString.call(a);
  if (tagA !== Object.prototype.toString.call(b)) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (a instanceof Date) {
    if (a.getTime() !== b.getTime()) return false;
  } else if (a instanceof RegExp) {
    if (a.source !== b.source || a.flags !== b.flags || a.lastIndex !== b.lastIndex) return false;
  } else if (a instanceof Error) {
    if (a.message !== b.message || a.name !== b.name) return false;
  } else if (ArrayBuffer.isView(a) && !(a instanceof DataView)) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      if (strict ? !Object.is(a[i], b[i]) : a[i] != b[i]) return false; // eslint-disable-line eqeqeq
    }
  } else if (a instanceof ArrayBuffer || a instanceof DataView) {
    const viewA = new Uint8Array(a instanceof DataView ? a.buffer.slice(a.byteOffset, a.byteOffset + a.byteLength) : a);
    const viewB = new Uint8Array(b instanceof DataView ? b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength) : b);
    if (viewA.length !== viewB.length || viewA.some((byte, i) => byte !== viewB[i])) return false;
  } else if (a instanceof Number || a instanceof String || a instanceof Boolean) {
    if (!Object.is(a.valueOf(), b.valueOf())) return false;
  }
  memo = memo || { a: [], b: [] };
  const seen = memo.a.indexOf(a);
  if (seen !== -1 && memo.b[seen] === b) return true;
  memo.a.push(a);
  memo.b.push(b);
  try {
    if (a instanceof Map) {
      if (a.size !== b.size) return false;
      for (const [key, value] of a) {
        if (b.has(key)) {
          if (!deepEqual(value, b.get(key), strict, memo)) return false;
        } else if (isPrimitive(key) || !findMapEntry(b, key, value, strict, memo)) {
          return false;
        }
      }
    } else if (a instanceof Set) {
      if (a.size !== b.size) return false;
      for (const value of a) {
        if (b.has(value)) continue;
        if (isPrimitive(value) && strict) return false;
        let found = false;
        for (const other of b) {
          if (deepEqual(value, other, strict, memo)) {
            found = true;
            break;
          }
        }
        if (!found) return false;
      }
    }
    return keysEqual(a, b, strict, memo);
  } finally {
    memo.a.pop();
    memo.b.pop();
  }
}

function findMapEntry(map, key, value, strict, memo) {
  for (const [otherKey, otherValue] of map) {
    if (deepEqual(key, otherKey, strict, memo) && deepEqual(value, otherValue, strict, memo)) return true;
  }
  return false;
}

function ownEnumerable(value, strict) {
  const keys = Object.keys(value);
  if (!strict) return keys;
  return keys.concat(Object.getOwnPropertySymbols(value).filter((symbol) => Object.prototype.propertyIsEnumerable.call(value, symbol)));
}

function keysEqual(a, b, strict, memo) {
  const isView = ArrayBuffer.isView(a) && !(a instanceof DataView);
  const keysA = ownEnumerable(a, strict).filter((key) => !isView || !/^\d+$/.test(key));
  const keysB = ownEnumerable(b, strict).filter((key) => !isView || !/^\d+$/.test(key));
  if (keysA.length !== keysB.length) return false;
  for (const key of keysA) {
    if (!Object.prototype.propertyIsEnumerable.call(b, key)) return false;
    if (!deepEqual(a[key], b[key], strict, memo)) return false;
  }
  return true;
}

module.exports = {
  isDeepEqual: (a, b) => deepEqual(a, b, false),
  isDeepStrictEqual: (a, b) => deepEqual(a, b, true),
};
