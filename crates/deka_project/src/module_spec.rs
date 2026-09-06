use std::path::{Path, PathBuf};

pub fn is_bare_module_specifier(spec: &str) -> bool {
    !spec.is_empty()
        && !spec.starts_with("./")
        && !spec.starts_with("../")
        && !spec.starts_with('/')
        && !spec.starts_with("http://")
        && !spec.starts_with("https://")
        && !spec.starts_with("file://")
}

/// Bare-specifier prefixes that name stdlib module families. Both the project
/// gate (`is_stdlib_module_spec`) and the browser import map emitted by
/// `deka build` derive their prefix lists from here so the two cannot drift:
/// the gate decides which specifiers are stdlib, the map decides where the
/// browser loads them from.
pub const STDLIB_SPEC_PREFIXES: &[&str] = &["component/", "deka/", "encoding/", "db/"];

pub fn module_spec_aliases(spec: &str) -> Vec<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(2);
    out.push(trimmed.to_string());

    if let Some(rest) = trimmed.strip_prefix("@deka/") {
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
        return out;
    }

    if is_bare_module_specifier(trimmed) && !trimmed.starts_with('@') {
        out.push(format!("@deka/{}", trimmed));
    }

    out
}

pub fn canonical_php_package_spec(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('@') {
        return Some(trimmed.to_string());
    }
    // Package specs are @scope/name. For convenience, only simple unscoped
    // tokens map to @deka/<name>; nested import paths (e.g. component/router)
    // are import-time aliases, not package names.
    if is_bare_module_specifier(trimmed) && !trimmed.contains('/') {
        return Some(format!("@deka/{}", trimmed));
    }
    None
}

/// Whether `name` is a package identity that can safely be used as a module
/// directory or a local-link key.
pub fn is_valid_package_name(name: &str) -> bool {
    if !name.starts_with('@') {
        return false;
    }

    let mut parts = name.split('/');
    let Some(scope) = parts.next() else {
        return false;
    };
    let Some(package) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || scope.len() <= 1 || package.is_empty() {
        return false;
    }

    scope[1..]
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        && package
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Source extensions DekaScript resolution recognizes, in search order.
///
/// `ds_source_candidates`, the missing-import help scanner, module-graph
/// ids, and the integrity walker all derive from this list so they cannot
/// disagree on what a module file is (deka#622 finding G). `.phpx` is not
/// on it: that layer was deleted in #601.
pub const DS_SOURCE_EXTENSIONS: &[&str] = &["ds", "dsx"];

pub fn is_ds_source_extension(ext: &str) -> bool {
    DS_SOURCE_EXTENSIONS.iter().any(|known| *known == ext)
}

pub fn is_ds_source_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(is_ds_source_extension)
}

/// Graph / help-text id for a path relative to `ds_modules`.
///
/// Strips `/index.{ext}` then `.{ext}` for each [`DS_SOURCE_EXTENSIONS`]
/// entry so `core/index.ds` and `core.ds` both become `core`. Leftover
/// `.phpx` is not a source extension and is left as-is.
pub fn ds_module_id_from_rel(rel: &str) -> String {
    let normalized = rel.replace('\\', "/");
    for ext in DS_SOURCE_EXTENSIONS {
        let index_suffix = format!("/index.{ext}");
        if let Some(stripped) = normalized.strip_suffix(&index_suffix) {
            return stripped.to_string();
        }
    }
    for ext in DS_SOURCE_EXTENSIONS {
        let suffix = format!(".{ext}");
        if let Some(stripped) = normalized.strip_suffix(&suffix) {
            return stripped.to_string();
        }
    }
    normalized
}

/// DekaScript source files for an import path (`foo` or `foo.ds`).
///
/// Resolution algorithm (deka#241; keep every resolver on this list):
///
/// 1. Relative (`./foo`, `../bar`) is joined to the importing file's directory.
/// 2. `@/foo` is joined to the project root (parent of `ds_modules`).
/// 3. Bare / `@deka/` specs are joined to `ds_modules` (plus the `@deka/` alias).
/// 4. Then:
///    - `foo.ds` → that file
///    - extensionless `foo` → `foo.ds`, then `foo/index.ds`
///    - any other extension (including `.phpx`) → no DekaScript source
///
/// Check-time (`validate_module_resolution`) and run-time (bundler, ESM
/// loader, `deka build`, WASM project, style-graph walker) must use this
/// list so they cannot disagree. The candidate files are derived from
/// [`DS_SOURCE_EXTENSIONS`].
pub fn ds_source_candidates(base: &Path) -> Vec<PathBuf> {
    match base.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if is_ds_source_extension(ext) => vec![base.to_path_buf()],
        Some(_) => Vec::new(),
        None => {
            let mut out = Vec::with_capacity(DS_SOURCE_EXTENSIONS.len() * 2);
            for ext in DS_SOURCE_EXTENSIONS {
                out.push(base.with_extension(*ext));
            }
            for ext in DS_SOURCE_EXTENSIONS {
                out.push(base.join(format!("index.{ext}")));
            }
            out
        }
    }
}

/// First existing file from [`ds_source_candidates`].
pub fn resolve_ds_source_file(base: &Path) -> Option<PathBuf> {
    ds_source_candidates(base)
        .into_iter()
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_php_package_spec, ds_source_candidates, is_valid_package_name,
        module_spec_aliases, STDLIB_SPEC_PREFIXES,
    };
    use std::path::Path;

    #[test]
    fn bare_spec_includes_deka_alias() {
        assert_eq!(module_spec_aliases("json"), vec!["json", "@deka/json"]);
    }

    #[test]
    fn deka_scope_includes_bare_alias() {
        assert_eq!(
            module_spec_aliases("@deka/json"),
            vec!["@deka/json", "json"]
        );
    }

    #[test]
    fn scoped_non_deka_has_no_alias() {
        assert_eq!(module_spec_aliases("@sami/tool"), vec!["@sami/tool"]);
    }

    #[test]
    fn canonicalizes_bare_packages_to_deka_scope() {
        assert_eq!(
            canonical_php_package_spec("json"),
            Some("@deka/json".to_string())
        );
    }

    #[test]
    fn does_not_map_nested_import_paths_to_package_specs() {
        assert_eq!(canonical_php_package_spec("component/router"), None);
    }

    #[test]
    fn package_names_are_scoped_and_path_safe() {
        assert!(is_valid_package_name("@deka/crypto"));
        assert!(is_valid_package_name("@sami/my-package_2"));
        assert!(!is_valid_package_name("crypto"));
        assert!(!is_valid_package_name("@deka/../escape"));
        assert!(!is_valid_package_name("@deka/crypto/extra"));
    }

    #[test]
    fn ds_candidates_are_file_then_index() {
        let base = Path::new("src/foo");
        // .dsx joined the list in 516ede7af (RFD 24 phase 2) and this
        // assertion was not updated, so runtime_core has been red on main
        // since. Order matters: file before index, .ds before .dsx.
        assert_eq!(
            ds_source_candidates(base),
            vec![
                Path::new("src/foo.ds").to_path_buf(),
                Path::new("src/foo.dsx").to_path_buf(),
                Path::new("src/foo/index.ds").to_path_buf(),
                Path::new("src/foo/index.dsx").to_path_buf(),
            ]
        );
    }

    #[test]
    fn ds_candidates_keep_explicit_ds() {
        let base = Path::new("src/foo.ds");
        assert_eq!(
            ds_source_candidates(base),
            vec![Path::new("src/foo.ds").to_path_buf()]
        );
    }

    #[test]
    fn ds_candidates_reject_phpx() {
        assert!(ds_source_candidates(Path::new("src/foo.phpx")).is_empty());
        assert!(ds_source_candidates(Path::new("src/foo.php")).is_empty());
    }

    #[test]
    fn stdlib_prefixes_carry_deka_alias() {
        // `deka build`'s browser import map resolves each prefix through the
        // @deka alias (the layout `deka install` writes); a prefix without an
        // alias would panic the map emitter, so pin the invariant here.
        for prefix in STDLIB_SPEC_PREFIXES {
            let aliases = module_spec_aliases(prefix);
            assert!(
                aliases.iter().any(|alias| alias.starts_with("@deka/")),
                "{prefix} must derive a @deka alias: {aliases:?}"
            );
        }
    }
}
