export function boom() {
  throw new Error("nope");
}
export function ok() {
  return 1;
}
export function parses(s) {
  return JSON.parse(s);
}
export function callsUnknown() {
  return missing();
}
function helper() {
  throw new Error("from helper");
}
export function usesHelper() {
  return helper();
}
export function caught() {
  try {
    throw new Error("contained");
  } catch (e) {
    return 0;
  }
}
