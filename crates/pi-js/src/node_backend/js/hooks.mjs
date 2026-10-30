// module.register hooks for Node versions without module.registerHooks. They run on Node's
// hooks thread and ask Rust through the main thread.
import { isBuiltin } from "node:module";
import { fileURLToPath } from "node:url";

let port;
let ownDir;
let sequence = 0;
const waiting = new Map();

export async function initialize(data) {
  port = data.port;
  ownDir = data.ownDir;
  port.on("message", (reply) => {
    const settle = waiting.get(reply.id);
    waiting.delete(reply.id);
    settle?.(reply);
  });
}

function ask(fields) {
  const id = `h${++sequence}`;
  return new Promise((resolve) => {
    waiting.set(id, resolve);
    port.postMessage({ ...fields, id });
  });
}

function toError(error) {
  const result = new Error(error.message);
  if (error.code) result.code = error.code;
  return result;
}

function parentPath(parentURL) {
  if (!parentURL || parentURL.startsWith(ownDir) || !parentURL.startsWith("file:")) return null;
  return fileURLToPath(parentURL);
}

export async function resolve(specifier, context, nextResolve) {
  if (specifier.startsWith("node:") || isBuiltin(specifier)) return nextResolve(specifier, context);
  const plain = specifier.startsWith("file:") ? fileURLToPath(specifier) : specifier;
  const reply = await ask({ t: "resolve", specifier: plain, parent: parentPath(context.parentURL), kind: "import" });
  if (reply.ok) return { url: reply.ok.url, shortCircuit: true };
  try {
    return await nextResolve(specifier, context);
  } catch {
    throw toError(reply.err);
  }
}

export async function load(url, context, nextLoad) {
  if (!url.startsWith("file:") || url.endsWith(".node")) return nextLoad(url, context);
  const reply = await ask({ t: "load", path: fileURLToPath(url) });
  if (reply.err) throw toError(reply.err);
  if (!reply.ok) return nextLoad(url, context);
  return { format: reply.ok.format, source: reply.ok.source, shortCircuit: true };
}
