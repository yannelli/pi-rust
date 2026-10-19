process.execArgv.push("--test-reporter=spec");
const { test, describe, it } = require("node:test");
test("sync pass", () => {});
test("skipped", { skip: "why" }, () => {});
test("todo one", { todo: true }, () => {});
test("parent", async (t) => {
  await t.test("child 1", () => {});
  await t.test("child 2", () => { throw new Error("boom"); });
});
describe("suite", () => {
  it("a", () => {});
  it.todo("b");
  it("c", (t) => { t.diagnostic("diag"); });
});
