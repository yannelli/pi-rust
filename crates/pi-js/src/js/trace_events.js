// node:trace_events: tracing objects keep Node's state, but no trace file is written.
const { codeError, invalidArgType, validate } = require("internal/errors");

const enabled = new Set();

class Tracing {
  #categories;
  #enabled = false;
  constructor(categories) {
    this.#categories = categories;
  }
  enable() {
    this.#enabled = true;
    enabled.add(this);
  }
  disable() {
    this.#enabled = false;
    enabled.delete(this);
  }
  get enabled() {
    return this.#enabled;
  }
  get categories() {
    return this.#categories.join(",");
  }
  [Symbol.for("nodejs.util.inspect.custom")](depth, options, inspect) {
    if (depth < 0) return this;
    return `Tracing ${inspect({ enabled: this.enabled, categories: this.categories }, options)}`;
  }
}

function createTracing(options) {
  validate.object(options, "options");
  if (!Array.isArray(options.categories)) throw invalidArgType("options.categories", "an instance of Array", options.categories);
  if (options.categories.length === 0) throw codeError(TypeError, "ERR_TRACE_EVENTS_CATEGORY_REQUIRED", "At least one category is required");
  options.categories.forEach((category, index) => validate.string(category, `options.categories[${index}]`));
  return new Tracing([...options.categories]);
}

function getEnabledCategories() {
  const categories = new Set();
  for (const tracing of enabled) for (const category of tracing.categories.split(",")) categories.add(category);
  return categories.size === 0 ? undefined : [...categories].join(",");
}

module.exports = { createTracing, getEnabledCategories };
