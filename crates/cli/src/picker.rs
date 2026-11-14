//! Interactive provider picker: a 5-row diffused viewport.
//! Arrow keys / PgUp / PgDn move, Enter selects, q / Esc / Ctrl-C cancel.
//! Edges fade; only the window is drawn so long lists don't scroll the shell.
//! Not a TTY? The caller falls back to a numeric prompt.

use anyhow::Result;
use proxy_fire_core::catalog::Catalog;
use proxy_fire_core::config::ProviderConfig;
use std::io::{IsTerminal, Read, Write};

const WINDOW: usize = 5;
const RENDER_LINES: usize = 8;
const ACCENT: &str = "\x1b[38;5;36m"; // cyan
const FADE: [&str; 4] = [
    "\x1b[38;5;231m",
    "\x1b[38;5;248m",
    "\x1b[38;5;245m",
    "\x1b[38;5;242m",
];
const DIM: &str = "\x1b[38;5;240m";
const RESET: &str = "\x1b[0m";

#[cfg(unix)]
struct RawMode(libc::termios);

#[cfg(unix)]
impl RawMode {
    fn enable() -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            anyhow::bail!("stdin is not a terminal");
        }
        let saved = t;
        t.c_lflag &= !(libc::ECHO | libc::ICANON);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
        Ok(Self(saved))
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.0) };
    }
}

/// Returns Some(index into providers) or None when cancelled.
/// Not a TTY → numeric prompt fallback.
pub fn pick(providers: &[&ProviderConfig], catalog: &Catalog) -> Result<Option<usize>> {
    if providers.is_empty() {
        println!("  no providers configured — add some to config.yaml");
        return Ok(None);
    }
    if !std::io::stdin().is_terminal() {
        return fallback_pick(providers, catalog);
    }
    let free_counts: Vec<usize> = providers
        .iter()
        .map(|p| {
            catalog
                .free_entries()
                .iter()
                .filter(|e| e.provider_id == p.id)
                .count()
        })
        .collect();
    let statuses: Vec<RowStatus> = providers
        .iter()
        .map(|p| {
            if p.api_key_env.is_empty() {
                RowStatus::Keyless
            } else if p.missing_key() {
                RowStatus::Missing
            } else {
                RowStatus::Keyed
            }
        })
        .collect();

    let _raw = RawMode::enable()?;
    let mut sel = 0usize;
    let mut offset = 0usize;

    print_header(providers.len());
    render(providers, &free_counts, &statuses, sel, offset)?;

    loop {
        let key = read_key()?;
        match key {
            Key::Up => sel = sel.saturating_sub(1),
            Key::Down => {
                if sel + 1 < providers.len() {
                    sel += 1;
                }
            }
            Key::PageUp => sel = sel.saturating_sub(WINDOW),
            Key::PageDown => sel = (sel + WINDOW).min(providers.len() - 1),
            Key::Home => sel = 0,
            Key::End => sel = providers.len() - 1,
            Key::Enter => {
                let _ = writeln!(std::io::stdout());
                return Ok(Some(sel));
            }
            Key::Cancel => {
                let _ = writeln!(std::io::stdout());
                return Ok(None);
            }
            Key::Unknown => continue,
        }
        // keep selection visible in the window
        if sel < offset {
            offset = sel;
        } else if sel >= offset + WINDOW {
            offset = sel + 1 - WINDOW;
        }
        move_up(RENDER_LINES);
        render(providers, &free_counts, &statuses, sel, offset)?;
    }
}

fn print_header(total: usize) {
    println!("\x1b[1mproxy-fire\x1b[0m \x1b[2m· add a provider key\x1b[0m  \x1b[2m({total} providers)\x1b[0m");
    println!("{DIM}──────────────────────────────────────────────────────────────{RESET}");
}

#[derive(Clone, Copy, PartialEq)]
enum RowStatus {
    Keyed,
    Missing,
    Keyless,
}

fn render(
    providers: &[&ProviderConfig],
    counts: &[usize],
    statuses: &[RowStatus],
    sel: usize,
    offset: usize,
) -> Result<()> {
    let mut out = String::new();
    // top fade row: e.g. "   ↑ 12 more"
    let above = offset;
    out.push_str(&format!("{DIM}    {:>2} more ↑{RESET}\x1b[K\n", above));
    for row in 0..WINDOW {
        let idx = offset + row;
        if idx >= providers.len() {
            out.push_str("\x1b[K\n");
            continue;
        }
        let p = providers[idx];
        let dist = (row as isize - ((sel - offset) as isize)).unsigned_abs();
        let color = FADE[dist.min(3)];
        let marker = if idx == sel {
            format!("{ACCENT}  ›{RESET} ")
        } else {
            "    ".to_string()
        };
        let (dot, dot_color) = match statuses[idx] {
            RowStatus::Keyed => ("●", "\x1b[38;5;114m"),
            RowStatus::Missing => ("●", "\x1b[38;5;203m"),
            RowStatus::Keyless => ("○", DIM),
        };
        let disabled = if p.enabled {
            "".to_string()
        } else {
            format!("  {DIM}disabled{RESET}")
        };
        out.push_str(&format!(
            "{marker}{color}{:>2}  {:<16}{RESET} {DIM}{:>3} models{RESET}  {dot_color}{dot}{RESET}{disabled}\x1b[K\n",
            idx + 1,
            p.id,
            counts[idx],
        ));
    }
    let below = providers.len().saturating_sub(offset + WINDOW);
    out.push_str(&format!(
        "{DIM}    {:>2} more ↓      ↑↓ move · enter to login · q to cancel{RESET}\x1b[K\n",
        below
    ));
    out.push_str(&format!(
        "{DIM}──────────────────────────────────────────────────────────────{RESET}\x1b[K"
    ));
    print!("{out}");
    std::io::stdout().flush()?;
    Ok(())
}

fn move_up(n: usize) {
    print!("\x1b[{}A", n);
    let _ = std::io::stdout().flush();
}

enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Cancel,
    Unknown,
}

fn read_key() -> Result<Key> {
    let mut buf = [0u8; 1];
    std::io::stdin().read_exact(&mut buf)?;
    match buf[0] {
        b'\r' | b'\n' => Ok(Key::Enter),
        b'k' | b'K' => Ok(Key::Up),
        b'j' | b'J' => Ok(Key::Down),
        b'q' | b'Q' | 3 => Ok(Key::Cancel), // q / Ctrl-C
        27 => {
            // Escape sequence: ESC [ (1-3 bytes) [ final byte ]
            let mut seq = [0u8; 3];
            for slot in seq.iter_mut() {
                let mut b = [0u8; 1];
                if std::io::stdin().read_exact(&mut b).is_err() {
                    break;
                }
                *slot = b[0];
                if b[0].is_ascii_alphabetic() || b[0] == b'~' {
                    break;
                }
            }
            Ok(match (seq[0], seq.get(1).copied()) {
                (b'[', Some(b'A')) | (b'O', Some(b'A')) => Key::Up,
                (b'[', Some(b'B')) | (b'O', Some(b'B')) => Key::Down,
                (b'[', Some(b'H')) | (b'O', Some(b'H')) => Key::Home,
                (b'[', Some(b'F')) | (b'O', Some(b'F')) => Key::End,
                (b'[', Some(b'5')) => Key::PageUp,
                (b'[', Some(b'6')) => Key::PageDown,
                (b'[', Some(b'1')) => Key::Home,
                (b'[', Some(b'4')) => Key::End,
                _ => Key::Cancel,
            })
        }
        _ => Ok(Key::Unknown),
    }
}

fn fallback_pick(providers: &[&ProviderConfig], catalog: &Catalog) -> Result<Option<usize>> {
    // Non-TTY: print numbered list, read a number or id.
    for (i, p) in providers.iter().enumerate() {
        let free = catalog
            .free_entries()
            .iter()
            .filter(|e| e.provider_id == p.id)
            .count();
        let status = if p.api_key_env.is_empty() {
            "no key"
        } else if p.missing_key() {
            "key missing"
        } else {
            "key set"
        };
        let disabled = if p.enabled { "" } else { " (disabled)" };
        println!(
            "  {:>2}  {:<18} {:>3} models  {}{}",
            i + 1,
            p.id,
            free,
            status,
            disabled
        );
    }
    println!();
    print!("  provider # or id (blank to cancel) › ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let pick = line.trim();
    if pick.is_empty() {
        println!("  cancelled.");
        return Ok(None);
    }
    match pick.parse::<usize>() {
        Ok(n) if n >= 1 && n <= providers.len() => Ok(Some(n - 1)),
        _ => match providers
            .iter()
            .position(|p| p.id.eq_ignore_ascii_case(pick))
        {
            Some(i) => Ok(Some(i)),
            None => anyhow::bail!("no provider matches {pick:?}"),
        },
    }
}
