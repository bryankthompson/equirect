#!/usr/bin/env python3
"""Follow a local funscript from the VR player's loopback position feed."""
from __future__ import annotations

import argparse
from bisect import bisect_right
from email.utils import parsedate_to_datetime
import json
import math
import os
from pathlib import Path
import re
import socket
import stat
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))  # handy_auto.py sits beside this file (run with python -I there is no script dir on the path)
import handy_auto  # noqa: E402

API_ROOT = "https://www.handyfeeling.com/api/handy-rest/v3"
DEFAULT_KEYS = Path.home() / ".config/handy/keys.json"
LOOKAHEAD_MS = 10_000
REFILL_MS = 5_000
SEEK_MS = 1_000
DRIFT_MS = 100
DRIFT_INTERVAL = 2.0
DRIFT_HEARTBEAT = 10.0
UDP_TIMEOUT = 2.0
RETRY_SECONDS = 3.0
AUTO_HORIZON_MS = 4 * 3600 * 1000  # how far ahead an auto-mode script is made (a video is never longer; it takes about 0.1 s)
MAX_POINTS = 100


class HandyError(Exception):
    """Only locally authored, credential-free messages may be displayed. `code` is the Handy's own error code, if it gave one."""

    def __init__(self, message="", code=None):
        super().__init__(message)
        self.code = code


# "Illegal state. Mode specific setup required first.": HSP was never set up, so nothing is playing yet.
NOT_SET_UP = 100004
# "Device not connected": the Handy is off, asleep or out of wifi range. It can come back, so the bridge waits for it.
DEVICE_NOT_CONNECTED = 1001


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def validate_actions(data, notes=None):
    """Funscript actions as strictly increasing integer (at ms, pos 0..100).

    Real scripts often carry fractional milliseconds, a repeated time or one point out of order; the Handy takes none of those, so they
    are tidied in memory (rounded, stable-sorted, a repeated time keeps its last point) and described in `notes`. The file is never
    changed and nothing is smoothed. A value that cannot be a position (not a number, negative time, pos outside 0..100) is an error."""
    if not isinstance(data, dict) or not isinstance(data.get("actions"), list):
        raise HandyError("bad funscript: expected actions array")
    raw = []
    rounded = 0
    for item in data["actions"]:
        if not isinstance(item, dict):
            raise HandyError("bad funscript: action must be an object")
        at, pos = item.get("at"), item.get("pos")
        if (isinstance(at, bool) or isinstance(pos, bool) or not isinstance(at, (int, float))
                or not isinstance(pos, (int, float)) or not math.isfinite(at) or not math.isfinite(pos)):
            raise HandyError("bad funscript: at and pos must be numbers")
        at_i, pos_i = round(at), round(pos)
        if at_i < 0 or not 0 <= pos_i <= 100:
            raise HandyError("bad funscript: at must be 0 ms or more and pos must be 0..100")
        rounded += (at_i != at) or (pos_i != pos)
        raw.append((at_i, pos_i))
    if not raw:
        raise HandyError("bad funscript: actions array is empty")
    out_of_order = sum(1 for i in range(1, len(raw)) if raw[i][0] < raw[i - 1][0])
    ordered = sorted(raw, key=lambda a: a[0])  # stable
    actions = []
    for at_i, pos_i in ordered:
        if actions and actions[-1]["at"] == at_i:
            actions[-1]["pos"] = pos_i
        else:
            actions.append({"at": at_i, "pos": pos_i})
    dropped = len(raw) - len(actions)
    if notes is not None:
        if rounded:
            notes.append(f"{rounded} values rounded to whole numbers")
        if out_of_order:
            notes.append(f"{out_of_order} points were out of order and got sorted")
        if dropped:
            notes.append(f"{dropped} points shared a time with another and were merged")
    return actions


def load_script(path, notes=None):
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, ValueError):
        raise HandyError("could not read funscript JSON") from None
    return validate_actions(data, notes)


DRIVE_MAP_FILE = Path.home() / ".config" / "handy" / "drive-map.json"
_DRIVE_MAP = {}


def load_drive_map(path=DRIVE_MAP_FILE):
    """Where the player's Windows drives and shares are mounted in WSL: {"X:": "/mnt/net/share", "\\\\host\\share": "/mnt/..."}.
    No file means no map (plain /mnt/<letter> then applies); a broken file is an error, never ignored."""
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    except (OSError, UnicodeError, ValueError):
        raise HandyError("drive-map.json is unreadable (expected a JSON object of prefix: WSL folder)") from None
    if not isinstance(raw, dict) or not all(isinstance(k, str) and isinstance(v, str) and v.startswith("/")
                                            for k, v in raw.items()):
        raise HandyError("drive-map.json must map a drive or share prefix to an absolute WSL folder")
    return raw


def set_drive_map(mapping):
    global _DRIVE_MAP
    _DRIVE_MAP = {k.replace("/", "\\").rstrip("\\").lower(): v for k, v in mapping.items()}


def video_path(value):
    if os.name != "nt":
        norm = value.replace("/", "\\")
        low = norm.lower()
        best = None
        for prefix, target in _DRIVE_MAP.items():
            if (low == prefix or low.startswith(prefix + "\\")) and (best is None or len(prefix) > len(best[0])):
                best = (prefix, target)
        if best:
            return Path(best[1]) / norm[len(best[0]):].lstrip("\\").replace("\\", "/")
        match = re.match(r"^([A-Za-z]):[\\/](.*)$", value)
        if match:
            return Path("/mnt") / match.group(1).lower() / match.group(2).replace("\\", "/")
    return Path(value)


def script_candidates(video, variant=None):
    if variant and (variant in (".", "..") or any(c in variant for c in "/\\\x00")):
        raise HandyError("variant must be a sidecar variant name")
    path = video_path(video)
    stem = path.stem
    # Do not treat metacharacters in video names as glob patterns.
    try:
        variants = sorted(p for p in path.parent.iterdir()
                          if p.name.startswith(stem + ".") and p.name.endswith(".funscript")
                          and p.name != stem + ".funscript" and p.is_file())
    except OSError:
        variants = []
    name = stem + ("." + variant if variant else "") + ".funscript"
    return path.parent / name, variants


def read_keys(path):
    fd = None
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | getattr(os, "O_NOFOLLOW", 0))
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o600):
            raise HandyError("key file must be an owned regular file with mode 0600")
        with os.fdopen(fd, "r", encoding="utf-8") as handle:
            fd = None
            data = json.load(handle)
    except HandyError:
        raise
    except (OSError, UnicodeError, ValueError):
        raise HandyError("could not read key file") from None
    finally:
        if fd is not None:
            os.close(fd)
    if not isinstance(data, dict):
        raise HandyError("key file must contain api_key and connection_key")
    api_key = data.get("api_key", "")
    token = data.get("bearer_token", "")
    connection = data.get("connection_key", "")
    for key in (api_key, token, connection):
        if not isinstance(key, str) or any(ord(c) < 33 or ord(c) > 126 for c in key):
            raise HandyError("key file contains an invalid header value")
    if not (api_key or token) or not connection:
        raise HandyError("key file needs api_key or bearer_token, and connection_key")
    return api_key, token, connection


def redact(message, secrets=()):
    value = str(message)
    for secret in sorted((s for s in secrets if s), key=len, reverse=True):
        value = value.replace(secret, "[redacted]")
    return value


class HspClient:
    def __init__(self, api_key="", connection_key="", bearer_token="", *, dry_run=False,
                 base_url=API_ROOT, opener=None, clock=time.time, sleep=time.sleep, emit=print):
        parsed = urllib.parse.urlsplit(base_url)
        local = (parsed.scheme == "http" and parsed.hostname in ("127.0.0.1", "::1")
                 and not parsed.username and not parsed.password and not parsed.query and not parsed.fragment)
        if base_url != API_ROOT and not local:
            raise HandyError("refusing non-production HSP server")
        self.base_url = base_url.rstrip("/")
        self.api_key, self.connection_key, self.bearer_token = api_key, connection_key, bearer_token
        self.secrets = (api_key, connection_key, bearer_token)
        self.dry_run = dry_run
        self.clock, self.sleep, self.emit = clock, sleep, emit
        self.opener = opener or urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        self.offset_ms = 0
        self.max_points = 100  # Explicit dry-run assumption; replaced by live setup.
        self.tail_index = 0
        self.rate_limit = self.rate_remaining = self.rate_reset = None
        self.next_request = 0.0
        self.sent_at = 0.0

    def log(self, message):
        self.emit(redact(message, self.secrets))

    def _rate_headers(self, headers):
        def number(name):
            try:
                value = float(headers.get(name))
                return value if math.isfinite(value) and value >= 0 else None
            except (ValueError, TypeError):
                return None
        self.rate_limit = number("X-RateLimit-Limit")
        self.rate_remaining = number("X-RateLimit-Remaining")
        self.rate_reset = number("X-RateLimit-Reset")
        if self.rate_remaining == 0:
            self.next_request = max(self.next_request, self.clock() + (self.rate_reset or 1))

    def _request(self, method, path, body=None, auth=True):
        # Only explicit 429 rejections are retried. Other failures may have
        # applied a mutation already. Bodies containing clocks are made at send time.
        for attempt in range(4):
            wait = max(0, self.next_request - self.clock())
            if wait and not self.dry_run:
                self.log(f"rate limit: waiting {wait:g}s")
                self.sleep(wait)
            self.sent_at = self.clock()
            actual_body = body() if callable(body) else body
            payload = None if actual_body is None else json.dumps(actual_body, separators=(",", ":")).encode()
            headers = {"Accept": "application/json"}
            if payload is not None:
                headers["Content-Type"] = "application/json"
            if auth:
                if self.bearer_token and not self.api_key:
                    headers["Authorization"] = "Bearer " + self.bearer_token
                else:
                    headers["X-Api-Key"] = self.api_key
                headers["X-Connection-Key"] = self.connection_key
            if self.dry_run:
                safe = {k: "[redacted]" if k in ("X-Api-Key", "X-Connection-Key", "Authorization") else v
                        for k, v in headers.items()}
                self.log(f"{method} {path} headers={json.dumps(safe, sort_keys=True)} body={json.dumps(actual_body)}")
                if path == "/servertime":
                    return {"server_time": round(self.clock() * 1000)}, {}
                return {"result": {"max_points": self.max_points}}, {}
            req = urllib.request.Request(self.base_url + path, data=payload, headers=headers, method=method)
            try:
                with self.opener.open(req, timeout=8) as response:
                    response_headers = response.headers
                    raw = response.read(1024 * 1024 + 1)
            except urllib.error.HTTPError as exc:
                response_headers, code = exc.headers, exc.code
                exc.close()
                self._rate_headers(response_headers)
                if code != 429:
                    raise HandyError(f"Handy request failed with HTTP {code}") from None
                if attempt == 3:
                    raise HandyError("Handy rate limit retry limit reached") from None
                retry = response_headers.get("Retry-After", "")
                try:
                    delay = float(retry)
                except ValueError:
                    try:
                        delay = parsedate_to_datetime(retry).timestamp() - self.clock()
                    except (ValueError, TypeError, OverflowError):
                        delay = 0
                if not math.isfinite(delay):
                    delay = 0
                self.next_request = max(self.next_request, self.clock() + max(2 ** attempt, delay, self.rate_reset or 0))
                continue
            except Exception:
                raise HandyError("Handy request failed") from None
            self._rate_headers(response_headers)
            try:
                if len(raw) > 1024 * 1024:
                    raise ValueError
                data = json.loads(raw)
            except (ValueError, UnicodeError):
                raise HandyError("Handy returned invalid JSON") from None
            if not isinstance(data, dict):
                raise HandyError("Handy returned an invalid response")
            if "error" in data:
                # Say which call failed and what the Handy answered (code, name, message: never a key; main() redacts anyway).
                err = data["error"]
                said = ""
                if isinstance(err, dict):
                    said = " " + " ".join(str(err[k]) for k in ("code", "name", "message") if err.get(k) is not None)
                raise HandyError(f"Handy returned a device error on {method} {path}:" + redact(said, self.secrets)[:200],
                                 code=err.get("code") if isinstance(err, dict) else None)
            return data, response_headers
        raise HandyError("Handy request failed")

    def sync_server_time(self, samples=30):
        offsets = []
        for _ in range(samples):
            data, _ = self._request("GET", "/servertime", auth=False)
            after = self.clock()
            server = data.get("server_time")
            if type(server) is not int:
                raise HandyError("invalid server-time response")
            offsets.append(server - (self.sent_at + after) * 500)
        self.offset_ms = statistics.median(offsets)
        return self.offset_ms

    def server_now(self):
        return round(self.clock() * 1000 + self.offset_ms)

    def clock_sync(self):
        self._request("GET", "/hstp/clocksync?s=true")

    def setup(self):
        data, _ = self._request("PUT", "/hsp/setup", {})
        result = data.get("result")
        capacity = result.get("max_points") if isinstance(result, dict) else None
        if (type(capacity) not in (int, float) or not math.isfinite(capacity)
                or capacity < 2 or capacity != int(capacity)):
            raise HandyError("HSP setup did not report a usable buffer capacity")
        self.max_points = int(capacity)
        self.tail_index = 0

    def stop(self):
        try:
            self._request("PUT", "/hsp/stop")
        except HandyError as exc:
            if exc.code != NOT_SET_UP:  # not set up yet = nothing playing = already stopped
                raise

    def pause(self):
        self._request("PUT", "/hsp/pause")

    def resume(self, pick_up=False):
        self._request("PUT", "/hsp/resume", {"pick_up": pick_up})

    def set_stroke(self, low, high):
        """The part of the slider travel the Handy may use, as fractions 0..1 (the player's stroke range setting)."""
        self._request("PUT", "/slider/stroke", {"min": round(low, 3), "max": round(high, 3)})

    def flush(self):
        self._request("PUT", "/hsp/flush")
        self.tail_index = 0

    def add(self, points, *, flush=False, stream_start=None):
        stream_start = self.tail_index if stream_start is None else stream_start
        for start in range(0, len(points), MAX_POINTS):
            chunk = points[start:start + MAX_POINTS]
            tail = stream_start + start + len(chunk)
            body = {"points": [{"t": p["at"], "x": p["pos"]} for p in chunk], "tail_point_stream_index": tail}
            if flush and start == 0:
                body["flush"] = True
            self._request("PUT", "/hsp/add", body)
            self.tail_index = tail

    def play(self, start_time, rate=1.0):
        self._request("PUT", "/hsp/play", lambda: {
            "start_time": int(start_time() if callable(start_time) else start_time),
            "server_time": self.server_now(), "playback_rate": rate,
            "pause_on_starving": False, "loop": False})

    def synctime(self, current_time):
        self._request("PUT", "/hsp/synctime", lambda: {
            "current_time": int(current_time() if callable(current_time) else current_time),
            "server_time": self.server_now(), "filter": 0.5})


class DryClient(HspClient):
    def __init__(self, emit=print):
        super().__init__(dry_run=True, emit=emit)


class Bridge:
    def __init__(self, client, *, variant=None, offset_ms=0, emit=print, monotonic=time.monotonic):
        self.client, self.variant, self.offset_ms = client, variant, offset_ms
        self.base_offset_ms = offset_ms  # --offset-ms; the player's haptics offset is added on top
        self.haptics, self.invert = None, False  # last haptics settings applied (see _apply_haptics)
        self.auto = self.auto_data = None        # auto-mode settings (validated) and the mined presets, loaded when first needed
        self.auto_failed = False
        self.auto_seed = 0
        self.script_path = None                  # the funscript beside the loaded video, if any
        self.generated_now = False               # the current actions come from auto mode
        self.emit = lambda message: emit(redact(message, client.secrets))
        self.monotonic = monotonic
        self.video = self.loaded_path = self.last = None
        self.actions, self.times = [], []
        self.sent = self.buffered_start = 0
        self.playing = self.started = False
        self.rate = self.active_rate = 1.0
        self.anchor_t = self.anchor_mono = self.last_sync = 0.0
        self.retry_after = 0.0

    def _current_t(self, now=None):
        if self.last is None:
            return self.anchor_t
        now = self.monotonic() if now is None else now
        return self.last["t"] + (now - self.last["received_mono"]) * 1000 * (self.last["rate"] if self.last["playing"] else 0)

    def _script_t(self, player_t):
        return round(player_t + self.offset_ms)

    def _position(self):
        return self._script_t(self._current_t())

    def _feed(self, t, *, reset=False):
        script_t = self._script_t(t)
        if (self.playing and self.sent and self.sent < len(self.actions)
                and self.times[self.sent - 1] < script_t - DRIFT_MS):
            # If capacity/latency could not cover the gap, discard expired
            # points and refill at the live clock instead of replaying a backlog.
            reset = True
        if reset:
            self.client.flush()
            if self.last is not None:
                script_t = self._position()
            self.sent = max(0, bisect_right(self.times, script_t) - 1)
            self.buffered_start = self.sent
        if script_t < 0 or self.sent == len(self.actions):
            return
        if self.sent > self.buffered_start and self.times[self.sent - 1] - script_t > REFILL_MS:
            return
        # Retain the predecessor, using the slower of estimated device and
        # player clocks plus a 100ms jitter margin before evicting played points.
        device_t = self.anchor_t + (self.monotonic() - self.anchor_mono) * 1000 * self.active_rate
        safe_time = min(script_t, self._script_t(device_t)) - DRIFT_MS if self.playing else script_t
        keep = max(self.buffered_start, bisect_right(self.times, safe_time) - 1)
        keep = min(keep, self.sent)
        available = max(0, self.client.max_points - (self.sent - keep))
        end = min(len(self.actions), bisect_right(self.times, script_t + LOOKAHEAD_MS) + 1,
                  self.sent + available)
        if end <= self.sent:
            return
        self.client.add(self.actions[self.sent:end], stream_start=self.sent)
        self.sent = end
        self.buffered_start = max(self.buffered_start, end - self.client.max_points)

    def load_video(self, video):
        self.client.stop()
        self.video = video
        self.playing = self.started = False
        self.actions, self.times = [], []
        self.sent = self.buffered_start = 0
        self.loaded_path = None
        path, variants = script_candidates(video, self.variant)
        if variants:
            prefix = video_path(video).stem + "."
            names = [p.name[len(prefix):-len(".funscript")] for p in variants]
            self.emit("Available variants: " + ", ".join(names))
        try:
            self.script_path = path if path.is_file() else None
        except OSError as exc:  # e.g. a Windows folder WSL cannot read (Input/output error): treat as no script, say so
            self.script_path = None
            self.emit(f"cannot look for a funscript beside the video ({exc.strerror or type(exc).__name__}: {path.parent}); "
                      "treating it as having none")
        self.auto_seed = int.from_bytes(os.urandom(4), "big")  # a new video gets its own run of patterns
        if not self._load_actions():
            return False
        self.client.setup()
        return True

    def _want_auto(self):
        mode = self.auto["mode"] if self.auto else "off"
        return mode == "always" or (mode == "no_script" and self.script_path is None)

    def _load_actions(self):
        """Make self.actions for the loaded video: its funscript, or an auto-mode script. False (and idle) when there is neither."""
        self.actions, self.times = [], []
        self.generated_now = self._want_auto()
        if self.generated_now:
            seed = self.auto.get("seed") or self.auto_seed  # a seed the player pinned, else this video's own
            self.actions = handy_auto.generate(self.auto_data, self.auto, seed, AUTO_HORIZON_MS)
            what = (f"auto mode: {self.auto['preset']} ({'no funscript' if self.script_path is None else 'instead of the funscript'}), "
                    f"seed {seed}")
        elif self.script_path is not None:
            notes = []
            try:
                self.actions = load_script(self.script_path, notes)
            except HandyError as exc:
                self.emit(str(exc) + "; idle")
                return False
            if notes:
                self.emit("script tidied in memory (the file is untouched): " + "; ".join(notes))
            what = f"script loaded: {self.script_path.name}"
        else:
            self.emit("No funscript beside video for the selected variant; idle")
            return False
        if self.invert:
            self.actions = flip_positions(self.actions)
        self.times = [a["at"] for a in self.actions]
        self.loaded_path = self.script_path
        self.emit(what if self.generated_now else f"{what} ({len(self.actions)} points, {self.times[-1] / 1000:.0f}s)")
        return True

    def _clean_auto(self, raw):
        """The auto-mode settings from the player, validated; None (auto off) when off, missing or not usable."""
        if not isinstance(raw, dict) or raw.get("mode") not in handy_auto.MODES or raw["mode"] == "off":
            return None
        if self.auto_data is None and not self.auto_failed:
            try:
                self.auto_data = handy_auto.load_presets()
            except RuntimeError as exc:
                self.auto_failed = True
                self.emit(str(exc) + "; auto mode is off")
        if self.auto_data is None:
            return None
        params = handy_auto.validate_params(raw, self.auto_data)
        if params is None:
            self.emit("auto mode settings are not valid; auto mode is off")
            return None
        seed = raw.get("seed")
        pinned = {"seed": seed} if type(seed) is int and 0 < seed < 2 ** 32 else {}  # 0 or none: a new seed for each video
        return {"mode": raw["mode"], **params, **pinned}

    def _play(self):
        self.client.play(self._position, self.last["rate"])
        self.playing = self.started = True
        self.active_rate = self.last["rate"]
        self.anchor_t = self._current_t()
        self.anchor_mono = self.last_sync = self.monotonic()

    def _apply_haptics(self, h):
        """Apply the player's haptics settings when they changed. True when playback must be re-synced
        (the offset moved or the script was flipped), so the Handy follows at once, not at the next seek."""
        if h is None or h == self.haptics:
            return False
        old = self.haptics
        if old is None or (old["stroke_min"], old["stroke_max"]) != (h["stroke_min"], h["stroke_max"]):
            self.client.set_stroke(h["stroke_min"] / 100, h["stroke_max"] / 100)  # first: if it fails, nothing else changed
        self.haptics = h
        self.offset_ms = self.base_offset_ms + h["offset_ms"]
        resync = old is not None and old["offset_ms"] != h["offset_ms"]
        auto = self._clean_auto(h.get("auto"))
        auto_changed, self.auto = auto != self.auto, auto
        invert_changed, self.invert = h["invert"] != self.invert, h["invert"]
        if auto_changed and self.video is not None:
            had_actions = bool(self.actions)
            if self._load_actions():  # builds with the new invert already applied
                if not had_actions:
                    self.client.setup()
            elif self.started:
                self.client.stop()
                self.playing = self.started = False
            resync = True
        elif invert_changed:
            self.actions = flip_positions(self.actions)
            resync = True
        self.emit(f"haptics: offset {h['offset_ms']:+d} ms, stroke {h['stroke_min']}-{h['stroke_max']}%, "
                  f"invert {'on' if h['invert'] else 'off'}"
                  + (f", auto {self.auto['mode']} {self.auto['preset']}" if self.auto else ", auto off"))
        return resync and bool(self.actions)

    def process_safely(self, msg):
        """process(), but a Handy error is reported and the bridge starts over (after a short pause) instead of ending."""
        if self.monotonic() < self.retry_after:
            return False
        try:
            return self.process(msg)
        except HandyError as exc:
            self.emit(f"Handy error, starting over in {RETRY_SECONDS:g} s: {exc}")
            self.video = self.loaded_path = self.last = None
            self.actions, self.times = [], []
            self.sent = self.buffered_start = 0
            self.playing = self.started = False
            self.retry_after = self.monotonic() + RETRY_SECONDS
            return False

    def process(self, msg):
        now = self.monotonic()
        if self.last is not None and msg["seq"] <= self.last["seq"]:
            return False
        old = self.last
        resync = self._apply_haptics(msg.get("haptics"))
        changed_video = msg["video"] != self.video
        expected = self._current_t(now) if old else msg["t"]
        seek = not changed_video and (abs(msg["t"] - expected) > SEEK_MS or resync)
        paused_jump = bool(old and not old["playing"] and abs(msg["t"] - old["t"]) > DRIFT_MS)
        was_playing = self.playing
        self.last = {**msg, "received_mono": now}
        if old is None:
            self.emit("player connected: " + Path(msg["video"].replace("\\", "/")).name)
        if old is None or msg["playing"] != old["playing"]:
            self.emit(("playing" if msg["playing"] else "paused") + f" at {msg['t'] / 1000:.1f}s"
                      + (f", speed {msg['rate']:g}" if msg["rate"] != 1 else ""))
        if seek:
            self.emit(f"seek to {msg['t'] / 1000:.1f}s")
        if changed_video:
            self.load_video(msg["video"])
            was_playing = False
        if not self.actions:
            return True
        if self._position() < 0:
            if changed_video:
                self.client.flush()
            if self.started:
                self.client.stop()
            self.playing = self.started = False
            self.sent = self.buffered_start = 0
            return True
        if changed_video or seek or paused_jump:
            if not changed_video:
                self.client.stop()
            self.playing = self.started = False
            self._feed(self._current_t(), reset=True)
            if msg["playing"]:
                self._play()
        elif not msg["playing"]:
            if was_playing:
                self.client.pause()
                self.anchor_t = msg["t"]
                self.anchor_mono = self.monotonic()
            self.playing = False
        elif not was_playing:
            self._feed(self._current_t(), reset=not self.sent)
            if self.started and msg["rate"] == self.active_rate:
                self.client.resume(pick_up=False)
                self.client.synctime(self._position)
                self.playing = True
                self.anchor_t = self._current_t()
                self.anchor_mono = self.last_sync = self.monotonic()
            else:
                self._play()
        elif msg["rate"] != self.active_rate:
            self._play()
        self.rate = msg["rate"]
        if self.playing:
            self._feed(self._current_t())
            now = self.monotonic()
            device_t = self.anchor_t + (now - self.anchor_mono) * 1000 * self.active_rate
            drift = self._current_t(now) - device_t
            if (abs(drift) >= DRIFT_MS and now - self.last_sync >= DRIFT_INTERVAL
                    or now - self.last_sync >= DRIFT_HEARTBEAT):
                self.client.synctime(self._position)
                now = self.monotonic()
                device_t = self.anchor_t + (now - self.anchor_mono) * 1000 * self.active_rate
                self.anchor_t = device_t + (self._current_t(now) - device_t) * 0.5
                self.anchor_mono = self.last_sync = now
        return True

    def timeout(self):
        if self.started:
            self.client.stop()
        self.playing = self.started = False
        self.last = None
        self.sent = self.buffered_start = 0


def flip_positions(actions):
    """The same script upside down (100 - pos): the player's Invert setting."""
    return [{"at": a["at"], "pos": 100 - a["pos"]} for a in actions]


def validate_haptics(h):
    """The player's haptics block, or None when it is missing or not usable (the bridge then keeps the last good one)."""
    if not isinstance(h, dict):
        return None
    offset, low, high, invert = (h.get(k) for k in ("offset_ms", "stroke_min", "stroke_max", "invert"))
    if any(type(v) is not int for v in (offset, low, high)) or type(invert) is not bool:
        return None
    if not -5000 <= offset <= 5000 or not 0 <= low < high <= 100:
        return None
    out = {"offset_ms": offset, "stroke_min": low, "stroke_max": high, "invert": invert}
    if isinstance(h.get("auto"), dict):
        out["auto"] = h["auto"]  # checked by the bridge, which knows the presets (Bridge._clean_auto)
    return out


def validate_message(raw):
    try:
        msg = json.loads(raw)
    except (UnicodeError, ValueError, RecursionError):
        return None
    if not isinstance(msg, dict):
        return None
    if type(msg.get("v")) is not int or msg["v"] != 1 or type(msg.get("seq")) is not int or msg["seq"] < 0:
        return None
    video, t, playing, rate = (msg.get(k) for k in ("video", "t", "playing", "rate"))
    if not isinstance(video, str) or not video or "\x00" in video or not video_path(video).is_absolute():
        return None
    if type(t) is not int or not 0 <= t <= 2 ** 53 or type(playing) is not bool:
        return None
    try:
        if type(rate) not in (int, float) or not math.isfinite(rate) or rate <= 0:
            return None
        msg["rate"] = float(rate)
    except OverflowError:
        return None
    msg["haptics"] = validate_haptics(msg.get("haptics"))
    return msg


def serve(args, client, emit=print, monotonic=time.monotonic):
    bridge = Bridge(client, variant=args.variant, offset_ms=args.offset_ms, emit=emit, monotonic=monotonic)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    last_packet = monotonic()
    try:
        sock.bind((args.bind, args.port))
        sock.settimeout(0.25)
        client.log(f"handy-sync listening on {args.bind}:{args.port}")
        while True:
            if bridge.last is not None and monotonic() - last_packet >= UDP_TIMEOUT:
                bridge.timeout()
            try:
                raw, _ = sock.recvfrom(65535)
            except socket.timeout:
                continue
            newest = validate_message(raw)
            sock.setblocking(False)
            # Bounded draining keeps a flooded listener from postponing stop forever.
            for _ in range(256):
                try:
                    raw, _ = sock.recvfrom(65535)
                except BlockingIOError:
                    break
                candidate = validate_message(raw)
                if candidate is not None and (newest is None or candidate["seq"] > newest["seq"]):
                    newest = candidate
            sock.settimeout(0.25)
            if newest is not None and bridge.process_safely(newest):
                last_packet = bridge.last["received_mono"]
    except KeyboardInterrupt:
        pass
    finally:
        try:
            client.stop()
        finally:
            sock.close()


def wait_for_handy(client, emit=print, sleep=time.sleep):
    """Clock-sync the Handy. One that is not connected yet (launched before it was switched on, or it dropped off wifi) is waited for,
    telling the user once, instead of ending the bridge: it only ends on a real startup problem such as a bad key."""
    told = False
    while True:
        try:
            client.clock_sync()
            if told:
                emit("handy-sync: the Handy is connected")
            return
        except HandyError as exc:
            if exc.code != DEVICE_NOT_CONNECTED:
                raise
            if not told:
                emit(f"handy-sync: the Handy is not connected (switch it on); trying again every {RETRY_SECONDS:g} s")
                told = True
            sleep(RETRY_SECONDS)


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "selftest":
        test_parser = argparse.ArgumentParser(prog="handy-sync selftest")
        test_parser.add_argument("--offline", action="store_true")
        args = test_parser.parse_args(argv[1:])
        # -I excludes cwd; only our own resolved tools directory is added.
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import handy_sync_selftest
        return handy_sync_selftest.run(offline=args.offline)
    parser = argparse.ArgumentParser(description="Follow a local funscript from the VR player")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true", help="print redacted calls, no HTTP (default)")
    mode.add_argument("--live", action="store_true", help="send calls to Handy cloud")
    parser.add_argument("--offset-ms", type=int, default=0)
    parser.add_argument("--variant")
    parser.add_argument("--port", type=int, default=17777)
    parser.add_argument("--bind", default="127.0.0.1",
                        help="address to listen on; a player on Windows cannot reach WSL's 127.0.0.1 (UDP), "
                             "so use 0.0.0.0 there (reachable from the Windows host only, WSL runs behind NAT)")
    parser.add_argument("--keys", type=Path)
    args = parser.parse_args(argv)
    if not 1 <= args.port <= 65535:
        parser.error("--port must be 1..65535")
    if args.variant and (args.variant in (".", "..") or any(c in args.variant for c in "/\\\x00")):
        parser.error("--variant must be a sidecar variant name")
    client = None
    try:
        drive_map = load_drive_map()
        set_drive_map(drive_map)
        print(f"handy-sync: drive map: {len(drive_map)} entries" if drive_map else "handy-sync: no drive map (plain /mnt/<letter>)", flush=True)
        keys = read_keys(args.keys or DEFAULT_KEYS) if args.live or args.keys else ("", "", "")
        client = HspClient(keys[0], keys[2], keys[1], dry_run=not args.live)
        if args.live:
            print("handy-sync: LIVE - sending to the Handy cloud; the Handy will move when a video with a script plays", flush=True)
        else:
            print("handy-sync: DRY RUN - nothing is sent, the Handy will NOT move. For the real device add --live --keys FILE", flush=True)
        client.sync_server_time(30)
        wait_for_handy(client, emit=lambda m: print(m, flush=True))
        serve(args, client)
        return 0
    except HandyError as exc:
        print("handy-sync: " + redact(exc, client.secrets if client else ()), file=sys.stderr)
    except OSError as exc:
        print("handy-sync: local I/O failed (" + redact(f"{exc.strerror or type(exc).__name__}"
              + (f": {exc.filename}" if exc.filename else ""), client.secrets if client else ())
              + "); check the UDP port, permissions and the video's folder", file=sys.stderr)
    except Exception:
        print("handy-sync: unexpected internal error", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
