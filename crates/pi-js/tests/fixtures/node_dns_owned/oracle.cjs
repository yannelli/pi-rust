// Runs one fixture body under real Node the way `run_cjs` runs it in pi-js and prints the JSON.
// Usage: node oracle.cjs <case.js> [expected.json]; with expected.json it exits 1 on a difference.
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

async function main() {
  const [casePath, expectedPath] = process.argv.slice(2);
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "pi-dns-oracle-")));
  fs.copyFileSync(path.join(__dirname, "dns_server.cjs"), path.join(root, "dns_server.cjs"));
  const body = fs.readFileSync(casePath, "utf8");
  fs.writeFileSync(path.join(root, "case-0.cjs"), `module.exports = async function () {\n${body}\n};\n`);
  const actual = await require(path.join(root, "case-0.cjs"))();
  fs.rmSync(root, { recursive: true, force: true });
  const text = JSON.stringify(actual);
  if (!expectedPath) {
    process.stdout.write(`${text}\n`);
    return 0;
  }
  const expected = JSON.parse(fs.readFileSync(expectedPath, "utf8"));
  // Cases skip these when the host has no ::1 or no 127.0.0.2.
  for (const key of ["v6Servers", "v6Answer", "boundSecond"]) if (!(key in actual)) delete expected[key];
  const keys = new Set([...Object.keys(expected), ...Object.keys(actual)]);
  const differing = [...keys].filter((key) => JSON.stringify(actual[key]) !== JSON.stringify(expected[key]));
  for (const key of differing) process.stdout.write(`${key}: actual ${JSON.stringify(actual[key])} expected ${JSON.stringify(expected[key])}\n`);
  return differing.length ? 1 : 0;
}

main().then(
  (code) => process.exit(code),
  (error) => {
    process.stderr.write(`${error.stack}\n`);
    process.exit(2);
  },
);
