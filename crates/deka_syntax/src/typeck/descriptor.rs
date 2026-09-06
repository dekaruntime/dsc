//! Static descriptor trees for `super` (deka#529, rfd#41). Kept for super
//! declarations (PR B): the `super fn` feature that produced these trees was
//! removed (deka#561), but the tree machinery is reused by PR B's
//! `super struct` / `User.type()` work.
//!
//! A `super fn validate<T>` call site names a concrete `T`, and the compiler
//! walks that type into a [`DescriptorTree`] — the static counterpart of the
//! runtime `__deka_type_of` descriptor. The tree is computed here in the
//! typechecker (the emitter never sees types; the lowering maps are the only
//! typeck→emitter channel, same pattern as `method_calls`/`type_of_calls`)
//! and printed by the emitter as frozen, content-deduped module-local
//! `__deka_super_desc$N` consts.
//!
//! Shape rule (same one #550 applied to `{kind, name, toString}`): every node
//! keeps `{kind, name, toString()}` so static and runtime descriptors stay
//! interchangeable, and composite nodes expose `fields`/`cases`/`elem`/
//! `inner`/`members`/`repr` so the schema endgame is not precluded.

use std::collections::HashSet;

use crate::ast;

use super::types::Type;
use super::Checker;

/// A fully-resolved static type descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DescriptorTree<'a> {
    /// Scalar / marker node: `{kind, name}` with `kind` drawn from the same
    /// vocabulary `__deka_type_of` uses (`number`, `string`, `struct`, …).
    Leaf { kind: &'a str, name: String },
    /// A reference back to a `super` declaration's own interned descriptor
    /// const (`__deka_super_desc$<Name>`), produced when a declaration's
    /// tree revisits a type already on the walk stack — i.e. a recursive
    /// type. Only the super-declaration path (which allows recursion) can
    /// produce these; `.signature()` keeps erroring on recursive types.
    Recurse { name: &'a str },
    Struct {
        name: &'a str,
        fields: Vec<DescriptorField<'a>>,
    },
    Interface {
        name: &'a str,
    },
    Newtype {
        name: &'a str,
        repr: Box<DescriptorTree<'a>>,
    },
    Enum {
        name: &'a str,
        cases: Vec<(&'a str, Option<DescriptorTree<'a>>)>,
    },
    Array {
        elem: Box<DescriptorTree<'a>>,
    },
    Option {
        inner: Box<DescriptorTree<'a>>,
    },
    Union {
        members: Vec<DescriptorTree<'a>>,
    },
}

/// One struct field in a descriptor tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorField<'a> {
    pub name: &'a str,
    pub optional: bool,
    pub ty: DescriptorTree<'a>,
}

/// Collect the names of every `Recurse` node in a tree (typechecker-side
/// mirror of the emitter's group walk; the two must agree on the group).
pub fn collect_recurse_refs<'a>(tree: &DescriptorTree<'a>, out: &mut Vec<&'a str>) {
    match tree {
        DescriptorTree::Recurse { name } => out.push(name),
        DescriptorTree::Struct { fields, .. } => {
            for field in fields.iter() {
                collect_recurse_refs(&field.ty, out);
            }
            }
        DescriptorTree::Enum { cases, .. } => {
            for (_, payload) in cases.iter() {
                if let Some(payload) = payload {
                    collect_recurse_refs(payload, out);
                }
            }
        }
        DescriptorTree::Newtype { repr, .. } => collect_recurse_refs(repr, out),
        DescriptorTree::Array { elem } | DescriptorTree::Option { inner: elem } => {
            collect_recurse_refs(elem, out)
        }
        DescriptorTree::Union { members } => {
            for member in members.iter() {
                collect_recurse_refs(member, out);
            }
        }
        DescriptorTree::Leaf { .. } | DescriptorTree::Interface { .. } => {}
    }
}

/// A compile-time JSON method call and the static shape it specializes.
#[derive(Clone, Debug)]
pub struct JsonCall<'a> {
    pub operation: JsonOperation,
    pub shape: DescriptorTree<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonOperation {
    ToJson,
    ParseJson,
}

/// A recorded `.type()` call site inside a `super` function. `param` is set
/// for `T.type()` (the emitter resolves it against the instantiation's
/// argument); `tree` is set for a concrete receiver (a static descriptor
/// tree, emitted as a `__deka_super_desc$N` const).
#[derive(Clone, Debug)]
pub struct StaticTypeCall<'a> {
    pub tree: Option<DescriptorTree<'a>>,
    pub param: Option<&'a str>,
}

// Kept for super declarations (PR B): with `super fn` removed (deka#561)
// nothing in the checker calls into these tree builders yet, but PR B's
// `super struct` / `User.type()` work re-populates them.
#[allow(dead_code)]
impl<'a> Checker<'a> {
    /// Walk a concrete type into its descriptor tree. Returns the "cannot
    /// describe" message on failure; the caller (a super call site) turns it
    /// into an error at that call site, which is the spec's rule: a type the
    /// compiler cannot describe — one holding a function value or derived
    /// from `unsafe` — is an error at the specific call site, not a viral
    /// constraint. Recursive types are an error here (`.signature()` inlines
    /// its tree; a recursive tree is infinite) — the super-declaration path
    /// passes `allow_recurse: true` and gets `Recurse` reference nodes
    /// instead, which the emitter resolves to interned const references.
    pub(super) fn descriptor_tree(
        &mut self,
        ty: &Type<'a>,
        span: ast::Span,
    ) -> Result<DescriptorTree<'a>, String> {
        self.descriptor_tree_mode(ty, span, false)
    }

    /// Super-declaration variant of [`Checker::descriptor_tree`]: recursion
    /// through a `super`-marked struct or enum yields `Recurse` reference
    /// nodes instead of an error.
    pub(super) fn descriptor_tree_super(
        &mut self,
        ty: &Type<'a>,
        span: ast::Span,
    ) -> Result<DescriptorTree<'a>, String> {
        self.descriptor_tree_mode(ty, span, true)
    }

    fn descriptor_tree_mode(
        &mut self,
        ty: &Type<'a>,
        span: ast::Span,
        allow_recurse: bool,
    ) -> Result<DescriptorTree<'a>, String> {
        let mut seen = HashSet::new();
        self.descriptor_tree_rec(ty, span, &mut seen, allow_recurse)
    }

    fn descriptor_tree_rec(
        &mut self,
        ty: &Type<'a>,
        span: ast::Span,
        seen: &mut HashSet<&'a str>,
        allow_recurse: bool,
    ) -> Result<DescriptorTree<'a>, String> {
        match ty {
            Type::Named { name } => {
                if self.enums.contains_key(name) {
                    return self.enum_tree(name, &[], span, seen, allow_recurse);
                }
                let kind = match *name {
                    "number" | "string" | "boolean" | "bytes" | "void" | "never" => *name,
                    "Type" => "type",
                    other => {
                        // A named type we cannot structurally walk (an
                        // opaque import, an unresolved alias): keep it as an
                        // opaque leaf rather than failing — the name is the
                        // useful information at the call site.
                        return Ok(DescriptorTree::Leaf {
                            kind: "unknown",
                            name: other.to_string(),
                        });
                    }
                };
                Ok(DescriptorTree::Leaf {
                    kind,
                    name: kind.to_string(),
                })
            }
            Type::Struct { name } => self.struct_tree(name, &[], span, seen, allow_recurse),
            Type::Generic { base, args } => {
                if self.structs.contains_key(base) {
                    self.struct_tree(base, args, span, seen, allow_recurse)
                } else if self.enums.contains_key(base) {
                    self.enum_tree(base, args, span, seen, allow_recurse)
                } else if *base == "Result" && args.len() == 2 {
                    // The prelude Result is a real enum at runtime; describe
                    // it structurally so `validate<Result<T, E>>` stays
                    // describable.
                    let ok = self.descriptor_tree_rec(&args[0], span, seen, allow_recurse)?;
                    let err = self.descriptor_tree_rec(&args[1], span, seen, allow_recurse)?;
                    Ok(DescriptorTree::Enum {
                        name: "Result",
                        cases: vec![("Ok", Some(ok)), ("Err", Some(err))],
                    })
                } else {
                    Err(format!(
                        "cannot describe type `{ty}` at this `super` call site"
                    ))
                }
            }
            Type::Newtype { name, repr } => {
                let repr_tree = DescriptorTree::Leaf {
                    kind: match repr {
                        ast::NewtypeRepr::Number => "number",
                        ast::NewtypeRepr::String => "string",
                        ast::NewtypeRepr::Bool => "boolean",
                    },
                    name: match repr {
                        ast::NewtypeRepr::Number => "number",
                        ast::NewtypeRepr::String => "string",
                        ast::NewtypeRepr::Bool => "boolean",
                    }
                    .to_string(),
                };
                Ok(DescriptorTree::Newtype {
                    name,
                    repr: Box::new(repr_tree),
                })
            }
            Type::Option { inner } => Ok(DescriptorTree::Option {
                inner: Box::new(self.descriptor_tree_rec(inner, span, seen, allow_recurse)?),
            }),
            Type::Array { elem } => Ok(DescriptorTree::Array {
                elem: Box::new(self.descriptor_tree_rec(elem, span, seen, allow_recurse)?),
            }),
            Type::Union { members } => {
                let mut trees = Vec::with_capacity(members.len());
                for member in members {
                    trees.push(self.descriptor_tree_rec(member, span, seen, allow_recurse)?);
                }
                Ok(DescriptorTree::Union { members: trees })
            }
            Type::None => Ok(DescriptorTree::Leaf {
                kind: "none",
                name: "none".to_string(),
            }),
            Type::Param { name } => Err(format!(
                "cannot describe type parameter `{name}` at this `super` call site; \
                 it must be instantiated with a concrete type"
            )),
            Type::Function { .. } => Err(format!(
                "cannot describe type `{ty}` (function-valued) at this `super` call site"
            )),
            Type::Infer | Type::Var => Err(
                "cannot describe this type at this `super` call site: it is derived from \
                 `unsafe` or otherwise unknown to the compiler"
                    .to_string(),
            ),
            Type::Interface { name } => Ok(DescriptorTree::Interface { name }),
            Type::Object { .. } => Err(format!(
                "cannot describe type `{ty}` at this `super` call site"
            )),
            Type::Error => Err("cannot describe this type at this `super` call site".to_string()),
            Type::Never => Ok(DescriptorTree::Leaf {
                kind: "never",
                name: "never".to_string(),
            }),
        }
    }

    fn struct_tree(
        &mut self,
        name: &'a str,
        args: &[Type<'a>],
        span: ast::Span,
        seen: &mut HashSet<&'a str>,
        allow_recurse: bool,
    ) -> Result<DescriptorTree<'a>, String> {
        if !seen.insert(name) {
            if allow_recurse {
                // Recursive super declaration: reference the interned const
                // instead of expanding the cycle.
                return Ok(DescriptorTree::Recurse { name });
            }
            return Err(format!(
                "cannot describe type `{name}` at this `super` call site: it is recursive"
            ));
        }
        let info = match self.structs.get(name) {
            Some(info) => info.clone(),
            None => {
                seen.remove(name);
                return Err(format!(
                    "cannot describe type `{name}` at this `super` call site: its shape is not visible here"
                ));
            }
        };
        let subst = self.generic_subst(&info.type_params, args);
        let mut fields = Vec::new();
        let result: Result<(), String> = (|| {
            for field in info.fields.iter() {
                let resolved = self.resolve_in_declaring_module(&info.type_params, &field.ty);
                let resolved = match subst {
                    Some(ref subst) => super::types::substitute_type(&resolved, subst),
                    None => resolved,
                };
                let tree = self
                    .descriptor_tree_rec(&resolved, span, seen, allow_recurse)
                    .map_err(|message| format!("field `{}`: {}", field.name, message))?;
                fields.push(DescriptorField {
                    name: field.name,
                    optional: field.optional,
                    ty: tree,
                });
            }
            // Embeds are fields for descriptor purposes: named, required, and
            // typed by the embedded struct.
            for embed in info.embeds.iter() {
                let tree = self.struct_tree(embed.name, &[], span, seen, allow_recurse)?;
                fields.push(DescriptorField {
                    name: embed.name,
                    optional: false,
                    ty: tree,
                });
            }
            Ok(())
        })();
        seen.remove(name);
        result?;
        Ok(DescriptorTree::Struct { name, fields })
    }

    fn enum_tree(
        &mut self,
        name: &'a str,
        args: &[Type<'a>],
        span: ast::Span,
        seen: &mut HashSet<&'a str>,
        allow_recurse: bool,
    ) -> Result<DescriptorTree<'a>, String> {
        if !seen.insert(name) {
            if allow_recurse {
                return Ok(DescriptorTree::Recurse { name });
            }
            return Err(format!(
                "cannot describe type `{name}` at this `super` call site: it is recursive"
            ));
        }
        let info = match self.enums.get(name) {
            Some(info) => info.clone(),
            None => {
                seen.remove(name);
                return Err(format!(
                    "cannot describe type `{name}` at this `super` call site: its shape is not visible here"
                ));
            }
        };
        let subst = self.generic_subst(&info.type_params, args);
        let mut cases = Vec::new();
        let mut result: Result<(), String> = Ok(());
        for case in info.cases.iter() {
            let tree = match &case.payload {
                Some(payload) => {
                    let resolved = self.resolve_in_declaring_module(&info.type_params, payload);
                    let resolved = match subst {
                        Some(ref subst) => super::types::substitute_type(&resolved, subst),
                        None => resolved,
                    };
                    match self.descriptor_tree_rec(&resolved, span, seen, allow_recurse) {
                        Ok(tree) => Some(tree),
                        Err(message) => {
                            result = Err(message);
                            break;
                        }
                    }
                }
                None => None,
            };
            cases.push((case.name, tree));
        }
        seen.remove(name);
        result?;
        Ok(DescriptorTree::Enum { name, cases })
    }

    /// Zip a generic declaration's type parameters against use-site
    /// arguments. `None` when the declaration is not generic.
    fn generic_subst(
        &mut self,
        type_params: &[ast::TypeParam<'a>],
        args: &[Type<'a>],
    ) -> Option<std::collections::HashMap<&'a str, Type<'a>>> {
        if type_params.is_empty() {
            return None;
        }
        Some(
            type_params
                .iter()
                .map(|p| p.name)
                .zip(args.iter().cloned())
                .collect(),
        )
    }

    /// Build the descriptor tree for a `super struct` declaration, with
    /// per-field error context: the Err names the field the walk failed on,
    /// which is what makes an undescribable `super` declaration actionable
    /// (deka#561 PR B). Recursive references produce `Recurse` nodes.
    pub(super) fn super_struct_tree(
        &mut self,
        name: &'a str,
        span: ast::Span,
    ) -> Result<DescriptorTree<'a>, String> {
        let info = match self.structs.get(name) {
            Some(info) => info.clone(),
            None => {
                return Err(format!(
                    "cannot describe type `{name}`: its shape is not visible here"
                ))
            }
        };
        let mut seen = HashSet::new();
        seen.insert(name);
        let mut fields = Vec::new();
        for field in info.fields.iter() {
            let resolved = self.resolve_in_declaring_module(&info.type_params, &field.ty);
            let tree = self
                .descriptor_tree_rec(&resolved, span, &mut seen, true)
                .map_err(|message| format!("field `{}`: {message}", field.name))?;
            fields.push(DescriptorField {
                name: field.name,
                optional: field.optional,
                ty: tree,
            });
        }
        // Embeds are fields for descriptor purposes: named, required, and
        // typed by the embedded struct (same rule struct_tree applies).
        for embed in info.embeds.iter() {
            let tree = self
                .struct_tree(embed.name, &[], span, &mut seen, true)
                .map_err(|message| format!("embedded struct `{}`: {message}", embed.name))?;
            fields.push(DescriptorField {
                name: embed.name,
                optional: false,
                ty: tree,
            });
        }
        Ok(DescriptorTree::Struct { name, fields })
    }

    /// Build the descriptor tree for a `super enum` declaration, with
    /// per-case error context. Recursive references produce `Recurse` nodes.
    pub(super) fn super_enum_tree(
        &mut self,
        name: &'a str,
        span: ast::Span,
    ) -> Result<DescriptorTree<'a>, String> {
        let info = match self.enums.get(name) {
            Some(info) => info.clone(),
            None => {
                return Err(format!(
                    "cannot describe type `{name}`: its shape is not visible here"
                ))
            }
        };
        let mut seen = HashSet::new();
        seen.insert(name);
        let mut cases = Vec::new();
        for case in info.cases.iter() {
            let tree = match &case.payload {
                Some(payload) => {
                    let resolved = self.resolve_in_declaring_module(&info.type_params, payload);
                    Some(
                        self.descriptor_tree_rec(&resolved, span, &mut seen, true)
                            .map_err(|message| format!("case `{}`: {message}", case.name))?,
                    )
                }
                None => None,
            };
            cases.push((case.name, tree));
        }
        Ok(DescriptorTree::Enum { name, cases })
    }

    /// Collect the struct/enum declaration names a resolved type refers to,
    /// for the super-marking transitive closure. Only names that are actually
    /// declared (locally or seeded from imports) are returned.
    pub(super) fn referenced_decl_names(&self, ty: &Type<'a>, out: &mut Vec<&'a str>) {
        match ty {
            Type::Named { name } => {
                if self.structs.contains_key(name) || self.enums.contains_key(name) {
                    out.push(name);
                }
            }
            Type::Struct { name } => out.push(name),
            Type::Generic { base, args } => {
                if self.structs.contains_key(base) || self.enums.contains_key(base) {
                    out.push(base);
                }
                for arg in args.iter() {
                    self.referenced_decl_names(arg, out);
                }
            }
            Type::Option { inner } | Type::Array { elem: inner } => {
                self.referenced_decl_names(inner, out);
            }
            Type::Union { members } => {
                for member in members.iter() {
                    self.referenced_decl_names(member, out);
                }
            }
            _ => {}
        }
    }

    /// Resolve the field / case-payload types of a declared struct or enum
    /// into `Type` form (used by the transitive-closure walk). Returns None
    /// when the name is not a declared struct or enum.
    pub(super) fn decl_member_types(&mut self, name: &'a str) -> Option<Vec<Type<'a>>> {
        if let Some(info) = self.structs.get(name) {
            let info = info.clone();
            let mut out = Vec::with_capacity(info.fields.len() + info.embeds.len());
            for field in info.fields.iter() {
                out.push(self.resolve_in_declaring_module(&info.type_params, &field.ty));
            }
            for embed in info.embeds.iter() {
                out.push(Type::Struct { name: embed.name });
            }
            return Some(out);
        }
        if let Some(info) = self.enums.get(name) {
            let info = info.clone();
            let mut out = Vec::new();
            for case in info.cases.iter() {
                if let Some(payload) = &case.payload {
                    out.push(self.resolve_in_declaring_module(&info.type_params, payload));
                }
            }
            return Some(out);
        }
        None
    }

    /// Is this name a type whose descriptor survives to runtime — either
    /// declared `super` locally (its tree was built during declaration
    /// collection) or imported from a module that declared it `super`?
    pub(super) fn is_super_decl(&self, name: &str) -> bool {
        if self.super_trees.contains_key(name) {
            return true;
        }
        if let Some(info) = self.structs.get(name) {
            return info.is_super;
        }
        if let Some(info) = self.enums.get(name) {
            return info.is_super;
        }
        false
    }

    /// Resolve any type alias chain down to the declared struct/enum name it
    /// ultimately names, when it does.
    pub(super) fn alias_target_decl(&self, name: &'a str) -> Option<&'a str> {
        let mut current = self.aliases.get(name)?;
        for _ in 0..16 {
            match current {
                ast::Type::Named { name: inner, .. } => {
                    if self.structs.contains_key(inner) || self.enums.contains_key(inner) {
                        return Some(inner);
                    }
                    if let Some(next) = self.aliases.get(inner) {
                        current = next;
                        continue;
                    }
                    return None;
                }
                _ => return None,
            }
        }
        None
    }

    /// Compute the super-declaration set and build every marked declaration's
    /// descriptor tree. Runs at the end of declaration collection so both
    /// the inference pass and the real check pass see `is_super_decl` /
    /// `super_trees` (diagnostics are infer_only-suppressed in the former).
    ///
    /// Marking is transitive: a `super` declaration's descriptor embeds the
    /// descriptors of every struct/enum it references, so those are marked
    /// too — auto-marking, because the alternative (an error forcing the
    /// author to repeat `super` on types they already own) only punishes
    /// composition. The error case is a field whose type cannot be described
    /// at all (function-valued, unsafe-derived, shape not visible); that is
    /// reported at the declaration, naming the field.
    pub(super) fn collect_super_declarations(&mut self) {
        // Spans and kinds for every local struct/enum declaration, plus the
        // explicit super marks.
        let mut spans: std::collections::HashMap<&'a str, ast::Span> =
            std::collections::HashMap::new();
        let mut kinds: std::collections::HashMap<&'a str, &'a str> =
            std::collections::HashMap::new();
        let mut explicit: HashSet<&'a str> = HashSet::new();
        for stmt in self.program.statements {
            match stmt {
                ast::Stmt::Struct {
                    name, is_super, span, ..
                } => {
                    spans.insert(name, *span);
                    kinds.insert(name, "struct");
                    if *is_super {
                        explicit.insert(name);
                    }
                }
                ast::Stmt::Enum {
                    name, is_super, span, ..
                } => {
                    spans.insert(name, *span);
                    kinds.insert(name, "enum");
                    if *is_super {
                        explicit.insert(name);
                    }
                }
                _ => {}
            }
        }

        // Transitive closure over referenced struct/enum declarations.
        // `provenance` records the span of the explicit `super` declaration
        // that (transitively) required each auto-marked type, so a failure in
        // an imported or auto-marked type points at a useful location.
        let mut marked: HashSet<&'a str> = explicit.clone();
        let mut provenance: std::collections::HashMap<&'a str, ast::Span> =
            explicit.iter().map(|n| (*n, spans[n])).collect();
        let mut worklist: Vec<&'a str> = explicit.iter().cloned().collect();
        while let Some(name) = worklist.pop() {
            let Some(members) = self.decl_member_types(name) else {
                continue;
            };
            let mut refs = Vec::new();
            for member in members.iter() {
                self.referenced_decl_names(member, &mut refs);
            }
            for referenced in refs {
                if marked.insert(referenced) {
                    let at = *provenance.get(name).expect("worklist names are marked");
                    provenance.insert(referenced, at);
                    worklist.push(referenced);
                }
            }
        }

        // Build and validate trees; declaration-time validation so a broken
        // `super` declaration errors even if `Name.type()` is never called.
        let mut ordered: Vec<&'a str> = marked.iter().cloned().collect();
        ordered.sort();
        for name in ordered {
            let span = *provenance
                .get(name)
                .expect("every marked name recorded provenance");
            let tree = if self.structs.contains_key(name) {
                self.super_struct_tree(name, span)
            } else {
                self.super_enum_tree(name, span)
            };
            match tree {
                Ok(tree) => {
                    self.super_trees.insert(name, tree);
                }
                Err(message) => {
                    let kind = kinds.get(name).copied().unwrap_or("type");
                    let at = if explicit.contains(name) {
                        format!("`super {kind} {name}`")
                    } else {
                        format!("{kind} `{name}` (required by a `super` declaration)")
                    };
                    self.error_span(
                        span,
                        format!("{at} cannot carry runtime type information: {message}"),
                    );
                }
            }
        }
    }

    /// Resolve a type annotation stored in a *declaration* (struct field,
    /// enum case payload) that may come from another module. The declaring
    /// context's type parameters are pushed so parameter references resolve;
    /// nested diagnostics are suppressed — the caller reports one
    /// "cannot describe" error at the super call site instead.
    fn resolve_in_declaring_module(
        &mut self,
        type_params: &'a [ast::TypeParam<'a>],
        ty: &ast::Type<'a>,
    ) -> Type<'a> {
        let saved_infer_only = self.infer_only;
        self.infer_only = true;
        self.push_type_params(type_params);
        let resolved = self.resolve_ast_type(ty);
        self.pop_type_params();
        self.infer_only = saved_infer_only;
        resolved
    }
}
