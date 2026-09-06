// ui/router — App() dispatch for generated api/middleware/worker entries.
// Hosts materialize this file; generated .ds imports it rather than inlining
// path/method/Option lowering as source text.

const OPAQUE_500 = {
  status: 500,
  body: "Internal Server Error",
  headers: { location: "" },
};

const NEXT = { status: 0, body: "", headers: { location: "" } };

function normalizePath(raw) {
  const withoutQuery = String(raw || "").split("?")[0].trim();
  if (!withoutQuery) return "/";
  let path = "/" + withoutQuery.replace(/^\/+/, "");
  if (path.length > 1) {
    while (path.endsWith("/")) path = path.slice(0, -1);
  }
  return path || "/";
}

function skipMiddlewarePath(path) {
  const p = normalizePath(path);
  return p === "/assets" || p.startsWith("/assets/");
}

function patternHits(pattern, path) {
  const p = normalizePath(path);
  if (typeof pattern !== "string") return false;
  if (pattern.endsWith("/:path*")) {
    const prefix = pattern.slice(0, -"/:path*".length);
    return p === prefix || p.startsWith(prefix + "/");
  }
  if (pattern.endsWith(":path*")) {
    const prefix = pattern.slice(0, -":path*".length).replace(/\/+$/, "");
    return p === prefix || p.startsWith(prefix + "/");
  }
  return p === normalizePath(pattern);
}

function matcherHits(matcher, path) {
  if (matcher == null) return true;
  if (!Array.isArray(matcher) || matcher.length === 0) return false;
  return matcher.some((pattern) => patternHits(pattern, path));
}

function decodeMiddlewareOption(opt) {
  if (opt == null) return NEXT;
  if (opt.__case === "None") return NEXT;
  if (opt.__case === "Some") return opt.value;
  if (typeof opt.status === "number") return opt;
  return NEXT;
}

export function runMiddleware(request, middleware, matcher) {
  const path = request && request.pathname === "" ? "/" : (request && request.pathname) || "/";
  if (skipMiddlewarePath(path)) return NEXT;
  if (!matcherHits(matcher, path)) return NEXT;
  if (typeof middleware !== "function") return NEXT;
  try {
    return decodeMiddlewareOption(middleware(request));
  } catch (_) {
    return OPAQUE_500;
  }
}

function staticPrefix(route) {
  const parts = [];
  for (const part of String(route || "").replace(/^\/+|\/+$/g, "").split("/")) {
    if (!part) continue;
    if (part.startsWith("[")) break;
    parts.push(part);
  }
  if (parts.length === 0) return "/";
  return "/" + parts.join("/") + "/";
}

function oneSegmentAfter(path, prefix) {
  const p = String(path || "");
  const pre = String(prefix || "");
  if (!p.startsWith(pre)) return false;
  const rest = p.slice(pre.length);
  if (!rest || rest.indexOf("/") >= 0) return false;
  return true;
}

function routeMatches(route, path) {
  if (route === path) return true;
  if (String(route).indexOf("[") < 0) return false;
  return oneSegmentAfter(path, staticPrefix(route));
}

function callHandler(fn, request) {
  try {
    return fn(request);
  } catch (_) {
    return OPAQUE_500;
  }
}

export function runApiRouter(request, routes) {
  const path = request && request.pathname === "" ? "/" : (request && request.pathname) || "/";
  const method = String((request && request.method) || "GET");
  const table = routes || {};
  let handlers = null;
  for (const route of Object.keys(table)) {
    if (routeMatches(route, path)) {
      handlers = table[route];
      break;
    }
  }
  if (!handlers) {
    if (path === "/api" || path.startsWith("/api/")) {
      return { status: 404, body: "Not found", headers: { location: "" } };
    }
    return { status: 404, body: "Not found", headers: { location: "" } };
  }
  let fn = handlers[method];
  if (typeof fn !== "function" && method === "HEAD" && typeof handlers.GET === "function") {
    fn = handlers.GET;
  }
  if (typeof fn !== "function") {
    return { status: 405, body: "Method not allowed", headers: { location: "" } };
  }
  return callHandler(fn, request);
}

export function runWorker(request, middleware, matcher, routes) {
  if (typeof middleware === "function") {
    const gated = runMiddleware(request, middleware, matcher);
    if (gated && gated.status !== 0) return gated;
  }
  const path = request && request.pathname === "" ? "/" : (request && request.pathname) || "/";
  if (path === "/api" || path.startsWith("/api/")) {
    return runApiRouter(request, routes);
  }
  return NEXT;
}
