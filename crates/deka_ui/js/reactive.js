// ui/reactive — signal / effect / live. No DOM. Safe in isolate and browser.

const contextStack = [];

export function signal(initial) {
  const subscribers = new Set();
  let value = initial;
  function get() {
    const ctx = contextStack[contextStack.length - 1];
    if (ctx) {
      subscribers.add(ctx.execute);
      if (ctx.onCleanup) {
        ctx.onCleanup(() => subscribers.delete(ctx.execute));
      }
    }
    return value;
  }
  function set(next) {
    if (Object.is(value, next)) return;
    value = next;
    for (const run of Array.from(subscribers)) run();
  }
  return [get, set];
}

export function effect(fn) {
  const dependencyCleanups = new Set();
  let userCleanup;
  function execute() {
    for (const c of dependencyCleanups) c();
    dependencyCleanups.clear();
    contextStack.push({
      execute,
      onCleanup(c) {
        dependencyCleanups.add(c);
      },
    });
    try {
      const maybe = fn();
      if (typeof maybe === "function") userCleanup = maybe;
    } finally {
      contextStack.pop();
    }
  }
  execute();
  return function dispose() {
    for (const c of dependencyCleanups) c();
    dependencyCleanups.clear();
    if (typeof userCleanup === "function") {
      const c = userCleanup;
      userCleanup = undefined;
      c();
    }
  };
}

export function live(fn) {
  return Object.freeze({ __live: true, read: fn });
}

export function isLive(node) {
  return node != null && typeof node === "object" && node.__live === true && typeof node.read === "function";
}

export const createSignal = signal;
export const createEffect = effect;
