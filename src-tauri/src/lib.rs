mod tracking;

use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "flv"];

#[derive(Default)]
struct JobControl {
    pause_requested: AtomicBool,
    stop_requested: AtomicBool,
    /// Handle to whichever ffmpeg process is currently running, so Stop can
    /// kill it immediately instead of waiting for it to finish.
    current_child: Mutex<Option<CommandChild>>,
}

struct AppState {
    control: Arc<JobControl>,
}

#[derive(Clone, Copy)]
struct CropOpts {
    ratio_w: f64,
    ratio_h: f64,
    /// -100 (fully left) .. 0 (center) .. 100 (fully right)
    offset_percent: f64,
    dynamic_tracking: bool,
}

/// A resolved pixel crop rect for one source video: (out_w, out_h, x, y)
type CropRect = (u32, u32, u32, u32);

enum CropMode {
    None,
    /// Fixed crop window for the whole chunk.
    Static(CropRect),
    /// Crop window that pans over time to follow the tracked subject, built
    /// from short trim+crop segments stitched together with ffmpeg's
    /// concat filter (more reliable across ffmpeg builds than sendcmd,
    /// which some builds don't actually honor for runtime crop changes).
    Dynamic {
        w: u32,
        h: u32,
        /// Pixels trimmed off the top (see compute_safe_top_crop) — 0
        /// when there's no safe margin to trim, keeping the previous
        /// full-height behavior.
        y_offset: u32,
        segments: Vec<tracking::CropSegment>,
    },
}

#[derive(Clone, Serialize)]
struct ProgressPayload {
    file_name: String,
    file_index: usize,
    file_count: usize,
    chunk_index: usize,
    chunk_count: usize,
    status: String, // "running" | "paused" | "done" | "stopped" | "error"
    message: String,
}

fn emit_progress(app: &AppHandle, payload: ProgressPayload) {
    let _ = app.emit("split-progress", payload);
}

/// Turn the user's format choice into a concrete file extension.
/// "source" (or anything unrecognized) keeps the original container.
fn resolve_ext(source_ext: &str, output_format: &str) -> String {
    match output_format {
        "mp4" => "mp4".to_string(),
        "mov" => "mov".to_string(),
        "mkv" => "mkv".to_string(),
        _ => source_ext.to_string(),
    }
}

async fn get_duration_seconds(app: &AppHandle, path: &Path) -> Result<f64, String> {
    let sidecar = app.shell().sidecar("ffprobe").map_err(|e| e.to_string())?;
    let output = sidecar
        .args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
            path.to_str().ok_or("Invalid path")?,
        ])
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse::<f64>()
        .map_err(|_| "Could not read video duration".to_string())
}

async fn get_video_dimensions(app: &AppHandle, path: &Path) -> Result<(u32, u32), String> {
    let sidecar = app.shell().sidecar("ffprobe").map_err(|e| e.to_string())?;
    let output = sidecar
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-of", "json",
            path.to_str().ok_or("Invalid path")?,
        ])
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let text = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let stream = json["streams"].get(0).ok_or("No video stream found")?;
    let width = stream["width"].as_u64().ok_or("Could not read width")? as u32;
    let height = stream["height"].as_u64().ok_or("Could not read height")? as u32;
    Ok((width, height))
}

/// Work out the pixel crop rect for a target aspect ratio + horizontal
/// offset. Always keeps the full source height and only ever narrows the
/// width — top/bottom are never touched, no matter the requested ratio.
fn compute_crop_rect(src_w: u32, src_h: u32, opts: CropOpts) -> CropRect {
    let target_ratio = opts.ratio_w / opts.ratio_h;

    let mut out_h = src_h;
    let mut out_w = ((out_h as f64) * target_ratio).round() as u32;
    out_w = out_w.min(src_w);

    // Most encoders need even dimensions for yuv420p.
    out_w -= out_w % 2;
    out_h -= out_h % 2;
    out_w = out_w.max(2);
    out_h = out_h.max(2);

    let max_x = src_w.saturating_sub(out_w) as f64;
    let center_x = max_x / 2.0;

    let shift = (opts.offset_percent.clamp(-100.0, 100.0) / 100.0) * center_x;
    let x = (center_x + shift).clamp(0.0, max_x).round() as u32;
    let y = 0; // full height always

    (out_w, out_h, x, y)
}

/// Escape a filesystem path for embedding inside an ffmpeg filter-graph
/// string. ffmpeg's filter parser treats `:` as an option separator and
/// `\` as its own escape character — a raw Windows path like
/// `C:\Users\...` breaks parsing (the drive-letter colon in particular).
/// Forward slashes work fine on Windows ffmpeg builds, so converting
/// separators avoids needing to escape every backslash too.
fn find_video_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

/// Run one ffmpeg cut/remux: [start, start+len) seconds.
/// If `crop` is None: pure stream copy (fast, lossless, no re-encode) — the
/// output container is simply whatever extension `output` ends in, so this
/// doubles as a lossless format-only conversion when start=0/len=full length.
/// If `crop` is Some: crops video with libx264 while keeping audio as a
/// stream copy — only the video stream is touched, so sync is unaffected.
async fn cut_chunk(
    app: &AppHandle,
    input: &Path,
    output: &Path,
    start_secs: f64,
    len_secs: f64,
    crop: &CropMode,
    control: &JobControl,
) -> Result<(), String> {
    let sidecar = app.shell().sidecar("ffmpeg").map_err(|e| e.to_string())?;

    let input_args: Vec<String> = vec![
        "-y".into(),
        "-ss".into(), start_secs.to_string(),
        "-i".into(), input.to_str().ok_or("Invalid input path")?.to_string(),
        "-t".into(), len_secs.to_string(),
    ];

    let target_ext = output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let is_mp4_family = target_ext == "mp4" || target_ext == "mov" || target_ext == "m4v";

    let mut filter_args: Vec<String> = Vec::new();
    let mut codec_args: Vec<String> = Vec::new();

    match crop {
        CropMode::Static((w, h, x, y)) => {
            filter_args.extend(["-vf".into(), format!("crop={w}:{h}:{x}:{y}")]);
            codec_args.extend([
                "-c:v".into(), "libx264".into(),
                "-preset".into(), "veryfast".into(),
                "-crf".into(), "20".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-c:a".into(), "copy".into(),
                "-map".into(), "0:v:0".into(),
                "-map".into(), "0:a:0?".into(),
            ]);
        }
        CropMode::Dynamic { w, h, y_offset, segments } => {
            // Built from short crop segments stitched with concat, rather
            // than sendcmd — some ffmpeg builds don't actually apply
            // sendcmd's runtime parameter changes despite crop being
            // documented as command-capable, so this sidesteps that
            // reliability gap. Each segment's crop x is a *linear ramp
            // expression* toward the next segment's position (using
            // ffmpeg's native per-frame expression evaluation for crop's
            // x/y — long-standing core functionality, not the runtime
            // command mechanism that turned out to be broken) rather than
            // a flat constant, so motion is genuinely continuous within
            // and across segments instead of a step function.
            let mut graph = String::new();
            for (i, seg) in segments.iter().enumerate() {
                let duration = (seg.end - seg.start).max(0.01);
                let next_x = segments.get(i + 1).map(|s| s.x).unwrap_or(seg.x);
                let x_expr = if next_x == seg.x {
                    seg.x.to_string()
                } else {
                    // Ramps from seg.x at t=0 to next_x at t=duration; t
                    // here is this segment's own elapsed time (PTS was
                    // just reset to 0 by setpts=PTS-STARTPTS above).
                    format!("{}+({}-{})*t/{duration:.3}", seg.x, next_x, seg.x)
                };
                graph.push_str(&format!(
                    "[0:v]trim=start={:.3}:end={:.3},setpts=PTS-STARTPTS,crop={w}:{h}:{x_expr}:{y_offset}[v{i}];",
                    seg.start, seg.end
                ));
            }
            let labels: String = (0..segments.len()).map(|i| format!("[v{i}]")).collect();
            graph.push_str(&format!("{labels}concat=n={}:v=1:a=0[vout]", segments.len()));

            // -filter_complex_script (reading the graph from a file, to
            // avoid any command-line length limit) turned out to be
            // rejected as an unrecognized option on at least one recent
            // ffmpeg nightly build, so this passes the graph inline
            // instead — tracking.rs now caps segment count per chunk
            // (MAX_SEGMENTS_PER_CHUNK) specifically so this string can
            // never grow long enough to hit Windows' process command-line
            // length limit (which surfaces as the misleading "filename or
            // extension too long" / OS error 206) in the first place.
            filter_args.extend(["-filter_complex".into(), graph]);

            codec_args.extend([
                "-c:v".into(), "libx264".into(),
                "-preset".into(), "veryfast".into(),
                "-crf".into(), "20".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-c:a".into(), "copy".into(),
                "-map".into(), "[vout]".into(),
                "-map".into(), "0:a:0?".into(),
            ]);
        }
        CropMode::None => {
            codec_args.extend(["-c".into(), "copy".into()]);
            if is_mp4_family {
                // MP4/MOV can't hold arbitrary subtitle/attachment/data
                // streams that source containers like MKV often carry —
                // mapping only video+audio avoids producing a file that's
                // broken or gets rejected by strict mobile importers.
                codec_args.extend([
                    "-map".into(), "0:v:0".into(),
                    "-map".into(), "0:a:0?".into(),
                ]);
            } else {
                codec_args.extend(["-map".into(), "0".into()]);
            }
        }
    }

    if is_mp4_family {
        // Puts the metadata (moov atom) at the front of the file instead of
        // the end — several mobile apps (CapCut on iOS included) are picky
        // about this and can fail to import an otherwise-valid MP4 without it.
        codec_args.extend(["-movflags".into(), "+faststart".into()]);
    }

    let mut args = input_args;
    args.extend(filter_args);
    args.extend(codec_args);
    args.push(output.to_str().ok_or("Invalid output path")?.to_string());

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (mut rx, child) = sidecar.args(arg_refs).spawn().map_err(|e| e.to_string())?;

    {
        let mut guard = control.current_child.lock().map_err(|e| e.to_string())?;
        *guard = Some(child);
    }

    let mut stderr_full = String::new();
    let mut outcome: Result<(), String> = Ok(());
    while let Some(event) = rx.recv().await {
        match event {
            CommandEvent::Stderr(bytes) => {
                stderr_full.push_str(&String::from_utf8_lossy(&bytes));
            }
            CommandEvent::Error(err) => {
                outcome = Err(err);
                break;
            }
            CommandEvent::Terminated(payload) => {
                if payload.code != Some(0) {
                    // Keep the last ~3000 chars — ffmpeg's actual specific
                    // error (which filter/option failed) is usually near
                    // the end, but a single last line was cutting it off.
                    let tail = if stderr_full.len() > 3000 {
                        &stderr_full[stderr_full.len() - 3000..]
                    } else {
                        &stderr_full
                    };
                    outcome = Err(format!("ffmpeg exited with error: {tail}"));
                }
                break;
            }
            _ => {}
        }
    }

    if let Ok(mut guard) = control.current_child.lock() {
        guard.take();
    }

    if outcome.is_err() && control.stop_requested.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(output);
        return Ok(());
    }

    outcome
}

async fn split_one_file(
    app: &AppHandle,
    input: &Path,
    output_root: &Path,
    chunk_minutes: f64,
    crop_opts: Option<CropOpts>,
    output_format: &str,
    file_index: usize,
    file_count: usize,
    control: &JobControl,
) -> Result<(), String> {
    let file_name = input.file_name().unwrap().to_string_lossy().to_string();
    let stem = input.file_stem().unwrap().to_string_lossy().to_string();
    let source_ext = input.extension().unwrap().to_string_lossy().to_string();
    let ext = resolve_ext(&source_ext, output_format);

    let dest_folder = output_root.join(&stem);
    std::fs::create_dir_all(&dest_folder).map_err(|e| e.to_string())?;

    let duration = get_duration_seconds(app, input).await?;
    let chunk_seconds = chunk_minutes * 60.0;
    let chunk_count = ((duration / chunk_seconds).ceil() as usize).max(1);

    // Static crop dimensions (w,h always the same regardless of offset —
    // only x moves), and for dynamic mode, a full-video smoothed subject
    // trajectory computed once up front, plus a safe top-crop offset.
    let mut static_crop_rect: Option<CropRect> = None;
    let mut dynamic_dims: Option<(u32, u32, u32)> = None; // (w, h, y_offset)
    let mut trajectory: Vec<tracking::TrackPoint> = Vec::new();

    if let Some(opts) = crop_opts {
        let (src_w, src_h) = get_video_dimensions(app, input).await?;
        if opts.dynamic_tracking {
            let model_path = app
                .path()
                .resource_dir()
                .map_err(|e| e.to_string())?
                .join("resources")
                .join("person_detector.onnx");
            emit_progress(app, ProgressPayload {
                file_name: file_name.clone(), file_index, file_count,
                chunk_index: 0, chunk_count, status: "running".into(),
                message: "Analyzing subject movement… 0%".into(),
            });
            let file_name_for_progress = file_name.clone();
            let raw = tracking::sample_and_track(app, input, &model_path, duration, |frac| {
                emit_progress(app, ProgressPayload {
                    file_name: file_name_for_progress.clone(), file_index, file_count,
                    chunk_index: 0, chunk_count, status: "running".into(),
                    message: format!("Analyzing subject movement… {}%", (frac * 100.0).round() as u32),
                });
            }).await?;
            let y_offset = tracking::compute_safe_top_crop(&raw, src_h);
            trajectory = tracking::smooth_trajectory(&raw);
            let (w, h, _, _) = compute_crop_rect(src_w, src_h, opts);
            // Trim the safe amount off the top — never scaled, just a
            // smaller height starting further down.
            let adjusted_h = h.saturating_sub(y_offset);
            dynamic_dims = Some((w, adjusted_h, y_offset));
        } else {
            static_crop_rect = Some(compute_crop_rect(src_w, src_h, opts));
        }
    }

    for i in 0..chunk_count {
        if control.stop_requested.load(Ordering::SeqCst) {
            emit_progress(app, ProgressPayload {
                file_name: file_name.clone(), file_index, file_count,
                chunk_index: i, chunk_count, status: "stopped".into(),
                message: "Stopped by user".into(),
            });
            return Ok(());
        }

        while control.pause_requested.load(Ordering::SeqCst) {
            emit_progress(app, ProgressPayload {
                file_name: file_name.clone(), file_index, file_count,
                chunk_index: i, chunk_count, status: "paused".into(),
                message: "Paused".into(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if control.stop_requested.load(Ordering::SeqCst) {
                return Ok(());
            }
        }

        let start = i as f64 * chunk_seconds;
        let this_len = (duration - start).min(chunk_seconds);
        let out_path = dest_folder.join(format!("{stem}_{:03}.{ext}", i + 1));

        let crop_mode: CropMode = if let Some((w, h, y_offset)) = dynamic_dims {
            let (src_w, _src_h) = get_video_dimensions(app, input).await?;
            let segments = tracking::build_crop_segments(&trajectory, start, this_len, src_w, w);
            CropMode::Dynamic { w, h, y_offset, segments }
        } else if let Some(rect) = static_crop_rect {
            CropMode::Static(rect)
        } else {
            CropMode::None
        };

        emit_progress(app, ProgressPayload {
            file_name: file_name.clone(), file_index, file_count,
            chunk_index: i, chunk_count, status: "running".into(),
            message: match crop_mode {
                CropMode::Dynamic { .. } => format!("Tracking + cutting chunk {}/{}", i + 1, chunk_count),
                CropMode::Static(_) => format!("Cropping + cutting chunk {}/{}", i + 1, chunk_count),
                CropMode::None => format!("Cutting chunk {}/{}", i + 1, chunk_count),
            },
        });

        cut_chunk(app, input, &out_path, start, this_len, &crop_mode, control).await?;

        if control.stop_requested.load(Ordering::SeqCst) {
            emit_progress(app, ProgressPayload {
                file_name: file_name.clone(), file_index, file_count,
                chunk_index: i, chunk_count, status: "stopped".into(),
                message: "Stopped by user".into(),
            });
            return Ok(());
        }
    }

    emit_progress(app, ProgressPayload {
        file_name, file_index, file_count,
        chunk_index: chunk_count, chunk_count, status: "done".into(),
        message: "File complete".into(),
    });
    Ok(())
}

/// Whole-file format conversion: no chunking, no crop, just a container
/// remux (stream copy — lossless) into the chosen extension.
async fn convert_one_file(
    app: &AppHandle,
    input: &Path,
    output_root: &Path,
    output_format: &str,
    file_index: usize,
    file_count: usize,
    control: &JobControl,
) -> Result<(), String> {
    let file_name = input.file_name().unwrap().to_string_lossy().to_string();
    let stem = input.file_stem().unwrap().to_string_lossy().to_string();
    let source_ext = input.extension().unwrap().to_string_lossy().to_string();
    let ext = resolve_ext(&source_ext, output_format);

    std::fs::create_dir_all(output_root).map_err(|e| e.to_string())?;

    let mut out_path = output_root.join(format!("{stem}.{ext}"));
    // Never silently overwrite the source file (e.g. same folder + same
    // extension because "source" format was picked with no real change).
    if out_path == input {
        out_path = output_root.join(format!("{stem}_converted.{ext}"));
    }

    if control.stop_requested.load(Ordering::SeqCst) {
        emit_progress(app, ProgressPayload {
            file_name, file_index, file_count,
            chunk_index: 0, chunk_count: 1, status: "stopped".into(),
            message: "Stopped by user".into(),
        });
        return Ok(());
    }
    while control.pause_requested.load(Ordering::SeqCst) {
        emit_progress(app, ProgressPayload {
            file_name: file_name.clone(), file_index, file_count,
            chunk_index: 0, chunk_count: 1, status: "paused".into(),
            message: "Paused".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if control.stop_requested.load(Ordering::SeqCst) {
            return Ok(());
        }
    }

    let duration = get_duration_seconds(app, input).await?;

    emit_progress(app, ProgressPayload {
        file_name: file_name.clone(), file_index, file_count,
        chunk_index: 0, chunk_count: 1, status: "running".into(),
        message: format!("Converting to .{ext}"),
    });

    cut_chunk(app, input, &out_path, 0.0, duration, &CropMode::None, control).await?;

    if control.stop_requested.load(Ordering::SeqCst) {
        emit_progress(app, ProgressPayload {
            file_name, file_index, file_count,
            chunk_index: 0, chunk_count: 1, status: "stopped".into(),
            message: "Stopped by user".into(),
        });
        return Ok(());
    }

    emit_progress(app, ProgressPayload {
        file_name, file_index, file_count,
        chunk_index: 1, chunk_count: 1, status: "done".into(),
        message: "File complete".into(),
    });
    Ok(())
}

fn resolve_input_and_output(
    input_path: &str,
    output_path: Option<String>,
) -> Result<(PathBuf, Vec<PathBuf>, PathBuf), String> {
    let input = PathBuf::from(input_path);
    if !input.exists() {
        return Err("Input path does not exist".into());
    }

    let output_root = match output_path {
        Some(p) => PathBuf::from(p),
        None => {
            if input.is_file() {
                input.parent().unwrap_or(Path::new(".")).to_path_buf()
            } else {
                input.clone()
            }
        }
    };
    std::fs::create_dir_all(&output_root).map_err(|e| e.to_string())?;

    let files: Vec<PathBuf> = if input.is_file() {
        vec![input.clone()]
    } else {
        find_video_files(&input)
    };
    if files.is_empty() {
        return Err("No video files found".into());
    }

    Ok((input, files, output_root))
}

#[tauri::command]
async fn start_split(
    app: AppHandle,
    state: State<'_, AppState>,
    input_path: String,
    output_path: Option<String>,
    chunk_minutes: f64,
    crop_enabled: bool,
    crop_ratio_w: f64,
    crop_ratio_h: f64,
    crop_offset_percent: f64,
    dynamic_tracking: bool,
    output_format: String,
) -> Result<(), String> {
    let control = state.control.clone();
    control.stop_requested.store(false, Ordering::SeqCst);
    control.pause_requested.store(false, Ordering::SeqCst);

    let (_input, files, output_root) = resolve_input_and_output(&input_path, output_path)?;

    let crop_opts = if crop_enabled {
        Some(CropOpts {
            ratio_w: crop_ratio_w,
            ratio_h: crop_ratio_h,
            offset_percent: crop_offset_percent,
            dynamic_tracking,
        })
    } else {
        None
    };

    tauri::async_runtime::spawn(async move {
        let count = files.len();
        for (idx, file) in files.iter().enumerate() {
            if control.stop_requested.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = split_one_file(
                &app, file, &output_root, chunk_minutes, crop_opts, &output_format, idx, count, &control,
            )
            .await
            {
                let _ = app.emit("split-progress", ProgressPayload {
                    file_name: file.file_name().unwrap().to_string_lossy().to_string(),
                    file_index: idx, file_count: count,
                    chunk_index: 0, chunk_count: 0, status: "error".into(),
                    message: e,
                });
            }
        }
    });

    Ok(())
}

#[tauri::command]
async fn convert_files(
    app: AppHandle,
    state: State<'_, AppState>,
    input_path: String,
    output_path: Option<String>,
    output_format: String,
) -> Result<(), String> {
    let control = state.control.clone();
    control.stop_requested.store(false, Ordering::SeqCst);
    control.pause_requested.store(false, Ordering::SeqCst);

    let (_input, files, output_root) = resolve_input_and_output(&input_path, output_path)?;

    tauri::async_runtime::spawn(async move {
        let count = files.len();
        for (idx, file) in files.iter().enumerate() {
            if control.stop_requested.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = convert_one_file(
                &app, file, &output_root, &output_format, idx, count, &control,
            )
            .await
            {
                let _ = app.emit("split-progress", ProgressPayload {
                    file_name: file.file_name().unwrap().to_string_lossy().to_string(),
                    file_index: idx, file_count: count,
                    chunk_index: 0, chunk_count: 0, status: "error".into(),
                    message: e,
                });
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn pause_split(state: State<'_, AppState>) {
    state.control.pause_requested.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn resume_split(state: State<'_, AppState>) {
    state.control.pause_requested.store(false, Ordering::SeqCst);
}

#[tauri::command]
fn stop_split(state: State<'_, AppState>) {
    state.control.stop_requested.store(true, Ordering::SeqCst);
    state.control.pause_requested.store(false, Ordering::SeqCst);
    if let Ok(mut guard) = state.control.current_child.lock() {
        if let Some(child) = guard.take() {
            let _ = child.kill();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { control: Arc::new(JobControl::default()) })
        .invoke_handler(tauri::generate_handler![
            start_split,
            convert_files,
            pause_split,
            resume_split,
            stop_split
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}