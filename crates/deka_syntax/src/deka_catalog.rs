//! Compiler-owned closed RFD 21 catalog (deka#881).
//! Helpers are emitted as module-private bindings; no runtime installation is needed.

/// A catalog value type — the DS-level type of an argument or success value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    /// `f64` / integer number.
    Number,
    /// UTF-8 string.
    String,
    /// Boolean.
    Bool,
    /// Immutable byte payload (`Uint8Array` representation, RFD 15).
    Bytes,
    /// `void` / unit.
    Unit,
    /// `Array<number>` (e.g. a byte list before `from_array`).
    NumList,
    /// Free-form JSON value (`unknown` on the DS surface).
    Any,
}

impl ValType {
    pub fn name(self) -> &'static str {
        match self {
            ValType::Number => "number",
            ValType::String => "string",
            ValType::Bool => "boolean",
            ValType::Bytes => "bytes",
            ValType::Unit => "void",
            ValType::NumList => "Array<number>",
            ValType::Any => "unknown",
        }
    }
}

/// The success shape of a helper, which fixes how the DS surface types it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnShape {
    /// `safe`: yields `T` directly.
    Value(ValType),
    /// `safe`: total, but partial — failure is `Option.None`, never a throw.
    OptionValue(ValType),
    /// `unsafe`: yields `Result<T, string>`; the `Err` payload is the thrown
    /// value's message, per dsc's bare `unsafe { }` contract (dsc#103).
    ResultValue(ValType),
}

/// Whether a helper can throw for arguments of its declared types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    /// Cannot throw; reachable under `safe { }` (and, redundantly, `unsafe`).
    Safe,
    /// May throw; reachable only under `unsafe { }`, which maps the throw to
    /// a `Result.Err` value.
    Unsafe,
}

/// One positional argument of a catalog helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogArg {
    pub name: &'static str,
    pub ty: ValType,
    /// Trailing optional arguments (`end?`) may be omitted at the call site.
    pub optional: bool,
}

const fn arg(name: &'static str, ty: ValType) -> CatalogArg {
    CatalogArg {
        name,
        ty,
        optional: false,
    }
}

const fn opt_arg(name: &'static str, ty: ValType) -> CatalogArg {
    CatalogArg {
        name,
        ty,
        optional: true,
    }
}

/// A single catalog helper: `deka.<kind>.<name>(args...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogMethod {
    pub name: &'static str,
    pub safety: Safety,
    pub args: &'static [CatalogArg],
    pub ret: ReturnShape,
    /// One-line contract note for diagnostics and docs.
    pub doc: &'static str,
}

/// A catalog kind and its helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogKind {
    pub name: &'static str,
    pub methods: &'static [CatalogMethod],
}

const BYTES_METHODS: &[CatalogMethod] = &[
    CatalogMethod {
        name: "len",
        safety: Safety::Safe,
        args: &[arg("b", ValType::Bytes)],
        ret: ReturnShape::Value(ValType::Number),
        doc: "byte length (byteLength)",
    },
    CatalogMethod {
        name: "get",
        safety: Safety::Safe,
        args: &[arg("b", ValType::Bytes), arg("index", ValType::Number)],
        // RFD 21: "out of range is a value, not a throw". RFD 15 (deka#756):
        // a non-integer index is the same kind of bounds failure — None, not
        // a silently truncated lookup.
        ret: ReturnShape::OptionValue(ValType::Number),
        doc: "indexed byte; non-integer or out of range is None, not a throw",
    },
    CatalogMethod {
        name: "slice",
        safety: Safety::Safe,
        args: &[
            arg("b", ValType::Bytes),
            arg("start", ValType::Number),
            opt_arg("end", ValType::Number),
        ],
        // RFD 15: copies. There is no `subarray` — a view could alias a
        // mutable buffer and escape as immutable bytes.
        ret: ReturnShape::Value(ValType::Bytes),
        doc: "copy of the byte range (never a view)",
    },
    CatalogMethod {
        name: "concat",
        safety: Safety::Safe,
        args: &[arg("a", ValType::Bytes), arg("b", ValType::Bytes)],
        ret: ReturnShape::Value(ValType::Bytes),
        doc: "fresh buffer, alloc + copy",
    },
    CatalogMethod {
        name: "from_array",
        safety: Safety::Safe,
        args: &[arg("items", ValType::NumList)],
        // RFD 15 (deka#756): every element must be an integer in 0..=255.
        // `Uint8Array.from` coerces (wraps negatives, truncates fractions,
        // reduces mod 256) — silently altered bytes. Reject instead.
        ret: ReturnShape::OptionValue(ValType::Bytes),
        doc: "from Array<number>; non-integer or out-of-range element is None",
    },
    CatalogMethod {
        name: "from_string",
        safety: Safety::Safe,
        args: &[arg("s", ValType::String)],
        ret: ReturnShape::Value(ValType::Bytes),
        doc: "TextEncoder.encode — total",
    },
    CatalogMethod {
        name: "to_hex",
        safety: Safety::Safe,
        args: &[arg("b", ValType::Bytes)],
        ret: ReturnShape::Value(ValType::String),
        doc: "lowercase hex; we own the loop",
    },
    CatalogMethod {
        name: "from_hex",
        safety: Safety::Safe,
        args: &[arg("s", ValType::String)],
        // RFD 21 rule 4: invalid input maps to Option.None — a value the
        // caller must handle, never a fabricated buffer.
        ret: ReturnShape::OptionValue(ValType::Bytes),
        doc: "hex decode; invalid input is None",
    },
    CatalogMethod {
        name: "to_base64",
        safety: Safety::Safe,
        args: &[arg("b", ValType::Bytes)],
        ret: ReturnShape::Value(ValType::String),
        doc: "standard base64; we own the loop",
    },
    CatalogMethod {
        name: "from_base64",
        safety: Safety::Safe,
        args: &[arg("s", ValType::String)],
        ret: ReturnShape::OptionValue(ValType::Bytes),
        doc: "base64 decode; invalid input is None",
    },
    CatalogMethod {
        name: "to_string",
        // RFD 15 over RFD 21: strict UTF-8 decode (TextDecoder fatal:true)
        // throws on invalid input, so the throw surfaces as Result.Err under
        // `unsafe`. There is intentionally no lossy variant.
        safety: Safety::Unsafe,
        args: &[arg("b", ValType::Bytes)],
        ret: ReturnShape::ResultValue(ValType::String),
        doc: "strict UTF-8 decode; invalid input is Err, never lossy",
    },
];

const JSON_METHODS: &[CatalogMethod] = &[
    CatalogMethod {
        name: "parse",
        safety: Safety::Unsafe,
        args: &[arg("s", ValType::String)],
        ret: ReturnShape::ResultValue(ValType::Any),
        doc: "JSON.parse — throws on malformed input",
    },
    CatalogMethod {
        name: "stringify",
        safety: Safety::Unsafe,
        args: &[arg("v", ValType::Any)],
        ret: ReturnShape::ResultValue(ValType::String),
        doc: "JSON.stringify — throws on cycles / bigint",
    },
    CatalogMethod {
        name: "validate",
        safety: Safety::Safe,
        args: &[arg("s", ValType::String)],
        // RFD 21 rule 4: the helper maps the throw to a boolean, so this is
        // total and classified safe.
        ret: ReturnShape::Value(ValType::Bool),
        doc: "true iff the input parses; never throws",
    },
];

const IO_METHODS: &[CatalogMethod] = &[CatalogMethod {
    name: "echo",
    safety: Safety::Safe,
    args: &[arg("message", ValType::String)],
    ret: ReturnShape::Value(ValType::Unit),
    doc: "one line of program output; console.log rebound to stdout",
}];

const TIME_METHODS: &[CatalogMethod] = &[CatalogMethod {
    name: "now",
    safety: Safety::Safe,
    args: &[],
    ret: ReturnShape::Value(ValType::Number),
    doc: "milliseconds since the Unix epoch; Date.now does not throw",
}];

/// The single authoritative `deka.*` catalog (RFD 21). Closed: a call outside
/// this table is a source diagnostic. Every entry is genuinely implemented in
/// [`CATALOG_HELPERS_JS`] — zero stubs. RFD 21's `deka.math` / `deka.string`
/// / `deka.cookies` families are deliberately absent until their owning
/// migration lands them (RFD 21 order steps 4–5), not stubbed.
pub const DEKA_CATALOG: &[CatalogKind] = &[
    CatalogKind {
        name: "bytes",
        methods: BYTES_METHODS,
    },
    CatalogKind {
        name: "json",
        methods: JSON_METHODS,
    },
    CatalogKind {
        name: "io",
        methods: IO_METHODS,
    },
    CatalogKind {
        name: "time",
        methods: TIME_METHODS,
    },
];

/// Look up a kind by name.
pub fn find_kind(name: &str) -> Option<&'static CatalogKind> {
    DEKA_CATALOG.iter().find(|kind| kind.name == name)
}

/// Look up a method by kind and method name.
pub fn find_method(kind: &str, method: &str) -> Option<&'static CatalogMethod> {
    find_kind(kind)?
        .methods
        .iter()
        .find(|candidate| candidate.name == method)
}

/// Whether `name` is a catalog kind (used by the loader's compiled-JS gate).
pub fn is_catalog_kind(name: &str) -> bool {
    find_kind(name).is_some()
}

/// Arity bounds for a method: `(required, total)`.
pub fn arity_bounds(method: &CatalogMethod) -> (usize, usize) {
    let total = method.args.len();
    let required = method.args.iter().take_while(|a| !a.optional).count();
    (required, total)
}

/// Validate a call's argument count, returning a caller-facing message.
pub fn check_arity(method: &CatalogMethod, argc: usize) -> Result<(), String> {
    let (required, total) = arity_bounds(method);
    if argc < required || argc > total {
        let expected = if required == total {
            format!("{total}")
        } else {
            format!("{required}..{total}")
        };
        return Err(format!("expected {expected} argument(s), found {argc}"));
    }
    Ok(())
}

/// JavaScript implementation of every catalog entry — the "our JS" RFD 21's
/// two doors call. The compiler binds this expression once per emitted module.
/// Option constructors are read lazily so binding can precede the shared prelude.
/// Nothing here is published on `globalThis` and nothing mutates
/// a prototype. It is an expression (not declarations) so evaluating it can
/// never leak a binding into the realm.
///
/// Helpers take only the argument types their catalog entry declares. `safe`
/// helpers are total over those types; `Option` results are built from the
/// realm prelude constructors so the value is a real DS `Option` on the
/// surface. `unsafe` helpers throw on failure and are compiled under
/// dsc's try/catch, which converts the throw into `Result.Err`.
pub const CATALOG_HELPERS_JS: &str = r#"
(function () {
  "use strict";
  const some = (value) => Option.Some(value);
  const none = () => Option.None;
  const BASE64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  const bytes = {
    len(b) { return b.byteLength; },
    get(b, index) {
      // Non-integer and out-of-range are the same failure: a bounds value,
      // never a truncated or wrapped lookup.
      if (!Number.isInteger(index)) return none();
      return index >= 0 && index < b.byteLength ? some(b[index]) : none();
    },
    slice(b, start, end) {
      // Uint8Array#slice copies; the result never aliases `b`.
      return b.slice(start, end === undefined ? b.byteLength : end);
    },
    concat(a, b) {
      const out = new Uint8Array(a.byteLength + b.byteLength);
      out.set(a, 0);
      out.set(b, a.byteLength);
      return out;
    },
    from_array(items) {
      // RFD 15: validate before allocating. Uint8Array.from coerces —
      // fractions truncate, negatives wrap, >255 reduces mod 256 — which is
      // exactly the silent-alteration failure mode this catalog forbids.
      for (let i = 0; i < items.length; i++) {
        const v = items[i];
        if (typeof v !== "number" || !Number.isInteger(v) || v < 0 || v > 255) {
          return none();
        }
      }
      return some(Uint8Array.from(items));
    },
    from_string(s) { return new TextEncoder().encode(s); },
    to_hex(b) {
      let out = "";
      for (let i = 0; i < b.byteLength; i++) out += b[i].toString(16).padStart(2, "0");
      return out;
    },
    from_hex(s) {
      // Strict: validate the whole string first. parseInt parses a numeric
      // prefix, so "6g" would silently decode to 0x06 — altered bytes.
      if (s.length % 2 !== 0) return none();
      if (!/^[0-9a-fA-F]*$/.test(s)) return none();
      const out = new Uint8Array(s.length / 2);
      for (let i = 0; i < out.length; i++) {
        out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
      }
      return some(out);
    },
    to_base64(b) {
      let out = "";
      for (let i = 0; i < b.byteLength; i += 3) {
        const n = (b[i] << 16) | ((i + 1 < b.byteLength ? b[i + 1] : 0) << 8) | (i + 2 < b.byteLength ? b[i + 2] : 0);
        out += BASE64[(n >> 18) & 63] + BASE64[(n >> 12) & 63]
          + (i + 1 < b.byteLength ? BASE64[(n >> 6) & 63] : "=")
          + (i + 2 < b.byteLength ? BASE64[n & 63] : "=");
      }
      return out;
    },
    from_base64(s) {
      if (s.length % 4 !== 0) return none();
      const pad = s.endsWith("==") ? 2 : s.endsWith("=") ? 1 : 0;
      const clean = pad ? s.slice(0, s.length - pad) : s;
      // A residual group of one sextet carries no whole bytes.
      if (clean.length % 4 === 1) return none();
      const rem = clean.length % 4;
      const length = (clean.length >> 2) * 3 + (rem === 2 ? 1 : rem === 3 ? 2 : 0);
      const out = new Uint8Array(length);
      let o = 0;
      for (let i = 0; i < clean.length; i += 4) {
        const chunk = [0, 1, 2, 3].map((k) => {
          const c = i + k < clean.length ? clean[i + k] : "A";
          const v = BASE64.indexOf(c);
          return v < 0 ? -1 : v;
        });
        if (chunk.some((v) => v < 0)) return none();
        const n = (chunk[0] << 18) | (chunk[1] << 12) | (chunk[2] << 6) | chunk[3];
        if (o < out.length) out[o++] = (n >> 16) & 255;
        if (o < out.length) out[o++] = (n >> 8) & 255;
        if (o < out.length) out[o++] = n & 255;
      }
      return some(out);
    },
    to_string(b) {
      // Strict UTF-8 (RFD 15): throws on invalid input so the failure is an
      // Err value under `unsafe` — never a lossy substitution. Decoded by
      // hand because the realm's TextDecoder polyfill (wintertc.js) ignores
      // `fatal` and never throws: the catalog cannot delegate its safety
      // property to a mutable ambient global.
      const arr = b instanceof Uint8Array ? b : new Uint8Array(b);
      let out = "";
      let i = 0;
      while (i < arr.length) {
        const lead = arr[i];
        let cp;
        let need;
        let min;
        if (lead < 0x80) {
          out += String.fromCharCode(lead);
          i++;
          continue;
        } else if (lead >= 0xc2 && lead < 0xe0) {
          cp = lead & 0x1f;
          need = 1;
          min = 0x80;
        } else if (lead >= 0xe0 && lead < 0xf0) {
          cp = lead & 0x0f;
          need = 2;
          min = 0x800;
        } else if (lead >= 0xf0 && lead < 0xf5) {
          cp = lead & 0x07;
          need = 3;
          min = 0x10000;
        } else {
          throw new Error("invalid UTF-8 lead byte");
        }
        if (i + need >= arr.length) throw new Error("truncated UTF-8 sequence");
        for (let k = 1; k <= need; k++) {
          const cont = arr[i + k];
          if ((cont & 0xc0) !== 0x80) throw new Error("invalid UTF-8 continuation byte");
          cp = (cp << 6) | (cont & 0x3f);
        }
        if (cp < min) throw new Error("overlong UTF-8 sequence");
        if (cp >= 0xd800 && cp < 0xe000) throw new Error("UTF-8 encodes a surrogate");
        if (cp > 0x10ffff) throw new Error("UTF-8 code point out of range");
        if (cp < 0x10000) {
          out += String.fromCharCode(cp);
        } else {
          out += String.fromCharCode(0xd800 + ((cp - 0x10000) >> 10), 0xdc00 + ((cp - 0x10000) & 0x3ff));
        }
        i += need + 1;
      }
      return out;
    },
  };

  const json = {
    parse(s) { return JSON.parse(s); },
    stringify(v) { return JSON.stringify(v); },
    validate(s) {
      try { JSON.parse(s); return true; } catch (_) { return false; }
    },
  };

  const io = {
    echo(message) { console.log(message); },
  };

  const time = {
    now() { return Date.now(); },
  };

  return Object.freeze({
    bytes: Object.freeze(bytes),
    json: Object.freeze(json),
    io: Object.freeze(io),
    time: Object.freeze(time),
  });
})()
"#;

#[cfg(test)]
#[path = "deka_catalog_tests.rs"]
mod tests;
