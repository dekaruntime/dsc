use std::path::{Path, PathBuf};

pub use deka_modules::modules::{
    links_path, read_linked_modules, read_links_at, write_links_at, LinkEntry, LinkManifest,
    DEKA_CONFIG_DIR, LINKS_FILE, LINKS_VERSION, MODULES_DIR,
};

/// Pre-cutover install directory. Resolution still accepts this if present.
///
/// deka-modules 0.1.0 does not treat `php_modules` as a modules directory.
pub const LEGACY_MODULES_DIR: &str = "php_modules";

pub fn is_modules_dir_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(MODULES_DIR) || name.eq_ignore_ascii_case(LEGACY_MODULES_DIR)
}

/// Directory new installs write into. Uses `ds_modules` unless this project
/// already has only a legacy `php_modules/` tree.
pub fn install_modules_dir(project: &Path) -> PathBuf {
    let modern = project.join(MODULES_DIR);
    if modern.is_dir() {
        return modern;
    }
    let legacy = project.join(LEGACY_MODULES_DIR);
    if legacy.is_dir() {
        return legacy;
    }
    modern
}

/// Directory to resolve imports from. Prefers `ds_modules/`, then `php_modules/`.
pub fn resolve_modules_dir(project: &Path) -> PathBuf {
    let modern = project.join(MODULES_DIR);
    if modern.is_dir() {
        return modern;
    }
    let legacy = project.join(LEGACY_MODULES_DIR);
    if legacy.is_dir() {
        return legacy;
    }
    modern
}

/// Every consumer-modules directory that exists on disk, modern first.
pub fn existing_modules_dirs(project: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let modern = project.join(MODULES_DIR);
    if modern.is_dir() {
        dirs.push(modern);
    }
    let legacy = project.join(LEGACY_MODULES_DIR);
    if legacy.is_dir() {
        dirs.push(legacy);
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::{
        existing_modules_dirs, install_modules_dir, is_modules_dir_name, links_path,
        read_linked_modules, resolve_modules_dir, write_links_at, LinkEntry, LinkManifest,
        MODULES_DIR,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn new_project_installs_into_ds_modules() {
        let root = PathBuf::from("/tmp/new-app");
        assert_eq!(install_modules_dir(&root), root.join(MODULES_DIR));
        assert_eq!(resolve_modules_dir(&root), root.join(MODULES_DIR));
    }

    #[test]
    fn legacy_php_modules_is_still_a_resolution_fallback() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        std::fs::create_dir(root.join("php_modules")).unwrap();
        assert_eq!(install_modules_dir(root), root.join("php_modules"));
        assert_eq!(resolve_modules_dir(root), root.join("php_modules"));
        assert_eq!(existing_modules_dirs(root), vec![root.join("php_modules")]);
        assert!(is_modules_dir_name("Php_Modules"));

        std::fs::create_dir(root.join(MODULES_DIR)).unwrap();
        assert_eq!(install_modules_dir(root), root.join(MODULES_DIR));
        assert_eq!(resolve_modules_dir(root), root.join(MODULES_DIR));
        assert_eq!(
            existing_modules_dirs(root),
            vec![root.join(MODULES_DIR), root.join("php_modules")]
        );
        assert!(is_modules_dir_name("Ds_Modules"));
    }

    #[test]
    fn local_links_are_atomic_and_resolve_canonical_targets() {
        let project = tempfile::tempdir().expect("project");
        let package = tempfile::tempdir().expect("package");
        std::fs::write(
            package.path().join("deka.json"),
            r#"{"name":"@deka/example","version":"0.1.0"}"#,
        )
        .unwrap();
        let manifest = LinkManifest {
            version: super::LINKS_VERSION,
            packages: BTreeMap::from([(
                "@deka/example".to_string(),
                LinkEntry {
                    path: package.path().to_path_buf(),
                },
            )]),
        };

        write_links_at(project.path(), &manifest).expect("write links");
        assert!(links_path(project.path()).is_file());
        let linked = read_linked_modules(project.path()).expect("read links");
        assert_eq!(
            linked["@deka/example"],
            package.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn malformed_link_state_fails_closed() {
        let project = tempfile::tempdir().expect("project");
        std::fs::create_dir_all(project.path().join(super::DEKA_CONFIG_DIR)).unwrap();
        std::fs::write(
            links_path(project.path()),
            "{\"version\":99,\"packages\":{}}\n",
        )
        .unwrap();
        let error = read_linked_modules(project.path()).expect_err("invalid links must fail");
        assert!(error.contains("unsupported local link manifest version"));
    }

    #[test]
    fn linked_target_manifest_must_match_link_name() {
        let project = tempfile::tempdir().expect("project");
        let package = tempfile::tempdir().expect("package");
        std::fs::write(
            package.path().join("deka.json"),
            r#"{"name":"@deka/other","version":"0.1.0"}"#,
        )
        .unwrap();
        let manifest = LinkManifest {
            version: super::LINKS_VERSION,
            packages: BTreeMap::from([(
                "@deka/example".to_string(),
                LinkEntry {
                    path: package.path().to_path_buf(),
                },
            )]),
        };
        write_links_at(project.path(), &manifest).unwrap();

        let error = read_linked_modules(project.path()).expect_err("mismatched target must fail");
        assert!(error.contains("points to package `@deka/other`"));
    }
}
