// node:test/reporters: tap, spec, dot, junit, and lcov over the runner's { type, data } events.
// Each reporter is a formatter of one event at a time, so the runner can also print directly.
const { Transform } = require("stream");
const { inspect } = require("util");
const path = require("path");

const SYMBOLS = {
  "test:fail": "✖ ",
  "test:pass": "✔ ",
  "test:diagnostic": "ℹ ",
  "arrow:right": "▶ ",
  "hyphen:minus": "﹣ ",
  "warning:alert": "⚠ ",
};
const LINE_BREAK = /\n|\r\n/;
const FRAME_START = /^ {4}at /;
const UNWRAP = new Set(["testCodeFailure", "hookFailed", "uncaughtException", "unhandledRejection"]);

function tapEscape(input) {
  return String(input)
    .replaceAll("\b", "\\b")
    .replaceAll("\f", "\\f")
    .replaceAll("\t", "\\t")
    .replaceAll("\n", "\\n")
    .replaceAll("\r", "\\r")
    .replaceAll("\v", "\\v")
    .replaceAll("\\", "\\\\")
    .replaceAll("#", "\\#");
}

function isError(value) {
  return value instanceof Error || Object.prototype.toString.call(value) === "[object Error]";
}

function isAssertionError(value) {
  return isError(value) && value.code === "ERR_ASSERTION";
}

function yamlValue(indent, name, value, seen) {
  if (value === undefined) return "";
  const prefix = `${indent}  ${name}:`;
  if (value === null) return `${prefix} ~\n`;
  if (typeof value !== "object") {
    if (typeof value !== "string") return `${prefix} ${inspect(value, { colors: false, breakLength: Infinity })}\n`;
    const lines = value.split(LINE_BREAK);
    if (lines.length === 1) return `${prefix} ${inspect(value, { colors: false, breakLength: Infinity })}\n`;
    return `${prefix} |-\n` + lines.map((line) => `${indent}    ${line}\n`).join("");
  }
  return toYaml(indent, name, value, seen);
}

function toYaml(indent, name, value, seen) {
  seen.add(value);
  const errorObject = isError(value);
  let result = "";
  let propsIndent = indent;
  if (name != null) {
    result += `${indent}  ${name}:${value instanceof Date ? ` ${value.toISOString()}` : ""}\n`;
    propsIndent += "  ";
  }
  for (const [key, entry] of Object.entries(value)) {
    if (errorObject && (key === "cause" || key === "code")) continue;
    if (seen.has(entry)) result += `${propsIndent}  ${key}: <Circular>\n`;
    else result += yamlValue(propsIndent, key, entry, seen);
  }
  if (!errorObject) return result;
  const { cause, failureType } = value;
  let { code, message, name: errorName, stack, expected, actual, operator } = value;
  let assertion = isAssertionError(value);
  if (code === "ERR_TEST_FAILURE" && UNWRAP.has(failureType)) {
    stack = cause?.stack ?? stack;
    code = cause?.code ?? code;
    errorName = cause?.name ?? errorName;
    message = cause?.message ?? message;
    if (isAssertionError(cause)) {
      ({ expected, actual } = cause);
      operator = cause.operator ?? operator;
      assertion = true;
    }
  }
  result += yamlValue(indent, "error", message ?? "<unknown error>", seen);
  if (code) result += yamlValue(indent, "code", code, seen);
  if (errorName && errorName !== "Error") result += yamlValue(indent, "name", errorName, seen);
  if (assertion) {
    result += yamlValue(indent, "expected", expected, new Set(seen));
    result += yamlValue(indent, "actual", actual, new Set(seen));
    if (operator) result += yamlValue(indent, "operator", operator, seen);
  }
  if (typeof stack === "string") {
    const frames = [];
    for (const frame of stack.split(LINE_BREAK)) {
      const processed = frame.replace(FRAME_START, "");
      if (processed.length > 0 && processed.length !== frame.length) frames.push(processed);
    }
    if (frames.length > 0) result += `${indent}  stack: |-\n` + frames.map((frame) => `${indent}    ${frame}\n`).join("");
  }
  return result;
}

function tapIndent(nesting) {
  return "    ".repeat(Math.max(0, nesting));
}

function tapTest(nesting, number, status, name, skip, todo) {
  let line = `${tapIndent(nesting)}${status} ${number}`;
  if (name) line += ` ${tapEscape(`- ${name}`)}`;
  if (skip !== undefined) line += ` # SKIP${typeof skip === "string" && skip.length ? ` ${tapEscape(skip)}` : ""}`;
  else if (todo !== undefined) line += ` # TODO${typeof todo === "string" && todo.length ? ` ${tapEscape(todo)}` : ""}`;
  return line + "\n";
}

function tapDetails(nesting, details = {}, location) {
  const indent = tapIndent(nesting);
  let text = `${indent}  ---\n`;
  text += yamlValue(indent, "duration_ms", details.duration_ms, new Set());
  text += yamlValue(indent, "type", details.type, new Set());
  if (location) text += yamlValue(indent, "location", location, new Set());
  if (details.error) text += toYaml(indent, null, details.error, new Set());
  return text + `${indent}  ...\n`;
}

function tapFormatter() {
  return {
    head: () => "TAP version 13\n",
    format({ type, data }) {
      switch (type) {
        case "test:fail": {
          const location = data.file ? `${data.file}:${data.line}:${data.column}` : null;
          return tapTest(data.nesting, data.testNumber, "not ok", data.name, data.skip, data.todo) + tapDetails(data.nesting, data.details, location);
        }
        case "test:pass":
          return tapTest(data.nesting, data.testNumber, "ok", data.name, data.skip, data.todo) + tapDetails(data.nesting, data.details, null);
        case "test:plan":
          return `${tapIndent(data.nesting)}1..${data.count}\n`;
        case "test:start":
          return `${tapIndent(data.nesting)}# Subtest: ${tapEscape(data.name)}\n`;
        case "test:stdout":
        case "test:stderr":
          return data.message.split(LINE_BREAK).filter((line) => line.length > 0).map((line) => `# ${tapEscape(line)}\n`).join("");
        case "test:diagnostic":
          return `${tapIndent(data.nesting)}# ${tapEscape(data.message)}\n`;
      }
      return "";
    },
    flush: () => "",
  };
}

function specIndent(nesting) {
  return "  ".repeat(Math.max(0, nesting));
}

function formatError(error, indent) {
  if (!error) return "";
  const err = error.code === "ERR_TEST_FAILURE" ? error.cause : error;
  const message = inspect(err, { colors: false }).split(/\r?\n/).join(`\n${indent}  `);
  return `\n${indent}  ${message}\n`;
}

function formatTestReport(type, data, prefix = "", indent = "", hasChildren = false, showErrors = true) {
  let symbol = SYMBOLS[type] ?? " ";
  const { skip, todo } = data;
  const duration = data.details?.duration_ms ? ` (${data.details.duration_ms}ms)` : "";
  let title = `${data.name}${duration}`;
  if (skip !== undefined) {
    title += ` # ${typeof skip === "string" && skip.length ? skip : "SKIP"}`;
    symbol = SYMBOLS["hyphen:minus"];
  } else if (todo !== undefined) {
    title += ` # ${typeof todo === "string" && todo.length ? todo : "TODO"}`;
    if (type === "test:fail") symbol = SYMBOLS["warning:alert"];
  }
  const error = showErrors ? formatError(data.details?.error, indent) : "";
  const tail = hasChildren ? (!error || data.details?.error?.failureType === "subtestsFailed" ? "" : `\n${error}`) : error;
  return `${prefix}${indent}${symbol}${title}${tail}`;
}

function specFormatter() {
  const stack = [];
  const reported = [];
  let failed = [];
  const cwd = process.cwd();
  function failures() {
    if (failed.length === 0) return "";
    const lines = [`\n${SYMBOLS["test:fail"]}failing tests:\n`];
    for (const test of failed) {
      if (test.file) lines.push(`test at ${path.relative(cwd, test.file)}:${test.line}:${test.column}`);
      lines.push(formatTestReport("test:fail", test));
    }
    failed = [];
    return lines.join("\n");
  }
  function report(type, data) {
    stack.shift();
    let prefix = "";
    while (stack.length) {
      const parent = stack.pop().data;
      reported.unshift(parent);
      prefix += `${specIndent(parent.nesting)}${SYMBOLS["arrow:right"]}${parent.name}\n`;
    }
    let hasChildren = false;
    if (reported[0] && reported[0].nesting === data.nesting && reported[0].name === data.name) {
      reported.shift();
      hasChildren = true;
    }
    return `${formatTestReport(type, data, prefix, specIndent(data.nesting), hasChildren, false)}\n`;
  }
  return {
    head: () => "",
    format({ type, data }) {
      switch (type) {
        case "test:fail":
          if (data.details?.error?.failureType !== "subtestsFailed") failed.push(data);
          return report(type, data);
        case "test:pass":
          return report(type, data);
        case "test:start":
          stack.unshift({ type, data });
          return "";
        case "test:stdout":
        case "test:stderr":
          return data.message;
        case "test:diagnostic":
          return `${specIndent(data.nesting)}${SYMBOLS[type]}${data.message}\n`;
        case "test:summary":
          return data.file === undefined ? failures() : "";
      }
      return "";
    },
    flush: failures,
  };
}

function dotFormatter() {
  let count = 0;
  const failed = [];
  const columns = Math.max(process.stdout.columns ?? 20, 20);
  return {
    head: () => "",
    format({ type, data }) {
      if (type !== "test:pass" && type !== "test:fail") return "";
      let text = type === "test:pass" ? "." : "X";
      if (type === "test:fail") failed.push(data);
      if (++count === columns) {
        text += "\n";
        count = 0;
      }
      return text;
    },
    flush() {
      let text = "\n";
      if (failed.length > 0) text += "\nFailed tests:\n\n" + failed.map((test) => formatTestReport("test:fail", test)).join("");
      return text;
    },
  };
}

function escapeContent(value = "") {
  return String(value).replace(/&/g, "&amp;").replace(/</g, "&lt;");
}

function treeToXml(tree) {
  if (typeof tree === "string") return `${escapeContent(tree)}\n`;
  const { tag, attrs, nesting, children, comment } = tree;
  const indent = "\t".repeat(nesting + 1);
  if (comment) return `${indent}<!-- ${String(comment).replace(/--/g, "&#45;&#45;")} -->\n`;
  const attributes = Object.entries(attrs)
    .map(([key, value]) => `${key}="${escapeContent(String(value).replace(/\n/g, "").replace(/"/g, "&quot;"))}"`)
    .join(" ");
  if (!children?.length) return `${indent}<${tag} ${attributes}/>\n`;
  return `${indent}<${tag} ${attributes}>\n${children.map(treeToXml).join("")}${indent}</${tag}>\n`;
}

function junitFormatter() {
  const hostname = require("os").hostname();
  const roots = [];
  let current = null;
  const isFailure = (node) => node?.children?.some((child) => child.tag === "failure") || node?.attrs?.failures;
  const isSkipped = (node) => node?.children?.some((child) => child.tag === "skipped") || node?.attrs?.skipped;
  function startTest(data) {
    const parent = current;
    current = { attrs: { name: data.name }, nesting: data.nesting, parent, children: [] };
    if (parent?.children) parent.children.push(current);
    if (!current.parent) roots.push(current);
  }
  function finish(type, data) {
    if (!current) startTest({ name: "root", nesting: 0 });
    if (current.attrs.name !== data.name || current.nesting !== data.nesting) startTest(data);
    const test = current;
    if (current?.nesting === data.nesting) current = current.parent;
    test.attrs.time = ((data.details?.duration_ms ?? 0) / 1000).toFixed(6);
    const real = test.children.filter((child) => child.comment == null);
    if (real.length > 0) {
      test.tag = "testsuite";
      Object.assign(test.attrs, { disabled: 0, errors: 0, tests: real.length });
      test.attrs.failures = test.children.filter(isFailure).length;
      test.attrs.skipped = test.children.filter(isSkipped).length;
      test.attrs.hostname = hostname;
      return;
    }
    test.tag = "testcase";
    test.attrs.classname = data.classname ?? "test";
    const nesting = data.nesting + 1;
    if (data.skip) test.children.push({ nesting, tag: "skipped", attrs: { type: "skipped", message: data.skip } });
    if (data.todo) test.children.push({ nesting, tag: "skipped", attrs: { type: "todo", message: data.todo } });
    if (type === "test:fail") {
      const error = data.details?.error;
      const attrs = { type: error?.failureType || error?.code, message: error?.message ?? "" };
      test.children.push({ nesting, tag: "failure", attrs, children: [inspect(error, { colors: false, breakLength: Infinity })] });
      test.failures = 1;
      test.attrs.failure = error?.message ?? "";
    }
  }
  return {
    head: () => '<?xml version="1.0" encoding="utf-8"?>\n<testsuites>\n',
    format({ type, data }) {
      if (type === "test:start") startTest(data);
      else if (type === "test:pass" || type === "test:fail") finish(type, data);
      else if (type === "test:diagnostic") (current?.children ?? roots).push({ nesting: data.nesting, comment: data.message });
      return "";
    },
    flush: () => roots.map(treeToXml).join("") + "</testsuites>\n",
  };
}

function generator(factory) {
  return async function* reporter(source) {
    const formatter = factory();
    const head = formatter.head();
    if (head) yield head;
    for await (const event of source) {
      const text = formatter.format(event);
      if (text) yield text;
    }
    const tail = formatter.flush();
    if (tail) yield tail;
  };
}

class SpecReporter extends Transform {
  #formatter = specFormatter();
  constructor() {
    super({ writableObjectMode: true });
  }
  _transform(event, encoding, callback) {
    callback(null, this.#formatter.format(event) || undefined);
  }
  _flush(callback) {
    callback(null, this.#formatter.flush() || undefined);
  }
}

async function* lcov() {
  throw internal.unsupported("node:test coverage (the lcov reporter)");
}

const formatters = { tap: tapFormatter, spec: specFormatter, dot: dotFormatter, junit: junitFormatter };

module.exports = {
  dot: generator(dotFormatter),
  junit: generator(junitFormatter),
  spec: SpecReporter,
  tap: generator(tapFormatter),
  lcov,
};
Object.defineProperty(module.exports, "formatters", { value: formatters });
