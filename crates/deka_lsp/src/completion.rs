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
