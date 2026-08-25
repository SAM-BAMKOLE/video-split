//! Dynamic subject tracking for the crop feature.
//!
//! Pipeline: sample low-res frames -> run a tiny ONNX face detector on each
//! -> build a raw horizontal-position trajectory over time -> reject
//! outliers and smooth it -> write an ffmpeg `sendcmd` script that pans the
//! crop window to follow the subject.
//!
//! Only horizontal position is ever tracked or moved — height and vertical
//! position are always fixed to the full source frame, matching the
//! "never crop vertically" rule the static crop already follows.

use ndarray::Array4;
use ort::session::Session;
use ort::value::Value;
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

const MODEL_W: usize = 320;
const MODEL_H: usize = 240;
const SAMPLE_FPS: f64 = 1.0; // frames analyzed per second of source video
const SCORE_THRESHOLD: f32 = 0.7;
const NMS_IOU_THRESHOLD: f32 = 0.3;

#[derive(Clone, Copy, Debug)]
struct FaceBox {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
}

impl FaceBox {
    fn area(&self) -> f32 {
        (self.x2 - self.x1).max(0.0) * (self.y2 - self.y1).max(0.0)
    }
    fn center_x(&self) -> f32 {
        (self.x1 + self.x2) / 2.0
    }
}

fn iou(a: &FaceBox, b: &FaceBox) -> f32 {
    let ix1 = a.x1.max(b.x1);
    let iy1 = a.y1.max(b.y1);
    let ix2 = a.x2.min(b.x2);
    let iy2 = a.y2.min(b.y2);
    let inter = (ix2 - ix1).max(0.0) * (iy2 - iy1).max(0.0);
    let union = a.area() + b.area() - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

fn nms(mut boxes: Vec<FaceBox>) -> Vec<FaceBox> {
    boxes.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let mut kept: Vec<FaceBox> = Vec::new();
    for b in boxes {
        if kept.iter().all(|k| iou(k, &b) < NMS_IOU_THRESHOLD) {
            kept.push(b);
        }
    }
    kept
}

/// One sampled point: seconds into the source video, normalized (0..1)
/// horizontal center of the chosen subject. `None` x means no face was
/// found in that frame.
#[derive(Clone, Copy, Debug)]
pub struct TrackPoint {
    pub t: f64,
    pub x_norm: Option<f64>,
}

/// Run the ONNX face detector on one RGB24 frame already resized to
/// MODEL_W x MODEL_H. Returns the largest above-threshold face, if any.
fn detect_main_face(session: &mut Session, rgb: &[u8]) -> Result<Option<FaceBox>, String> {
    // rgb is MODEL_H * MODEL_W * 3 bytes, HWC, values 0-255.
    // Build the CHW-normalized tensor data as one flat pass over the pixel
    // buffer instead of per-element indexed writes into the ndarray — this
    // avoids repeated bounds-checked multi-dim indexing (Array4's [[..]]
    // operator), which matters a lot for a loop run tens of thousands of
    // times per source video.
    let plane_size = MODEL_W * MODEL_H;
    let mut chw = vec![0f32; 3 * plane_size];
    for i in 0..plane_size {
        let px = i * 3;
        chw[i] = (rgb[px] as f32 - 127.0) / 128.0; // R plane
        chw[plane_size + i] = (rgb[px + 1] as f32 - 127.0) / 128.0; // G plane
        chw[2 * plane_size + i] = (rgb[px + 2] as f32 - 127.0) / 128.0; // B plane
    }
    let tensor = Array4::from_shape_vec((1, 3, MODEL_H, MODEL_W), chw)
        .map_err(|e| e.to_string())?;

    let input = Value::from_array(tensor).map_err(|e| e.to_string())?;
    let outputs = session
        .run(ort::inputs!["input" => input])
        .map_err(|e| e.to_string())?;

    // NOTE: output tensor order/names come from the exported UltraFace
    // ONNX graph (typically "scores" then "boxes", already NMS-ready
    // in normalized 0..1 coordinates). If your exported model differs,
    // check the actual output names with Netron and adjust the two
    // indices below.
    let scores = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;
    let boxes = outputs[1]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;

    let (scores_shape, scores_data) = scores;
    let (_boxes_shape, boxes_data) = boxes;
    let num_priors = scores_shape[1] as usize;

    let mut candidates = Vec::new();
    for i in 0..num_priors {
        let face_score = scores_data[i * 2 + 1]; // index 1 = face class
        if face_score >= SCORE_THRESHOLD {
            candidates.push(FaceBox {
                x1: boxes_data[i * 4],
                y1: boxes_data[i * 4 + 1],
                x2: boxes_data[i * 4 + 2],
                y2: boxes_data[i * 4 + 3],
                score: face_score,
            });
        }
    }

    let kept = nms(candidates);
    // Largest face = assumed main subject (closest/most prominent speaker).
    Ok(kept.into_iter().max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap()))
}

/// Decode the source video at a low sample rate/resolution and run face
/// detection on each sampled frame, returning a raw (possibly gappy)
/// trajectory of normalized horizontal subject position over time.
pub async fn sample_and_track(
    app: &AppHandle,
    input: &Path,
    model_path: &Path,
    duration_secs: f64,
    mut on_progress: impl FnMut(f64), // fraction 0.0..1.0
) -> Result<Vec<TrackPoint>, String> {
    let mut session = Session::builder()
        .map_err(|e| e.to_string())?
        .commit_from_file(model_path)
        .map_err(|e| e.to_string())?;

    let sidecar = app.shell().sidecar("ffmpeg").map_err(|e| e.to_string())?;
    let (mut rx, _child) = sidecar
        .args([
            "-i", input.to_str().ok_or("Invalid input path")?,
            "-vf", &format!("fps={SAMPLE_FPS},scale={MODEL_W}:{MODEL_H}"),
            "-f", "rawvideo",
            "-pix_fmt", "rgb24",
            "-",
        ])
        .spawn()
        .map_err(|e| e.to_string())?;

    let frame_bytes = MODEL_W * MODEL_H * 3;
    let mut buffer: Vec<u8> = Vec::with_capacity(frame_bytes * 4);
    let mut points: Vec<TrackPoint> = Vec::new();
    let mut frame_index: usize = 0;
    let total_frames_estimate = (duration_secs * SAMPLE_FPS).max(1.0);
    let mut last_reported = 0.0;

    while let Some(event) = rx.recv().await {
        if let CommandEvent::Stdout(chunk) = event {
            buffer.extend_from_slice(&chunk);
            while buffer.len() >= frame_bytes {
                let frame: Vec<u8> = buffer.drain(0..frame_bytes).collect();
                let t = frame_index as f64 / SAMPLE_FPS;
                if t > duration_secs {
                    break;
                }
                let face = detect_main_face(&mut session, &frame)?;
                points.push(TrackPoint {
                    t,
                    x_norm: face.map(|f| f.center_x() as f64),
                });
                frame_index += 1;

                let progress = (frame_index as f64 / total_frames_estimate).min(1.0);
                if progress - last_reported >= 0.02 {
                    on_progress(progress);
                    last_reported = progress;
                }
            }
        }
    }

    on_progress(1.0);
    Ok(points)
}

/// Fill gaps (hold last known position), reject sudden outlier jumps, then
/// apply zero-phase (forward-backward) smoothing so the pan is smooth
/// without lagging behind the subject's actual movement.
///
/// A plain forward-only EMA is a *causal* filter — appropriate for live
/// streaming, where you can't know the future. We're not live: the whole
/// trajectory is known before any chunk gets cut, so running the EMA
/// forward and then backward over its own output cancels the phase delay
/// entirely (the standard "filtfilt" technique), instead of the crop
/// visibly catching up late to where the subject already is.
pub fn smooth_trajectory(raw: &[TrackPoint]) -> Vec<TrackPoint> {
    if raw.is_empty() {
        return Vec::new();
    }

    // 1. Fill gaps by holding the last known value (default to center 0.5
    //    if nothing has been seen yet).
    let mut filled: Vec<f64> = Vec::with_capacity(raw.len());
    let mut last = 0.5;
    for p in raw {
        last = p.x_norm.unwrap_or(last);
        filled.push(last);
    }

    // 2. Reject outliers: if a point jumps further than a plausible max
    //    pan speed would allow since the previous value, clamp it instead
    //    of following it — handles a stray misdetection elsewhere in
    //    frame without producing a whip-pan. This is intentionally
    //    generous since real subject movement (walking across a stage)
    //    should still be allowed through.
    const MAX_SPEED_PER_SEC: f64 = 0.6; // fraction of frame width per second
    let mut clamped: Vec<f64> = Vec::with_capacity(filled.len());
    let mut prev = filled[0];
    for (i, &v) in filled.iter().enumerate() {
        let dt = if i == 0 { 0.0 } else { raw[i].t - raw[i - 1].t };
        let max_delta = MAX_SPEED_PER_SEC * dt.max(1.0 / SAMPLE_FPS);
        let delta = (v - prev).clamp(-max_delta, max_delta);
        prev += delta;
        clamped.push(prev);
    }

    // 3. Zero-phase smoothing: EMA forward, then EMA backward over the
    //    forward pass's output. Each pass alone lags in its own
    //    direction; averaging both directions cancels the lag while
    //    still suppressing jitter.
    const EMA_ALPHA: f64 = 0.35;
    let mut forward = Vec::with_capacity(clamped.len());
    let mut ema = clamped[0];
    for &v in &clamped {
        ema = EMA_ALPHA * v + (1.0 - EMA_ALPHA) * ema;
        forward.push(ema);
    }
    let mut smoothed = vec![0.0; forward.len()];
    let mut ema_back = *forward.last().unwrap();
    for i in (0..forward.len()).rev() {
        ema_back = EMA_ALPHA * forward[i] + (1.0 - EMA_ALPHA) * ema_back;
        smoothed[i] = ema_back;
    }

    raw.iter()
        .zip(smoothed)
        .map(|(p, x)| TrackPoint { t: p.t, x_norm: Some(x) })
        .collect()
}

/// How often to emit a crop-position command, in seconds. This is
/// independent of SAMPLE_FPS (how often we actually detect the subject) —
/// `sendcmd` jumps to each new value instantly rather than interpolating,
/// so writing one command per raw detection (e.g. once a second) makes the
/// pan visibly teleport. Interpolating between the smoothed trajectory
/// points at a much finer step makes the motion read as continuous.
const INTERP_STEP_SECS: f64 = 0.1;

/// Slice a full-video trajectory down to one chunk's time range and
/// re-base timestamps to start at 0 (matching how -ss resets PTS for the
/// cut chunk), then write an ffmpeg `sendcmd` script that pans a filter
/// named `trackcrop`'s `x` parameter to follow the smoothed trajectory.
/// The trajectory itself is only known at SAMPLE_FPS resolution, so this
/// linearly interpolates between consecutive points to produce a much
/// denser command list — that's what makes the resulting pan look like a
/// continuous follow rather than a once-a-second jump.
///
/// Returns the initial x pixel offset (for the filter's starting value)
/// and the path to the generated command file.
pub fn write_sendcmd_for_chunk(
    full_trajectory: &[TrackPoint],
    chunk_start: f64,
    chunk_len: f64,
    src_w: u32,
    out_w: u32,
    cmd_path: &PathBuf,
) -> Result<u32, String> {
    let max_x = src_w.saturating_sub(out_w) as f64;

    let x_norm_at = |abs_t: f64| -> f64 {
        if full_trajectory.is_empty() {
            return 0.5;
        }
        let raw_idx = abs_t * SAMPLE_FPS;
        let i0 = (raw_idx.floor() as isize).clamp(0, full_trajectory.len() as isize - 1) as usize;
        let i1 = (i0 + 1).min(full_trajectory.len() - 1);
        let frac = (raw_idx - i0 as f64).clamp(0.0, 1.0);
        let x0 = full_trajectory[i0].x_norm.unwrap_or(0.5);
        let x1 = full_trajectory[i1].x_norm.unwrap_or(x0);
        x0 + (x1 - x0) * frac
    };

    let mut lines = String::new();
    let mut first_x: Option<u32> = None;

    let mut local_t = 0.0;
    while local_t <= chunk_len {
        let abs_t = chunk_start + local_t;
        let x_norm = x_norm_at(abs_t);
        let center_px = x_norm * src_w as f64;
        let x_px = (center_px - out_w as f64 / 2.0).clamp(0.0, max_x).round() as u32;
        if first_x.is_none() {
            first_x = Some(x_px);
        }
        lines.push_str(&format!("{local_t:.3} trackcrop x {x_px};\n"));
        local_t += INTERP_STEP_SECS;
    }

    let initial_x = first_x.unwrap_or((max_x / 2.0).round() as u32);
    if lines.is_empty() {
        lines.push_str(&format!("0.0 trackcrop x {initial_x};\n"));
    }

    std::fs::write(cmd_path, lines).map_err(|e| e.to_string())?;
    Ok(initial_x)
}