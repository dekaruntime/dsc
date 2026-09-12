//! Acceptance tests for the closed `deka.*` catalog (RFD 21) and the strict
//! bytes shape (RFD 15, deka#756).
//!
//! Extracted from `deka_catalog.rs` so the catalog table + helper JS stay
//! under the file-size gate (deka#391). Rust-only assertions are
//! unconditional; the JS behavior proofs drive `CATALOG_HELPERS_JS` through
//! node when a node binary is available and skip (print + early return)
//! otherwise.

use super::*;

#[test]
fn catalog_is_closed_and_wellformed() {
    assert!(!DEKA_CATALOG.is_empty());
    for kind in DEKA_CATALOG {
        assert!(!kind.name.is_empty());
        assert!(
            !kind.methods.is_empty(),
            "kind '{}' has no methods",
            kind.name
        );
        for method in kind.methods {
            assert!(
                !method.name.is_empty(),
                "{}. method with empty name",
                kind.name
            );
            assert!(
                !method.doc.is_empty(),
                "{}.{} has no doc",
                kind.name,
                method.name
            );
            let mut optional_seen = false;
            for a in method.args {
                assert!(!a.name.is_empty());
                if a.optional {
                    optional_seen = true;
                } else {
                    assert!(
                        !optional_seen,
                        "{}.{}: required arg after optional",
                        kind.name, method.name
                    );
                }
            }
            // Safety and return shape agree (RFD 21 rule 3): a Result
            // shape means the helper throws -> Unsafe.
            match (method.safety, method.ret) {
                (Safety::Unsafe, ReturnShape::ResultValue(_)) => {}
                (Safety::Safe, ReturnShape::Value(_) | ReturnShape::OptionValue(_)) => {}
                _ => panic!(
                    "{}.{}: safety {:?} disagrees with return shape {:?}",
                    kind.name, method.name, method.safety, method.ret
                ),
            }
        }
        // No duplicate method names within a kind.
        for (i, a) in kind.methods.iter().enumerate() {
            for b in &kind.methods[..i] {
                assert_ne!(
                    a.name, b.name,
                    "kind '{}' duplicates '{}'",
                    kind.name, a.name
                );
            }
        }
    }
}

#[test]
fn every_entry_is_findable_and_unknown_names_are_not() {
    for kind in DEKA_CATALOG {
        assert_eq!(find_kind(kind.name).map(|k| k.name), Some(kind.name));
        assert!(is_catalog_kind(kind.name));
        for method in kind.methods {
            let found = find_method(kind.name, method.name).expect("find_method");
            assert_eq!(found.name, method.name);
        }
    }
    assert!(find_kind("bogus").is_none());
    assert!(find_method("bytes", "bogus").is_none());
    assert!(find_method("bogus", "len").is_none());
    assert!(!is_catalog_kind("bogus"));
}

#[test]
fn arity_counts_top_level_arguments_only() {
    let get = find_method("bytes", "get").unwrap();
    assert_eq!(arity_bounds(get), (2, 2));
    assert!(check_arity(get, 2).is_ok());
    assert!(check_arity(get, 1).is_err());
    assert!(check_arity(get, 3).is_err());

    let slice = find_method("bytes", "slice").unwrap();
    assert_eq!(arity_bounds(slice), (2, 3));
    assert!(check_arity(slice, 2).is_ok());
    assert!(check_arity(slice, 3).is_ok());
    assert!(check_arity(slice, 1).is_err());
    assert!(check_arity(slice, 4).is_err());

    let now = find_method("time", "now").unwrap();
    assert_eq!(arity_bounds(now), (0, 0));
    assert!(check_arity(now, 0).is_ok());
    assert!(check_arity(now, 1).is_err());
}

/// RFD 15's stricter bytes shape is the contract deka#756 codifies. Pin it:
/// strict fallible decode, immutability, no aliasing views, no lossy path,
/// validation instead of coercion.
#[test]
fn bytes_family_follows_rfd15_not_rfd21_lossy_shape() {
    // `to_string` is Unsafe (strict UTF-8, Err on failure) — RFD 21's
    // table says safe/lossy; RFD 15 wins (deka#756 inherits this).
    let to_string = find_method("bytes", "to_string").expect("to_string present");
    assert_eq!(to_string.safety, Safety::Unsafe);
    assert_eq!(to_string.ret, ReturnShape::ResultValue(ValType::String));

    // No lossy variant: silent replacement characters are the defect
    // RFD 15 exists to remove.
    assert!(find_method("bytes", "to_string_lossy").is_none());

    // Immutable bytes: no mutating `set`.
    assert!(find_method("bytes", "set").is_none());

    // No view-producing `subarray`; `slice` copies (RFD 15).
    assert!(find_method("bytes", "subarray").is_none());
    let slice = find_method("bytes", "slice").unwrap();
    assert_eq!(slice.ret, ReturnShape::Value(ValType::Bytes));

    // Total conversions stay safe; fallible decoders return Option values.
    assert_eq!(
        find_method("bytes", "from_string").unwrap().safety,
        Safety::Safe
    );
    assert_eq!(
        find_method("bytes", "from_hex").unwrap().ret,
        ReturnShape::OptionValue(ValType::Bytes)
    );
    assert_eq!(
        find_method("bytes", "get").unwrap().ret,
        ReturnShape::OptionValue(ValType::Number)
    );

    // `from_array` validates: invalid elements are None, never coerced
    // (Uint8Array.from wraps negatives, truncates fractions, reduces
    // mod 256 — all silent alteration).
    assert_eq!(
        find_method("bytes", "from_array").unwrap().ret,
        ReturnShape::OptionValue(ValType::Bytes)
    );
}

#[test]
fn classification_is_by_return_type() {
    assert_eq!(find_method("json", "parse").unwrap().safety, Safety::Unsafe);
    assert_eq!(
        find_method("json", "stringify").unwrap().safety,
        Safety::Unsafe
    );
    assert_eq!(
        find_method("json", "validate").unwrap().ret,
        ReturnShape::Value(ValType::Bool)
    );
    assert_eq!(find_method("io", "echo").unwrap().safety, Safety::Safe);
    assert_eq!(find_method("time", "now").unwrap().safety, Safety::Safe);
}

/// Emitted-JS proof (issue acceptance): the shipped helper source must not
/// publish on globalThis and must not mutate prototypes.
#[test]
fn helper_js_never_touches_global_this_or_prototypes() {
    assert!(
        !CATALOG_HELPERS_JS.contains("globalThis"),
        "catalog helpers must not touch globalThis"
    );
    assert!(
        !CATALOG_HELPERS_JS.contains(".prototype."),
        "catalog helpers must not mutate prototypes"
    );
    assert!(
        !CATALOG_HELPERS_JS.contains("Object.assign"),
        "frozen literal surface only"
    );
}

/// Every catalog entry has a shipped JS implementation with the same name
/// — the "zero stubs" rule: nothing is catalogued that does not exist.
#[test]
fn every_catalog_entry_is_implemented_in_helper_js() {
    for kind in DEKA_CATALOG {
        for method in kind.methods {
            let needle = format!("{}( ", method.name);
            let needle2 = format!("{}(", method.name);
            assert!(
                CATALOG_HELPERS_JS.contains(&needle) || CATALOG_HELPERS_JS.contains(&needle2),
                "deka.{}.{} is catalogued but not implemented in CATALOG_HELPERS_JS",
                kind.name,
                method.name
            );
        }
    }
}

/// deka#801: the catalog must be uninfluenceable by the process
/// environment. Poison every DEKA_* override and prove lookups are
/// unchanged.
#[test]
fn catalog_is_environment_independent() {
    for (key, value) in [
        ("DEKA_CATALOG", "bytes.len=bogus"),
        ("DEKA_HOST_GRANTS", "[]"),
        ("DEKA_SECURITY_POLICY", "{}"),
        ("DEKA_DSC", "/nonexistent"),
    ] {
        // SAFETY: test-only env mutation; serialized by the test harness.
        unsafe { std::env::set_var(key, value) };
    }
    let looked_up = (
        find_method("bytes", "len").map(|m| m.safety),
        find_method("bytes", "bogus"),
        DEKA_CATALOG.len(),
    );
    for key in [
        "DEKA_CATALOG",
        "DEKA_HOST_GRANTS",
        "DEKA_SECURITY_POLICY",
        "DEKA_DSC",
    ] {
        // SAFETY: test-only env restore; serialized by the test harness.
        unsafe { std::env::remove_var(key) };
    }
    assert_eq!(looked_up.0, Some(Safety::Safe));
    assert_eq!(looked_up.1, None);
    assert!(looked_up.2 >= 4);
}

/// Locate a node binary once; behavior proofs skip without it.
fn node_available() -> bool {
    static NODE: OnceLock<bool> = OnceLock::new();
    *NODE.get_or_init(|| {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

use std::sync::OnceLock;

/// Run a node -e script, asserting success, and return its stdout.
fn run_node(script: &str) -> String {
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .output()
        .expect("exec node");
    assert!(
        output.status.success(),
        "node behavior proof failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Node-driven behavior proof when node is available (skipped otherwise):
/// safe helpers never throw and return their declared shape, including the
/// failure paths; unsafe helpers throw for the failure path.
#[test]
fn helper_js_behavior_via_node_when_available() {
    if !node_available() {
        eprintln!("node not available; skipping helper behavior proof");
        return;
    }
    let script = format!(
        r#"
const Option = {{
  Some: (value) => ({{ __enum: "Option", __case: "Some", name: "Some", value }}),
  None: {{ __enum: "Option", __case: "None", name: "None" }},
}};
const c = {CATALOG_HELPERS_JS};
const assert = require("node:assert/strict");
// safe: total, declared types.
assert.equal(c.bytes.len(c.bytes.from_string("abc")), 3);
assert.equal(c.bytes.get(c.bytes.from_string("ab"), 5).__case, "None");
assert.equal(c.bytes.get(c.bytes.from_string("ab"), 1).value, 98);
const sl = c.bytes.slice(c.bytes.from_string("abcd"), 1, 3);
assert.deepEqual([...sl], [98, 99]);
const cat = c.bytes.concat(c.bytes.from_string("ab"), c.bytes.from_string("cd"));
assert.deepEqual([...cat], [97, 98, 99, 100]);
// from_array is partial: valid input is Some, invalid is None.
assert.deepEqual([...c.bytes.from_array([1, 2, 255]).value], [1, 2, 255]);
assert.equal(c.bytes.to_hex(c.bytes.from_string("ab")), "6162");
assert.deepEqual([...c.bytes.from_hex("6162").value], [97, 98]);
assert.deepEqual([...c.bytes.from_hex("616B").value], [97, 107]); // uppercase digits
// failure path: a value the caller must handle, never fabricated bytes.
assert.equal(c.bytes.from_hex("zz").__case, "None");
assert.equal(c.bytes.from_hex("abc").__case, "None");
const b64 = c.bytes.to_base64(c.bytes.from_string("abc"));
assert.equal(b64, "YWJj");
assert.deepEqual([...c.bytes.from_base64(b64).value], [97, 98, 99]);
assert.equal(c.bytes.from_base64("!!").__case, "None");
assert.equal(c.bytes.from_base64("YQ").__case, "None"); // unpadded is invalid input
// unsafe: strict decode throws on invalid UTF-8 (no lossy substitution).
assert.throws(() => c.bytes.to_string(new Uint8Array([0xff])));
assert.equal(c.bytes.to_string(c.bytes.from_string("ok")), "ok");
// json + io + time.
assert.equal(c.json.validate("{{}}"), true);
assert.equal(c.json.validate("{{"), false);
assert.throws(() => c.json.parse("{{"));
assert.equal(typeof c.time.now(), "number");
// no ambient publication, nothing mutable.
assert.equal(typeof globalThis.__dekaCatalogBuild, "undefined");
assert.equal(globalThis.__dekaCatalog, undefined);
assert.ok(Object.isFrozen(c) && Object.isFrozen(c.bytes));
console.log("ok");
"#
    );
    assert_eq!(run_node(&script).trim(), "ok");
}

/// The silent-corruption matrix (deka#756 acceptance): every malformed
/// input is an error value — None for safe decoders, a throw for the
/// strict unsafe decode — never altered bytes.
#[test]
fn helper_js_rejects_malformed_input_as_values_via_node() {
    if !node_available() {
        eprintln!("node not available; skipping malformed-input proof");
        return;
    }
    let script = format!(
        r#"
const Option = {{
  Some: (value) => ({{ __enum: "Option", __case: "Some", name: "Some", value }}),
  None: {{ __enum: "Option", __case: "None", name: "None" }},
}};
const c = {CATALOG_HELPERS_JS};
const assert = require("node:assert/strict");
const isNone = (v) => v.__case === "None";

// --- hex: malformed is None, never a prefix-parsed or truncated buffer ---
// "6g" is the load-bearing case: parseInt would read the "6" and silently
// emit 0x06. So would any prefix parser.
for (const bad of ["6g", "g6", "zz", "0x61", "61 62", "6 1", "６１", "61\n62"]) {{
  assert.ok(isNone(c.bytes.from_hex(bad)), `from_hex({{JSON.stringify(bad)}}) must be None`);
}}
assert.ok(isNone(c.bytes.from_hex("abc")), "odd length is None");
assert.ok(isNone(c.bytes.from_hex("61 6")), "odd length with garbage is None");
// Valid extremes still parse.
assert.deepEqual([...c.bytes.from_hex("").value], []);
assert.deepEqual([...c.bytes.from_hex("00ff10").value], [0, 255, 16]);
assert.deepEqual([...c.bytes.from_hex("00FF10").value], [0, 255, 16]);

// --- base64: malformed is None ---
for (const bad of ["!!", "YQ", "Y", "YWI", "YQ=", "YQ===", "Y Q=", "YQ==YQ==", "====", "=W==", "éé=="]) {{
  assert.ok(isNone(c.bytes.from_base64(bad)), `from_base64({{JSON.stringify(bad)}}) must be None`);
}}
assert.deepEqual([...c.bytes.from_base64("").value], []);
assert.deepEqual([...c.bytes.from_base64("TQ==").value], [77]);
assert.deepEqual([...c.bytes.from_base64("TWE=").value], [77, 97]);
assert.deepEqual([...c.bytes.from_base64("TWFu").value], [77, 97, 110]);

// --- from_array: invalid elements are None, never coerced ---
// Uint8Array.from would: truncate 1.5 -> 1, wrap -1 -> 255,
// reduce 256 -> 0, turn "97" and NaN into 0.
for (const bad of [[1.5], [-1], [256], [300], ["97"], [NaN], [null], [97, 256], [undefined]]) {{
  assert.ok(isNone(c.bytes.from_array(bad)), `from_array(${{JSON.stringify(bad)}}) must be None`);
}}
assert.deepEqual([...c.bytes.from_array([]).value], []);
assert.deepEqual([...c.bytes.from_array([0, 127, 128, 255]).value], [0, 127, 128, 255]);

// --- get: non-integer and out-of-range indices are None ---
const ab = c.bytes.from_string("ab");
assert.ok(isNone(c.bytes.get(ab, 1.5)), "fractional index is None, not truncated");
assert.ok(isNone(c.bytes.get(ab, -1)), "negative index is None");
assert.ok(isNone(c.bytes.get(ab, 2)), "index == length is None");
assert.equal(c.bytes.get(ab, 0).value, 97);

// --- to_string: every malformed UTF-8 shape throws (Err under unsafe) ---
for (const bad of [
  [0xff],                    // invalid lead byte
  [0xc2],                    // truncated sequence
  [0xc2, 0x20],              // invalid continuation
  [0xc0, 0x80],              // overlong NUL
  [0xed, 0xa0, 0x80],        // surrogate
  [0xf4, 0x90, 0x80, 0x80],  // beyond U+10FFFF
  [0x61, 0xc2],              // valid then truncated
]) {{
  assert.throws(() => c.bytes.to_string(new Uint8Array(bad)), `to_string(${{bad}}) must throw`);
}}
// Round trip across the full byte range and multi-byte sequences.
const all = new Uint8Array(256);
for (let i = 0; i < 256; i++) all[i] = i;
const allHex = c.bytes.to_hex(all);
assert.equal(allHex.length, 512);
assert.deepEqual([...c.bytes.from_hex(allHex).value], [...all]);
const b64All = c.bytes.to_base64(all);
assert.deepEqual([...c.bytes.from_base64(b64All).value], [...all]);
assert.equal(c.bytes.to_string(c.bytes.from_string("héllo 𝄞")), "héllo 𝄞");
console.log("ok");
"#
    );
    assert_eq!(run_node(&script).trim(), "ok");
}

/// Immutable ownership (deka#756 acceptance): no helper mutates its input,
/// and no helper returns a view over a buffer it does not own — mutating
/// any returned bytes can never change another value's bytes.
#[test]
fn helper_js_bytes_never_alias_or_mutate_via_node() {
    if !node_available() {
        eprintln!("node not available; skipping aliasing proof");
        return;
    }
    let script = format!(
        r#"
const Option = {{
  Some: (value) => ({{ __enum: "Option", __case: "Some", name: "Some", value }}),
  None: {{ __enum: "Option", __case: "None", name: "None" }},
}};
const c = {CATALOG_HELPERS_JS};
const assert = require("node:assert/strict");
const snap = (b) => Array.from(b);

const src = c.bytes.from_string("hello");
const before = snap(src);

// Read-only helpers must not mutate their input.
c.bytes.to_hex(src);
c.bytes.to_base64(src);
c.bytes.to_string(src);
c.bytes.len(src);
c.bytes.get(src, 0);
c.bytes.slice(src, 0);
c.bytes.concat(src, src);
c.bytes.from_array(snap(src));
assert.deepEqual(snap(src), before, "read helpers mutated their input");

// slice copies: mutating the result leaves the source untouched.
const sl = c.bytes.slice(src, 1, 3);
sl[0] = 0;
assert.deepEqual(snap(src), before, "slice result aliases the source");

// concat allocates fresh: mutating the result leaves both inputs untouched.
const cat = c.bytes.concat(src, src);
cat[0] = 0;
assert.deepEqual(snap(src), before, "concat result aliases an input");

// Decoders allocate fresh buffers.
const dec = c.bytes.from_hex("6162").value;
dec[0] = 0;
assert.deepEqual(snap(src), before);
const dec64 = c.bytes.from_base64("aGVsbG8=").value;
dec64[0] = 0;
assert.deepEqual(snap(src), before);

// No mutator exists on the surface at all.
assert.equal(c.bytes.set, undefined);
assert.equal(c.bytes.subarray, undefined);
assert.equal(c.bytes.fill, undefined);
assert.ok(Object.isFrozen(c.bytes));
console.log("ok");
"#
    );
    assert_eq!(run_node(&script).trim(), "ok");
}
