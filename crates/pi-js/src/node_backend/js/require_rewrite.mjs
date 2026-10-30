// Rewrites `require(` calls in CommonJS source as `require.call(void 0, `. Strings, template
// text, comments, and regular expression literals are copied as they are.

const WORD = /[\w$]|[^\x00-\x7f]/;
const REGEX_AFTER = new Set([
  "return", "typeof", "instanceof", "in", "of", "new", "delete", "void",
  "throw", "case", "do", "else", "yield", "await",
]);

export function hideRequireCalls(source) {
  let out = "";
  let i = 0;
  // What came before: "start", "op" (a regex may follow), "value", ".", or a word.
  let last = "start";
  const braces = [];

  const copyUntil = (end) => {
    out += source.slice(i, end);
    i = end;
  };
  const skipQuoted = (quote) => {
    let j = i + 1;
    while (j < source.length && source[j] !== quote && source[j] !== "\n") j += source[j] === "\\" ? 2 : 1;
    copyUntil(Math.min(j + 1, source.length));
  };
  const skipTemplate = () => {
    let j = i;
    while (j < source.length) {
      if (source[j] === "\\") j += 2;
      else if (source[j] === "`") return copyUntil(j + 1), (last = "value");
      else if (source[j] === "$" && source[j + 1] === "{") return copyUntil(j + 2), braces.push(true), (last = "op");
      else j += 1;
    }
    copyUntil(source.length);
  };
  const skipRegex = () => {
    let j = i + 1;
    let inClass = false;
    while (j < source.length && source[j] !== "\n" && (inClass || source[j] !== "/")) {
      if (source[j] === "\\") j += 1;
      else if (source[j] === "[") inClass = true;
      else if (source[j] === "]") inClass = false;
      j += 1;
    }
    j += 1;
    while (j < source.length && WORD.test(source[j])) j += 1;
    copyUntil(Math.min(j, source.length));
  };

  if (source.startsWith("#!")) copyUntil(source.indexOf("\n") < 0 ? source.length : source.indexOf("\n"));
  while (i < source.length) {
    const c = source[i];
    const next = source[i + 1];
    if (/\s/.test(c)) {
      copyUntil(i + 1);
    } else if (c === "/" && next === "/") {
      const end = source.indexOf("\n", i);
      copyUntil(end < 0 ? source.length : end);
    } else if (c === "/" && next === "*") {
      const end = source.indexOf("*/", i + 2);
      copyUntil(end < 0 ? source.length : end + 2);
    } else if (c === "'" || c === '"') {
      skipQuoted(c);
      last = "value";
    } else if (c === "`") {
      copyUntil(i + 1);
      skipTemplate();
    } else if (c === "/") {
      const regex = last === "start" || last === "op" || REGEX_AFTER.has(last);
      if (regex) skipRegex();
      else copyUntil(i + 1);
      last = regex ? "value" : "op";
    } else if (WORD.test(c)) {
      let j = i;
      while (j < source.length && WORD.test(source[j])) j += 1;
      const word = source.slice(i, j);
      const call = /^\s*\(/.exec(source.slice(j, j + 64));
      if (word === "require" && last !== "." && last !== "function" && call) {
        out += "require.call(void 0, ";
        i = j + call[0].length;
        last = "op";
      } else {
        copyUntil(j);
        last = word;
      }
    } else if (c === "." && source.startsWith("...", i)) {
      copyUntil(i + 3);
      last = "op";
    } else if (c === "{") {
      braces.push(false);
      copyUntil(i + 1);
      last = "op";
    } else if (c === "}" && braces.pop()) {
      copyUntil(i + 1);
      skipTemplate();
    } else {
      copyUntil(i + 1);
      last = c === "." ? "." : c === ")" || c === "]" || c === "}" ? "value" : "op";
    }
  }
  return out;
}
