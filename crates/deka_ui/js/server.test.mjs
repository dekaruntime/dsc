import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const sourceUrl = new URL("./server.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const start = source.indexOf("function inlineScriptJson");
const end = source.indexOf("\n\nfunction utf8Bytes", start);

assert.notEqual(start, -1, "server runtime must define inlineScriptJson");
assert.notEqual(end, -1, "server runtime must terminate inlineScriptJson before utf8Bytes");

const context = {};
vm.runInNewContext(
  `${source.slice(start, end)}\nglobalThis.inlineScriptJson = inlineScriptJson;`,
  context,
);

for (const value of [
  "line\nbreak",
  'slash\\quote"',
  "</script><script>globalThis.pwned = true</script>",
  "line\u2028separator",
  "paragraph\u2029separator",
]) {
  const encoded = context.inlineScriptJson(value);
  assert.equal(vm.runInNewContext(`(${encoded})`), value);
  assert.ok(!encoded.includes("<"), "inline script JSON must not contain HTML tag starts");
  assert.ok(!encoded.includes("\u2028"), "inline script JSON must escape U+2028");
  assert.ok(!encoded.includes("\u2029"), "inline script JSON must escape U+2029");
}
