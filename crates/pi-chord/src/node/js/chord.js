// @earendil-works/chord for facets in the embedded runtime: facet setup, services, and
// replicated state forward to the Rust host through `pi:host`.
import host from "pi:host";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";

export {
  awaitWithContext,
  BACKGROUND_CONTEXT,
  createContextKey,
  TODO_CONTEXT,
  withAbortSignal,
  withCancel,
  withContextValue,
  withoutAbortSignal,
} from "@earendil-works/chord/context";

const STATE = Symbol("chord.state");
const objects = new Map();
const contexts = new Map();
let nextObject = 1;

const call = (op, ...args) => host.call("chord", op, ...args);
const register = (value) => {
  const id = nextObject++;
  objects.set(id, value);
  return id;
};
const registerOnce = (callback) => {
  const id = register((...args) => {
    objects.delete(id);
    return callback(...args);
  });
  return id;
};
const isRecord = (value) => typeof value === "object" && value !== null && !Array.isArray(value);
const isContext = (value) =>
  typeof value === "object" && value !== null && typeof value.value === "function" && typeof value.toString === "function";
const token = (service) => {
  if (!isRecord(service) || typeof service.id !== "string") throw new TypeError("Invalid service token");
  return { id: service.id, local: service.local === true };
};

function contextFor(id) {
  if (id === 0) return BACKGROUND_CONTEXT;
  let entry = contexts.get(id);
  if (entry === undefined) {
    const controller = new AbortController();
    entry = {
      controller,
      context: {
        get abortSignal() {
          return controller.signal;
        },
        value: () => undefined,
        toString: () => "[Context BACKGROUND_CONTEXT].WithCancel",
      },
    };
    contexts.set(id, entry);
  }
  return entry.context;
}

function subscribe(open, listener) {
  const callback = register(listener);
  const subscription = open(callback);
  return () => {
    objects.delete(callback);
    call("unsubscribe", subscription);
  };
}

function stateObject(id) {
  return Object.freeze({
    [STATE]: id,
    get value() {
      return call("state.value", id);
    },
    change(_context, recipe) {
      const draft = structuredClone(call("state.value", id));
      recipe(draft);
      call("state.replace", id, draft);
    },
    replace(_context, value) {
      call("state.replace", id, value);
    },
    subscribe(listener) {
      return subscribe((callback) => call("state.subscribe", id, callback), listener);
    },
  });
}

function describe(service, implementation) {
  if (!isRecord(implementation)) throw new TypeError(`Service ${service.id} implementation must be an object`);
  if (service.local) return null;
  return Object.keys(implementation).map((name) => {
    const value = implementation[name];
    if (typeof value === "function") return { name, kind: "method" };
    if (isRecord(value) && typeof value[STATE] === "number") return { name, kind: "state", state: value[STATE] };
    throw new TypeError(`Remote service ${service.id} member ${name} is not remotely exposable`);
  });
}

function memberView(handle, member) {
  const invoke = (...args) => {
    if (args.length > 0 && isContext(args[args.length - 1])) args.pop();
    return host.call("chord.async", "call", handle, member, args);
  };
  return new Proxy(invoke, {
    get(_target, property) {
      if (property === "value") return call("member.value", handle, member);
      if (property === "subscribe") {
        return (listener) => subscribe((callback) => call("member.subscribe", handle, member, callback), listener);
      }
      return undefined;
    },
  });
}

function remoteView(handle) {
  const members = new Map();
  return new Proxy(Object.create(null), {
    get(_target, property) {
      if (typeof property !== "string" || property === "then") return undefined;
      let member = members.get(property);
      if (member === undefined) {
        member = memberView(handle, property);
        members.set(property, member);
      }
      return member;
    },
  });
}

function localView(handle) {
  const target = () => objects.get(call("local.get", handle));
  return new Proxy(Object.create(null), {
    get: (_target, property) => target()[property],
    has: (_target, property) => property in target(),
    ownKeys: () => Reflect.ownKeys(target()),
    getOwnPropertyDescriptor: (_target, property) => {
      const descriptor = Reflect.getOwnPropertyDescriptor(target(), property);
      return descriptor === undefined ? undefined : { ...descriptor, configurable: true };
    },
  });
}

const view = (result) => (result.local ? localView(result.handle) : remoteView(result.handle));

function environment(envId) {
  const op = (name, ...args) => call("env", envId, name, ...args);
  return Object.freeze({
    use: (service) => view(op("use", token(service))),
    observe: (service, handler) => op("observe", token(service), register(handler)),
    provide: (service, implementation) => {
      const description = describe(token(service), implementation);
      op("provide", token(service), register(implementation), description);
    },
    provideMany: (service) => {
      const spawner = op("provideMany", token(service));
      return Object.freeze({
        spawn(key, implementation) {
          const description = describe(token(service), implementation);
          const closer = call("spawn", spawner, key, register(implementation), description);
          return () => call("close", closer);
        },
      });
    },
    replicatedState: (initial) => stateObject(op("replicatedState", initial)),
    own: (disposal) => op("own", registerOnce(disposal)),
    onActivate: (callback) => op("onActivate", registerOnce(callback)),
    onDeactivate: (callback) => op("own", registerOnce(callback)),
  });
}

export function defineFacet(facet) {
  return facet;
}

export function defineService(id, options) {
  if (typeof id !== "string" || id.length === 0) throw new TypeError("Service ID must not be empty");
  if (id.startsWith("$chord.")) throw new TypeError("Service IDs beginning with $chord. are reserved");
  return Object.freeze({ id, local: options?.local ?? false });
}

export function replicatedState(initial) {
  return stateObject(call("state.create", initial));
}

/** The host side of the bridge; Rust calls these by object ID. */
export const __chordRuntime = Object.freeze({
  describeModule(imported) {
    if (!isRecord(imported)) return { module: false, facets: [] };
    const exported = imported.default;
    const candidates = Array.isArray(exported) ? exported : [exported];
    const facets = candidates.map((candidate) => {
      if (!isRecord(candidate) || typeof candidate.id !== "string" || candidate.id.length === 0) return { invalid: "id" };
      if (typeof candidate.setup !== "function") return { invalid: "setup", id: candidate.id };
      return { id: candidate.id, object: register(candidate) };
    });
    return { module: true, facets };
  },
  setup(facetId, envId) {
    const facet = objects.get(facetId);
    const result = facet.setup(environment(envId));
    if (result !== null && typeof result === "object" && typeof result.then === "function") {
      void Promise.resolve(result).catch(() => {});
      throw new Error(`Facet ${facet.id} setup must be synchronous`);
    }
  },
  invoke(objectId, member, args, contextId) {
    const target = objects.get(objectId);
    return target[member](...args, contextFor(contextId));
  },
  callFunction(functionId, args) {
    return objects.get(functionId)(...args);
  },
  observe(handlerId, result, contextId) {
    return objects.get(handlerId)(view(result), contextFor(contextId));
  },
  abort(contextId, message) {
    contextFor(contextId);
    contexts.get(contextId)?.controller.abort(new Error(message));
  },
  releaseContext(contextId) {
    contexts.delete(contextId);
  },
  release(objectId) {
    objects.delete(objectId);
  },
});
