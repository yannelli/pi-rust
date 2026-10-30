"use strict";
// Globals that pi-js's transpiled output calls. Node keeps async context itself, so the await
// wrappers pass values through.
const identity = (value) => value;
Object.defineProperty(globalThis, "__pi_aw", { value: identity });
Object.defineProperty(globalThis, "__pi_au", { value: identity });

// Helpers called by transpiled TypeScript decorators (oxc external helper mode).
const babelHelpers = {
  decorate(decorators, target, key, desc) {
    const count = arguments.length;
    let result = count < 3 ? target : desc === null ? (desc = Object.getOwnPropertyDescriptor(target, key)) : desc;
    for (let i = decorators.length - 1; i >= 0; i--) {
      const decorator = decorators[i];
      if (decorator) result = (count < 3 ? decorator(result) : count > 3 ? decorator(target, key, result) : decorator(target, key)) || result;
    }
    if (count > 3 && result) Object.defineProperty(target, key, result);
    return result;
  },
  decorateParam(index, decorator) {
    return (target, key) => decorator(target, key, index);
  },
  decorateMetadata(key, value) {
    if (typeof Reflect === "object" && typeof Reflect.metadata === "function") return Reflect.metadata(key, value);
    return undefined;
  },
};
Object.defineProperty(globalThis, "babelHelpers", { value: babelHelpers, writable: true, configurable: true });
