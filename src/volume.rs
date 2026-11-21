//! The sound level: one setting for every video, saved in `volume.json` next to `screen.json`. The audio callback
//! (`audio.rs`) reads the gain from `current_gain`, so a change is heard at once, on any video.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use directories::ProjectDirs;

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Volume {
    /// 0 to 100.
    pub percent: u8,
    pub muted: bool,
}

impl Default for Volume {
    fn default() -> Self {
        Self { percent: 100, muted: false }
    }
}

impl Volume {
    pub fn set_percent(&mut self, p: f32) {
        self.percent = if p.is_finite() { p.round().clamp(0.0, 100.0) as u8 } else { 100 };
    }

    /// What the samples are multiplied by. Squared, so the slider feels even: half way is a quarter of the power-ish,
    /// which is about half as loud to the ear.
    pub fn gain(&self) -> f32 {
        if self.muted { return 0.0; }
        let f = self.percent.min(100) as f32 / 100.0;
        f * f
    }
}

/// The gain the audio thread uses (the bits of an f32).
static GAIN: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

/// Make `v` the level of the sound now playing and of every video after.
pub fn publish(v: &Volume) {
    GAIN.store(v.gain().to_bits(), Ordering::Relaxed);
}

pub fn current_gain() -> f32 {
    f32::from_bits(GAIN.load(Ordering::Relaxed))
}

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "equirect").map(|d| d.config_dir().join("volume.json"))
}

/// The saved level. A missing file is normal (full volume); an unreadable one is reported, the default is used, and the
/// next save replaces it.
pub fn load() -> (Volume, Option<String>) {
    match config_path() {
        Some(p) => load_from(&p),
        None => (Volume::default(), Some("no config folder: the volume cannot be saved".into())),
    }
}

pub fn load_from(path: &Path) -> (Volume, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Volume>(&text) {
            Ok(mut v) => {
                v.set_percent(v.percent as f32);
                (v, None)
            }
            Err(e) => (
                Volume::default(),
                Some(format!("{} is not valid ({e}): using full volume, the next change replaces it", path.display())),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Volume::default(), None),
        Err(e) => (Volume::default(), Some(format!("cannot read {}: {e}", path.display()))),
    }
}

pub fn save(v: &Volume) -> Result<(), String> {
    let p = config_path().ok_or("no config folder: the volume cannot be saved")?;
    save_to(&p, v)
}

pub fn save_to(path: &Path, v: &Volume) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Two clicks on the speaker this close together are a double click: mute or unmute.
pub const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);
/// How long the volume row stays over the seek bar after the last click or slider move.
pub const ROW_SHOWN: std::time::Duration = std::time::Duration::from_secs(4);

/// The speaker button: a click opens the volume slider, a double click mutes or unmutes.
#[derive(Default)]
pub struct VolumeRow {
    last_click: Option<std::time::Instant>,
    open_until: Option<std::time::Instant>,
}

impl VolumeRow {
    /// A click on the speaker. Opens (or keeps open) the volume row; true when it was the second click of a double click.
    pub fn click(&mut self, now: std::time::Instant) -> bool {
        let double = self.last_click.is_some_and(|t| now.duration_since(t) <= DOUBLE_CLICK);
        self.last_click = if double { None } else { Some(now) };
        self.touch(now);
        double
    }

    /// Keep the row open a while longer (the slider moved).
    pub fn touch(&mut self, now: std::time::Instant) {
        self.open_until = Some(now + ROW_SHOWN);
    }

    pub fn is_open(&self, now: std::time::Instant) -> bool {
        self.open_until.is_some_and(|t| now < t)
    }
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[test]
    fn gain_follows_the_slider_and_mute_silences() {
        let mut v = Volume::default();
        assert_eq!(v.gain(), 1.0, "full volume leaves the sound alone");
        v.set_percent(50.0);
        assert_eq!(v.gain(), 0.25);
        v.set_percent(0.0);
        assert_eq!(v.gain(), 0.0);
        v.set_percent(80.0);
        v.muted = true;
        assert_eq!(v.gain(), 0.0, "muted is silent but keeps the level for unmuting");
        assert_eq!(v.percent, 80);
        v.set_percent(250.0);
        assert_eq!(v.percent, 100);
        v.set_percent(f32::NAN);
        assert_eq!(v.percent, 100, "not a number is full volume, never silence by accident");
    }

    #[test]
    fn saved_level_comes_back_and_damage_is_reported_not_trusted() {
        let dir = std::env::temp_dir().join(format!("equirect-volume-test-{}", std::process::id()));
        let path = dir.join("volume.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_from(&path), (Volume::default(), None), "a missing file is just the default");

        let v = Volume { percent: 35, muted: true };
        save_to(&path, &v).unwrap();
        assert_eq!(load_from(&path), (v, None));

        std::fs::write(&path, "{ nope").unwrap();
        let (got, problem) = load_from(&path);
        assert_eq!(got, Volume::default());
        assert!(problem.unwrap().contains("not valid"), "the damage is reported");

        std::fs::write(&path, r#"{"percent": 200}"#).unwrap();
        assert_eq!(load_from(&path).0.percent, 100, "an out-of-range level is pulled back in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_opens_the_volume_row_and_a_quick_second_click_is_a_double_click() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut row = VolumeRow::default();
        assert!(!row.is_open(t0));
        assert!(!row.click(t0), "the first click is a plain click");
        assert!(row.is_open(t0 + Duration::from_secs(3)), "and the row opens");
        assert!(!row.is_open(t0 + ROW_SHOWN + Duration::from_millis(1)), "then closes by itself");
        assert!(row.click(t0 + Duration::from_millis(300)), "a second click within the double-click time");
        assert!(!row.click(t0 + Duration::from_millis(500)), "a double click is used up: the next click starts over");
        assert!(!row.click(t0 + Duration::from_secs(2)), "a slow second click is just another click");
        row.touch(t0 + Duration::from_secs(10));
        assert!(row.is_open(t0 + Duration::from_secs(13)), "moving the slider keeps the row open");
    }
}
