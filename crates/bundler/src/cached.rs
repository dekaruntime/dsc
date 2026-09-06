use std::fs;

use crate::bundler::{JsBundle, bundle_browser_assets, resolve_entry};

/// Bundle with cache support.
///
/// This wraps bundle_browser_assets with a simple file-level cache.
/// For Phase 1, we cache based on entry file mtime.
/// Phase 2 will add dependency tracking for smarter invalidation.
pub fn bundle_browser_assets_cached(
    entry: &str,
    cache: &mut crate::cache::ModuleCache,
) -> Result<JsBundle, String> {
    if !cache.is_enabled() {
        // Cache disabled, just call through
        return bundle_browser_assets(entry);
    }

    // Get entry path and metadata
    let entry_path = resolve_entry(entry)?;
    let metadata = fs::metadata(&entry_path)
        .map_err(|e| format!("Failed to read entry file metadata: {}", e))?;

    let mtime = metadata
        .modified()
        .map_err(|e| format!("Failed to get entry file mtime: {}", e))?;

    let source =
        fs::read_to_string(&entry_path).map_err(|e| format!("Failed to read entry file: {}", e))?;

    let content_hash = crate::cache::hash_file_content(&source);

    // Try to get from cache
    if let Some(cached) = cache.get(&entry_path) {
        if cached.content_hash == content_hash {
            // Cache hit! Parse the transformed code back into JsBundle
            // For now, we'll store just the JS code. CSS caching comes later.
            stdio::debug("cache", "HIT - using cached bundle");
            return Ok(JsBundle {
                code: cached.transformed_code,
                css: None, // TODO: Cache CSS too
                assets: vec![],
            });
        }
    }

    // Cache miss - run the bundler
    stdio::debug("cache", "MISS - bundling from scratch");
    let bundle = bundle_browser_assets(entry)?;

    // Store in cache
    let cached = crate::cache::CachedModule {
        path: entry_path.clone(),
        source,
        mtime,
        content_hash,
        transformed_code: bundle.code.clone(),
        dependencies: vec![], // TODO: Track dependencies in incremental builds
        resolved_dependencies: vec![], // TODO: Track resolved deps in incremental builds
    };

    cache.put(&entry_path, cached);

    Ok(bundle)
}
