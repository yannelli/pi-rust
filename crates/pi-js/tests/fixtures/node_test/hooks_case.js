const { test, describe, it, before, after, beforeEach, afterEach } = require("node:test");
const order = (globalThis.order = []);
beforeEach((t) => order.push("root beforeEach " + t.name));
afterEach((t) => order.push("root afterEach " + t.name));
before(() => order.push("root before"));
after(() => order.push("root after"));
describe.skip("skipped suite", () => { order.push("skipped suite body"); it("inner", () => order.push("inner ran")); });
describe("outer", () => {
  order.push("outer body");
  before(() => order.push("outer before"));
  after(() => order.push("outer after"));
  beforeEach((t) => order.push("outer beforeEach " + t.name));
  afterEach((t) => order.push("outer afterEach " + t.name));
  describe("inner", () => {
    beforeEach((t) => order.push("inner beforeEach " + t.name));
    afterEach((t) => order.push("inner afterEach " + t.name));
    it("leaf", () => order.push("leaf"));
  });
  it("second", () => order.push("second"));
});
test("with sub", async (t) => {
  t.before(() => order.push("t.before"));
  t.beforeEach((t) => order.push("t.beforeEach " + t.name));
  t.afterEach((t) => order.push("t.afterEach " + t.name));
  t.after(() => order.push("t.after"));
  await t.test("sub", (s) => order.push("sub " + s.fullName));
  await t.test("sub2", () => order.push("sub2"));
});
order.push("declared");
