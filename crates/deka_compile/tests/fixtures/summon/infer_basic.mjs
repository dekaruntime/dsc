export function add(a, b) {
  return a + b;
}
export function noop() {}
export const secret = (handle) => handle.secret;
export function scene() {
  return { id: 1 };
}
export function count(...values) {
  return values.length;
}
export function withDefault(n = 1) {
  return n;
}
export async function later() {
  return 9;
}
export const scalar = 42;
