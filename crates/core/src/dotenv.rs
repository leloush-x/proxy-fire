//! Minimal .env loader, first-file-wins, real env vars always win.
//!
//! Syntax per line: KEY=value, KEY="v v", KEY='v', # comment, blank lines.

use crate::paths::{data_dir, expand_home};
use std::collections::HashMap;
use std::sync::RwLock;

static DOTENV: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// Load env files in order (first file wins within files; real env wins over all).
pub fn load_dotenv(paths: &[&str]) {
    let mut merged: HashMap<String, String> = HashMap::new();
    for path in paths {
        if path.is_empty() {
            continue;
        }
        let path = expand_home(path);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue; // absent is fine
        };
        for raw in contents.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = parse_line(line) {
                merged.entry(k).or_insert(v);
            }
        }
    }
    *DOTENV.write().unwrap() = Some(merged);
}

fn parse_line(line: &str) -> Option<(String, String)> {
    let idx = line.find('=')?;
    if idx == 0 {
        return None;
    }
    let key = line[..idx].trim().to_string();
    let value_raw = line[idx + 1..].trim().to_string();

    // Strip surrounding quotes
    let mut value = value_raw.clone();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first_last = |a: u8, b: u8| bytes[0] == a && bytes[bytes.len() - 1] == b;
        if first_last(b'"', b'"') || first_last(b'\'', b'\'') {
            value = value[1..value.len() - 1].to_string();
        }
    }

    // Strip inline comment (only outside quotes)
    if !value_raw.starts_with('"') && !value_raw.starts_with('\'') {
        if let Some(ci) = value.find('#') {
            value = value[..ci].trim().to_string();
        }
    }

    if key.is_empty() {
        None
    } else {
        Some((key, value))
    }
}

/// Look up a key: real env first, then loaded files.
pub fn lookup_env(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    DOTENV.read().unwrap().as_ref()?.get(key).cloned()
}

/// Standard search locations; config_dir is the dir of the loaded config file.
pub fn default_env_paths(config_dir: &str) -> Vec<String> {
    let mut paths = vec![".env".to_string(), ".secrets".to_string()];
    if !config_dir.is_empty() && config_dir != "." {
        paths.push(format!("{config_dir}/.env"));
        paths.push(format!("{config_dir}/.secrets"));
    }
    let dir = data_dir();
    paths.push(dir.join(".env").to_string_lossy().into_owned());
    paths.push(dir.join(".secrets").to_string_lossy().into_owned());
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines() {
        assert_eq!(parse_line("A=1"), Some(("A".into(), "1".into())));
        assert_eq!(parse_line("A=\"x y\""), Some(("A".into(), "x y".into())));
        assert_eq!(parse_line("A='x'"), Some(("A".into(), "x".into())));
        assert_eq!(parse_line("A=a # comment"), Some(("A".into(), "a".into())));
        assert_eq!(parse_line("# comment"), None);
        assert_eq!(parse_line(""), None);
    }
}
