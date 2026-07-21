# omp-fff

`omp-fff` is a resident multi-root file-search service plus a thin oh-my-pi interconnect.

- `fff-routerd` is a pure-Rust daemon embedding `fff-search`. It owns warm indexes, frecency/history databases, scanning, watching, pagination, formatting, and routing across indexed roots. It listens on a Unix domain socket by default and on TCP when `FFF_ROUTER_LISTEN` is set (Windows is TCP-only), and a root may be a *forwarding root* whose queries proxy to a sibling daemon with paths rewritten both ways.
- `extension/` is the only TypeScript runtime component. It registers `ffgrep`, `fffind`, `fff-multi-grep`, health/atlas/rescan commands, and `@` path completion, then sends HTTP requests over a Unix domain socket.

The design keeps hot state resident and treats the command path as a narrow kernel pipe. Health is published passively as an atomic snapshot, so readers do not need to wake the daemon. A fresh snapshot is also a liveness lease: the watchdog can recover a wedged process without introducing a second control protocol.

This project is an independent implementation. Its tool vocabulary follows FFF's own MCP server, while the wire protocol is documented independently in [PROTOCOL.md](PROTOCOL.md).

## Layout

- `crates/fff-routerd/`: resident Rust daemon
- `extension/`: native OMP extension for Bun
- `bench/`: byte-parity and randomized A/B latency harnesses

## Install

The Nix/Home Manager integration builds `fff-routerd`, deploys it as `fff-router.service`, installs the extension at `~/.omp/agent/extensions/omp-fff`, and retains the existing `fff` CLI and watchdog.

For development:

```sh
cargo build --release
cd extension
install dependencies with Bun
bun test
bun x tsc --noEmit
```

Configure roots as a JSON array at `~/.config/fff-router/roots.json`:

```json
[
  { "id": "work", "path": "/home/me/work" },
  { "id": "win-c", "path": "/mnt/c", "forward": { "url": "http://127.0.0.1:47997", "remotePrefix": "C:\\" } }
]
```

Start the daemon with `target/release/fff-routerd`. Runtime and state paths, all endpoints, exact errors, and output rules are specified in [PROTOCOL.md](PROTOCOL.md).

## WSL federation

Under WSL the daemon runs twice from the same source: the WSL-side router is the front door every client talks to, and a Windows-native sibling (built with the MSVC toolchain, TCP on `127.0.0.1:47997`, config under `%LOCALAPPDATA%\fff-router`) indexes NTFS natively — real file watchers, no 9P bridge. Windows drives are never local picker roots on the WSL side; `/mnt/*` coverage always goes through a forwarding root that proxies to the sibling and rewrites `C:\...` paths to `/mnt/c/...` and back. The Nix/Home Manager integration is unix-only; the Windows daemon is installed and started out of band.
