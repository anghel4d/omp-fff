use crate::roots::{HealthRow, RootRuntime};
use std::fs;
use std::path::Path;
use std::sync::Arc;

pub fn health(roots: &[Arc<RootRuntime>]) -> Vec<HealthRow> {
    roots.iter().map(|root| root.health()).collect()
}

pub fn publish(path: &Path, roots: &[Arc<RootRuntime>]) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(&health(roots)).map_err(|error| error.to_string())?;
    fs::write(&temp, data).map_err(|error| error.to_string())?;
    fs::rename(&temp, path).map_err(|error| error.to_string())
}
