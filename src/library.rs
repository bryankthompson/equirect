//! The user's own list of favorite videos, kept in `library.json` next to the per-video settings.
//!
//! A favorite is the source exactly as the player opened it (a file path or an http(s) URL).
//! Two spellings of the same file (case, `/` against `\`) count as one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Favorite {
    pub source: String,
    /// Seconds since the Unix epoch when it was added.
    pub added: u64,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct LibraryFile {
    #[serde(default)]
    favorites: Vec<Favorite>,
}

pub struct Library {
    path: Option<PathBuf>,
    favorites: Vec<Favorite>,
    keys: HashSet<String>,
}

/// One key for every spelling of the same source.
fn key_of(source: &str) -> String {
    source.replace('/', "\\").to_lowercase()
}

impl Library {
    /// The library in the user's equirect config folder. A broken file is reported and ignored,
    /// never overwritten silently: the error comes back so the caller can show it.
    pub fn open() -> (Self, Option<String>) {
        let path = ProjectDirs::from("", "", "equirect").map(|d| d.config_dir().join("library.json"));
        match path {
            Some(p) => Self::load_from(&p),
            None => (Self { path: None, favorites: Vec::new(), keys: HashSet::new() },
                     Some("no config folder: favorites cannot be saved".into())),
        }
    }

    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        let mut lib = Self { path: Some(path.to_path_buf()), favorites: Vec::new(), keys: HashSet::new() };
        let err = match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<LibraryFile>(&text) {
                Ok(f) => {
                    for fav in f.favorites {
                        if lib.keys.insert(key_of(&fav.source)) {
                            lib.favorites.push(fav);
                        }
                    }
                    None
                }
                Err(e) => {
                    // Keep the unreadable file as it is; saving would replace it, so say so.
                    lib.path = None;
                    Some(format!("library.json is damaged ({e}); favorites will not be saved until it is fixed"))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                lib.path = None;
                Some(format!("cannot read library.json: {e}"))
            }
        };
        (lib, err)
    }

    pub fn is_favorite(&self, source: &str) -> bool {
        self.keys.contains(&key_of(source))
    }

    /// Newest first.
    pub fn favorites(&self) -> impl Iterator<Item = &Favorite> {
        self.favorites.iter().rev()
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.favorites.len()
    }

    /// Flip the favorite flag and save. Returns the new state, or an error text when the file could
    /// not be written (the in-memory state still changes, so the heart answers the click).
    pub fn toggle(&mut self, source: &str) -> (bool, Option<String>) {
        let key = key_of(source);
        let now_favorite = if self.keys.remove(&key) {
            self.favorites.retain(|f| key_of(&f.source) != key);
            false
        } else {
            let added = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            self.keys.insert(key);
            self.favorites.push(Favorite { source: source.to_string(), added });
            true
        };
        (now_favorite, self.save().err())
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err("favorites cannot be saved (no usable library.json)".into());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&LibraryFile { favorites: self.favorites.clone() })
            .map_err(|e| e.to_string())?;
        // Write beside, then rename: a crash never leaves half a file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("equirect-library-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("library.json")
    }

    #[test]
    fn toggle_saves_and_reloads() {
        let p = temp_file("toggle");
        let (mut lib, err) = Library::load_from(&p);
        assert!(err.is_none());
        assert_eq!(lib.toggle(r"G:\vr\a.mp4"), (true, None));
        assert_eq!(lib.toggle("https://x.test/b.mp4"), (true, None));
        let (lib2, err2) = Library::load_from(&p);
        assert!(err2.is_none());
        assert_eq!(lib2.count(), 2);
        assert!(lib2.is_favorite(r"G:\vr\a.mp4"));
        // Newest first.
        assert_eq!(lib2.favorites().next().unwrap().source, "https://x.test/b.mp4");
        let mut lib2 = lib2;
        assert_eq!(lib2.toggle(r"G:\vr\a.mp4"), (false, None));
        assert!(!Library::load_from(&p).0.is_favorite(r"G:\vr\a.mp4"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn spellings_of_one_file_are_one_favorite() {
        let p = temp_file("spelling");
        let (mut lib, _) = Library::load_from(&p);
        lib.toggle(r"G:\VR\Clip.mp4");
        assert!(lib.is_favorite("g:/vr/clip.mp4"));
        assert_eq!(lib.toggle("g:/vr/clip.MP4").0, false);
        assert_eq!(lib.count(), 0);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn damaged_file_is_reported_and_not_overwritten() {
        let p = temp_file("damaged");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        let (mut lib, err) = Library::load_from(&p);
        assert!(err.unwrap().contains("damaged"));
        let (_, save_err) = lib.toggle(r"G:\a.mp4");
        assert!(save_err.is_some());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
