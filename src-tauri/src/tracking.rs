//! Dynamic subject tracking for the crop feature.
//!
//! Pipeline: sample low-res frames -> run a small ONNX *person* detector on
//! each -> build a raw horizontal-position trajectory over time -> reject
//! outliers and smooth it -> write an ffmpeg `sendcmd` script that pans the
//! crop window to follow the subject.
//!
//! v0.4.0 change: switched from a face-only detector to a person detector
//! (COCO class 0). A face detector loses the subject constantly on this
//! kind of footage — looking down at notes, turning to address the room,
//! any angle away from front-on — while a person's bounding box stays
//! stable through all of that, and naturally covers "abdomen upward"
//! framing rather than tracking a small floating face.
//!
//! Only horizontal position is ever tracked or moved — height and vertical
//! position are always fixed to the full source frame, matching the
//! "never crop vertically" rule the static crop already follows.

use ndarray::Array4;
use ort::session::Session;
use ort::value::Value;
use std::path::Path;
use tauri::AppHandle;
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

// Must be a multiple of 32 (YOLO architecture requirement). 384 is a
// middle ground between the tiny 320x240 face model's speed and full
// 640x640 YOLO accuracy — bump to 640 if tracking quality still isn't
// good enough and you have processing time to spare.
const MODEL_SIZE: usize = 384;
const SAMPLE_FPS: f64 = 1.0; // frames analyzed per second of source video
const SCORE_THRESHOLD: f32 = 0.5;
const COCO_PERSON_CLASS: f32 = 0.0;

#[derive(Clone, Copy, Debug)]
struct PersonBox {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

impl PersonBox {
    fn area(&self) -> f32 {
        (self.x2 - self.x1).max(0.0) * (self.y2 - self.y1).max(0.0)
    }
    fn center_x(&self) -> f32 {
        (self.x1 + self.x2) / 2.0
    }
}

/// One sampled point: seconds into the source video, normalized (0..1)
/// horizontal center of the chosen subject, and normalized (0..1) top
/// edge of their bounding box (used only for the safe top-crop margin —
/// horizontal tracking never uses this). `None` means no person was
/// found in that frame.
#[derive(Clone, Copy, Debug)]
pub struct TrackPoint {
    pub t: f64,
    pub x_norm: Option<f64>,
    pub y1_norm: Option<f64>,
}

/// Run the ONNX person detector (YOLO26n, exported with end2end=True so
/// NMS is already applied inside the model graph) on one RGB24 frame
/// already resized to MODEL_SIZE x MODEL_SIZE. Returns the largest
/// above-threshold person, if any.
fn detect_main_person(session: &mut Session, rgb: &[u8]) -> Result<Option<PersonBox>, String> {
    // rgb is MODEL_SIZE * MODEL_SIZE * 3 bytes, HWC, values 0-255.
    // Ultralytics preprocessing: RGB, CHW, normalized to 0..1 (divide by
    // 255) — no mean subtraction, unlike the previous face model.
    let plane_size = MODEL_SIZE * MODEL_SIZE;
    let mut chw = vec![0f32; 3 * plane_size];
    for i in 0..plane_size {
        let px = i * 3;
        chw[i] = rgb[px] as f32 / 255.0; // R plane
        chw[plane_size + i] = rgb[px + 1] as f32 / 255.0; // G plane
        chw[2 * plane_size + i] = rgb[px + 2] as f32 / 255.0; // B plane
    }
    let tensor = Array4::from_shape_vec((1, 3, MODEL_SIZE, MODEL_SIZE), chw)
        .map_err(|e| e.to_string())?;

    let input = Value::from_array(tensor).map_err(|e| e.to_string())?;
    let outputs = session
        .run(ort::inputs!["images" => input])
        .map_err(|e| e.to_string())?;

    // NOTE: an end2end-exported YOLO model outputs a single tensor shaped
    // (1, max_detections, 6) = [x1, y1, x2, y2, confidence, class_id],
    // already NMS-filtered, coordinates in the model's input pixel space
    // (0..MODEL_SIZE). If your export differs, check the actual output
    // name/shape with Netron and adjust here — the input name ("images")
    // is Ultralytics' standard, but double-check that too.
    let (shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
    let num_dets = shape[1] as usize;

    let mut best: Option<PersonBox> = None;
    for i in 0..num_dets {
        let base = i * 6;
        let confidence = data[base + 4];
        let class_id = data[base + 5];
        if confidence < SCORE_THRESHOLD || (class_id - COCO_PERSON_CLASS).abs() > 0.5 {
            continue;
        }
        let candidate = PersonBox {
            x1: data[base],
            y1: data[base + 1],
            x2: data[base + 2],
            y2: data[base + 3],
        };
        // Largest person = assumed main subject (closest/most prominent
        // speaker vs. anyone else visible in frame, e.g. a sign-language
        // interpreter or someone crossing the stage in the background).
        if best.map_or(true, |b: PersonBox| candidate.area() > b.area()) {
            best = Some(candidate);
        }
    }

    Ok(best)
}

/// Decode the source video at a low sample rate/resolution and run person
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
            "-vf", &format!("fps={SAMPLE_FPS},scale={MODEL_SIZE}:{MODEL_SIZE}"),
            "-f", "rawvideo",
            "-pix_fmt", "rgb24",
            "-",
        ])
        .spawn()
        .map_err(|e| e.to_string())?;

    let frame_bytes = MODEL_SIZE * MODEL_SIZE * 3;
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
                let person = detect_main_person(&mut session, &frame)?;
                points.push(TrackPoint {
                    t,
                    // This model's boxes are in pixel space (0..MODEL_SIZE),
                    // unlike the previous face model's pre-normalized
                    // output — normalize to a 0..1 fraction here.
                    x_norm: person.map(|p| (p.center_x() / MODEL_SIZE as f32) as f64),
                    y1_norm: person.map(|p| (p.y1 / MODEL_SIZE as f32) as f64),
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
        .map(|(p, x)| TrackPoint { t: p.t, x_norm: Some(x), y1_norm: p.y1_norm })
        .collect()
}

/// How often to emit a crop-position command, in seconds. This is
/// independent of SAMPLE_FPS (how often we actually detect the subject) —
/// `sendcmd` jumps to each new value instantly rather than interpolating,
/// so writing one command per raw detection (e.g. once a second) makes the
/// pan visibly teleport. Interpolating between the smoothed trajectory
/// points at a much finer step makes the motion read as continuous.
const INTERP_STEP_SECS: f64 = 0.1;

/// A new segment starts once the crop position has drifted this many
/// pixels from the current segment's position. The underlying trajectory
/// is already smooth (zero-phase filtered), so a tight threshold here
/// traces that smooth curve with many small steps — which reads as a
/// continuous, natural pan rather than a handful of abrupt jumps. Raise
/// this if you'd rather trade smoothness for fewer segments/faster
/// encoding; lower it for an even smoother (but heavier) pan.
const SEGMENT_PIXEL_THRESHOLD: f64 = 4.0;

/// Even if the subject barely moves, force a new segment at least this
/// often, so a very long still stretch doesn't collapse into one giant
/// segment (kept moderate since a tight pixel threshold above already
/// does most of the work during real movement).
const MAX_SEGMENT_SECS: f64 = 2.0;

/// How much clearance to keep above the highest observed head position,
/// as a fraction of source height — the crop line sits this far above
/// the head, never closer.
const TOP_CROP_SAFETY_MARGIN: f64 = 0.08;

/// Hard cap on how much can ever be trimmed off the top, as a fraction of
/// source height — keeps this a modest trim rather than a major reframe,
/// even if the subject spends the whole video near the bottom of frame.
const TOP_CROP_MAX_FRACTION: f64 = 0.18;

/// Work out how many pixels can safely be trimmed off the top of the
/// frame without ever cropping into the subject's head. Uses the RAW
/// (unsmoothed) trajectory deliberately — smoothing could average away a
/// single brief moment where the head reached unusually high (e.g. a
/// hand raised overhead moment or upright the head visibly moving up),
/// and this needs to stay safe even in that case, not just on average.
pub fn compute_safe_top_crop(raw: &[TrackPoint], src_h: u32) -> u32 {
    let min_y1_norm = raw
        .iter()
        .filter_map(|p| p.y1_norm)
        .fold(f64::INFINITY, f64::min);

    if !min_y1_norm.is_finite() {
        // No person ever detected — don't guess, trim nothing.
        return 0;
    }

    let highest_head_px = min_y1_norm * src_h as f64;
    let margin_px = TOP_CROP_SAFETY_MARGIN * src_h as f64;
    let safe_crop_px = (highest_head_px - margin_px).max(0.0);
    let max_allowed_px = TOP_CROP_MAX_FRACTION * src_h as f64;

    safe_crop_px.min(max_allowed_px).floor() as u32
}

/// Hard ceiling on segments per chunk, regardless of how much the subject
/// moves. Each segment adds roughly 90-110 characters to the ffmpeg
/// filter graph string; staying under this keeps that string comfortably
/// within Windows' process command-line length limit (which surfaces as
/// the misleading "filename or extension too long" / OS error 206 if
/// exceeded) even in the worst case of near-continuous fast movement.
const MAX_SEGMENTS_PER_CHUNK: usize = 220;

fn segment_with_threshold(
    x_px_at: &dyn Fn(f64) -> u32,
    chunk_len: f64,
    pixel_threshold: f64,
) -> Vec<CropSegment> {
    let mut segments: Vec<CropSegment> = Vec::new();
    let mut seg_start = 0.0;
    let mut seg_x = x_px_at(0.0);
    let mut local_t = INTERP_STEP_SECS;

    while local_t < chunk_len {
        let candidate_x = x_px_at(local_t);
        let drifted = (candidate_x as f64 - seg_x as f64).abs() >= pixel_threshold;
        let too_long = local_t - seg_start >= MAX_SEGMENT_SECS;
        if drifted || too_long {
            segments.push(CropSegment { start: seg_start, end: local_t, x: seg_x });
            seg_start = local_t;
            seg_x = candidate_x;
        }
        local_t += INTERP_STEP_SECS;
    }
    segments.push(CropSegment { start: seg_start, end: chunk_len, x: seg_x });
    segments
}

/// One piece of a chunk: [start, end) in chunk-local seconds, with a fixed
/// crop x position for that span.
pub struct CropSegment {
    pub start: f64,
    pub end: f64,
    pub x: u32,
}

/// Slice a full-video trajectory down to one chunk's time range and turn
/// it into a list of short segments, each with its own fixed crop x
/// position. Segments only split where the tracked position actually
/// drifts meaningfully, so a mostly-still subject produces few segments
/// while real movement produces more, closely-spaced ones — the caller
/// stitches these back together with ffmpeg's trim+crop+concat filters,
/// which (unlike `sendcmd`) reliably changes crop position mid-encode.
/// If the natural (finest) segmentation would exceed
/// `MAX_SEGMENTS_PER_CHUNK`, the threshold is progressively coarsened
/// until it fits — this guarantees a hard bound on the resulting filter
/// graph's size no matter how much the subject moves.
pub fn build_crop_segments(
    full_trajectory: &[TrackPoint],
    chunk_start: f64,
    chunk_len: f64,
    src_w: u32,
    out_w: u32,
) -> Vec<CropSegment> {
    let max_x = src_w.saturating_sub(out_w) as f64;
    if max_x < 1.0 {
        eprintln!(
            "[tracking] Warning: crop width ({out_w}px) leaves no margin to pan within source width ({src_w}px) — tracking will have no visible effect for this chunk. This usually means the source is already narrower than or equal to the target crop ratio (e.g. testing against an already-cropped file)."
        );
    }

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

    let x_px_at = move |local_t: f64| -> u32 {
        let x_norm = x_norm_at(chunk_start + local_t);
        let center_px = x_norm * src_w as f64;
        (center_px - out_w as f64 / 2.0).clamp(0.0, max_x).round() as u32
    };

    let mut threshold = SEGMENT_PIXEL_THRESHOLD;
    let mut segments = segment_with_threshold(&x_px_at, chunk_len, threshold);
    let mut attempts = 0;
    while segments.len() > MAX_SEGMENTS_PER_CHUNK && attempts < 20 {
        threshold *= 1.5;
        segments = segment_with_threshold(&x_px_at, chunk_len, threshold);
        attempts += 1;
    }
    if attempts > 0 {
        eprintln!(
            "[tracking] Coarsened crop segmentation ({} segments would have exceeded the safety cap) — threshold raised to {threshold:.1}px for this chunk to keep the filter graph a safe size.",
            MAX_SEGMENTS_PER_CHUNK
        );
    }

    segments
}