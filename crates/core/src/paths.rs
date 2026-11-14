//! Shared filesystem helpers: home expansion, atomic writes, data-file lookup.

use std::fs;
use std::path::{Path, PathBuf};

/// Replace a leading `~/` with the user's home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Write `contents` to `path` atomically (tmp file + rename).
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("part")
    ));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

/// The conventional per-user data directory.
pub fn data_dir() -> PathBuf {
    expand_home("~/.proxy-fire")
}

/// Find a bundled data file (families.yaml, capabilities.yaml, …) so the binaries
/// work from any directory. Search order, first existing file wins:
/// 1. ./configs/<name>, 2. <config dir>/<name>, 3. ~/.proxy-fire/<name>,
/// 4. <executable dir>/configs/<name>. Falls back to "configs/<name>" for error messages.
pub fn find_data_file(name: &str, config_path: &str) -> PathBuf {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("configs").join(name)];
    if !config_path.is_empty() {
        if let Some(dir) = Path::new(config_path).parent() {
            candidates.push(dir.join(name));
        }
    }
    candidates.push(data_dir().join(name));
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.canonicalize().unwrap_or(exe);
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("configs").join(name));
        }
    }
    for c in &candidates {
        if c.is_file() {
            return c.clone();
        }
    }
    PathBuf::from("configs").join(name)
}

pub fn file_exists(path: &Path) -> bool {
    path.is_file()
}

/// The catalog to read at startup: the configured path if it exists, otherwise
/// the bundled catalog.json (so a fresh install has models before the first scan).
/// Writers and watchers keep using the configured path.
pub fn catalog_read_path(path: &str, config_path: &str) -> PathBuf {
    let expanded = expand_home(path);
    if file_exists(&expanded) {
        return expanded;
    }
    let bundled = find_data_file("catalog.json", config_path);
    if file_exists(&bundled) {
        return bundled;
    }
    expanded
}

/// Read a bundled data file by name: on-disk copies win (same search order
/// as find_data_file), then fall back to the copy embedded in the binary.
pub fn read_data_file(name: &str, config_path: &str) -> Option<String> {
    let path = find_data_file(name, config_path);
    if path.is_file() {
        return std::fs::read_to_string(&path).ok();
    }
    crate::embedded::for_name(name).map(str::to_string)
}

/// Materialize the embedded catalog to the user's catalog path on first run.
/// Subsequent runs use the file (which `proxy-fire scan update` rewrites).
pub fn ensure_user_catalog(path: &str) -> std::io::Result<()> {
    let expanded = expand_home(path);
    if file_exists(&expanded) {
        return Ok(());
    }
    if let Some(dir) = expanded.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&expanded, crate::embedded::CATALOG)
}
