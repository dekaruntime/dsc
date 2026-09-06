//! Single source of truth for the shared runtime prelude, consumed by every
//! construction site (deka#582):
//!
//! - [`module_prelude`] — the enum bindings (`Result`/`Option` constructors)
//!   inlined into compiled modules.
//! - [`pool_prelude`] — the `globalThis`-guarded form injected by the
//!   isolate pool bootstrap (`crates/pool/src/isolate_pool/worker_execution.rs`).
//! - [`to_result_helper`] — the `__deka_to_result` expression (deka#578)
//!   that tags host-bridge envelopes with the same shapes.
//! - [`RESULT_OK`]/[`RESULT_ERR`] (crate-internal) — spliced directly into
//!   the emitter's `unsafe { }` Ok/Err arms (deka#622 finding F) so those
//!   values carry the exact branded shape `Result.Ok`/`Result.Err` produce.
//! - [`PreludeDemand`] / [`shared_prelude`] — the demand-driven synthesis
//!   (deka#595): each module records which helpers and which *members* of
//!   those helpers it needs, the module graph unions the sets, and the
//!   program prelude is built once from that union. [`struct_helper`] and
//!   [`type_of_helper`] are the ONE definitions of those helpers; both the
//!   module-local inline prelude and the once-per-program bundle prelude are
//!   assembled here so no second spelling can drift (deka#622).
//!
//! Freezing rules follow rfd#13 principles 1 and 8: the namespace objects
//! (`Result`, `Option`) are shared and long-lived, so they stay frozen;
//! ephemeral `Ok(v)`/`Some(v)` values carry data, not guarantees, and are
//! never frozen.

/// Shared `Result` constructors (deka#582). `pub(crate)` because
/// `emit_unsafe` splices these expressions into its Ok/Err arms (deka#622
/// finding F); the values it produces must be the same branded shape
/// `Result.Ok`/`Result.Err` produce, never a second transcription.
pub(crate) const RESULT_OK: &str = r#"(value) => ({ __enum: "Result", __case: "Ok", name: "Ok", value })"#;
pub(crate) const RESULT_ERR: &str = r#"(error) => ({ __enum: "Result", __case: "Err", name: "Err", error })"#;
const OPTION_SOME: &str = r#"(value) => ({ __enum: "Option", __case: "Some", name: "Some", value })"#;
const OPTION_NONE: &str = r#"({ __enum: "Option", __case: "None", name: "None" })"#;

/// Module-local prelude emitted into compiled modules: frozen namespace
/// consts plus the bare `Ok`/`Err`/`Some`/`None` aliases the emitter
/// rewrites references to.
pub fn module_prelude() -> String {
    format!(
        "const Result = Object.freeze({{\n\
        \x20 Ok: {RESULT_OK},\n\
        \x20 Err: {RESULT_ERR}\n\
        }});\n\
        const Option = Object.freeze({{\n\
        \x20 Some: {OPTION_SOME},\n\
        \x20 None: {OPTION_NONE}\n\
        }});\n\
        const Ok = Result.Ok;\n\
        const Err = Result.Err;\n\
        const Some = Option.Some;\n\
        const None = Option.None;\n"
    )
}

/// Pool bootstrap prelude: the same constructors, installed on `globalThis`
/// behind `typeof` guards so user code and repeated bootstraps cannot
/// clobber them.
pub fn pool_prelude() -> String {
    format!(
        "if (typeof globalThis.Option === 'undefined') {{\n\
        \x20   globalThis.Option = Object.freeze({{\n\
        \x20       Some: {OPTION_SOME},\n\
        \x20       None: {OPTION_NONE}\n\
        \x20   }});\n\
        }}\n\
        if (typeof globalThis.Result === 'undefined') {{\n\
        \x20   globalThis.Result = Object.freeze({{\n\
        \x20       Ok: {RESULT_OK},\n\
        \x20       Err: {RESULT_ERR}\n\
        \x20   }});\n\
        }}\n"
    )
}

/// `__deka_to_result` (deka#578): normalize a `{ ok, value | error }`
/// bridge envelope into a tagged `Result`. Built from the same constructor
/// expressions as the prelude, so the envelope shape can never drift from
/// what `Result.Ok`/`Result.Err` produce.
pub fn to_result_helper() -> String {
    format!(
        "(r) => (r && r.ok)\n\
        \x20   ? ({RESULT_OK})(r.value)\n\
        \x20   : ({RESULT_ERR})((r && r.error) ? r.error : \"host bridge failed\")"
    )
}

// ---------------------------------------------------------------------------
// Whole-graph prelude synthesis (deka#595)
// ---------------------------------------------------------------------------

/// Which *members* of the `__deka_struct` factory some module actually uses.
/// Each part of the helper is emitted iff some module in the program asked
/// for it — a program with no impl blocks gets a factory with no `impl`, no
/// `implMut`, and no `MutationError` class.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StructDemand {
    /// Any `.impl(` registration (immutable receiver methods).
    pub impl_methods: bool,
    /// Any `.implMut(` registration (mutable receiver methods). Also gates
    /// the `__deka_MutationError` class, which only `implMut` bodies throw.
    pub impl_mut: bool,
    /// Any struct factory constructed with an embeds map.
    pub embeds: bool,
}

impl StructDemand {
    /// True when no member is demanded — the helper is still emitted (some
    /// module constructs structs) but carries none of the optional parts.
    pub fn is_empty(&self) -> bool {
        !self.impl_methods && !self.impl_mut && !self.embeds
    }
}

/// The shared-runtime-helper demand of ONE module. The module graph unions
/// these into the program-level demand set and synthesizes the prelude once
/// per program (deka#595) instead of once per module.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreludeDemand {
    /// `Some(_)` iff the module needs the `__deka_struct` factory helper.
    pub structs: Option<StructDemand>,
    /// `const __p = Symbol.for('deka.nt')` — the module declares or imports
    /// newtypes whose emitted code reads the shared payload symbol.
    pub newtype: bool,
    /// `__deka_type_of` plus its descriptor cache — the module has call
    /// sites. Its brand-tag branches are gated by the program-level
    /// `declares_*` bits below, not by this flag.
    pub type_of: bool,
    /// The enum prelude bindings (`Result`/`Option`/`Ok`/`Err`/`Some`/`None`).
    pub enums: bool,
    /// The module declares structs. A brand tag can only exist if some
    /// module in the program declares that kind — values cross module
    /// boundaries without their type binding being imported (function
    /// returns), so import-based demand would miss them.
    pub declares_structs: bool,
    /// The module declares enum objects.
    pub declares_enums: bool,
    /// The module declares newtypes.
    pub declares_newtypes: bool,
}

impl PreludeDemand {
    /// Union another module's demand into this one. This is the whole-graph
    /// demand set the program prelude is synthesized from.
    pub fn union(&mut self, other: &PreludeDemand) {
        match (&mut self.structs, &other.structs) {
            (Some(a), Some(b)) => {
                a.impl_methods |= b.impl_methods;
                a.impl_mut |= b.impl_mut;
                a.embeds |= b.embeds;
            }
            (None, Some(_)) => self.structs = other.structs,
            _ => {}
        }
        self.newtype |= other.newtype;
        self.type_of |= other.type_of;
        self.enums |= other.enums;
        self.declares_structs |= other.declares_structs;
        self.declares_enums |= other.declares_enums;
        self.declares_newtypes |= other.declares_newtypes;
    }

    /// True when no shared helper is needed at all — the program prelude is
    /// empty and nothing is emitted (the property `__deka_type_of` already
    /// had, generalized to every helper).
    pub fn is_empty(&self) -> bool {
        self.structs.is_none() && !self.newtype && !self.type_of && !self.enums
    }
}

/// Fragment shared by every spelling of the `__deka_struct` helper: the
/// factory closure, brand tag, and prototype wiring. The optional members
/// (`impl` / `implMut` / embeds) are spliced between this and [`STRUCT_TAIL`].
const STRUCT_HEAD: &str = "function __deka_struct(id,embeds){function f(fields){return Object.assign({__proto__:f.prototype},fields);}f.id=id;Object.defineProperty(f,'name',{value:id,configurable:true});f.prototype=Object.create(null);Object.defineProperty(f.prototype,'__deka_struct',{value:id,enumerable:false,writable:false,configurable:false});f.prototype.constructor=f;";
const STRUCT_IMPL: &str = "f.impl=(a,b)=>{if(typeof a==='string'){const k=a;f.prototype[k]=function(...x){return b.apply(this,x);};}else{for(const k in a)f.prototype[k]=a[k];}return f;};";
const STRUCT_IMPL_MUT: &str = "f.implMut=(a,b)=>{if(typeof a==='string'){const k=a;f.prototype[k]=function(...x){if(Object.isFrozen(this))throw new __deka_MutationError(`cannot call mutable method '${k}' on immutable ${id}`);return b.apply(this,x);};}else{for(const k in a){const fn=a[k];f.prototype[k]=function(...x){if(Object.isFrozen(this))throw new __deka_MutationError(`cannot call mutable method '${k}' on immutable ${id}`);return fn.apply(this,x);};}}return f;};";
const STRUCT_EMBEDS: &str = "if(embeds){for(const [embedName,embedFactory] of Object.entries(embeds)){for(const key of Object.keys(embedFactory.prototype)){f.prototype[key]=function(...args){return this[embedName][key](...args);};}}}";
const STRUCT_TAIL: &str = "return f;}";

/// The ONE definition of the `__deka_struct` factory helper (deka#582's
/// single-spelling rule applies to bodies too — deka#622). The shape is
/// determined by demand: each of `impl` / `implMut` / the embeds loop is
/// present iff [`StructDemand`] says some module uses it. With every member
/// demanded this is byte-identical to the helper the emitter always emitted.
pub fn struct_helper(demand: &StructDemand) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(STRUCT_HEAD);
    if demand.impl_methods {
        out.push_str(STRUCT_IMPL);
    }
    if demand.impl_mut {
        out.push_str(STRUCT_IMPL_MUT);
    }
    if demand.embeds {
        out.push_str(STRUCT_EMBEDS);
    }
    out.push_str(STRUCT_TAIL);
    out
}

/// The `__deka_MutationError` class thrown by `implMut` method bodies.
/// Emitted iff some module registers a mutable receiver method.
pub const MUTATION_ERROR: &str = "class __deka_MutationError extends Error{constructor(m){super(m);this.name='MutationError';}}";

/// The shared newtype payload symbol (`const __p = Symbol.for('deka.nt')`).
/// Emitted iff some module's newtype code reads `__p`.
pub const NEWTYPE_SYMBOL: &str = "const __p = Symbol.for('deka.nt');";

/// The descriptor cache backing `__deka_type_of`. Emitted with the helper.
pub const TYPE_OF_CACHE: &str = "const __deka_type_cache = new Map();";

const TYPE_OF_HEAD: &str = "function __deka_type_of(v){const mk=(k,n)=>{const key=k+\":\"+n;let t=__deka_type_cache.get(key);if(!t){t=Object.freeze({kind:k,name:n,toString(){return this.name;}});__deka_type_cache.set(key,t);}return t;};if(v===null||v===undefined)return mk(\"none\",\"none\");if(v instanceof Uint8Array)return mk(\"bytes\",\"bytes\");const ty=typeof v;if(ty===\"string\"||ty===\"number\"||ty===\"boolean\"||ty===\"function\")return mk(ty,ty);if(Array.isArray(v))return mk(\"array\",\"Array\");";
const TYPE_OF_NEWTYPES: &str = "const nt=v.__deka_newtype;if(nt)return mk(\"newtype\",nt);";
const TYPE_OF_STRUCTS: &str = "const st=v.__deka_struct;if(st)return mk(\"struct\",st);";
const TYPE_OF_ENUMS: &str = "const en=v.__enum;if(en)return mk(\"enum\",en);";
const TYPE_OF_TAIL: &str = "return mk(\"object\",\"object\");}";

/// Which brand-tag branches of `__deka_type_of` to emit. At program level
/// this mirrors `PreludeDemand::declares_*`: a branch is present iff some
/// module in the program declares values of that kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TypeOfDemand {
    pub newtypes: bool,
    pub structs: bool,
    pub enums: bool,
}

/// The ONE definition of the `__deka_type_of` builtin helper. Brand-tag
/// branches are present iff the program can produce values of that kind —
/// an unreachable branch is dead weight (deka#595). With every kind
/// demanded this is byte-identical to the helper the emitter always emitted.
pub fn type_of_helper(demand: &TypeOfDemand) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(TYPE_OF_HEAD);
    if demand.newtypes {
        out.push_str(TYPE_OF_NEWTYPES);
    }
    if demand.structs {
        out.push_str(TYPE_OF_STRUCTS);
    }
    if demand.enums {
        out.push_str(TYPE_OF_ENUMS);
    }
    out.push_str(TYPE_OF_TAIL);
    out
}

/// The shared runtime prelude for a demand set: every helper some module
/// asked for, each carrying only the members demanded, in a stable order.
/// This is the once-per-program prelude the module graph prepends to a
/// bundle (deka#595); a single module compiles inline it the same way, so
/// there is exactly one construction site for the whole prelude shape.
pub fn shared_prelude(demand: &PreludeDemand) -> String {
    let mut out = String::with_capacity(1024);
    if let Some(structs) = &demand.structs {
        out.push_str(&struct_helper(structs));
        out.push('\n');
        if structs.impl_mut {
            out.push_str(MUTATION_ERROR);
            out.push('\n');
        }
    }
    if demand.newtype {
        out.push_str(NEWTYPE_SYMBOL);
        out.push('\n');
    }
    if demand.type_of {
        out.push_str(TYPE_OF_CACHE);
        out.push('\n');
        out.push_str(&type_of_helper(&TypeOfDemand {
            newtypes: demand.declares_newtypes,
            structs: demand.declares_structs,
            enums: demand.declares_enums,
        }));
        out.push('\n');
    }
    if demand.enums {
        out.push_str(&module_prelude());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_prelude_matches_emitter_contract() {
        // Byte-exact contract asserted by the emitter's existing tests and
        // snapshots; drift here means drift in compiled output.
        assert_eq!(
            module_prelude(),
            concat!(
                "const Result = Object.freeze({\n",
                "  Ok: (value) => ({ __enum: \"Result\", __case: \"Ok\", name: \"Ok\", value }),\n",
                "  Err: (error) => ({ __enum: \"Result\", __case: \"Err\", name: \"Err\", error })\n",
                "});\n",
                "const Option = Object.freeze({\n",
                "  Some: (value) => ({ __enum: \"Option\", __case: \"Some\", name: \"Some\", value }),\n",
                "  None: ({ __enum: \"Option\", __case: \"None\", name: \"None\" })\n",
                "});\n",
                "const Ok = Result.Ok;\n",
                "const Err = Result.Err;\n",
                "const Some = Option.Some;\n",
                "const None = Option.None;\n",
            )
        );
    }

    #[test]
    fn all_sites_share_constructor_shapes() {
        let module = module_prelude();
        let pool = pool_prelude();
        let to_result = to_result_helper();
        for ctor in [RESULT_OK, RESULT_ERR, OPTION_SOME, OPTION_NONE] {
            assert!(module.contains(ctor), "module prelude missing {ctor}");
            // `None` is a module-only alias target; the pool installs the
            // namespaced table that the same constructor defines.
            assert!(pool.contains(ctor), "pool prelude missing {ctor}");
        }
        // The bridge helper calls the Result constructors rather than
        // re-transcribing the tagged literals: the only `__enum` mentions
        // are the ones inside the shared constructor expressions.
        assert!(to_result.contains(RESULT_OK), "to_result missing Ok ctor");
        assert!(to_result.contains(RESULT_ERR), "to_result missing Err ctor");
        assert_eq!(
            to_result.matches("__enum").count(),
            RESULT_OK.matches("__enum").count() + RESULT_ERR.matches("__enum").count(),
            "to_result must reuse constructors, not re-transcribe literals: {to_result}"
        );
    }

    #[test]
    fn ephemeral_values_are_never_frozen() {
        for site in [module_prelude(), pool_prelude(), to_result_helper()] {
            assert!(
                !site.contains("Object.freeze({ __enum:"),
                "ephemeral enum values must not be frozen (rfd#13): {site}"
            );
        }
        // The long-lived namespace tables stay frozen.
        assert!(module_prelude().contains("const Result = Object.freeze({"));
        assert!(pool_prelude().contains("globalThis.Result = Object.freeze({"));
    }
}

#[cfg(test)]
mod demand_tests {
    use super::*;

    #[test]
    fn struct_helper_gates_each_member_independently() {
        let full = StructDemand {
            impl_methods: true,
            impl_mut: true,
            embeds: true,
        };
        let all = struct_helper(&full);
        // Full demand carries each member exactly once.
        assert_eq!(all.matches("f.impl=").count(), 1, "{all}");
        assert_eq!(all.matches("f.implMut=").count(), 1, "{all}");
        assert_eq!(all.matches("Object.entries(embeds)").count(), 1, "{all}");
        assert!(all.starts_with("function __deka_struct(id,embeds){"), "{all}");
        assert!(all.ends_with("return f;}"), "{all}");

        // No demand: the bare factory, none of the optional machinery.
        let bare = struct_helper(&StructDemand::default());
        assert!(bare.starts_with("function __deka_struct(id,embeds){"), "{bare}");
        assert!(bare.ends_with("return f;}"), "{bare}");
        assert!(!bare.contains("f.impl"), "{bare}");
        assert!(!bare.contains("implMut"), "{bare}");
        assert!(!bare.contains("Object.entries(embeds)"), "{bare}");

        // Each member is independent of the others.
        let only_mut = struct_helper(&StructDemand {
            impl_methods: false,
            impl_mut: true,
            embeds: false,
        });
        assert!(!only_mut.contains("f.impl="), "{only_mut}");
        assert!(only_mut.contains("f.implMut="), "{only_mut}");
        let only_embeds = struct_helper(&StructDemand {
            impl_methods: false,
            impl_mut: false,
            embeds: true,
        });
        assert!(only_embeds.contains("Object.entries(embeds)"), "{only_embeds}");
        assert!(!only_embeds.contains("f.impl"), "{only_embeds}");
    }

    #[test]
    fn type_of_helper_gates_unreachable_branches() {
        let full = type_of_helper(&TypeOfDemand {
            newtypes: true,
            structs: true,
            enums: true,
        });
        for branch in [
            "v.__deka_newtype",
            "v.__deka_struct",
            "v.__enum",
            "return mk(\"object\",\"object\");",
        ] {
            assert!(full.contains(branch), "full helper missing {branch}: {full}");
        }
        let primitives_only = type_of_helper(&TypeOfDemand::default());
        assert!(
            primitives_only.contains("return mk(\"object\",\"object\");"),
            "{primitives_only}"
        );
        for branch in ["v.__deka_newtype", "v.__deka_struct", "v.__enum"] {
            assert!(
                !primitives_only.contains(branch),
                "unreachable branch {branch} emitted: {primitives_only}"
            );
        }
    }

    #[test]
    fn shared_prelude_emits_only_demanded_helpers() {
        // Empty demand: nothing at all (the __deka_type_of property,
        // generalized to every helper).
        assert_eq!(shared_prelude(&PreludeDemand::default()), "");

        // Enums only: byte-identical to the module prelude.
        let enums_only = shared_prelude(&PreludeDemand {
            enums: true,
            ..PreludeDemand::default()
        });
        assert_eq!(enums_only, module_prelude());

        // Structs with a mutable method: helper + MutationError, no
        // impl/implMut-free extras, no enum bindings.
        let with_mut = shared_prelude(&PreludeDemand {
            structs: Some(StructDemand {
                impl_methods: false,
                impl_mut: true,
                embeds: false,
            }),
            ..PreludeDemand::default()
        });
        assert_eq!(with_mut.matches("function __deka_struct").count(), 1, "{with_mut}");
        assert!(with_mut.contains(MUTATION_ERROR), "{with_mut}");
        assert!(!with_mut.contains("const Result = Object.freeze"), "{with_mut}");
        assert!(!with_mut.contains("__deka_type_of"), "{with_mut}");
    }

    #[test]
    fn demand_union_is_member_granular() {
        let mut a = PreludeDemand {
            structs: Some(StructDemand {
                impl_methods: true,
                impl_mut: false,
                embeds: false,
            }),
            type_of: true,
            declares_structs: true,
            ..PreludeDemand::default()
        };
        let b = PreludeDemand {
            structs: Some(StructDemand {
                impl_methods: false,
                impl_mut: true,
                embeds: true,
            }),
            newtype: true,
            enums: true,
            declares_enums: true,
            declares_newtypes: true,
            ..PreludeDemand::default()
        };
        a.union(&b);
        let structs = a.structs.expect("union keeps struct demand");
        assert!(structs.impl_methods && structs.impl_mut && structs.embeds);
        assert!(a.newtype && a.enums && a.type_of);
        assert!(
            a.declares_structs && a.declares_enums && a.declares_newtypes,
            "declares_* bits must union too"
        );
        // Union with empty demand is the identity.
        let before = a.clone();
        a.union(&PreludeDemand::default());
        assert_eq!(a, before);
    }
}
