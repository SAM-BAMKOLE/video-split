# Video Splitter (Tauri + React)

A desktop app that splits long videos into fixed-length chunks with no
re-encoding (audio/video sync preserved exactly). Ships as a single
installer — ffmpeg is bundled inside, the user never installs anything
separately.

## Architecture

- **Rust backend** (`src-tauri/src/lib.rs`): probes each video's duration
  with `ffprobe`, then cuts it chunk-by-chunk with `ffmpeg -c copy`
  (stream copy — fast, lossless, sync-safe). Runs **one ffmpeg process
  per chunk**, not one process for the whole file — this is what makes
  Pause/Stop and a real progress bar possible.
- **React frontend** (`src/App.tsx`): file/folder pickers, chunk-length
  input, progress bar, Start/Pause/Resume/Stop. Listens for a
  `split-progress` event emitted from Rust after every chunk.

## 1. Get ffmpeg onto Windows (and Mac/Linux) without asking the user

Tauri's "sidecar" feature bundles arbitrary binaries into your installer.
You do this once, at build time — the end user just double-clicks your
installer and ffmpeg is already there.

### Get static builds

- **Windows**: https://github.com/BtbN/FFmpeg-Builds/releases — grab the
  `win64-gpl` (or `win64-lgpl`, see licensing note below) zip, it
  contains `ffmpeg.exe` and `ffprobe.exe`.
- **macOS**: https://evermeet.cx/ffmpeg/ (separate `ffmpeg` and
  `ffprobe` binaries).
- **Linux**: same BtbN releases page has `linux64-gpl`/`linux64-lgpl`.

### Place and rename them

Tauri expects each sidecar binary named with the Rust target triple
suffix. Find yours with:

```
rustc -Vv | grep host
```

Then place, e.g. on Windows:

```
src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe
src-tauri/binaries/ffprobe-x86_64-pc-windows-msvc.exe
```

On Apple Silicon Mac:

```
src-tauri/binaries/ffmpeg-aarch64-apple-darwin
src-tauri/binaries/ffprobe-aarch64-apple-darwin
```

`tauri.conf.json` already declares `externalBin: ["binaries/ffmpeg", "binaries/ffprobe"]`
— the Tauri CLI finds the right file per platform automatically at build time.

If you need to ship for multiple platforms (e.g. build Windows installers
from a Mac), you'll need the binaries for each target triple present, or
build on/for each target via CI (GitHub Actions runners for
windows-latest / macos-latest / ubuntu-latest is the common approach).

### Licensing note (important, don't skip)

**Update:** the crop-for-reels feature re-encodes video with `libx264`,
which is GPL-licensed — the LGPL build no longer covers this. **Swap the
bundled ffmpeg/ffprobe binaries to the GPL build**
(`ffmpeg-master-latest-win64-gpl.zip` on the same BtbN releases page),
same filenames, same `src-tauri/binaries/` location. This is safe:
your app calls ffmpeg as a separate external process (not linked into
your own binary), which is the standard "mere aggregation" pattern —
it doesn't place your Rust/React code under the GPL. You do still need
to include ffmpeg's `LICENSE.txt` somewhere accessible (an About screen
is the usual spot) as a redistribution requirement.

If you only ever use plain splitting (crop toggle off), the LGPL build
would technically still work for that path — but since crop defaults
to on now, ship the GPL build so it works out of the box.

## 2. Install prerequisites

- Node.js 18+
- Rust (https://rustup.rs)
- Tauri CLI: `npm install` (already in devDependencies) or `cargo install tauri-cli`
- Platform build tools:
  - Windows: "Desktop development with C++" workload from Visual Studio Build Tools
  - macOS: Xcode Command Line Tools
  - Linux: see https://v2.tauri.app/start/prerequisites/ for the exact apt packages (webkit2gtk etc.)

## 3. Run in dev mode

```
npm install
npx tauri dev
```

## 4. Build the installer

```
npx tauri build
```

Output lands in `src-tauri/target/release/bundle/`:

- Windows: `.msi` and/or `.exe` (NSIS) installer
- macOS: `.app` and `.dmg`
- Linux: `.AppImage` and/or `.deb`

Since most of your users are on Windows, `msi` is the safer default
(better enterprise/IT compatibility); NSIS `.exe` gives a friendlier
custom install wizard if you want that instead. Both are already listed
in `tauri.conf.json > bundle > targets`.

## 5. Code signing (optional, recommended before wide distribution)

Unsigned installers trigger Windows SmartScreen and macOS Gatekeeper
warnings. For real distribution you'd want a Windows code-signing
certificate and an Apple Developer ID — Tauri has built-in config hooks
for both (`tauri.conf.json > bundle > windows/macOS` signing fields). Not
required to get the app working, just to make the install experience
warning-free.

## Notes on the splitting behavior

- Chunk cuts use `-ss` before `-i` (fast keyframe seek) with `-c copy`,
  so each chunk is cut in roughly the time it takes to read that much
  data off disk — not the time it takes to "watch" that chunk.
- Cuts snap to the nearest keyframe, so a "15 min" chunk may run a few
  seconds long/short. This is the trade-off for zero re-encoding and
  perfect sync.
- Pause takes effect between chunks (usually a few-second delay at
  most), not mid-chunk — this keeps the implementation simple and
  reliable across platforms. If you need instant mid-chunk pause later,
  that requires OS-level process suspension (`SIGSTOP`/`SIGCONT` on
  Unix; a WinAPI call like `NtSuspendProcess` on Windows) — happy to add
  that if it turns out to matter in practice.

## Dynamic subject tracking (experimental)

The "Follow subject automatically" toggle replaces the fixed crop offset
with a continuously panning crop that tracks the main detected subject
through the video. This is a much bigger feature than the static crop —
read this section before relying on it.

### How it works

1. A fast analysis pass decodes the source at low resolution/framerate
   (1 fps, 384×384) and runs a small ONNX **person** detector on each
   sampled frame — this happens once per source video, before any chunk
   cutting.
2. Detections are turned into a horizontal-position trajectory over
   time, gaps are filled by holding the last known position, outlier
   jumps are clamped, and the result is smoothed with a zero-phase
   (forward-backward) filter so the pan is smooth _and_ doesn't lag
   behind the subject's real position.
3. For each chunk, that trajectory is turned into a list of short
   segments — each with its own fixed crop x position — splitting only
   where the tracked position actually drifts meaningfully (a mostly
   still subject produces very few segments; real movement produces
   more, closely-spaced ones).
4. Those segments get stitched together in a single ffmpeg call using
   `trim` + `crop` + `concat` filters (one small crop per segment,
   concatenated back into one continuous output) — a step earlier than
   this used ffmpeg's `sendcmd` filter to change the crop position
   live mid-encode, but that turned out to not actually take effect on
   at least one real-world ffmpeg build despite being documented as
   supported, so it was replaced with this segment-based approach,
   which only relies on filters (`trim`/`crop`/`concat`) that have
   already been proven reliable elsewhere in this app.

Only horizontal position is ever tracked or moved. Height and vertical
position stay exactly as they are for the static crop (full source
height, always) — this feature only decides _where along the width_ the
crop window sits at each moment.

**v0.4.0: switched from a face detector to a person detector.** A face
detector loses the subject any time they look down, turn to address the
room, or angle away from front-on — extremely common in sermon-style
footage. A person's bounding box (head to at least mid-torso, usually
more) stays far more stable through all of that, and also directly gives
the "whole body centered, not just a floating face" framing.

### Required setup before this works

1. **Export the model.** This uses Ultralytics' YOLO26n — as of YOLO26,
   end-to-end NMS-free export is the _default_ (no `end2end=True` flag
   needed), so no manual box-decoding/NMS code is needed on the Rust
   side — that was the biggest source of bugs with a raw YOLO export.
   On a machine with Python:
   ```
   pip install ultralytics
   python -c "from ultralytics import YOLO; YOLO('yolo26n.pt').export(format='onnx', imgsz=384, opset=13)"
   ```
   This downloads the pretrained nano checkpoint and produces
   `yolo26n.onnx` in the current directory.
2. **Place it** at `src-tauri/resources/person_detector.onnx` (rename to
   exactly that).
3. `tauri.conf.json` already lists it under `bundle.resources`, so a
   normal `tauri build` packages it into the installer automatically.
4. **First build note:** the `ort` crate downloads a matching prebuilt
   ONNX Runtime binary during compilation — this needs internet access
   on the machine doing the build (not the end user's machine; that
   binary gets bundled into your output).

### Things to verify once you can actually run it

Same caveat as before: I wrote this against Ultralytics' documented
end2end export format, but couldn't run the export or the resulting
inference myself. Worth checking on your first run:

- **Output shape/name.** Code assumes a single output tensor shaped
  `(1, max_detections, 6)` = `[x1, y1, x2, y2, confidence, class_id]`,
  and an input named `"images"`. If detection looks wrong or errors on
  the tensor shape, open the model in [Netron](https://netron.app) and
  confirm both against what's actually in `tracking.rs`.
- **Coordinate space.** Assumed to be pixel coordinates in the model's
  384×384 input space (hence dividing by `MODEL_SIZE` to normalize) —
  some export configurations output normalized 0..1 coordinates
  instead, which would need that division removed.
- **Tuning constants** live at the top of `tracking.rs`:
  - `SCORE_THRESHOLD` (0.5) — how confident a detection must be to count
  - `MAX_SPEED_PER_SEC` (0.6) — how fast the crop is allowed to pan, as
    a fraction of frame width per second
  - `SEGMENT_PIXEL_THRESHOLD` (12px) — how far the tracked position must
    drift before a new crop segment starts (lower = follows more
    precisely but creates more segments/cuts; higher = smoother but
    coarser following)
  - `MAX_SEGMENT_SECS` (4.0) — forces a new segment at least this often
    even if the subject barely moves
  - `EMA_ALPHA` (0.35) — smoothing strength (applied both directions, so
    this affects smoothness, not lag — lag is already eliminated)
  - `SAMPLE_FPS` (1.0) — how often the analysis pass samples frames
  - `MODEL_SIZE` (384) — bump to 640 (standard YOLO input size, re-export
    with `imgsz=640`) if tracking accuracy still isn't good enough and
    you have processing time to spare; this is the main quality/speed
    trade-off lever now that the model itself has more headroom than
    the old face detector did.

### Performance

This adds a full analysis decode pass per source video on top of the
existing crop re-encode. A 384×384 person detector is meaningfully more
expensive per frame than the old 320×240 face detector was — expect
analysis to take longer than it did before. `MODEL_SIZE` and
`SAMPLE_FPS` are the two levers to trade accuracy for speed if it's too
slow for your real workloads.

### v0.4.x refinements: smoother panning + safe top trim

**Smoother panning.** The segment threshold that decides when a new crop
position starts was tightened significantly (12px → 4px). The underlying
tracked position is already smooth (zero-phase filtered), so a tight
threshold just traces that smooth curve with many small steps instead of
collapsing it into a few large, abrupt jumps — reads as continuous
panning rather than jump-cuts. `SEGMENT_PIXEL_THRESHOLD` in `tracking.rs`
is the dial: lower for even smoother (more segments, slightly heavier
encode), higher for fewer/coarser jumps.

**Safe top trim.** Dynamic tracking mode now also trims a modest amount
off the _top_ of the frame — the height that was previously always the
full source height can now be slightly less. This is calculated once per
video, not per frame:

1. Find the highest position the subject's head ever reaches across the
   whole video (the _raw_, unsmoothed data — deliberately not the
   smoothed trajectory, so a single brief moment of an unusually high
   head position still counts and stays protected).
2. Keep a safety margin above that (`TOP_CROP_SAFETY_MARGIN`, default 8%
   of source height) — the crop line never gets closer to the head than
   this.
3. Cap the total trim at `TOP_CROP_MAX_FRACTION` (default 18% of source
   height) so this stays a modest trim, not a redesign of the framing.

This is a pure crop — no scaling — so it can't introduce any zoom. It
only applies in dynamic tracking mode, since that's the only mode with
head-position data available; plain static crop is unaffected.

### v0.4.x: genuinely continuous panning (not just fine steps)

Building on the segment approach above: each segment's crop `x` is now a
**linear ramp expression** (`x0 + (x1-x0)*t/duration`) toward the _next_
segment's position, rather than a flat constant. This uses ffmpeg's
native per-frame expression evaluation for `crop`'s x/y — long-standing
core filter functionality, not the runtime-command mechanism (`sendcmd`)
that turned out not to work. Since consecutive segments' end/start values
match exactly, the ramps connect with zero visible jump at the seams —
the result is truly continuous motion, not a step function with very
small steps. Segment boundaries (still governed by
`SEGMENT_PIXEL_THRESHOLD`) now only decide how often the _ramp target_
updates, not whether motion is smooth.

### Fix: "filename or extension is too long" (OS error 206)

A chunk with a lot of subject movement can generate hundreds of crop
segments, and the resulting `-filter_complex` string could exceed
Windows' process command-line length limit — Windows surfaces that as
"The filename or extension is too long," which is a misleading message
for what's actually a command-line-length problem, not an issue with any
actual filename.

First attempt at fixing this was writing the filter graph to a file and
using `-filter_complex_script` instead of inlining it — that option has
no length limit at all, but turned out to be rejected as "unrecognized"
on at least one real ffmpeg nightly build (possibly caught mid-way
through ffmpeg's ongoing internal CLI rewrite), so it wasn't safe to
depend on.

**Actual fix:** `MAX_SEGMENTS_PER_CHUNK` in `tracking.rs` (default 220)
caps how many segments a single chunk can ever produce. If the natural
(finest) segmentation from `SEGMENT_PIXEL_THRESHOLD` would exceed that,
the threshold is progressively coarsened just for that chunk until it
fits — this guarantees the resulting `-filter_complex` string stays a
safe, bounded length no matter how much the subject moves, without
depending on any ffmpeg option beyond the core `-filter_complex` that's
been reliable throughout this whole feature.
