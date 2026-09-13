// Private node data for the existing ui/server, ui/client and ui/form APIs.
// JSX emission uses the configured React runtime and never constructs these nodes.

export const Fragment = Symbol.for("deka.ui.Fragment");

export function isComponentNode(value) {
  return value != null && typeof value === "object" && value.__componentNode === true;
}

export function normalizeJsxChildren(children) {
  if (children == null) return [];
  if (Array.isArray(children)) {
    const out = [];
    for (const child of children) {
      if (child == null || child === false || child === true) continue;
      if (Array.isArray(child)) {
        for (const inner of normalizeJsxChildren(child)) out.push(inner);
      } else {
        out.push(child);
      }
    }
    return out;
  }
  if (children === false || children === true) return [];
  return [children];
}

export function createComponentNode(tag, props) {
  const input = props ?? {};
  const { children, ...rest } = input;
  const node = {
    tag,
    props: Object.freeze(rest),
    children: Object.freeze(normalizeJsxChildren(children)),
  };
  Object.defineProperty(node, "__componentNode", {
    value: true,
    enumerable: false,
    writable: false,
    configurable: false,
  });
  return Object.freeze(node);
}
