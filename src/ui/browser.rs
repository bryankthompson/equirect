use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::browser_prefs::BrowserPrefs;
use crate::thumbs::Thumbs;

pub const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mkv", "webm"];

// ── Location ──────────────────────────────────────────────────────────────────

/// A location in the browser: either a local filesystem path or a remote URL.
#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    Local(PathBuf),
    /// An `http://` or `https://` URL (file or directory).
    Remote(String),
}

impl Location {
    /// Human-readable string shown in the header bar.
    pub fn display(&self) -> String {
        match self {
            Location::Local(p)  => p.display().to_string(),
            Location::Remote(u) => u.clone(),
        }
    }

    /// Parent location, or `None` if already at root.
    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Local(p) => p.parent().map(|p| Location::Local(p.to_path_buf())),
            Location::Remote(u) => {
                let parent = crate::net::parent_url(u);
                if parent.trim_end_matches('/') != u.trim_end_matches('/') {
                    Some(Location::Remote(parent))
                } else {
                    None
                }
            }
        }
    }

    pub fn as_local(&self) -> Option<&PathBuf> {
        if let Location::Local(p) = self { Some(p) } else { None }
    }

    #[allow(dead_code)]
    pub fn as_remote(&self) -> Option<&str> {
        if let Location::Remote(u) = self { Some(u) } else { None }
    }
}

// ── BrowserEntry ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum BrowserEntry {
    Parent,
    Dir(String, Location),
    /// Name, where it is, and whether a `.funscript` sits beside it.
    Video(String, Location, bool),
}

// ── BrowserState ─────────────────────────────────────────────────────────────

pub struct BrowserState {
    pub location:               Location,
    pub entries:                Vec<BrowserEntry>,
    pub current_video:          Option<Location>,
    /// Set to true after creation or navigation; causes the list to scroll to
    /// the current video on the next draw.
    pub needs_scroll_to_current: bool,
    /// Set to true when entries are freshly loaded (not served from cache).
    /// The app reads this to update the folder cache, then clears it.
    pub just_loaded: bool,
    /// Toggled by the "Volumes" button — shows the drive picker inline.
    pub show_volumes: bool,
    /// The list shows the favorites (`favorite_entries`) instead of the folder.
    pub showing_favorites: bool,
    pub favorite_entries: Vec<BrowserEntry>,
    /// Hide videos that have no funscript beside them.
    pub scripts_only: bool,
    /// Pins, thumbnail size and panel size (the app saves them when `BrowserActions::prefs_changed` says so).
    pub prefs: BrowserPrefs,
    /// Makes the pictures the grid shows.
    thumbs: Arc<Thumbs>,
    /// A video's name cut to the two lines of a tile, by (name, tile width, font size): cutting means measuring text.
    labels: HashMap<(String, u32, u32), [String; 2]>,
    // Async loading state for remote locations.
    loading_rx: Option<std::sync::mpsc::Receiver<Result<Vec<BrowserEntry>, String>>>,
    pub is_loading:  bool,
    pub load_error:  Option<String>,
}

/// Actions the browser UI wants the app to perform.
#[derive(Default)]
pub struct BrowserActions {
    pub play:          Option<Location>,
    /// Navigate via ".." (go up) — destination may be served from cache.
    pub navigate:      Option<Location>,
    /// User explicitly clicked a folder in the list — invalidate its cache entry.
    pub select_dir:    Option<Location>,
    pub close:         bool,
    /// The root path of the volume the user selected in the volumes popup.
    /// The app resolves which directory to navigate to within that volume.
    pub select_volume: Option<PathBuf>,
    /// The "Favorites" button was clicked: the app fills `favorite_entries` and turns the list on.
    pub open_favorites: bool,
    /// A pin, the thumbnail size or the panel size changed: `BrowserState::prefs` has the new values.
    pub prefs_changed: bool,
}

impl BrowserState {
    /// `cached`: pre-loaded entries from the folder cache; if `Some`, skip the
    /// actual read.  Pass `None` to always read fresh.
    pub fn new(
        location: Location,
        current_video: Option<Location>,
        cached: Option<Vec<BrowserEntry>>,
        prefs: BrowserPrefs,
        thumbs: Arc<Thumbs>,
    ) -> Self {
        let mut s = Self {
            location,
            entries: Vec::new(),
            current_video,
            needs_scroll_to_current: true,
            just_loaded: false,
            show_volumes: false,
            showing_favorites: false,
            favorite_entries: Vec::new(),
            scripts_only: false,
            prefs,
            thumbs,
            labels: HashMap::new(),
            loading_rx: None,
            is_loading: false,
            load_error: None,
        };
        if let Some(entries) = cached {
            s.entries = entries;
        } else {
            s.start_load();
        }
        s
    }

    /// Navigate to `location`.  Pass cached entries to skip the real read, or
    /// `None` to always read fresh (e.g. after cache invalidation).
    pub fn navigate_to(&mut self, location: Location, cached: Option<Vec<BrowserEntry>>) {
        self.showing_favorites       = false;
        self.location                = location;
        self.entries                 = Vec::new();
        self.load_error              = None;
        self.needs_scroll_to_current = false;
        self.just_loaded             = false;
        if let Some(entries) = cached {
            self.entries = entries;
        } else {
            self.start_load();
        }
    }

    /// Call once per frame.  If an async remote load has completed, populate
    /// `entries`.
    pub fn poll_loading(&mut self) {
        let Some(rx) = &self.loading_rx else { return };
        if let Ok(result) = rx.try_recv() {
            self.loading_rx = None;
            self.is_loading = false;
            match result {
                Ok(entries) => {
                    self.entries     = entries;
                    self.load_error  = None;
                    self.just_loaded = true;
                }
                Err(e) => self.load_error = Some(e),
            }
        }
    }

    // ── private ───────────────────────────────────────────────────────────────

    fn start_load(&mut self) {
        match self.location.clone() {
            Location::Local(path) => {
                self.is_loading = false;
                self.loading_rx = None;
                self.refresh_local(&path);
                self.just_loaded = true;
            }
            Location::Remote(url) => {
                self.is_loading = true;
                let (tx, rx) = std::sync::mpsc::channel();
                self.loading_rx = Some(rx);
                std::thread::spawn(move || {
                    let result = crate::net::list_http_dir(&url).map(|items| {
                        let mut entries: Vec<BrowserEntry> = vec![BrowserEntry::Parent];
                        for item in items {
                            let loc = Location::Remote(item.url);
                            if item.is_dir {
                                entries.push(BrowserEntry::Dir(item.name, loc));
                            } else {
                                entries.push(BrowserEntry::Video(item.name, loc, false));
                            }
                        }
                        entries
                    }).map_err(|e| e.to_string());
                    let _ = tx.send(result);
                });
            }
        }
    }

    fn refresh_local(&mut self, path: &PathBuf) {
        self.entries = load_local_entries(path);
    }
}

/// Build a full directory listing for a local path in the same format the
/// browser uses (Parent first, then sorted dirs, then sorted videos).
/// Exposed so other code (e.g. prev/next navigation) can share it.
pub fn load_local_entries(path: &PathBuf) -> Vec<BrowserEntry> {
    let listing_started = std::time::Instant::now();
    let entries = load_local_entries_inner(path);
    crate::diag::slow(&format!("listing {} ({} entries)", path.display(), entries.len()), listing_started, 300);
    entries
}

fn load_local_entries_inner(path: &PathBuf) -> Vec<BrowserEntry> {
    let mut dirs:   Vec<(String, PathBuf)> = Vec::new();
    let mut videos: Vec<(String, PathBuf)> = Vec::new();
    // Lower-case names a video can be scripted under: `clip.funscript` and `clip.Bounce.funscript` both give `clip`.
    let mut scripted_stems: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let p    = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') { continue; }
            // The type comes with the directory listing; a stat per entry is a network round trip on a share.
            // Only a link needs a look at its target.
            let is_dir = match entry.file_type() {
                Ok(ft) if !ft.is_symlink() => ft.is_dir(),
                _ => p.is_dir(),
            };
            if is_dir {
                if crate::browser_prefs::is_excluded(&p) { continue; }
                dirs.push((name, p));
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if VIDEO_EXTS.iter().any(|&v| v.eq_ignore_ascii_case(ext)) {
                    videos.push((name, p));
                } else if ext.eq_ignore_ascii_case("funscript") {
                    let base = name[..name.len() - ".funscript".len()].to_lowercase();
                    if let Some((stem, _variant)) = base.rsplit_once('.') {
                        scripted_stems.insert(stem.to_string());
                    }
                    scripted_stems.insert(base);
                }
            }
        }
    }

    dirs.sort_by(  |(a, _), (b, _)| a.cmp(b));
    videos.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut entries = vec![BrowserEntry::Parent];
    for (name, p) in dirs   { entries.push(BrowserEntry::Dir  (name, Location::Local(p))); }
    for (name, p) in videos {
        let scripted = p.file_stem().map_or(false, |s| scripted_stems.contains(&s.to_string_lossy().to_lowercase()));
        entries.push(BrowserEntry::Video(name, Location::Local(p), scripted));
    }
    entries
}

/// Build entries from a remote HTTP listing in the same format the browser uses.
pub fn remote_entries(items: Vec<crate::net::RemoteItem>) -> Vec<BrowserEntry> {
    let mut entries = vec![BrowserEntry::Parent];
    for item in items {
        let loc = Location::Remote(item.url);
        if item.is_dir {
            entries.push(BrowserEntry::Dir(item.name, loc));
        } else {
            entries.push(BrowserEntry::Video(item.name, loc, false));
        }
    }
    entries
}


// ── draw ──────────────────────────────────────────────────────────────────────

use super::draw_list::{Color, Rect};
use super::icons;
use super::ui::{Align, FONT_DEFAULT, FONT_SMALL, ROW_HOVER, ROW_SELECTED, TEXT, TEXT_MUTED, Ui};
use crate::browser_prefs::{PanelSize, ViewSize};
use crate::thumbs::Thumb;

const ROW_H: f32 = 34.0;
/// Space between thumbnail tiles, across and down.
const GAP: f32 = 8.0;
/// The gap the layout puts between two rows by itself (`ITEM_SPACING_Y`), so a tile row can add the rest of `GAP`.
const LAYOUT_ROW_GAP: f32 = 2.0;
const YELLOW:    Color = Color { r: 1.0, g: 0.92, b: 0.30, a: 1.0 };
const RED:       Color = Color { r: 1.0, g: 0.30, b: 0.30, a: 1.0 };
const GREEN:     Color = Color { r: 0.40, g: 0.90, b: 0.50, a: 1.0 };
const LOAD_HINT: Color = Color { r: 0.59, g: 0.78, b: 1.0,  a: 1.0 };
const TILE_BG:   Color = Color { r: 0.17, g: 0.17, b: 0.17, a: 0.95 };
const PICTURE_BG: Color = Color { r: 0.06, g: 0.06, b: 0.06, a: 1.0 };
const WHITE:     Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
const FOLDER:    Color = Color { r: 0.85, g: 0.66, b: 0.27, a: 1.0 };

/// A folder sign (a tab on a body) centred in `area`, `size` pixels wide. The font has no folder character.
fn folder_shape(ui: &mut Ui, area: Rect, size: f32) {
    let body = Rect::new(area.x + (area.w - size) * 0.5, area.y + (area.h - size * 0.7) * 0.5 + size * 0.06, size, size * 0.7);
    ui.draw.rect(Rect::new(body.x, body.y - size * 0.13, size * 0.42, size * 0.16), FOLDER);
    ui.draw.rect(body, FOLDER);
}

/// What clicking an entry does (the same in the list and in the grid).
fn activate(entry: &BrowserEntry, parent: &Option<Location>, showing_favorites: bool, actions: &mut BrowserActions, leave_favorites: &mut bool) {
    match entry {
        BrowserEntry::Parent => {
            if showing_favorites { *leave_favorites = true; }
            else if let Some(loc) = parent { actions.navigate = Some(loc.clone()); }
        }
        BrowserEntry::Dir(_, loc)      => actions.select_dir = Some(loc.clone()),
        BrowserEntry::Video(_, loc, _) => actions.play = Some(loc.clone()),
    }
}

pub fn draw(ui: &mut Ui, state: &mut BrowserState) -> BrowserActions {
    state.poll_loading();

    let mut actions = BrowserActions::default();

    let scroll_to_current  = state.needs_scroll_to_current;
    state.needs_scroll_to_current = false;
    let current_video      = state.current_video.clone();
    let parent_loc         = state.location.parent();
    let showing_favorites  = state.showing_favorites;
    let scripts_only       = state.scripts_only && !showing_favorites;
    let dir_display        = if showing_favorites {
        format!("Favorites ({})", state.favorite_entries.len())
    } else {
        state.location.display()
    };
    let is_local           = state.location.as_local().is_some();

    // ── header: path label, volumes toggle, close ────────────────────────────
    // 44 = tallest child (close button: 28 icon + 12 pad).  horizontal_h
    // centers the (shorter) path label vertically against the buttons.
    ui.horizontal_h(44.0, |ui| {
        ui.label_styled(&dir_display, FONT_DEFAULT, TEXT_MUTED);
        ui.horizontal_rtl(|ui| {
            if ui.icon_button(icons::ICON_CLOSE, 28.0, false) {
                actions.close = true;
            }
            if is_local {
                if ui.button("Volumes") {
                    state.show_volumes = !state.show_volumes;
                    state.showing_favorites = false;
                }
            }
            if ui.button(if state.scripts_only { "All videos" } else { "Scripts only" }) {
                state.scripts_only = !state.scripts_only;
            }
            if ui.button("Favorites") {
                if showing_favorites {
                    state.showing_favorites = false;
                } else {
                    state.show_volumes = false;
                    actions.open_favorites = true;
                }
            }
        });
    });

    // ── pins: one click to a folder, and a toggle to pin or unpin the one shown ──
    let folder = state.location.display();
    let can_pin = is_local && !showing_favorites;
    ui.horizontal(|ui| {
        ui.label_styled("Pins", FONT_DEFAULT, TEXT_MUTED);
        if state.prefs.pins.is_empty() {
            ui.label_styled("none yet: browse to a folder and press Pin this folder", FONT_DEFAULT, TEXT_MUTED);
        }
        let mut go = None;
        for pin in &state.prefs.pins {
            if ui.toggle_button(&pin.name, is_local && BrowserPrefs::within(pin, &folder)) {
                go = Some(pin.path.clone());
            }
        }
        if let Some(path) = go {
            state.show_volumes = false;
            actions.select_dir = Some(Location::Local(PathBuf::from(path)));
        }
        ui.horizontal_rtl(|ui| {
            if can_pin {
                let pinned = state.prefs.is_pinned(&folder);
                if ui.button(if pinned { "Unpin this folder" } else { "Pin this folder" }) {
                    state.prefs.toggle_pin(&folder);
                    actions.prefs_changed = true;
                }
            }
        });
    });

    // ── how it looks: thumbnail size and panel size ──────────────────────────
    ui.horizontal(|ui| {
        ui.label_styled("View", FONT_DEFAULT, TEXT_MUTED);
        for v in ViewSize::ALL {
            if ui.toggle_button(v.label(), state.prefs.view == v) && state.prefs.view != v {
                state.prefs.view = v;
                actions.prefs_changed = true;
            }
        }
        ui.add_space(24.0);
        ui.label_styled("Panel", FONT_DEFAULT, TEXT_MUTED);
        for p in PanelSize::ALL {
            if ui.toggle_button(p.label(), state.prefs.panel == p) && state.prefs.panel != p {
                state.prefs.panel = p;
                actions.prefs_changed = true;
            }
        }
    });
    ui.separator();

    // ── volumes popup (inline — replaces the entry list while open) ──────────
    if state.show_volumes && is_local {
        let vols = crate::volumes::list_volumes();
        if vols.is_empty() {
            ui.label_styled("(no volumes found)", FONT_DEFAULT, TEXT_MUTED);
        } else {
            ui.scroll_area(0xC0_FFEEu64, ui.available_height(), |ui| {
                for vol in &vols {
                    let rect = ui.allocate_full_width(ROW_H);
                    let hit = ui.visible_part(rect);
                    let bg = if ui.hovered(hit) { ROW_HOVER } else { Color::TRANSPARENT };
                    if bg.a > 0.0 { ui.draw.rect(rect, bg); }
                    ui.draw_text_in(rect, &vol.label, FONT_DEFAULT, TEXT, Align::Left);
                    if ui.activated(hit) {
                        actions.select_volume = Some(vol.root.clone());
                        state.show_volumes    = false;
                    }
                }
            });
        }
        return actions;
    }

    // ── loading / error states ───────────────────────────────────────────────
    if state.is_loading {
        ui.label_styled("Loading…", FONT_DEFAULT, LOAD_HINT);
        return actions;
    }
    if let Some(err) = state.load_error.clone() {
        ui.label_styled(&format!("Error: {err}"), FONT_DEFAULT, RED);
        return actions;
    }

    // ── the entries: text rows or a grid of thumbnails ───────────────────────
    let viewport_h = ui.available_height();
    let view = state.prefs.view;
    let mut leave_favorites = false;
    {
        let BrowserState { entries, favorite_entries, thumbs, labels, .. } = &mut *state;
        let list: &Vec<BrowserEntry> = if showing_favorites { favorite_entries } else { entries };
        // what is on show: unscripted videos are left out in "Scripts only"
        let shown: Vec<&BrowserEntry> = list.iter()
            .filter(|e| !(scripts_only && matches!(e, BrowserEntry::Video(_, _, false))))
            .collect();
        let current_index = shown.iter().position(|e| {
            matches!(e, BrowserEntry::Video(_, loc, _) if current_video.as_ref() == Some(loc))
        });
        if showing_favorites && list.len() <= 1 {
            ui.label_styled("No favorites yet. Press the heart on the controls while a video plays.", FONT_DEFAULT, TEXT_MUTED);
        }

        if view == ViewSize::List {
            // If we just navigated, scroll the current video into view (pre-seed the scroll area's stored offset).
            if scroll_to_current {
                if let Some(idx) = current_index {
                    let off = (idx as f32 * (ROW_H + 2.0) - viewport_h / 2.0 + ROW_H / 2.0).max(0.0);
                    ui.set_scroll_offset(BROWSER_LIST_ID, off);
                }
            }
            ui.scroll_area(BROWSER_LIST_ID, viewport_h, |ui| {
                for entry in &shown {
                    let (display, is_selected, text_color) = match entry {
                        BrowserEntry::Parent => (
                            if showing_favorites { ".. (back to the folder)".to_string() } else { "..".to_string() },
                            false, TEXT,
                        ),
                        BrowserEntry::Dir(name, _) => (name.clone(), false, TEXT),
                        BrowserEntry::Video(name, loc, _) => {
                            let is_cur = current_video.as_ref() == Some(loc);
                            (name.clone(), is_cur, if is_cur { YELLOW } else { TEXT })
                        }
                    };
                    let rect = ui.allocate_full_width(ROW_H);
                    let hit  = ui.visible_part(rect);
                    let bg = if ui.hovered(hit) {
                        ROW_HOVER
                    } else if is_selected {
                        ROW_SELECTED
                    } else {
                        Color::TRANSPARENT
                    };
                    if bg.a > 0.0 { ui.draw.rect(rect, bg); }
                    if matches!(entry, BrowserEntry::Dir(..)) {
                        folder_shape(ui, Rect::new(rect.x + 4.0, rect.y, 26.0, rect.h), 22.0);
                        ui.draw_text_in(Rect::new(rect.x + 36.0, rect.y, rect.w - 36.0, rect.h), &display, FONT_DEFAULT, text_color, Align::Left);
                    } else {
                        ui.draw_text_in(rect, &display, FONT_DEFAULT, text_color, Align::Left);
                    }
                    let mut right_used = 0.0;
                    if matches!(entry, BrowserEntry::Video(_, _, true)) {
                        ui.draw_text_in(rect, "script", FONT_DEFAULT, GREEN, Align::Right);
                        right_used = ui.measure_text("script", FONT_DEFAULT) + 16.0;
                    }
                    if matches!(entry, BrowserEntry::Video(n, _, _) if looks_vr(n)) {
                        ui.draw_text_in(Rect::new(rect.x, rect.y, rect.w - right_used, rect.h), "VR", FONT_DEFAULT, VR_BLUE, Align::Right);
                    }
                    if ui.activated(hit) {
                        activate(entry, &parent_loc, showing_favorites, &mut actions, &mut leave_favorites);
                    }
                }
            });
        } else {
            let font = if view == ViewSize::Small { FONT_SMALL } else { FONT_DEFAULT };
            let line_h = ui.line_height(font);
            let avail_w = ui.available_width() - SCROLLBAR_ALLOWANCE;
            let (cols, tile_w) = grid_geometry(avail_w, view.tile_width());
            let picture_h = (tile_w * 9.0 / 16.0).round();
            let tile_h = picture_h + 2.0 * line_h + 12.0;
            if scroll_to_current {
                if let Some(idx) = current_index {
                    let off = ((idx / cols) as f32 * (tile_h + GAP) - viewport_h / 2.0 + tile_h / 2.0).max(0.0);
                    ui.set_scroll_offset(BROWSER_LIST_ID, off);
                }
            }
            if labels.len() > 4000 { labels.clear(); }
            ui.scroll_area(BROWSER_LIST_ID, viewport_h, |ui| {
                for (row_no, row_entries) in shown.chunks(cols).enumerate() {
                    let row = ui.allocate_full_width(tile_h);
                    ui.add_space(GAP - LAYOUT_ROW_GAP);
                    if ui.visible_part(row).h <= 0.0 { continue; } // scrolled out of sight: no pictures asked for either
                    for (i, entry) in row_entries.iter().enumerate() {
                        let rect = Rect::new(row.x + i as f32 * (tile_w + GAP), row.y, tile_w, tile_h);
                        let tile = Tile { entry, rect, picture_h, font, line_h, current: current_video.as_ref(), thumbs, labels };
                        if draw_tile(ui, tile) {
                            activate(entry, &parent_loc, showing_favorites, &mut actions, &mut leave_favorites);
                        }
                    }
                    let _ = row_no;
                }
            });
        }
    }
    if leave_favorites {
        state.showing_favorites = false;
    }

    actions
}

/// Room the scroll area keeps for its scrollbar (12 px bar and a 4 px gap).
const SCROLLBAR_ALLOWANCE: f32 = 16.0;

/// How many tiles of about `target` width fit in `avail`, and the width they get (stretched to fill the row).
fn grid_geometry(avail: f32, target: f32) -> (usize, f32) {
    let cols = (((avail + GAP) / (target + GAP)).floor() as usize).max(1);
    (cols, ((avail - GAP * (cols - 1) as f32) / cols as f32).floor().max(40.0))
}

struct Tile<'a> {
    entry:     &'a BrowserEntry,
    rect:      Rect,
    picture_h: f32,
    font:      f32,
    line_h:    f32,
    current:   Option<&'a Location>,
    thumbs:    &'a Thumbs,
    labels:    &'a mut HashMap<(String, u32, u32), [String; 2]>,
}

/// One tile: the picture (or a folder sign) over a two-line name. True when it was clicked.
fn draw_tile(ui: &mut Ui, t: Tile) -> bool {
    let Tile { entry, rect, picture_h, font, line_h, current, thumbs, labels } = t;
    let hit = ui.visible_part(rect);
    let hovered = ui.hovered(hit);
    let picture = Rect::new(rect.x, rect.y, rect.w, picture_h);
    ui.draw.rect(rect, if hovered { ROW_HOVER } else { TILE_BG });
    ui.draw.rect(picture, PICTURE_BG);

    let (label, color) = match entry {
        BrowserEntry::Parent => {
            ui.draw_text_in(picture, "..", 56.0, TEXT_MUTED, Align::Center);
            ("Up".to_string(), TEXT)
        }
        BrowserEntry::Dir(name, _) => {
            folder_shape(ui, picture, (picture.h * 0.55).min(110.0));
            (name.clone(), TEXT)
        }
        BrowserEntry::Video(name, loc, scripted) => {
            let found = match loc {
                Location::Local(p) => thumbs.get(&p.to_string_lossy()),
                Location::Remote(_) => Thumb::Failed,
            };
            match found {
                Thumb::Ready { uv, size } => ui.draw.thumb(picture, thumb_uv(uv, size, name, picture.w / picture.h), WHITE),
                Thumb::Loading => ui.draw_text_in(picture, "…", 32.0, TEXT_MUTED, Align::Center),
                Thumb::Failed => ui.draw_text_in(picture, "no preview", FONT_SMALL, TEXT_MUTED, Align::Center),
            }
            if *scripted {
                let w = ui.measure_text("script", FONT_SMALL) + 16.0;
                let pill = Rect::new(picture.right() - w - 6.0, picture.y + 6.0, w, ui.line_height(FONT_SMALL) + 4.0);
                ui.draw.rect(pill, Color::rgba(0.0, 0.0, 0.0, 0.7));
                ui.draw_text_in(pill, "script", FONT_SMALL, GREEN, Align::Center);
            }
            if looks_vr(name) {
                let w = ui.measure_text("VR", FONT_SMALL) + 16.0;
                let pill = Rect::new(picture.x + 6.0, picture.y + 6.0, w, ui.line_height(FONT_SMALL) + 4.0);
                ui.draw.rect(pill, Color::rgba(0.0, 0.0, 0.0, 0.7));
                ui.draw_text_in(pill, "VR", FONT_SMALL, VR_BLUE, Align::Center);
            }
            let is_cur = current == Some(loc);
            if is_cur { ui.draw.rect_outline(rect, YELLOW, 3.0); }
            (strip_extension(name).to_string(), if is_cur { YELLOW } else { TEXT })
        }
    };

    let max_w = rect.w - 16.0;
    let key = (label.clone(), max_w as u32, font as u32);
    let lines = labels.entry(key).or_insert_with(|| wrap2(|s| ui.measure_text(s, font), &label, max_w)).clone();
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() { continue; }
        let r = Rect::new(rect.x, picture.bottom() + 4.0 + i as f32 * line_h, rect.w, line_h);
        ui.draw_text_in(r, line, font, color, Align::Left);
    }
    ui.activated(hit)
}

/// "name.mp4" is shown as "name": the tile has little room and every entry in a video list ends the same way.
fn strip_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && (1..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) => stem,
        _ => name,
    }
}

/// A name on at most two lines no wider than `max_w`; if it is longer than that the second line ends in "…".
/// `measure` gives the width of a text.
fn wrap2(mut measure: impl FnMut(&str) -> f32, text: &str, max_w: f32) -> [String; 2] {
    let chars: Vec<char> = text.chars().collect();
    // the most characters of `chars` (at least one) that fit
    let mut fit = |chars: &[char], suffix: &str| -> usize {
        let (mut lo, mut hi) = (1usize, chars.len());
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let s: String = chars[..mid].iter().collect::<String>() + suffix;
            if measure(&s) <= max_w { lo = mid; } else { hi = mid - 1; }
        }
        lo
    };
    if chars.is_empty() { return [String::new(), String::new()]; }
    let n = fit(&chars, "");
    if n == chars.len() {
        return [text.to_string(), String::new()];
    }
    // break after a separator close to the end of the line, rather than in the middle of a word
    let mut cut = n;
    if let Some(i) = chars[..n].iter().rposition(|c| matches!(c, ' ' | '_' | '-' | '.')) {
        if i > 0 && i + 14 >= n { cut = i + 1; }
    }
    let first: String = chars[..cut].iter().collect::<String>().trim_end().to_string();
    let rest: Vec<char> = chars[cut..].iter().collect::<String>().trim_start().chars().collect();
    let rest_all: String = rest.iter().collect();
    if rest.is_empty() {
        return [first, String::new()];
    }
    let whole = fit(&rest, "");
    if whole == rest.len() {
        return [first, rest_all];
    }
    let m = fit(&rest, "…");
    let second = rest[..m].iter().collect::<String>().trim_end().to_string() + "…";
    [first, second]
}

/// Which part of a thumbnail to show, as fractions (x0, y0, x1, y1) of it: one eye of a stereo video, else all of it.
/// The name says how the eyes are laid out; failing that, a 2:1 picture is taken as two eyes side by side.
fn eye_part(name: &str, w: u32, h: u32) -> [f32; 4] {
    use crate::ui::settings::StereoLayout;
    let named = crate::video_meta::guess_layout(name, None).map(|s| s.stereo).filter(|s| *s != StereoLayout::OneView);
    let a = w as f32 / h.max(1) as f32;
    match named.unwrap_or(if (1.9..=2.1).contains(&a) { StereoLayout::LR } else { StereoLayout::OneView }) {
        StereoLayout::LR      => [0.0, 0.0, 0.5, 1.0],
        StereoLayout::RL      => [0.5, 0.0, 1.0, 1.0],
        StereoLayout::TB      => [0.0, 0.0, 1.0, 0.5],
        StereoLayout::BT      => [0.0, 0.5, 1.0, 1.0],
        StereoLayout::OneView => [0.0, 0.0, 1.0, 1.0],
    }
}

/// The texture rectangle to draw into a tile of `tile_aspect` (width / height): the video's one eye, cut to fill the tile
/// (the long side is cropped, nothing is stretched). `full` is the whole picture's rectangle in the texture.
fn thumb_uv(full: [f32; 4], size: (u32, u32), name: &str, tile_aspect: f32) -> [f32; 4] {
    let [fx0, fy0, fx1, fy1] = eye_part(name, size.0, size.1);
    let eye_aspect = (fx1 - fx0) * size.0 as f32 / ((fy1 - fy0) * size.1 as f32).max(1.0);
    let (mut x0, mut y0, mut x1, mut y1) = (fx0, fy0, fx1, fy1);
    if eye_aspect > tile_aspect {
        let half = (fx1 - fx0) * (tile_aspect / eye_aspect) / 2.0;
        let c = (fx0 + fx1) / 2.0;
        x0 = c - half;
        x1 = c + half;
    } else {
        let half = (fy1 - fy0) * (eye_aspect / tile_aspect) / 2.0;
        let c = (fy0 + fy1) / 2.0;
        y0 = c - half;
        y1 = c + half;
    }
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    [lerp(full[0], full[2], x0), lerp(full[1], full[3], y0), lerp(full[0], full[2], x1), lerp(full[1], full[3], y1)]
}

const BROWSER_LIST_ID: u64 = 0xB0_55_E5;

/// Colour of the "VR" marker next to the green "script" one.
const VR_BLUE: Color = Color { r: 0.35, g: 0.78, b: 1.0, a: 1.0 };

/// The file name says it is a VR video (180, 360, side-by-side, top-bottom...): the same guess the player makes
/// when it opens the file. A video whose settings were chosen by hand and has no such word is not marked.
fn looks_vr(name: &str) -> bool {
    crate::video_meta::detect_from_name(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_says_vr_gets_the_vr_marker() {
        for vr in ["Beach_180_LR.mp4", "tour 360 TB.mkv", "clip_SBS.mp4", "x_fisheye190_y.mp4", "Show 3DH.mp4"] {
            assert!(looks_vr(vr), "{vr}");
        }
        for flat in ["holiday.mp4", "Episode 1080p.mp4", "lrx.mp4", "notes.txt.mp4"] {
            assert!(!looks_vr(flat), "{flat}");
        }
    }

    #[test]
    fn videos_with_a_script_beside_them_are_marked() {
        let dir = std::env::temp_dir().join(format!("equirect-browser-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.mp4", "a.funscript", "B.MP4", "b.Bounce.funscript", "c.mp4", "d.funscript", "e.v2.mp4", "e.v2.funscript"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let scripted: Vec<(String, bool)> = load_local_entries(&dir).into_iter().filter_map(|e| match e {
            BrowserEntry::Video(n, _, s) => Some((n, s)),
            _ => None,
        }).collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(scripted, vec![
            ("B.MP4".to_string(), true),   // `b.Bounce.funscript` is a variant of `b`, case-insensitive
            ("a.mp4".to_string(), true),
            ("c.mp4".to_string(), false),
            ("e.v2.mp4".to_string(), true), // a dot in the video's own name still matches
        ]);
    }
}

#[cfg(test)]
mod browser_view_tests {
    use super::*;

    /// 10 px per character, so a width is easy to count.
    fn ten(s: &str) -> f32 { s.chars().count() as f32 * 10.0 }

    #[test]
    fn the_grid_fits_whole_tiles_and_stretches_them_to_the_row() {
        for target in [150.0, 230.0, 350.0] {
            for avail in [740.0, 1000.0, 1160.0, 1560.0] {
                let (cols, w) = grid_geometry(avail, target);
                let used = cols as f32 * w + (cols - 1) as f32 * GAP;
                assert!(cols >= 1 && used <= avail && avail - used < cols as f32 + 1.0, "{target} in {avail}: {cols} x {w}");
                assert!(w >= target - 1.0 || cols == 1, "tiles never come out much smaller than asked: {w} for {target}");
            }
        }
        assert_eq!(grid_geometry(100.0, 350.0).0, 1, "a narrow panel still gets one column");
    }

    #[test]
    fn names_wrap_onto_two_lines_and_a_long_one_ends_in_an_ellipsis() {
        assert_eq!(wrap2(ten, "short", 100.0), ["short".to_string(), String::new()]);
        let two = wrap2(ten, "Some Title With Spaces In It", 150.0);
        assert!(ten(&two[0]) <= 150.0 && ten(&two[1]) <= 150.0, "{two:?}");
        assert!(!two[1].ends_with('…') && two[0].ends_with("With") || two[0].ends_with("Title"), "breaks at a word: {two:?}");
        let long = wrap2(ten, &"word ".repeat(40), 150.0);
        assert!(long[1].ends_with('…') && ten(&long[1]) <= 150.0, "{long:?}");
        let one_word = wrap2(ten, &"x".repeat(100), 150.0);
        assert!(ten(&one_word[0]) <= 150.0 && one_word[1].ends_with('…'), "{one_word:?}");
        assert_eq!(wrap2(ten, "", 100.0), [String::new(), String::new()]);
    }

    #[test]
    fn extensions_are_dropped_but_not_names() {
        assert_eq!(strip_extension("movie_180_LR.mp4"), "movie_180_LR");
        assert_eq!(strip_extension("a.b.c.mkv"), "a.b.c");
        assert_eq!(strip_extension("no extension"), "no extension");
        assert_eq!(strip_extension(".hidden"), ".hidden");
        assert_eq!(strip_extension("Mr. Smith goes"), "Mr. Smith goes", "a long or spaced tail is not an extension");
    }

    #[test]
    fn a_stereo_picture_shows_one_eye_filling_the_tile() {
        let full = [0.0, 0.0, 1.0, 1.0];
        // a 2:1 picture named side-by-side: the left half, 1:1, cropped to a 16:9 tile (height trimmed)
        let uv = thumb_uv(full, (384, 192), "scene_180_LR.mp4", 16.0 / 9.0);
        assert!((uv[0] - 0.0).abs() < 1e-5 && (uv[2] - 0.5).abs() < 1e-5, "left eye only: {uv:?}");
        let shown_aspect = (uv[2] - uv[0]) * 384.0 / ((uv[3] - uv[1]) * 192.0);
        assert!((shown_aspect - 16.0 / 9.0).abs() < 0.02, "fills the tile without stretching: {shown_aspect}");
        // right-left layout takes the right half
        let rl = thumb_uv(full, (384, 192), "x_RL.mp4", 16.0 / 9.0);
        assert!(rl[0] >= 0.5 - 1e-5 && (rl[2] - 1.0).abs() < 1e-5, "{rl:?}");
        // top/bottom takes the top half
        let tb = thumb_uv(full, (384, 216), "x_TB.mp4", 16.0 / 9.0);
        assert!(tb[3] <= 0.5 + 1e-5, "{tb:?}");
        // an ordinary 16:9 picture is shown whole
        let flat = thumb_uv(full, (384, 216), "holiday.mp4", 16.0 / 9.0);
        assert!(flat.iter().zip([0.0, 0.0, 1.0, 1.0]).all(|(a, b)| (a - b).abs() < 1e-4), "{flat:?}");
        // the slot's own rectangle is honoured (not the whole texture)
        let slot = [0.2, 0.4, 0.3, 0.5];
        let uv = thumb_uv(slot, (384, 216), "holiday.mp4", 16.0 / 9.0);
        assert!(uv[0] >= 0.2 - 1e-6 && uv[2] <= 0.3 + 1e-6 && uv[1] >= 0.4 - 1e-6 && uv[3] <= 0.5 + 1e-6, "{uv:?}");
    }
}
