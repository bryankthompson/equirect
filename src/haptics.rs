//! The Handy settings the player sends to the bridge (`tools/handy-sync`) in every position message.
//!
//! Stroke range, invert and the auto-mode settings are one value for all videos and live here, in `haptics.json` next to the
//! per-video settings. The offset differs per script, so it is saved per video (`VideoSettings::haptics_offset_ms`).
//!
//! Auto mode (the bridge makes strokes when a video has no funscript, or instead of it) takes its presets from
//! `tools/auto_presets.json`, mined from real funscripts by `tools/funscript_patterns.py`; the file is compiled in.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The two stroke ends stay at least this far apart (percent), or the Handy would barely move.
pub const MIN_GAP: u8 = 5;
/// The offset slider goes from `-OFFSET_RANGE_MS` to `+OFFSET_RANGE_MS`.
pub const OFFSET_RANGE_MS: i32 = 1000;

#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Haptics {
    /// Lowest and highest part of the Handy's travel to use, in percent (0 to 100).
    #[serde(default = "default_min")]
    pub stroke_min: u8,
    #[serde(default = "default_max")]
    pub stroke_max: u8,
    /// Flip every stroke upside down.
    #[serde(default)]
    pub invert: bool,
    #[serde(default)]
    pub auto: Auto,
}

fn default_min() -> u8 { 0 }
fn default_max() -> u8 { 100 }

impl Default for Haptics {
    fn default() -> Self {
        Self { stroke_min: default_min(), stroke_max: default_max(), invert: false, auto: Auto::default() }
    }
}

impl Haptics {
    /// Move the low end, never closer than `MIN_GAP` to the high end.
    pub fn set_stroke_min(&mut self, v: u8) {
        self.stroke_min = v.min(self.stroke_max.saturating_sub(MIN_GAP));
    }

    /// Move the high end, never closer than `MIN_GAP` to the low end.
    pub fn set_stroke_max(&mut self, v: u8) {
        self.stroke_max = v.min(100).max((self.stroke_min + MIN_GAP).min(100));
    }

    /// A hand-edited or damaged file can hold anything: bring it back into range.
    fn sanitized(mut self) -> Self {
        self.stroke_max = self.stroke_max.clamp(MIN_GAP, 100);
        self.stroke_min = self.stroke_min.min(self.stroke_max - MIN_GAP);
        self.auto = self.auto.sanitized();
        self
    }
}

// ── auto mode ────────────────────────────────────────────────────────────────

/// When the bridge makes its own strokes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMode {
    #[default]
    Off,
    /// Only for a video that has no funscript.
    NoScript,
    /// Always, instead of the funscript.
    Always,
}

/// A preset from `tools/auto_presets.json`: its name and the slider values it starts with.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AutoPreset {
    pub id: String,
    pub name: String,
    pub speed: [f32; 2],
    pub min_stroke: f32,
    pub pattern_s: [f32; 2],
    pub pause_s: [f32; 2],
}

#[derive(serde::Deserialize)]
struct PresetFile {
    presets: Vec<AutoPreset>,
}

static PRESET_JSON: &str = include_str!("../tools/auto_presets.json");

/// The presets, in the order of the file (the most common style first).
pub fn presets() -> &'static [AutoPreset] {
    static LIST: std::sync::OnceLock<Vec<AutoPreset>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let file: PresetFile = serde_json::from_str(PRESET_JSON).expect("tools/auto_presets.json is not valid (a test checks it)");
        assert!(!file.presets.is_empty(), "tools/auto_presets.json has no presets");
        file.presets
    })
}

/// Auto-mode settings, as the bridge wants them (`tools/handy_auto.py`, `docs/handy-sync.md`). Speed is a percent of the
/// Handy's top speed, stroke a percent of its travel, the times are seconds (a pause range of 0 to 0 means no pauses).
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)] // a block with fields missing gets the defaults for those
pub struct Auto {
    pub mode: AutoMode,
    pub preset: String,
    pub speed: [u8; 2],
    pub stroke: [u8; 2],
    pub min_stroke: u8,
    pub pattern_s: [f32; 2],
    pub pause_s: [f32; 2],
    /// The random seed the bridge builds the script from: the same seed, style and sliders always give the same script.
    /// 0 means a new seed for each video (the bridge picks it and logs it).
    pub seed: u32,
}

/// A seed to keep, six digits so it is easy to note down: made from the clock, mixed so two presses a moment apart differ.
pub fn new_seed() -> u32 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
    six_digits(nanos)
}

fn six_digits(n: u64) -> u32 {
    let mut x = n ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    100_000 + (x % 900_000) as u32
}

fn tenth(v: f32) -> f32 { (v * 10.0).round() / 10.0 }

impl Default for Auto {
    fn default() -> Self {
        let mut a = Self {
            mode: AutoMode::Off, preset: String::new(), speed: [0, 0], stroke: [0, 100],
            min_stroke: 0, pattern_s: [0.0, 0.0], pause_s: [0.0, 0.0], seed: 0,
        };
        a.apply_preset(&presets()[0]);
        a
    }
}

impl Auto {
    /// Choose a preset: its slider values replace the current ones (the stroke range and the seed stay: they are yours, not the style's).
    pub fn apply_preset(&mut self, p: &AutoPreset) {
        self.preset = p.id.clone();
        self.speed = [p.speed[0].round() as u8, p.speed[1].round() as u8];
        self.min_stroke = p.min_stroke.round() as u8;
        self.pattern_s = [tenth(p.pattern_s[0]), tenth(p.pattern_s[1])];
        self.pause_s = [tenth(p.pause_s[0]), tenth(p.pause_s[1])];
        self.sanitize_in_place();
    }

    /// Anything a hand-edited or old file can hold, brought back to values the bridge accepts.
    fn sanitized(mut self) -> Self {
        if !presets().iter().any(|p| p.id == self.preset) {
            self.apply_preset(&presets()[0]);
        }
        self.sanitize_in_place();
        self
    }

    fn sanitize_in_place(&mut self) {
        fix_pair(&mut self.speed, 0, 100);
        fix_pair(&mut self.stroke, 0, 100);
        self.min_stroke = self.min_stroke.min(100);
        for (pair, lo) in [(&mut self.pattern_s, 0.1_f32), (&mut self.pause_s, 0.0_f32)] {
            for v in pair.iter_mut() {
                *v = if v.is_finite() { tenth(v.clamp(lo, 120.0)) } else { lo };
            }
            if pair[0] > pair[1] {
                pair[1] = pair[0];
            }
        }
    }
}

/// Both ends inside `lo..=hi` and at least `MIN_GAP` apart.
fn fix_pair(p: &mut [u8; 2], lo: u8, hi: u8) {
    p[1] = p[1].clamp(lo + MIN_GAP, hi);
    p[0] = p[0].clamp(lo, p[1] - MIN_GAP);
}

/// Offset clamped to the slider range and to whole 10 ms steps.
pub fn snap_offset(ms: f32) -> i32 {
    (((ms / 10.0).round() as i32) * 10).clamp(-OFFSET_RANGE_MS, OFFSET_RANGE_MS)
}

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "equirect").map(|d| d.config_dir().join("haptics.json"))
}

/// The saved settings. A missing file is normal (defaults); an unreadable one is reported, the defaults are used,
/// and the next save replaces it.
pub fn load() -> (Haptics, Option<String>) {
    match config_path() {
        Some(p) => load_from(&p),
        None => (Haptics::default(), Some("no config folder: haptics settings cannot be saved".into())),
    }
}

pub fn load_from(path: &Path) -> (Haptics, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Haptics>(&text) {
            Ok(h) => (h.sanitized(), None),
            Err(e) => (
                Haptics::default(),
                Some(format!("{} is not valid ({e}): using the defaults, the next change replaces it", path.display())),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Haptics::default(), None),
        Err(e) => (Haptics::default(), Some(format!("cannot read {}: {e}", path.display()))),
    }
}

pub fn save(h: &Haptics) -> Result<(), String> {
    let p = config_path().ok_or("no config folder: haptics settings cannot be saved")?;
    save_to(&p, h)
}

pub fn save_to(path: &Path, h: &Haptics) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(h).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod haptics_tests {
    use super::*;

    #[test]
    fn the_stroke_ends_keep_their_gap() {
        let mut h = Haptics::default();
        h.set_stroke_min(98);
        assert_eq!((h.stroke_min, h.stroke_max), (95, 100));
        h.set_stroke_max(2);
        assert_eq!((h.stroke_min, h.stroke_max), (95, 100), "max cannot pass min + gap");
        h.set_stroke_min(0);
        h.set_stroke_max(3);
        assert_eq!((h.stroke_min, h.stroke_max), (0, 5));
    }

    #[test]
    fn offset_snaps_to_ten_and_stays_in_range() {
        assert_eq!(snap_offset(104.0), 100);
        assert_eq!(snap_offset(-106.0), -110);
        assert_eq!(snap_offset(5000.0), 1000);
        assert_eq!(snap_offset(-5000.0), -1000);
    }

    #[test]
    fn saved_settings_come_back_and_damage_is_reported_not_trusted() {
        let dir = std::env::temp_dir().join(format!("equirect-haptics-test-{}", std::process::id()));
        let file = dir.join("haptics.json");
        assert_eq!(load_from(&file), (Haptics::default(), None), "no file is normal");

        let mut h = Haptics { stroke_min: 20, stroke_max: 80, invert: true, auto: Auto::default() };
        h.auto.mode = AutoMode::NoScript;
        save_to(&file, &h).unwrap();
        assert_eq!(load_from(&file), (h, None));

        std::fs::write(&file, r#"{"stroke_min": 90, "stroke_max": 40}"#).unwrap();
        let (fixed, err) = load_from(&file);
        assert!(err.is_none() && fixed.stroke_max - fixed.stroke_min >= MIN_GAP, "{fixed:?}");

        std::fs::write(&file, "not json").unwrap();
        let (fallback, err) = load_from(&file);
        assert_eq!(fallback, Haptics::default());
        assert!(err.unwrap().contains("not valid"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_compiled_in_presets_are_usable() {
        let list = presets();
        assert!(list.len() >= 4, "{} presets", list.len());
        for p in list {
            let mut a = Auto::default();
            a.apply_preset(p);
            assert_eq!(a.preset, p.id);
            assert!(a.speed[1] - a.speed[0] >= MIN_GAP, "{}: {:?}", p.id, a.speed);
            assert!(a.pattern_s[0] >= 0.1 && a.pattern_s[0] <= a.pattern_s[1], "{}: {:?}", p.id, a.pattern_s);
            assert!(a.pause_s[0] <= a.pause_s[1] && a.pause_s[1] <= 120.0, "{}: {:?}", p.id, a.pause_s);
        }
    }

    #[test]
    fn a_preset_keeps_the_stroke_range_and_the_mode() {
        let mut a = Auto::default();
        a.mode = AutoMode::Always;
        a.stroke = [10, 70];
        a.apply_preset(presets().last().unwrap());
        assert_eq!((a.mode, a.stroke), (AutoMode::Always, [10, 70]));
    }

    #[test]
    fn old_and_damaged_auto_settings_are_brought_back_in_range() {
        // an older haptics.json has no auto block at all
        let old: Haptics = serde_json::from_str(r#"{"stroke_min": 10, "stroke_max": 90, "invert": false}"#).unwrap();
        assert_eq!(old.auto, Auto::default());
        // a known style with nonsense numbers: every value is brought back into range, the mode is kept
        let id = &presets()[0].id;
        let damaged: Haptics = serde_json::from_str(&format!(
            r#"{{"auto": {{"mode": "always", "preset": "{id}", "speed": [99, 1], "stroke": [0, 3], "min_stroke": 250,
                          "pattern_s": [50.0, 2.0], "pause_s": [-1.0, 900.0]}}}}"#)).unwrap();
        let fixed = damaged.sanitized().auto;
        assert_eq!(fixed.mode, AutoMode::Always, "the mode is kept");
        assert_eq!(&fixed.preset, id);
        assert!(fixed.speed[1] - fixed.speed[0] >= MIN_GAP && fixed.stroke[1] - fixed.stroke[0] >= MIN_GAP, "{fixed:?}");
        assert!(fixed.min_stroke <= 100 && fixed.pattern_s[0] <= fixed.pattern_s[1] && fixed.pause_s == [0.0, 120.0], "{fixed:?}");

        // a style that no longer exists: the first style's values, the mode and the stroke range stay
        let gone: Haptics = serde_json::from_str(
            r#"{"auto": {"mode": "no_script", "preset": "gone", "stroke": [10, 60], "speed": [1, 2]}}"#).unwrap();
        let fixed = gone.sanitized().auto;
        assert_eq!((fixed.mode, fixed.stroke), (AutoMode::NoScript, [10, 60]));
        assert_eq!(fixed.preset, presets()[0].id);
        assert_eq!(fixed.speed, [presets()[0].speed[0].round() as u8, presets()[0].speed[1].round() as u8]);
    }

    #[test]
    fn the_message_form_is_what_the_bridge_expects() {
        let mut a = Auto::default();
        a.mode = AutoMode::NoScript;
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["mode"], "no_script");
        for key in ["preset", "speed", "stroke", "min_stroke", "pattern_s", "pause_s", "seed"] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["stroke"], serde_json::json!([0, 100]));
    }

    #[test]
    fn seeds_are_six_digits_and_differ() {
        let seeds: Vec<u32> = (0..2000u64).map(|n| six_digits(n * 7919)).collect();
        assert!(seeds.iter().all(|s| (100_000..=999_999).contains(s)));
        let distinct: std::collections::HashSet<_> = seeds.iter().collect();
        assert!(distinct.len() > 1900, "{} distinct of 2000", distinct.len());
        assert!((100_000..=999_999).contains(&new_seed()));
    }

    #[test]
    fn a_style_keeps_the_seed_and_an_old_file_has_none() {
        let mut a = Auto::default();
        assert_eq!(a.seed, 0, "a new seed for each video unless one is kept");
        a.seed = 424242;
        a.apply_preset(presets().last().unwrap());
        assert_eq!(a.seed, 424242);
        let old: Haptics = serde_json::from_str(r#"{"auto": {"mode": "always"}}"#).unwrap();
        assert_eq!(old.auto.seed, 0);
        assert_eq!(serde_json::to_value(&a).unwrap()["seed"], 424242);
    }
}
