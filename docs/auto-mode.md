# Auto mode (the Handy without a funscript)

For a video with no funscript, or when you want something else, the bridge makes the strokes itself. Not a random walk: it plays back
patterns that were **mined from real, hand-made funscripts** (the author's own collection), so it moves the way a person scripting a PMV does.
Built, deployed and clicked through in a desktop window 2026-11-19; not tried in the headset yet.

## Using it
Settings (`S`) has the tabs Video, Screen, Haptics, **Auto** and Audio. All of Auto is saved for every video (`haptics.json`).

- **Mode:** Off, *No funscript only* (a video that has a script still follows its script) or Always (instead of the script).
- **Style:** Steady, Steady with breaks, Short and quick, Slow and full, Tease, Flutter, **Random**. Choosing one fills the sliders; move them as you like.
  Random is not one style: every one to four minutes it moves to another of the six (never the same twice in a row; the common ones more often),
  and each stretch gets its own speed range, stroke window, pattern time and pauses, drawn at random *inside the limits the sliders set*.
  So the sliders are the fence, and the seed decides what happens inside it.
- **Seed:** the number the script is built from. "Each video" (the default) gives every video a new one, so each plays differently; the bridge
  logs it ("auto mode: random (no funscript), seed 482913" in `bridge.log`). **New seed** keeps a new six-digit one: the same seed with the same
  style and sliders always plays the same script, so a run you liked can be played again, and pressing it while playing starts a different run at once.
- **Speed:** a range, percent of the Handy's top speed (100% is about 364 script units a second on a full stroke).
- **Stroke:** the part of the travel the patterns live in. A pattern keeps its shape inside it.
- **Shortest:** no stroke shorter than this.
- **Pattern time:** how long one pattern lasts. **Pause:** how long a pause between patterns lasts; drag its right end all the way left for none (a thumb pushes the other one along).

The Handy's own stroke range, offset and invert on the Haptics tab still apply on top.

## What was found in the funscripts (2026-11-19)
1,684 files, 1,295 distinct ones (the rest are copies), about 6 million strokes, 188,000 *phrases* (the same stroke repeated 3+ times).

- **Steady phrases are the main thing.** 54% of 12-cycle stretches repeat one range and tempo; phrases last a median 2.5 s, a tenth of them 15 s or more.
  Ten kinds cover them: full strokes at four tempos (a stroke cycle of 0.3 s up to 1.9 s), shallow strokes near the top, near the bottom,
  and a fast flutter. After a phrase, 46% of the time the same kind follows, usually at a slightly different tempo; tempo changes are otherwise
  sudden, not ramps (a speed-up over 4+ phrases is rare, about 1%).
- **Accent rhythms** (a long stroke, then shorter ones, repeating) are about 5% of the stretches: long-short, long-short-short, long-short-medium-short.
- **Pauses** of a second or more are rare (a script has one every 3 minutes at the median), last a median 2.3 s (one in ten over 16 s) and
  rest at the top 45%, the bottom 36%, the middle 18% of the time.
- **Natural jitter:** the time of a stroke varies about 7% inside a phrase; the ends stay put.
- **Six styles of whole script** (what the presets are): steady full strokes (38% of scripts), the same with breaks (19%), shorter quicker
  strokes (18%), slow full strokes (15%), tease with long pauses (5%) and a very fast flutter (4%).
- **Much of it asks more than the Handy can do:** the median stroke is 333 units/s, one in ten is over 556 and one in a hundred over 950,
  while the Handy's firmware stops at about 364. So auto mode never asks for more than the top speed; a pattern mined at 428 units/s plays at 364.

## How it works
- `tools/funscript_patterns.py [ROOT ...] [--list FILE] [--out FILE] [--seed N]` reads the scripts and writes `tools/auto_presets.json`
  (about 2 minutes for 1,684 files; needs numpy). The file holds statistics only: no titles, paths or server names. Same scripts and seed, same file.
  The player compiles the file in (the preset names and starting slider values); the bridge reads it from `tools/`.
- `tools/handy_auto.py` plays it back as a list of `{at, pos}` actions, standard library only: picks the next pattern from how patterns followed
  each other, sizes it by the sliders, adds the jitter, sometimes an accent rhythm, sometimes a pause. The same seed and settings always give the same
  script; each video gets a new seed, so every video plays differently.
- The player sends the settings in the `haptics.auto` block of the position message (`docs/handy-sync.md`); the bridge builds the script for the
  whole video (4 hours ahead, about 0.1 s) and feeds the Handy the same way as a funscript. Changing a setting rebuilds it and re-syncs at once.
- Check against the real scripts (generated 40 minutes per style, then analysed the same way): pauses, tempo, range and amplitude land near
  the real styles (Tease 31% paused against 34% in the real ones), and no stroke goes above the Handy's top speed.

## Not done yet
- Looked at, left out as too rare: gradual speed ramps (build-ups), pauses in the middle of a phrase.
- Not tried in the headset: how the Auto page reaches with a controller, and whether it *feels* like a person did it. (A window check found and fixed
  two slider faults: Pause could not reach "none", and two thumbs on one spot could not be pulled apart.)
  The presets can be renamed or re-cut (`N_STYLES` and `style_name()` in `funscript_patterns.py`).
- More tuning ideas if it feels flat: follow the video's own loudness or scene cuts, a "build up and release" arc over several minutes.
