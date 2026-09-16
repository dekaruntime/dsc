use super::*;

pub(crate) fn completion_for_annotation(
    source: &str,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    let line_start = source[..offset.min(source.len())]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let prefix = &source[line_start..offset.min(source.len())];
    let at = prefix.rfind('@')?;
    let typed = &prefix[at + 1..];
    if typed
        .chars()
        .any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric()))
    {
        return None;
    }

    let mut items = Vec::new();
    for (name, detail) in annotation_catalog() {
        if !typed.is_empty() && !name.starts_with(typed) {
            continue;
        }
        let (insert_text, insert_text_format) = match name {
            "index" => (
                Some("index(${1:\"idx_name\"})".to_string()),
                Some(InsertTextFormat::SNIPPET),
            ),
            "map" => (
                Some("map(${1:\"column_name\"})".to_string()),
                Some(InsertTextFormat::SNIPPET),
            ),
            "default" => (
                Some("default(${1:value})".to_string()),
                Some(InsertTextFormat::SNIPPET),
            ),
            "relation" => (
                Some("relation(${1:\"hasMany\"}, ${2:\"Model\"}, ${3:\"foreignKey\"})".to_string()),
                Some(InsertTextFormat::SNIPPET),
            ),
            _ => (Some(name.to_string()), None),
        };
        items.push(CompletionItem {
            label: format!("@{}", name),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some("struct field annotation".to_string()),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: detail.to_string(),
            })),
            insert_text,
            insert_text_format,
            ..CompletionItem::default()
        });
    }
    Some(items)
}

pub(crate) fn annotation_catalog() -> Vec<(&'static str, &'static str)> {
    vec![
        ("id", "Primary key marker. No arguments."),
        ("unique", "Unique constraint marker. No arguments."),
        (
            "autoIncrement",
            "Auto-increment marker. Requires an `int` field.",
        ),
        (
            "index",
            "Secondary index marker. Optional string index name argument.",
        ),
        (
            "map",
            "Column mapping marker. Requires a string column name.",
        ),
        ("default", "Default value marker. Requires one argument."),
        (
            "relation",
            "Relation marker. Requires three string arguments: relation kind (`hasMany|belongsTo|hasOne`), model name, foreign key.",
        ),
    ]
}

pub(crate) fn builtin_completion_items() -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for name in [
        "Option", "Result", "Promise", "Object", "array", "int", "string", "bool", "float",
    ] {
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::TYPE_PARAMETER),
            ..CompletionItem::default()
        });
    }
    items
}

pub(crate) fn stdlib_completion_items() -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for name in [
        "panic",
        "is_valid_element",
        "create_root",
        "readFile",
        "readFileSync",
        "writeFile",
        "writeFileSync",
        "connect",
        "connectSync",
        "query",
        "querySync",
        "queryOne",
        "queryOneSync",
        "open",
        "openSync",
        "openHandle",
        "openHandleSync",
        "exec",
        "execSync",
        "begin",
        "beginSync",
        "commit",
        "commitSync",
        "rollback",
        "rollbackSync",
        "close",
        "closeSync",
        "read",
        "readSync",
        "readExact",
        "readExactSync",
        "write",
        "writeSync",
        "setDeadline",
        "setDeadlineSync",
    ] {
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            ..CompletionItem::default()
        });
    }
    items
}

pub(crate) fn snippet_completion_items() -> Vec<CompletionItem> {
    vec![
        CompletionItem {
            label: "snippet:function".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("function ${1:name}(${2:arg}: ${3:string}): ${4:string} {\n    return ${5:''};\n}".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("DekaScript function template".to_string()),
            ..CompletionItem::default()
        },
        CompletionItem {
            label: "snippet:async-function".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some(
                "async function ${1:name}(${2:arg}: Promise<${3:string}>): Promise<${3:string}> {\n    return await ${2:arg};\n}"
                    .to_string(),
            ),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("DekaScript async function template".to_string()),
            ..CompletionItem::default()
        },
        CompletionItem {
            label: "snippet:object".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("const ${1:name}: { ${2:field}: ${3:string} } = { ${2:field}: ${4:''} };".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("DekaScript object template".to_string()),
            ..CompletionItem::default()
        },
        CompletionItem {
            label: "snippet:import".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("import { ${1:symbol} } from '${2:module}'".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("DekaScript import template".to_string()),
            ..CompletionItem::default()
        },
        CompletionItem {
            label: "snippet:component".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("function ${1:component}(${2:props}: { ${3:message}: string }): string {\n    return ${2:props}.${3:message};\n}".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("DekaScript component template".to_string()),
            ..CompletionItem::default()
        },
        CompletionItem {
            label: "snippet:frontmatter".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("import { ${1:symbol} } from '${2:module}';\n\nconst ${3:value} = ${1:symbol};\n".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("DekaScript module template".to_string()),
            ..CompletionItem::default()
        },
    ]
}

/// The identifier fragment immediately before `offset` — the prefix the user
/// has typed and that completion items must match.
pub(crate) fn identifier_prefix_at(source: &str, offset: usize) -> &str {
    let bytes = source.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 {
        let byte = bytes[start - 1];
        if byte == b'_' || byte.is_ascii_alphanumeric() {
            start -= 1;
        } else {
            break;
        }
    }
    &source[start..offset.min(bytes.len())]
}

pub(crate) fn filter_by_prefix(
    items: Vec<CompletionItem>,
    prefix: &str,
) -> Vec<CompletionItem> {
    if prefix.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| item.label.starts_with(prefix))
        .collect()
}

fn scope_item_completion_kind(kind: deka_syntax::ScopeItemKind) -> CompletionItemKind {
    match kind {
        deka_syntax::ScopeItemKind::Function => CompletionItemKind::FUNCTION,
        deka_syntax::ScopeItemKind::Const => CompletionItemKind::CONSTANT,
        deka_syntax::ScopeItemKind::Variable => CompletionItemKind::VARIABLE,
        deka_syntax::ScopeItemKind::Param => CompletionItemKind::VARIABLE,
        deka_syntax::ScopeItemKind::Import => CompletionItemKind::MODULE,
        deka_syntax::ScopeItemKind::Struct => CompletionItemKind::STRUCT,
        deka_syntax::ScopeItemKind::Enum => CompletionItemKind::ENUM,
        deka_syntax::ScopeItemKind::Type => CompletionItemKind::TYPE_PARAMETER,
    }
}

fn scope_item_detail(kind: deka_syntax::ScopeItemKind) -> &'static str {
    match kind {
        deka_syntax::ScopeItemKind::Function => "function",
        deka_syntax::ScopeItemKind::Const => "constant",
        deka_syntax::ScopeItemKind::Variable => "variable",
        deka_syntax::ScopeItemKind::Param => "parameter",
        deka_syntax::ScopeItemKind::Import => "imported",
        deka_syntax::ScopeItemKind::Struct => "struct",
        deka_syntax::ScopeItemKind::Enum => "enum",
        deka_syntax::ScopeItemKind::Type => "type",
    }
}

/// Completion items for every name in scope at `offset`: locals and params of
/// the enclosing blocks, the module's top-level items, and its imports — all
/// from the parsed program, not a text scan.
pub(crate) fn scope_completion_items(
    program: &deka_syntax::Program,
    offset: usize,
) -> Vec<CompletionItem> {
    deka_syntax::names_in_scope_at_offset(program, offset)
        .into_iter()
        .map(|item| CompletionItem {
            label: item.name.to_string(),
            kind: Some(scope_item_completion_kind(item.kind)),
            detail: Some(scope_item_detail(item.kind).to_string()),
            ..CompletionItem::default()
        })
        .collect()
}

/// The ambient React-style builtins (`useState`, …) the compiler
/// auto-imports in `.dsx` modules — offered there so `const x = use│` finds
/// them even though no import statement names them.
pub(crate) fn ambient_hook_completion_items() -> Vec<CompletionItem> {
    deka_syntax::AMBIENT_REACT_BUILTINS
        .iter()
        .map(|name| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some("ambient hook (auto-imported from @js/react)".to_string()),
            ..CompletionItem::default()
        })
        .collect()
}

/// Component completions for a cursor in JSX tag position: the in-scope names
/// that can be tags — the UpperCamelCase functions, constants and imports the
/// scope query reports. Returns `None` when the cursor is not in tag position.
pub(crate) fn component_completion_items(
    program: Option<&deka_syntax::Program>,
    source: &str,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    let prefix = match program {
        Some(program) => deka_syntax::jsx_tag_prefix_at(program, source, offset)
            .map(str::to_string)
            .or_else(|| jsx_tag_prefix_textual(source, offset)),
        None => jsx_tag_prefix_textual(source, offset),
    }?;
    let items = match program {
        Some(program) => scope_completion_items(program, offset),
        None => Vec::new(),
    };
    let items: Vec<CompletionItem> = items
        .into_iter()
        .filter(|item| {
            item.label
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
        })
        .collect();
    Some(filter_by_prefix(items, &prefix))
}

/// Tag-position detection for source that does not parse (mid-edit): the
/// cursor follows `<` directly, and the `<` does not look like a comparison
/// (its left neighbour is not an expression-ending character).
fn jsx_tag_prefix_textual(source: &str, offset: usize) -> Option<String> {
    let bytes = source.as_bytes();
    let offset = offset.min(bytes.len());
    let mut start = offset;
    while start > 0 {
        let byte = bytes[start - 1];
        if byte == b'_' || byte == b'.' || byte.is_ascii_alphanumeric() {
            start -= 1;
        } else {
            break;
        }
    }
    if start == 0 || bytes[start - 1] != b'<' {
        return None;
    }
    let mut left = start - 1;
    while left > 0 && bytes[left - 1].is_ascii_whitespace() {
        left -= 1;
    }
    if left > 0 {
        let byte = bytes[left - 1];
        let ends_expression = byte == b'_'
            || byte.is_ascii_alphanumeric()
            || matches!(byte, b')' | b']' | b'}' | b'>' | b'"' | b'\'');
        if ends_expression {
            return None;
        }
    }
    Some(source[start..offset].to_string())
}
