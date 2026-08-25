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
with a continuously panning crop that tracks the main detected face
through the video. This is a much bigger feature than the static crop —
read this section before relying on it.

### How it works
1. A fast analysis pass decodes the source at low resolution/framerate
   (2 fps, 320×240) and runs a tiny ONNX face detector on each sampled
   frame — this happens once per source video, before any chunk cutting.
2. Detections are turned into a horizontal-position trajectory over
   time, gaps are filled by holding the last known position, outlier
   jumps are clamped, and the result is smoothed (exponential moving
   average) so the pan looks natural rather than jittery.
3. For each chunk, that trajectory is sliced to the chunk's time range
   and written out as an ffmpeg `sendcmd` script — a list of
   `timestamp → crop x position` lines.
4. The actual chunk cut still happens in one native ffmpeg encode pass;
   `sendcmd` just updates the crop filter's `x` parameter live as it
   processes, so there's no separate frame-by-frame render step.

Only horizontal position is ever tracked or moved. Height and vertical
position stay exactly as they are for the static crop (full source
height, always) — this feature only decides *where along the width* the
crop window sits at each moment.

### Required setup before this works
1. **Get the model.** Download `version-RFB-320.onnx` from
   https://github.com/Linzaer/Ultra-Light-Fast-Generic-Face-Detector-1MB
   (MIT-licensed, ~1.5MB).
2. **Place it** at `src-tauri/resources/face_detector.onnx` (rename to
   exactly that).
3. `tauri.conf.json` already lists it under `bundle.resources`, so a
   normal `tauri build` packages it into the installer automatically.
4. **First build note:** the `ort` crate downloads a matching prebuilt
   ONNX Runtime binary during compilation — this needs internet access
   on the machine doing the build (not the end user's machine; that
   binary gets bundled into your output).

### Things to verify once you can actually run it
I wrote this against the well-documented public spec of this model and
of ffmpeg's `sendcmd` filter, but couldn't execute either in the
environment I built this in — a few specifics are worth a quick sanity
check on your first real run:
- **Output tensor order.** `tracking.rs` reads the model's two outputs
  by index (`outputs[0]` = scores, `outputs[1]` = boxes). If detection
  looks wrong, open the model in [Netron](https://netron.app) and
  confirm the actual output names/order match.
- **`sendcmd` targeting syntax.** The filter graph names the crop
  instance via `crop=...@trackcrop` and targets it in the command file
  as `trackcrop x <value>`. This is the standard documented pattern for
  runtime-adjustable filter options, but it's worth a quick one-file
  test before trusting it on a real batch.
- **Tuning constants** live at the top of `tracking.rs` if the result
  needs adjusting:
  - `SCORE_THRESHOLD` (0.7) — how confident a detection must be to count
  - `MAX_SPEED_PER_SEC` (0.35) — how fast the crop is allowed to pan,
    as a fraction of frame width per second (lower = calmer, slower to
    follow quick movement)
  - `EMA_ALPHA` (0.25) — smoothing strength (lower = smoother but more
    "lag" behind the actual subject position)
  - `SAMPLE_FPS` (2.0) — how often the analysis pass samples frames
    (higher = more responsive tracking, slower analysis pass)

### Performance
This adds a full analysis decode pass per source video on top of the
existing crop re-encode, so total processing time per video will be
noticeably longer than either plain splitting or the static crop.
