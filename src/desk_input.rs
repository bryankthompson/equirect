//! Desktop mouse and keyboard, usable with the headset on.
//!
//! The controllers keep working; this adds a third pointer (the mouse) and keyboard
//! shortcuts that become the same `ControlBarActions` a button press would.  The player
//! window must have focus (click it once, before or after putting the headset on).
//!
//! Mouse: raw motion moves a dot on the visible panel, left button = trigger, wheel scrolls
//! lists.  The window grabs the cursor on the first click (see `App::window_event`) so
//! clicks cannot land on other windows; Alt+Tab gives the desktop mouse back.
//!
//! Keys: Space/K pause, Left/Right seek 5 s (Shift 30 s), Home start, N/P or PgDn/PgUp next/prev,
//! F or H favorite, B browser, S settings, L loop, ] speed, M/Tab show or hide the panel
//! (Esc too, in the headset).

use winit::event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{KeyCode, PhysicalKey};

use crate::input::ControllerState;
use crate::ui::control_bar::ControlBarActions;
use crate::ui::panel::PanelRenderer;

/// Panel pixels moved per raw mouse count.
const MOUSE_GAIN: f32 = 1.5;
/// Upper bound for the stored position, so flinging the mouse never needs a long way back. It must reach the far corner of the
/// biggest panel (the Huge browser is 1600 x 1200 px); `pointer` then pulls it back inside the panel that is showing.
const MAX_PX: f32 = 2048.0;
/// List scroll per wheel notch, in thumbstick units (the panel multiplies by its scroll speed).
const WHEEL_NOTCH: f32 = 3.0;

pub struct DeskInput {
    /// Mouse position in panel pixels.
    pos: (f32, f32),
    /// The window has grabbed the mouse; until then motion, buttons and wheel are ignored.
    active: bool,
    clicking: bool,
    /// Wheel notches since the last frame (up is positive).
    wheel: f32,
    /// Left button went down since the last frame.
    click_edge: bool,
    shift: bool,
    /// Esc maps to the panel toggle (the headset mode has no other use for it).
    esc_toggles_panel: bool,
    pending: ControlBarActions,
}

impl DeskInput {
    pub fn new(esc_toggles_panel: bool) -> Self {
        Self {
            pos: (410.0, 80.0),
            active: false,
            clicking: false,
            wheel: 0.0,
            click_edge: false,
            shift: false,
            esc_toggles_panel,
            pending: ControlBarActions::default(),
        }
    }

    /// The window grabbed (true) or released (false) the mouse.
    pub fn set_active(&mut self, on: bool) {
        self.active = on;
        if !on {
            self.clicking = false;
            self.wheel = 0.0;
        }
    }

    pub fn handle_device_event(&mut self, event: &DeviceEvent) {
        if let (true, DeviceEvent::MouseMotion { delta: (dx, dy) }) = (self.active, event) {
            self.pos.0 = (self.pos.0 + *dx as f32 * MOUSE_GAIN).clamp(0.0, MAX_PX);
            self.pos.1 = (self.pos.1 + *dy as f32 * MOUSE_GAIN).clamp(0.0, MAX_PX);
        }
    }

    pub fn handle_window_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::ModifiersChanged(m) => self.shift = m.state().shift_key(),
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } if self.active => {
                let down = *state == ElementState::Pressed;
                if down && !self.clicking { self.click_edge = true; }
                self.clicking = down;
            }
            WindowEvent::MouseWheel { delta, .. } if self.active => {
                self.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32 / 40.0,
                };
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.key(code, event.repeat);
                }
            }
            _ => {}
        }
    }

    fn key(&mut self, code: KeyCode, repeat: bool) {
        let p = &mut self.pending;
        // Seeking repeats while the key is held; everything else fires once per press.
        let seek = |p: &mut ControlBarActions, secs: f64| {
            *p.seek_delta_secs.get_or_insert(0.0) += secs;
        };
        let step = if self.shift { 30.0 } else { 5.0 };
        match code {
            KeyCode::ArrowRight => seek(p,  step),
            KeyCode::ArrowLeft  => seek(p, -step),
            _ if repeat => {}
            KeyCode::Space | KeyCode::KeyK            => p.play_pause = true,
            KeyCode::KeyN | KeyCode::PageDown         => p.next = true,
            KeyCode::KeyP | KeyCode::PageUp           => p.prev = true,
            KeyCode::KeyF | KeyCode::KeyH             => p.toggle_favorite = true,
            KeyCode::KeyB                             => p.show_browser = true,
            KeyCode::KeyS                             => p.show_settings = true,
            KeyCode::KeyL                             => p.cycle_loop = true,
            KeyCode::BracketRight                     => p.cycle_speed = true,
            KeyCode::Home                             => p.seek_frac = Some(0.0),
            KeyCode::KeyM | KeyCode::Tab              => p.menu_toggle = true,
            KeyCode::Escape if self.esc_toggles_panel => p.menu_toggle = true,
            _ => {}
        }
    }

    /// Fold the keys pressed since the last frame into `actions`.
    pub fn apply_keys(&mut self, actions: &mut ControlBarActions) {
        let k = std::mem::take(&mut self.pending);
        actions.play_pause      |= k.play_pause;
        actions.prev            |= k.prev;
        actions.next            |= k.next;
        actions.cycle_speed     |= k.cycle_speed;
        actions.cycle_loop      |= k.cycle_loop;
        actions.show_settings   |= k.show_settings;
        actions.show_browser    |= k.show_browser;
        actions.toggle_favorite |= k.toggle_favorite;
        actions.menu_toggle     |= k.menu_toggle;
        if k.seek_frac.is_some() { actions.seek_frac = k.seek_frac; }
        if let Some(d) = k.seek_delta_secs {
            *actions.seek_delta_secs.get_or_insert(0.0) += d;
        }
    }

    /// True once per left-button press (for "click shows the hidden panel").
    pub fn take_click(&mut self) -> bool {
        std::mem::take(&mut self.click_edge)
    }

    /// The mouse as a pointer aimed at `panel`, in the panel's own frame; `None` until the
    /// window has grabbed the mouse.
    pub fn pointer(&mut self, panel: &PanelRenderer) -> Option<ControllerState> {
        if !self.active { return None; }
        let (w, h) = panel.pixel_size();
        // One pixel inside the edge, so rounding never makes the ray miss the quad.
        self.pos.0 = self.pos.0.clamp(1.0, w - 1.0);
        self.pos.1 = self.pos.1.clamp(1.0, h - 1.0);
        let (origin, dir) = panel.ray_through_pixel(self.pos.0, self.pos.1);
        Some(ControllerState {
            ray_origin:   origin,
            ray_dir:      dir,
            pose_valid:   true,
            clicking:     self.clicking,
            thumbstick_x: 0.0,
            thumbstick_y: std::mem::take(&mut self.wheel) * WHEEL_NOTCH,
            menu_pressed: false,
            grip_pressed: false,
            is_mouse:     true,
        })
    }
}

#[cfg(test)]
mod desk_input_tests {
    use super::*;

    fn pressed(d: &mut DeskInput, code: KeyCode, repeat: bool) { d.key(code, repeat); }

    #[test]
    fn keys_become_the_same_actions_as_the_buttons() {
        let mut d = DeskInput::new(true);
        pressed(&mut d, KeyCode::Space, false);
        pressed(&mut d, KeyCode::KeyF, false);
        pressed(&mut d, KeyCode::Escape, false);
        let mut a = ControlBarActions::default();
        d.apply_keys(&mut a);
        assert!(a.play_pause && a.toggle_favorite && a.menu_toggle);
        // taken once: a second frame sees nothing
        let mut b = ControlBarActions::default();
        d.apply_keys(&mut b);
        assert!(!b.play_pause && !b.toggle_favorite && !b.menu_toggle);
    }

    #[test]
    fn held_arrow_keys_keep_seeking_but_other_keys_do_not_repeat() {
        let mut d = DeskInput::new(true);
        pressed(&mut d, KeyCode::ArrowRight, false);
        pressed(&mut d, KeyCode::ArrowRight, true);
        pressed(&mut d, KeyCode::Space, true);
        let mut a = ControlBarActions::default();
        d.apply_keys(&mut a);
        assert_eq!(a.seek_delta_secs, Some(10.0));
        assert!(!a.play_pause);
    }

    #[test]
    fn shift_makes_the_seek_step_30_s_and_esc_is_free_in_window_mode() {
        let mut d = DeskInput::new(false);
        d.shift = true;
        pressed(&mut d, KeyCode::ArrowLeft, false);
        pressed(&mut d, KeyCode::Escape, false);
        let mut a = ControlBarActions::default();
        d.apply_keys(&mut a);
        assert_eq!(a.seek_delta_secs, Some(-30.0));
        assert!(!a.menu_toggle);
    }

    #[test]
    fn mouse_is_ignored_until_the_window_grabs_it() {
        let mut d = DeskInput::new(true);
        d.handle_device_event(&DeviceEvent::MouseMotion { delta: (100.0, 0.0) });
        assert_eq!(d.pos.0, 410.0);
        d.set_active(true);
        d.handle_device_event(&DeviceEvent::MouseMotion { delta: (100.0, 0.0) });
        assert_eq!(d.pos.0, 410.0 + 100.0 * MOUSE_GAIN);
        d.set_active(false);
        assert!(!d.clicking);
    }

    #[test]
    fn the_mouse_can_reach_every_corner_of_the_biggest_panel() {
        let (w, h, _, _) = crate::browser_prefs::PanelSize::Huge.dims();
        assert!(MAX_PX >= w.max(h) as f32, "a {w} x {h} panel must be fully reachable (limit {MAX_PX})");
    }
}
