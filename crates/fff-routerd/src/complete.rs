use crate::roots::{resolve_root, RootRuntime};
use fff::{FuzzySearchOptions, MixedItemRef, MixedSearchConfig, PaginationArgs, QueryParser};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct CompletionResponse {
    pub items: Vec<CompletionItem>,
}

#[derive(Debug, Serialize)]
pub struct CompletionItem {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "relativePath")]
    pub relative_path: String,
    pub name: String,
}

pub fn complete(
    roots: &[std::sync::Arc<RootRuntime>],
    requested: &str,
    query: &str,
    limit: usize,
) -> CompletionResponse {
    let plain = roots.iter().map(std::sync::Arc::as_ref).collect::<Vec<_>>();
    let Ok((root, rel)) = resolve_root(&plain, requested) else {
        return CompletionResponse { items: Vec::new() };
    };
    if root.ensure_ready().is_err() {
        return CompletionResponse { items: Vec::new() };
    }
    let prefix = rel.map(|value| format!("{}/", value.trim_end_matches('/'))).unwrap_or_default();
    let scoped_query = if prefix.is_empty() {
        query.to_string()
    } else {
        format!("{prefix} {query}")
    };
    let parser = QueryParser::new(MixedSearchConfig);
    let parsed = parser.parse(&scoped_query);
    let Ok(picker_guard) = root.picker.read() else {
        return CompletionResponse { items: Vec::new() };
    };
    let Some(picker) = picker_guard.as_ref() else {
        return CompletionResponse { items: Vec::new() };
    };
    let query_guard = root.query_tracker.read().ok();
    let query_tracker = query_guard.as_ref().and_then(|guard| guard.as_ref());
    let result = picker.fuzzy_search_mixed(
        &parsed,
        query_tracker,
        FuzzySearchOptions {
            pagination: PaginationArgs { offset: 0, limit: limit.saturating_mul(3) },
            ..Default::default()
        },
    );
    let mut items = Vec::with_capacity(limit);
    for item in result.items {
        let (kind, relative_path) = match item {
            MixedItemRef::File(file) => ("file", file.relative_path(picker)),
            MixedItemRef::Dir(dir) => ("directory", dir.relative_path(picker)),
        };
        if !prefix.is_empty() && !relative_path.starts_with(&prefix) {
            continue;
        }
        let stripped = relative_path.strip_prefix(&prefix).unwrap_or(&relative_path).to_string();
        if stripped.is_empty() {
            continue;
        }
        let name = Path::new(stripped.trim_end_matches('/'))
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| stripped.clone());
        items.push(CompletionItem { kind, relative_path: stripped, name });
        if items.len() == limit {
            break;
        }
    }
    CompletionResponse { items }
}
