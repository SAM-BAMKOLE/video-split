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
This app only ever calls `-c copy` — it never encodes anything — so you
don't need libx264/libx265/etc. Use an **LGPL** ffmpeg build
(`win64-lgpl` on the BtbN page), not the GPL one. LGPL lets you bundle
the binary in a closed-source/commercial app without your own app
becoming subject to GPL. If you ever add re-encoding features that
require GPL-only codecs, revisit this. Either way, include ffmpeg's
license file (`LICENSE.txt` from the build) somewhere in your app's
"About" screen — it's a redistribution requirement even under LGPL.

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
