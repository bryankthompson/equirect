use std::path::PathBuf;
use std::sync::atomic::Ordering;

use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent},
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};

use crate::pointer_renderer::PointerRenderer;
use crate::renderer::Renderer;
use crate::ui::browser::{BrowserEntry, BrowserState, Location};
use crate::ui::settings::{SettingsTab, SettingsView, VideoSettings};
use crate::video_layer::{VideoSwapchain, use_xr_layer};
use crate::library::Library;
use crate::telemetry::Telemetry;
use crate::resume;
use crate::video_meta;

// ── helpers ───────────────────────────────────────────────────────────────────

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Short display name for a video source string.
fn source_display_name(s: &str) -> String {
    if is_url(s) {
        let raw = s.trim_end_matches('/').rsplit('/').next().unwrap_or(s);
        crate::net::url_decode(raw)
    } else {
        std::path::Path::new(s)
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_string()
    }
}

/// The browser panel at the size the user chose (the text and thumbnails keep their pixel size, so a bigger panel holds more).
fn browser_panel_for(
    device: &wgpu::Device,
    fmt: wgpu::TextureFormat,
    pos: glam::Vec3,
    size: crate::browser_prefs::PanelSize,
) -> PanelRenderer {
    let (w_px, h_px, w_m, h_m) = size.dims();
    PanelRenderer::new(device, fmt, w_px, h_px, pos, w_m, h_m)
}

fn source_to_location(s: &str) -> Location {
    if is_url(s) {
        Location::Remote(s.to_string())
    } else {
        Location::Local(PathBuf::from(s))
    }
}

/// Returns the source string of the video `delta` steps away from `current`
/// in a sorted folder listing.  Uses the folder cache; falls back to a fresh
/// read (blocking for remote) and caches the result.
fn adjacent_video(
    current: &str,
    folder_cache: &mut std::collections::HashMap<String, Vec<BrowserEntry>>,
    delta: i32,
) -> Option<String> {
    use crate::ui::browser::{load_local_entries, remote_entries};

    let parent_loc = if is_url(current) {
        Location::Remote(crate::net::parent_url(current))
    } else {
        let p = std::path::Path::new(current);
        Location::Local(p.parent()?.to_path_buf())
    };

    let key = cache_key(&parent_loc);

    let entries: Vec<BrowserEntry> = if let Some(cached) = folder_cache.get(&key) {
        cached.clone()
    } else {
        let fresh = match &parent_loc {
            Location::Local(dir) => load_local_entries(dir),
            Location::Remote(url) => {
                match crate::net::list_http_dir(url) {
                    Ok(items) => remote_entries(items),
                    Err(e) => { eprintln!("prev/next: listing {url} failed: {e}"); return None; }
                }
            }
        };
        folder_cache.insert(key, fresh.clone());
        fresh
    };

    let videos: Vec<String> = entries.iter().filter_map(|e| {
        if let BrowserEntry::Video(_, loc, _) = e {
            Some(location_to_source(loc.clone()))
        } else {
            None
        }
    }).collect();

    if videos.is_empty() { return None; }
    let n   = videos.len() as i32;
    let pos = videos.iter().position(|v: &String| {
        v.trim_end_matches('/') == current.trim_end_matches('/')
    })? as i32;
    Some(videos[((pos + delta).rem_euclid(n)) as usize].clone())
}

fn cache_key(loc: &Location) -> String {
    match loc {
        Location::Local(p)  => p.to_string_lossy().into_owned(),
        Location::Remote(u) => u.trim_end_matches('/').to_string(),
    }
}

fn location_to_source(loc: Location) -> String {
    match loc {
        Location::Local(p)  => p.to_string_lossy().into_owned(),
        Location::Remote(u) => u,
    }
}

fn load_meta(source: &str) -> Option<video_meta::VideoMeta> {
    if is_url(source) {
        video_meta::load_url(source)
    } else {
        video_meta::load(std::path::Path::new(source))
    }
}

fn save_meta(source: &str, meta: &video_meta::VideoMeta) {
    if is_url(source) {
        video_meta::save_url(source, meta);
    } else {
        video_meta::save(std::path::Path::new(source), meta);
    }
}

fn store_meta(source: &str, meta: &video_meta::VideoMeta) {
    if is_url(source) {
        video_meta::store_url(source, meta);
    } else {
        video_meta::store(std::path::Path::new(source), meta);
    }
}

/// What the position file of the playing video holds, as last written here: (video, position; None is "start over").
type PositionWritten = Option<(String, Option<f64>)>;

/// Write where `source` stopped (`pos` of `duration` seconds), or forget it near the start and the end (`resume.rs` has the
/// rules). Leaves the video's settings alone; a video with no settings file yet gets `settings` saved with the position.
fn write_position(
    written: &mut PositionWritten,
    source: &str,
    pos: f64,
    duration: f64,
    settings: &crate::ui::settings::VideoSettings,
) {
    let keep = resume::worth_keeping(pos, duration);
    let same = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1.0,
        (None, None) => true,
        _ => false,
    };
    if let Some((s, p)) = written {
        if s == source && same(*p, keep) { return; }
    }
    let mut meta = match load_meta(source) {
        Some(m) => m,
        None if keep.is_none() => { *written = Some((source.to_string(), None)); return; } // nothing on file to forget
        None => video_meta::VideoMeta::new(settings.clone()),
    };
    if !same(meta.position_secs, keep) {
        meta.position_secs = keep;
        store_meta(source, &meta);
    }
    *written = Some((source.to_string(), keep));
}

// The two next macros are used where `renderer` or `mode` is borrowed from `self`, so they touch only the fields they need
// (a method on `self` would not compile there).

/// Save where the playing video is (see `write_position`).
macro_rules! remember_position {
    ($s:expr) => {
        if let (Some(src), true) = ($s.video_source.as_deref(), $s.video_decoder.is_some()) {
            write_position(
                &mut $s.position_written, src,
                $s.control_bar_state.current_secs, $s.control_bar_state.duration_secs, &$s.video_settings,
            );
        }
    };
}

/// A video that was stopped part way starts where it stopped, with a "Resumed at" note on the control bar (brought up for a few
/// seconds if it was hidden). `$allowed` false (the same video picked again) starts it from the beginning instead.
macro_rules! resume_into {
    ($s:expr, $source:expr, $decoder:expr, $allowed:expr) => {{
        $s.control_bar_state.notice = None;
        $s.resume_notice_until = None;
        let duration = if $decoder.duration_us > 0 { $decoder.duration_us as f64 / 1_000_000.0 } else { 0.0 };
        let saved = if $allowed { load_meta($source).and_then(|m| m.position_secs) } else { None };
        if let Some(start) = resume::start_for(saved, duration) {
            *$decoder.seek_request.lock().unwrap() = Some((start * 1_000_000.0) as u64);
            $s.control_bar_state.current_secs = start;
            $s.seek_target_secs = Some(start);
            $s.seek_timeout = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
            $s.control_bar_state.notice = Some(format!("Resumed at {}", crate::ui::control_bar::fmt_time(start)));
            $s.resume_notice_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(6));
            $s.resume_hides_panel = $s.panel_mode == PanelMode::Hidden;
            if $s.resume_hides_panel { $s.panel_mode = PanelMode::ControlBar; }
            crate::diag::log(&format!("resume: {} at {}", source_display_name($source), crate::ui::control_bar::fmt_time(start)));
        }
    }};
}

// ── App ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum PanelMode {
    ControlBar,
    Browser,
    Settings,
    Hidden,
}
use crate::local::LocalMode;
use crate::mode::Mode;
use crate::ui::assets::UiAssets;
/// Colour cells in the seek bar's heat strip.
const HEAT_CELLS: usize = 200;

use crate::ui::control_bar::{ControlBarState, SPEEDS, info_extra_height, video_info};
use crate::ui::panel::PanelRenderer;
use crate::video::decoder::VideoDecoder;
use crate::video::texture::VideoTexture;
use crate::video_renderer::VideoRenderer;
use crate::vr::{VrContext, VrPreInit};

pub struct App {
    // Drop order matters: mode before renderer so any XR session is destroyed
    // while the wgpu Vulkan device is still alive.
    mode: Option<Box<dyn Mode>>,
    /// True if `--local` was passed on the command line — skip VR setup entirely.
    local_mode: bool,
    /// Shared GPU UI assets (font atlas, icon sheet, pipeline).
    ui_assets: Option<UiAssets>,
    renderer: Option<Renderer>,
    video_decoder: Option<VideoDecoder>,
    video_texture: Option<VideoTexture>,
    video_renderer: Option<VideoRenderer>,    // fisheye shader fallback
    video_swapchain: Option<VideoSwapchain>,  // XR layer path (all other modes)
    panel_renderer: Option<PanelRenderer>,
    pointer_renderer: Option<PointerRenderer>,
    control_bar_state: ControlBarState,
    /// The user's favorites (library.json).
    library: Library,
    /// Play position for the Handy bridge (UDP), if switched on.
    telemetry: Option<Telemetry>,
    /// Current video source: a local file path or an http(s):// URL.
    video_source: Option<String>,
    /// The source whose `.nfo` notes are in `control_bar_state.info` (read once per video).
    info_source: Option<String>,
    /// The funscript beside the playing video, for the heat map on the seek bar, and what the strip was cut for.
    heat_script: Option<crate::heatmap::Script>,
    heat_source: Option<String>,
    heat_duration: f64,
    /// How many pixels taller than plain the control bar panel is, to hold those notes.
    panel_extra_px: u32,
    // File browser
    browser_state: Option<BrowserState>,
    browser_panel: Option<PanelRenderer>,
    /// Pins, thumbnail size and panel size of the browser: saved in browser.json.
    browser_prefs: crate::browser_prefs::BrowserPrefs,
    /// Makes the browser's thumbnails (Windows shell thumbnails, a few background threads).
    thumbs: std::sync::Arc<crate::thumbs::Thumbs>,
    /// Cached folder contents keyed by normalized location string.
    /// Persists across browser open/close cycles.  Entries are invalidated
    /// when the user explicitly clicks a folder to enter it.
    folder_cache: std::collections::HashMap<String, Vec<BrowserEntry>>,
    /// Directory to show in the browser at startup; consumed on first use.
    initial_browser_dir: Option<PathBuf>,
    // Settings
    video_settings: VideoSettings,
    /// Handy stroke range and invert: one value for every video, saved in haptics.json.
    haptics: crate::haptics::Haptics,
    /// Where the screen hangs: one value for every video, saved in screen.json.
    screen: crate::screen::Screen,
    /// Sound level and mute: one value for every video, saved in volume.json.
    volume: crate::volume::Volume,
    /// The speaker's click state and how long the volume slider stays on the control bar.
    volume_row: crate::volume::VolumeRow,
    settings_tab: SettingsTab,
    settings_panel: Option<PanelRenderer>,
    // Panel visibility
    panel_mode: PanelMode,
    seek_target_secs: Option<f64>,
    /// What the position file of the playing video holds, as last written (to skip writing the same thing again).
    position_written: PositionWritten,
    position_saved_at: std::time::Instant,
    /// The "Resumed at" note stays on the control bar until then; `resume_hides_panel`: the bar was brought up for it, so it goes again.
    resume_notice_until: Option<std::time::Instant>,
    resume_hides_panel: bool,
    seek_timeout: Option<std::time::Instant>,
    /// Headset mode: the window holds the desktop mouse (first click), so it drives the panels.
    mouse_captured: bool,
    /// IPC receiver: messages from a second-instance launch (file path or URL).
    ipc_rx: std::sync::mpsc::Receiver<String>,
    /// Pending IPC-requested video source, queued until VR is ready.
    pending_ipc_source: Option<String>,
    /// If set, seek to this position (seconds) once the first video opens.
    initial_start_secs: Option<f64>,
    /// When the previous pass of the frame loop started, to log long gaps (see diag.rs).
    last_pass: Option<std::time::Instant>,
}


impl App {
    pub fn new(
        video_source: Option<String>,
        initial_browser_dir: Option<PathBuf>,
        ipc_rx: std::sync::mpsc::Receiver<String>,
        initial_start_secs: Option<f64>,
        local_mode: bool,
    ) -> Self {
        let panel_mode = if video_source.is_none() && initial_browser_dir.is_some() {
            PanelMode::Browser
        } else {
            PanelMode::ControlBar
        };
        let (library, library_error) = Library::open();
        let mut control_bar_state = ControlBarState::default();
        if let Some(e) = library_error {
            eprintln!("Library: {e}");
            control_bar_state.error = Some(e);
        }
        Self {
            mode: None,
            local_mode,
            ui_assets: None,
            renderer: None,
            video_decoder: None,
            video_texture: None,
            video_renderer: None,
            video_swapchain: None,
            panel_renderer: None,
            pointer_renderer: None,
            control_bar_state,
            library,
            telemetry: Telemetry::from_env(),
            video_source,
            info_source: None,
            heat_script: None,
            heat_source: None,
            heat_duration: 0.0,
            panel_extra_px: 0,
            browser_state: None,
            browser_panel: None,
            browser_prefs: {
                let (p, problem) = crate::browser_prefs::load();
                if let Some(e) = problem {
                    eprintln!("Browser: {e}");
                    crate::diag::log(&format!("browser: {e}"));
                }
                crate::browser_prefs::apply_exclusions(&p);
                p
            },
            thumbs: crate::thumbs::Thumbs::new(),
            folder_cache: std::collections::HashMap::new(),
            initial_browser_dir,
            video_settings: VideoSettings::new(),
            haptics: {
                let (h, problem) = crate::haptics::load();
                if let Some(p) = problem {
                    eprintln!("Haptics: {p}");
                    crate::diag::log(&format!("haptics: {p}"));
                }
                h
            },
            screen: {
                let (s, problem) = crate::screen::load();
                if let Some(p) = problem {
                    eprintln!("Screen: {p}");
                    crate::diag::log(&format!("screen: {p}"));
                }
                s
            },
            volume_row: Default::default(),
            volume: {
                let (v, problem) = crate::volume::load();
                if let Some(p) = problem {
                    eprintln!("Volume: {p}");
                    crate::diag::log(&format!("volume: {p}"));
                }
                crate::volume::publish(&v);
                v
            },
            settings_tab: SettingsTab::default(),
            settings_panel: None,
            panel_mode,
            seek_target_secs: None,
            position_written: None,
            position_saved_at: std::time::Instant::now(),
            resume_notice_until: None,
            resume_hides_panel: false,
            seek_timeout: None,
            mouse_captured: false,
            ipc_rx,
            pending_ipc_source: None,
            initial_start_secs,
            last_pass: None,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title(if cfg!(debug_assertions) {
                            concat!("equirect v", env!("CARGO_PKG_VERSION"), "-dev")
                        } else {
                            concat!("equirect v", env!("CARGO_PKG_VERSION"))
                        })
                        .with_inner_size(winit::dpi::LogicalSize::new(640u32, 428u32)),
                )
                .expect("Failed to create window"),
        );

        // Set title-bar (ICON_SMALL) and taskbar (ICON_BIG) icons by loading
        // the icon embedded in the PE by build.rs/winresource (resource ID 1).
        // SHChangeNotify is required to flush the shell's per-path icon cache;
        // without it the taskbar shows a stale blank icon for the exe path.
        #[cfg(target_os = "windows")]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_UPDATEITEM, SHCNF_PATHW, SHCNF_FLUSH};
            use windows::Win32::UI::WindowsAndMessaging::{
                LoadIconW, SendMessageW, SetClassLongPtrW,
                WM_SETICON, ICON_BIG, ICON_SMALL, GCLP_HICON, GCLP_HICONSM,
            };
            use windows::core::PCWSTR;
            if let Ok(exe) = std::env::current_exe() {
                let wide: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
                unsafe { SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW | SHCNF_FLUSH, Some(wide.as_ptr() as *const _), None); }
            }
            if let Ok(handle) = window.window_handle() {
                if let RawWindowHandle::Win32(h) = handle.as_raw() {
                    unsafe {
                        let hwnd  = HWND(h.hwnd.get() as *mut core::ffi::c_void);
                        let hmod  = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
                        let hinst = Some(HINSTANCE(hmod.0));
                        if let Ok(icon) = LoadIconW(hinst, PCWSTR(1 as *const u16)) {
                            let lparam = Some(LPARAM(icon.0 as isize));
                            SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG   as usize)), lparam);
                            SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), lparam);
                            SetClassLongPtrW(hwnd, GCLP_HICON,   icon.0 as isize);
                            SetClassLongPtrW(hwnd, GCLP_HICONSM, icon.0 as isize);
                        }
                    }
                }
            }
        }

        // VR setup is skipped entirely in --local mode so the app can run
        // without an OpenXR runtime or headset.
        let vr_pre = if self.local_mode { None } else { VrPreInit::new() };
        let xr_exts = vr_pre.as_ref().map(|v| v.required_device_extensions()).unwrap_or_default();

        let mut renderer = Renderer::new(window, &xr_exts);

        let mode: Option<Box<dyn Mode>> = if self.local_mode {
            Some(Box::new(LocalMode::new(&renderer)))
        } else {
            let vr = vr_pre.and_then(|pre| VrContext::new(&renderer, pre));
            if let Some(ref vr) = vr {
                renderer.prepare_for_xr(vr.swapchain_format);
            }
            vr.map(|v| Box::new(v) as Box<dyn Mode>)
        };

        let target_fmt = mode
            .as_ref()
            .map(|m| m.target_format())
            .unwrap_or_else(|| renderer.surface_format());

        if let Some(ref source) = self.video_source {
            let mut from_saved = false;
            if let Some(meta) = load_meta(source) {
                self.video_settings = meta.settings;
                from_saved = true;
            } else if let Some(s) = video_meta::detect_from_name(source) {
                eprintln!("Video: settings guessed from the file name");
                self.video_settings = s;
            }
            let open_started = std::time::Instant::now();
            let opened = VideoDecoder::open(source.clone());
            crate::diag::slow(&format!("opening {}", source_display_name(source)), open_started, 300);
            match opened {
                Err(e) => {
                    self.control_bar_state.error = Some(format!("Can't play video: {e}"));
                    self.panel_mode = PanelMode::ControlBar;
                }
                Ok(decoder) => {
                    self.control_bar_state.error = None;
                    // Not seen before: now that the size is known, fill in what the name left out.
                    if !from_saved {
                        if let Some(s) = video_meta::guess_layout(source, Some((decoder.width, decoder.height))) {
                            save_meta(source, &video_meta::VideoMeta::new(s.clone()));
                            self.video_settings = s;
                        }
                    }
                    let tex = decoder.gpu_bgra_handle
                        .and_then(|h| VideoTexture::new_gpu(
                            &renderer.device, &renderer.instance, &renderer.adapter,
                            decoder.width, decoder.height, h,
                        ))
                        .unwrap_or_else(|| VideoTexture::new(
                            &renderer.device, decoder.width, decoder.height, decoder.is_nv12,
                        ));
                    let vr_rend = VideoRenderer::new(
                        &renderer.device, target_fmt,
                        decoder.width, decoder.height,
                    );
                    let sc = mode.as_ref().and_then(|m| {
                        m.create_video_swapchain(&renderer.device, decoder.width, decoder.height)
                    });
                    self.control_bar_state.video_name = source_display_name(source);
                    if decoder.duration_us > 0 {
                        self.control_bar_state.duration_secs =
                            decoder.duration_us as f64 / 1_000_000.0;
                    }
                    // --start / -t: seek to the requested position on first open (it wins over a resume).
                    if let Some(secs) = self.initial_start_secs.take() {
                        let target_us = (secs * 1_000_000.0) as u64;
                        *decoder.seek_request.lock().unwrap() = Some(target_us);
                        self.control_bar_state.current_secs = secs;
                        self.seek_target_secs = Some(secs);
                        self.seek_timeout = Some(
                            std::time::Instant::now() + std::time::Duration::from_secs(30),
                        );
                    } else {
                        resume_into!(self, source, decoder, true);
                    }
                    self.video_decoder   = Some(decoder);
                    self.video_texture   = Some(tex);
                    self.video_renderer  = Some(vr_rend);
                    self.video_swapchain = sc;
                }
            }
        }

        let cb_pos      = mode.as_ref().map(|m| m.control_bar_position()).unwrap_or(glam::Vec3::new(0.0, 0.0, -2.0));
        let browser_pos = mode.as_ref().map(|m| m.browser_position()).unwrap_or(glam::Vec3::new(0.0, 1.2, -2.0));

        // Size the control bar texture to fit its content exactly — no slack
        // around the icon row, no empty space below the slider.  The constants
        // come from control_bar.rs so changing the icon count or row heights
        // there automatically resizes the panel.
        let (cb_w_px, cb_h_px, cb_w_m, cb_h_m) = crate::ui::control_bar::panel_size(0);

        self.panel_renderer = Some(PanelRenderer::new(
            &renderer.device, target_fmt,
            cb_w_px, cb_h_px,
            cb_pos,
            cb_w_m, cb_h_m,
        ));

        self.pointer_renderer = Some(PointerRenderer::new(&renderer.device, target_fmt));

        if let Some(dir) = self.initial_browser_dir.take() {
            let current_loc = self.video_source.as_deref().map(source_to_location);
            let loc = Location::Local(dir);
            let cached = self.folder_cache.get(&cache_key(&loc)).cloned();
            self.browser_state = Some(BrowserState::new(loc, current_loc, cached, self.browser_prefs.clone(), self.thumbs.clone()));
            self.browser_panel = Some(browser_panel_for(&renderer.device, target_fmt, browser_pos, self.browser_prefs.panel));
        }

        self.ui_assets = Some(UiAssets::new(&renderer.device, &renderer.queue, self.thumbs.clone()));
        self.renderer = Some(renderer);
        self.mode = mode;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Forward every event to the active mode (LocalMode uses these for
        // mouse/keyboard input; VrContext ignores them).
        // Headset mode: the first left click grabs the mouse (cursor hidden, kept inside the
        // window) so it can drive the panels; losing focus (Alt+Tab) gives it back.  The grabbing
        // click itself is not passed on, or it would press whatever the dot sits on.
        let mut pass_on = true;
        if let (Some(m), Some(r)) = (&mut self.mode, &self.renderer) {
            if !m.renders_to_window() {
                match &event {
                    WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. }
                        if !self.mouse_captured =>
                    {
                        let w = r.window();
                        if let Err(e) = w.set_cursor_grab(winit::window::CursorGrabMode::Confined) {
                            eprintln!("mouse: cannot confine the cursor: {e}");
                        }
                        w.set_cursor_visible(false);
                        self.mouse_captured = true;
                        m.set_mouse_active(true);
                        eprintln!("mouse: captured by the player window (Alt+Tab to release)");
                        pass_on = false;
                    }
                    WindowEvent::Focused(false) if self.mouse_captured => {
                        let w = r.window();
                        let _ = w.set_cursor_grab(winit::window::CursorGrabMode::None);
                        w.set_cursor_visible(true);
                        self.mouse_captured = false;
                        m.set_mouse_active(false);
                        eprintln!("mouse: released");
                    }
                    _ => {}
                }
            }
            if pass_on { m.handle_window_event(&event); }
        } else if let Some(m) = &mut self.mode {
            m.handle_window_event(&event);
        }
        match event {
            WindowEvent::CloseRequested => {
                remember_position!(self);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer { r.resize(size); }
            }
            WindowEvent::RedrawRequested => {
                let Some(r) = &mut self.renderer else { return };
                // In modes that render directly to the window (LocalMode), the
                // surface is drawn from `about_to_wait` — don't double-render
                // the logo on top.
                let renders_to_window = self.mode.as_ref().map_or(false, |m| m.renders_to_window());
                if !renders_to_window {
                    if !r.render() {
                        let size = r.window().inner_size();
                        r.resize(size);
                    }
                }
                r.window().request_redraw();
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(m) = &mut self.mode {
            m.handle_device_event(&event);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(renderer) = &self.renderer else { return };

        // A pass that comes much later than the last one is a freeze: write down what was going on.
        if let Some(prev) = self.last_pass {
            let gap = prev.elapsed().as_millis();
            if gap >= 250 {
                let mode = match self.panel_mode {
                    PanelMode::ControlBar => "controls", PanelMode::Browser => "browser",
                    PanelMode::Settings => "settings", PanelMode::Hidden => "hidden",
                };
                crate::diag::log(&format!(
                    "slow: frame loop paused {gap} ms (panel: {mode}, playing: {}, video: {})",
                    self.control_bar_state.is_playing,
                    self.video_source.as_deref().map_or("none".to_string(), source_display_name),
                ));
            }
        }
        self.last_pass = Some(std::time::Instant::now());

        // Poll IPC channel for messages from a second instance.
        while let Ok(msg) = self.ipc_rx.try_recv() {
            if msg.is_empty() {
                renderer.window().focus_window();
            } else {
                self.pending_ipc_source = Some(msg);
            }
        }

        if self.control_bar_state.duration_secs == 0.0 {
            if let Some(dec) = &self.video_decoder {
                if dec.duration_us > 0 {
                    self.control_bar_state.duration_secs =
                        dec.duration_us as f64 / 1_000_000.0;
                }
            }
        }

        if let (Some(decoder), Some(texture)) = (&self.video_decoder, &self.video_texture) {
            if let Some(frame) = decoder.take_frame() {
                texture.upload(&renderer.queue, &frame);
                if let Some(vr_rend) = &mut self.video_renderer {
                    vr_rend.set_texture(&renderer.device, texture);
                }
                if let Some(sc) = &mut self.video_swapchain {
                    sc.set_texture(&renderer.device, &renderer.queue, texture);
                }
            }
            let pts_secs = decoder.current_pts_us.load(Ordering::Relaxed) as f64 / 1_000_000.0;
            if let Some(target) = self.seek_target_secs {
                let timed_out = self.seek_timeout
                    .map_or(false, |t| std::time::Instant::now() >= t);
                if pts_secs >= target - 0.1 || timed_out {
                    self.control_bar_state.current_secs = pts_secs;
                    self.seek_target_secs = None;
                    self.seek_timeout = None;
                }
            } else {
                self.control_bar_state.current_secs = pts_secs;
            }
        }

        // Remember where playback is every few seconds, and take the "Resumed at" note off again.
        if self.control_bar_state.is_playing && self.seek_target_secs.is_none()
            && self.position_saved_at.elapsed() >= std::time::Duration::from_secs(5)
        {
            self.position_saved_at = std::time::Instant::now();
            remember_position!(self);
        }
        if let Some(until) = self.resume_notice_until {
            if std::time::Instant::now() >= until {
                self.resume_notice_until = None;
                self.control_bar_state.notice = None;
                if self.resume_hides_panel && self.panel_mode == PanelMode::ControlBar {
                    self.panel_mode = PanelMode::Hidden;
                }
            }
        }

        if let Some(mode) = &mut self.mode {
            if mode.exit_requested() {
                remember_position!(self);
                event_loop.exit();
                return;
            }
            if mode.should_quit() {
                renderer
                    .device
                    .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
                    .ok();
                self.video_swapchain = None;
                self.mode = None;
                return;
            }

            let use_layer = mode.supports_video_layer()
                && use_xr_layer(&self.video_settings, mode.supports_equirect2());
            let mut video_settings_snap = self.video_settings.clone();
            video_settings_snap.screen = self.screen;
            let video_layer_arg =
                if use_layer { self.video_swapchain.as_mut().map(|sc| (sc, &video_settings_snap)) }
                else { None };
            let video_shader_arg = if !use_layer {
                self.video_renderer.as_ref()
                    .zip(self.video_texture.as_ref())
                    .map(|(r, t)| (r, t, &video_settings_snap))
            } else { None };

            self.control_bar_state.is_favorite = self.video_source.as_deref()
                .map_or(false, |s| self.library.is_favorite(s));
            self.control_bar_state.muted = self.volume.muted;
            self.control_bar_state.volume_percent = self.volume.percent;
            self.control_bar_state.volume_open = self.volume_row.is_open(std::time::Instant::now());
            if let (Some(tel), Some(src), Some(_)) =
                (self.telemetry.as_mut(), self.video_source.as_deref(), self.video_decoder.as_ref())
            {
                tel.tick(
                    src,
                    self.control_bar_state.current_secs,
                    self.control_bar_state.is_playing,
                    SPEEDS[self.control_bar_state.speed_index],
                    (self.video_settings.haptics_offset_ms, self.haptics.clone()),
                );
            }
            // ── heat map from the funscript: read once per video, cut again when the length is known ──
            if self.heat_source != self.video_source {
                self.heat_source = self.video_source.clone();
                self.heat_script = self.video_source.as_deref()
                    .filter(|s| !is_url(s))
                    .and_then(|s| crate::heatmap::read_beside(std::path::Path::new(s)));
                self.heat_duration = 0.0;
                self.control_bar_state.heat.clear();
            }
            let duration = self.control_bar_state.duration_secs;
            if let Some(script) = &self.heat_script {
                if duration > 0.0 && (duration - self.heat_duration).abs() > 0.5 {
                    self.heat_duration = duration;
                    self.control_bar_state.heat = script.heat(duration, HEAT_CELLS);
                }
            }
            // ── notes from the video's .nfo: read once per video, read-only ───────
            if self.info_source != self.video_source {
                self.info_source = self.video_source.clone();
                let name = self.video_source.as_deref().map(source_display_name).unwrap_or_default();
                let nfo = self.video_source.as_deref()
                    .filter(|s| !is_url(s))
                    .and_then(|s| crate::nfo::read_beside(std::path::Path::new(s)));
                let atlas = &mut self.ui_assets.as_mut().expect("UiAssets not initialised").atlas;
                self.control_bar_state.info = nfo.and_then(|n| video_info(&n, &name, atlas));
                let extra = info_extra_height(self.control_bar_state.info.as_ref(), atlas).ceil() as u32;
                if extra != self.panel_extra_px {
                    // Grow upward: the bottom edge, and so the buttons, stay where they were.
                    let (w_px, h_px, w_m, h_m) = crate::ui::control_bar::panel_size(extra);
                    let mut pos = mode.control_bar_position();
                    pos.y += (h_m - crate::ui::control_bar::panel_size(0).3) / 2.0;
                    self.panel_renderer = Some(PanelRenderer::new(
                        &renderer.device, mode.target_format(),
                        w_px, h_px,
                        pos,
                        w_m, h_m,
                    ));
                    self.panel_extra_px = extra;
                    crate::diag::log(&format!("notes: control bar is {extra} px taller"));
                }
            }
            let panel_arg =
                if self.panel_mode == PanelMode::ControlBar {
                    self.panel_renderer.as_mut().map(|p| (p, &self.control_bar_state))
                } else { None };
            let browser_arg =
                if self.panel_mode == PanelMode::Browser {
                    self.browser_panel.as_mut().zip(self.browser_state.as_mut())
                } else { None };
            let settings_arg =
                if self.panel_mode == PanelMode::Settings {
                    self.settings_panel.as_mut().map(|p| (p, SettingsView {
                        video:   &mut self.video_settings,
                        haptics: &mut self.haptics,
                        screen:  &mut self.screen,
                        volume:  &mut self.volume,
                        tab:     &mut self.settings_tab,
                    }))
                } else { None };
            let pointer_arg =
                if self.panel_mode != PanelMode::Hidden { self.pointer_renderer.as_ref() }
                else { None };

            let ui_assets = self.ui_assets.as_mut().expect("UiAssets not initialised");
            let (actions, browser_actions, settings_actions) = mode.render_frame(
                renderer,
                ui_assets,
                video_layer_arg,
                video_shader_arg,
                panel_arg,
                browser_arg,
                settings_arg,
                pointer_arg,
            );

            // ── sound: a click on the speaker opens the volume slider, a double click mutes; the Audio page sets it too ─
            let now = std::time::Instant::now();
            let mut volume_changed = settings_actions.volume_changed;
            if actions.speaker_clicked && self.volume_row.click(now) {
                self.volume.muted = !self.volume.muted;
                volume_changed = true;
            }
            if let Some(percent) = actions.set_volume {
                self.volume.set_percent(percent as f32);
                self.volume.muted = false; // moving the slider means "I want to hear it"
                self.volume_row.touch(now);
                volume_changed = true;
            }
            if volume_changed {
                crate::volume::publish(&self.volume);
                if let Err(e) = crate::volume::save(&self.volume) {
                    eprintln!("Volume: {e}");
                    crate::diag::log(&format!("volume: {e}"));
                }
            }

            // ── favorites ─────────────────────────────────────────────────────

            if actions.toggle_favorite {
                if let Some(src) = self.video_source.clone() {
                    let (now, err) = self.library.toggle(&src);
                    eprintln!("Library: {} {}", if now { "favorite added:" } else { "favorite removed:" }, src);
                    if let Some(e) = err {
                        eprintln!("Library: {e}");
                        self.control_bar_state.error = Some(format!("Favorite not saved: {e}"));
                    }
                }
            }
            if browser_actions.open_favorites {
                let mut entries = vec![BrowserEntry::Parent];
                for fav in self.library.favorites() {
                    let loc = source_to_location(&fav.source);
                    let scripted = match &loc {
                        Location::Local(p) => p.with_extension("funscript").is_file(),
                        Location::Remote(_) => false,
                    };
                    entries.push(BrowserEntry::Video(source_display_name(&fav.source), loc, scripted));
                }
                if let Some(bs) = &mut self.browser_state {
                    bs.favorite_entries = entries;
                    bs.showing_favorites = true;
                }
            }

            // ── browser actions ───────────────────────────────────────────────

            if browser_actions.close {
                self.browser_state = None;
                self.browser_panel = None;
                self.panel_mode    = PanelMode::ControlBar;
            }

            // A pin, the thumbnail size or the panel size changed: remember it, and hang a panel of the new size.
            if browser_actions.prefs_changed {
                if let Some(bs) = &self.browser_state {
                    let resized = bs.prefs.panel != self.browser_prefs.panel;
                    self.browser_prefs = bs.prefs.clone();
                    crate::browser_prefs::apply_exclusions(&self.browser_prefs);
                    if let Err(e) = crate::browser_prefs::save(&self.browser_prefs) {
                        eprintln!("Browser: {e}");
                        crate::diag::log(&format!("browser: {e}"));
                    }
                    if resized && self.browser_panel.is_some() {
                        self.browser_panel = Some(browser_panel_for(
                            &renderer.device, mode.target_format(), mode.browser_position(), self.browser_prefs.panel,
                        ));
                    }
                }
            }

            // ".." / parent navigation — use cache.
            if let Some(loc) = browser_actions.navigate {
                if let Some(dir) = loc.as_local() {
                    let vol_root = crate::volumes::volume_root_of(dir);
                    video_meta::save_last_dir(dir);
                    video_meta::save_volume_last_dir(&vol_root, dir);
                }
                let cached = self.folder_cache.get(&cache_key(&loc)).cloned();
                if let Some(bs) = &mut self.browser_state {
                    bs.navigate_to(loc, cached);
                }
            }

            // Explicit folder click — invalidate that folder's cache entry.
            if let Some(loc) = browser_actions.select_dir {
                if let Some(dir) = loc.as_local() {
                    let vol_root = crate::volumes::volume_root_of(dir);
                    video_meta::save_last_dir(dir);
                    video_meta::save_volume_last_dir(&vol_root, dir);
                }
                self.folder_cache.remove(&cache_key(&loc));
                if let Some(bs) = &mut self.browser_state {
                    bs.navigate_to(loc, None);
                }
            }

            if let Some(vol_root) = browser_actions.select_volume {
                if let Some(bs) = &self.browser_state {
                    if let Some(cur_dir) = bs.location.as_local() {
                        let cur_root = crate::volumes::volume_root_of(cur_dir);
                        video_meta::save_volume_last_dir(&cur_root, cur_dir);
                    }
                }
                let target = video_meta::resolve_dir_for_volume(&vol_root);
                video_meta::save_last_dir(&target);
                let loc = Location::Local(target);
                let cached = self.folder_cache.get(&cache_key(&loc)).cloned();
                if let Some(bs) = &mut self.browser_state {
                    bs.navigate_to(loc, cached);
                }
            }

            // Write freshly-loaded entries into the cache.
            if let Some(bs) = &mut self.browser_state {
                if bs.just_loaded {
                    self.folder_cache.insert(cache_key(&bs.location), bs.entries.clone());
                    bs.just_loaded = false;
                }
            }

            // Determine whether we're loading a new source from the browser or from IPC.
            let play_source: Option<String> = if let Some(play_loc) = browser_actions.play {
                if let Location::Local(ref p) = play_loc {
                    if let Some(parent) = p.parent() {
                        video_meta::save_last_dir(parent);
                    }
                }
                self.browser_state = None;
                self.browser_panel = None;
                self.panel_mode    = PanelMode::Hidden;
                Some(location_to_source(play_loc))
            } else if let Some(src) = self.pending_ipc_source.take() {
                renderer.window().focus_window();
                // Directory → open browser there; file/URL → play as video.
                if std::path::Path::new(&src).is_dir() {
                    let dir = std::path::PathBuf::from(&src);
                    video_meta::save_last_dir(&dir);
                    let current_loc = self.video_source.as_deref().map(source_to_location);
                    let loc = Location::Local(dir);
                    let cached = self.folder_cache.get(&cache_key(&loc)).cloned();
                    self.browser_state = Some(BrowserState::new(loc, current_loc, cached, self.browser_prefs.clone(), self.thumbs.clone()));
                    if self.browser_panel.is_none() {
                        self.browser_panel = Some(browser_panel_for(
                            &renderer.device, mode.target_format(), mode.browser_position(), self.browser_prefs.panel,
                        ));
                    }
                    self.panel_mode = PanelMode::Browser;
                    None
                } else {
                    let loc = source_to_location(&src);
                    if let Location::Local(ref p) = loc {
                        if let Some(parent) = p.parent() {
                            video_meta::save_last_dir(parent);
                        }
                    }
                    self.browser_state = None;
                    self.browser_panel = None;
                    self.panel_mode    = PanelMode::Hidden;
                    Some(src)
                }
            } else {
                None
            };

            if let Some(new_source) = play_source {
                let target_fmt = mode.target_format();
                // Where the video that is leaving stopped; picking the playing video again starts it over.
                remember_position!(self);
                let same_video = self.video_source.as_deref() == Some(new_source.as_str());

                self.video_settings = load_meta(&new_source)
                    .map(|m| m.settings)
                    .unwrap_or_else(|| {
                        // No saved settings: guess from the file name; a name with no hint is
                        // shown flat (not the previous video's settings, which broke 2D clips
                        // played after a 180 one).
                        match video_meta::detect_from_name(&new_source) {
                            Some(s) => {
                                save_meta(&new_source, &video_meta::VideoMeta::new(s.clone()));
                                s
                            }
                            None => crate::ui::settings::VideoSettings::new(),
                        }
                    });

                let from_saved = load_meta(&new_source).is_some();

                self.video_swapchain = None;
                self.video_decoder   = None;
                self.video_texture   = None;
                self.video_renderer  = None;

                let open_started = std::time::Instant::now();
                let opened = VideoDecoder::open(new_source.clone());
                crate::diag::slow(&format!("opening {}", source_display_name(&new_source)), open_started, 300);
                match opened {
                    Err(e) => {
                        self.control_bar_state.error = Some(format!("Can't play video: {e}"));
                        self.panel_mode  = PanelMode::ControlBar;
                        self.video_source = Some(new_source);
                    }
                    Ok(decoder) => {
                        self.control_bar_state.error = None;
                        // Not seen before: now that the size is known, fill in what the name left out.
                        if !from_saved {
                            if let Some(s) = video_meta::guess_layout(&new_source, Some((decoder.width, decoder.height))) {
                                save_meta(&new_source, &video_meta::VideoMeta::new(s.clone()));
                                self.video_settings = s;
                            }
                        }
                        let tex = decoder.gpu_bgra_handle
                            .and_then(|h| VideoTexture::new_gpu(
                                &renderer.device, &renderer.instance, &renderer.adapter,
                                decoder.width, decoder.height, h,
                            ))
                            .unwrap_or_else(|| VideoTexture::new(
                                &renderer.device, decoder.width, decoder.height, decoder.is_nv12,
                            ));
                        let vr_rend = VideoRenderer::new(
                            &renderer.device, target_fmt,
                            decoder.width, decoder.height,
                        );
                        let sc = mode.create_video_swapchain(
                            &renderer.device, decoder.width, decoder.height,
                        );
                        self.control_bar_state.video_name = source_display_name(&new_source);
                        self.control_bar_state.duration_secs =
                            if decoder.duration_us > 0 { decoder.duration_us as f64 / 1_000_000.0 }
                            else { 0.0 };
                        self.control_bar_state.current_secs = 0.0;
                        self.control_bar_state.is_playing   = true;
                        resume_into!(self, &new_source, decoder, !same_video);
                        self.video_source    = Some(new_source);
                        self.video_decoder   = Some(decoder);
                        self.video_texture   = Some(tex);
                        self.video_renderer  = Some(vr_rend);
                        self.video_swapchain = sc;
                    }
                }
            }

            // ── settings actions ──────────────────────────────────────────────

            // Reset: the settings-screen button, or both grips squeezed together (which also
            // brings the control bar back, in case the settings screen is the problem).
            if settings_actions.reset || actions.reset_settings {
                let src = self.video_source.clone();
                let dims = self.video_decoder.as_ref().map(|d| (d.width, d.height));
                let fresh = src.as_deref()
                    .and_then(|s| video_meta::guess_layout(s, dims))
                    .unwrap_or_else(crate::ui::settings::VideoSettings::new);
                self.video_settings = fresh.clone();
                if let Some(s) = &src {
                    save_meta(s, &video_meta::VideoMeta::new(fresh));
                }
                if actions.reset_settings {
                    self.panel_mode = PanelMode::ControlBar;
                }
            }
            if settings_actions.center {
                mode.center();
            }
            if settings_actions.changed {
                if let Some(ref source) = self.video_source {
                    save_meta(source, &video_meta::VideoMeta::new(self.video_settings.clone()));
                }
            }
            if settings_actions.haptics_changed {
                if let Err(e) = crate::haptics::save(&self.haptics) {
                    eprintln!("Haptics: {e}");
                    crate::diag::log(&format!("haptics: {e}"));
                }
            }
            if settings_actions.screen_changed {
                if let Err(e) = crate::screen::save(&self.screen) {
                    eprintln!("Screen: {e}");
                    crate::diag::log(&format!("screen: {e}"));
                }
            }
            if settings_actions.close {
                self.panel_mode = PanelMode::ControlBar;
            }

            // ── control bar actions ───────────────────────────────────────────

            if actions.play_pause {
                self.control_bar_state.is_playing = !self.control_bar_state.is_playing;
                if let Some(dec) = &self.video_decoder {
                    dec.paused.store(!self.control_bar_state.is_playing, Ordering::Relaxed);
                }
            }

            if actions.cycle_speed {
                self.control_bar_state.speed_index =
                    (self.control_bar_state.speed_index + 1) % SPEEDS.len();
                if let Some(dec) = &self.video_decoder {
                    dec.speed_index.store(
                        self.control_bar_state.speed_index as u32, Ordering::Relaxed,
                    );
                }
            }

            if actions.cycle_loop {
                let current_us = self.video_decoder.as_ref()
                    .map(|d| d.current_pts_us.load(Ordering::Relaxed))
                    .unwrap_or(0);
                match self.control_bar_state.loop_state {
                    0 => {
                        if let Some(dec) = &self.video_decoder {
                            dec.loop_start_us.store(current_us, Ordering::Relaxed);
                            dec.loop_state.store(1, Ordering::Relaxed);
                        }
                        self.control_bar_state.loop_state = 1;
                    }
                    1 => {
                        if let Some(dec) = &self.video_decoder {
                            dec.loop_end_us.store(current_us, Ordering::Relaxed);
                            dec.loop_state.store(2, Ordering::Relaxed);
                        }
                        self.control_bar_state.loop_state = 2;
                    }
                    _ => {
                        if let Some(dec) = &self.video_decoder {
                            dec.loop_state.store(0, Ordering::Relaxed);
                        }
                        self.control_bar_state.loop_state = 0;
                    }
                }
            }

            if let Some(frac) = actions.seek_frac {
                if let Some(dec) = &self.video_decoder {
                    if dec.duration_us > 0 {
                        let target_us = (frac as f64 * dec.duration_us as f64) as u64;
                        *dec.seek_request.lock().unwrap() = Some(target_us);
                        dec.audio_seek.store(target_us, Ordering::Relaxed);
                        dec.audio_flush_gen.fetch_add(1, Ordering::Relaxed);
                        let target_secs = target_us as f64 / 1_000_000.0;
                        self.control_bar_state.current_secs = target_secs;
                        self.seek_target_secs = Some(target_secs);
                        self.seek_timeout = Some(
                            std::time::Instant::now() + std::time::Duration::from_millis(500),
                        );
                    }
                }
            }

            if let Some(delta_secs) = actions.seek_delta_secs {
                if let Some(dec) = &self.video_decoder {
                    if dec.duration_us > 0 {
                        let duration_secs = dec.duration_us as f64 / 1_000_000.0;
                        let target_secs =
                            (self.control_bar_state.current_secs + delta_secs)
                            .rem_euclid(duration_secs);
                        let target_us = (target_secs * 1_000_000.0) as u64;
                        *dec.seek_request.lock().unwrap() = Some(target_us);
                        dec.audio_seek.store(target_us, Ordering::Relaxed);
                        dec.audio_flush_gen.fetch_add(1, Ordering::Relaxed);
                        self.control_bar_state.current_secs = target_secs;
                        self.seek_target_secs = Some(target_secs);
                        self.seek_timeout = Some(
                            std::time::Instant::now() + std::time::Duration::from_millis(500),
                        );
                    }
                }
            }

            // ── prev / next video ─────────────────────────────────────────────
            let nav_delta: Option<i32> = if actions.prev { Some(-1) }
                else if actions.next { Some(1) }
                else { None };

            if let Some(delta) = nav_delta {
                let next = self.video_source.as_deref()
                    .and_then(|s| adjacent_video(s, &mut self.folder_cache, delta));

                if let Some(new_source) = next {
                    let target_fmt = mode.target_format();
                    remember_position!(self);
                    // Same rule as picking from the browser: saved settings, else a guess from the
                    // name (flat when the name says nothing), never the previous video's settings.
                    let from_saved = load_meta(&new_source).is_some();
                    self.video_settings = load_meta(&new_source)
                        .map(|m| m.settings)
                        .unwrap_or_else(|| {
                            video_meta::detect_from_name(&new_source)
                                .unwrap_or_else(crate::ui::settings::VideoSettings::new)
                        });
                    self.video_swapchain = None;
                    self.video_decoder   = None;
                    self.video_texture   = None;
                    self.video_renderer  = None;
                    let open_started = std::time::Instant::now();
                let opened = VideoDecoder::open(new_source.clone());
                crate::diag::slow(&format!("opening {}", source_display_name(&new_source)), open_started, 300);
                match opened {
                        Err(e) => {
                            self.control_bar_state.error = Some(format!("Can't play video: {e}"));
                            self.panel_mode  = PanelMode::ControlBar;
                            self.video_source = Some(new_source);
                        }
                        Ok(decoder) => {
                            self.control_bar_state.error = None;
                            if !from_saved {
                                if let Some(s) = video_meta::guess_layout(&new_source, Some((decoder.width, decoder.height))) {
                                    save_meta(&new_source, &video_meta::VideoMeta::new(s.clone()));
                                    self.video_settings = s;
                                }
                            }
                            let tex = decoder.gpu_bgra_handle
                                .and_then(|h| VideoTexture::new_gpu(
                                    &renderer.device, &renderer.instance, &renderer.adapter,
                                    decoder.width, decoder.height, h,
                                ))
                                .unwrap_or_else(|| VideoTexture::new(
                                    &renderer.device, decoder.width, decoder.height, decoder.is_nv12,
                                ));
                            let vr_rend = VideoRenderer::new(
                                &renderer.device, target_fmt,
                                decoder.width, decoder.height,
                            );
                            let sc = mode.create_video_swapchain(
                                &renderer.device, decoder.width, decoder.height,
                            );
                            self.control_bar_state.video_name = source_display_name(&new_source);
                            self.control_bar_state.duration_secs =
                                if decoder.duration_us > 0 { decoder.duration_us as f64 / 1_000_000.0 }
                                else { 0.0 };
                            self.control_bar_state.current_secs = 0.0;
                            self.control_bar_state.is_playing   = true;
                            resume_into!(self, &new_source, decoder, true);
                            self.video_source    = Some(new_source);
                            self.video_decoder   = Some(decoder);
                            self.video_texture   = Some(tex);
                            self.video_renderer  = Some(vr_rend);
                            self.video_swapchain = sc;
                        }
                    }
                }
            }

            if actions.show_settings && self.panel_mode != PanelMode::Settings {
                if self.settings_panel.is_none() {
                    self.settings_panel = Some(PanelRenderer::new(
                        &renderer.device, mode.target_format(),
                        800, 600,
                        mode.settings_position(),
                        1.6, 1.0,
                    ));
                }
                self.panel_mode = PanelMode::Settings;
            }

            if actions.show_browser && self.panel_mode != PanelMode::Browser {
                if self.browser_state.is_none() {
                    let start_loc = match self.video_source.as_deref() {
                        Some(s) if is_url(s) => {
                            Location::Remote(crate::net::parent_url(s))
                        }
                        Some(s) => {
                            let p = std::path::Path::new(s);
                            Location::Local(
                                p.parent().map(|d| d.to_path_buf())
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                            )
                        }
                        None => Location::Local(std::env::current_dir().unwrap_or_default()),
                    };
                    let current_loc = self.video_source.as_deref().map(source_to_location);
                    let cached = self.folder_cache.get(&cache_key(&start_loc)).cloned();
                    self.browser_state = Some(BrowserState::new(start_loc, current_loc, cached, self.browser_prefs.clone(), self.thumbs.clone()));
                    self.browser_panel = Some(browser_panel_for(
                        &renderer.device, mode.target_format(), mode.browser_position(), self.browser_prefs.panel,
                    ));
                }
                self.panel_mode = PanelMode::Browser;
            }

            if actions.menu_toggle {
                self.panel_mode = match self.panel_mode {
                    PanelMode::Hidden     => PanelMode::ControlBar,
                    PanelMode::ControlBar => PanelMode::Hidden,
                    PanelMode::Settings   => PanelMode::ControlBar,
                    PanelMode::Browser    => PanelMode::ControlBar,
                };
            }

            if actions.exit {
                if let (Some(tel), Some(src)) = (self.telemetry.as_mut(), self.video_source.as_deref()) {
                    tel.stop(src, self.control_bar_state.current_secs, SPEEDS[self.control_bar_state.speed_index]);
                }
                remember_position!(self);
                mode.request_exit();
                event_loop.exit();
            }
        }

        renderer.window().request_redraw();
    }
}
