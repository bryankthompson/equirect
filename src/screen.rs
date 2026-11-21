//! Where the flat, curved and 3D screen hangs in front of the viewer: one setting for every video, saved in `screen.json`
//! next to `haptics.json`. (Zoom, the screen's size, stays per video in `VideoSettings`.) 180 and 360 video surround the
//! viewer and ignore all of this. Drawn by `video_renderer::build_screen`.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// Metres from the viewer to the middle of the screen.
pub const DISTANCE_RANGE: (f32, f32) = (1.0, 8.0);
/// Metres above the floor of the middle of the screen (and of the point the tilt turns around).
pub const HEIGHT_RANGE: (f32, f32) = (0.0, 3.0);
/// Degrees; positive turns the screen up, for watching lying on the back.
pub const TILT_RANGE: (f32, f32) = (-90.0, 90.0);

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Screen {
    /// Percent, Curved mode only: 0 is flat, 100 is a cylinder around the viewer (every point of the screen as far from the
    /// eyes as its middle). In between, the radius is the distance divided by the fraction.
    pub curve: u8,
    pub distance: f32,
    pub height: f32,
    pub tilt: f32,
}

impl Default for Screen {
    /// The old fixed place (2.5 m away, 1.0 m up, level), with the curve a Curved screen starts at.
    fn default() -> Self {
        Self { curve: 100, distance: 2.5, height: 1.0, tilt: 0.0 }
    }
}

fn snap(v: f32, step: f32, range: (f32, f32)) -> f32 {
    if !v.is_finite() { return f32::NAN; }
    ((v / step).round() * step).clamp(range.0, range.1)
}

impl Screen {
    pub fn set_curve(&mut self, percent: f32) { self.curve = percent.round().clamp(0.0, 100.0) as u8; }
    pub fn set_distance(&mut self, m: f32) { self.distance = snap(m, 0.1, DISTANCE_RANGE); }
    pub fn set_height(&mut self, m: f32)   { self.height = snap(m, 0.1, HEIGHT_RANGE); }
    pub fn set_tilt(&mut self, deg: f32)   { self.tilt = snap(deg, 1.0, TILT_RANGE); }

    /// Every value back inside its range (a saved file may be old or edited by hand); one that is not a number becomes its default.
    pub fn sanitized(mut self) -> Self {
        let d = Self::default();
        self.set_curve(self.curve as f32);
        self.set_distance(self.distance);
        self.set_height(self.height);
        self.set_tilt(self.tilt);
        if self.distance.is_nan() { self.distance = d.distance; }
        if self.height.is_nan()   { self.height = d.height; }
        if self.tilt.is_nan()     { self.tilt = d.tilt; }
        self
    }
}

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "equirect").map(|d| d.config_dir().join("screen.json"))
}

/// The saved settings. A missing file is normal (defaults); an unreadable one is reported, the defaults are used,
/// and the next save replaces it.
pub fn load() -> (Screen, Option<String>) {
    match config_path() {
        Some(p) => load_from(&p),
        None => (Screen::default(), Some("no config folder: screen settings cannot be saved".into())),
    }
}

pub fn load_from(path: &Path) -> (Screen, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Screen>(&text) {
            Ok(s) => (s.sanitized(), None),
            Err(e) => (
                Screen::default(),
                Some(format!("{} is not valid ({e}): using the defaults, the next change replaces it", path.display())),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Screen::default(), None),
        Err(e) => (Screen::default(), Some(format!("cannot read {}: {e}", path.display()))),
    }
}

pub fn save(s: &Screen) -> Result<(), String> {
    let p = config_path().ok_or("no config folder: screen settings cannot be saved")?;
    save_to(&p, s)
}

pub fn save_to(path: &Path, s: &Screen) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod screen_tests {
    use super::*;

    #[test]
    fn values_snap_and_stay_in_range() {
        let mut s = Screen::default();
        s.set_distance(2.34);
        s.set_height(9.0);
        s.set_tilt(-120.0);
        s.set_curve(250.0);
        assert_eq!((s.distance, s.height, s.tilt, s.curve), (2.3, 3.0, -90.0, 100));
        s.set_distance(0.0);
        assert_eq!(s.distance, 1.0);
    }

    #[test]
    fn saved_settings_come_back_and_damage_is_reported_not_trusted() {
        let dir = std::env::temp_dir().join(format!("equirect-screen-test-{}", std::process::id()));
        let path = dir.join("screen.json");
        let mine = Screen { curve: 40, distance: 3.5, height: 0.6, tilt: 30.0 };
        save_to(&path, &mine).unwrap();
        assert_eq!(load_from(&path), (mine, None));

        // an old or hand-made file: missing values take the defaults, wild ones are brought back into range
        std::fs::write(&path, r#"{"distance": 99, "tilt": 500}"#).unwrap();
        let (s, problem) = load_from(&path);
        assert!(problem.is_none());
        assert_eq!((s.curve, s.distance, s.height, s.tilt), (100, 8.0, 1.0, 90.0));

        std::fs::write(&path, "not json").unwrap();
        let (s, problem) = load_from(&path);
        assert_eq!(s, Screen::default());
        assert!(problem.unwrap().contains("not valid"));

        assert_eq!(load_from(&dir.join("nothing-here.json")), (Screen::default(), None));
        std::fs::remove_dir_all(&dir).ok();
    }
}
