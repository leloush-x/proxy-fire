//! Catalog git-sync — synchronous primitives shared by the proxy (replica
//! pull / scanner push loops) and proxy-fire-scan (push after manual update).
//!
//! Scanner role (one machine): scan_interval + auto_push → git commit/push.
//! Replica role (all others): remote_url + pull_interval → HTTP fetch.
//! The HTTP fetch itself is done by the async caller; this module validates
//! and writes it.

use crate::config::GitSyncConfig;
use crate::paths::expand_home;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cap on a replica download. Real catalogs are a few hundred KB.
pub const MAX_REMOTE_CATALOG_BYTES: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRole {
    /// Run a full scan every interval, then push.
    Scanner,
    /// Fetch remote_url every interval.
    Replica,
}

pub fn decide_role(cfg: &GitSyncConfig) -> Option<SyncRole> {
    if !cfg.enabled {
        return None;
    }
    if cfg.auto_push
        && !cfg.scan_interval.is_empty()
        && crate::config::parse_duration(&cfg.scan_interval).is_some()
    {
        return Some(SyncRole::Scanner);
    }
    if !cfg.remote_url.is_empty()
        && !cfg.pull_interval.is_empty()
        && crate::config::parse_duration(&cfg.pull_interval).is_some()
    {
        return Some(SyncRole::Replica);
    }
    None
}

/// How long since the catalog file was modified; missing ⇒ 999h (scan now).
pub fn catalog_age(path: &str) -> std::time::Duration {
    match std::fs::metadata(expand_home(path)).and_then(|m| m.modified()) {
        Ok(t) => t
            .elapsed()
            .unwrap_or(std::time::Duration::from_secs(999 * 3600)),
        Err(_) => std::time::Duration::from_secs(999 * 3600),
    }
}

/// Number of entries in a catalog JSON file.
pub fn count_entries(path: &str) -> std::io::Result<usize> {
    let data = std::fs::read(expand_home(path))?;
    let v: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(v.get("entries")
        .and_then(|e| e.as_array())
        .map(|a| a.len())
        .unwrap_or(0))
}

/// Validate a remote catalog payload (JSON, non-empty entries, size cap) and
/// atomically replace `local_path`. An error/truncated/empty catalog leaves
/// the current one in place. Returns the entry count.
pub fn write_remote_catalog(data: &[u8], local_path: &str) -> Result<usize, String> {
    if data.len() > MAX_REMOTE_CATALOG_BYTES {
        return Err(format!(
            "response exceeds {} bytes",
            MAX_REMOTE_CATALOG_BYTES
        ));
    }
    let cat: crate::catalog::Catalog =
        serde_json::from_slice(data).map_err(|e| format!("not a catalog: {e}"))?;
    if cat.entries.is_empty() {
        return Err("catalog has no entries".to_string());
    }
    let expanded = expand_home(local_path);
    let tmp = expanded.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &expanded).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(cat.entries.len())
}

/// Copy the catalog into the git repo and commit+push. The pathspec limits
/// the commit to the catalog file — anything else already staged stays out.
pub fn git_commit_push(local_catalog_path: &str, cfg: &GitSyncConfig) -> Result<(), String> {
    let repo_path = if cfg.repo_path.is_empty() {
        "."
    } else {
        cfg.repo_path.as_str()
    };
    let catalog_in_repo = if cfg.catalog_in_repo.is_empty() {
        "data/catalog.json"
    } else {
        cfg.catalog_in_repo.as_str()
    };

    let dst = PathBuf::from(repo_path).join(catalog_in_repo);
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }

    let count = count_entries(local_catalog_path).unwrap_or(0);
    copy_file(&expand_home(local_catalog_path), &dst)?;

    let msg = format!(
        "chore: catalog update {} ({} models)",
        chrono::Utc::now().format("%Y-%m-%dT%H:%MZ"),
        count
    );

    for args in [
        ["add", catalog_in_repo].as_slice(),
        [
            "commit",
            "--allow-empty",
            "-m",
            msg.as_str(),
            "--",
            catalog_in_repo,
        ]
        .as_slice(),
        ["push"].as_slice(),
    ] {
        let out = run_git(repo_path, args)?;
        tracing::info!("gitsync: git {} ok", args[0]);
        let _ = out;
    }
    tracing::info!("gitsync: pushed {count} models to {repo_path}:{catalog_in_repo}");
    Ok(())
}

/// Push after a manual scan, when auto_push is enabled. No-op otherwise.
pub fn push_after_scan(local_catalog_path: &str, cfg: &GitSyncConfig) -> Result<(), String> {
    if !cfg.enabled || !cfg.auto_push {
        return Ok(());
    }
    git_commit_push(local_catalog_path, cfg)
}

fn run_git(dir: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {}: {e}", args[0]))?;
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("git {}: {}\n{text}", args[0], out.status));
    }
    Ok(text)
}

fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = dst.with_extension("tmp");
    std::fs::copy(src, &tmp).map_err(|e| format!("copy catalog: {e}"))?;
    std::fs::rename(&tmp, dst).map_err(|e| e.to_string())
}
