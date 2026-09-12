export function handle() { return { secret: 42 }; }
export const read = (handle) => handle.secret;
export function absent() { return null; }
export function missing() { return undefined; }
export function number() { return 7; }
export function noop() {}
export function optional(value) { return value === undefined ? 0 : value; }
export function fail() { throw 'foreign failure'; }
export function defaulted(value = 8) { return value; }
export function variadic(...values) { return values.length; }
export const scalar = 42;
function aliased(value) { return value; }
export { aliased as renamed };
export async function later() { return 9; }
