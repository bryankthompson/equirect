# Vive fork of equirect (base upstream v1.3.4)

Upstream: https://github.com/greggman/equirect (MIT, Gregg Tavares). This fork adds what a Vive Pro 2 on SteamVR
needed, plus a small library and Handy hook. It sits on upstream `0525bb4` (v1.3.4); `git diff 0525bb4` shows all of it.

## What differs from upstream
- **SteamVR / Vive Pro 2**: video is drawn in the scene (SteamVR overlay layers hid the panels; `EQUIRECT_USE_LAYER=1`
  brings layers back), off-axis projection terms so asymmetric eyes fuse, Vive wand and Index bindings (`input.rs`),
  audio on the headset device (`EQUIRECT_AUDIO_DEVICE`, default `vive`).
- **Center** (`vr.rs`): the panels, the screen and the pointer rays live in a frame with its own origin and facing, so they can be moved.
  Centring puts that origin on the floor under your head and turns the frame to face where you look. It happens at start, when
  SteamVR's centre moves ("Reset seated position": we draw in the stage, which that does not move, so the player watches the
  LOCAL space inside it and follows; `player.log` says "SteamVR centre moved" and logs every reference-space change event), on the
  **Center** button of the Screen page (upright, yaw only) and on a grip squeeze (locked to the full head direction, as before, now
  moving the origin too). Heights stay as they were: the Screen page's Height still sets them.
- **Audio controls** (`volume.rs`, `audio.rs`): a click on the speaker on the control bar opens a volume slider in the seek bar's place for 4 s
  (moving it unmutes); a double click mutes and unmutes (red and crossed out when muted). Settings has an Audio tab with a volume slider and a Muted button. One setting for all videos (`volume.json`); the audio callback
  multiplies by the gain (the slider squared, so it feels even), heard at once. The control bar has 10 icons.
- **Heat map** (`heatmap.rs`, `ui/ui.rs::slider_sized_heat`): a strip of colour above the seek bar shows how fast the strokes of the
  `.funscript` beside the video are (still, blue, green, yellow, red at the Handy's top speed), so you can see where the action is
  and jump there. The plain `<video>.funscript` is used, else the first variant `<video>.<name>.funscript`. Read-only; a script that
  cannot be read is logged in player.log. Not shown for URLs or videos with no script.
- **VR marker in the library** (`ui/browser.rs`): a cyan "VR" pill on the thumbnail (and "VR" in the list) when the file name says 180, 360,
  SBS, top-bottom and so on: the same guess the player makes when it opens the file.
- **Mode guessing** (`video_meta.rs`): projection and stereo layout from the file name and frame size; a Reset button.
- **Favorites** (`library.rs`): heart button, Favorites list in `%APPDATA%\equirect\config\library.json`.
  The browser marks videos that have a `.funscript` beside them.
- **Library browser: pins, thumbnails, bigger panel** (`browser_prefs.rs`, `thumbs.rs`, `ui/browser.rs`): the browser has a Pins row (one click to a folder;
  "Pin this folder" / "Unpin this folder" for the one shown), a View row (List, or thumbnails Small / Medium / Large) and a Panel row
  (Normal 1.6 x 1.2 m, Large 2.4 x 1.8 m, Huge 3.2 x 2.4 m; Large is the default). Thumbnails are the Windows shell's (the ones Explorer shows;
  local and network files, cached by Windows), made on 3 background threads into one 3840 x 2592 texture of 120 slots (least recently shown
  is reused); a stereo video shows one eye (from the name `_LR`/`_TB`..., or a 2:1 picture). A file Windows has no picture for says "no preview".
  All saved in `%APPDATA%\equirect\config\browser.json`. A pin can list `"exclude"` folders that are left out of every listing (edit the file by hand;
  restart the player). About 7 pins fit in a Large panel's row.
- **Resume where you stopped** (`resume.rs`, `video_meta.rs`, `app.rs`): the playing position is saved in the video's own settings file
  (`video-<hash>.json`, field `position_secs`) every 5 s while playing, when you switch to another video, and when the player closes.
  Reopening the video starts there and the control bar shows "Resumed at 12:34" for 6 s (it is brought up for that and goes away
  again if it was hidden). Stopped in the first 30 s or the last 60 s counts as start over, and a position past the end of a shorter
  file is ignored. `--start` / `-t` beats a resume; picking the video that is already playing starts it over; Previous/Next resume too.
  A crash loses at most 5 s. To forget one, drag the slider to the start and leave, or delete the `position_secs` line.
- **Notes from the NFO** (`nfo.rs`, `ui/control_bar.rs`): when `<video name>.nfo` sits beside a local video, its title (if it differs
  from the file name), studio and plot show above the control buttons (plot up to 4 lines). The bar grows upward only then, so the
  buttons stay put. Read-only; an unreadable NFO is logged in player.log. Not shown for URLs.
- **Screen page and Curved mode** (`screen.rs`, `video_renderer.rs::build_screen`, `ui/settings.rs`): Settings has Video, Screen, Haptics, Auto and
  Audio tabs. Screen: curvature (0 flat .. 100% a cylinder around you), distance (1..8 m), height (0..3 m) and tilt (-90..+90 degrees, turns the
  screen about your head: + for watching lying on your back), the same for every video (`screen.json`); Zoom stays per video on the Video page.
  Curvature applies in Curved mode only; distance, height and tilt move the Flat, Curved and 3D screens (180 and 360 surround you).
  Before 2026-11-19 the Curved button did nothing in the scene renderer (the default): flat, curved and 3D all drew the same flat quad.
  The XR-layer fallback (`EQUIRECT_USE_LAYER=1`) ignores these settings.
- **Haptics page** (`haptics.rs`, `ui/settings.rs`): Settings (`S`) has Video, Screen, Haptics, Auto and Audio tabs. Haptics: offset (-1000..1000 ms, saved
  per video), stroke min/max (percent) and invert (both for all videos, `haptics.json`). Sent to the bridge in every position message;
  details in `docs/handy-sync.md`. Its **Auto** tab turns on auto mode (styles including **Random**, which keeps changing style inside the sliders' limits, and a **seed** to keep or renew): the bridge makes the strokes itself from patterns mined from real
  funscripts (style presets, speed, stroke, shortest, pattern time, pause): `docs/auto-mode.md`.
- **Handy position sender** (`telemetry.rs`): UDP JSON about 4 per second to `EQUIRECT_HANDY_ADDR` (default
  `127.0.0.1:17777`, `off` disables). A bridge in WSL needs the WSL address, not 127.0.0.1 (`hostname -I` in WSL; start the bridge with `--bind 0.0.0.0`).
- **Desktop window** (`--local`): the control bar sits mid-window (`local.rs`; upstream's VR default of y=1.4 would put it above the view). Do not start
  a second player while the headset one runs: a second launch hands its file to the running one (single-instance pipe) and replaces what is playing.
- **Mouse and keyboard in the headset** (`desk_input.rs`): click the window once, it grabs the cursor and a dot drives the
  visible panel; Alt+Tab releases. Keys: Space/K pause, Left/Right seek 5 s (Shift 30 s), Home start, N/P or PgDn/PgUp
  next/previous, F/H favorite, B browser, S settings, L loop, ] speed, M/Tab/Esc panel. Bigger playhead grab zone (`ui/ui.rs`).
- **player.log** (`diag.rs`): slow opens, folder listings and frames, in `%APPDATA%\equirect\config\`.

## Building
Build and test as upstream does (`cargo build --release`, `cargo test --release`; Windows, MSVC build tools). Everything here was built and
tried on Windows 11 with SteamVR and a Vive Pro 2.

## Handy bridge
`tools/handy-sync` (Python, `tools/handy_sync.py`, doc `docs/handy-sync.md`) reads the UDP position messages and streams the
`.funscript` beside the video to a Handy over wifi (HSP). Default is a dry run; `tools/handy-sync selftest` runs its tests.

## Not tried yet in the headset
- Center, the Audio page and mute, the heat map and the VR marker (built and unit-tested 2026-11-20; the control bar, the notes, the heat map, mute,
  every Settings tab and the library's VR and script marks were checked in a window; Center itself needs the headset): does SteamVR's centre now
  move the screen (if not, `player.log` shows which space it moved), and does a grip squeeze or the Center button put the screen where you look?
- Resume where you stopped (built, unit-tested, checked in a window with a 4-minute clip 2026-11-19: autosave, crash, relaunch and clean close):
  is the "Resumed at" note on the control bar enough of a hint, and is 6 s about right?
- The bigger library browser with thumbnails (built, unit-tested, clicked through in a window 2026-11-19, thumbnails checked on local and network
  files): is Large the right panel size, and are the three tile sizes right to pick from with a controller?
- The Screen page and the real Curved mode (built, unit-tested and checked in a window 2026-11-19): is 100% curvature comfortable, do height and
  tilt sit right for how the author watches (seated, lying down), and is the default 2.5 m distance right?
- The NFO notes above the controls (built, unit-tested and checked in a window, 2026-11-18).
- The Auto page and auto mode (built, unit-tested, deployed and clicked through in a window 2026-11-19; **not tried in the headset**):
  does the page reach with a controller, and does the result feel like a person did it?
- The Haptics page (built, unit-tested, clicked through in a window 2026-11-19): does the Handy really follow a changed offset, stroke
  range and invert, and are the sliders easy to hit with a controller?

## Tried in the headset
Favorites, the bigger playhead grab zone and the mouse and keyboard all felt good (the author, 2026-11-18). The mouse speed
(`MOUSE_GAIN` 1.5) was a guess and stays unless he says otherwise.
