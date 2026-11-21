//! What the library browser remembers between runs, in `browser.json` next to the other settings: the pinned folders (with
//! folders to leave out of the listing), the thumbnail size and the panel size. One value for every video.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use directories::ProjectDirs;

/// How the browser lists a folder: text rows, or a grid of thumbnails in three sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewSize {
    List,
    Small,
    #[default]
    Medium,
    Large,
}

impl ViewSize {
    pub const ALL: [ViewSize; 4] = [ViewSize::List, ViewSize::Small, ViewSize::Medium, ViewSize::Large];

    pub fn label(self) -> &'static str {
        match self {
            ViewSize::List => "List",
            ViewSize::Small => "Small",
            ViewSize::Medium => "Medium",
            ViewSize::Large => "Large",
        }
    }

    /// The width a thumbnail tile aims for, in panel pixels (0 for the text list). The grid fits as many as the panel holds
    /// and stretches them to fill the row.
    pub fn tile_width(self) -> f32 {
        match self {
            ViewSize::List => 0.0,
            ViewSize::Small => 150.0,
            ViewSize::Medium => 230.0,
            ViewSize::Large => 350.0,
        }
    }
}

/// How big the browser panel hangs in front of the viewer. The text and thumbnails keep their pixel size, so a bigger panel
/// holds more of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelSize {
    Normal,
    #[default]
    Large,
    Huge,
}

impl PanelSize {
    pub const ALL: [PanelSize; 3] = [PanelSize::Normal, PanelSize::Large, PanelSize::Huge];

    pub fn label(self) -> &'static str {
        match self {
            PanelSize::Normal => "Normal",
            PanelSize::Large => "Large",
            PanelSize::Huge => "Huge",
        }
    }

    /// Panel size in pixels (width, height) and in metres (width, height), at the same 500 pixels per metre every panel has.
    pub fn dims(self) -> (u32, u32, f32, f32) {
        match self {
            PanelSize::Normal => (800, 600, 1.6, 1.2),
            PanelSize::Large => (1200, 900, 2.4, 1.8),
            PanelSize::Huge => (1600, 1200, 3.2, 2.4),
        }
    }
}

/// A folder to jump to from the browser. `exclude` lists folders that are left out of every listing (a Steam library in
/// the middle of a video share, say).
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Pin {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Clone, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BrowserPrefs {
    pub view: ViewSize,
    pub panel: PanelSize,
    pub pins: Vec<Pin>,
}

/// A path as one comparable string: backslashes, no trailing one, lower case (Windows paths ignore case).
fn norm(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// The last folder name of a path, for a pin made in the browser: `G:\a\b` is "b", `\\server\share` is "share", `G:\` is "G:".
pub fn default_name(path: &str) -> String {
    let n = path.replace('/', "\\");
    let n = n.trim_end_matches('\\');
    n.rsplit('\\').next().filter(|s| !s.is_empty()).unwrap_or(path).to_string()
}

impl BrowserPrefs {
    /// Empty paths go, an empty name becomes the folder's name.
    pub fn sanitized(mut self) -> Self {
        self.pins.retain(|p| !p.path.trim().is_empty());
        for p in &mut self.pins {
            if p.name.trim().is_empty() {
                p.name = default_name(&p.path);
            }
        }
        self
    }

    /// Is exactly this folder pinned?
    pub fn is_pinned(&self, folder: &str) -> bool {
        self.pins.iter().any(|p| norm(&p.path) == norm(folder))
    }

    /// Pin the folder, or take its pin away if it has one. True when it is pinned afterwards.
    pub fn toggle_pin(&mut self, folder: &str) -> bool {
        if self.is_pinned(folder) {
            self.pins.retain(|p| norm(&p.path) != norm(folder));
            false
        } else {
            self.pins.push(Pin { name: default_name(folder), path: folder.to_string(), exclude: Vec::new() });
            true
        }
    }

    /// Is `location` the pin's folder or inside it? (Lights up the pin while browsing below it.)
    pub fn within(pin: &Pin, location: &str) -> bool {
        let (p, l) = (norm(&pin.path), norm(location));
        l == p || l.starts_with(&format!("{p}\\"))
    }

    fn excluded_dirs(&self) -> Vec<String> {
        self.pins.iter().flat_map(|p| p.exclude.iter()).map(|e| norm(e)).collect()
    }
}

// ── folders left out of listings ──────────────────────────────────────────────

/// Every pin's `exclude`, normalised. Folder listings run on several threads and have no access to the prefs, so they ask here.
static EXCLUDED: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Make the prefs' exclusions the ones listings use. Call after loading and after every change.
pub fn apply_exclusions(prefs: &BrowserPrefs) {
    if let Ok(mut w) = EXCLUDED.write() {
        *w = prefs.excluded_dirs();
    }
}

/// Is this folder one a pin leaves out?
pub fn is_excluded(dir: &Path) -> bool {
    let d = norm(&dir.to_string_lossy());
    EXCLUDED.read().map(|e| e.iter().any(|x| *x == d)).unwrap_or(false)
}

// ── saved file ────────────────────────────────────────────────────────────────

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "equirect").map(|d| d.config_dir().join("browser.json"))
}

/// The saved settings. A missing file is normal (defaults); an unreadable one is reported, the defaults are used,
/// and the next save replaces it.
pub fn load() -> (BrowserPrefs, Option<String>) {
    match config_path() {
        Some(p) => load_from(&p),
        None => (BrowserPrefs::default(), Some("no config folder: browser settings cannot be saved".into())),
    }
}

pub fn load_from(path: &Path) -> (BrowserPrefs, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<BrowserPrefs>(&text) {
            Ok(p) => (p.sanitized(), None),
            Err(e) => (
                BrowserPrefs::default(),
                Some(format!("{} is not valid ({e}): using the defaults, the next change replaces it", path.display())),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (BrowserPrefs::default(), None),
        Err(e) => (BrowserPrefs::default(), Some(format!("cannot read {}: {e}", path.display()))),
    }
}

pub fn save(p: &BrowserPrefs) -> Result<(), String> {
    let path = config_path().ok_or("no config folder: browser settings cannot be saved")?;
    save_to(&path, p)
}

pub fn save_to(path: &Path, p: &BrowserPrefs) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(p).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod browser_prefs_tests {
    use super::*;

    #[test]
    fn pins_are_named_after_their_folder_and_toggle() {
        assert_eq!(default_name(r"G:\Users\me\Downloads\batch"), "batch");
        assert_eq!(default_name(r"\\server\share3\"), "share3");
        assert_eq!(default_name("G:/"), "G:");
        let mut p = BrowserPrefs::default();
        assert!(p.toggle_pin(r"\\server\share2\VR"));
        assert_eq!(p.pins[0].name, "VR");
        assert!(p.is_pinned(r"\\SERVER\share2\vr\"), "case and a trailing slash do not matter");
        assert!(!p.toggle_pin("//server/share2/VR"), "a second toggle unpins");
        assert!(p.pins.is_empty());
    }

    #[test]
    fn a_pin_lights_up_below_it_but_not_on_a_lookalike() {
        let pin = Pin { name: "x".into(), path: r"\\server\share3".into(), exclude: vec![] };
        assert!(BrowserPrefs::within(&pin, r"\\server\share3"));
        assert!(BrowserPrefs::within(&pin, r"\\server\share3\VR Collection\8K"));
        assert!(!BrowserPrefs::within(&pin, r"\\server\share30"));
        assert!(!BrowserPrefs::within(&pin, r"\\server\share2"));
    }

    #[test]
    fn excluded_folders_are_left_out_of_listings() {
        let mut p = BrowserPrefs::default();
        p.pins.push(Pin { name: "share4".into(), path: r"\\server\share4".into(), exclude: vec![r"\\server\share4\SteamLibrary".into()] });
        apply_exclusions(&p);
        assert!(is_excluded(Path::new(r"\\server\share4\steamlibrary")));
        assert!(is_excluded(Path::new(r"\\server\share4\SteamLibrary\")));
        assert!(!is_excluded(Path::new(r"\\server\share4\SteamLibrary2")));
        assert!(!is_excluded(Path::new(r"\\server\share4")));
        apply_exclusions(&BrowserPrefs::default());
        assert!(!is_excluded(Path::new(r"\\server\share4\SteamLibrary")));
    }

    #[test]
    fn saved_prefs_come_back_and_old_or_damaged_files_are_handled() {
        let dir = std::env::temp_dir().join(format!("equirect-browser-prefs-test-{}", std::process::id()));
        let path = dir.join("browser.json");
        let mine = BrowserPrefs {
            view: ViewSize::Large,
            panel: PanelSize::Huge,
            pins: vec![Pin { name: "Batch".into(), path: r"G:\x".into(), exclude: vec![r"G:\x\y".into()] }],
        };
        save_to(&path, &mine).unwrap();
        assert_eq!(load_from(&path), (mine, None));

        // a hand-written file: missing values take the defaults, a pin with no name gets its folder's, one with no path goes
        std::fs::write(&path, r#"{"pins": [{"name": "", "path": "D:\\videos"}, {"name": "empty", "path": " "}]}"#).unwrap();
        let (p, problem) = load_from(&path);
        assert!(problem.is_none());
        assert_eq!((p.view, p.panel), (ViewSize::Medium, PanelSize::Large));
        assert_eq!(p.pins, vec![Pin { name: "videos".into(), path: r"D:\videos".into(), exclude: vec![] }]);

        std::fs::write(&path, "not json").unwrap();
        let (p, problem) = load_from(&path);
        assert_eq!(p, BrowserPrefs::default());
        assert!(problem.unwrap().contains("not valid"));
        assert_eq!(load_from(&dir.join("nothing-here.json")), (BrowserPrefs::default(), None));
        std::fs::remove_dir_all(&dir).ok();
    }
}
