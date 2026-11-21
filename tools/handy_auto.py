"""Auto mode for the Handy bridge: make a funscript out of the patterns mined from real scripts.

`tools/funscript_patterns.py` reads a pile of funscripts and writes `tools/auto_presets.json` (patterns, how they follow each other,
pauses, presets). `generate()` plays that back as a list of `{"at": ms, "pos": 0..100}` actions: a chain of phrases (the same stroke
repeated a few seconds), sometimes an accent rhythm (long-short, long-short-short), with a little natural jitter and the odd pause.

The "random" style chains the mined styles: every minute or few it moves to another style with its own speed, stroke window, pattern time
and pauses, all drawn from the seed and kept inside the sliders (which are its limits).

The sliders (all in `params`, like Handyverse's Random Stroker):
  speed      [lo, hi]  percent of the Handy's top speed (100 = about 364 script units a second on a full stroke)
  stroke     [lo, hi]  the part of the travel the patterns live in; a pattern keeps its shape inside it
  min_stroke           no stroke shorter than this many units
  pattern_s  [lo, hi]  how long one pattern lasts, seconds
  pause_s    [lo, hi]  how long a pause between patterns lasts; [0, 0] means no pauses
Standard library only. The same seed and params always give the same script.
"""
import json
import math
import random
from pathlib import Path

PRESETS_FILE = Path(__file__).with_name("auto_presets.json")
MODES = ("off", "no_script", "always")


def load_presets(path=PRESETS_FILE):
    """The mined patterns and presets. A missing or damaged file is an error to show, not to hide."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
        assert data["presets"] and data["patterns"] and data["transitions"] and data["handy_max_speed"] > 0
    except (OSError, ValueError, KeyError, TypeError, AssertionError) as e:
        raise RuntimeError(f"auto mode needs {path}: {e} (make it with tools/funscript_patterns.py)") from e
    return data


def preset(data, preset_id):
    for p in data["presets"]:
        if p["id"] == preset_id:
            return p
    return None


def defaults(p):
    """The slider values a preset starts with."""
    return {"preset": p["id"], "speed": list(p["speed"]), "stroke": [0, 100], "min_stroke": p["min_stroke"],
            "pattern_s": list(p["pattern_s"]), "pause_s": list(p["pause_s"])}


def _pair(v, lo, hi, gap=0.0):
    if not isinstance(v, (list, tuple)) or len(v) != 2:
        return None
    if any(isinstance(x, bool) or not isinstance(x, (int, float)) or not math.isfinite(x) for x in v):
        return None
    a, b = float(v[0]), float(v[1])
    if not (lo <= a <= b <= hi) or b - a < gap:
        return None
    return [a, b]


def validate_params(raw, data):
    """The params as the generator wants them, or None when anything is missing or out of range."""
    if not isinstance(raw, dict) or preset(data, raw.get("preset")) is None:
        return None
    speed, stroke = _pair(raw.get("speed"), 0, 100, 5), _pair(raw.get("stroke"), 0, 100, 5)
    pattern, pause = _pair(raw.get("pattern_s"), 0.1, 120), _pair(raw.get("pause_s"), 0, 120)
    ms = raw.get("min_stroke")
    if None in (speed, stroke, pattern, pause) or isinstance(ms, bool) or not isinstance(ms, (int, float)) or not 0 <= ms <= 100:
        return None
    return {"preset": raw["preset"], "speed": speed, "stroke": stroke, "min_stroke": float(ms),
            "pattern_s": pattern, "pause_s": pause}


class _Path:
    """The line being drawn: stroke by stroke, never faster than the Handy (or the speed slider) allows."""

    def __init__(self, cap, speed_max):
        self.cap = cap * min(speed_max, 100.0) / 100.0
        self.pts = [(0.0, 0.0)]

    @property
    def t(self):
        return self.pts[-1][0]

    @property
    def pos(self):
        return self.pts[-1][1]

    def move(self, to, seconds):
        seconds = max(seconds, abs(to - self.pos) / self.cap)
        self.pts.append((self.t + seconds * 1000.0, to))

    def hold(self, seconds):
        self.pts.append((self.t + seconds * 1000.0, self.pos))


def _pick(rng, weights):
    total = sum(weights)
    if total <= 0:
        return rng.randrange(len(weights))
    x = rng.random() * total
    for i, w in enumerate(weights):
        x -= w
        if x < 0:
            return i
    return len(weights) - 1


RANDOM = "random"
SCENE_SECONDS = (60.0, 240.0)  # how long the random generator stays in one style


def random_scenes(data, params, seed, until_ms):
    """The plan of a Random script: [(style params, seconds), ...] long enough for `until_ms`.
    Each scene is one of the mined styles (never the same twice in a row) with its own speed, stroke window,
    pattern time and pauses, all drawn from the seed and all inside the sliders' limits."""
    rng = random.Random(seed)
    styles = [p for p in data["presets"] if p["id"] != RANDOM and "weights" in p]
    weights = [p["share"] ** 0.5 for p in styles]  # steady styles come up more often, the rare ones still do
    s_lo, s_hi = params["speed"]
    w_lo, w_hi = params["stroke"]
    q_lo, q_hi = params["pattern_s"]
    z_lo, z_hi = params["pause_s"]
    scenes, total, prev = [], 0.0, None
    while total < until_ms / 1000.0:
        pool = [i for i in range(len(styles)) if i != prev]
        i = pool[_pick(rng, [weights[j] for j in pool])]
        style, prev = styles[i], i
        hi = max(s_lo + (s_hi - s_lo) * rng.uniform(0.4, 1.0), s_lo + 5.0)
        lo = min(s_lo + (hi - s_lo) * rng.uniform(0.0, 0.6), hi - 5.0)
        width = max((w_hi - w_lo) * rng.uniform(0.5, 1.0), min(5.0, w_hi - w_lo))
        start = w_lo + (w_hi - w_lo - width) * rng.random()
        k = rng.uniform(0.7, 1.4)
        pattern = [min(max(style["pattern_s"][0] * k, q_lo), q_hi), min(max(style["pattern_s"][1] * k, q_lo), q_hi)]
        pause = [0.0, 0.0] if z_hi <= 0 else [min(max(style["pause_s"][0], z_lo), z_hi), min(max(style["pause_s"][1], z_lo), z_hi)]
        seconds = rng.uniform(*SCENE_SECONDS)
        scenes.append(({"preset": style["id"], "speed": [lo, hi], "stroke": [start, start + width],
                        "min_stroke": params["min_stroke"], "pattern_s": pattern, "pause_s": pause}, seconds, rng.getrandbits(32)))
        total += seconds
    return scenes


def generate_random(data, params, seed, until_ms):
    """A whole script that keeps changing: the scenes of `random_scenes`, joined by a gentle move that never
    asks the Handy for more than its top speed."""
    cap = float(data["handy_max_speed"])
    out = []
    for scene, seconds, scene_seed in random_scenes(data, params, seed, until_ms):
        acts = generate(data, scene, scene_seed, seconds * 1000.0)
        if out:
            gap = max(800.0, abs(acts[0]["pos"] - out[-1]["pos"]) / cap * 1000.0 * 1.25)
            shift = out[-1]["at"] + int(round(gap))
        else:
            shift = 0
        out.extend({"at": a["at"] + shift, "pos": a["pos"]} for a in acts)
    return out


def generate(data, params, seed, until_ms):
    """Actions from 0 to at least `until_ms`: [{"at": int ms, "pos": int 0..100}, ...]."""
    if params["preset"] == RANDOM:
        return generate_random(data, params, seed, until_ms)
    p = preset(data, params["preset"])
    rng = random.Random(seed)
    cap = float(data["handy_max_speed"])
    s_lo, s_hi = params["speed"]
    w_lo, w_hi = params["stroke"]
    ref_lo, ref_hi = p["speed"]
    steady = [x for x in data["patterns"] if x["kind"] == "steady"]
    accents = [x for x in data["patterns"] if x["kind"] == "accent"]
    gw = [max(x["weight"], 1e-6) for x in steady]
    pw = p["weights"]
    ref_secs = sum(w * x["secs"][1] for w, x in zip(pw, steady)) or 3.0
    jitter = data["humanize"]
    cv, end_sd = jitter["period_cv"], jitter["end_sd"]
    rest = data["pauses"]["rest"]
    path = _Path(cap, s_hi)
    path.pts[0] = (0.0, w_lo)
    prev = None

    def geometry(low, high, period):
        """A pattern's stroke inside the stroke window and its time for one stroke (seconds)."""
        lo = w_lo + low / 100.0 * (w_hi - w_lo)
        hi = w_lo + high / 100.0 * (w_hi - w_lo)
        need = min(params["min_stroke"], w_hi - w_lo)
        if hi - lo < need:  # grow around the middle, inside the window
            mid = min(max((lo + hi) / 2.0, w_lo + need / 2.0), w_hi - need / 2.0)
            lo, hi = mid - need / 2.0, mid + need / 2.0
        natural = min((high - low) / max(period / 2.0, 0.02) / cap * 100.0, 100.0)
        f = (natural - ref_lo) / max(ref_hi - ref_lo, 1.0)
        speed = min(max(s_lo + f * (s_hi - s_lo), s_lo), s_hi)  # percent of the top speed
        return lo, hi, max((hi - lo) / (max(speed, 1.0) / 100.0 * cap), 0.03)

    def cycle(lo, hi, stroke_s, jitter_ends):
        near_top = abs(path.pos - hi) <= abs(path.pos - lo)
        a, b = (lo, hi) if near_top else (hi, lo)
        if jitter_ends:
            a = min(max(a + rng.gauss(0, end_sd), w_lo), w_hi)
            b = min(max(b + rng.gauss(0, end_sd), w_lo), w_hi)
        for end in (a, b):
            path.move(end, stroke_s * min(max(1.0 + rng.gauss(0, cv), 0.75), 1.25))

    while path.t < until_ms:
        if accents and rng.random() < data.get("accent_chance", 0.0):
            pat = accents[_pick(rng, [x["weight"] for x in accents])]
            seconds = max(rng.uniform(*params["pattern_s"]) * pat["secs"][1] / ref_secs, 1.0)
            cycles = [geometry(lo, hi, per) for lo, hi, per in pat["template"]]
            if abs(path.pos - cycles[0][0]) > 1 and abs(path.pos - cycles[0][1]) > 1:
                path.move(cycles[0][1], cycles[0][2])
            i = 0
            start = path.t
            while path.t - start < seconds * 1000.0 or i % len(cycles):
                lo, hi, stroke_s = cycles[i % len(cycles)]
                cycle(lo, hi, stroke_s, False)
                i += 1
            prev = None
        else:
            weights = pw if prev is None else [t * (a / g) for t, a, g in zip(data["transitions"][prev], pw, gw)]
            prev = _pick(rng, weights)
            pat = steady[prev]
            per = pat["period_s"]
            lo, hi, stroke_s = geometry(pat["low"], pat["high"], rng.uniform(per[0], per[2]))
            seconds = rng.uniform(*params["pattern_s"]) * min(max(pat["secs"][1] / ref_secs, 0.4), 2.5)
            n = max(2, round(seconds / (2 * stroke_s)))
            if abs(path.pos - lo) > 1 and abs(path.pos - hi) > 1:
                path.move(hi, stroke_s)
            for _ in range(n):
                cycle(lo, hi, stroke_s, hi - lo < 90)
        if params["pause_s"][1] > 0 and rng.random() < p["pause_chance"]:
            where = rng.random()
            target = w_hi if where < rest["top"] else w_lo if where < rest["top"] + rest["bottom"] else (w_lo + w_hi) / 2.0
            if abs(target - path.pos) > 1:
                path.move(target, 0.6)
            path.hold(rng.uniform(*params["pause_s"]))
    out, last = [], -1
    for t, pos in path.pts:
        at = int(round(t))
        if at > last:
            out.append({"at": at, "pos": int(round(min(max(pos, 0.0), 100.0)))})
            last = at
    return out
