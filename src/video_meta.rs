use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use sha2::{Digest, Sha256};

use crate::ui::settings::{Projection, StereoLayout, VideoMode, VideoSettings};

/// Guess the video mode from the file name, DeoVR/SLR style: `..._180_LR.mp4`,
/// `..._360_TB.mp4`, `..._SBS.mp4`, `..._fisheye190_...`. Returns `None` when the name
/// says nothing (the caller then keeps the previous video's settings).
pub fn detect_from_name(source: &str) -> Option<VideoSettings> {
    guess_layout(source, None)
}

/// Same, and when the video size is known it also fills in what the name leaves out: a 180 file
/// with no stereo word is left/right if its frame is 2:1 (both eyes side by side), top/bottom
/// if it is tall; a big unmarked 2:1 frame is taken as side-by-side 180 (the library convention).
pub fn guess_layout(source: &str, dims: Option<(u32, u32)>) -> Option<VideoSettings> {
    let name = source.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(source).to_lowercase();
    let tokens: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).collect();
    let has = |set: &[&str]| tokens.iter().any(|t| set.contains(t));
    let starts = |p: &str| tokens.iter().any(|t| t.starts_with(p));

    let fisheye = starts("fisheye") || starts("mkx") || has(&["rf52"]);
    let mut mode = if has(&["360", "vr360"]) || starts("360x") {
        Some(VideoMode::View360)
    } else if fisheye || has(&["180", "vr180"]) || starts("180x") {
        Some(VideoMode::View180)
    } else {
        None
    };
    let mut stereo = if has(&["lr", "sbs", "3dh", "hsbs"]) {
        Some(StereoLayout::LR)
    } else if has(&["rl"]) {
        Some(StereoLayout::RL)
    } else if has(&["tb", "ou", "3dv", "htb"]) {
        Some(StereoLayout::TB)
    } else if has(&["bt"]) {
        Some(StereoLayout::BT)
    } else {
        None
    };
    if let Some((w, h)) = dims {
        let a = w as f32 / h.max(1) as f32;
        if stereo.is_none() {
            match mode {
                Some(VideoMode::View180) if a >= 1.7 => stereo = Some(StereoLayout::LR),
                Some(VideoMode::View180) if a <= 0.6 => stereo = Some(StereoLayout::TB),
                Some(VideoMode::View360) if a >= 3.4 => stereo = Some(StereoLayout::LR),
                Some(VideoMode::View360) if (0.8..=1.25).contains(&a) => stereo = Some(StereoLayout::TB),
                _ => {}
            }
        }
        if mode.is_none() && stereo.is_none() && w >= 3840 && (1.9..=2.1).contains(&a) {
            mode = Some(VideoMode::View180);
            stereo = Some(StereoLayout::LR);
        }
    }
    if mode.is_none() && stereo.is_none() {
        return None;
    }
    let mut s = VideoSettings::new();
    s.mode = mode.unwrap_or(VideoMode::Sbs3D);
    s.stereo = stereo.unwrap_or(StereoLayout::OneView);
    if fisheye {
        s.proj = Projection::Fisheye;
    }
    Some(s)
}

/// Per-video metadata stored in the user config directory.
/// Saved as `video-<sha256-of-absolute-path>.json`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VideoMeta {
    pub settings: VideoSettings,
    /// Where playback stopped, in seconds, if it is worth coming back to (`resume.rs` decides). Old files have none.
    #[serde(default)]
    pub position_secs: Option<f64>,
}

impl VideoMeta {
    /// Settings only. Saving it keeps a position already on file (see `save`).
    pub fn new(settings: VideoSettings) -> Self {
        Self { settings, position_secs: None }
    }
}

fn config_meta_path(key: &str) -> Option<std::path::PathBuf> {
    let dirs = ProjectDirs::from("", "", "equirect")?;
    let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
    let mut p = dirs.config_dir().to_path_buf();
    p.push(format!("video-{}.json", hash));
    Some(p)
}

fn meta_path(video_path: &Path) -> Option<std::path::PathBuf> {
    let canonical = video_path.canonicalize().ok()
        .unwrap_or_else(|| video_path.to_path_buf());
    config_meta_path(&canonical.to_string_lossy())
}

/// Load saved metadata for `video_path`.  Returns `None` if no metadata exists yet.
pub fn load(video_path: &Path) -> Option<VideoMeta> {
    let path = meta_path(video_path)?;
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save metadata for `video_path`.  Silently ignores I/O errors. A `meta` with no position keeps the one on file, so saving
/// settings never makes a video forget where it stopped; `store` is the one that sets or clears the position.
pub fn save(video_path: &Path, meta: &VideoMeta) {
    let Some(path) = meta_path(video_path) else { return };
    write_meta(&path, &keeping_position(&path, meta));
}

/// Save exactly `meta`, position included (None forgets it).
pub fn store(video_path: &Path, meta: &VideoMeta) {
    let Some(path) = meta_path(video_path) else { return };
    write_meta(&path, meta);
}

/// Load saved metadata for a URL.  Returns `None` if no metadata exists yet.
pub fn load_url(url: &str) -> Option<VideoMeta> {
    let path = config_meta_path(url)?;
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save metadata for a URL.  Silently ignores I/O errors. Keeps the saved position like `save`.
pub fn save_url(url: &str, meta: &VideoMeta) {
    let Some(path) = config_meta_path(url) else { return };
    write_meta(&path, &keeping_position(&path, meta));
}

/// Save exactly `meta` for a URL, position included.
pub fn store_url(url: &str, meta: &VideoMeta) {
    let Some(path) = config_meta_path(url) else { return };
    write_meta(&path, meta);
}

/// `meta`, with the position already in the file at `path` when `meta` has none of its own.
fn keeping_position(path: &Path, meta: &VideoMeta) -> VideoMeta {
    let mut out = VideoMeta { settings: meta.settings.clone(), position_secs: meta.position_secs };
    if out.position_secs.is_none() {
        out.position_secs = std::fs::read_to_string(path).ok()
            .and_then(|t| serde_json::from_str::<VideoMeta>(&t).ok())
            .and_then(|m| m.position_secs);
    }
    out
}

fn write_meta(path: &std::path::Path, meta: &VideoMeta) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(meta) {
        let _ = std::fs::write(path, json);
    }
}

// ── app-wide state (state.json) ────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AppState {
    last_dir: Option<PathBuf>,
    /// Maps a volume root path (as a string) to the last browsed directory
    /// within that volume.  Missing on old state files; defaults to empty.
    #[serde(default)]
    volume_last_dirs: HashMap<String, PathBuf>,
}

fn config_dir() -> Option<PathBuf> {
    Some(ProjectDirs::from("", "", "equirect")?.config_dir().to_path_buf())
}

fn state_path() -> Option<PathBuf> {
    let mut p = config_dir()?;
    p.push("state.json");
    Some(p)
}

fn load_app_state() -> AppState {
    let Some(path) = state_path() else { return AppState::default() };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_app_state(state: &AppState) {
    let Some(path) = state_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&path, json);
    }
}

/// Returns the last browsed directory if it was saved and still exists.
pub fn load_last_dir() -> Option<PathBuf> {
    let dir = load_app_state().last_dir?;
    if dir.is_dir() { Some(dir) } else { None }
}

/// Persist `dir` as the last browsed directory.
pub fn save_last_dir(dir: &Path) {
    let mut state = load_app_state();
    state.last_dir = Some(dir.to_path_buf());
    save_app_state(&state);
}

// ── per-volume last-directory ──────────────────────────────────────────────

/// Persist `dir` as the last browsed directory for the given `volume_root`.
pub fn save_volume_last_dir(volume_root: &Path, dir: &Path) {
    let mut state = load_app_state();
    let key = volume_root.to_string_lossy().into_owned();
    state.volume_last_dirs.insert(key, dir.to_path_buf());
    save_app_state(&state);
}

/// Return the best directory to navigate to when the user selects `volume_root`.
///
/// Loads the last saved directory for that volume, then walks up the path
/// until an existing directory is found.  Falls back to `volume_root` itself.
pub fn resolve_dir_for_volume(volume_root: &Path) -> PathBuf {
    let key = volume_root.to_string_lossy().into_owned();
    let saved = load_app_state().volume_last_dirs.get(&key).cloned();

    if let Some(mut dir) = saved {
        loop {
            if dir.is_dir() {
                return dir;
            }
            // Don't climb above the volume root.
            if dir == volume_root {
                break;
            }
            match dir.parent() {
                Some(p) => dir = p.to_path_buf(),
                None => break,
            }
        }
    }

    volume_root.to_path_buf()
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn lay(name: &str, dims: Option<(u32, u32)>) -> Option<(VideoMode, StereoLayout, bool)> {
        guess_layout(name, dims).map(|s| (s.mode, s.stereo, s.proj == Projection::Fisheye))
    }

    #[test]
    fn named_layouts() {
        assert_eq!(lay("292_quad_ranger_180x180_3dh.mp4", None), Some((VideoMode::View180, StereoLayout::LR, false)));
        assert_eq!(lay("clip_360_TB.mp4", None), Some((VideoMode::View360, StereoLayout::TB, false)));
        assert_eq!(lay("x_MKX200.mp4", None), Some((VideoMode::View180, StereoLayout::OneView, true)));
        assert_eq!(lay("movie_SBS.mp4", None), Some((VideoMode::Sbs3D, StereoLayout::LR, false)));
        assert_eq!(lay("holiday.mp4", None), None);
    }

    #[test]
    fn size_fills_in_the_stereo_word() {
        // 180 with no stereo word, 2:1 frame: both eyes side by side.
        assert_eq!(lay("Studio.VR180.3840p_save.mp4", Some((7680, 3840))), Some((VideoMode::View180, StereoLayout::LR, false)));
        // Without the size it stays mono (the name alone cannot tell).
        assert_eq!(lay("Studio.VR180.3840p_save.mp4", None), Some((VideoMode::View180, StereoLayout::OneView, false)));
        // Big unmarked 2:1 frame is side-by-side 180; a normal 16:9 file stays flat.
        assert_eq!(lay("133-czechvrfetish-3d-5400x2700.mp4", Some((5400, 2700))), Some((VideoMode::View180, StereoLayout::LR, false)));
        assert_eq!(lay("holiday.mp4", Some((1920, 1080))), None);
        assert_eq!(lay("small.mp4", Some((1280, 640))), None);
    }
}

#[cfg(test)]
mod position_tests {
    use super::*;

    #[test]
    fn a_settings_save_keeps_the_saved_position_and_a_store_can_clear_it() {
        let dir = std::env::temp_dir().join(format!("equirect-position-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video-x.json");

        let mut meta = VideoMeta::new(VideoSettings::new());
        meta.position_secs = Some(754.5);
        write_meta(&path, &meta);

        // saving settings alone (what the Settings page does) must not make the video forget where it stopped
        let mut changed = VideoSettings::new();
        changed.zoom = 1.7;
        write_meta(&path, &keeping_position(&path, &VideoMeta::new(changed.clone())));
        let back: VideoMeta = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.position_secs, Some(754.5));
        assert_eq!(back.settings.zoom, 1.7);

        // a new position replaces it, and None forgets it (store writes exactly what it is given)
        let mut later = back;
        later.position_secs = Some(900.0);
        assert_eq!(keeping_position(&path, &later).position_secs, Some(900.0));
        later.position_secs = None;
        write_meta(&path, &later);
        let back: VideoMeta = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.position_secs, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_saved_before_positions_existed_still_loads() {
        let old = r#"{"settings": {"mode": "Curved2D", "proj": "Equirect", "stereo": "OneView", "zoom": 1.5, "haptics_offset_ms": 0}}"#;
        let meta: VideoMeta = serde_json::from_str(old).expect("an old file loads");
        assert_eq!(meta.position_secs, None);
        assert_eq!(meta.settings.zoom, 1.5);
    }
}
