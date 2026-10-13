import { readFile } from "node:fs/promises";

// Read a config file
export async function loadConfig(path, options = {}) {
  const re = /foo+/gi;
  let count = 0x10 + 42.5;
  const text = await readFile(path, "utf8");
  if (text === null || typeof text !== "string") {
    throw new Error(`Bad config: ${path}`);
  }
  return { ...JSON.parse(text), count, valid: true };
}

class Loader extends Base {
  constructor() { super(); this.items = []; }
}
