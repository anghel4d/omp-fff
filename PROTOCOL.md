# fff-router protocol

This document is the normative boundary between `fff-routerd` and every client. Unless explicitly marked as a new endpoint or parameter, behavior is transcribed from the deployed Bun router that this daemon replaces.

## Filesystem contract

- Runtime directory: `${FFF_ROUTER_RUNTIME_DIR}` when set; otherwise `${XDG_RUNTIME_DIR:-/run/user/<uid>}/fff-router`.
- Socket: `<runtime-dir>/fff.sock`.
- Health snapshot: `<runtime-dir>/health.json`.
- Roots configuration: `${FFF_ROUTER_CONFIG:-~/.config/fff-router/roots.json}`. The file is a JSON array of `{ "id": string, "path": string }`; an entry may additionally carry a `"forward"` object as specified under [Forwarding roots](#forwarding-roots).
- State directory: `${FFF_ROUTER_STATE_DIR:-~/.local/state/fff-router}`.
- Per-root state: `<state-dir>/<id>.frecency.db` and `<state-dir>/<id>.history.db`.

On Windows every default hangs off `%LOCALAPPDATA%\fff-router` instead: roots configuration at `<base>\roots.json`, runtime directory `<base>\run`, state directory `<base>\state`. The `FFF_ROUTER_RUNTIME_DIR`, `FFF_ROUTER_CONFIG`, and `FFF_ROUTER_STATE_DIR` overrides work exactly as on unix. `LOCALAPPDATA` being unset is a process-start failure, as `HOME` is on unix.

The daemon creates the runtime directory recursively with mode `0700`, unlinks a stale socket before binding, writes the health snapshot once at startup and every two seconds, and publishes it atomically by writing `health.json.tmp` then renaming it to `health.json`. Graceful shutdown removes both `fff.sock` and `health.json`. The `0700` mode and every socket-file behavior are unix-only; in TCP mode only `health.json` exists and is removed on shutdown.

## Transport

The transport is HTTP/1.1 over the Unix domain socket. The authority used by clients is conventionally `http://fff` and is not semantically significant.

Setting `FFF_ROUTER_LISTEN` to a non-empty `host:port` value (for example `127.0.0.1:47997`) selects a TCP listener instead, on any platform. On Windows, which has no Unix domain sockets, TCP is the only mode and the listener defaults to `127.0.0.1:47997` when `FFF_ROUTER_LISTEN` is unset. On unix, behavior with `FFF_ROUTER_LISTEN` unset is byte-identical to the socket-only daemon. No endpoint carries authentication: in socket mode the `0700` runtime directory is the access control, so a TCP listener must bind loopback only.

Text endpoints return `content-type: text/plain; charset=utf-8` and append exactly one trailing newline to the handler body. JSON endpoints return JSON. Unless stated otherwise, unknown routes or wrong methods return status 404 with body `not found\n`.

Repeated query parameters preserve their input order.

## Common parameter rules

Unknown parameters are rejected before handler execution:

```text
INVALID_PARAM: unknown parameter <k>; allowed: <allowed names joined by ", ">
```

A required string is missing when its first value is absent or empty:

```text
INVALID_PARAM: missing required parameter: <name>
```

Bounded integer parameters accept only finite base-10 integer values within the inclusive range:

```text
INVALID_PARAM: <name> must be an integer in [<min>, <max>]
```

`path` values used for routing must be absolute:

```text
INVALID_PATH: path must be absolute: <p>
```

Routing resolves the requested path lexically, selects every configured root containing it, and chooses the matching root with the longest path. If no root contains the request:

```text
UNINDEXED_PATH: <p> is not under any FFF-indexed root.
Indexed roots: <configured paths joined by ", ">
Use native grep/glob/read for this path.
```

A cursor may be used only with the root that issued it:

```text
INVALID_CURSOR: cursor belongs to a different index
```

## Endpoints

### `GET /find`

Allowed parameters, in this exact order for unknown-parameter errors:

| Name | Cardinality | Required | Bounds/default | Meaning |
|---|---:|---:|---|---|
| `path` | one | yes | non-empty absolute path | Selects the longest containing indexed root and contributes a relative path constraint. |
| `query` | one | yes | non-empty | Fuzzy path query. |
| `limit` | one | no | integer 1–200, default 30 | Page size. |
| `cursor` | one | no | opaque string | Resumes a prior find page; the stored query, pattern, limit, page, and root take precedence. |

The effective engine query is `buildQuery(relativeConstraint, query, no excludes, rootPath)`. Pagination begins at page 0. A successful response is formatted by the find-output rules below.

When the page is not weak, contains at least `limit` items, and more matches remain, append:

```text

[More available. cursor="<token>"]
```

When the page is weak and at least one result is shown, append:

```text

[Weak fuzzy matches; output capped at <shown>/<totalMatched>]
```

### `GET /grep`

Allowed parameters, in this exact order:

| Name | Cardinality | Required | Bounds/default | Meaning |
|---|---:|---:|---|---|
| `path` | one | yes | non-empty absolute path | Selects the root and contributes a relative path constraint. |
| `pattern` | one | yes | non-empty | Content search pattern. |
| `exclude` | repeatable | no | empty list | Each value may contain comma- or whitespace-separated constraints. |
| `case` | one | no | sensitive only for `1` or `true`; otherwise smart-case | Case behavior. |
| `mode` | one | no | `plain`, `regex`, or `fuzzy`; default `plain` | Engine search mode. |
| `context` | one | no | integer 0–20, default 0 | Equal before/after context. |
| `limit` | one | no | integer 1–200, default 20 | Page size. |
| `cursor` | one | no | opaque string | Resumes engine file-offset state. |

Any other `mode` yields:

```text
INVALID_PARAM: mode must be one of plain, regex, fuzzy
```

After trimming, wildcard-only or otherwise non-concrete patterns matching this expression are rejected:

```regex
^(?:[.^$]*(?:[.][*+?]|\*|\+)[.^$]*|[.^$\s]*|\.\*\??|\.\*[+?]?|\.\+\??|\.|\*|\?)$
```

The error is:

```text
INVALID_PATTERN: grep needs a concrete substring or identifier
```

The engine query is `buildQuery(relativeConstraint, pattern, excludes, rootPath)`. Engine options are: selected mode; `smartCase = !caseSensitive`; `maxMatchesPerFile = min(limit, 50)`; equal before/after context; `pageSize = limit`; definition classification enabled; and the resolved cursor or null. Whitespace trimming is disabled in the engine because output formatting performs the trim.

If the first search has zero items, there is no cursor, and mode is not `regex`, retry in fuzzy mode with no context and no cursor. If that retry has items, prefix the response:

```text
[0 exact matches; fuzzy fallback]
```

If an engine continuation remains, append:

```text

[Continue with cursor="<token>"]
```

### `GET /multigrep`

Allowed parameters, in this exact order:

| Name | Cardinality | Required | Bounds/default | Meaning |
|---|---:|---:|---|---|
| `path` | one | yes | non-empty absolute path | Selects the root and contributes a relative constraint. |
| `pattern` | repeatable | yes, at least one | every value non-empty | Literal alternatives combined with OR semantics. |
| `exclude` | repeatable | no | empty list | Normalized exclusion constraints. |
| `constraint` | repeatable | no | empty list | **omp-fff addition:** every value is appended verbatim, in order, to the engine constraints string after the routed relative constraint and normalized excludes. Empty values are ignored. |
| `context` | one | no | integer 0–20, default 0 | Equal before/after context. |
| `limit` | one | no | integer 1–200, default 20 | Page size. |
| `cursor` | one | no | opaque string | Resumes engine file-offset state. |

Missing, empty, or partly empty pattern lists yield:

```text
INVALID_PARAM: missing required parameter: pattern
```

Engine options are: `patterns`; constructed constraints or none; `maxMatchesPerFile = min(limit, 50)`; `smartCase = true`; equal before/after context; `pageSize = limit`; and the resolved cursor or null. Output uses grep formatting and the same continuation footer as `/grep`.

### `POST /rescan`

Allowed parameters:

| Name | Cardinality | Required | Default | Meaning |
|---|---:|---:|---|---|
| `path` | one | no | absent | When present and non-empty, rescan only its routed root. Otherwise rescan every initialized, non-unhealthy root. |

Every triggered root enters `scanning`, records the current timestamp in `lastScanAt`, and starts an engine rescan. The response is:

```text
Rescan triggered: <root ids joined by ", ">
```

### `GET /healthz`

No parameters are accepted. Returns a JSON array in configured-root order:

```json
[
  {
    "id": "root-id",
    "path": "/absolute/root",
    "files": 123,
    "state": "starting|scanning|ready|unhealthy",
    "lastScanAt": "ISO-8601 timestamp or null",
    "error": "message or null"
  }
]
```

`files` is the current scanned/live file count when available and `0` before the picker exposes one.

### `GET /complete`

This endpoint is added by omp-fff for editor `@` completion.

Allowed parameters, in this exact order:

| Name | Cardinality | Required | Bounds/default | Meaning |
|---|---:|---:|---|---|
| `path` | one | yes | non-empty absolute path | Completion scope. |
| `query` | one | no | default empty string | Fuzzy relative-path prefix/query. |
| `limit` | one | no | integer 1–50, default 20 | Maximum items. |

Returns:

```json
{
  "items": [
    { "type": "file", "relativePath": "src/main.rs", "name": "main.rs" },
    { "type": "directory", "relativePath": "src/", "name": "src" }
  ]
}
```

When `path` is below a root, the search is scoped by prefixing the root-relative directory plus a space to the query, results outside that prefix are dropped, and the prefix is stripped from each returned `relativePath`. Paths outside configured roots return `{ "items": [] }` rather than a coverage error. Implementations without mixed file/directory search may return file items only.

## Query construction

### `normalizePathConstraint(pathConstraint, base)`

1. Trim surrounding whitespace. Empty input remains empty.
2. If absolute, rebase it relative to `base` and canonicalize separators to `/`.
   - Equal to the root becomes no constraint.
   - Outside the root throws:

      ```text
      Path constraint must be inside the indexed root: <original constraint>
      ```
3. `.` and `./` become no constraint. A leading `./` is removed.
4. `dir/**` and `dir/**/*` become `dir/` when `dir` has no glob metacharacter among `* ? [ {`.
5. Values beginning or ending with `/` are preserved.
6. Values containing `* ? [ {` are preserved verbatim.
7. A final path segment matching `\.[A-Za-z][A-Za-z0-9]{0,9}$` is treated as a filename constraint and preserved.
8. Every other value gains a trailing `/`.

### `normalizeExcludes(exclude, base)`

Each repeated value is split on commas or whitespace. Empty pieces are removed. One leading `!` is stripped, the remainder is normalized as a path constraint, and each non-empty result is prefixed with exactly one `!`.

### `buildQuery(pathConstraint, pattern, exclude, base)`

Join with one ASCII space, omitting empty pieces, in this order:

1. normalized routed relative constraint;
2. normalized `!` exclusions;
3. pattern.

### `buildConstraints(pathConstraint, exclude, base, verbatimConstraints)`

Join with one ASCII space, omitting empty pieces, in this order:

1. normalized routed relative constraint;
2. normalized `!` exclusions;
3. each non-empty `constraint` value verbatim and in request order.

Return no constraints when the resulting list is empty.

## Output formatting

### Annotation

A file header may receive at most one annotation, with this precedence:

1. When git status exists and is not `clean`, `unknown`, or empty: ` [<gitStatus> in git]`.
2. Otherwise use `totalFrecencyScore`, falling back to `accessFrecencyScore`, then zero:
   - score ≥25: ` [very often touched]`
   - score ≥20: ` [often touched]`
   - otherwise no annotation.

### Truncation

Every grep line is trimmed on both ends. At most 500 characters are retained; longer values become the first 500 characters followed by `...`.

### Grep and multigrep

No items:

```text
No matches found
```

Items are grouped in engine order. At each relative-path transition, insert a blank line unless it is the first file, then emit:

```text
<absolute path><annotation>
```

For every match:

```text
 <context-before-line-number>- <trimmed/truncated text>
 <match-line-number>: <trimmed/truncated text>
 <context-after-line-number>- <trimmed/truncated text>
```

The first before-context line number is `matchLine - beforeCount`; after-context begins at `matchLine + 1`.

### Find

No items:

```text
No files found matching pattern
```

Otherwise emit one `<absolute path><annotation>` per shown item.

A page is weak when the top result's total score, defaulting to zero, is less than:

```text
floor(pattern.length * 12 * 0.5)
```

Weak pages show at most `min(5, limit)` items. Strong pages show at most `limit`.

## Errors and statuses

Handler errors are returned as plain text with one trailing newline.

| Message prefix | HTTP status |
|---|---:|
| `INVALID_` | 400 |
| `UNINDEXED_PATH` | 404 |
| `INDEX_NOT_READY` | 503 |
| `INDEX_UNHEALTHY` | 503 |
| anything else | 500 |

`FORWARD_UPSTREAM_ERROR` responses (see [Forwarding roots](#forwarding-roots)) carry status 503 explicitly, and proxied upstream responses keep the upstream's status and content type.

Readiness errors are exact:

```text
INDEX_NOT_READY: <id> is still starting; retry shortly or use native tools.
INDEX_NOT_READY: <id> is still scanning; retry shortly or use native tools.
INDEX_UNHEALTHY: <id>: <err>
```

## Root loading and lifecycle

Root configuration failures are process-start failures with these exact messages:

```text
<config path> must contain a JSON array
Invalid root entry at index <i>
Duplicate root id: <id>
Root path must be absolute: <p>
```

A configured path that does not exist or is not a directory remains represented in health as `state: "unhealthy"`, `error: "directory does not exist"`.

Valid roots start as `starting`. Initialization is launched in configuration order, staggered two seconds apart. Each picker uses AI mode, no mmap cache, no content indexing, no symlink following, and watching disabled only when the root begins with `/mnt/`. The daemon opens each root's frecency/history database when possible, enters `scanning`, waits up to 120 seconds for the initial scan, and enters `ready` with `lastScanAt` set when scanning completes. An initialization error makes the root `unhealthy` and records the engine message.

A request to a non-ready picker waits again for up to 10 seconds. Success changes `scanning` to `ready`; timeout returns the scanning readiness error.

Every two seconds the daemon changes any `scanning` root whose engine reports no active scan to `ready`, updates `lastScanAt`, and publishes health.

Each `/mnt/` root receives a periodic rescan every 15 minutes, with timers staggered 30 seconds apart. A periodic rescan is skipped when its picker is unavailable/unhealthy or any initialized root is scanning.

On `SIGINT` or `SIGTERM`, the daemon destroys/cancels every picker, removes the socket and health snapshot, and exits successfully. On Windows only Ctrl-C is handled; the cleanup is the same, minus the socket.

## Forwarding roots

A root entry may carry a `"forward"` object, making it a forwarding root: requests routed to it are proxied to another `fff-routerd` (typically the Windows-side daemon over loopback TCP) instead of a local index.

```json
{
  "id": "win-c",
  "path": "/mnt/c",
  "forward": { "url": "http://127.0.0.1:47997", "remotePrefix": "C:\\" }
}
```

- `url` must be `http://host:port` — plain HTTP, no TLS, no path. Anything else is a process-start failure: `Invalid forward config for root <id>: <reason>`, as are unknown `forward` keys.
- `remotePrefix` is the absolute path prefix on the upstream side that maps to the local `path`. A prefix matching `^[A-Za-z]:` is Windows-style and must continue with a separator (`C:\` yes, `C:` no); otherwise it must begin with `/`.

A forwarding root has no local picker, opens no frecency/history databases, never scans, and is exempt from the local-directory existence check, the two-second scan poll, and the `/mnt/` periodic rescan timers — even when its `path` begins with `/mnt/`. The upstream daemon owns the index and its freshness.

### Request proxying

Routing selects forwarding roots by the same lexical longest-prefix match, but skips local readiness and picker checks: the proxy attempt itself doubles as the reachability probe, so an `unhealthy` forwarding root recovers on the next successful request. The request is re-sent to the upstream daemon on the same endpoint with an identical query string — identical pairs in identical order, values percent-encoded — except that every `path` value is rewritten local→remote: strip the root's `path`, join the remainder onto `remotePrefix`. Windows-style prefixes join with `\` and convert the remaining `/` separators to `\`. `/rescan` is proxied as POST; everything else as GET. Parameter validation for forwarded requests happens upstream. Cursors pass through verbatim in both directions and are never re-scoped.

The upstream response is returned with its status and content type intact, after a remote→local rewrite of absolute upstream paths in the body: occurrences of `remotePrefix` (separator- and case-insensitive when Windows-style) at a path boundary become the local root path, and on such rewritten lines `\` becomes `/`. Rewrites are anchored to line starts — grep group headers and find lines begin at column 0 — plus the `Indexed roots: ` listing mid-line; verbatim file content that merely mentions the remote prefix is never touched.

### Upstream failures

Timeouts are 2 seconds to connect, 90 seconds per forwarded request, and 10 seconds for a health probe. Failures are split by phase:

- Connect failure (refused, connect timeout): the upstream daemon is unreachable. The root becomes `unhealthy` with error `forward upstream unreachable: <detail>`, and the request fails with `INDEX_UNHEALTHY: <id>: <that message>` (status 503).
- Post-connect failure or request timeout (upstream still scanning, a slow cold grep): a per-request error that does not change the root's probed health. The request fails with status 503 and body `FORWARD_UPSTREAM_ERROR: <id>: forward upstream request failed: <detail>`.
- `/complete` keeps its no-coverage-errors contract: any upstream failure returns `{ "items": [] }`.

### Health

A forwarding root starts as `starting` and is probed instead of scanned: once at startup, and again after any successful forwarded request while not `ready`. A probe fetches the upstream `/healthz` and folds the upstream root(s) whose paths cover (or are covered by) `remotePrefix` into the local health row — `state` and `lastScanAt` from the covering root (preferring a `ready` one), `files` summed across covering roots. An unreachable upstream, a non-200 probe, invalid health JSON, or no covering upstream root all make the root `unhealthy` with a message naming the cause. The row shape in `/healthz` and `health.json` is unchanged.

## Windows daemon

The same binary builds and runs on Windows (MSVC, rustc ≥ 1.88). Differences are confined to the platform seams already described: TCP-only transport defaulting to `127.0.0.1:47997`, defaults under `%LOCALAPPDATA%\fff-router`, no socket files or `0700` mode, and Ctrl-C-only shutdown. Everything else — endpoints, parameters, errors, formatting, root lifecycle — is identical, with root paths written Windows-style (`C:\...`) in `roots.json`; emitted absolute paths keep `/`-joined relative parts, which the forwarding rewrite normalizes on the WSL side.
