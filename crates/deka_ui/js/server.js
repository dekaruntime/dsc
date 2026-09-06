// ui/server — render ComponentNodes to HTML. Never imported from ui/jsx.
// Function tags are invoked here. Text and attributes are escaped.

import { Fragment, isComponentNode } from "./jsx.js";
import { isLive } from "./reactive.js";
import { Suspense } from "./suspense.js";

function escapeHtml(text) {
  return String(text)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function utf8Bytes(str) {
  if (typeof TextEncoder === "function") return Array.from(new TextEncoder().encode(String(str)));
  const s = String(str);
  const out = [];
  for (let i = 0; i < s.length; i++) {
    let c = s.charCodeAt(i);
    if (c < 0x80) out.push(c);
    else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
      i += 1;
      c = 0x10000 + ((c & 0x3ff) << 10) + (s.charCodeAt(i) & 0x3ff);
      out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 0x3f), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
    } else {
      out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
    }
  }
  return out;
}

function base64Encode(str) {
  const bytes = utf8Bytes(str);
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let output = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const a = bytes[i];
    const b = i + 1 < bytes.length ? bytes[i + 1] : 0;
    const c = i + 2 < bytes.length ? bytes[i + 2] : 0;
    const triple = (a << 16) | (b << 8) | c;
    output += alphabet[(triple >> 18) & 63];
    output += alphabet[(triple >> 12) & 63];
    output += i + 1 < bytes.length ? alphabet[(triple >> 6) & 63] : "=";
    output += i + 2 < bytes.length ? alphabet[triple & 63] : "=";
  }
  return output;
}

function liveText(value) {
  if (value == null || typeof value === "boolean" || value === "") return "\u200b";
  if (typeof value === "string" || typeof value === "number") return String(value);
  return "\u200b";
}

const DEFER_TTL_SECS = 3600;
const DEFER_NONCE_LEN = 12;

let deferRequest = null;
let deferSessionCookie = "deka_sid";

function getDeferSecret() {
  try {
    if (typeof globalThis !== "undefined" && globalThis.__DEKA_DEFER_SECRET) {
      return String(globalThis.__DEKA_DEFER_SECRET);
    }
  } catch (_) {}
  return "";
}

export function bindDefer(request, secret, cookieName) {
  deferRequest = request || null;
  if (cookieName !== undefined && cookieName !== null) {
    deferSessionCookie = String(cookieName);
  }
  if (secret) {
    try {
      globalThis.__DEKA_DEFER_SECRET = String(secret);
    } catch (_) {}
  }
}

function cookieHeader(request) {
  const req = request || deferRequest;
  const headers = req && req.headers;
  if (!headers) return "";
  return String(headers.cookie || headers.Cookie || "");
}

function cookieValue(header, name) {
  const want = String(name || "");
  if (!want) return "";
  const parts = String(header || "").split(";");
  for (let i = 0; i < parts.length; i++) {
    const part = parts[i].trim();
    const eq = part.indexOf("=");
    if (eq <= 0) continue;
    if (part.slice(0, eq) !== want) continue;
    const raw = part.slice(eq + 1).trim();
    try {
      return decodeURIComponent(raw);
    } catch (_) {
      return raw;
    }
  }
  return "";
}

function sessionFromRequest(request) {
  return cookieValue(cookieHeader(request), deferSessionCookie);
}

function deferAad(name, request) {
  return "props:" + String(name || "") + "\n" + sessionFromRequest(request);
}

function hostFn() {
  try {
    const h = globalThis[Symbol.for("deka.host.internal")];
    if (h && typeof h.host === "function") return h.host;
  } catch (_) {}
  if (typeof __deka_host === "function") return __deka_host;
  return null;
}

function keyBytesFromSecret(secret) {
  const s = String(secret || getDeferSecret() || "");
  if (!/^[0-9a-fA-F]{64}$/.test(s)) return null;
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function randomNonce() {
  const out = new Uint8Array(DEFER_NONCE_LEN);
  const host = hostFn();
  if (host) {
    const r = host("crypto", "random_bytes", [DEFER_NONCE_LEN]);
    if (r && r.ok && r.value) {
      const src = r.value instanceof Uint8Array ? r.value : new Uint8Array(r.value);
      out.set(src.subarray(0, DEFER_NONCE_LEN));
      return out;
    }
  }
  if (globalThis.crypto && typeof globalThis.crypto.getRandomValues === "function") {
    globalThis.crypto.getRandomValues(out);
    return out;
  }
  return null;
}

function u8Concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

function encodeEnc(nonce, ciphertext) {
  return base64EncodeBytes(u8Concat(nonce, ciphertext));
}

function decodeEnc(enc) {
  const raw = base64DecodeBytes(enc);
  if (raw.length < DEFER_NONCE_LEN + 16) return null;
  return { nonce: raw.subarray(0, DEFER_NONCE_LEN), ciphertext: raw.subarray(DEFER_NONCE_LEN) };
}

function base64EncodeBytes(bytes) {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes || []);
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let output = "";
  for (let i = 0; i < u8.length; i += 3) {
    const a = u8[i];
    const b = i + 1 < u8.length ? u8[i + 1] : 0;
    const c = i + 2 < u8.length ? u8[i + 2] : 0;
    const triple = (a << 16) | (b << 8) | c;
    output += alphabet[(triple >> 18) & 63];
    output += alphabet[(triple >> 12) & 63];
    output += i + 1 < u8.length ? alphabet[(triple >> 6) & 63] : "=";
    output += i + 2 < u8.length ? alphabet[triple & 63] : "=";
  }
  return output;
}

function base64DecodeBytes(raw) {
  const cleaned = String(raw || "").replace(/[^A-Za-z0-9+/=]/g, "");
  if (!cleaned) return new Uint8Array();
  if (typeof atob === "function") {
    try {
      const binary = atob(cleaned);
      const out = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i) & 255;
      return out;
    } catch (_) {}
  }
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const bytes = [];
  for (let i = 0; i < cleaned.length; i += 4) {
    const a = alphabet.indexOf(cleaned[i]);
    const b = alphabet.indexOf(cleaned[i + 1]);
    const c = alphabet.indexOf(cleaned[i + 2]);
    const d = alphabet.indexOf(cleaned[i + 3]);
    const triple = ((a & 63) << 18) | ((b & 63) << 12) | ((c & 63) << 6) | (d & 63);
    bytes.push((triple >> 16) & 255);
    if (cleaned[i + 2] !== "=") bytes.push((triple >> 8) & 255);
    if (cleaned[i + 3] !== "=") bytes.push(triple & 255);
  }
  return new Uint8Array(bytes);
}

function utf8U8(str) {
  const raw = utf8Bytes(str);
  return raw instanceof Uint8Array ? raw : new Uint8Array(raw);
}

function utf8Decode(bytes) {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes || []);
  if (typeof TextDecoder === "function") {
    try {
      return new TextDecoder().decode(u8);
    } catch (_) {}
  }
  let out = "";
  for (let i = 0; i < u8.length; i++) out += String.fromCharCode(u8[i]);
  return out;
}

function nowSecs() {
  return Math.floor((Date.now ? Date.now() : 0) / 1000);
}

function encryptPropsSync(name, props, request) {
  const key = keyBytesFromSecret();
  const nonce = randomNonce();
  if (!key || !nonce) return "";
  const exp = nowSecs() + DEFER_TTL_SECS;
  const plaintext = utf8U8(JSON.stringify({ props: props || {}, exp }));
  const aad = utf8U8(deferAad(name, request));
  const host = hostFn();
  if (host) {
    const r = host("crypto", "aes_256_gcm_encrypt", [key, nonce, plaintext, aad]);
    if (!r || r.ok !== true || r.value == null) return "";
    const ct = r.value instanceof Uint8Array ? r.value : new Uint8Array(r.value);
    return encodeEnc(nonce, ct);
  }
  return "";
}

async function decryptEncAsync(name, enc, request) {
  const packed = decodeEnc(enc);
  const key = keyBytesFromSecret();
  if (!packed || !key) return null;
  const aad = utf8U8(deferAad(name, request));
  const host = hostFn();
  if (host) {
    const r = host("crypto", "aes_256_gcm_decrypt", [key, packed.nonce, packed.ciphertext, aad]);
    if (!r || r.ok !== true || r.value == null) return null;
    const pt = r.value instanceof Uint8Array ? r.value : new Uint8Array(r.value);
    return utf8Decode(pt);
  }
  if (globalThis.crypto && crypto.subtle && typeof crypto.subtle.importKey === "function") {
    try {
      const cryptoKey = await crypto.subtle.importKey("raw", key, { name: "AES-GCM" }, false, ["decrypt"]);
      const pt = await crypto.subtle.decrypt(
        { name: "AES-GCM", iv: packed.nonce, additionalData: aad },
        cryptoKey,
        packed.ciphertext
      );
      return utf8Decode(new Uint8Array(pt));
    } catch (_) {
      return null;
    }
  }
  return null;
}

function deferJson(status, body, extraHeaders) {
  const headers = {
    "content-type": "application/json",
    "cache-control": "private, no-store",
    "x-robots-tag": "noindex",
  };
  if (extraHeaders) {
    for (const k of Object.keys(extraHeaders)) headers[k] = extraHeaders[k];
  }
  return { status, body, headers };
}

export async function runDeferBatch(body, secret, registry, cacheControl, request, cookieName) {
  if (secret) bindDefer(request, secret, cookieName);
  else {
    if (request) deferRequest = request;
    if (cookieName !== undefined && cookieName !== null) deferSessionCookie = String(cookieName);
  }
  let payload = {};
  try {
    payload = JSON.parse(body || "{}");
  } catch (_) {
    return deferJson(400, "{\"error\":\"invalid json\"}");
  }
  const islands = Array.isArray(payload.islands) ? payload.islands.slice(0, 32) : [];
  for (const item of islands) {
    if (!item) continue;
    if (item.props != null || item.mac != null) {
      return deferJson(400, "{\"error\":\"plaintext props are not allowed\"}");
    }
  }
  const fragments = {};
  const seen = {};
  const handlers = registry || {};
  const req = request || deferRequest;
  for (const item of islands) {
    const key = String((item && (item.id || item.name)) || "");
    if (!key || seen[key]) continue;
    seen[key] = true;
    const name = item && item.name;
    const enc = item && item.enc;
    if (!name || !enc) continue;
    const raw = await decryptEncAsync(name, enc, req);
    if (raw == null) continue;
    let decoded;
    try {
      decoded = JSON.parse(raw);
    } catch (_) {
      continue;
    }
    const exp = decoded && typeof decoded.exp === "number" ? decoded.exp : 0;
    if (exp && exp < nowSecs()) continue;
    const props = decoded && decoded.props && typeof decoded.props === "object" ? decoded.props : {};
    const fn = handlers[name];
    if (typeof fn !== "function") continue;
    const tree = fn(props, req);
    const rendered = renderToString(tree, req);
    fragments[key] = rendered && rendered.html ? rendered.html : "";
  }
  const cache = cacheControl || "private, no-store";
  return deferJson(200, JSON.stringify({ fragments }), {
    "cache-control": cache,
    vary: "cookie",
  });
}

const VOID = new Set([
  "area", "base", "br", "col", "embed", "hr", "img", "input",
  "link", "meta", "param", "source", "track", "wbr",
]);

function jsonSafe(value) {
  if (value == null) return value;
  const t = typeof value;
  if (t === "string" || t === "number" || t === "boolean") return value;
  if (Array.isArray(value)) {
    return value.map(jsonSafe).filter((item) => item !== undefined);
  }
  if (t !== "object") return undefined;
  if (value.__componentNode || value.__live) return undefined;
  const out = {};
  for (const [key, child] of Object.entries(value)) {
    const next = jsonSafe(child);
    if (next !== undefined) out[key] = next;
  }
  return out;
}

function serializeIslandProps(props) {
  try {
    return JSON.stringify(jsonSafe(props) ?? {});
  } catch (_) {
    return "{}";
  }
}

function extractDirectives(props) {
  const rest = {};
  const directives = [];
  let cache = null;
  for (const [key, value] of Object.entries(props ?? {})) {
    if (key.startsWith("client:") && value !== false && value != null) {
      directives.push(key.slice(7));
    } else if (key === "server:defer" && value !== false && value != null) {
      directives.push("defer");
    } else if (key.startsWith("server:")) {
      // unknown server: axis
    } else if (key === "cache") {
      cache = String(value);
    } else if (typeof value === "function" && key.startsWith("on")) {
      // event handlers are client-only
    } else {
      rest[key] = value;
    }
  }
  return { rest, directives, cache };
}

function fallbackNodes(children) {
  const list = Array.isArray(children) ? children : children == null ? [] : [children];
  const out = [];
  for (const child of list) {
    if (!isComponentNode(child)) continue;
    if (child.props && child.props.slot === "fallback") out.push(child);
  }
  return out;
}

function wrapDeferred(name, directive, props, cache, id, html) {
  const cachePart = cache ? ` cache:${base64Encode(String(cache))}` : "";
  const enc = encryptPropsSync(name, jsonSafe(props) ?? {}, deferRequest);
  const encPart = enc ? ` enc:${enc}` : "";
  return `<!--deka-island start:${base64Encode(name)} directive:${base64Encode(directive)} id:${base64Encode(id)}${encPart}${cachePart}--><span data-deka-defer="${escapeHtml(id)}">${html}</span><!--deka-island end:${base64Encode(name)}-->`;
}

function renderAttributes(props) {
  let attrs = "";
  for (const [key, value] of Object.entries(props ?? {})) {
    if (typeof value === "function") continue;
    if (key.length > 2 && key.startsWith("on")) continue;
    if (value === true) {
      attrs += ` ${escapeHtml(key)}`;
    } else if (value === false || value == null) {
      continue;
    } else {
      attrs += ` ${escapeHtml(key)}="${escapeHtml(value)}"`;
    }
  }
  return attrs;
}

function forwardClass(html, className) {
  if (!className || typeof html !== "string" || html[0] !== "<") return html;
  const escaped = escapeHtml(className);
  return html.replace(/^<([^\s>\/]+)((?:\s[^>]*)?)(\/?>)/, (m, tag, attrs, close) => {
    if (/\sclass\s*=/.test(attrs)) return m;
    return `<${tag}${attrs} class="${escaped}"${close}`;
  });
}

function isPromise(value) {
  return value != null && (typeof value === "object" || typeof value === "function") && typeof value.then === "function";
}

function isSuspenseTag(tag) {
  return tag === Suspense || (typeof tag === "function" && tag.__dekaSuspense === true);
}

function wrapFallback(id, fallbackHtml) {
  return `<div id="${escapeHtml(id)}" data-deka-suspense="pending">${fallbackHtml}</div>`;
}

let deferSeq = 0;

function nextDeferId() {
  deferSeq += 1;
  return "D:" + deferSeq;
}

function createCtx() {
  return { boundaryId: 0, stack: [], pending: [], boundaryChildren: {} };
}

function handlePromiseSync(ctx, promise) {
  if (ctx.stack.length === 0) return "";
  const id = ctx.stack[ctx.stack.length - 1];
  const existing = ctx.pending.find((item) => item.id === id);
  if (existing) {
    existing.promise = Promise.all([existing.promise, promise]);
    return "";
  }
  ctx.pending.push({
    id,
    promise,
    children: ctx.boundaryChildren ? ctx.boundaryChildren[id] : null,
  });
  return "";
}

function renderNode(node, ctx) {
  if (node == null || typeof node === "boolean") return "";
  if (isLive(node)) {
    let value;
    try {
      value = node.read();
    } catch (_) {
      return "";
    }
    if (isComponentNode(value) || Array.isArray(value)) return renderNode(value, ctx);
    return escapeHtml(liveText(value));
  }
  if (typeof node === "string" || typeof node === "number") {
    return escapeHtml(String(node));
  }
  if (Array.isArray(node)) {
    let out = "";
    for (const child of node) out += renderNode(child, ctx);
    return out;
  }
  if (!isComponentNode(node)) {
    return escapeHtml(String(node));
  }

  const { tag, props, children } = node;

  if (tag === Fragment) {
    return renderNode(children, ctx);
  }

  if (typeof tag === "function") {
    if (isSuspenseTag(tag)) {
      return renderSuspenseSync(node, ctx);
    }
    const { rest, directives, cache } = extractDirectives(props);
    if (directives.includes("defer")) {
      const html = renderNode(fallbackNodes(children), ctx);
      return wrapDeferred(tag.name || "Anonymous", "defer", rest, cache, nextDeferId(), html);
    }
    const result = tag({ ...rest, children });
    if (isPromise(result)) return handlePromiseSync(ctx, result);
    const html = forwardClass(renderNode(result, ctx), rest.class);
    if (directives.length === 0) return html;
    const islandName = tag.name || "Anonymous";
    const directive = directives[0];
    return `<!--deka-island start:${base64Encode(islandName)} directive:${base64Encode(directive)} props:${base64Encode(serializeIslandProps(rest))}-->${html}<!--deka-island end:${base64Encode(islandName)}-->`;
  }

  if (typeof tag === "string") {
    const { rest, directives } = extractDirectives(props);
    const attrs = renderAttributes(rest);
    let markerAttrs = "";
    for (const directive of directives) {
      markerAttrs += ` data-client-${escapeHtml(directive)}`;
    }
    const childHtml = renderNode(children, ctx);
    if (childHtml === "" && VOID.has(tag)) {
      return `<${tag}${attrs}${markerAttrs} />`;
    }
    return `<${tag}${attrs}${markerAttrs}>${childHtml}</${tag}>`;
  }

  return "";
}

function renderSuspenseSync(node, ctx) {
  const id = "S:" + (++ctx.boundaryId);
  ctx.stack.push(id);
  ctx.boundaryChildren[id] = node.children;
  const inner = renderNode(node.children, ctx);
  ctx.stack.pop();
  if (ctx.pending.some((item) => item.id === id)) {
    const fallbackHtml = renderNode(node.props ? node.props.fallback : null, ctx);
    return wrapFallback(id, fallbackHtml);
  }
  return inner;
}

async function renderNodeAsync(node) {
  if (node == null || typeof node === "boolean") return "";
  if (isLive(node)) {
    let value;
    try {
      value = node.read();
    } catch (_) {
      return "";
    }
    if (isComponentNode(value) || Array.isArray(value)) return await renderNodeAsync(value);
    return escapeHtml(liveText(value));
  }
  if (typeof node === "string" || typeof node === "number") {
    return escapeHtml(String(node));
  }
  if (Array.isArray(node)) {
    let out = "";
    for (const child of node) out += await renderNodeAsync(child);
    return out;
  }
  if (!isComponentNode(node)) {
    return escapeHtml(String(node));
  }

  const { tag, props, children } = node;

  if (tag === Fragment) {
    return await renderNodeAsync(children);
  }

  if (typeof tag === "function") {
    if (isSuspenseTag(tag)) {
      return await renderNodeAsync(children);
    }
    const { rest, directives, cache } = extractDirectives(props);
    if (directives.includes("defer")) {
      const html = await renderNodeAsync(fallbackNodes(children));
      return wrapDeferred(tag.name || "Anonymous", "defer", rest, cache, nextDeferId(), html);
    }
    let result = tag({ ...rest, children });
    if (isPromise(result)) result = await result;
    const html = forwardClass(await renderNodeAsync(result), rest.class);
    if (directives.length === 0) return html;
    const islandName = tag.name || "Anonymous";
    const directive = directives[0];
    return `<!--deka-island start:${base64Encode(islandName)} directive:${base64Encode(directive)} props:${base64Encode(serializeIslandProps(rest))}-->${html}<!--deka-island end:${base64Encode(islandName)}-->`;
  }

  if (typeof tag === "string") {
    const { rest, directives } = extractDirectives(props);
    const attrs = renderAttributes(rest);
    let markerAttrs = "";
    for (const directive of directives) {
      markerAttrs += ` data-client-${escapeHtml(directive)}`;
    }
    const childHtml = await renderNodeAsync(children);
    if (childHtml === "" && VOID.has(tag)) {
      return `<${tag}${attrs}${markerAttrs} />`;
    }
    return `<${tag}${attrs}${markerAttrs}>${childHtml}</${tag}>`;
  }

  return "";
}

export function renderToString(node, request) {
  const prev = deferRequest;
  if (request !== undefined) deferRequest = request || null;
  deferSeq = 0;
  try {
    const ctx = createCtx();
    return { html: renderNode(node, ctx), boundaries: ctx.pending.map((item) => item.id) };
  } finally {
    deferRequest = prev;
  }
}

export async function renderToStringAsync(node, request) {
  const prev = deferRequest;
  if (request !== undefined) deferRequest = request || null;
  deferSeq = 0;
  try {
    return { html: await renderNodeAsync(node), boundaries: [] };
  } finally {
    deferRequest = prev;
  }
}

function swapChunk(id, html) {
  const templateId = "deka-swap-" + id;
  const tid = JSON.stringify(templateId);
  const sid = JSON.stringify(id);
  return `<template id="${escapeHtml(templateId)}">${html}</template><script>(() => { const t = document.getElementById(${tid}); const slot = document.getElementById(${sid}); if (slot && t) slot.replaceWith(t.content.cloneNode(true)); t && t.remove(); document.currentScript && document.currentScript.remove(); })();</script>`;
}

function encodeChunk(text) {
  if (typeof TextEncoder === "function") return new TextEncoder().encode(text);
  return text;
}

async function nextResolved(queue) {
  return await new Promise((resolve, reject) => {
    let settled = false;
    for (const item of queue) {
      Promise.resolve(item.promise).then(
        (value) => {
          if (settled) return;
          settled = true;
          resolve({ item, value });
        },
        (error) => {
          if (settled) return;
          settled = true;
          reject(error);
        }
      );
    }
  });
}

async function* iterateChunks(node) {
  deferSeq = 0;
  const ctx = createCtx();
  yield renderNode(node, ctx);
  const queue = ctx.pending.slice();
  ctx.pending.length = 0;
  while (queue.length > 0) {
    const selected = await nextResolved(queue);
    const index = queue.indexOf(selected.item);
    if (index >= 0) queue.splice(index, 1);
    // Stream the resolved tree with the same sync renderer so nested
    // Suspense can enqueue more boundaries. renderNodeAsync unwraps
    // Suspense and waits, which collapsed nested fallbacks (Hats
    // jsx_suspense_stream_swap).
    const html = renderNode(selected.value, ctx);
    for (const extra of ctx.pending) queue.push(extra);
    ctx.pending.length = 0;
    if (selected.item.id) yield swapChunk(selected.item.id, html);
  }
}

function createByteStream(start) {
  if (typeof ReadableStream === "function") {
    return new ReadableStream({ start });
  }
  const chunks = [];
  let done = false;
  let failure = null;
  let wake = null;
  const controller = {
    enqueue(chunk) {
      chunks.push(chunk);
      if (wake) {
        const w = wake;
        wake = null;
        w();
      }
    },
    close() {
      done = true;
      if (wake) {
        const w = wake;
        wake = null;
        w();
      }
    },
    error(err) {
      failure = err;
      done = true;
      if (wake) {
        const w = wake;
        wake = null;
        w();
      }
    },
  };
  const started = Promise.resolve(start(controller));
  return {
    getReader() {
      let i = 0;
      return {
        async read() {
          await started;
          while (i >= chunks.length && !done) {
            await new Promise((resolve) => {
              wake = resolve;
            });
          }
          if (failure) throw failure;
          if (i >= chunks.length) return { done: true, value: undefined };
          return { done: false, value: chunks[i++] };
        },
      };
    },
  };
}

export function renderToStream(node, request) {
  const prev = deferRequest;
  if (request !== undefined) deferRequest = request || null;
  return createByteStream(async (controller) => {
    try {
      for await (const text of iterateChunks(node)) {
        controller.enqueue(encodeChunk(text));
      }
      controller.close();
    } finally {
      deferRequest = prev;
    }
  });
}

export async function collectStream(stream) {
  const reader = stream.getReader();
  const decoder = typeof TextDecoder === "function" ? new TextDecoder() : null;
  let out = "";
  for (;;) {
    const step = await reader.read();
    if (step.done) break;
    const value = step.value;
    if (typeof value === "string") out += value;
    else if (decoder) out += decoder.decode(value, { stream: true });
  }
  if (decoder) out += decoder.decode();
  return out;
}

export async function renderToStreamHtml(node, request) {
  const prev = deferRequest;
  if (request !== undefined) deferRequest = request || null;
  try {
    let out = "";
    for await (const text of iterateChunks(node)) out += text;
    return out;
  } finally {
    deferRequest = prev;
  }
}

export { escapeHtml, liveText, Suspense };
