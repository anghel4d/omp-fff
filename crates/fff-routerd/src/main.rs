mod complete;
mod cursors;
mod format;
mod forward;
mod http;
mod query;
mod roots;
mod snapshot;

use crate::cursors::CursorStore;
use crate::http::AppState;
use crate::roots::{load_roots, RootRuntime, RootState};
use mimalloc::MiMalloc;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::resolve()?;
    fs::create_dir_all(&paths.state_dir)?;
    fs::create_dir_all(&paths.runtime_dir)?;
    #[cfg(unix)]
    {
        // 0700 on the runtime dir is the socket's only access control; also drop any stale socket.
        fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(0o700))?;
        let _ = fs::remove_file(&paths.socket);
    }
    let _ = fs::remove_file(&paths.health);
    let log_file = paths.state_dir.join("fff-routerd.log");
    if let Err(error) = fff::log::init_tracing(log_file.to_string_lossy().as_ref(), Some("warn"), None) {
        eprintln!("Warning: Failed to init tracing: {error}");
    }

    let roots = Arc::new(load_roots(&paths.config)?.into_iter().map(Arc::new).collect::<Vec<_>>());
    snapshot::publish(&paths.health, &roots)?;
    spawn_initialization(Arc::clone(&roots), paths.state_dir.clone());
    spawn_health_tick(Arc::clone(&roots), paths.health.clone());
    spawn_mnt_rescans(Arc::clone(&roots));

    let state = AppState {
        roots: Arc::clone(&roots),
        grep_cursors: Arc::new(Mutex::new(CursorStore::default())),
        find_cursors: Arc::new(Mutex::new(CursorStore::default())),
    };
    // FFF_ROUTER_LISTEN=host:port selects TCP on any platform; without it, unix
    // keeps the domain socket and Windows falls back to loopback TCP.
    if let Some(addr) = tcp_listen_addr() {
        let listener = TcpListener::bind(&addr).await?;
        println!("fff-routerd listening on tcp:{} with {} roots", listener.local_addr()?, roots.len());
        axum::serve(listener, http::router(state))
            .with_graceful_shutdown(shutdown_cleanup(Arc::clone(&roots), None, paths.health.clone()))
            .await?;
        return Ok(());
    }

    #[cfg(unix)]
    {
        let listener = UnixListener::bind(&paths.socket)?;
        println!("fff-routerd listening on unix:{} with {} roots", paths.socket.display(), roots.len());
        axum::serve(listener, http::router(state))
            .with_graceful_shutdown(shutdown_cleanup(Arc::clone(&roots), Some(paths.socket.clone()), paths.health.clone()))
            .await?;
    }
    Ok(())
}

/// TCP listen address, when TCP mode is in effect: `FFF_ROUTER_LISTEN` takes
/// precedence everywhere, and Windows (no Unix sockets) defaults to loopback.
fn tcp_listen_addr() -> Option<String> {
    let configured = env::var("FFF_ROUTER_LISTEN").ok().filter(|value| !value.is_empty());
    if cfg!(windows) {
        Some(configured.unwrap_or_else(|| "127.0.0.1:47997".to_string()))
    } else {
        configured
    }
}

/// Wait for a shutdown signal, then cancel every picker and remove the wire
/// artifacts: the socket in UDS mode, the health snapshot in both modes.
async fn shutdown_cleanup(roots: Arc<Vec<Arc<RootRuntime>>>, socket: Option<PathBuf>, health: PathBuf) {
    shutdown_signal().await;
    for root in roots.iter() {
        root.picker.cancel();
    }
    if let Some(socket) = socket {
        let _ = fs::remove_file(socket);
    }
    let _ = fs::remove_file(health);
}

fn spawn_initialization(roots: Arc<Vec<Arc<RootRuntime>>>, state_dir: PathBuf) {
    tokio::spawn(async move {
        for (index, root) in roots.iter().enumerate() {
            let root = Arc::clone(root);
            if root.forward.is_some() {
                // Forwarding roots have no local index to build; init is a
                // cheap upstream health probe that must never block siblings.
                tokio::spawn(async move { forward::probe_and_apply(&root).await });
            } else {
                let state_dir = state_dir.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = root.initialize(&state_dir) {
                        eprintln!("[{}] {error}", root.id);
                        root.set_unhealthy(error);
                    }
                });
            }
            if index + 1 < roots.len() {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });
}

fn spawn_health_tick(roots: Arc<Vec<Arc<RootRuntime>>>, health: PathBuf) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            for root in roots.iter() {
                root.refresh_scan_state();
            }
            if let Err(error) = snapshot::publish(&health, &roots) {
                eprintln!("failed to publish health snapshot: {error}");
            }
        }
    });
}

/// Read a positive-integer seconds value from `key`, falling back to `default`
/// on absence or a non-positive/unparseable value.
fn env_secs(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|value| value.parse::<u64>().ok()).filter(|value| *value > 0).unwrap_or(default)
}

fn spawn_mnt_rescans(roots: Arc<Vec<Arc<RootRuntime>>>) {
    // 9p/drvfs (/mnt/*) can't deliver inotify events, so these roots are kept
    // fresh by periodic full rescans instead. Tunable via env; default is a slow
    // 60min sweep. First poll is delayed a full period so we don't re-scan right
    // after the startup index.
    let period = Duration::from_secs(env_secs("FFF_ROUTER_MNT_RESCAN_SECS", 60 * 60));
    let stagger = env_secs("FFF_ROUTER_MNT_RESCAN_STAGGER_SECS", 30);
    // Forwarding roots are excluded even when they cover /mnt/*: the upstream
    // daemon owns freshness and there is no local index to rescan.
    for (index, root) in roots.iter().filter(|root| root.forward.is_none() && root.path.starts_with("/mnt/")).enumerate() {
        let root = Arc::clone(root);
        let roots = Arc::clone(&roots);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(index as u64 * stagger)).await;
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                interval.tick().await;
                if root.state() == RootState::Unhealthy {
                    continue;
                }
                let any_scanning = roots.iter().any(|candidate| {
                    candidate
                        .picker
                        .read()
                        .ok()
                        .and_then(|guard| guard.as_ref().map(|picker| picker.is_scan_active()))
                        .unwrap_or(false)
                });
                if !any_scanning {
                    let _ = root.trigger_rescan();
                }
            }
        });
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

struct Paths {
    runtime_dir: PathBuf,
    #[cfg(unix)]
    socket: PathBuf,
    health: PathBuf,
    config: PathBuf,
    state_dir: PathBuf,
}

impl Paths {
    #[cfg(unix)]
    fn resolve() -> Result<Self, String> {
        let home = env::var_os("HOME").map(PathBuf::from).ok_or("HOME is unset")?;
        let runtime_dir = if let Some(value) = env::var_os("FFF_ROUTER_RUNTIME_DIR") {
            PathBuf::from(value)
        } else {
            let base = env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc_getuid() })));
            base.join("fff-router")
        };
        let config = env::var_os("FFF_ROUTER_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/fff-router/roots.json"));
        let state_dir = env::var_os("FFF_ROUTER_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state/fff-router"));
        Ok(Self {
            socket: runtime_dir.join("fff.sock"),
            health: runtime_dir.join("health.json"),
            runtime_dir,
            config,
            state_dir,
        })
    }

    /// Everything hangs off `%LOCALAPPDATA%\fff-router` on Windows: roots.json at
    /// the top level, with `run` (health snapshot) and `state` subdirectories.
    /// The FFF_ROUTER_* overrides work exactly as on unix.
    #[cfg(windows)]
    fn resolve() -> Result<Self, String> {
        let base = env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or("LOCALAPPDATA is unset")?.join("fff-router");
        let runtime_dir = env::var_os("FFF_ROUTER_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| base.join("run"));
        let config = env::var_os("FFF_ROUTER_CONFIG").map(PathBuf::from).unwrap_or_else(|| base.join("roots.json"));
        let state_dir = env::var_os("FFF_ROUTER_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| base.join("state"));
        Ok(Self { health: runtime_dir.join("health.json"), runtime_dir, config, state_dir })
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn c_getuid() -> u32;
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    c_getuid()
}
