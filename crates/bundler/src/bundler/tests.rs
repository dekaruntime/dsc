use super::*;
use deka_project::modules::{write_links_at, LinkEntry, LinkManifest, MODULES_DIR};
use std::ops::Deref;

#[test]
fn test_is_valid_identifier() {
    assert!(is_valid_identifier("foo"));
    assert!(is_valid_identifier("_foo"));
    assert!(is_valid_identifier("$foo"));
    assert!(!is_valid_identifier("1foo"));
    assert!(!is_valid_identifier("foo-bar"));
    assert!(!is_valid_identifier(""));
}

#[test]
fn test_to_camel_case() {
    assert_eq!(to_camel_case("foo-bar"), "fooBar");
    assert_eq!(to_camel_case("foo_bar"), "fooBar");
    assert_eq!(to_camel_case("foo bar"), "fooBar");
    assert_eq!(to_camel_case("Foo"), "Foo");
    assert_eq!(to_camel_case("foo--bar"), "fooBar");
}

#[test]
fn test_append_named_exports() {
    let mut lines = Vec::new();
    append_named_exports(&mut lines, "foo").unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("export const foo"));

    let mut lines = Vec::new();
    append_named_exports(&mut lines, "foo-bar").unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("export const fooBar"));

    let mut lines = Vec::new();
    append_named_exports(&mut lines, "1bad").unwrap();
    assert!(lines.is_empty());
}

pub(super) struct SimpleVirtualSource {
    pub(super) entry: PathBuf,
    pub(super) code: String,
}

impl VirtualSource for SimpleVirtualSource {
    fn load_virtual(&self, path: &Path) -> Result<Option<String>, String> {
        if path == self.entry {
            Ok(Some(self.code.clone()))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug)]
pub(super) struct TestTempDir(tempfile::TempDir);

impl TestTempDir {
    pub(super) fn path_buf(&self) -> PathBuf {
        self.0.path().to_path_buf()
    }
}

impl AsRef<Path> for TestTempDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

impl Deref for TestTempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

pub(super) fn make_tmp_dir(name: &str) -> TestTempDir {
    TestTempDir(
        tempfile::Builder::new()
            .prefix(&format!("deka_bundler_test_{name}_"))
            .tempdir()
            .expect("create unique test dir"),
    )
}

#[test]
fn bundle_produces_valid_js() {
    let tmp = make_tmp_dir("valid_js");
    let entry = tmp.join("entry.js");
    std::fs::write(&entry, "export const x = 42;\n").expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: "export const x = 42;\n".to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: false,
            iife: false,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("bundle should succeed");
    assert!(
        result.contains("42"),
        "expected value in bundle: {}",
        result
    );
}

#[test]
fn bundle_rejects_malformed_module() {
    let tmp = make_tmp_dir("malformed_module");
    let entry = tmp.path_buf().join("entry.js");
    let source = "export const = ;\n";
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });

    let err = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: false,
            iife: false,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect_err("malformed modules must fail the bundle");

    assert!(!err.trim().is_empty(), "bundle returned an empty error");
}

#[test]
fn bundle_allows_parent_relative_ds_import_from_subdirectory() {
    let tmp = make_tmp_dir("parent_relative_ds_bundle");
    let api_dir = tmp.join("api");
    std::fs::create_dir_all(&api_dir).expect("create api dir");

    let entry = api_dir.join("checkout.ds");
    let entry_source =
        "import { helperValue } from '../helpers';\nexport const result = helperValue + 1;\n";
    std::fs::write(&entry, entry_source).expect("write entry");
    std::fs::write(tmp.join("helpers.ds"), "export const helperValue = 41;\n")
        .expect("write helper");

    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: entry_source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: false,
            iife: false,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("bundle should resolve ../helpers.ds from api/checkout.ds");

    assert!(
        result.contains("41") && result.contains("result"),
        "expected parent helper module in bundle: {}",
        result
    );

}

#[test]
fn bundle_with_iife_wrapping() {
    let tmp = make_tmp_dir("iife");
    let entry = tmp.join("entry.js");
    std::fs::write(&entry, "const msg = 'hello';\n").expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: "const msg = 'hello';\n".to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: false,
            iife: true,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("bundle should succeed");
    assert!(
        result.contains("function") || result.contains("hello"),
        "expected IIFE or content in bundle: {}",
        result
    );
}

#[test]
fn bundle_iife_strips_exports_and_await() {
    let tmp = make_tmp_dir("iife_exports");
    let entry = tmp.join("entry.js");
    let source = r#"
export const dekaBuildMode = "subset-ast";
export const dekaTargetSemantics = "js";
function App(req) { return { status: 200, body: "ok" }; }
const __deka_main = async () => {
let app = App;
globalThis.app = app;
};
await __deka_main();
"#;
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: true,
            iife: true,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("bundle should succeed");
    // IIFE mode should NOT contain export statements
    assert!(
        !result.contains("export "),
        "IIFE bundle should not contain export statements: {}",
        result
    );
    // Must start with `(async function` for pre-bundled IIFE detection
    let trimmed = result.trim_start();
    assert!(
        trimmed.starts_with("(async function"),
        "IIFE bundle should start with (async function: starts with {:?}",
        &trimmed[..60.min(trimmed.len())]
    );
}

#[test]
fn bundle_minified_output_is_valid() {
    let tmp = make_tmp_dir("minified");
    let entry = tmp.join("entry.js");
    std::fs::write(&entry, "export const greeting = 'hello world';\n").expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: "export const greeting = 'hello world';\n".to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: true,
            iife: false,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("minified bundle should succeed");
    assert!(
        result.contains("hello world"),
        "expected string in minified bundle: {}",
        result
    );
}

/// Regression: `if (cond) x = y;` must not be rewritten into
/// `cond && x = y` (invalid: assignment LHS not parenthesized).
/// Root cause: `compress_if_stmt_as_expr` in
/// swc_ecma_minifier/src/compress/pure/bools.rs runs whenever
/// `conditionals || bools` is enabled; the bundler disables both.
#[test]
fn bundle_minified_preserves_if_assignment() {
    let tmp = make_tmp_dir("if_assign");
    let entry = tmp.join("entry.js");
    // The minifier used to emit `s < 0 && s = Math.max(...)` here,
    // which Bun/V8 rejects as an invalid LHS in assignment.
    let source = "\
        export function substr(src, start) {\n\
          let s = Number(start) || 0;\n\
          if (s < 0) s = Math.max(src.length + s, 0);\n\
          return src.slice(s);\n\
        }\n\
        export const out = substr('hello', -2);\n";
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: true,
            iife: false,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("minified bundle should succeed");
    assert!(
        !result.contains("&& s ="),
        "minifier broke `if (cond) x = y` into `cond && x = y`: {}",
        result
    );
}

/// Regression: compress must not fold adjacent statements into a
/// `for-of` head. `count = 0; for (let _ of arr) ...` must stay two
/// statements — otherwise we get `for (let _ of count = 0, arr)`,
/// which is a parse error. Disabling `sequences` prevents this.
#[test]
fn bundle_minified_preserves_for_of_head() {
    let tmp = make_tmp_dir("for_of");
    let entry = tmp.join("entry.js");
    let source = "\
        export function __c(value) {\n\
          let count = 0;\n\
          for (const _ of (Array.isArray(value) ? value : [])) {\n\
            count += 1;\n\
          }\n\
          return count;\n\
        }\n\
        export const out = __c([1, 2, 3]);\n";
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: true,
            iife: false,
            client: false,
            prelude: None,
        },
        provider,
    )
    .expect("minified bundle should succeed");
    assert!(
        !result.contains("of count = 0,"),
        "minifier folded a statement into the for-of head: {}",
        result
    );
}

#[test]
fn client_bundle_rejects_ui_server_import() {
    let tmp = make_tmp_dir("client_ui_server");
    let entry = tmp.join("island.js");
    let source = "import { renderToString } from \"ui/server\";\nexport const x = renderToString;\n";
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let err = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: false,
            iife: false,
            client: true,
            prelude: None,
        },
        provider,
    )
    .expect_err("client bundle must reject ui/server");
    assert!(
        err.contains("ui/server"),
        "{err}"
    );
}

#[test]
fn client_bundle_allows_ui_jsx() {
    let tmp = make_tmp_dir("client_ui_jsx");
    let entry = tmp.join("island.js");
    let source = "import { jsx } from \"ui/jsx\";\nexport const node = jsx(\"div\", { children: \"ok\" });\n";
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.path_buf(),
            minify: false,
            iife: false,
            client: true,
            prelude: None,
        },
        provider,
    )
    .expect("client bundle may import ui/jsx");
    assert!(
        result.contains("jsx") || result.contains("div"),
        "{result}"
    );
    assert!(
        !result.contains("renderToString"),
        "ui/server leaked into client bundle: {result}"
    );
}

#[test]
fn resolver_only_uses_project_local_php_modules() {
    // There is no stdlib fallback. A stdlib path passed to the resolver
    // is ignored — only project-local php_modules/ is consulted.
    let project = make_tmp_dir("no_stdlib_fallback_project");
    let stdlib = make_tmp_dir("no_stdlib_fallback_stdlib");

    std::fs::create_dir_all(project.join(MODULES_DIR)).unwrap();
    let crypto_dir = stdlib.join("crypto");
    std::fs::create_dir_all(&crypto_dir).unwrap();
    std::fs::write(
        crypto_dir.join("index.js"),
        "export function random_hex() { return '0a'; }\n",
    )
    .unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();
    assert!(
        resolver.resolve_php_module("crypto").unwrap().is_none(),
        "stdlib fallback is disabled: resolver must return None for missing packages"
    );

    // If project-local has the module it resolves normally.
    let local_crypto = project.join(MODULES_DIR).join("crypto");
    std::fs::create_dir_all(&local_crypto).unwrap();
    std::fs::write(
        local_crypto.join("index.js"),
        "export function random_hex() { return 'local'; }\n",
    )
    .unwrap();

    let resolver2 = DekaResolver::new(project.path_buf(), false).unwrap();
    let result2 = resolver2.resolve_php_module("crypto").unwrap();
    assert!(result2.is_some(), "expected local resolution");
    assert!(
        result2.unwrap().starts_with(&project),
        "local php_modules should resolve"
    );

}

#[test]
fn resolver_prefers_local_link_over_installed_package_and_aliases() {
    let project = make_tmp_dir("local_link_precedence");
    let package = make_tmp_dir("local_link_package");
    let package_root = package.canonicalize().unwrap();
    std::fs::write(
        package.join("deka.json"),
        r#"{"name":"@deka/example","version":"0.1.0"}"#,
    )
    .unwrap();
    let installed = project.join(MODULES_DIR).join("@deka").join("example");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(
        installed.join("index.ds"),
        "export const source = 'installed';\n",
    )
    .unwrap();
    std::fs::write(
        package.join("index.ds"),
        "export const source = 'linked';\n",
    )
    .unwrap();

    write_links_at(
        &project,
        &LinkManifest {
            version: deka_project::modules::LINKS_VERSION,
            packages: std::collections::BTreeMap::from([(
                "@deka/example".to_string(),
                LinkEntry {
                    path: package_root.clone(),
                },
            )]),
        },
    )
    .unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();
    for specifier in ["@deka/example", "example"] {
        let resolved = resolver
            .resolve_php_module(specifier)
            .unwrap()
            .expect("linked package should resolve");
        assert!(resolved.starts_with(&package_root));
    }

}

#[test]
fn resolver_does_not_fall_back_when_linked_subpath_is_missing() {
    let project = make_tmp_dir("local_link_missing_subpath");
    let package = make_tmp_dir("local_link_missing_subpath_package");
    let installed = project.join(MODULES_DIR).join("@deka").join("example");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(
        installed.join("missing.ds"),
        "export const source = 'installed';\n",
    )
    .unwrap();
    std::fs::write(
        package.join("deka.json"),
        r#"{"name":"@deka/example","version":"0.1.0"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("index.ds"),
        "export const source = 'linked';\n",
    )
    .unwrap();
    write_links_at(
        &project,
        &LinkManifest {
            version: deka_project::modules::LINKS_VERSION,
            packages: std::collections::BTreeMap::from([(
                "@deka/example".to_string(),
                LinkEntry {
                    path: package.canonicalize().unwrap(),
                },
            )]),
        },
    )
    .unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();
    let error = resolver
        .resolve_php_module("@deka/example/missing")
        .expect_err("missing linked subpath must not use installed bytes");
    assert!(error.contains("unable to resolve linked module"), "{error}");

}

#[test]
fn resolver_rejects_stale_local_link_instead_of_falling_back() {
    let project = make_tmp_dir("stale_local_link");
    let missing = project.join("missing-package");
    write_links_at(
        &project,
        &LinkManifest {
            version: deka_project::modules::LINKS_VERSION,
            packages: std::collections::BTreeMap::from([(
                "@deka/example".to_string(),
                LinkEntry { path: missing },
            )]),
        },
    )
    .unwrap();

    let error = match DekaResolver::new(project.path_buf(), false) {
        Ok(_) => panic!("stale link must fail"),
        Err(error) => error,
    };
    assert!(error.contains("missing target"));
}

#[test]
fn resolver_rejects_path_traversal() {
    let project = make_tmp_dir("path_traversal");
    let modules = project.join(MODULES_DIR);
    std::fs::create_dir_all(modules.join("component")).unwrap();
    std::fs::write(
        modules.join("component").join("button.js"),
        "export const Button = 'ok';\n",
    )
    .unwrap();

    // Also create a file outside php_modules to be the traversal target
    std::fs::write(project.join("secret.js"), "export const secret = 'oops';\n").unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();

    // Normal resolution should work
    let normal = resolver.resolve_php_module("component/button").unwrap();
    assert!(normal.is_some(), "normal module resolution should work");

    // Path traversal should fail — the specifier escapes php_modules/
    let traversal = resolver
        .resolve_php_module("component/../../secret")
        .unwrap();
    assert!(
        traversal.is_none(),
        "path traversal should be rejected, but resolved to: {:?}",
        traversal
    );

}

// Regression for issue #36: a file in api/ must be able to import from
// the package root via `../helpers`.  The resolved path must stay within
// the project root — this is the security check in guard_path_traversal.
#[test]
fn resolver_allows_parent_relative_import_within_project() {
    let project = make_tmp_dir("parent_relative_import");

    // Create project structure:
    //   helpers.js           <- the shared helper at the project root
    //   api/checkout.js      <- file that imports ../helpers
    let api_dir = project.join("api");
    std::fs::create_dir_all(&api_dir).unwrap();
    std::fs::write(
        project.join("helpers.js"),
        "export function client_ip() { return '127.0.0.1'; }\n",
    )
    .unwrap();
    std::fs::write(
        api_dir.join("checkout.js"),
        "import { client_ip } from '../helpers';\n",
    )
    .unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();

    // Resolve `../helpers` from `api/checkout.js`
    let base = FileName::Real(api_dir.join("checkout.js"));
    let result = resolver.resolve(&base, "../helpers");
    assert!(
        result.is_ok(),
        "expected ../helpers to resolve from api/checkout.js, got: {:?}",
        result
    );
    let resolved_path = match result.unwrap().filename {
        FileName::Real(p) => p,
        other => panic!("expected FileName::Real, got {other:?}"),
    };
    assert!(
        resolved_path.starts_with(&project),
        "resolved path {:?} must stay within project root {:?}",
        resolved_path,
        project
    );

}

// Ensure that `../../..` traversal that exits the project root is blocked.
// DekaResolver now calls guard_path_traversal before returning Ok for any
// resolved relative path, so even files that exist outside the root are
// rejected with a path-traversal error (not file-not-found).
#[test]
fn resolver_parent_relative_import_stays_within_project() {
    // Layout:
    //   workspace/
    //     outside.js          <- the target file: exists but outside project
    //     project/
    //       api/
    //         checkout.js     <- base file
    //
    // `../outside` from `workspace/project/api/checkout.js`
    // resolves to `workspace/outside.js` — exists but outside the project root.
    let workspace = make_tmp_dir("parent_relative_escaping_workspace");
    let project = workspace.join("project");
    let api_dir = project.join("api");
    std::fs::create_dir_all(&api_dir).unwrap();
    std::fs::write(
        api_dir.join("checkout.js"),
        "import { x } from '../../outside';\n",
    )
    .unwrap();

    // Create a file that DOES exist but is OUTSIDE the project root.
    // This is the file that the previous implementation silently allowed
    // through (the only reason it returned Err was file-not-found).
    std::fs::write(workspace.join("outside.js"), "export const x = 'leaked';\n").unwrap();

    let resolver = DekaResolver::new(project.clone(), false).unwrap();
    let base = FileName::Real(api_dir.join("checkout.js"));

    // `../../outside` from `project/api/checkout.js` resolves to
    // `workspace/outside.js` — which exists but is outside the project root.
    // guard_path_traversal must reject it.
    let result = resolver.resolve(&base, "../../outside");
    assert!(
        result.is_err(),
        "expected path-traversal Err for import resolving outside project root, got Ok"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Path traversal") || err_msg.contains("traversal"),
        "expected path-traversal message, got: {err_msg}"
    );

}

// Confirm that going all-the-way out (many ../ hops) is also rejected.
#[test]
fn resolver_deep_traversal_to_system_path_is_rejected() {
    let project = make_tmp_dir("deep_traversal");
    let src_dir = project.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("index.js"), "// entry\n").unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();
    let base = FileName::Real(src_dir.join("index.js"));

    // This specifier attempts to climb to /etc/passwd (or an analogous
    // path on this OS). It either doesn't exist (Err from file-not-found)
    // or it does exist but must be rejected by the path-traversal guard.
    // Either way it must NOT return Ok with a path outside the project.
    let result = resolver.resolve(&base, "../../../../../../etc/passwd");
    if let Ok(ref res) = result {
        if let FileName::Real(ref p) = res.filename {
            panic!(
                "resolver returned Ok with path outside project root: {:?}",
                p
            );
        }
    }
    // Err is the expected outcome (traversal rejected or file not found).

}

// Positive test: a within-project cross-package `../` import is allowed.
// (The existing resolver_allows_parent_relative_import_within_project test
//  covers api/ → root helpers. This test covers one level deeper nesting.)
#[test]
fn resolver_allows_parent_relative_import_two_levels_within_project() {
    let project = make_tmp_dir("parent_relative_two_levels");
    let deep_dir = project.join("components").join("ui");
    std::fs::create_dir_all(&deep_dir).unwrap();
    std::fs::write(
        project.join("utils.js"),
        "export function fmt(x) { return String(x); }\n",
    )
    .unwrap();
    std::fs::write(
        deep_dir.join("button.js"),
        "import { fmt } from '../../utils';\n",
    )
    .unwrap();

    let resolver = DekaResolver::new(project.path_buf(), false).unwrap();
    let base = FileName::Real(deep_dir.join("button.js"));
    let result = resolver.resolve(&base, "../../utils");
    assert!(
        result.is_ok(),
        "expected ../../utils to resolve from components/ui/button.js, got: {:?}",
        result
    );
    let resolved_path = match result.unwrap().filename {
        FileName::Real(p) => p,
        other => panic!("expected FileName::Real, got {other:?}"),
    };
    assert!(
        resolved_path.starts_with(&project),
        "resolved path {:?} must stay within project root {:?}",
        resolved_path,
        project
    );

}
