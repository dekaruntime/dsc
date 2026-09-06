//! Type representation used by the v2 typechecker.

use std::fmt;

/// Type used internally by the typechecker.
///
/// `Type::None` is the type of the literal `none`.  It is distinct from
/// `Option<T>` but assignable to any `Option<T>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type<'a> {
    /// Sentinel used for error recovery.
    Error,
    /// Placeholder used while a function's return type is being inferred, and
    /// the marker for "the checker could not work this out".
    Infer,
    /// An unconstrained type variable.
    ///
    /// Produced where a construct genuinely does not mention a type rather than
    /// failing to determine one: the `E` in `Ok(x)`, the `T` in `None`, the
    /// element type of `[]`, the result element of `map`. It asserts nothing,
    /// so it unifies with any type — which is sound precisely because there is
    /// no claim to violate.
    ///
    /// Distinct from `Infer` on purpose (deka#468). Collapsing the two is what
    /// made deka#252 intractable: "I could not work this out" and "this is
    /// genuinely open" were spelled identically, so tightening one broke the
    /// other.
    Var,
    /// The bottom type (`never`). Not produced by the parser, but accepted in
    /// annotations for forward compatibility.
    Never,
    /// The type of the literal `none`.
    None,
    /// A named scalar or user-defined type.
    Named { name: &'a str },
    /// `Option<T>`.
    Option { inner: Box<Type<'a>> },
    /// Function type.
    Function {
        params: Vec<Type<'a>>,
        ret: Box<Type<'a>>,
        /// Number of trailing parameters that have default values and may be
        /// omitted at call sites.
        optional: usize,
    },
    /// Generic instantiation, e.g. `Result<number, string>`.
    Generic { base: &'a str, args: Vec<Type<'a>> },
    /// A user-defined struct type.
    Struct { name: &'a str },
    /// An array type, e.g. `Array<number>` or `number[]`.
    Array { elem: Box<Type<'a>> },
    /// An object record type with known fields.
    Object { fields: Vec<(&'a str, Type<'a>)> },
    /// A declared interface type.
    Interface { name: &'a str },
    /// A boxed newtype over a primitive representation.
    Newtype {
        name: &'a str,
        repr: crate::ast::NewtypeRepr,
    },
    /// A type parameter, e.g. `T` inside a generic function or type.
    Param { name: &'a str },
    /// A union of types whose members each have a decidable runtime
    /// predicate: primitives, named structs, enums, interfaces (rfd#42,
    /// deka#530).
    Union { members: Vec<Type<'a>> },
}

impl<'a> Type<'a> {
    pub fn is_error(&self) -> bool {
        matches!(self, Type::Error)
    }

    /// Return the element type exposed by a collection operation.
    ///
    /// `Var` is deliberately preserved for an unconstrained collection (for
    /// example the element type of `[]`).  `Infer` remains the fallback for a
    /// value that is not a collection or whose type is still opaque.
    pub(super) fn collection_element(&self) -> Self {
        match self {
            Type::Array { elem } => elem.as_ref().clone(),
            Type::Named { name: "string" } => Type::Named { name: "string" },
            Type::Var => Type::Var,
            Type::Error => Type::Error,
            _ => Type::Infer,
        }
    }

    pub fn from_newtype_repr(repr: crate::ast::NewtypeRepr) -> Self {
        match repr {
            crate::ast::NewtypeRepr::Number => Type::Named { name: "number" },
            crate::ast::NewtypeRepr::String => Type::Named { name: "string" },
            crate::ast::NewtypeRepr::Bool => Type::Named { name: "boolean" },
        }
    }
}

/// Replace type parameters according to `subst`.
///
/// Lives in `types` (not `expr`) because the `super` descriptor walker
/// (deka#529) substitutes struct/enum type arguments the same way call-site
/// substitution does.
pub fn substitute_type<'a>(ty: &Type<'a>, subst: &std::collections::HashMap<&'a str, Type<'a>>) -> Type<'a> {
    match ty {
        Type::Param { name } => subst.get(name).cloned().unwrap_or_else(|| Type::Param { name }),
        Type::Option { inner } => Type::Option {
            inner: Box::new(substitute_type(inner, subst)),
        },
        Type::Array { elem } => Type::Array {
            elem: Box::new(substitute_type(elem, subst)),
        },
        Type::Function { params, ret, optional } => Type::Function {
            params: params.iter().map(|p| substitute_type(p, subst)).collect(),
            ret: Box::new(substitute_type(ret, subst)),
            optional: *optional,
        },
        Type::Generic { base, args } => Type::Generic {
            base,
            args: args.iter().map(|a| substitute_type(a, subst)).collect(),
        },
        Type::Union { members } => Type::Union {
            members: members.iter().map(|m| substitute_type(m, subst)).collect(),
        },
        other => other.clone(),
    }
}

/// How a primitive conversion call (`parseNumber(x)`, `unboxNumber(x)`,
/// `toNumber(x)`, `string(x)`) should be lowered after
/// typechecking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnwrapKind {
    /// The argument is already the primitive; erase the call.
    Identity,
    /// The argument is a newtype; access its payload via `__p`.
    Payload,
    /// Widen the argument to `string` (`String(x)` in JS).
    WidenToString,
    /// Widen the argument to `toNumber` (`Number(x)` in JS).
    WidenToNumber,
    /// Convert a string argument to `Option<number>` for `parseNumber`
    /// (`Number(x)` wrapped).
    StringToOptionNumber,
}

/// Which operand of a mixed newtype/primitive operation is the newtype.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewtypeSide {
    Left,
    Right,
}

/// Which array builtin a recorded call site rewrites to (deka#561,
/// deka#566). JS has no `Array.prototype.first`/`last`, and `pop`/`shift`
/// return raw values rather than the `Option<T>` the type system declares,
/// so the emitter rewrites all four to an Option-producing expression:
/// a real `Some`/`None` construction at the site, not a trust-that-JS-
/// lines-up passthrough. `pop`/`shift` additionally mutate, which is why
/// the checker rejects immutable receivers before emission (const arrays
/// are frozen at creation — a runtime pop would throw).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayAccess {
    First,
    Last,
    Pop,
    Shift,
}

/// Whether a builtin `Math`-backed method on `number` is total or partial
/// (deka#378 step 2, rfd#40 phase 2). JS numbers have no such methods, so
/// the emitter rewrites calls to `Math.*` expressions; the checker records
/// each call site, parallel to `array_first_last_calls`.
///
/// A partial method is one where JavaScript answers some inputs with `NaN`
/// (`Math.sqrt(-1)`, `Math.log(-1)`, `Math.pow(-2, 0.5)`). `NaN` is a value
/// of type `number` that is not a number — the exact lie rfd#13 calls the
/// worst class of bug — so the rewrite returns `Option<number>` and yields
/// `None` exactly where JS would produce `NaN`. `Infinity` is a legitimate
/// IEEE-754 value and passes through as `Some`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberMath {
    /// Total: `Math.<name>` answers every input with a genuine number.
    Total,
    /// Partial: wrap the result, `None` exactly where JS produces `NaN`.
    Partial,
}

/// The runtime predicate a union member type-pattern compiles to (rfd#42,
/// deka#530). Computed by the checker and handed to the emitter through
/// `TypeckResult::union_type_patterns`, parallel to `enum_case_patterns`,
/// so the emitter never re-derives types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnionMemberTest<'a> {
    /// Primitive scalar: `typeof x === "..."` (string/number/boolean;
    /// `void` tests `"undefined"`).
    Primitive(&'a str),
    /// `x instanceof Uint8Array`.
    Bytes,
    /// Named struct: `x?.__deka_struct === "<Name>"` (the brand tag is read
    /// directly; no helper, no global — deka#551).
    Struct(&'a str),
    /// Enum: `x.__enum === "<Name>"`.
    Enum(&'a str),
}

/// How a binary or unary operator on newtypes should be lowered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorRewrite<'a> {
    /// Same-newtype arithmetic (+, -): wrap raw primitive result in constructor.
    NewtypeBinary { name: &'a str },
    /// Same-newtype division: returns number (payload / payload).
    NewtypeDiv,
    /// Newtype-op-primitive arithmetic (*, /, %): wrap raw result in constructor.
    NewtypeScalar { name: &'a str, side: NewtypeSide },
    /// Same-newtype comparison (==, !=, <, <=, >, >=): compare payloads.
    NewtypeCompare,
    /// Unary arithmetic on a newtype (-, +): wrap raw result in constructor.
    NewtypeUnary { name: &'a str },
}

impl fmt::Display for Type<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Error => write!(f, "<error>"),
            Type::Infer => write!(f, "<infer>"),
            Type::Var => write!(f, "_"),
            Type::Never => write!(f, "never"),
            Type::None => write!(f, "none"),
            Type::Named { name } => write!(f, "{name}"),
            Type::Option { inner } => write!(f, "Option<{inner}>"),
            Type::Function {
                params,
                ret,
                optional,
            } => {
                write!(f, "fn(")?;
                let required = params.len().saturating_sub(*optional);
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    if i == required && *optional > 0 {
                        write!(f, "optional ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") {ret}")
            }
            Type::Generic { base, args } => {
                write!(f, "{base}<")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                write!(f, ">")
            }
            Type::Struct { name } => write!(f, "{name}"),
            Type::Array { elem } => write!(f, "Array<{elem}>"),
            Type::Object { fields } => {
                write!(f, "{{")?;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                write!(f, "}}")
            }
            Type::Interface { name } => write!(f, "{name}"),
            Type::Newtype { name, .. } => write!(f, "{name}"),
            Type::Param { name } => write!(f, "{name}"),
            Type::Union { members } => {
                // Print members sorted so union types have one canonical
                // spelling regardless of declaration order (rfd#42).
                let mut sorted: Vec<&Type> = members.iter().collect();
                sorted.sort_by_key(|m| m.to_string());
                for (i, member) in sorted.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{member}")?;
                }
                Ok(())
            }
        }
    }
}
