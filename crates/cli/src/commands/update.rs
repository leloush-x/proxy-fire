//! `proxy-fire update` — self-update from GitHub releases.
//!
//! Checks the latest release of leloush-x/proxy-fire, compares it with the
//! running version, and (unless only checking) downloads the matching Linux
//! tarball, verifies it against checksums.txt, and atomically replaces the
//! running binary. Unix-only; on failure the running binary is left intact.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "leloush-x/proxy-fire";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub async fn run(check_only: bool) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("proxy-fire")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("http client")?;

    println!("  current version: v{CURRENT}");
    let latest = fetch_latest(&client)
        .await
        .context("fetching latest release")?;
    let latest_version = latest.tag_name.trim_start_matches('v').to_string();
    println!("  latest release : {}", latest.tag_name);

    match compare_versions(&latest_version, CURRENT) {
        std::cmp::Ordering::Greater => {}
        std::cmp::Ordering::Equal => {
            println!("  \x1b[32m✓ already up to date\x1b[0m");
            return Ok(());
        }
        std::cmp::Ordering::Less => {
            println!("  \x1b[33m! running a newer build than the latest release\x1b[0m");
            return Ok(());
        }
    }

    if check_only {
        println!("  update available: v{CURRENT} → v{latest_version} (run `proxy-fire update`)");
        return Ok(());
    }

    let wanted = asset_name_for_this_platform(&latest_version);
    let asset = latest
        .assets
        .iter()
        .find(|a| a.name == wanted)
        .with_context(|| format!("release {} has no asset {wanted}", latest.tag_name))?;
    let checksums = latest
        .assets
        .iter()
        .find(|a| a.name == "checksums.txt")
        .context("release has no checksums.txt")?;

    let tmp = std::env::temp_dir().join(format!("proxy-fire-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let result = async {
        println!("  downloading {wanted} …");
        let tarball = tmp.join(&wanted);
        download(&client, &asset.browser_download_url, &tarball).await?;
        let sums = tmp.join("checksums.txt");
        download(&client, &checksums.browser_download_url, &sums).await?;
        verify_checksum(&sums, &wanted, &tarball)?;
        let new_bin = extract_binary(&tarball, &tmp)?;

        let exe = std::env::current_exe().context("current exe path")?;
        replace_binary(&new_bin, &exe)?;
        println!(
            "  \x1b[32m✓ updated {} → {} at {}\x1b[0m",
            exe.display(),
            latest.tag_name,
            latest_version
        );
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

async fn fetch_latest(client: &reqwest::Client) -> Result<Release> {
    let resp = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("GitHub API returned {}", resp.status().as_u16());
    }
    Ok(resp.json().await?)
}

/// Very small semver compare: numeric triplets, ignores pre-release tags.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> [u64; 3] {
        let mut out = [0u64; 3];
        for (i, seg) in s.split('.').take(3).enumerate() {
            out[i] = seg
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        }
        out
    }
    parts(a).cmp(&parts(b))
}

fn asset_name_for_this_platform(version: &str) -> String {
    #[cfg(target_arch = "x86_64")]
    let arch = "amd64";
    #[cfg(target_arch = "aarch64")]
    let arch = "arm64";
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let arch = "unknown";
    format!("proxy-fire_{version}_linux_{arch}.tar.gz")
}

async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("download {url}: HTTP {}", resp.status().as_u16());
    }
    let bytes = resp.bytes().await?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String> {
    use sha2::Digest;
    let bytes = std::fs::read(path)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_checksum(sums_file: &Path, asset: &str, tarball: &Path) -> Result<()> {
    let sums = std::fs::read_to_string(sums_file)?;
    let expected = sums
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(asset))
        .and_then(|l| l.split_whitespace().next())
        .with_context(|| format!("{asset} not present in checksums.txt"))?;
    let actual = sha256_hex(tarball)?;
    if actual != expected {
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }
    println!("  \x1b[32m✓\x1b[0m checksum verified");
    Ok(())
}

/// Extract the tarball with the system tar (always present on our Linux targets).
fn extract_binary(tarball: &Path, tmp: &Path) -> Result<PathBuf> {
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(tmp)
        .output()
        .context("running tar (is it installed?)")?;
    if !out.status.success() {
        bail!("tar failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let bin = tmp.join("proxy-fire");
    if !bin.is_file() {
        bail!("archive did not contain a 'proxy-fire' binary");
    }
    Ok(bin)
}

/// Atomically swap in the new binary (same-dir temp file + rename).
fn replace_binary(new_bin: &Path, exe: &Path) -> Result<()> {
    let dir = exe.parent().context("exe has no parent dir")?;
    let tmp_bin = dir.join(format!(".proxy-fire.update-{}", std::process::id()));
    std::fs::copy(new_bin, &tmp_bin)
        .with_context(|| format!("copying to {}", tmp_bin.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_bin, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp_bin, exe).with_context(|| format!("replacing {}", exe.display()))?;
    Ok(())
}
