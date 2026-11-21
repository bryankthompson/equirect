use super::draw_list::Color;
use super::icons;
use super::atlas::GlyphAtlas;
use super::ui::{
    FONT_DEFAULT, FONT_SMALL, FONT_TINY, ITEM_SPACING_X, ITEM_SPACING_Y, TEXT, TEXT_MUTED, Ui,
};

/// What the video's `.nfo` says, already cut into lines that fit the panel (see `crate::nfo`).
/// Shown above the buttons; the panel is made taller by `info_extra_height` to hold it.
#[derive(Clone, Default, PartialEq)]
pub struct VideoInfo {
    /// Title (when it says more than the file name) and studio, one line; may be empty.
    pub heading: String,
    /// The plot, wrapped.
    pub plot: Vec<String>,
}

/// Widest line of notes, in pixels: the content width of the bar, less a little slack.
const INFO_WIDTH: f32 = CONTROL_BAR_CONTENT_W - 16.0;
/// Most plot lines shown; a longer plot ends in "…".
const INFO_PLOT_LINES: usize = 4;

/// The notes to show for a video: its title (only when it differs from `file_name`) and studio on one line,
/// then the plot. None when the NFO has nothing to add.
pub fn video_info(nfo: &crate::nfo::Nfo, file_name: &str, atlas: &mut GlyphAtlas) -> Option<VideoInfo> {
    let mut parts: Vec<&str> = Vec::new();
    if !nfo.title.is_empty() && !nfo.title.eq_ignore_ascii_case(file_name) {
        parts.push(&nfo.title);
    }
    if !nfo.studio.is_empty() {
        parts.push(&nfo.studio);
    }
    let heading = crate::nfo::wrap(&parts.join("  \u{b7}  "), INFO_WIDTH, 1, |t| atlas.measure(t, FONT_DEFAULT))
        .pop()
        .unwrap_or_default();
    let plot = crate::nfo::wrap(&nfo.plot, INFO_WIDTH, INFO_PLOT_LINES, |t| atlas.measure(t, FONT_SMALL));
    if heading.is_empty() && plot.is_empty() { None } else { Some(VideoInfo { heading, plot }) }
}

/// Extra panel height in pixels that `info` needs on top of the plain control bar. Matches what
/// `draw` allocates: each label takes its line height (rounded up) plus the item spacing.
pub fn info_extra_height(info: Option<&VideoInfo>, atlas: &GlyphAtlas) -> f32 {
    let Some(info) = info else { return 0.0 };
    let mut h = 0.0;
    if !info.heading.is_empty() {
        h += atlas.line_height(FONT_DEFAULT).ceil() + ITEM_SPACING_Y;
    }
    h += info.plot.len() as f32 * (atlas.line_height(FONT_SMALL).ceil() + ITEM_SPACING_Y);
    h
}

/// Persistent state for the control bar panel.
pub struct ControlBarState {
    pub video_name: String,
    pub error: Option<String>,
    pub is_playing: bool,
    /// Current playback position in seconds.
    pub current_secs: f64,
    /// Total duration in seconds; 0 if not yet known.
    pub duration_secs: f64,
    /// Index into SPEEDS.
    pub speed_index: usize,
    /// Loop state: 0 = off, 1 = start set, 2 = active (start + end set).
    pub loop_state: u8,
    /// The current video is in the user's favorites (the heart is red).
    pub is_favorite: bool,
    /// The sound is muted (the speaker is crossed out).
    pub muted: bool,
    /// The volume slider takes the seek bar's place for a few seconds (the speaker was clicked).
    pub volume_open: bool,
    /// The sound level, percent, shown on that slider.
    pub volume_percent: u8,
    /// The funscript's heat map for the seek bar (0 cold .. 255 hot, left to right); empty when there is no script.
    pub heat: Vec<u8>,
    /// Title, studio and plot from the video's `.nfo`, if it has something to show.
    pub info: Option<VideoInfo>,
    /// A short note shown in place of the video's name for a few seconds ("Resumed at 12:34").
    pub notice: Option<String>,
}

pub const SPEEDS: [f32; 5] = [1.0, 2.0 / 3.0, 0.5, 1.0 / 3.0, 0.25];

impl Default for ControlBarState {
    fn default() -> Self {
        Self {
            video_name: String::new(),
            error: None,
            is_playing: true,
            current_secs: 0.0,
            duration_secs: 0.0,
            speed_index: 0,
            loop_state: 0,
            is_favorite: false,
            muted: false,
            volume_open: false,
            volume_percent: 100,
            heat: Vec::new(),
            info: None,
            notice: None,
        }
    }
}

/// Actions that the control bar UI wants the app to perform.
#[derive(Default)]
pub struct ControlBarActions {
    pub play_pause: bool,
    pub prev: bool,
    pub next: bool,
    pub cycle_speed: bool,
    pub cycle_loop: bool,
    pub show_settings: bool,
    pub show_browser: bool,
    /// The heart was clicked: add or remove the current video from the favorites.
    pub toggle_favorite: bool,
    /// The speaker was clicked: the app opens the volume slider, and a double click mutes or unmutes.
    pub speaker_clicked: bool,
    /// The volume slider moved: the new level, percent.
    pub set_volume: Option<u8>,
    pub exit: bool,
    /// Seek target in [0, 1] (fraction of duration).
    pub seek_frac: Option<f32>,
    /// Thumbstick-driven seek: signed seconds to add to the current position
    /// (wraps at duration boundaries). Only emitted at most every 100 ms.
    pub seek_delta_secs: Option<f64>,
    /// B (right) / Y (left) button just pressed — toggle panel visibility.
    pub menu_toggle: bool,
    /// Both grips squeezed together: put the video settings back to the detected ones
    /// and bring the control bar back (escape hatch when a settings click leaves a mess).
    pub reset_settings: bool,
}

pub const ICON_SIZE:    f32 = 56.0;
/// Total width of one icon button: icon size + horizontal padding from
/// `ui.rs::icon_button` (8 px each side).
pub const ICON_BTN_W:   f32 = ICON_SIZE + 16.0;
/// Number of icon buttons in the row (matches the calls in `draw`).
pub const ICON_COUNT:   usize = 10;
const RED:              Color = Color { r: 1.0, g: 0.30, b: 0.30, a: 1.0 };
const NOTICE:           Color = Color { r: 1.0, g: 0.92, b: 0.30, a: 1.0 };

/// Exact panel-content dimensions implied by the icon row + label/slider rows.
/// Exposed so `app.rs` can size the panel texture and world quad to match.
pub const CONTROL_BAR_CONTENT_W: f32 =
    ICON_COUNT as f32 * ICON_BTN_W + (ICON_COUNT as f32 - 1.0) * ITEM_SPACING_X;
/// Icon row (56 + 6+6 pad_y) + spacing + title row (32) + spacing + slider row (48).
pub const CONTROL_BAR_CONTENT_H: f32 = (ICON_SIZE + 12.0) + 6.0 + 32.0 + 6.0 + 48.0;

/// World width of the control bar panel, in metres.
const CONTROL_BAR_WORLD_W: f32 = 1.6;

/// The control bar panel for `extra_px` pixels of notes above the buttons:
/// (pixel width, pixel height, world width, world height in metres).
pub fn panel_size(extra_px: u32) -> (u32, u32, f32, f32) {
    let w_px = (CONTROL_BAR_CONTENT_W + super::ui::PADDING * 2.0).ceil() as u32;
    let h_px = (CONTROL_BAR_CONTENT_H + super::ui::PADDING * 2.0).ceil() as u32 + extra_px;
    (w_px, h_px, CONTROL_BAR_WORLD_W, CONTROL_BAR_WORLD_W * h_px as f32 / w_px as f32)
}

pub fn draw(ui: &mut Ui, state: &ControlBarState) -> ControlBarActions {
    let mut actions = ControlBarActions::default();

    // ── notes from the .nfo (read-only) ───────────────────────────────────
    if let Some(info) = &state.info {
        if !info.heading.is_empty() {
            ui.label_styled(&info.heading, FONT_DEFAULT, TEXT);
        }
        for line in &info.plot {
            ui.label_styled(line, FONT_SMALL, TEXT_MUTED);
        }
    }

    // ── icon row (no inter-button gap to match toolbar look) ──────────────
    ui.horizontal(|ui| {
        // The panel content area is sized to exactly fit this row, so any
        // residual slack from rounding is harmless — distribute it evenly.
        let slack = (ui.available_width() - CONTROL_BAR_CONTENT_W).max(0.0);
        ui.add_space(slack * 0.5);

        if ui.icon_button(icons::ICON_PREV, ICON_SIZE, false) { actions.prev = true; }

        let play_sprite = if state.is_playing { icons::ICON_PAUSE } else { icons::ICON_PLAY };
        if ui.icon_button(play_sprite, ICON_SIZE, false) { actions.play_pause = true; }

        if ui.icon_button(icons::ICON_NEXT, ICON_SIZE, false) { actions.next = true; }

        let speed_sprite = match state.speed_index {
            0 => icons::ICON_SPEED_1X,
            1 => icons::ICON_SPEED__66X,
            2 => icons::ICON_SPEED__5X,
            3 => icons::ICON_SPEED__33X,
            _ => icons::ICON_SPEED__25X,
        };
        if ui.icon_button(speed_sprite, ICON_SIZE, false) { actions.cycle_speed = true; }

        let loop_sprite = match state.loop_state {
            0 => icons::ICON_LOOP_0,
            1 => icons::ICON_LOOP_1,
            _ => icons::ICON_LOOP_2,
        };
        let loop_active = state.loop_state > 0;
        if ui.icon_button(loop_sprite, ICON_SIZE, loop_active) { actions.cycle_loop = true; }

        if ui.icon_button(icons::ICON_SETTINGS, ICON_SIZE, false) { actions.show_settings = true; }
        if ui.icon_button(icons::ICON_FOLDER,   ICON_SIZE, false) { actions.show_browser  = true; }
        if ui.heart_button(ICON_SIZE, state.is_favorite) { actions.toggle_favorite = true; }
        if ui.speaker_button(ICON_SIZE, state.muted) { actions.speaker_clicked = true; }
        if ui.icon_button(icons::ICON_EXIT,     ICON_SIZE, false) { actions.exit          = true; }
    });

    // ── video name / error  +  app version (centered together) ────────────
    ui.horizontal_h(32.0, |ui| {
        if let Some(err) = &state.error {
            ui.label_styled(err, FONT_DEFAULT, RED);
        } else if let Some(note) = &state.notice {
            ui.label_styled(note, FONT_DEFAULT, NOTICE);
        } else {
            ui.label_styled(&state.video_name, FONT_DEFAULT, TEXT);
        }
        ui.horizontal_rtl(|ui| {
            ui.label_styled(
                concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION")),
                FONT_TINY, TEXT_MUTED,
            );
        });
    });

    // ── the volume slider, when the speaker was clicked: it takes the seek bar's place for a few seconds ──
    if state.volume_open {
        let level = if state.muted { "Muted".to_string() } else { format!("{}%", state.volume_percent) };
        ui.horizontal_h(48.0, |ui| {
            let text_w = ui.measure_text("Muted", FONT_DEFAULT).max(ui.measure_text("100%", FONT_DEFAULT));
            ui.label_styled("Volume", FONT_DEFAULT, TEXT);
            ui.add_space(16.0);
            let slider_w = (ui.available_width() - text_w - 12.0).max(50.0);
            let mut value = if state.muted { 0.0 } else { state.volume_percent as f32 };
            if ui.slider_sized(slider_w, &mut value, 0.0, 100.0) {
                actions.set_volume = Some(value.round() as u8);
            }
            ui.horizontal_rtl(|ui| {
                ui.label_styled(&level, FONT_DEFAULT, if state.muted { RED } else { TEXT });
            });
        });
        return actions;
    }

    // ── seek scrubber + time (centered together at slider height) ─────────
    let time_label = format!(
        "{} / {}",
        fmt_time(state.current_secs),
        if state.duration_secs > 0.0 { fmt_time(state.duration_secs) } else { "--:--".into() }
    );

    let mut frac = if state.duration_secs > 0.0 {
        (state.current_secs / state.duration_secs).clamp(0.0, 1.0) as f32
    } else { 0.0 };

    // 48 = slider height — putting the row at that height vertically centers
    // the (shorter) time label with the (taller) slider.
    ui.horizontal_h(48.0, |ui| {
        // Reserve exactly the time label's pixel width on the right edge,
        // then let the slider take everything to the left of it.
        let time_w   = ui.measure_text(&time_label, FONT_DEFAULT);
        let gap      = 12.0;
        let slider_w = (ui.available_width() - time_w - gap).max(50.0);
        if ui.slider_sized_heat(slider_w, &mut frac, 0.0, 1.0, &state.heat) {
            actions.seek_frac = Some(frac);
        }
        // horizontal_rtl takes the remaining row width and pins its contents
        // to the right edge, so the time label always sits flush right.
        ui.horizontal_rtl(|ui| {
            ui.label_styled(&time_label, FONT_DEFAULT, TEXT);
        });
    });

    actions
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs as u64;
    let m = s / 60;
    let h = m / 60;
    if h > 0 {
        format!("{h}:{:02}:{:02}", m % 60, s % 60)
    } else {
        format!("{m}:{:02}", s % 60)
    }
}
