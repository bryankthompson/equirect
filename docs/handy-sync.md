# Handy sync bridge

`tools/handy-sync` follows the VR player's local UDP clock and streams an existing
funscript to a firmware-4 Handy over wifi using Handy REST v3 HSP. It never edits
scripts or uploads a script URL. Python 3's standard library is sufficient.

Start with the default dry run:

```bash
tools/handy-sync --dry-run --port 17777
```

The first output line says `DRY RUN - nothing is sent, the Handy will NOT move` (or `LIVE`). A dry run never moves the
device, however well the player and bridge are talking.

Dry run prints the HTTP methods, paths, headers with credentials redacted, and
JSON bodies that would be sent. By default it listens only on `127.0.0.1` (`--bind ADDR` changes that, see below); it makes no HTTP
requests and needs no keys. The server clock is simulated in dry run.
Dry run assumes a 100-point device buffer; live setup reads the actual capacity.
Run the bridge under Linux/WSL, where the private key-file permissions can be
checked. The player can run on Windows.

The operator's first real-device run, after reviewing dry-run output:

```bash
tools/handy-sync --live --keys ~/.config/handy/keys.json --port 17777
```

The operator creates the key file privately. It must be a regular file owned by
the current user with mode **0600**, not a symlink. Its JSON object has two
nonempty string fields: `api_key` and `connection_key`. These become `X-Api-Key`
and `X-Connection-Key`. Never paste key values into a command, chat, or report.
The bridge does not create the file, extract credentials from another player,
or follow HTTP redirects. The default location is the current user's
`.config/handy/keys.json` below their home directory.

## Player messages and paths

Send UTF-8 JSON datagrams to `127.0.0.1:17777`, about four times per second and on
every change:

```json
{"v":1,"seq":1,"video":"C:\\Videos\\example.mp4","t":12345,"playing":true,"rate":1.0,
 "haptics":{"offset_ms":0,"stroke_min":0,"stroke_max":100,"invert":false}}
```

`seq` is an increasing nonnegative integer, `t` is the video position in
milliseconds, `playing` is a JSON boolean, and `rate` is positive. `haptics` is
optional (an older player sends none): see "Haptics settings from the player"
below. Late or duplicate sequence numbers are ignored.
After two seconds without accepted telemetry, the bridge stops and accepts a
new sequence, allowing a restarted player to reconnect.
The watchdog is checked between synchronous HTTP operations. An outstanding
request (eight-second timeout) or a rate-limit wait can postpone the stop;
this is not a hard real-time stop guarantee.

Network drives and shares are not mounted under `/mnt/<letter>` in WSL, so `~/.config/handy/drive-map.json` says where each is
mounted: `{"X:": "/mnt/net/share3", "\\\\server\\share4": "/mnt/net/share4"}` (drive letters and UNC share
prefixes, case-insensitive, longest prefix wins). The first output line says how many entries it loaded. A broken file stops the bridge;
no file means the plain rule below.

On Linux/WSL, drive paths such as `X:\\Videos\\example.mp4` map to
`/mnt/x/Videos/example.mp4`. Absolute POSIX paths work directly. On Windows,
native absolute drive paths stay native. UNC share mapping is not implemented;
send a mounted POSIX path instead. The script must be visible to the bridge at
that path.

**Player on Windows, bridge in WSL (tested 2026-11-17, WSL2 in NAT mode):** a UDP datagram sent from
Windows to `127.0.0.1` never reaches a WSL listener, but one sent to the WSL address (`hostname -I`) does. So run
the bridge with `--bind 0.0.0.0` (WSL sits behind NAT, so only the Windows host and WSL itself can reach it) and
give the player that address: `EQUIRECT_HANDY_ADDR=<wsl address>:17777`. The player's Desktop launcher does this
by asking `wsl.exe -d Debian hostname -I`. The WSL address can change after a WSL restart.

Lookup selects `<video stem>.funscript` beside the video, lists available
`<video stem>.<Variant>.funscript` alternatives, and accepts `--variant NAME` to
choose one explicitly. The bridge needs strictly increasing whole-millisecond times and `pos` 0 through 100, which many real scripts do not
have (about 6% of a 196-script sample: fractional times, repeated times, one point out of order). It tidies those
**in memory** (rounds, stable-sorts, a repeated time keeps its last point), prints what it changed, and never touches the file.
Values that cannot be a position (not a number, negative time, `pos` outside 0..100) and malformed or empty scripts are rejected. Missing
or invalid scripts are reported once per video selection and leave playback
idle. It does not smooth, sort, rewrite, or create scripts.

`--offset-ms 150` advances the script by 150 ms against the video; a negative
value delays it. `--variant Hard --offset-ms 150` can be added to either command.

## Haptics settings from the player

The player's Settings panel has a Haptics page (key `S`, then the Haptics tab) and sends what it holds in every message:

- `offset_ms` (-1000..1000, per video, saved with the video's settings): added to `--offset-ms`. A change re-syncs playback at once
  (stop, refill, play) instead of waiting for the next seek.
- `stroke_min`, `stroke_max` (percent, 0 to 100, at least 5 apart, one value for all videos, saved in `%APPDATA%\equirect\config\haptics.json`):
  sent to the Handy as `PUT /slider/stroke {"min","max"}` (fractions) on the first message and whenever they change.
- `invert` (all videos): the bridge flips every position to `100 - pos` in memory, also for scripts loaded later, and re-syncs.

- `auto` (optional, all videos): `{"mode": "off" | "no_script" | "always", "preset": ID, "speed": [lo, hi], "stroke": [lo, hi], "min_stroke": N,
  "pattern_s": [lo, hi], "pause_s": [lo, hi], "seed": N}`. `seed` (optional, 1 to 4294967295; 0 or missing: a new one for each video) pins the
  random seed, so the same seed, style and sliders play the same script; the bridge logs the seed it used ("auto mode: ..., seed N"). The style
  `random` chains the other styles (one to four minutes each, each with its own speed, stroke window, pattern time and pauses, inside the
  sliders' limits). When the mode says so, the bridge makes the script itself (`tools/handy_auto.py`, presets in
  `tools/auto_presets.json`, see `docs/auto-mode.md`) for a video without a funscript (`no_script`) or instead of it (`always`); changing it
  rebuilds the script and re-syncs. An invalid block (unknown preset, ranges out of order) turns auto mode off and says so in the log.

The player holds a changed value back for 300 ms so dragging a slider does not make the bridge re-sync on every frame. A `haptics` block that is
missing or not valid is ignored and the last good one stays. The Handy has no speed setting in its API (the firmware caps it), so there is no speed control.

## Timing and buffering

Live startup takes 30 `/servertime` samples and uses the median midpoint clock
offset to limit the effect of latency outliers. `/hstp/clocksync?s=true`
synchronizes the device clock. All HSP calls use the supplied v3 specification.
Server response bodies and exception details are never copied into errors.

The bridge sends at most 100 original points per add call. It aims for ten
seconds of script look-ahead, refilling below five seconds, while respecting
the device's reported buffer capacity. A seek includes the original predecessor
point; sparse scripts include the next point beyond the look-ahead boundary.
This preserves interpolation without inventing new points. Extremely dense
scripts or cloud throttling can still exhaust the buffer. Playback does not
pause the script clock on starvation, so subsequent points keep following the
video clock. If the buffer has already expired, refill flushes the expired
points and starts at the current script position rather than replaying them.

A new file stops playback and reloads. A position error greater than 1000 ms
against the previous expected sample is a seek: stop, flush, refill, and restart
at the new time. This threshold exceeds ordinary 250 ms telemetry jitter.
Paused-position changes over 100 ms also rebuild the buffer. Pause/resume and
rate changes update playback explicitly. Drift correction uses a 100 ms
threshold, at least two seconds between corrections, and a 0.5 filter; a
ten-second heartbeat also sends a correction. These values avoid reacting to
every packet while preventing small errors from accumulating indefinitely.

Rate-limit responses honor `X-RateLimit-Remaining`, `X-RateLimit-Limit`, and
`X-RateLimit-Reset` (seconds until reset), plus 429 backoff. Failed or ambiguous
mutating requests are not blindly replayed. Only 429 rejections are retried,
up to three times, waiting at least the reported reset/retry delay. A timeout, shutdown, or transport
failure attempts a stop. A cloud outage can prevent that stop from reaching the
device; the operator can stop the device directly.

## Tests and verification boundary

```bash
tools/handy-sync selftest
tools/handy-sync selftest --offline
```

The full suite uses a standard-library HTTP server bound to loopback and fake
credentials created in temporary files. It must fail if the environment cannot
create the required sockets. The explicitly requested offline suite covers
pure logic and simulated transports without claiming loopback integration.

Development in the restricted lane cannot create sockets, so full mock-server
and UDP integration must be run in the lead's environment. Tested since: the full mock suite (24 tests, run by the lead outside the lane) and Windows-player-to-WSL-bridge
UDP delivery in dry run (the real player sent its position, the bridge printed the HSP call sequence). No Handy
cloud call, real credential, real device motion, or headset timing has been tested. The first live run belongs to the author.

## Started by the launcher

A launcher can start `tools/handy-bridge` in its own minimized WSL window before the player. It runs the bridge **live**
(`--live --keys ~/.config/handy/keys.json --port 17777 --bind 0.0.0.0`), only one at a time (it checks that UDP port 17777 is free), and also
writes `~/.local/state/handy-sync/bridge.log`. Closing the window stops the Handy following the player.

A Handy error is shown with the call and the Handy's own code and message, for example
`Handy returned a device error on PUT /hsp/stop: 100004 Illegal state ...`, and the bridge starts over after 3 seconds instead of ending.
A stop before HSP is set up (error 100004) is not an error: nothing is playing yet.
If the Handy is not connected when the bridge starts (error 1001: switched off, asleep, off wifi), the bridge says so once and tries again every
3 seconds until it is, instead of ending (found 2026-11-19: it used to exit at the clock sync and the Handy then never followed). A real startup
problem, such as a refused key, still ends it with the reason in the window and the log.
