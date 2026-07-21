use crate::forward::ForwardTarget;
use fff::file_picker::FilePicker;
use fff::frecency::FrecencyTracker;
use fff::query_tracker::QueryTracker;
use fff::{FFFMode, FilePickerOptions, SharedFilePicker, SharedFrecency, SharedQueryTracker};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
struct RawRootConfig {
    id: serde_json::Value,
    path: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthRow {
    pub id: String,
    pub path: String,
    pub files: usize,
    pub state: RootState,
    #[serde(rename = "lastScanAt")]
    pub last_scan_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RootState {
    Starting,
    Scanning,
    Ready,
    Unhealthy,
}

#[derive(Debug)]
struct RootStatus {
    state: RootState,
    last_scan_at: Option<String>,
    error: Option<String>,
    /// Probed file count for forwarding roots; local roots read the picker.
    files: Option<usize>,
}

#[derive(Debug)]
pub struct RootRuntime {
    pub id: String,
    pub path: PathBuf,
    pub picker: SharedFilePicker,
    pub frecency: SharedFrecency,
    pub query_tracker: SharedQueryTracker,
    /// Present on forwarding roots: queries proxy upstream, no local picker.
    pub forward: Option<ForwardTarget>,
    status: Mutex<RootStatus>,
}

impl RootRuntime {
    fn new(id: String, path: PathBuf, state: RootState, error: Option<String>, forward: Option<ForwardTarget>) -> Self {
        Self {
            id,
            path,
            picker: SharedFilePicker::default(),
            frecency: SharedFrecency::default(),
            query_tracker: SharedQueryTracker::default(),
            forward,
            status: Mutex::new(RootStatus { state, last_scan_at: None, error, files: None }),
        }
    }

    pub fn state(&self) -> RootState {
        lock(&self.status).state
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.status).error.clone()
    }

    pub fn set_state(&self, state: RootState, timestamp: bool) {
        let mut status = lock(&self.status);
        status.state = state;
        if timestamp {
            status.last_scan_at = Some(now_iso());
        }
    }

    pub fn set_unhealthy(&self, error: String) {
        let mut status = lock(&self.status);
        status.state = RootState::Unhealthy;
        status.error = Some(error);
    }

    /// Fold an upstream probe result into a forwarding root's status in one
    /// locked write: state, upstream lastScanAt, message, and file count.
    pub fn set_forward_status(&self, state: RootState, last_scan_at: Option<String>, error: Option<String>, files: usize) {
        let mut status = lock(&self.status);
        status.state = state;
        status.last_scan_at = last_scan_at;
        status.error = error;
        status.files = Some(files);
    }

    pub fn health(&self) -> HealthRow {
        let picker_files = self
            .picker
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|picker| picker.get_scan_progress().scanned_files_count))
            .unwrap_or(0);
        let status = lock(&self.status);
        HealthRow {
            id: self.id.clone(),
            path: self.path.to_string_lossy().into_owned(),
            files: status.files.unwrap_or(picker_files),
            state: status.state,
            last_scan_at: status.last_scan_at.clone(),
            error: status.error.clone(),
        }
    }

    pub fn initialize(&self, state_dir: &Path) -> Result<(), String> {
        if self.state() == RootState::Unhealthy {
            return Ok(());
        }
        let frecency_path = state_dir.join(format!("{}.frecency.db", self.id));
        match FrecencyTracker::open(&frecency_path) {
            Ok(tracker) => {
                let _ = self.frecency.init(tracker);
            }
            Err(error) => eprintln!("[{}] failed to open frecency database: {error}", self.id),
        }
        let history_path = state_dir.join(format!("{}.history.db", self.id));
        match QueryTracker::open(&history_path) {
            Ok(tracker) => {
                let _ = self.query_tracker.init(tracker);
            }
            Err(error) => eprintln!("[{}] failed to open history database: {error}", self.id),
        }
        FilePicker::new_with_shared_state(
            self.picker.clone(),
            self.frecency.clone(),
            FilePickerOptions {
                base_path: self.path.to_string_lossy().into_owned(),
                enable_mmap_cache: false,
                enable_content_indexing: false,
                mode: FFFMode::Ai,
                watch: !self.path.starts_with("/mnt/"),
                follow_symlinks: false,
                enable_home_dir_scanning: true,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;
        self.set_state(RootState::Scanning, false);
        if self.picker.wait_for_scan(Duration::from_secs(120)) {
            self.set_state(RootState::Ready, true);
        }
        Ok(())
    }

    pub fn ensure_ready(&self) -> Result<(), String> {
        match self.state() {
            RootState::Unhealthy => Err(format!(
                "INDEX_UNHEALTHY: {}: {}",
                self.id,
                self.error().unwrap_or_else(|| "unknown error".into())
            )),
            RootState::Starting => Err(format!(
                "INDEX_NOT_READY: {} is still starting; retry shortly or use native tools.",
                self.id
            )),
            RootState::Ready => Ok(()),
            RootState::Scanning => {
                if self.picker.wait_for_scan(Duration::from_secs(10)) {
                    self.set_state(RootState::Ready, true);
                    Ok(())
                } else {
                    Err(format!(
                        "INDEX_NOT_READY: {} is still scanning; retry shortly or use native tools.",
                        self.id
                    ))
                }
            }
        }
    }

    pub fn trigger_rescan(&self) -> Result<(), String> {
        self.picker
            .trigger_full_rescan_async(&self.frecency)
            .map_err(|error| error.to_string())?;
        self.set_state(RootState::Scanning, true);
        Ok(())
    }

    pub fn refresh_scan_state(&self) {
        // Forwarding roots have no local scan; their state only moves on probes.
        if self.forward.is_some() || self.state() != RootState::Scanning {
            return;
        }
        let active = self
            .picker
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|picker| picker.is_scan_active()))
            .unwrap_or(true);
        if !active {
            self.set_state(RootState::Ready, true);
        }
    }
}

pub fn load_roots(config_path: &Path) -> Result<Vec<RootRuntime>, String> {
    let text = fs::read_to_string(config_path).map_err(|error| error.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let array = value
        .as_array()
        .ok_or_else(|| format!("{} must contain a JSON array", config_path.display()))?;
    let mut ids = HashSet::new();
    let mut roots = Vec::with_capacity(array.len());
    for (index, value) in array.iter().enumerate() {
        let raw: RawRootConfig = serde_json::from_value(value.clone())
            .map_err(|_| format!("Invalid root entry at index {index}"))?;
        let id = raw.id.as_str().ok_or_else(|| format!("Invalid root entry at index {index}"))?;
        let path = raw.path.as_str().ok_or_else(|| format!("Invalid root entry at index {index}"))?;
        if !ids.insert(id.to_string()) {
            return Err(format!("Duplicate root id: {id}"));
        }
        let path_buf = PathBuf::from(path);
        if !path_buf.is_absolute() {
            return Err(format!("Root path must be absolute: {path}"));
        }
        let forward = match value.get("forward") {
            Some(config) if !config.is_null() => Some(
                ForwardTarget::from_value(config)
                    .map_err(|error| format!("Invalid forward config for root {id}: {error}"))?,
            ),
            _ => None,
        };
        if forward.is_some() {
            // Forwarding roots need no local directory, picker, or databases:
            // the upstream daemon owns the index and its state comes from probes.
            roots.push(RootRuntime::new(id.to_string(), path_buf, RootState::Starting, None, forward));
        } else if !path_buf.is_dir() {
            roots.push(RootRuntime::new(
                id.to_string(),
                path_buf,
                RootState::Unhealthy,
                Some("directory does not exist".into()),
                None,
            ));
        } else {
            roots.push(RootRuntime::new(id.to_string(), path_buf, RootState::Starting, None, None));
        }
    }
    Ok(roots)
}

pub fn resolve_root<'a>(roots: &'a [&RootRuntime], requested: &str) -> Result<(&'a RootRuntime, Option<String>), String> {
    let requested_path = Path::new(requested);
    if !requested_path.is_absolute() {
        return Err(format!("INVALID_PATH: path must be absolute: {requested}"));
    }
    let resolved = lexical_normalize(requested_path);
    let mut matches = roots
        .iter()
        .filter(|root| resolved.starts_with(&root.path))
        .collect::<Vec<_>>();
    matches.sort_by_key(|root| std::cmp::Reverse(root.path.as_os_str().len()));
    let Some(root) = matches.first().copied() else {
        return Err(format!(
            "UNINDEXED_PATH: {requested} is not under any FFF-indexed root.\nIndexed roots: {}\nUse native grep/glob/read for this path.",
            roots.iter().map(|root| root.path.to_string_lossy()).collect::<Vec<_>>().join(", ")
        ));
    };
    if root.state() == RootState::Unhealthy {
        return Err(format!(
            "INDEX_UNHEALTHY: {}: {}",
            root.id,
            root.error().unwrap_or_else(|| "unknown error".into())
        ));
    }
    if root.picker.read().ok().and_then(|g| g.as_ref().map(|_| ())).is_none() {
        return Err(format!(
            "INDEX_NOT_READY: {} is still starting; retry shortly or use native tools.",
            root.id
        ));
    }
    let rel = resolved
        .strip_prefix(&root.path)
        .ok()
        .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
        .filter(|value| !value.is_empty());
    Ok((root, rel))
}

/// Longest-prefix match like `resolve_root`, but only when the winning root is
/// a forwarding root. State and picker checks are deliberately skipped: forward
/// roots never touch local pickers, and the proxy attempt itself doubles as the
/// lazy reachability probe. `None` sends the request down the local path.
pub fn match_forward<'a>(roots: &'a [Arc<RootRuntime>], requested: &str) -> Option<(&'a Arc<RootRuntime>, Option<String>)> {
    let requested_path = Path::new(requested);
    if !requested_path.is_absolute() {
        return None;
    }
    let resolved = lexical_normalize(requested_path);
    let mut matches = roots
        .iter()
        .filter(|root| resolved.starts_with(&root.path))
        .collect::<Vec<_>>();
    matches.sort_by_key(|root| std::cmp::Reverse(root.path.as_os_str().len()));
    let root = matches.first().copied()?;
    root.forward.as_ref()?;
    let rel = resolved
        .strip_prefix(&root.path)
        .ok()
        .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
        .filter(|value| !value.is_empty());
    Some((root, rel))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn now_iso() -> String {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.000Z")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make a '/'-shaped test path absolute on the host platform, so these
    /// tests also pass under the Windows toolchain.
    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        }
    }

    fn load_from_value(name: &str, config: serde_json::Value) -> Result<Vec<RootRuntime>, String> {
        let path = std::env::temp_dir().join(format!("fff-routerd-test-{}-{name}.json", std::process::id()));
        fs::write(&path, config.to_string()).unwrap();
        let result = load_roots(&path);
        let _ = fs::remove_file(&path);
        result
    }

    #[test]
    fn forward_roots_load_without_local_directory() {
        let roots = load_from_value(
            "fwd-ok",
            serde_json::json!([{
                "id": "win-c",
                "path": abs("/nonexistent-forward-root"),
                "forward": {"url": "http://127.0.0.1:47997", "remotePrefix": "C:\\"},
            }]),
        )
        .unwrap();
        assert_eq!(roots.len(), 1);
        let root = &roots[0];
        assert_eq!(root.id, "win-c");
        assert_eq!(root.state(), RootState::Starting);
        let forward = root.forward.as_ref().expect("forward config parsed");
        assert_eq!(forward.authority, "127.0.0.1:47997");
        assert_eq!(forward.remote_prefix, "C:\\");
    }

    #[test]
    fn invalid_forward_config_is_rejected_with_context() {
        let error = load_from_value(
            "fwd-bad",
            serde_json::json!([{
                "id": "win-c",
                "path": abs("/mnt/c"),
                "forward": {"url": "https://host:1", "remotePrefix": "C:\\"},
            }]),
        )
        .unwrap_err();
        assert!(error.contains("Invalid forward config for root win-c"), "{error}");
    }

    #[test]
    fn match_forward_only_selects_forwarding_roots() {
        let forward = crate::forward::ForwardTarget::from_value(
            &serde_json::json!({"url": "http://127.0.0.1:47997", "remotePrefix": "C:\\"}),
        )
        .unwrap();
        let roots = vec![
            Arc::new(RootRuntime::new("home".into(), PathBuf::from(abs("/home/user")), RootState::Starting, None, None)),
            Arc::new(RootRuntime::new("win-c".into(), PathBuf::from(abs("/mnt/c")), RootState::Starting, None, Some(forward))),
        ];
        let (root, rel) = match_forward(&roots, &abs("/mnt/c/Users/Pyrus/my docs")).expect("forward root matched");
        assert_eq!(root.id, "win-c");
        assert_eq!(rel.as_deref(), Some("Users/Pyrus/my docs"));
        let (_, rel) = match_forward(&roots, &abs("/mnt/c")).expect("root itself matches");
        assert_eq!(rel, None);
        assert!(match_forward(&roots, &abs("/home/user/src")).is_none(), "local roots go down the local path");
        assert!(match_forward(&roots, "relative").is_none());
    }
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
