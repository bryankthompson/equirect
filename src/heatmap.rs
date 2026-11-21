//! The funscript's heat map: how fast the strokes are along the video, drawn as a strip of colour above the seek bar
//! (`ui::Ui::slider_sized_heat`). Read-only: the player only looks at the script, the bridge (`tools/handy-sync`) is
//! what moves the Handy. Which file: the plain `<video>.funscript`, else the first variant `<video>.<name>.funscript`.

use std::path::{Path, PathBuf};

/// Script units per second the Handy's firmware tops out at (see docs): a bucket this fast is full red.
const SPEED_CAP: f64 = 364.0;

/// The strokes of a script: (seconds into the video, position 0..100), in time order.
pub struct Script {
    actions: Vec<(f64, f32)>,
}

/// The funscript beside `video`, or None when there is none. A script that exists but cannot be read or understood is
/// reported in player.log (and treated as no script), never silently skipped.
pub fn read_beside(video: &Path) -> Option<Script> {
    let path = script_path(video)?;
    let started = std::time::Instant::now();
    let read = std::fs::read(&path);
    crate::diag::slow(&format!("reading {}", path.display()), started, 200);
    match read {
        Ok(bytes) => match parse(&String::from_utf8_lossy(&bytes)) {
            Ok(s) => Some(s),
            Err(e) => {
                crate::diag::log(&format!("heat map: {}: {e}", path.display()));
                None
            }
        },
        Err(e) => {
            crate::diag::log(&format!("cannot read {}: {e}", path.display()));
            None
        }
    }
}

/// `clip.funscript` if it is there, else the first (by name) of `clip.<variant>.funscript`; names compare without case.
fn script_path(video: &Path) -> Option<PathBuf> {
    let dir = video.parent()?;
    let stem = video.file_stem()?.to_string_lossy().to_lowercase();
    let plain = format!("{stem}.funscript");
    let prefix = format!("{stem}.");
    let mut variants: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name == plain {
            return Some(entry.path());
        }
        if name.starts_with(&prefix) && name.ends_with(".funscript") {
            variants.push(entry.path());
        }
    }
    variants.sort();
    variants.into_iter().next()
}

pub fn parse(text: &str) -> Result<Script, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON ({e})"))?;
    let list = json.get("actions").and_then(|a| a.as_array()).ok_or("no \"actions\" list")?;
    let mut actions: Vec<(f64, f32)> = list.iter()
        .filter_map(|a| Some((a.get("at")?.as_f64()? / 1000.0, a.get("pos")?.as_f64()? as f32)))
        .filter(|(at, pos)| at.is_finite() && *at >= 0.0 && pos.is_finite())
        .collect();
    if actions.len() < 2 {
        return Err("fewer than two strokes".into());
    }
    actions.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(Script { actions })
}

impl Script {
    /// `cells` heat values (0 still, 255 as fast as the Handy goes) across a video of `duration_secs`: each is the mean
    /// speed of the strokes in its slice of time. The root of the speed is taken so that a calm part still shows.
    pub fn heat(&self, duration_secs: f64, cells: usize) -> Vec<u8> {
        if cells == 0 || !(duration_secs > 0.0) {
            return vec![0; cells];
        }
        let cell = duration_secs / cells as f64;
        let mut travelled = vec![0.0_f64; cells]; // sum of speed x seconds, per cell
        for w in self.actions.windows(2) {
            let ((t0, p0), (t1, p1)) = (w[0], w[1]);
            let (a, b) = (t0, t1.min(duration_secs));
            if t1 <= t0 || b <= a { continue; }
            let speed = (p1 - p0).abs() as f64 / (t1 - t0);
            let first = ((a / cell) as usize).min(cells - 1);
            let last = ((b / cell) as usize).min(cells - 1);
            for (i, t) in travelled.iter_mut().enumerate().take(last + 1).skip(first) {
                let overlap = ((i + 1) as f64 * cell).min(b) - (i as f64 * cell).max(a);
                if overlap > 0.0 { *t += speed * overlap; }
            }
        }
        travelled.iter()
            .map(|t| ((t / cell / SPEED_CAP).min(1.0).sqrt() * 255.0).round() as u8)
            .collect()
    }
}

#[cfg(test)]
mod heatmap_tests {
    use super::*;

    #[test]
    fn a_fast_part_is_hotter_than_a_slow_part_and_a_gap_is_cold() {
        // 0-10 s: a full stroke every half second (200 units/s); 10-20 s: one slow drift; then nothing.
        let mut text = String::from(r#"{"actions":["#);
        for i in 0..=20 {
            text += &format!(r#"{{"at":{},"pos":{}}},"#, i * 500, if i % 2 == 0 { 0 } else { 100 });
        }
        text += r#"{"at":20000,"pos":10}]}"#; // from the last stroke (0 at 10 s) a drift to 10 by 20 s
        let heat = parse(&text).unwrap().heat(40.0, 4);
        assert!(heat[0] > 150, "fast strokes are hot: {heat:?}");
        assert!(heat[1] > 0 && heat[1] < heat[0], "a slow drift is cooler: {heat:?}");
        assert_eq!((heat[2], heat[3]), (0, 0), "no strokes, no heat: {heat:?}");
    }

    #[test]
    fn a_bad_script_is_an_error_and_a_messy_one_is_tidied() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"actions":[]}"#).is_err());
        assert!(parse(r#"{"actions":[{"at":5,"pos":1}]}"#).is_err(), "one stroke is not a script");
        // Out of order, with a broken entry: sorted, the broken one dropped.
        let s = parse(r#"{"actions":[{"at":2000,"pos":50},{"at":"x","pos":1},{"at":0,"pos":0}]}"#).unwrap();
        assert_eq!(s.actions, vec![(0.0, 0.0), (2.0, 50.0)]);
        assert_eq!(s.heat(0.0, 3), vec![0, 0, 0], "an unknown length gives no heat, not a crash");
    }

    #[test]
    fn the_plain_script_wins_over_a_variant_and_a_variant_is_found_alone() {
        let dir = std::env::temp_dir().join(format!("equirect-heat-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.mp4", "A.Bounce.funscript", "a.FunScript", "b.mp4", "b.Wave.funscript", "b.Alt.funscript", "c.mp4"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let name = |v: &str| script_path(&dir.join(v)).map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
        assert_eq!(name("a.mp4"), Some("a.FunScript".into()), "the plain script, whatever its case, beats a variant");
        assert_eq!(name("b.mp4"), Some("b.Alt.funscript".into()), "the first variant by name");
        assert_eq!(name("c.mp4"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
