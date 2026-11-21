//! A small log file for "why did it freeze": `%APPDATA%\equirect\config\player.log`.
//! Only slow things are written (video opens, folder listings, frames that took too long), one line each, newest at the end.
//! The file is cut back to its last 200 KB when it passes 400 KB, so it never grows without limit.

use std::io::Write;
use std::time::Instant;

use directories::ProjectDirs;

fn path() -> Option<std::path::PathBuf> {
    Some(ProjectDirs::from("", "", "equirect")?.config_dir().join("player.log"))
}

pub fn log(msg: &str) {
    eprintln!("{msg}");
    let Some(p) = path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&p).map_or(false, |m| m.len() > 400_000) {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let keep = text.len().saturating_sub(200_000);
            let start = text[keep..].find('\n').map_or(keep, |i| keep + i + 1);
            let _ = std::fs::write(&p, &text[start..]);
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60); // UTC
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{h:02}:{m:02}:{s:02}Z {msg}");
    }
}

/// Log `what` with its duration when it took at least `min_ms`.
pub fn slow(what: &str, started: Instant, min_ms: u128) {
    let ms = started.elapsed().as_millis();
    if ms >= min_ms {
        log(&format!("slow: {what} took {ms} ms"));
    }
}
