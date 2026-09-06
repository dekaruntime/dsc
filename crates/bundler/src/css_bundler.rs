use base64::Engine;
use lightningcss::bundler::{Bundler, FileProvider};
use lightningcss::css_modules::{Config as CssModulesConfig, CssModuleExports, CssModuleReference};
use lightningcss::dependencies::{Dependency, DependencyOptions};
use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet, ToCssResult};
use lightningcss::targets::{Browsers, Targets};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct CssBundle {
    pub code: String,
    pub exports: Option<HashMap<String, String>>,
    pub assets: Vec<CssAsset>,
}

#[derive(Clone)]
pub struct CssAsset {
    pub placeholder: String,
    pub file_name: String,
    pub content_type: String,
    pub body_base64: String,
}

pub fn bundle_css(path: &str, css_modules: bool, minify: bool) -> Result<CssBundle, String> {
    let entry_path = Path::new(path);
    let fs = FileProvider::new();
    let mut options = ParserOptions::default();
    options.filename = path.to_string();
    if css_modules {
        options.css_modules = Some(CssModulesConfig::default());
    }

    let mut bundler = Bundler::new(&fs, None, options);
    let stylesheet = bundler
        .bundle(entry_path)
        .map_err(|err| format!("Failed to bundle CSS {}: {}", path, err))?;

    Ok(process_stylesheet(
        stylesheet,
        entry_path,
        minify,
        css_modules,
    )?)
}

pub fn transform_css(
    source: &str,
    filename: &str,
    css_modules: bool,
    minify: bool,
) -> Result<CssBundle, String> {
    let mut options = ParserOptions::default();
    options.filename = filename.to_string();
    if css_modules {
        options.css_modules = Some(CssModulesConfig::default());
    }
    let stylesheet = StyleSheet::parse(source, options)
        .map_err(|err| format!("Failed to parse CSS {}: {}", filename, err))?;
    let entry_path = Path::new(filename);
    Ok(process_stylesheet(
        stylesheet,
        entry_path,
        minify,
        css_modules,
    )?)
}

fn process_stylesheet(
    mut stylesheet: StyleSheet,
    entry_path: &Path,
    minify: bool,
    css_modules: bool,
) -> Result<CssBundle, String> {
    let targets = Targets::from(default_browsers());
    stylesheet
        .minify(MinifyOptions {
            targets,
            ..MinifyOptions::default()
        })
        .map_err(|err| format!("Failed to minify CSS: {}", err))?;

    let mut to_css = stylesheet
        .to_css(PrinterOptions {
            minify,
            targets,
            analyze_dependencies: Some(DependencyOptions {
                remove_imports: true,
            }),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("Failed to emit CSS: {}", err))?;

    let base_dir = entry_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let assets = collect_url_dependencies(&mut to_css, &base_dir)?;

    Ok(CssBundle {
        code: to_css.code,
        exports: if css_modules {
            to_css.exports.map(exports_to_map)
        } else {
            None
        },
        assets,
    })
}

fn exports_to_map(exports: CssModuleExports) -> HashMap<String, String> {
    exports
        .into_iter()
        .map(|(key, value)| {
            let mut names = vec![value.name];
            for composed in value.composes {
                if let Some(extra) = composed_name(&composed) {
                    if !names.contains(&extra) {
                        names.push(extra);
                    }
                }
            }
            (key, names.join(" "))
        })
        .collect()
}

fn composed_name(reference: &CssModuleReference) -> Option<String> {
    match reference {
        CssModuleReference::Local { name } => Some(name.clone()),
        CssModuleReference::Global { name } => Some(name.clone()),
        CssModuleReference::Dependency { name, .. } => Some(name.clone()),
    }
}

fn collect_url_dependencies(
    result: &mut ToCssResult,
    base_dir: &Path,
) -> Result<Vec<CssAsset>, String> {
    let dependencies = match result.dependencies.take() {
        Some(deps) => deps,
        None => return Ok(Vec::new()),
    };

    let mut assets = Vec::new();
    for dependency in dependencies {
        if let Dependency::Url(dep) = dependency {
            if let Some(asset) = build_asset(&dep.url, &dep.placeholder, base_dir)? {
                assets.push(asset);
            }
        }
    }
    Ok(assets)
}

fn build_asset(url: &str, placeholder: &str, base_dir: &Path) -> Result<Option<CssAsset>, String> {
    if url.starts_with("data:")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("//")
    {
        return Ok(None);
    }

    let (path_part, _suffix) = split_url(url);
    let resolved = if path_part.starts_with('/') {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        cwd.join(path_part.trim_start_matches('/'))
    } else {
        base_dir.join(path_part)
    };

    if !resolved.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&resolved)
        .map_err(|err| format!("Failed to read asset {}: {}", resolved.display(), err))?;
    let mime = mime_guess::from_path(&resolved).first_or_octet_stream();
    let hash = hash_bytes(&bytes);
    let ext = resolved
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let file_name = if ext.is_empty() {
        format!("asset-{}", hash)
    } else {
        format!("asset-{}.{}", hash, ext)
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(Some(CssAsset {
        placeholder: placeholder.to_string(),
        file_name,
        content_type: mime.to_string(),
        body_base64: encoded,
    }))
}

fn split_url(url: &str) -> (String, String) {
    let mut path = url.to_string();
    let mut suffix = String::new();
    if let Some(idx) = url.find('#') {
        path = url[..idx].to_string();
        suffix = url[idx..].to_string();
    }
    if let Some(idx) = path.find('?') {
        suffix = format!("{}{}", &path[idx..], suffix);
        path = path[..idx].to_string();
    }
    (path, suffix)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::new();
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn default_browsers() -> Browsers {
    Browsers {
        android: None,
        chrome: Some(87),
        edge: Some(88),
        firefox: Some(78),
        ie: None,
        ios_saf: None,
        opera: None,
        safari: Some(14),
        samsung: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        dir.push(format!("deka-css-test-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn test_split_url() {
        let (path, suffix) = split_url("images/logo.png?size=2#hash");
        assert_eq!(path, "images/logo.png");
        assert_eq!(suffix, "?size=2#hash");

        let (path, suffix) = split_url("images/logo.png");
        assert_eq!(path, "images/logo.png");
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_hash_bytes() {
        let hash = hash_bytes(b"hello");
        assert_eq!(hash, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    #[test]
    fn test_build_asset_ignores_data_and_http() {
        let dir = make_temp_dir();
        let asset = build_asset("data:image/png;base64,abc", "url:0", &dir).unwrap();
        assert!(asset.is_none());
        let asset = build_asset("https://example.com/logo.png", "url:1", &dir).unwrap();
        assert!(asset.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_build_asset_reads_file() {
        let dir = make_temp_dir();
        let file_path = dir.join("logo.png");
        let bytes = vec![0_u8, 1, 2, 3];
        std::fs::write(&file_path, &bytes).expect("write asset");

        let asset = build_asset("logo.png?x=1#hash", "url:0", &dir)
            .unwrap()
            .expect("expected asset");
        let expected_hash = hash_bytes(&bytes);
        let expected_name = format!("asset-{}.png", expected_hash);

        assert_eq!(asset.placeholder, "url:0");
        assert_eq!(asset.file_name, expected_name);
        assert_eq!(asset.content_type, "image/png");
        assert_eq!(
            asset.body_base64,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
