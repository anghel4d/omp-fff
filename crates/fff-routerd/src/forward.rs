//! Forwarding roots: a root whose queries are proxied to another fff-routerd
//! (typically the Windows-side daemon over loopback TCP) instead of a local
//! picker. Request paths are rewritten local->remote before proxying and
//! absolute paths in the response body are rewritten remote->local, so callers
//! only ever see paths under the local root. The HTTP client is a hand-rolled
//! HTTP/1.1 GET/POST over `tokio::net::TcpStream` — plain http, no TLS, no
//! extra dependencies.

use crate::roots::{RootRuntime, RootState};
use serde_json::Value;
use std::io::ErrorKind;
use std::time::Duration;
use tokio::net::TcpStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Forwarded data requests must outlast the upstream's own 10s ensure_ready
/// wait plus a realistic cold-cache grep over a large NTFS tree, while staying
/// under the fff CLI's curl -m 120; a timeout here is a per-request failure,
/// never proof the upstream daemon is down.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// Health probes hit /healthz, which answers instantly on a live daemon, so a
/// short timeout is enough and a slow answer genuinely means "wedged".
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Transport failure on the upstream hop, split by phase so callers can tell
/// "daemon is down" apart from "this one request failed".
#[derive(Debug)]
pub enum ForwardError {
    /// The TCP connection could not be established (refused, connect timeout):
    /// the upstream daemon is unreachable and the root may be marked unhealthy.
    Connect(String),
    /// The connection succeeded but the request failed or timed out (upstream
    /// still scanning, slow grep): a per-request problem that must not flip
    /// the root's probed health state.
    Request(String),
}

impl ForwardError {
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Connect(_))
    }
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(message) | Self::Request(message) => formatter.write_str(message),
        }
    }
}

/// Validated `"forward"` entry from roots.json:
/// `{ "url": "http://host:port", "remotePrefix": "C:\\" }`.
#[derive(Debug)]
pub struct ForwardTarget {
    /// host:port of the upstream daemon, extracted from the http:// url.
    pub authority: String,
    /// Absolute path prefix on the upstream side that maps to the local root.
    pub remote_prefix: String,
}

impl ForwardTarget {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or_else(|| "forward must be an object".to_string())?;
        for key in object.keys() {
            if key != "url" && key != "remotePrefix" {
                return Err(format!("unknown forward key: {key}"));
            }
        }
        let url = object.get("url").and_then(Value::as_str).unwrap_or_default();
        if url.is_empty() {
            return Err("forward.url must be a non-empty string".into());
        }
        let authority = url
            .strip_prefix("http://")
            .map(|rest| rest.trim_end_matches('/'))
            .ok_or_else(|| format!("forward.url must be http://host:port (no TLS): {url}"))?;
        let valid = authority
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && !host.contains('/') && port.parse::<u16>().is_ok());
        if !valid {
            return Err(format!("forward.url must be http://host:port (no TLS): {url}"));
        }
        let remote_prefix = object.get("remotePrefix").and_then(Value::as_str).unwrap_or_default();
        if remote_prefix.is_empty() {
            return Err("forward.remotePrefix must be a non-empty string".into());
        }
        let absolute = if windows_style_prefix(remote_prefix) {
            remote_prefix.as_bytes().get(2).copied().is_some_and(is_sep)
        } else {
            remote_prefix.starts_with('/')
        };
        if !absolute {
            return Err(format!("forward.remotePrefix must be absolute: {remote_prefix}"));
        }
        Ok(Self { authority: authority.to_string(), remote_prefix: remote_prefix.to_string() })
    }

    /// Map a root-relative request path ('/'-separated, as produced by root
    /// matching) onto the upstream prefix. Windows-style prefixes (`C:\...`)
    /// join with `\` and convert the remaining separators.
    pub fn to_remote(&self, rel: Option<&str>) -> String {
        let Some(rel) = rel else {
            return self.remote_prefix.clone();
        };
        if windows_style_prefix(&self.remote_prefix) {
            format!("{}\\{}", self.remote_prefix.trim_end_matches(['\\', '/']), rel.replace('/', "\\"))
        } else {
            format!("{}/{rel}", self.remote_prefix.trim_end_matches('/'))
        }
    }

    /// Rewrite absolute upstream paths embedded in a response body back onto
    /// the local root (the reverse of `to_remote`, including `\` -> `/`).
    pub fn rewrite_to_local(&self, body: &str, local_root: &str) -> String {
        rewrite_remote_to_local(body, &self.remote_prefix, local_root)
    }
}

fn is_sep(byte: u8) -> bool {
    byte == b'/' || byte == b'\\'
}

/// `C:`-style drive prefix, per the `^[A-Za-z]:` convention.
fn windows_style_prefix(prefix: &str) -> bool {
    let bytes = prefix.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Textual remote->local rewrite over a whole response body. Rewrites are
/// anchored to line starts: grep group headers and find list lines always
/// begin at column 0 while match/context lines begin with a space, so
/// verbatim file CONTENT that merely mentions the remote prefix (e.g. a
/// source line containing `C:\Windows\System32`) is never touched. The one
/// sanctioned mid-line site is the upstream UNINDEXED_PATH "Indexed roots: "
/// listing, whose tail consists solely of paths. A matched occurrence of
/// `remote_prefix` (separator- and case-insensitive for Windows-style
/// prefixes) at a path boundary is replaced with `local_root`; for
/// Windows-style prefixes the rest of that line has `\` converted to `/` so
/// nested remote paths come out '/'-separated.
pub fn rewrite_remote_to_local(body: &str, remote_prefix: &str, local_root: &str) -> String {
    const INDEXED_ROOTS: &[u8] = b"Indexed roots: ";
    let windows = windows_style_prefix(remote_prefix);
    let prefix = remote_prefix.as_bytes();
    let prefix_ends_with_sep = prefix.last().copied().is_some_and(is_sep);
    let bytes = body.as_bytes();
    let local = local_root.trim_end_matches(['/', '\\']);
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut convert_line = false;
    let mut line_start = 0usize;
    // Line consists solely of paths after "Indexed roots: " => mid-line ok.
    let mut paths_line = false;
    let mut index = 0;
    while index < bytes.len() {
        if index == line_start {
            paths_line = bytes[line_start..].starts_with(INDEXED_ROOTS);
        }
        let anchored = index == line_start || paths_line;
        if anchored
            && prefix_matches_at(bytes, index, prefix, windows)
            && boundary_ok(bytes, index + prefix.len(), prefix_ends_with_sep)
        {
            out.extend_from_slice(local.as_bytes());
            index += prefix.len();
            if prefix_ends_with_sep {
                // The prefix carried its own trailing separator (e.g. "C:\");
                // collapse any duplicate and re-emit exactly one '/' when the
                // path continues past the root.
                if bytes.get(index).copied().is_some_and(is_sep) {
                    index += 1;
                }
                let continues = bytes
                    .get(index)
                    .is_some_and(|byte| !matches!(byte, b'\n' | b'\r' | b' ' | b'\t' | b'"' | b',' | b']' | b')'));
                if continues {
                    out.push(b'/');
                }
            }
            convert_line = windows;
            continue;
        }
        let byte = bytes[index];
        if byte == b'\n' {
            convert_line = false;
            line_start = index + 1;
        }
        out.push(if convert_line && byte == b'\\' { b'/' } else { byte });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn prefix_matches_at(bytes: &[u8], at: usize, prefix: &[u8], windows: bool) -> bool {
    if at + prefix.len() > bytes.len() {
        return false;
    }
    prefix.iter().zip(&bytes[at..]).all(|(expected, actual)| {
        if windows && is_sep(*expected) {
            is_sep(*actual)
        } else if windows {
            expected.eq_ignore_ascii_case(actual)
        } else {
            expected == actual
        }
    })
}

/// A prefix without its own trailing separator must be followed by a
/// separator or a non-path-continuing character, so `C:\Users\Pyrus` never
/// rewrites inside `C:\Users\PyrusOther`.
fn boundary_ok(bytes: &[u8], end: usize, prefix_ends_with_sep: bool) -> bool {
    if prefix_ends_with_sep {
        return true;
    }
    match bytes.get(end) {
        None => true,
        Some(byte) if is_sep(*byte) => true,
        Some(byte) => !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
    }
}

/// Probe the upstream daemon's /healthz and fold the result into the forward
/// root's status: unreachable => unhealthy with a clear message, reachable =>
/// state/files/lastScanAt of the upstream root(s) covering the remote prefix.
/// Never panics and never blocks sibling roots; bounded by the client timeouts.
pub async fn probe_and_apply(root: &RootRuntime) {
    let Some(forward) = root.forward.as_ref() else { return };
    match http_request_with(&forward.authority, "GET", "/healthz", PROBE_TIMEOUT).await {
        Ok((200, _, body)) => apply_upstream_health(root, forward, &body),
        Ok((status, _, _)) => root.set_unhealthy(format!("forward upstream returned status {status} for /healthz")),
        Err(error) => root.set_unhealthy(format!("forward upstream unreachable: {error}")),
    }
}

fn apply_upstream_health(root: &RootRuntime, forward: &ForwardTarget, body: &str) {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(body) else {
        root.set_unhealthy("forward upstream returned invalid health JSON".into());
        return;
    };
    let prefix = normalize_for_match(&forward.remote_prefix);
    let matching = rows
        .iter()
        .filter(|row| {
            row.get("path").and_then(Value::as_str).is_some_and(|path| {
                let row_path = normalize_for_match(path);
                prefix.starts_with(&row_path) || row_path.starts_with(&prefix)
            })
        })
        .collect::<Vec<_>>();
    let Some(first) = matching.first() else {
        root.set_unhealthy(format!("forward upstream has no root covering {}", forward.remote_prefix));
        return;
    };
    let files = matching
        .iter()
        .filter_map(|row| row.get("files").and_then(Value::as_u64))
        .sum::<u64>() as usize;
    let chosen = matching
        .iter()
        .find(|row| row.get("state").and_then(Value::as_str) == Some("ready"))
        .unwrap_or(first);
    let last_scan_at = chosen.get("lastScanAt").and_then(Value::as_str).map(str::to_string);
    let state = match chosen.get("state").and_then(Value::as_str) {
        Some("ready") => RootState::Ready,
        Some("scanning") => RootState::Scanning,
        Some("unhealthy") => RootState::Unhealthy,
        _ => RootState::Starting,
    };
    let error = (state == RootState::Unhealthy).then(|| {
        format!(
            "forward upstream root unhealthy: {}",
            chosen.get("error").and_then(Value::as_str).unwrap_or("unknown error")
        )
    });
    root.set_forward_status(state, last_scan_at, error, files);
}

/// Separator/case-normalized comparison key for matching the remote prefix
/// against upstream root paths (Windows paths are case-insensitive).
fn normalize_for_match(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase()
}

/// Minimal HTTP/1.1 client for the upstream hop: single request, Host +
/// Connection: close, honors Content-Length (reads to EOF otherwise). Returns
/// (status, content-type, body); transport problems come back as Err, split
/// into connect-phase and request-phase failures.
pub async fn http_request(authority: &str, method: &str, target: &str) -> Result<(u16, Option<String>, String), ForwardError> {
    http_request_with(authority, method, target, REQUEST_TIMEOUT).await
}

async fn http_request_with(
    authority: &str,
    method: &str,
    target: &str,
    timeout: Duration,
) -> Result<(u16, Option<String>, String), ForwardError> {
    tokio::time::timeout(timeout, request_inner(authority, method, target))
        .await
        .map_err(|_| ForwardError::Request(format!("request to {authority} timed out")))?
}

async fn request_inner(authority: &str, method: &str, target: &str) -> Result<(u16, Option<String>, String), ForwardError> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(authority))
        .await
        .map_err(|_| ForwardError::Connect(format!("connect to {authority} timed out")))?
        .map_err(|error| ForwardError::Connect(format!("connect to {authority} failed: {error}")))?;
    let body_header = if method == "POST" { "Content-Length: 0\r\n" } else { "" };
    let request = format!("{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n{body_header}\r\n");
    write_all(&stream, request.as_bytes())
        .await
        .map_err(|error| ForwardError::Request(format!("write to {authority} failed: {error}")))?;
    let mut buffer: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position;
        }
        let count = read_some(&stream, &mut chunk)
            .await
            .map_err(|error| ForwardError::Request(format!("read from {authority} failed: {error}")))?;
        if count == 0 {
            return Err(ForwardError::Request(format!("connection to {authority} closed before response headers")));
        }
        buffer.extend_from_slice(&chunk[..count]);
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| ForwardError::Request(format!("malformed status line from {authority}: {status_line}")))?;
    let mut content_length: Option<usize> = None;
    let mut content_type: Option<String> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_string());
        }
    }
    let mut body = buffer[header_end + 4..].to_vec();
    if let Some(length) = content_length {
        while body.len() < length {
            let count = read_some(&stream, &mut chunk)
                .await
                .map_err(|error| ForwardError::Request(format!("read from {authority} failed: {error}")))?;
            if count == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..count]);
        }
        body.truncate(length);
    } else {
        // Connection: close means EOF terminates the body.
        loop {
            let count = read_some(&stream, &mut chunk)
                .await
                .map_err(|error| ForwardError::Request(format!("read from {authority} failed: {error}")))?;
            if count == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..count]);
        }
    }
    Ok((status, content_type, String::from_utf8_lossy(&body).into_owned()))
}

/// Readiness-loop write; tokio's io-util extension traits are not enabled, so
/// the stream's inherent try_read/try_write API is used instead.
async fn write_all(stream: &TcpStream, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        stream.writable().await?;
        match stream.try_write(data) {
            Ok(count) => data = &data[count..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Readiness-loop read of at least one byte; `Ok(0)` is EOF.
async fn read_some(stream: &TcpStream, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        stream.readable().await?;
        match stream.try_read(buffer) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(json: &str) -> Result<ForwardTarget, String> {
        ForwardTarget::from_value(&serde_json::from_str(json).unwrap())
    }

    fn win_c() -> ForwardTarget {
        target(r#"{"url": "http://127.0.0.1:47997", "remotePrefix": "C:\\"}"#).unwrap()
    }

    #[test]
    fn forward_config_parses_and_validates() {
        let ok = win_c();
        assert_eq!(ok.authority, "127.0.0.1:47997");
        assert_eq!(ok.remote_prefix, "C:\\");
        assert_eq!(target(r#"{"url": "http://host:1/", "remotePrefix": "/data"}"#).unwrap().authority, "host:1");
        assert!(target(r#"{"url": "", "remotePrefix": "C:\\"}"#).unwrap_err().contains("forward.url"));
        assert!(target(r#"{"remotePrefix": "C:\\"}"#).unwrap_err().contains("forward.url"));
        assert!(target(r#"{"url": "https://host:1", "remotePrefix": "C:\\"}"#).unwrap_err().contains("no TLS"));
        assert!(target(r#"{"url": "http://host:1/extra", "remotePrefix": "C:\\"}"#).unwrap_err().contains("no TLS"));
        assert!(target(r#"{"url": "http://host", "remotePrefix": "C:\\"}"#).unwrap_err().contains("host:port"));
        assert!(target(r#"{"url": "http://host:1", "remotePrefix": ""}"#).unwrap_err().contains("remotePrefix"));
        assert!(target(r#"{"url": "http://host:1", "remotePrefix": "relative"}"#).unwrap_err().contains("absolute"));
        assert!(target(r#"{"url": "http://host:1", "remotePrefix": "C:"}"#).unwrap_err().contains("absolute"));
        assert!(target(r#"{"url": "http://host:1", "remotePrefix": "/d", "bogus": 1}"#).unwrap_err().contains("unknown forward key"));
        assert!(target(r#""not an object""#).unwrap_err().contains("object"));
    }

    #[test]
    fn to_remote_windows_prefix() {
        let forward = win_c();
        assert_eq!(forward.to_remote(None), "C:\\");
        assert_eq!(forward.to_remote(Some("Users")), "C:\\Users");
        assert_eq!(
            forward.to_remote(Some("Users/Pyrus/my docs/nested dir/file name.txt")),
            "C:\\Users\\Pyrus\\my docs\\nested dir\\file name.txt"
        );
    }

    #[test]
    fn to_remote_unix_prefix_passthrough() {
        let forward = target(r#"{"url": "http://host:1", "remotePrefix": "/data"}"#).unwrap();
        assert_eq!(forward.to_remote(None), "/data");
        assert_eq!(forward.to_remote(Some("foo/bar baz.txt")), "/data/foo/bar baz.txt");
    }

    #[test]
    fn rewrite_windows_paths_to_local() {
        let forward = win_c();
        assert_eq!(
            forward.rewrite_to_local("C:\\Users\\Pyrus\\my docs\\file.txt", "/mnt/c"),
            "/mnt/c/Users/Pyrus/my docs/file.txt"
        );
        // Upstream format joins root and relative with '/', so mixed separators appear.
        assert_eq!(
            forward.rewrite_to_local("C:\\/Users/Pyrus/proj/src/main.rs", "/mnt/c"),
            "/mnt/c/Users/Pyrus/proj/src/main.rs"
        );
        // Bare drive root does not grow a trailing slash mid-sentence.
        assert_eq!(forward.rewrite_to_local("Indexed roots: C:\\\n", "/mnt/c"), "Indexed roots: /mnt/c\n");
    }

    #[test]
    fn rewrite_respects_path_boundaries() {
        let forward = target(r#"{"url": "http://host:1", "remotePrefix": "C:\\Users\\Pyrus"}"#).unwrap();
        assert_eq!(
            forward.rewrite_to_local("C:\\Users\\Pyrus\\proj\\a.rs", "/mnt/c/Users/Pyrus"),
            "/mnt/c/Users/Pyrus/proj/a.rs"
        );
        assert_eq!(
            forward.rewrite_to_local("C:\\Users\\Pyrus [often touched]", "/mnt/c/Users/Pyrus"),
            "/mnt/c/Users/Pyrus [often touched]"
        );
        // A longer sibling directory must not be rewritten.
        assert_eq!(
            forward.rewrite_to_local("C:\\Users\\PyrusOther\\x", "/mnt/c/Users/Pyrus"),
            "C:\\Users\\PyrusOther\\x"
        );
    }

    #[test]
    fn rewrite_unix_prefix_passthrough() {
        let forward = target(r#"{"url": "http://host:1", "remotePrefix": "/data"}"#).unwrap();
        assert_eq!(forward.rewrite_to_local("/data/foo/bar.txt\n", "/srv/data"), "/srv/data/foo/bar.txt\n");
        // No boundary => untouched, and case stays significant for unix prefixes.
        assert_eq!(forward.rewrite_to_local("/database/x\n", "/srv/data"), "/database/x\n");
        assert_eq!(forward.rewrite_to_local("/DATA/x\n", "/srv/data"), "/DATA/x\n");
    }

    #[test]
    fn rewrite_realistic_grep_payload() {
        let forward = win_c();
        let body = concat!(
            "C:\\/proj/src/main.rs [modified in git]\n",
            " 10: fn main() {\n",
            " 11:     let path = \"unrelated\\\\thing\";\n",
            "\n",
            "C:\\/proj/my docs/notes with spaces.md\n",
            "  1: # notes\n",
            "\n",
            "[Continue with cursor=\"3\"]\n",
        );
        let expected = concat!(
            "/mnt/c/proj/src/main.rs [modified in git]\n",
            " 10: fn main() {\n",
            " 11:     let path = \"unrelated\\\\thing\";\n",
            "\n",
            "/mnt/c/proj/my docs/notes with spaces.md\n",
            "  1: # notes\n",
            "\n",
            "[Continue with cursor=\"3\"]\n",
        );
        assert_eq!(forward.rewrite_to_local(body, "/mnt/c"), expected);
    }

    #[test]
    fn rewrite_leaves_content_lines_untouched() {
        let forward = win_c();
        // Match/context lines begin with a space; a remote prefix appearing in
        // verbatim file content must never be rewritten, and no backslash on
        // such a line may be converted.
        let body = concat!(
            "C:\\/proj/src/env.rs\n",
            " 10:     let path = \"C:\\Windows\\System32\\cmd.exe\";\n",
            " 11- // see C:\\proj\\readme.md and a lone \\ backslash\n",
        );
        let expected = concat!(
            "/mnt/c/proj/src/env.rs\n",
            " 10:     let path = \"C:\\Windows\\System32\\cmd.exe\";\n",
            " 11- // see C:\\proj\\readme.md and a lone \\ backslash\n",
        );
        assert_eq!(forward.rewrite_to_local(body, "/mnt/c"), expected);
        // Mid-line occurrences outside the "Indexed roots: " listing stay put
        // even when the line is not a match line.
        assert_eq!(
            forward.rewrite_to_local("note that C:\\Users is remote\n", "/mnt/c"),
            "note that C:\\Users is remote\n"
        );
    }

    #[test]
    fn rewrite_indexed_roots_listing_rewrites_mid_line() {
        let forward = win_c();
        assert_eq!(
            forward.rewrite_to_local("Indexed roots: C:\\Users\\Pyrus, C:\\proj\n", "/mnt/c"),
            "Indexed roots: /mnt/c/Users/Pyrus, /mnt/c/proj\n"
        );
    }

    #[test]
    fn rewrite_round_trips_to_remote() {
        let forward = win_c();
        let remote = forward.to_remote(Some("sub dir/inner/file name.txt"));
        assert_eq!(forward.rewrite_to_local(&remote, "/mnt/c"), "/mnt/c/sub dir/inner/file name.txt");
    }
}
