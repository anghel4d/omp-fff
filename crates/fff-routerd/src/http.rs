use crate::complete;
use crate::cursors::{CursorStore, FindCursor, GrepCursor};
use crate::format::{self, AnnotationData, FindItem, GrepLine};
use crate::forward;
use crate::query;
use crate::roots::{match_forward, resolve_root, RootRuntime, RootState};
use crate::snapshot;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{header, Response, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use fff::git::format_git_status;
use fff::{AiGrepConfig, FuzzySearchOptions, GrepMode, GrepSearchOptions, PaginationArgs, QueryParser};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct AppState {
    pub roots: Arc<Vec<Arc<RootRuntime>>>,
    pub grep_cursors: Arc<Mutex<CursorStore<GrepCursor>>>,
    pub find_cursors: Arc<Mutex<CursorStore<FindCursor>>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/find", get(find))
        .route("/grep", get(grep))
        .route("/multigrep", get(multigrep))
        .route("/rescan", post(rescan))
        .route("/healthz", get(healthz))
        .route("/complete", get(complete_handler))
        .fallback(not_found)
        .with_state(state)
}

async fn find(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response<Body> {
    let params = Params::parse(raw.as_deref());
    if let Some(response) = forward_request(&state, &params, "/find").await {
        return response;
    }
    respond_text(find_inner(&state, params))
}

/// When `path` targets a forwarding root, proxy the request to the upstream
/// daemon (same endpoint, path rewritten local->remote) and return its
/// response with absolute paths rewritten remote->local. `None` means the
/// request is served locally. Success and failure double as the lazy upstream
/// probe: transport errors mark the root unhealthy, recoveries re-probe.
async fn forward_request(state: &AppState, params: &Params, route: &str) -> Option<Response<Body>> {
    let requested = params.first("path").filter(|value| !value.is_empty())?;
    let (root, rel) = match_forward(&state.roots, requested)?;
    let target_config = root.forward.as_ref()?;
    let remote_path = target_config.to_remote(rel.as_deref());
    let target = format!("{route}?{}", params.encode_with_path(&remote_path));
    let method = if route == "/rescan" { "POST" } else { "GET" };
    match forward::http_request(&target_config.authority, method, &target).await {
        Ok((status, content_type, body)) => {
            if root.state() != RootState::Ready {
                let probe_root = Arc::clone(root);
                tokio::spawn(async move { forward::probe_and_apply(&probe_root).await });
            }
            let rewritten = target_config.rewrite_to_local(&body, &root.path.to_string_lossy());
            Some(proxied_response(status, content_type, rewritten))
        }
        Err(error) => {
            let connect_failure = error.is_connect();
            let message = if connect_failure {
                // The daemon itself is unreachable: fold that into the probed
                // health so atlas/health.json explain why requests fail.
                let message = format!("forward upstream unreachable: {error}");
                root.set_unhealthy(message.clone());
                message
            } else {
                // Post-connect failure or timeout (upstream still scanning, a
                // slow cold-cache grep): a per-request error that must not
                // poison the root's probed health state.
                format!("forward upstream request failed: {error}")
            };
            if route == "/complete" {
                // /complete swallows coverage errors into an empty list; keep
                // that contract when the upstream misbehaves (health shows why).
                Some(respond_json(Ok("{\"items\":[]}".to_string())))
            } else if connect_failure {
                Some(respond_text(Err(format!("INDEX_UNHEALTHY: {}: {message}", root.id))))
            } else {
                Some(proxied_response(503, None, format!("FORWARD_UPSTREAM_ERROR: {}: {message}\n", root.id)))
            }
        }
    }
}

/// Pass an upstream response through with its status and content type intact,
/// so error prefixes and JSON endpoints keep their PROTOCOL.md semantics.
fn proxied_response(status: u16, content_type: Option<String>, body: String) -> Response<Body> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, content_type.unwrap_or_else(|| "text/plain; charset=utf-8".to_string()))
        .body(Body::from(body))
        .unwrap()
}

fn find_inner(state: &AppState, params: Params) -> Result<String, String> {
    params.reject_unknown(&["path", "query", "limit", "cursor"])?;
    let requested = params.required("path")?;
    let user_query = params.required("query")?;
    let limit = params.integer("limit", 30, 1, 200)?;
    let cursor_token = params.first("cursor");
    let roots = state.roots.iter().map(Arc::as_ref).collect::<Vec<_>>();
    let (root, rel) = resolve_root(&roots, requested)?;
    root.ensure_ready()?;
    let resumed = lock(&state.find_cursors).get(cursor_token);
    if resumed.as_ref().is_some_and(|cursor| cursor.root_id != root.id) {
        return Err("INVALID_CURSOR: cursor belongs to a different index".into());
    }
    let effective_query = match &resumed {
        Some(cursor) => cursor.query.clone(),
        None => query::build_query(rel.as_deref(), user_query, &[], &root.path)?,
    };
    let page = resumed.as_ref().map_or(0, |cursor| cursor.page);
    let effective_limit = resumed.as_ref().map_or(limit, |cursor| cursor.limit);
    let pattern = resumed.as_ref().map_or(user_query, |cursor| cursor.pattern.as_str());
    let parser = QueryParser::default();
    let parsed = parser.parse(&effective_query);
    let picker_guard = root.picker.read().map_err(|error| error.to_string())?;
    let picker = picker_guard.as_ref().ok_or_else(|| format!(
        "INDEX_NOT_READY: {} is still starting; retry shortly or use native tools.", root.id
    ))?;
    let query_guard = root.query_tracker.read().map_err(|error| error.to_string())?;
    let found = picker.fuzzy_search(
        &parsed,
        query_guard.as_ref(),
        FuzzySearchOptions {
            pagination: PaginationArgs { offset: page * effective_limit, limit: effective_limit },
            ..Default::default()
        },
    );
    let items = found
        .items
        .iter()
        .map(|file| FindItem {
            relative_path: file.relative_path(picker),
            annotation: annotation_data(file),
        })
        .collect::<Vec<_>>();
    let scores = found.scores.iter().map(|score| score.total).collect::<Vec<_>>();
    let (mut output, weak, shown_count) = format::format_find(
        &root.path.to_string_lossy(),
        &items,
        &scores,
        effective_limit,
        pattern,
    );
    let shown = page * effective_limit + found.items.len();
    if !weak && found.items.len() >= effective_limit && found.total_matched > shown {
        let token = lock(&state.find_cursors).put(FindCursor {
            query: effective_query,
            pattern: pattern.to_string(),
            limit: effective_limit,
            page: page + 1,
            root_id: root.id.clone(),
        });
        output.push_str(&format!("\n\n[More available. cursor=\"{token}\"]"));
    } else if weak && shown_count > 0 {
        output.push_str(&format!(
            "\n\n[Weak fuzzy matches; output capped at {shown_count}/{}]",
            found.total_matched
        ));
    }
    Ok(output)
}

async fn grep(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response<Body> {
    let params = Params::parse(raw.as_deref());
    if let Some(response) = forward_request(&state, &params, "/grep").await {
        return response;
    }
    respond_text(grep_inner(&state, params))
}

fn grep_inner(state: &AppState, params: Params) -> Result<String, String> {
    params.reject_unknown(&["path", "pattern", "exclude", "case", "mode", "context", "limit", "cursor"])?;
    let requested = params.required("path")?;
    let pattern = params.required("pattern")?;
    if is_invalid_pattern(pattern.trim()) {
        return Err("INVALID_PATTERN: grep needs a concrete substring or identifier".into());
    }
    let mode = match params.first("mode").unwrap_or("plain") {
        "plain" => GrepMode::PlainText,
        "regex" => GrepMode::Regex,
        "fuzzy" => GrepMode::Fuzzy,
        _ => return Err("INVALID_PARAM: mode must be one of plain, regex, fuzzy".into()),
    };
    let context = params.integer("context", 0, 0, 20)?;
    let limit = params.integer("limit", 20, 1, 200)?;
    let case_sensitive = matches!(params.first("case"), Some("1" | "true"));
    let roots = state.roots.iter().map(Arc::as_ref).collect::<Vec<_>>();
    let (root, rel) = resolve_root(&roots, requested)?;
    root.ensure_ready()?;
    let cursor_token = params.first("cursor");
    let resumed = lock(&state.grep_cursors).get(cursor_token);
    if resumed.as_ref().is_some_and(|cursor| cursor.root_id != root.id) {
        return Err("INVALID_CURSOR: cursor belongs to a different index".into());
    }
    let excludes = params.all_owned("exclude");
    let built = query::build_query(rel.as_deref(), pattern, &excludes, &root.path)?;
    let (mut output, next_offset) = run_grep(root, &built, mode, !case_sensitive, context, limit, resumed.as_ref().map_or(0, |cursor| cursor.file_offset))?;
    if output == "No matches found" && cursor_token.is_none() && mode != GrepMode::Regex {
        let (fuzzy, fuzzy_next) = run_grep(root, &built, GrepMode::Fuzzy, !case_sensitive, 0, limit, 0)?;
        if fuzzy != "No matches found" {
            output = format!("[0 exact matches; fuzzy fallback]\n{fuzzy}");
            if fuzzy_next > 0 {
                let token = lock(&state.grep_cursors).put(GrepCursor { file_offset: fuzzy_next, root_id: root.id.clone() });
                output.push_str(&format!("\n\n[Continue with cursor=\"{token}\"]"));
            }
            return Ok(output);
        }
    }
    if next_offset > 0 {
        let token = lock(&state.grep_cursors).put(GrepCursor { file_offset: next_offset, root_id: root.id.clone() });
        output.push_str(&format!("\n\n[Continue with cursor=\"{token}\"]"));
    }
    Ok(output)
}

fn run_grep(
    root: &RootRuntime,
    query_text: &str,
    mode: GrepMode,
    smart_case: bool,
    context: usize,
    limit: usize,
    file_offset: usize,
) -> Result<(String, usize), String> {
    let parser = QueryParser::new(AiGrepConfig);
    let parsed = parser.parse(query_text);
    let picker_guard = root.picker.read().map_err(|error| error.to_string())?;
    let picker = picker_guard.as_ref().ok_or_else(|| "picker unavailable".to_string())?;
    let result = picker.grep(
        &parsed,
        &GrepSearchOptions {
            max_matches_per_file: limit.min(50),
            smart_case,
            file_offset,
            page_limit: limit,
            mode,
            before_context: context,
            after_context: context,
            classify_definitions: true,
            trim_whitespace: false,
            ..Default::default()
        },
    );
    let lines = grep_lines(picker, &result);
    Ok((format::format_grep(&root.path.to_string_lossy(), &lines), result.next_file_offset))
}

async fn multigrep(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response<Body> {
    let params = Params::parse(raw.as_deref());
    if let Some(response) = forward_request(&state, &params, "/multigrep").await {
        return response;
    }
    respond_text(multigrep_inner(&state, params))
}

fn multigrep_inner(state: &AppState, params: Params) -> Result<String, String> {
    params.reject_unknown(&["path", "pattern", "exclude", "constraint", "context", "limit", "cursor"])?;
    let requested = params.required("path")?;
    let patterns = params.all("pattern");
    if patterns.is_empty() || patterns.iter().any(|pattern| pattern.is_empty()) {
        return Err("INVALID_PARAM: missing required parameter: pattern".into());
    }
    let context = params.integer("context", 0, 0, 20)?;
    let limit = params.integer("limit", 20, 1, 200)?;
    let roots = state.roots.iter().map(Arc::as_ref).collect::<Vec<_>>();
    let (root, rel) = resolve_root(&roots, requested)?;
    root.ensure_ready()?;
    let cursor_token = params.first("cursor");
    let resumed = lock(&state.grep_cursors).get(cursor_token);
    if resumed.as_ref().is_some_and(|cursor| cursor.root_id != root.id) {
        return Err("INVALID_CURSOR: cursor belongs to a different index".into());
    }
    let excludes = params.all_owned("exclude");
    let constraints_text = query::build_constraints(rel.as_deref(), &excludes, &params.all_owned("constraint"), &root.path)?.unwrap_or_default();
    let parser = QueryParser::new(AiGrepConfig);
    let parsed_constraints = parser.parse(&constraints_text);
    let picker_guard = root.picker.read().map_err(|error| error.to_string())?;
    let picker = picker_guard.as_ref().ok_or_else(|| "picker unavailable".to_string())?;
    let result = picker.multi_grep(
        &patterns,
        &parsed_constraints.constraints,
        &GrepSearchOptions {
            max_matches_per_file: limit.min(50),
            smart_case: true,
            file_offset: resumed.as_ref().map_or(0, |cursor| cursor.file_offset),
            page_limit: limit,
            before_context: context,
            after_context: context,
            trim_whitespace: false,
            ..Default::default()
        },
    );
    let mut output = format::format_grep(&root.path.to_string_lossy(), &grep_lines(picker, &result));
    if result.next_file_offset > 0 {
        let token = lock(&state.grep_cursors).put(GrepCursor { file_offset: result.next_file_offset, root_id: root.id.clone() });
        output.push_str(&format!("\n\n[Continue with cursor=\"{token}\"]"));
    }
    Ok(output)
}

async fn rescan(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response<Body> {
    let params = Params::parse(raw.as_deref());
    if let Some(response) = forward_request(&state, &params, "/rescan").await {
        return response;
    }
    respond_text(rescan_inner(&state, params))
}

fn rescan_inner(state: &AppState, params: Params) -> Result<String, String> {
    params.reject_unknown(&["path"])?;
    let requested = params.first("path").filter(|value| !value.is_empty());
    let mut triggered = Vec::new();
    if let Some(requested) = requested {
        let roots = state.roots.iter().map(Arc::as_ref).collect::<Vec<_>>();
        let (root, _) = resolve_root(&roots, requested)?;
        root.trigger_rescan()?;
        triggered.push(root.id.clone());
    } else {
        for root in state.roots.iter() {
            if root.state() != crate::roots::RootState::Unhealthy
                && root.picker.read().ok().is_some_and(|guard| guard.is_some())
            {
                root.trigger_rescan()?;
                triggered.push(root.id.clone());
            }
        }
    }
    Ok(format!("Rescan triggered: {}", triggered.join(", ")))
}

async fn healthz(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response<Body> {
    let params = Params::parse(raw.as_deref());
    let result = params.reject_unknown(&[]).and_then(|_| serde_json::to_string(&snapshot::health(&state.roots)).map_err(|error| error.to_string()));
    respond_json(result)
}

async fn complete_handler(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response<Body> {
    let params = Params::parse(raw.as_deref());
    if let Some(response) = forward_request(&state, &params, "/complete").await {
        return response;
    }
    let result = (|| {
        params.reject_unknown(&["path", "query", "limit"])?;
        let path = params.required("path")?;
        let query = params.first("query").unwrap_or("");
        let limit = params.integer("limit", 20, 1, 50)?;
        serde_json::to_string(&complete::complete(&state.roots, path, query, limit)).map_err(|error| error.to_string())
    })();
    respond_json(result)
}

async fn not_found() -> Response<Body> {
    text_response(StatusCode::NOT_FOUND, "not found")
}

fn grep_lines(picker: &fff::FilePicker, result: &fff::GrepResult<'_>) -> Vec<GrepLine> {
    result.matches.iter().map(|item| {
        let file = result.files[item.file_index];
        GrepLine {
            relative_path: file.relative_path(picker),
            annotation: annotation_data(file),
            line_number: item.line_number,
            line_content: item.line_content.clone(),
            context_before: item.context_before.clone(),
            context_after: item.context_after.clone(),
        }
    }).collect()
}

fn annotation_data(file: &fff::FileItem) -> AnnotationData {
    AnnotationData {
        git_status: Some(format_git_status(file.git_status).to_string()),
        total_frecency_score: file.total_frecency_score(),
        access_frecency_score: file.access_frecency_score as i32,
    }
}

fn error_status(message: &str) -> StatusCode {
    if message.starts_with("INVALID_") {
        StatusCode::BAD_REQUEST
    } else if message.starts_with("UNINDEXED_PATH") {
        StatusCode::NOT_FOUND
    } else if message.starts_with("INDEX_NOT_READY") || message.starts_with("INDEX_UNHEALTHY") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

fn respond_text(result: Result<String, String>) -> Response<Body> {
    match result {
        Ok(body) => text_response(StatusCode::OK, &body),
        Err(error) => text_response(error_status(&error), &error),
    }
}

fn respond_json(result: Result<String, String>) -> Response<Body> {
    match result {
        Ok(body) => Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE, "application/json").body(Body::from(body)).unwrap(),
        Err(error) => text_response(error_status(&error), &error),
    }
}

fn text_response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("{body}\n")))
        .unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_invalid_pattern(pattern: &str) -> bool {
    if pattern.is_empty() || pattern.chars().all(|ch| ch.is_whitespace() || matches!(ch, '.' | '^' | '$')) {
        return true;
    }
    matches!(pattern, "." | "*" | "?" | ".*" | ".*?" | ".*+" | ".+" | ".+?" | "+")
        || pattern.chars().all(|ch| matches!(ch, '.' | '^' | '$' | '*' | '+' | '?'))
}

#[derive(Clone, Debug, Default)]
struct Params(Vec<(String, String)>);

impl Params {
    fn parse(raw: Option<&str>) -> Self {
        let mut values = Vec::new();
        if let Some(raw) = raw {
            for pair in raw.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let mut split = pair.splitn(2, '=');
                values.push((decode(split.next().unwrap_or_default()), decode(split.next().unwrap_or_default())));
            }
        }
        Self(values)
    }

    fn reject_unknown(&self, allowed: &[&str]) -> Result<(), String> {
        let set = allowed.iter().copied().collect::<HashSet<_>>();
        for (key, _) in &self.0 {
            if !set.contains(key.as_str()) {
                return Err(format!("INVALID_PARAM: unknown parameter {key}; allowed: {}", allowed.join(", ")));
            }
        }
        Ok(())
    }

    fn first(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.first(name).filter(|value| !value.is_empty()).ok_or_else(|| format!("INVALID_PARAM: missing required parameter: {name}"))
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.0.iter().filter(|(key, _)| key == name).map(|(_, value)| value.as_str()).collect()
    }

    fn all_owned(&self, name: &str) -> Vec<String> {
        self.all(name).into_iter().map(str::to_string).collect()
    }

    /// Rebuild the query string for the upstream hop: identical pairs in
    /// identical order, except every `path` value becomes the rewritten remote
    /// path. Values are percent-encoded so upstream decoding round-trips.
    fn encode_with_path(&self, path: &str) -> String {
        self.0
            .iter()
            .map(|(key, value)| {
                let value = if key == "path" { path } else { value.as_str() };
                format!("{}={}", encode(key), encode(value))
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    fn integer(&self, name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
        let Some(raw) = self.first(name) else { return Ok(default) };
        let value = raw.parse::<usize>().map_err(|_| format!("INVALID_PARAM: {name} must be an integer in [{min}, {max}]"))?;
        if !(min..=max).contains(&value) {
            return Err(format!("INVALID_PARAM: {name} must be an integer in [{min}, {max}]"));
        }
        Ok(value)
    }
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    output.push(high * 16 + low);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => output.push(byte as char),
            other => output.push_str(&format!("%{other:02X}")),
        }
    }
    output
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_errors_are_exact() {
        let params = Params::parse(Some("bad=1"));
        assert_eq!(params.reject_unknown(&["path"]).unwrap_err(), "INVALID_PARAM: unknown parameter bad; allowed: path");
        assert_eq!(Params::default().required("path").unwrap_err(), "INVALID_PARAM: missing required parameter: path");
        assert_eq!(Params::parse(Some("limit=0")).integer("limit", 20, 1, 200).unwrap_err(), "INVALID_PARAM: limit must be an integer in [1, 200]");
    }

    #[test]
    fn decoding_matches_url_query_rules() {
        let params = Params::parse(Some("pattern=hello+world&pattern=a%2Fb"));
        assert_eq!(params.all("pattern"), vec!["hello world", "a/b"]);
    }

    #[test]
    fn forwarded_query_reencodes_with_rewritten_path() {
        let params = Params::parse(Some("path=%2Fmnt%2Fc%2Fmy+docs&pattern=a+b&pattern=c%5C&limit=5"));
        let encoded = params.encode_with_path("C:\\my docs");
        assert_eq!(encoded, "path=C%3A%5Cmy%20docs&pattern=a%20b&pattern=c%5C&limit=5");
        // Round-trips through the same decoder the upstream daemon uses.
        let reparsed = Params::parse(Some(&encoded));
        assert_eq!(reparsed.first("path"), Some("C:\\my docs"));
        assert_eq!(reparsed.all("pattern"), vec!["a b", "c\\"]);
    }
}
