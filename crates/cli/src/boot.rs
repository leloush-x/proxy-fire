//! Boot animation: a diffused thin RGB LED strip scans on, settles, and fades.
//! TTY only (skipped when piped), disable with PROXY_FIRE_NO_ANIM=1 or --no-anim.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

const WIDTH: usize = 34;
const FRAME: Duration = Duration::from_millis(26);

/// Play the strip. Returns immediately when stdout is not a TTY.
pub fn play() {
    if std::env::var_os("PROXY_FIRE_NO_ANIM").is_some() || !std::io::stdout().is_terminal() {
        return;
    }
    let mut out = std::io::stdout();
    let frames = 32;
    for f in 0..frames {
        let t = f as f64 / frames as f64;
        // Diffusion: a soft gaussian band sweeping across the strip.
        let head = -0.2 + t * 1.4;
        let mut line = String::with_capacity(WIDTH * 20);
        line.push_str("\r\x1b[2K");
        for i in 0..WIDTH {
            let x = i as f64 / (WIDTH - 1) as f64;
            let d = (x - head).abs();
            // gaussian glow + per-LED stipple for the diffused look
            let glow = (-9.0 * d * d).exp();
            if glow < 0.02 && (i * 7 + f) % 5 != 0 {
                line.push_str("\x1b[38;5;236m·\x1b[0m");
                continue;
            }
            let (r, g, b) = hue_rgb(0.58 - 0.42 * (x * 0.6 + t * 0.4), glow);
            line.push_str(&format!("\x1b[38;2;{r};{g};{b}m●\x1b[0m"));
        }
        line.push_str("  \x1b[2mproxy-fire\x1b[0m");
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
        std::thread::sleep(FRAME);
    }
    // settle: strip breathes once, then clears
    let _ = out.write_all(
        format!(
            "\r\x1b[2K\x1b[38;2;120;200;255m{}\x1b[0m\x1b[2m  proxy-fire ready\x1b[0m\n",
            "▬".repeat(WIDTH / 2)
        )
        .as_bytes(),
    );
    let _ = out.flush();
    std::thread::sleep(Duration::from_millis(90));
}

/// HSV-ish to RGB, brightness = glow (0..1).
fn hue_rgb(hue: f64, brightness: f64) -> (u8, u8, u8) {
    let h = hue.rem_euclid(1.0) * 6.0;
    let (r, g, b) = match h as i32 {
        0 => (1.0, h, 0.0),
        1 => (1.0 - (h - 1.0), 1.0, 0.0),
        2 => (0.0, 1.0, h - 2.0),
        3 => (0.0, 1.0 - (h - 3.0), 1.0),
        4 => (h - 4.0, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - (h - 5.0)),
    };
    let v = brightness.min(1.0);
    (
        (r * 255.0 * v) as u8,
        (g * 255.0 * v) as u8,
        (b * 255.0 * v) as u8,
    )
}

/// One quick confirmation blip, used after the animation (non-TTY safe).
pub fn ready_banner(addr: &str) {
    if std::io::stdout().is_terminal() {
        eprintln!("\x1b[38;2;120;200;255m▬\x1b[0m \x1b[1mproxy-fire\x1b[0m \x1b[2mlistening\x1b[0m \x1b[36m{addr}\x1b[0m");
    }
}

/// Simple elapsed helper for callers that want to time the boot.
pub fn elapsed_since(start: Instant) -> Duration {
    start.elapsed()
}
