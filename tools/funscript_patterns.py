#!/usr/bin/env python3
"""Mine human patterns out of a pile of funscripts and write `tools/auto_presets.json` for the Handy auto mode.

    python3 tools/funscript_patterns.py [ROOT ...] [--list FILE] [--out FILE] [--seed N]

What it does: reads every `*.funscript` under the ROOTs (duplicates by content are counted once), reduces each to its strokes
(the turning points), groups repeating strokes into *phrases* (same range and tempo for 3+ cycles), clusters the phrases into
a handful of *patterns*, finds accent rhythms (long-short, long-short-short, ...), the pauses, and groups whole scripts into
*styles*; a style becomes a preset.

The output holds statistics only: no file names, titles or paths. Needs numpy (offline tool; the bridge itself is stdlib only).
Re-running with the same scripts and seed gives the same file.
"""
import argparse
import hashlib
import json
import sys
from pathlib import Path

import numpy as np

HANDY_MAX_SPEED = 364.0  # script units per second on a full stroke: the firmware cap (docs/handy-sync.md)
ZIGZAG = 8.0             # wiggles smaller than this are not strokes
MIN_CYCLES = 3           # a phrase repeats at least this often
TEMPO_TOL, END_TOL = 0.3, 15.0
N_PATTERNS, N_STYLES = 10, 6
# The "Random" style: its sliders are the limits the random generator stays inside (handy_auto.generate_random).
RANDOM_PRESET = {"id": "random", "name": "Random", "speed": [20.0, 100.0], "stroke": [0.0, 100.0], "min_stroke": 20.0,
                 "pattern_s": [0.3, 15.0], "pause_s": [1.3, 12.0]}
HERE = Path(__file__).resolve().parent


# ── reading ────────────────────────────────────────────────────────────────────

def find_files(roots, list_files):
    """Every *.funscript under the roots, plus the paths listed (one per line) in the list files."""
    paths = []
    for root in roots:
        paths += sorted(Path(root).rglob("*.funscript"))
    for lst in list_files:
        paths += [Path(line.rstrip("\n")) for line in open(lst, encoding="utf-8", errors="replace") if line.strip()]
    return sorted(set(paths))


def load_scripts(paths):
    """Distinct scripts as float arrays [[ms, pos], ...] (sorted by time); also how many files were skipped."""
    seen, scripts, skipped = set(), [], 0
    for path in paths:
        try:
            actions = json.loads(path.read_bytes())["actions"]
            a = np.array([(x["at"], x["pos"]) for x in actions], dtype=float)
        except (OSError, ValueError, KeyError, TypeError):
            skipped += 1
            continue
        if len(a) < 50 or not np.isfinite(a).all():
            skipped += 1
            continue
        a = a[np.argsort(a[:, 0], kind="stable")]
        key = hashlib.sha1(a.tobytes()).digest()
        if key not in seen:
            seen.add(key)
            scripts.append(a)
    return scripts, skipped


# ── strokes, phrases, pauses ───────────────────────────────────────────────────

def turning_points(a, thr=ZIGZAG):
    """The extremes of the motion (zigzag), ignoring wiggles smaller than `thr`: [[ms, pos], ...]."""
    t, p = a[:, 0], a[:, 1]
    out = [(t[0], p[0])]
    direction, ext_t, ext_p = 0, t[0], p[0]
    for i in range(1, len(t)):
        if direction >= 0 and p[i] >= ext_p:
            direction = 1 if (p[i] > ext_p or direction == 1) else direction
            ext_t, ext_p = t[i], p[i]
        elif direction <= 0 and p[i] <= ext_p:
            direction = -1 if (p[i] < ext_p or direction == -1) else direction
            ext_t, ext_p = t[i], p[i]
        elif abs(p[i] - ext_p) >= thr:
            out.append((ext_t, ext_p))
            direction = -direction if direction else (1 if p[i] > ext_p else -1)
            ext_t, ext_p = t[i], p[i]
    out.append((ext_t, ext_p))
    res = [out[0]]
    for q in out[1:]:
        if q[0] > res[-1][0]:
            res.append(q)
    return np.array(res)


def find_phrases(e, script_id):
    """Runs of 3+ cycles with the same range and tempo. One row per phrase:
    script, high, low, period_s, cycles, start_s, period_cv, high_sd, low_sd."""
    rows, i, m = [], 0, len(e)
    while i + 2 < m:
        j, cyc = i, []
        while j + 2 < m:
            per = (e[j + 2, 0] - e[j, 0]) / 1000.0
            hi, lo = max(e[j, 1], e[j + 1, 1]), min(e[j, 1], e[j + 1, 1])
            if cyc:
                c = np.array(cyc)
                mper, mhi, mlo = np.median(c[:, 0]), np.median(c[:, 1]), np.median(c[:, 2])
                if not ((1 - TEMPO_TOL) * mper <= per <= (1 + TEMPO_TOL) * mper
                        and abs(hi - mhi) <= END_TOL and abs(lo - mlo) <= END_TOL):
                    break
            cyc.append((per, hi, lo))
            j += 2
        if len(cyc) >= MIN_CYCLES:
            c = np.array(cyc)
            per = np.median(c[:, 0])
            rows.append((script_id, np.median(c[:, 1]), np.median(c[:, 2]), per, len(cyc), e[i, 0] / 1000.0,
                         c[:, 0].std() / per, c[:, 1].std(), c[:, 2].std()))
            i = j
        else:
            i += 2
    return rows


def find_pauses(a):
    """Stretches of 1 s or more with (almost) no movement: [(seconds, rest position), ...]."""
    t, p = a[:, 0] / 1000.0, a[:, 1]
    out, i = [], 0
    while i < len(t) - 1:
        j = i
        while j + 1 < len(t) and abs(p[j + 1] - p[i]) <= 4:
            j += 1
        if t[j] - t[i] >= 1.0:
            out.append((t[j] - t[i], p[i]))
        i = max(j, i + 1)
    return out


def find_accents(e, per_script_limit=400):
    """Windows of 12 cycles that repeat every 2, 3 or 4 cycles better than every cycle: {L: [window cycles...]}."""
    n = (len(e) - 1) // 2
    found = {2: [], 3: [], 4: []}
    steady = total = 0
    if n < 16:
        return found, steady, total
    hi = np.array([max(e[2 * k, 1], e[2 * k + 1, 1], e[2 * k + 2, 1]) for k in range(n)])
    lo = np.array([min(e[2 * k, 1], e[2 * k + 1, 1], e[2 * k + 2, 1]) for k in range(n)])
    per = np.array([(e[2 * k + 2, 0] - e[2 * k, 0]) / 1000.0 for k in range(n)])
    X = np.column_stack([lo, hi, per])
    Xn = np.column_stack([lo / 100.0, hi / 100.0, np.log2(per) / 2.0])
    for s in range(0, n - 12, 12):
        if per[s:s + 12].max() > 3.0:
            continue
        x = Xn[s:s + 12]
        err = {L: np.abs(x[L:] - x[:-L]).mean() for L in (1, 2, 3, 4)}
        total += 1
        if err[1] < 0.06:
            steady += 1
            continue
        best = min((2, 3, 4), key=lambda L: err[L])
        if err[best] < 0.5 * err[1] and err[best] < 0.08 and len(found[best]) < per_script_limit:
            found[best].append((s, X[s:s + best]))
    return found, steady, total


# ── clustering ─────────────────────────────────────────────────────────────────

def kmeans(X, w, k, rng, restarts=4, iters=40):
    best = None
    for _ in range(restarts):
        c = X[rng.choice(len(X), k, replace=False, p=w / w.sum())].copy()
        for _ in range(iters):
            lab = ((X[:, None, :] - c[None]) ** 2).sum(-1).argmin(1)
            for j in range(k):
                m = lab == j
                if m.any():
                    c[j] = np.average(X[m], axis=0, weights=w[m])
        lab = ((X[:, None, :] - c[None]) ** 2).sum(-1).argmin(1)
        inertia = float((w * ((X - c[lab]) ** 2).sum(1)).sum())
        if best is None or inertia < best[0]:
            best = (inertia, c, lab)
    return best[1], best[2]


def pct(x, qs):
    return [round(float(v), 3) for v in np.percentile(x, qs)]


def describe(low, high, period):
    rng_word = ("full" if high - low >= 80 else "top" if low >= 35 else "bottom" if high <= 65 else "short")
    tempo = ("slow" if period >= 0.8 else "medium" if period >= 0.4 else "quick" if period >= 0.18 else "flutter")
    return f"{rng_word} {tempo} {period:.2f}s"


# ── main analysis ──────────────────────────────────────────────────────────────

def analyse(scripts, seed=1):
    rng = np.random.default_rng(seed)
    phrases, pauses, accents = [], [], {2: [], 3: [], 4: []}
    acc_steady = acc_total = 0
    per_script_pauses = []
    spans = []
    for sid, a in enumerate(scripts):
        e = turning_points(a)
        spans.append((a[-1, 0] - a[0, 0]) / 1000.0)
        pz = find_pauses(a)
        per_script_pauses.append(pz)
        pauses += pz
        if len(e) < 6:
            continue
        phrases += find_phrases(e, sid)
        f, s, t = find_accents(e)
        acc_steady, acc_total = acc_steady + s, acc_total + t
        for L in f:
            accents[L] += [x for _, x in f[L]]
    P = np.array(phrases)
    if len(P) < 1000:
        raise SystemExit(f"only {len(P)} phrases found: need a bigger pile of scripts")
    w = np.minimum(P[:, 4], 30)
    X = np.column_stack([P[:, 2] / 100.0, P[:, 1] / 100.0, np.log2(P[:, 3]) / 2.0])
    centres, lab = kmeans(X, w, N_PATTERNS, rng)
    order = np.argsort(-np.array([w[lab == j].sum() for j in range(N_PATTERNS)]))
    rank = np.empty(N_PATTERNS, dtype=int)
    rank[order] = np.arange(N_PATTERNS)
    lab = rank[lab]  # 0 = the most common pattern
    secs = P[:, 3] * P[:, 4]

    patterns = []
    total_w = w.sum()
    for k in range(N_PATTERNS):
        m = lab == k
        low, high, per = float(np.median(P[m, 2])), float(np.median(P[m, 1])), float(np.median(P[m, 3]))
        patterns.append({
            "id": f"p{k}", "kind": "steady", "name": describe(low, high, per),
            "low": round(low, 1), "high": round(high, 1), "period_s": pct(P[m, 3], [25, 50, 75]),
            "speed": round(min((high - low) / (per / 2), 9999.0), 1), "secs": pct(secs[m], [25, 50, 75]),
            "weight": round(float(w[m].sum() / total_w), 4),
        })
    # accent rhythms: the same cycle sequence repeated; weight = share of windows
    for L, wins in accents.items():
        if len(wins) < 200:
            continue
        arr = np.array(wins)  # n, L, 3 (low, high, period)
        al = []
        for s in arr:
            r = int(np.argmax(s[:, 1] - s[:, 0] + 0.001 * s[:, 2]))
            al.append(np.roll(s, -r, axis=0))
        al = np.array(al)
        tmpl = np.median(al, axis=0)
        patterns.append({
            "id": f"a{L}", "kind": "accent", "name": f"accent every {L}",
            "template": [[round(float(c[0]), 1), round(float(c[1]), 1), round(float(c[2]), 3)] for c in tmpl],
            "secs": [round(12 * float(tmpl[:, 2].mean()) * f, 2) for f in (0.5, 1.0, 1.5)],
            "weight": round(len(wins) / max(acc_total, 1), 4),
        })
    n_steady = N_PATTERNS
    # transitions between steady patterns inside a script, in order
    T = np.zeros((N_PATTERNS, N_PATTERNS))
    for i in range(len(P) - 1):
        if P[i, 0] == P[i + 1, 0]:
            T[lab[i], lab[i + 1]] += 1
    T = T / np.maximum(T.sum(1, keepdims=True), 1)

    pz = np.array(pauses) if pauses else np.array([[2.0, 0.0]])
    pauses_out = {
        "length_s": pct(pz[:, 0], [25, 50, 75, 90]),
        "rest": {"bottom": round(float((pz[:, 1] <= 10).mean()), 3),
                 "middle": round(float(((pz[:, 1] > 10) & (pz[:, 1] < 90)).mean()), 3),
                 "top": round(float((pz[:, 1] >= 90).mean()), 3)},
    }
    long_run = P[:, 4] >= 6
    inner = long_run & ((P[:, 1] - P[:, 2]) < 90)  # strokes that stop at 0 or 100 have no end jitter to measure
    humanize = {"period_cv": round(float(np.median(P[long_run, 6])), 3),
                "end_sd": round(float(np.median(np.concatenate([P[inner, 7], P[inner, 8]]))), 2)}

    presets = make_presets(scripts, spans, P, lab, w, secs, per_script_pauses, patterns, rng)
    presets.append(RANDOM_PRESET)  # not mined: handy_auto.py chains the mined styles at random
    summary = {
        "scripts": len(scripts), "phrases": int(len(P)),
        "accent_windows_share": round(sum(len(v) for v in accents.values()) / max(acc_total, 1), 4),
        "steady_windows_share": round(acc_steady / max(acc_total, 1), 4),
    }
    return {
        "version": 1,
        "about": "Statistics mined from funscripts by tools/funscript_patterns.py; no titles or paths.",
        "handy_max_speed": HANDY_MAX_SPEED,
        "corpus": summary,
        "accent_chance": summary["accent_windows_share"],  # how often a phrase is an accent rhythm
        "patterns": patterns,
        "transitions": [[round(float(v), 3) for v in row] for row in T],
        "pauses": pauses_out,
        "humanize": humanize,
        "presets": presets,
    }


def style_name(period, paused, high, low, taken):
    if paused >= 0.25:
        name = "Tease"
    elif period >= 0.7:
        name = "Slow and full"
    elif period <= 0.2:
        name = "Flutter"
    elif high - low <= 70 and period < 0.4:
        name = "Short and quick"
    elif paused >= 0.1:
        name = "Steady with breaks"
    else:
        name = "Steady"
    base, n = name, 2
    while name in taken:
        name, n = f"{base} {n}", n + 1
    return name


def make_presets(scripts, spans, P, lab, w, secs, per_script_pauses, patterns, rng):
    """Group whole scripts by style; each group becomes a preset."""
    feats, ids = [], []
    for sid, a in enumerate(scripts):
        m = P[:, 0] == sid
        if m.sum() < 5:
            continue
        ww = np.minimum(P[m, 4], 30)
        period = float(np.average(np.log2(P[m, 3]), weights=ww))
        lo, hi = float(np.average(P[m, 2], weights=ww)), float(np.average(P[m, 1], weights=ww))
        paused = sum(s for s, _ in per_script_pauses[sid]) / max(spans[sid], 1.0)
        rate = np.log(m.sum() / max(spans[sid] / 60.0, 0.1))
        feats.append([lo / 100, hi / 100, period / 2, min(paused, 0.6) * 4, rate / 3])
        ids.append(sid)
    F = np.array(feats)
    c, labs = kmeans(F, np.ones(len(F)), N_STYLES, rng, restarts=6, iters=60)
    out, taken = [], set()
    script_style = {sid: int(l) for sid, l in zip(ids, labs)}
    phrase_style = np.array([script_style.get(int(s), -1) for s in P[:, 0]])
    n_pat = len(set(lab))
    for j in np.argsort(-np.bincount(labs, minlength=N_STYLES)):
        sel = labs == j
        if not sel.any():
            continue
        f = F[sel].mean(0)
        m = phrase_style == j
        if m.sum() < 50:
            continue
        ww = np.minimum(P[m, 4], 30)
        weights = [float(ww[lab[m] == k].sum() / ww.sum()) for k in range(n_pat)]
        speeds = np.minimum((P[m, 1] - P[m, 2]) / (P[m, 3] / 2) / HANDY_MAX_SPEED * 100.0, 100.0)
        s_lo, s_hi = np.percentile(speeds, [10, 90])
        sids = [sid for sid, l in script_style.items() if l == j]
        pz = [x for sid in sids for x in per_script_pauses[sid]]
        chance = min(len(pz) / max(int(m.sum()), 1), 0.6)
        plen = np.percentile([x[0] for x in pz], [25, 85]) if pz else np.array([1.0, 4.0])  # pauses have a long tail
        amp = P[m, 1] - P[m, 2]
        name = style_name(2 ** (f[2] * 2), f[3] / 4, f[1] * 100, f[0] * 100, taken)
        taken.add(name)
        out.append({
            "id": name.lower().replace(" ", "-"), "name": name,
            "share": round(float(sel.mean()), 3),
            "weights": [round(v, 4) for v in weights],
            "pause_chance": round(float(chance), 3),
            "speed": [round(float(max(s_lo, 5.0)), 1), round(float(max(s_hi, s_lo + 10.0)), 1)],
            "stroke": [round(float(np.percentile(P[m, 2], 10)), 1), round(float(np.percentile(P[m, 1], 90)), 1)],
            "min_stroke": round(float(np.percentile(amp, 10)), 1),
            "pattern_s": [round(float(v), 1) for v in np.percentile(secs[m], [25, 75])],
            "pause_s": [round(float(v), 1) for v in plen],
        })
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("roots", nargs="*", help="folders to search for *.funscript")
    ap.add_argument("--list", action="append", default=[], metavar="FILE", help="a file with one funscript path per line (repeatable)")
    ap.add_argument("--out", default=str(HERE / "auto_presets.json"))
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args(argv)
    if not args.roots and not args.list:
        ap.error("give at least one folder or --list FILE")
    scripts, skipped = load_scripts(find_files(args.roots, args.list))
    print(f"read {len(scripts)} distinct scripts ({skipped} unreadable or too short)", file=sys.stderr)
    if not scripts:
        raise SystemExit("no usable funscripts found")
    result = analyse(scripts, args.seed)
    Path(args.out).write_text(json.dumps(result, indent=1) + "\n")
    print(f"wrote {args.out}: {len(result['patterns'])} patterns, {len(result['presets'])} presets", file=sys.stderr)
    for p in result["presets"]:
        print(f"  {p['name']:20s} share {p['share']*100:4.1f}%  speed {p['speed']}%  stroke {p['stroke']}  min {p['min_stroke']}"
              f"  pattern {p['pattern_s']}s  pause {p['pause_s']}s (chance {p['pause_chance']})", file=sys.stderr)


if __name__ == "__main__":
    main()
