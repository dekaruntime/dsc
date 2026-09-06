use super::*;

pub(crate) struct ImportInfo {
    pub(crate) imported: String,
    pub(crate) local: String,
    pub(crate) from: String,
    pub(crate) span: Span,
    pub(crate) module_span: Option<Span>,
    pub(crate) is_wasm: bool,
}

pub(crate) fn parse_imports(source: &str) -> Vec<ImportInfo> {
    let mut imports = Vec::new();
    let mut offset = 0usize;
    for line in source.lines() {
        let line_len = line.len();
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") {
            let is_wasm = line.contains(" as wasm");
            let module_info = parse_module_path_with_span(line, offset);
            let mut rest = trimmed
                .strip_prefix("import")
                .unwrap_or(trimmed)
                .trim_start();
            let mut default_name: Option<&str> = None;
            let mut spec_part: Option<&str> = None;

            if rest.starts_with('{') {
                if let (Some(open), Some(close)) = (line.find('{'), line.find('}')) {
                    spec_part = Some(&line[open + 1..close]);
                    rest = &rest[rest.find('}').unwrap_or(0) + 1..];
                }
            } else {
                if let Some((name, after)) = parse_ident_from_str(rest) {
                    default_name = Some(name);
                    rest = after.trim_start();
                    if let Some(rest_after_comma) = rest.strip_prefix(',') {
                        rest = rest_after_comma.trim_start();
                        if let (Some(open), Some(close)) = (line.find('{'), line.find('}')) {
                            spec_part = Some(&line[open + 1..close]);
                            rest = &rest[rest.find('}').unwrap_or(0) + 1..];
                        }
                    }
                }
            }

            if let Some(name) = default_name {
                if let Some(col) = line.find(name) {
                    let span = Span::new(offset + col, offset + col + name.len());
                    if let Some((module, module_span)) = module_info.clone() {
                        imports.push(ImportInfo {
                            imported: "default".to_string(),
                            local: name.to_string(),
                            from: module,
                            span,
                            module_span: Some(module_span),
                            is_wasm,
                        });
                    }
                }
            }

            if let Some(spec_part) = spec_part {
                if let (Some(open), Some(_close)) = (line.find('{'), line.find('}')) {
                    let mut cursor = open + 1;
                    for spec in spec_part.split(',') {
                        let spec_trim = spec.trim();
                        if spec_trim.is_empty() {
                            cursor += spec.len() + 1;
                            continue;
                        }
                        let (imported, local) =
                            if let Some((left, right)) = spec_trim.split_once(" as ") {
                                (left.trim(), right.trim())
                            } else {
                                (spec_trim, spec_trim)
                            };
                        let local_pos = line[cursor..].find(local).map(|idx| cursor + idx);
                        if let Some(local_pos) = local_pos {
                            let span =
                                Span::new(offset + local_pos, offset + local_pos + local.len());
                            if let Some((module, module_span)) = module_info.clone() {
                                imports.push(ImportInfo {
                                    imported: imported.to_string(),
                                    local: local.to_string(),
                                    from: module,
                                    span,
                                    module_span: Some(module_span),
                                    is_wasm,
                                });
                            } else {
                                imports.push(ImportInfo {
                                    imported: imported.to_string(),
                                    local: local.to_string(),
                                    from: imported.to_string(),
                                    span,
                                    module_span: None,
                                    is_wasm,
                                });
                            }
                        }
                        cursor += spec.len() + 1;
                    }
                }
            }
        }
        offset += line_len + 1;
    }
    imports
}

pub(crate) fn parse_ident_from_str(input: &str) -> Option<(&str, &str)> {
    let mut chars = input.char_indices();
    let (start, first) = chars.next()?;
    if start != 0 {
        return None;
    }
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    Some((&input[..end], &input[end..]))
}

pub(crate) fn parse_module_path(line: &str) -> Option<String> {
    let from_idx = line.find("from")?;
    let rest = &line[from_idx + 4..];
    let quote = rest.find(&['\'', '"'][..])?;
    let quote_char = rest.chars().nth(quote)?;
    let after = &rest[quote + 1..];
    let end = after.find(quote_char)?;
    Some(after[..end].to_string())
}

pub(crate) fn parse_module_path_with_span(
    line: &str,
    line_offset: usize,
) -> Option<(String, Span)> {
    let from_idx = line.find("from")?;
    let rest = &line[from_idx + 4..];
    let quote = rest.find(&['\'', '"'][..])?;
    let quote_char = rest.chars().nth(quote)?;
    let after = &rest[quote + 1..];
    let end = after.find(quote_char)?;
    let start = line_offset + from_idx + 4 + quote + 1;
    let end_pos = start + end;
    Some((after[..end].to_string(), Span::new(start, end_pos)))
}

pub(crate) fn import_module_at_offset(source: &str, offset: usize) -> Option<String> {
    for (line, line_offset) in line_with_offsets(source) {
        let line_end = line_offset + line.len();
        if offset < line_offset || offset > line_end {
            continue;
        }
        if !line.contains("import") || !line.contains("from") {
            return None;
        }
        let (module_spec, span) = parse_module_path_with_span(line, line_offset)?;
        if offset >= span.start && offset <= span.end {
            return Some(module_spec);
        }
        return None;
    }
    None
}

pub(crate) fn import_module_spans(source: &str, module_spec: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    for (line, line_offset) in line_with_offsets(source) {
        if !line.contains("import") || !line.contains("from") {
            continue;
        }
        if let Some((found, span)) = parse_module_path_with_span(line, line_offset) {
            if found == module_spec {
                spans.push(span);
            }
        }
    }
    spans
}

pub(crate) fn line_with_offsets(source: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in source.split('\n') {
        out.push((line, offset));
        offset += line.len() + 1;
    }
    out
}

pub(crate) struct ExportInfo {
    pub(crate) name: String,
    pub(crate) kind: Option<CompletionItemKind>,
}

pub(crate) fn module_exports(
    root: &Path,
    module_spec: &str,
    is_wasm: bool,
) -> Option<Vec<ExportInfo>> {
    let path = resolve_module_file(root, module_spec, is_wasm)?;
    let source = fs::read_to_string(path).ok()?;
    Some(parse_exported_names(&source))
}

pub(crate) fn parse_exported_names(source: &str) -> Vec<ExportInfo> {
    let mut exports = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("export ") {
            continue;
        }
        let rest = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some(name) = rest.strip_prefix("function ") {
            if let Some(token) = export_token(name) {
                exports.push(ExportInfo {
                    name: token,
                    kind: Some(CompletionItemKind::FUNCTION),
                });
            }
            continue;
        }
        if let Some(open) = rest.find('{') {
            let close = rest.rfind('}').unwrap_or(rest.len());
            let inner = &rest[open + 1..close];
            for spec in inner.split(',') {
                let spec = spec.trim();
                if spec.is_empty() {
                    continue;
                }
                let name = if let Some((_, alias)) = spec.split_once(" as ") {
                    alias.trim()
                } else {
                    spec
                };
                if !name.is_empty() {
                    exports.push(ExportInfo {
                        name: name.to_string(),
                        kind: Some(CompletionItemKind::VARIABLE),
                    });
                }
            }
            continue;
        }
        if let Some(name) = rest.strip_prefix("const ") {
            if let Some(token) = export_token(name) {
                exports.push(ExportInfo {
                    name: token,
                    kind: Some(CompletionItemKind::CONSTANT),
                });
            }
            continue;
        }
        if let Some(name) = rest.strip_prefix("type ") {
            if let Some(token) = export_token(name) {
                exports.push(ExportInfo {
                    name: token,
                    kind: Some(CompletionItemKind::TYPE_PARAMETER),
                });
            }
            continue;
        }
        if let Some(name) = rest.strip_prefix("struct ") {
            if let Some(token) = export_token(name) {
                exports.push(ExportInfo {
                    name: token,
                    kind: Some(CompletionItemKind::STRUCT),
                });
            }
            continue;
        }
        if let Some(name) = rest.strip_prefix("enum ") {
            if let Some(token) = export_token(name) {
                exports.push(ExportInfo {
                    name: token,
                    kind: Some(CompletionItemKind::ENUM),
                });
            }
            continue;
        }
    }
    exports
}

pub(crate) fn export_token(input: &str) -> Option<String> {
    let raw = input.trim_start().split_whitespace().next()?;
    let cleaned = raw
        .trim_end_matches(|c: char| c == ';' || c == ',' || c == '{')
        .split('(')
        .next()
        .unwrap_or(raw)
        .trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

pub(crate) fn completion_for_import(
    source: &str,
    file_path: &str,
    offset: usize,
    workspace_roots: &[PathBuf],
) -> Option<Vec<CompletionItem>> {
    let line_index = LineIndex::new(source);
    let position = line_index.offset_to_position(offset);
    let line = position.line as usize;
    let line_start = *line_index.line_starts.get(line)?;
    let line_end = source[line_start..]
        .find('\n')
        .map(|idx| line_start + idx)
        .unwrap_or(source.len());
    let line_text = &source[line_start..line_end];
    if !line_text.contains("import") || !line_text.contains("from") {
        return None;
    }
    let rel = offset.saturating_sub(line_start);

    if let Some(open) = line_text.find('{') {
        let close = line_text.find('}').unwrap_or(line_text.len());
        if rel > open && rel <= close {
            let module_spec = parse_module_path(line_text)?;
            let root = find_php_modules_root(Path::new(file_path), workspace_roots)?;
            let is_wasm = line_text.contains(" as wasm");
            let exports = module_exports(&root, &module_spec, is_wasm)?;
            let prefix_start = line_text[..rel]
                .rfind(',')
                .map(|idx| idx + 1)
                .unwrap_or(open + 1);
            let raw_prefix = line_text[prefix_start..rel].trim();
            let prefix = raw_prefix
                .split_whitespace()
                .last()
                .unwrap_or(raw_prefix)
                .trim();
            let mut items = Vec::new();
            for export in exports {
                if !prefix.is_empty() && !export.name.starts_with(prefix) {
                    continue;
                }
                items.push(CompletionItem {
                    label: export.name,
                    kind: export.kind,
                    ..CompletionItem::default()
                });
            }
            return Some(items);
        }
    }

    let before_cursor = &source[line_start..offset.min(source.len())];
    let quote_pos = before_cursor.rfind(&['\'', '"'][..])?;
    let prefix = &before_cursor[quote_pos + 1..];

    let root = find_php_modules_root(Path::new(file_path), workspace_roots)?;
    let modules = if prefix.starts_with("@/") {
        list_project_modules(root.parent()?)
            .into_iter()
            .map(|name| format!("@/{}", name))
            .collect::<Vec<_>>()
    } else {
        list_php_modules(&root)
    };
    let mut items = Vec::new();
    for module in modules {
        if !prefix.is_empty() && !module.starts_with(prefix) {
            continue;
        }
        items.push(CompletionItem {
            label: module.clone(),
            kind: Some(CompletionItemKind::MODULE),
            ..CompletionItem::default()
        });
    }
    Some(items)
}
fn modules_dir_if_present(root: &Path) -> Option<PathBuf> {
    for name in ["ds_modules", "php_modules"] {
        let candidate = root.join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn find_php_modules_root(start: &Path, workspace_roots: &[PathBuf]) -> Option<PathBuf> {
    if let Ok(root) = std::env::var("DEKA_MODULE_ROOT") {
        if let Some(candidate) = modules_dir_if_present(Path::new(&root)) {
            return Some(candidate);
        }
    }
    let mut current = start.to_path_buf();
    if current.is_file() {
        current.pop();
    }
    loop {
        if let Some(candidate) = modules_dir_if_present(&current) {
            return Some(candidate);
        }
        if !current.pop() {
            break;
        }
    }
    for workspace_root in workspace_roots {
        if let Some(candidate) = modules_dir_if_present(workspace_root) {
            return Some(candidate);
        }
    }
    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(candidate) = modules_dir_if_present(&current_dir) {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn list_php_modules(root: &Path) -> Vec<String> {
    let mut modules = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return modules;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if name.starts_with('@') && path.is_dir() {
            if let Ok(children) = fs::read_dir(&path) {
                for child in children.flatten() {
                    let child_name = child.file_name().to_string_lossy().to_string();
                    if child.path().is_dir() {
                        modules.push(format!("{}/{}", name, child_name));
                    }
                }
            }
        } else if path.is_dir() {
            modules.push(name);
        }
    }
    modules.sort();
    modules
}

pub(crate) fn list_project_modules(project_root: &Path) -> Vec<String> {
    let mut modules = Vec::new();
    let Ok(entries) = fs::read_dir(project_root) else {
        return modules;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || name == "php_modules"
            || name == "ds_modules"
            || name == "target"
            || name == "node_modules"
        {
            continue;
        }
        if path.is_dir() {
            modules.push(name);
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
                if ext == "ds" {
                    modules.push(name);
                }
            }
        }
    }
    modules.sort();
    modules
}

pub(crate) fn resolve_module_file(
    root: &Path,
    module_spec: &str,
    is_wasm: bool,
) -> Option<PathBuf> {
    let base = if let Some(rest) = module_spec.strip_prefix("@/") {
        let project_root = root.parent()?;
        project_root.join(rest)
    } else {
        root.join(module_spec)
    };
    if base.is_file() && is_dekascript_path(&base) {
        return Some(base);
    }
    let dekascript = base.with_extension("ds");
    if dekascript.is_file() {
        return Some(dekascript);
    }
    if base.is_dir() {
        if is_wasm {
            let stub = base.join("module.d.ds");
            if stub.is_file() {
                return Some(stub);
            }
        }
        let index = base.join("index.ds");
        if index.is_file() {
            return Some(index);
        }
        let module = base.join("module.ds");
        if module.is_file() {
            return Some(module);
        }
    }
    None
}

pub(crate) fn collect_module_rename_edits(
    roots: &[PathBuf],
    active_uri: &Url,
    active_text: &str,
    old_module: &str,
    new_module: &str,
) -> HashMap<Url, Vec<TextEdit>> {
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for root in roots {
        for file in collect_dekascript_files(root) {
            let file_uri = match Url::from_file_path(&file) {
                Ok(uri) => uri,
                Err(_) => continue,
            };
            let content = if &file_uri == active_uri {
                active_text.to_string()
            } else {
                fs::read_to_string(&file).unwrap_or_default()
            };
            let line_index = LineIndex::new(&content);
            let mut edits = Vec::new();
            for span in import_module_spans(&content, old_module) {
                edits.push(TextEdit {
                    range: span_to_range(span, &line_index),
                    new_text: new_module.to_string(),
                });
            }
            if !edits.is_empty() {
                changes.insert(file_uri, edits);
            }
        }
    }
    changes
}

pub(crate) fn collect_reference_locations(
    roots: &[PathBuf],
    active_uri: &Url,
    active_text: &str,
    symbol: &str,
) -> Vec<Location> {
    let mut locations = Vec::new();
    for root in roots {
        for file in collect_dekascript_files(root) {
            let file_uri = match Url::from_file_path(&file) {
                Ok(uri) => uri,
                Err(_) => continue,
            };
            let content = if &file_uri == active_uri {
                active_text.to_string()
            } else {
                fs::read_to_string(&file).unwrap_or_default()
            };
            let line_index = LineIndex::new(&content);
            for span in find_word_occurrences(content.as_bytes(), symbol) {
                locations.push(Location {
                    uri: file_uri.clone(),
                    range: span_to_range(span, &line_index),
                });
            }
        }
    }
    locations
}

pub(crate) fn collect_symbol_rename_edits(
    roots: &[PathBuf],
    active_uri: &Url,
    active_text: &str,
    old_symbol: &str,
    new_symbol: &str,
) -> HashMap<Url, Vec<TextEdit>> {
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for root in roots {
        for file in collect_dekascript_files(root) {
            let file_uri = match Url::from_file_path(&file) {
                Ok(uri) => uri,
                Err(_) => continue,
            };
            let content = if &file_uri == active_uri {
                active_text.to_string()
            } else {
                fs::read_to_string(&file).unwrap_or_default()
            };
            let line_index = LineIndex::new(&content);
            let mut edits = Vec::new();
            for span in find_word_occurrences(content.as_bytes(), old_symbol) {
                edits.push(TextEdit {
                    range: span_to_range(span, &line_index),
                    new_text: new_symbol.to_string(),
                });
            }
            if !edits.is_empty() {
                changes.insert(file_uri, edits);
            }
        }
    }
    changes
}

pub(crate) fn collect_dekascript_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if should_skip_dir(&path) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
                if ext == "ds" {
                    files.push(path);
                }
            }
        }
    }
    files.sort();
    files
}

pub(crate) fn should_skip_dir(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name.starts_with('.')
        || name == "node_modules"
        || name == "target"
        || name == "dist"
        || name == "build"
        || name == "vendor"
}


