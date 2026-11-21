use super::icons::{self, IconSprite};
use crate::haptics::{self, AutoMode, Haptics};
use crate::screen::{self, Screen};
use crate::volume::Volume;
use super::ui::{FONT_DEFAULT, FONT_SMALL, TEXT, TEXT_DISABLED, TEXT_MUTED, Ui};

/// Video geometry mode.
#[derive(Clone, Copy, PartialEq, Default, Debug, serde::Serialize, serde::Deserialize)]
pub enum VideoMode {
    #[default]
    Flat2D,
    Curved2D,
    Sbs3D,
    View180,
    View360,
}

/// Projection type (only meaningful for 180 / 360).
#[derive(Clone, Copy, PartialEq, Default, Debug, serde::Serialize, serde::Deserialize)]
pub enum Projection {
    #[default]
    Equirect,
    Fisheye,
}

/// Stereo layout (meaningful for 3D / 180 / 360).
#[derive(Clone, Copy, PartialEq, Default, Debug, serde::Serialize, serde::Deserialize)]
pub enum StereoLayout {
    #[default]
    OneView,
    LR,
    RL,
    TB,
    BT,
}

/// Persistent settings that affect how the video is rendered.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoSettings {
    pub mode:   VideoMode,
    pub proj:   Projection,
    pub stereo: StereoLayout,
    pub zoom:   f32,
    /// Handy offset for this video in ms (positive: the Handy moves earlier). Per video, because each script has its own timing.
    #[serde(default)]
    pub haptics_offset_ms: i32,
    /// Where the screen hangs: one value for every video (`screen.json`), so not saved with the video. The app copies it in
    /// before each frame, for the renderer.
    #[serde(skip)]
    pub screen: Screen,
}

impl VideoSettings {
    pub fn new() -> Self {
        Self { zoom: 1.0, ..Default::default() }
    }
}

/// Which page of the settings panel is showing.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum SettingsTab {
    #[default]
    Video,
    Screen,
    Haptics,
    Auto,
    Audio,
}

/// Everything the settings panel shows and edits.
pub struct SettingsView<'a> {
    pub video:   &'a mut VideoSettings,
    pub haptics: &'a mut Haptics,
    pub screen:  &'a mut Screen,
    pub volume:  &'a mut Volume,
    pub tab:     &'a mut SettingsTab,
}

/// Actions the settings UI wants the app to perform.
#[derive(Default)]
pub struct SettingsActions {
    pub close: bool,
    /// Video settings (including this video's Handy offset) changed: save them for the video.
    pub changed: bool,
    /// The stroke range or invert changed: save them for all videos.
    pub haptics_changed: bool,
    /// Where the screen hangs changed: save it for all videos.
    pub screen_changed: bool,
    /// "Reset" button: back to the settings detected from the file name (or flat).
    pub reset: bool,
    /// The volume or mute changed: save it for all videos.
    pub volume_changed: bool,
    /// "Center" button: put the screen and panels in front of the viewer, where they look now.
    pub center: bool,
}

const ICON_SIZE: f32 = 96.0;

pub fn draw(ui: &mut Ui, view: &mut SettingsView) -> SettingsActions {
    let mut actions = SettingsActions::default();

    // ── header (title left, close right) ─────────────────────────────────────
    ui.horizontal_h(44.0, |ui| {
        ui.label_styled("Settings", FONT_DEFAULT + 2.0, TEXT);
        ui.horizontal_rtl(|ui| {
            if ui.icon_button(icons::ICON_CLOSE, 28.0, false) {
                actions.close = true;
            }
            if *view.tab == SettingsTab::Video && ui.button("Reset") {
                actions.reset = true;
            }
        });
    });
    ui.horizontal(|ui| {
        if ui.toggle_button("Video", *view.tab == SettingsTab::Video) {
            *view.tab = SettingsTab::Video;
        }
        if ui.toggle_button("Screen", *view.tab == SettingsTab::Screen) {
            *view.tab = SettingsTab::Screen;
        }
        if ui.toggle_button("Haptics", *view.tab == SettingsTab::Haptics) {
            *view.tab = SettingsTab::Haptics;
        }
        if ui.toggle_button("Auto", *view.tab == SettingsTab::Auto) {
            *view.tab = SettingsTab::Auto;
        }
        if ui.toggle_button("Audio", *view.tab == SettingsTab::Audio) {
            *view.tab = SettingsTab::Audio;
        }
    });
    ui.separator();

    match *view.tab {
        SettingsTab::Video   => video_body(ui, view.video, &mut actions),
        SettingsTab::Screen  => screen_body(ui, view.video.mode, view.screen, &mut actions),
        SettingsTab::Haptics => haptics_body(ui, view.video, view.haptics, &mut actions),
        SettingsTab::Auto    => auto_body(ui, view.haptics, &mut actions),
        SettingsTab::Audio   => audio_body(ui, view.volume, &mut actions),
    }
    actions
}

/// The width of the label column of a slider row: wide enough for "Pattern time", so every slider starts at the same x.
const ROW_LABEL_W: f32 = 130.0;

/// Draw a row label and pad it to `label_w`, so the sliders of several rows line up.
fn row_label(ui: &mut Ui, label: &str, label_w: f32) {
    ui.label_styled(label, FONT_DEFAULT, TEXT);
    let used = ui.measure_text(label, FONT_DEFAULT);
    if used < label_w { ui.add_space(label_w - used); }
}

/// A row of "label, slider, value" with the value text on the right. Returns true when the slider moved `value`.
fn slider_row(ui: &mut Ui, label: &str, value: &mut f32, min: f32, max: f32, text: &str) -> bool {
    let mut moved = false;
    // The row is as tall as the slider (48 px, all of it grabbable), which centres the label and the value on the track.
    ui.horizontal_h(48.0, |ui| {
        row_label(ui, label, ROW_LABEL_W);
        let text_w   = ui.measure_text("+1000 ms", FONT_DEFAULT) + 12.0;
        let slider_w = (ui.available_width() - text_w - 20.0).max(50.0);
        moved = ui.slider_sized(slider_w, value, min, max);
        ui.add_space(10.0);
        ui.label_styled(text, FONT_DEFAULT, TEXT);
    });
    moved
}

/// "label, two-thumb slider, value": `lo` and `hi` on a 0..1 track. True when a thumb moved.
fn range_row(ui: &mut Ui, label: &str, lo: &mut f32, hi: &mut f32, gap: f32, text: &str) -> bool {
    let mut moved = false;
    ui.horizontal(|ui| {
        row_label(ui, label, ROW_LABEL_W);
        let text_w   = ui.measure_text("120.0 - 120.0 s", FONT_DEFAULT) + 12.0;
        let slider_w = (ui.available_width() - text_w - 20.0).max(50.0);
        moved = ui.range_slider_sized(slider_w, lo, hi, 0.0, 1.0, gap);
        ui.add_space(10.0);
        ui.label_styled(text, FONT_DEFAULT, TEXT);
    });
    ui.add_space(14.0);
    moved
}

/// A percent range (0..100, kept at least `MIN_GAP` apart). True when changed.
fn percent_range_row(ui: &mut Ui, label: &str, pair: &mut [u8; 2]) -> bool {
    let (mut lo, mut hi) = (pair[0] as f32 / 100.0, pair[1] as f32 / 100.0);
    let text = format!("{} - {}%", pair[0], pair[1]);
    if range_row(ui, label, &mut lo, &mut hi, haptics::MIN_GAP as f32 / 100.0, &text) {
        *pair = [(lo * 100.0).round() as u8, (hi * 100.0).round() as u8];
        return true;
    }
    false
}

/// A seconds range, 0..60 s. The track is squared so the short times (a tenth of a second up to a few) get room too.
fn seconds_range_row(ui: &mut Ui, label: &str, pair: &mut [f32; 2], least: f32) -> bool {
    let (mut lo, mut hi) = ((pair[0] / 60.0).sqrt(), (pair[1] / 60.0).sqrt());
    let text = if pair[1] <= 0.0 { "none".to_string() } else { format!("{:.1} - {:.1} s", pair[0], pair[1]) };
    if range_row(ui, label, &mut lo, &mut hi, 0.0, &text) {
        let tenth = |t: f32| ((t * t * 60.0) * 10.0).round() / 10.0;
        let (a, b) = (tenth(lo).max(least), tenth(hi).max(least));
        *pair = [a, b.max(a)];
        return true;
    }
    false
}

fn screen_body(ui: &mut Ui, mode: VideoMode, s: &mut Screen, actions: &mut SettingsActions) {
    if matches!(mode, VideoMode::View180 | VideoMode::View360) {
        section_label(ui, "180 and 360 video surrounds you, so these only move the Flat, Curved and 3D screens", true);
    } else {
        section_label(ui, "Where the screen hangs (all videos). Zoom, on the Video page, sets its size", true);
    }
    let mut v = s.curve as f32;
    if slider_row(ui, "Curvature", &mut v, 0.0, 100.0, &format!("{}%", s.curve)) {
        s.set_curve(v);
        actions.screen_changed = true;
    }
    let mut v = s.distance;
    if slider_row(ui, "Distance", &mut v, screen::DISTANCE_RANGE.0, screen::DISTANCE_RANGE.1, &format!("{:.1} m", s.distance)) {
        s.set_distance(v);
        actions.screen_changed = true;
    }
    let mut v = s.height;
    if slider_row(ui, "Height", &mut v, screen::HEIGHT_RANGE.0, screen::HEIGHT_RANGE.1, &format!("{:.1} m", s.height)) {
        s.set_height(v);
        actions.screen_changed = true;
    }
    let mut v = s.tilt;
    if slider_row(ui, "Tilt", &mut v, screen::TILT_RANGE.0, screen::TILT_RANGE.1, &format!("{:+.0}\u{b0}", s.tilt)) {
        s.set_tilt(v);
        actions.screen_changed = true;
    }
    ui.horizontal(|ui| {
        if ui.button("Center") {
            actions.center = true;
        }
        if ui.button("Reset screen") && *s != Screen::default() {
            *s = Screen::default();
            actions.screen_changed = true;
        }
    });
    ui.add_space(6.0);
    ui.label_styled("Center puts the screen and panels in front of you, where you look now (a grip squeeze does it too, and so does SteamVR's centre).", FONT_SMALL, TEXT_MUTED);
    ui.label_styled("Curvature works in Curved mode (Video page): 100% wraps the screen around you.", FONT_SMALL, TEXT_MUTED);
    ui.label_styled("Tilt turns the screen up (+) or down (-) around your head: + to watch lying on your back.", FONT_SMALL, TEXT_MUTED);
}

fn audio_body(ui: &mut Ui, v: &mut Volume, actions: &mut SettingsActions) {
    section_label(ui, "Sound level (all videos). The speaker on the control bar mutes and unmutes", true);
    let mut p = v.percent as f32;
    if slider_row(ui, "Volume", &mut p, 0.0, 100.0, &format!("{}%", v.percent)) {
        v.set_percent(p);
        actions.volume_changed = true;
    }
    ui.horizontal(|ui| {
        if ui.toggle_button("Muted", v.muted) {
            v.muted = !v.muted;
            actions.volume_changed = true;
        }
    });
    ui.add_space(6.0);
    ui.label_styled("The sound plays on the headset's own speakers (EQUIRECT_AUDIO_DEVICE picks another device).", FONT_SMALL, TEXT_MUTED);
}

fn auto_body(ui: &mut Ui, h: &mut Haptics, actions: &mut SettingsActions) {
    section_label(ui, "Auto mode: the bridge makes the strokes itself, from patterns found in real funscripts", true);
    ui.horizontal(|ui| {
        for (mode, name) in [(AutoMode::Off, "Off"), (AutoMode::NoScript, "No funscript only"), (AutoMode::Always, "Always")] {
            if ui.toggle_button(name, h.auto.mode == mode) && h.auto.mode != mode {
                h.auto.mode = mode;
                actions.haptics_changed = true;
            }
        }
    });
    ui.add_space(4.0);
    section_label(ui, "Style (choosing one sets the sliders below)", false);
    for row in haptics::presets().chunks(4) { // four to a row: seven styles stay on two rows and the page fits
        ui.horizontal(|ui| {
            for p in row {
                if ui.toggle_button(&p.name, h.auto.preset == p.id) && h.auto.preset != p.id {
                    h.auto.apply_preset(p);
                    actions.haptics_changed = true;
                }
            }
        });
    }
    ui.add_space(6.0);
    if percent_range_row(ui, "Speed", &mut h.auto.speed) { actions.haptics_changed = true; }
    if percent_range_row(ui, "Stroke", &mut h.auto.stroke) { actions.haptics_changed = true; }
    let mut min_stroke = h.auto.min_stroke as f32;
    if slider_row(ui, "Shortest", &mut min_stroke, 0.0, 100.0, &format!("{}%", h.auto.min_stroke)) {
        h.auto.min_stroke = min_stroke.round() as u8;
        actions.haptics_changed = true;
    }
    if seconds_range_row(ui, "Pattern time", &mut h.auto.pattern_s, 0.1) { actions.haptics_changed = true; }
    if seconds_range_row(ui, "Pause", &mut h.auto.pause_s, 0.0) { actions.haptics_changed = true; }
    ui.horizontal(|ui| {
        if ui.button("New seed") {
            h.auto.seed = haptics::new_seed();
            actions.haptics_changed = true;
        }
        if h.auto.seed != 0 && ui.button("Each video") {
            h.auto.seed = 0;
            actions.haptics_changed = true;
        }
        let text = if h.auto.seed == 0 { "Seed: a new one for each video".to_string() } else { format!("Seed: {}", h.auto.seed) };
        ui.label_styled(&text, FONT_DEFAULT, TEXT);
    });
    ui.label_styled("Speed: 100% is the Handy's top speed. No pauses: drag the right end of Pause all the way left.", FONT_SMALL, TEXT_MUTED);
    ui.label_styled("Random changes style every 1 to 4 minutes, inside the limits of the sliders. The same seed plays the same script.", FONT_SMALL, TEXT_MUTED);
}

fn haptics_body(ui: &mut Ui, video: &mut VideoSettings, h: &mut Haptics, actions: &mut SettingsActions) {
    section_label(ui, "Offset: + makes the Handy move earlier, - later (saved for this video)", true);
    ui.horizontal(|ui| {
        if ui.button("-10") {
            video.haptics_offset_ms = haptics::snap_offset(video.haptics_offset_ms as f32 - 10.0);
            actions.changed = true;
        }
        if ui.button("+10") {
            video.haptics_offset_ms = haptics::snap_offset(video.haptics_offset_ms as f32 + 10.0);
            actions.changed = true;
        }
        if ui.button("0") && video.haptics_offset_ms != 0 {
            video.haptics_offset_ms = 0;
            actions.changed = true;
        }
    });
    let mut offset = video.haptics_offset_ms as f32;
    let text = format!("{:+} ms", video.haptics_offset_ms);
    let range = haptics::OFFSET_RANGE_MS as f32;
    if slider_row(ui, "Offset", &mut offset, -range, range, &text) {
        video.haptics_offset_ms = haptics::snap_offset(offset);
        actions.changed = true;
    }
    ui.add_space(8.0);

    section_label(ui, "Stroke range: the part of the Handy's travel to use (all videos)", true);
    let mut lo = h.stroke_min as f32;
    if slider_row(ui, "Min", &mut lo, 0.0, 100.0, &format!("{}%", h.stroke_min)) {
        h.set_stroke_min(lo.round() as u8);
        actions.haptics_changed = true;
    }
    let mut hi = h.stroke_max as f32;
    if slider_row(ui, "Max", &mut hi, 0.0, 100.0, &format!("{}%", h.stroke_max)) {
        h.set_stroke_max(hi.round() as u8);
        actions.haptics_changed = true;
    }
    ui.add_space(8.0);

    section_label(ui, "Invert: flip every stroke upside down (all videos)", true);
    ui.horizontal(|ui| {
        if ui.toggle_button(if h.invert { "Invert: on" } else { "Invert: off" }, h.invert) {
            h.invert = !h.invert;
            actions.haptics_changed = true;
        }
        ui.horizontal_rtl(|ui| {
            if ui.button("Reset haptics") {
                let auto = h.auto.clone(); // the auto-mode settings have their own page
                *h = Haptics::default();
                h.auto = auto;
                video.haptics_offset_ms = 0;
                actions.changed = true;
                actions.haptics_changed = true;
            }
        });
    });
    ui.add_space(8.0);
    ui.label_styled("Sent to the Handy bridge, which has to be running (the launcher starts it).", FONT_SMALL, TEXT_MUTED);
}

fn video_body(ui: &mut Ui, state: &mut VideoSettings, actions: &mut SettingsActions) {
    let proj_enabled   = matches!(state.mode, VideoMode::View180 | VideoMode::View360);
    let stereo_enabled = matches!(state.mode, VideoMode::Sbs3D | VideoMode::View180 | VideoMode::View360);

    // ── 1. Video mode ─────────────────────────────────────────────────────────
    section_label(ui, "Video Mode", true);
    ui.horizontal(|ui| {
        let icon_modes: &[(IconSprite, VideoMode)] = &[
            (icons::ICON_PLANE,      VideoMode::Flat2D),
            (icons::ICON_CURVE,      VideoMode::Curved2D),
            (icons::ICON_3D,         VideoMode::Sbs3D),
            (icons::ICON_HEMISPHERE, VideoMode::View180),
            (icons::ICON_SPHERE,     VideoMode::View360),
        ];
        for &(icon, mode) in icon_modes {
            let selected = state.mode == mode;
            if ui.icon_button(icon, ICON_SIZE, selected) && !selected {
                state.mode = mode;
                if !matches!(state.mode, VideoMode::View180 | VideoMode::View360) {
                    state.proj = Projection::default();
                }
                if !matches!(state.mode, VideoMode::Sbs3D | VideoMode::View180 | VideoMode::View360) {
                    state.stereo = StereoLayout::default();
                }
                actions.changed = true;
            }
        }
    });
    ui.add_space(8.0);

    // ── 2. Projection ─────────────────────────────────────────────────────────
    section_label(ui, "Projection", proj_enabled);
    ui.disabled(!proj_enabled, |ui| {
        ui.horizontal(|ui| {
            let projs: &[(IconSprite, Projection)] = &[
                (icons::ICON_EQUIRECT, Projection::Equirect),
                (icons::ICON_FISHEYE,  Projection::Fisheye),
            ];
            for &(icon, proj) in projs {
                let selected = state.proj == proj;
                if ui.icon_button(icon, ICON_SIZE, selected) && !selected {
                    state.proj = proj;
                    actions.changed = true;
                }
            }
        });
    });
    ui.add_space(8.0);

    // ── 3. Stereo layout ──────────────────────────────────────────────────────
    section_label(ui, "Stereo Layout", stereo_enabled);
    ui.disabled(!stereo_enabled, |ui| {
        ui.horizontal(|ui| {
            // "One View" only makes sense for 180/360 (not plain 3D).
            let one_view_ok = matches!(state.mode, VideoMode::View180 | VideoMode::View360);
            let layouts: &[(IconSprite, StereoLayout, bool)] = &[
                (icons::ICON_ONEVIEW, StereoLayout::OneView, one_view_ok),
                (icons::ICON_LR,      StereoLayout::LR,      true),
                (icons::ICON_RL,      StereoLayout::RL,      true),
                (icons::ICON_TB,      StereoLayout::TB,      true),
                (icons::ICON_BT,      StereoLayout::BT,      true),
            ];
            for &(icon, layout, avail) in layouts {
                let selected = state.stereo == layout;
                let pressed = ui.disabled(!avail, |ui| {
                    ui.icon_button(icon, ICON_SIZE, selected)
                });
                if pressed && !selected && avail {
                    state.stereo = layout;
                    actions.changed = true;
                }
            }
        });
    });
    ui.add_space(8.0);

    // ── 4. Zoom ───────────────────────────────────────────────────────────────
    section_label(ui, "Zoom", true);
    ui.horizontal_h(48.0, |ui| {
        if ui.button("1×") && (state.zoom - 1.0).abs() > 0.001 {
            state.zoom = 1.0;
            actions.changed = true;
        }
        let zoom_label = format!("{:.2}×", state.zoom);
        let label_w    = ui.measure_text(&zoom_label, FONT_DEFAULT) + 12.0;
        let slider_w   = (ui.available_width() - label_w - 8.0).max(50.0);
        if ui.slider_sized(slider_w, &mut state.zoom, 0.25, 4.0) {
            actions.changed = true;
        }
        ui.label_styled(&zoom_label, FONT_DEFAULT, TEXT);
    });
}

fn section_label(ui: &mut Ui, text: &str, enabled: bool) {
    let color = if enabled { TEXT_MUTED } else { TEXT_DISABLED };
    ui.label_styled(text, FONT_SMALL, color);
}
