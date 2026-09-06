use crate::module_spec::is_valid_package_name;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Consumer package install directory (`deka add` / `deka install`).
pub const MODULES_DIR: &str = "ds_modules";
/// Pre-cutover install directory. Resolution still accepts this if present.
pub const LEGACY_MODULES_DIR: &str = "php_modules";
pub const DEKA_CONFIG_DIR: &str = ".deka";
pub const LINKS_FILE: &str = "links.json";
pub const LINKS_VERSION: u32 = 1;

/// Project-local overrides used by `deka link`. Links are developer state,
/// not a reproducible dependency or a publishable package input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkManifest {
    #[serde(default = "default_links_version")]
    pub version: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, LinkEntry>,
}

impl Default for LinkManifest {
    fn default() -> Self {
        Self {
            version: LINKS_VERSION,
            packages: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkEntry {
    pub path: PathBuf,
}

fn default_links_version() -> u32 {
    LINKS_VERSION
}

pub fn links_path(project: &Path) -> PathBuf {
    project.join(DEKA_CONFIG_DIR).join(LINKS_FILE)
}

/// Read link state strictly. A malformed link must fail closed rather than
/// silently falling back to a registry package with different source bytes.
pub fn read_links_at(project: &Path) -> Result<LinkManifest, String> {
    let path = links_path(project);
    if !path.exists() {
        return Ok(LinkManifest {
            version: LINKS_VERSION,
            packages: BTreeMap::new(),
        });
    }

    let mut raw = String::new();
    File::open(&path)
        .and_then(|mut file| file.read_to_string(&mut raw))
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let manifest: LinkManifest =
        serde_json::from_str(&raw).map_err(|err| format!("invalid {}: {err}", path.display()))?;
    validate_link_manifest(&manifest)?;
    Ok(manifest)
}

/// Resolve and validate all link targets once at project startup.
pub fn read_linked_modules(project: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let manifest = read_links_at(project)?;
    manifest
        .packages
        .into_iter()
        .map(|(name, entry)| {
            let path = fs::canonicalize(&entry.path).map_err(|err| {
                format!(
                    "local link for {name} points to missing target {}: {err}",
                    entry.path.display()
                )
            })?;
            if !path.is_dir() {
                return Err(format!(
                    "local link for {name} is not a directory: {}",
                    path.display()
                ));
            }
            validate_linked_package_manifest(&name, &path)?;
            Ok((name, path))
        })
        .collect()
}

fn validate_linked_package_manifest(name: &str, path: &Path) -> Result<(), String> {
    let manifest_path = path.join("deka.json");
    let raw = fs::read_to_string(&manifest_path).map_err(|err| {
        format!(
            "local link for {name} has no readable package manifest {}: {err}",
            manifest_path.display()
        )
    })?;
    let manifest: Value = serde_json::from_str(&raw).map_err(|err| {
        format!(
            "local link for {name} has an invalid package manifest {}: {err}",
            manifest_path.display()
        )
    })?;
    let target_name = manifest
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "local link for {name} has no non-empty `name` in {}",
                manifest_path.display()
            )
        })?;
    if !is_valid_package_name(target_name) {
        return Err(format!(
            "local link for {name} has invalid package name `{target_name}` in {}",
            manifest_path.display()
        ));
    }
    if target_name != name {
        return Err(format!(
            "local link for {name} points to package `{target_name}` in {}",
            manifest_path.display()
        ));
    }
    Ok(())
}

/// Replace the link manifest atomically. The target directories are never
/// modified by this function, which makes unlink safe by construction.
pub fn write_links_at(project: &Path, manifest: &LinkManifest) -> Result<(), String> {
    validate_link_manifest(manifest)?;
    let directory = project.join(DEKA_CONFIG_DIR);
    fs::create_dir_all(&directory)
        .map_err(|err| format!("failed to create {}: {err}", directory.display()))?;
    let path = directory.join(LINKS_FILE);
    let temp = directory.join(format!(
        ".{LINKS_FILE}.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let result = (|| {
        let mut file = File::create(&temp)
            .map_err(|err| format!("failed to create {}: {err}", temp.display()))?;
        serde_json::to_writer_pretty(&mut file, manifest)
            .map_err(|err| format!("failed to serialize {}: {err}", path.display()))?;
        file.write_all(b"\n")
            .map_err(|err| format!("failed to write {}: {err}", temp.display()))?;
        file.sync_all()
            .map_err(|err| format!("failed to sync {}: {err}", temp.display()))?;
        fs::rename(&temp, &path)
            .map_err(|err| format!("failed to replace {}: {err}", path.display()))?;
        File::open(&directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|err| format!("failed to sync {}: {err}", directory.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn validate_link_manifest(manifest: &LinkManifest) -> Result<(), String> {
    if manifest.version != LINKS_VERSION {
        return Err(format!(
            "unsupported local link manifest version {}; expected {}",
            manifest.version, LINKS_VERSION
        ));
    }
    for (name, entry) in &manifest.packages {
        if !is_valid_package_name(name) {
            return Err(format!("invalid local link package name `{name}`"));
        }
        if !entry.path.is_absolute() {
            return Err(format!(
                "local link for {name} must use an absolute target path"
            ));
        }
    }
    Ok(())
}

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

pub fn detect_deka_module_root_with<Exists, CurrentExe>(
    handler_path: &str,
    lock_exists: &Exists,
    current_exe: &CurrentExe,
) -> Option<PathBuf>
where
    Exists: Fn(&Path) -> bool,
    CurrentExe: Fn() -> Option<PathBuf>,
{
    let path = Path::new(handler_path);
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };

    let mut current = start;
    loop {
        let candidate = current.join("deka.lock");
        if lock_exists(&candidate) {
            return Some(current);
        }
        if !current.pop() {
            break;
        }
    }

    // Fallback for ad-hoc execution outside a project: derive root from
    // binary path (.../target/release/cli -> repo root).
    if let Some(exe) = current_exe() {
        let resolved_exe = exe.canonicalize().unwrap_or(exe);
        if let Some(release_dir) = resolved_exe.parent()
            && let Some(target_dir) = release_dir.parent()
            && let Some(repo_root) = target_dir.parent()
        {
            let lock_path = repo_root.join("deka.lock");
            if lock_exists(&lock_path) {
                return Some(repo_root.to_path_buf());
            }
        }
    }

    None
}

pub fn ensure_deka_module_root_env_with<Exists, CurrentExe, Get, Set>(
    handler_path: &str,
    lock_exists: &Exists,
    current_exe: &CurrentExe,
    env_get: &Get,
    env_set: &mut Set,
) where
    Exists: Fn(&Path) -> bool,
    CurrentExe: Fn() -> Option<PathBuf>,
    Get: Fn(&str) -> Option<String>,
    Set: FnMut(&str, &str),
{
    if env_get("DEKA_MODULE_ROOT").is_some() {
        return;
    }
    if let Some(root) = detect_deka_module_root_with(handler_path, lock_exists, current_exe)
        && let Some(root_str) = root.to_str()
    {
        env_set("DEKA_MODULE_ROOT", root_str);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LinkEntry, LinkManifest, MODULES_DIR, detect_deka_module_root_with,
        ensure_deka_module_root_env_with, install_modules_dir, links_path, read_linked_modules,
        resolve_modules_dir, write_links_at,
    };
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    fn lockset(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn new_project_installs_into_ds_modules() {
        let root = PathBuf::from("/tmp/new-app");
        assert_eq!(install_modules_dir(&root), root.join(MODULES_DIR));
        assert_eq!(resolve_modules_dir(&root), root.join(MODULES_DIR));
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

    #[test]
    fn detect_prefers_local_project_lock() {
        let locks = lockset(&["/repo/deka.lock", "/global/deka.lock"]);
        let exists = |path: &Path| locks.contains(path);
        let current_exe = || Some(PathBuf::from("/repo/target/release/cli"));
        let resolved = detect_deka_module_root_with("/repo/app/main.phpx", &exists, &current_exe)
            .expect("resolve root");
        assert_eq!(resolved, PathBuf::from("/repo"));
    }

    #[test]
    fn detect_falls_back_to_exe_repo_root() {
        let locks = lockset(&["/repo/deka.lock"]);
        let exists = |path: &Path| locks.contains(path);
        let current_exe = || Some(PathBuf::from("/repo/target/release/cli"));
        let resolved =
            detect_deka_module_root_with("/tmp/outside/main.phpx", &exists, &current_exe)
                .expect("resolve fallback root");
        assert_eq!(resolved, PathBuf::from("/repo"));
    }

    #[test]
    fn ensure_keeps_existing_deka_module_root() {
        let locks = lockset(&["/repo/deka.lock"]);
        let exists = |path: &Path| locks.contains(path);
        let current_exe = || Some(PathBuf::from("/repo/target/release/cli"));
        let env_map = HashMap::from([("DEKA_MODULE_ROOT".to_string(), "/already".to_string())]);
        let env_get = |key: &str| env_map.get(key).cloned();
        let mut captured: Vec<(String, String)> = Vec::new();
        let mut env_set = |k: &str, v: &str| captured.push((k.to_string(), v.to_string()));
        ensure_deka_module_root_env_with(
            "/tmp/outside/main.phpx",
            &exists,
            &current_exe,
            &env_get,
            &mut env_set,
        );
        assert!(captured.is_empty(), "existing env must not be overridden");
    }

    #[test]
    fn adwa_process_model_commands_use_same_runtime_resolution_path() {
        let locks = lockset(&["/repo/deka.lock"]);
        let exists = |path: &Path| locks.contains(path);
        let current_exe = || Some(PathBuf::from("/repo/target/release/cli"));
        for command in ["ls", "deka db"] {
            let env = Arc::new(Mutex::new(HashMap::<String, String>::new()));
            let env_get_store = Arc::clone(&env);
            let env_get = move |key: &str| {
                env_get_store
                    .lock()
                    .ok()
                    .and_then(|map| map.get(key).cloned())
            };
            let env_set_store = Arc::clone(&env);
            let mut env_set = |k: &str, v: &str| {
                if let Ok(mut map) = env_set_store.lock() {
                    map.insert(k.to_string(), v.to_string());
                }
            };
            let handler = format!("/tmp/adwa/{}.phpx", command.replace(' ', "_"));
            ensure_deka_module_root_env_with(
                &handler,
                &exists,
                &current_exe,
                &env_get,
                &mut env_set,
            );
            assert_eq!(
                env.lock()
                    .ok()
                    .and_then(|map| map.get("DEKA_MODULE_ROOT").cloned()),
                Some("/repo".to_string()),
                "command '{}' should inherit runtime root resolver",
                command
            );
        }
    }
}
