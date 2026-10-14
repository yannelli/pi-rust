const { run } = require("node:test");
const { tap, dot, junit, spec } = require("node:test/reporters");
const path = require("path");
const files = [path.join(__dirname, "runfiles/a.test.js"), path.join(__dirname, "runfiles/b.test.cjs")];
async function collect(reporter) {
  let out = "";
  for await (const chunk of run({ files }).compose(reporter)) out += chunk;
  return out;
}
(async () => {
  const events = [];
  const stream = run({ files });
  for (const type of ["test:start", "test:pass", "test:fail", "test:plan", "test:diagnostic"]) {
    stream.on(type, (data) => events.push(`${type} ${data.name ?? data.count ?? data.message.split(" ")[0]} ${data.nesting}`));
  }
  for await (const event of stream);
  const output = { events, tap: await collect(tap), dot: await collect(dot), spec: await collect(new spec()) };
  const xml = await collect(junit);
  output.junit = [xml.startsWith('<?xml version="1.0" encoding="utf-8"?>\n<testsuites>\n'), xml.includes('<testcase name="a1"'), xml.includes('failure="a2 fails"'), xml.includes("<!-- tests 3 -->")];
  process.stdout.write(JSON.stringify(output));
})();
