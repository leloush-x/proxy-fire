//! `proxy-fire login` — dynamic provider login (list, prompt, validate, save).
use anyhow::{Context, Result};
use proxy_fire_core::catalog::Catalog;
use proxy_fire_core::config::ProviderConfig;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

pub async fn run(cfg_file: &str) -> Result<()> {
    let cfg = proxy_fire_core::config::load(cfg_file).context("loading config")?;
    let providers: Vec<&ProviderConfig> = cfg.providers.iter().collect();

    // Catalog (optional) — used to show how many free models each provider has.
    let cat_path = proxy_fire_core::paths::catalog_read_path(&cfg.catalog.path, cfg_file);
    let cat = Catalog::load(&cat_path).unwrap_or_default();
    let free_counts = |id: &str| {
        cat.free_entries()
            .iter()
            .filter(|e| e.provider_id == id)
            .count()
    };

    print_header();
    for (i, p) in providers.iter().enumerate() {
        let status = if p.api_key_env.is_empty() {
            "\x1b[2mno key needed\x1b[0m"
        } else if p.missing_key() {
            "\x1b[31mkey missing\x1b[0m"
        } else {
            "\x1b[32mkey set\x1b[0m"
        };
        let enabled = if p.enabled {
            ""
        } else {
            " \x1b[2m(disabled)\x1b[0m"
        };
        println!(
            "  \x1b[36m{:>2}\x1b[0m  {:<18} {:<26} {:>3} models  {status}{enabled}",
            i + 1,
            p.id,
            p.name,
            free_counts(&p.id)
        );
    }

    let mut stdout = std::io::stdout();
    println!();
    print!("  \x1b[1mprovider # or id (blank to cancel)\x1b[0m › ");
    let _ = stdout.flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return Ok(());
    }
    let pick = line.trim();
    if pick.is_empty() {
        println!("  cancelled.");
        return Ok(());
    }

    let chosen: &ProviderConfig = match pick.parse::<usize>() {
        Ok(n) if n >= 1 && n <= providers.len() => providers[n - 1],
        _ => match providers.iter().find(|p| p.id.eq_ignore_ascii_case(pick)) {
            Some(p) => p,
            None => anyhow::bail!("no provider matches {pick:?}"),
        },
    };
    println!();
    login_provider(chosen).await
}

fn print_header() {
    println!("\x1b[1mproxy-fire\x1b[0m \x1b[2m· add a provider key\x1b[0m");
    println!("\x1b[2m──────────────────────────────────────────────────────────────\x1b[0m");
}

/// Dynamic per-provider flow.
async fn login_provider(p: &ProviderConfig) -> Result<()> {
    if p.api_key_env.is_empty() {
        println!(
            "  \x1b[32m✓\x1b[0m {} needs no API key (local / keyless). Nothing to save.",
            p.id
        );
        println!("  \x1b[2mStart it with: proxy-fire -c <config>\x1b[0m");
        return Ok(());
    }

    if !p.missing_key() {
        println!(
            "  \x1b[2m{} already reads {} keys (found {}).\x1b[0m",
            p.id,
            p.api_key_env,
            p.all_keys().len()
        );
        print!("  \x1b[2mreplace the first key? [y/N]\x1b[0m › ");
        let _ = std::io::stdout().flush();
        let mut yn = String::new();
        std::io::stdin().read_line(&mut yn)?;
        if !yn.trim().to_ascii_lowercase().starts_with('y') {
            println!("  kept existing key(s).");
            return add_extra_key(p);
        }
    }

    print!("  {} key › ", p.api_key_env);
    let _ = std::io::stdout().flush();
    let key = read_hidden()?;
    if key.trim().is_empty() {
        anyhow::bail!("empty key, nothing saved");
    }

    println!(
        "\x1b[2m  validating {} against {} …\x1b[0m",
        p.api_key_env, p.base_url
    );
    let verdict = validate_key(p, key.trim()).await;
    match verdict {
        KeyVerdict::Verified => {
            println!("  \x1b[32m✓ key works — provider is live.\x1b[0m");
        }
        KeyVerdict::RateLimited => {
            println!("  \x1b[33m!\x1b[0m key works (rate limited right now — that's fine).");
        }
        KeyVerdict::Rejected => {
            print!("  \x1b[31mkey rejected\x1b[0m — save anyway? [y/N] › ");
            let _ = std::io::stdout().flush();
            let mut yn = String::new();
            std::io::stdin().read_line(&mut yn)?;
            if !yn.trim().to_ascii_lowercase().starts_with('y') {
                anyhow::bail!("key not saved");
            }
        }
        KeyVerdict::Unverifiable => {
            println!("  \x1b[2mcould not verify the key — saving unverified.\x1b[0m");
        }
    }

    save_key(p, key.trim(), 1)?;
    println!(
        "  \x1b[32m✓\x1b[0m saved {} to {}",
        p.api_key_env,
        env_file_path().display()
    );

    add_extra_key(p)
}

/// Offer key slots 2..9 (multi-account pooling).
fn add_extra_key(p: &ProviderConfig) -> Result<()> {
    if p.api_key_env.is_empty() {
        return Ok(());
    }
    print!(
        "\n  \x1b[2madd another {}_2 account to pool keys? [y/N]\x1b[0m › ",
        p.api_key_env
    );
    let _ = std::io::stdout().flush();
    let mut yn = String::new();
    std::io::stdin().read_line(&mut yn)?;
    if !yn.trim().to_ascii_lowercase().starts_with('y') {
        return Ok(());
    }
    print!("  {}_2 › ", p.api_key_env);
    let _ = std::io::stdout().flush();
    let key = read_hidden()?;
    if key.trim().is_empty() {
        return Ok(());
    }
    save_key(p, key.trim(), 2)?;
    println!(
        "  \x1b[32m✓\x1b[0m saved {}_2 — the proxy rotates keys on 429 and skips a 401'd key.",
        p.api_key_env
    );
    Ok(())
}

/// Live check: run the provider's own scanner with the candidate key.
/// Live check. Providers whose /models endpoint is public (OpenRouter, HF Hub)
/// cannot prove a key with it, so we spend one real token: a max_tokens=1 chat
/// call with a model from the bundled catalog. 429 = authenticated but
/// rate-limited (still a valid key).
#[derive(Debug, PartialEq)]
enum KeyVerdict {
    Verified,
    RateLimited,
    Rejected,
    Unverifiable,
}

async fn validate_key(p: &ProviderConfig, key: &str) -> KeyVerdict {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build();
    let Ok(client) = client else { return KeyVerdict::Unverifiable };

    // Prefer an authenticated 1-token probe on a catalog model we know exists.
    let model = catalog_model_for(&p.id);
    if let Some(model) = model {
        let url = format!("{}/chat/completions", p.base_url.trim_end_matches('/'));
        let mut payload = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "Hi"}]
        });
        if !p.native_features.service_tier.is_empty() {
            payload["service_tier"] = serde_json::Value::String(p.native_features.service_tier.clone());
        }
        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_string());
        // Candidate credentials (the env var isn't set yet at validation time)
        let h = if p.auth_header.is_empty() {
            "Authorization".to_string()
        } else {
            p.auth_header.clone()
        };
        let prefix = if p.auth_prefix.is_empty() && h == "Authorization" {
            "Bearer ".to_string()
        } else {
            p.auth_prefix.clone()
        };
        req = req.header(&h, format!("{prefix}{key}"));
        for (k, val) in &p.extra_headers {
            req = req.header(k, val);
        }

        return match req.send().await {
            Ok(resp) => match resp.status().as_u16() {
                200 => KeyVerdict::Verified,
                429 => KeyVerdict::RateLimited,
                401 | 403 => KeyVerdict::Rejected,
                _ => KeyVerdict::Unverifiable,
            },
            Err(_) => KeyVerdict::Unverifiable,
        };
    }

    // No catalog model: fall back to auth-sensitive endpoint if any (401/403 → rejected).
    let mut probe = p.clone();
    probe.api_key = key.to_string();
    probe.api_key_env.clear();
    let provider = proxy_fire_providers::get(&p.id);
    match tokio::time::timeout(
        std::time::Duration::from_secs(25),
        provider.scan(&client, &probe),
    )
    .await
    {
        Ok(Ok(entries)) if !entries.is_empty() => KeyVerdict::Verified,
        Ok(Ok(_)) => KeyVerdict::Unverifiable,
        _ => KeyVerdict::Unverifiable,
    }
}

/// First free catalog model for a provider from the bundled catalog.
fn catalog_model_for(provider_id: &str) -> Option<String> {
    let path = proxy_fire_core::paths::find_data_file("catalog.json", "");
    let cat = proxy_fire_core::catalog::Catalog::load(&path).ok()?;
    cat.free_entries()
        .into_iter()
        .find(|e| e.provider_id.eq_ignore_ascii_case(provider_id))
        .map(|e| e.model_id.clone())
}

fn env_file_path() -> PathBuf {
    proxy_fire_core::paths::data_dir().join(".env")
}

/// Write/replace `KEY=<index>` in ~/.proxy-fire/.env, preserving comments.
fn save_key(p: &ProviderConfig, key: &str, slot: usize) -> Result<()> {
    let path = env_file_path();
    std::fs::create_dir_all(proxy_fire_core::paths::data_dir())?;
    let var = if slot == 1 {
        p.api_key_env.clone()
    } else {
        format!("{}_{}", p.api_key_env, slot)
    };
    let mut contents = std::fs::read_to_string(&path).unwrap_or_default();
    let line = format!("{var}={key}");
    let prefix = format!("{var}=");
    if let Some(_pos) = contents
        .lines()
        .position(|l| l.trim_start().starts_with(&prefix))
    {
        // Replace in place: rebuild line by line.
        let mut out: Vec<String> = Vec::new();
        let mut done = false;
        for l in contents.lines() {
            if !done && l.trim_start().starts_with(&prefix) {
                out.push(line.clone());
                done = true;
            } else {
                out.push(l.to_string());
            }
        }
        contents = out.join("\n");
    } else if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
        contents.push_str(&line);
    } else if !contents.is_empty() {
        contents.push_str(&line);
    } else {
        contents = line;
    }
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Read one line with echo disabled (raw termios; no external dep).
fn read_hidden() -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } == 0 {
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            term.c_lflag |= libc::ICANON;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
            let mut line = String::new();
            let res = std::io::stdin().read_line(&mut line);
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
            println!();
            res?;
            return Ok(line.trim().to_string());
        }
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// True when interactive prompts make sense.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal()
}
