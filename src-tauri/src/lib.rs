use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "flv"];

#[derive(Default)]
struct JobControl {
    pause_requested: AtomicBool,
    stop_requested: AtomicBool,
}

struct AppState {
    control: Arc<JobControl>,
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

/// Run one ffmpeg stream-copy cut: [start, start+len) seconds.
async fn cut_chunk(
    app: &AppHandle,
    input: &Path,
    output: &Path,
    start_secs: f64,
    len_secs: f64,
) -> Result<(), String> {
    let sidecar = app.shell().sidecar("ffmpeg").map_err(|e| e.to_string())?;
    let (mut rx, _child) = sidecar
        .args([
            "-y",
            "-ss", &start_secs.to_string(),
            "-i", input.to_str().ok_or("Invalid input path")?,
            "-t", &len_secs.to_string(),
            "-c", "copy",
            "-map", "0",
            output.to_str().ok_or("Invalid output path")?,
        ])
        .spawn()
        .map_err(|e| e.to_string())?;

    let mut stderr_tail = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            CommandEvent::Stderr(bytes) => {
                stderr_tail = String::from_utf8_lossy(&bytes).to_string();
            }
            CommandEvent::Error(err) => {
                return Err(err);
            }
            CommandEvent::Terminated(payload) => {
                if payload.code != Some(0) {
                    return Err(format!("ffmpeg exited with error: {stderr_tail}"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn split_one_file(
    app: &AppHandle,
    input: &Path,
    output_root: &Path,
    chunk_minutes: f64,
    file_index: usize,
    file_count: usize,
    control: &JobControl,
) -> Result<(), String> {
    let file_name = input.file_name().unwrap().to_string_lossy().to_string();
    let stem = input.file_stem().unwrap().to_string_lossy().to_string();
    let ext = input.extension().unwrap().to_string_lossy().to_string();

    let dest_folder = output_root.join(&stem);
    std::fs::create_dir_all(&dest_folder).map_err(|e| e.to_string())?;

    let duration = get_duration_seconds(app, input).await?;
    let chunk_seconds = chunk_minutes * 60.0;
    let chunk_count = ((duration / chunk_seconds).ceil() as usize).max(1);

    for i in 0..chunk_count {
        // Stop check
        if control.stop_requested.load(Ordering::SeqCst) {
            emit_progress(app, ProgressPayload {
                file_name: file_name.clone(), file_index, file_count,
                chunk_index: i, chunk_count, status: "stopped".into(),
                message: "Stopped by user".into(),
            });
            return Ok(());
        }

        // Pause check (poll between chunks)
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

        emit_progress(app, ProgressPayload {
            file_name: file_name.clone(), file_index, file_count,
            chunk_index: i, chunk_count, status: "running".into(),
            message: format!("Cutting chunk {}/{}", i + 1, chunk_count),
        });

        cut_chunk(app, input, &out_path, start, this_len).await?;
    }

    emit_progress(app, ProgressPayload {
        file_name, file_index, file_count,
        chunk_index: chunk_count, chunk_count, status: "done".into(),
        message: "File complete".into(),
    });
    Ok(())
}

#[tauri::command]
async fn start_split(
    app: AppHandle,
    state: State<'_, AppState>,
    input_path: String,
    output_path: Option<String>,
    chunk_minutes: f64,
) -> Result<(), String> {
    let control = state.control.clone();
    control.stop_requested.store(false, Ordering::SeqCst);
    control.pause_requested.store(false, Ordering::SeqCst);

    let input = PathBuf::from(&input_path);
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

    tauri::async_runtime::spawn(async move {
        let count = files.len();
        for (idx, file) in files.iter().enumerate() {
            if control.stop_requested.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) =
                split_one_file(&app, file, &output_root, chunk_minutes, idx, count, &control).await
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
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { control: Arc::new(JobControl::default()) })
        .invoke_handler(tauri::generate_handler![
            start_split,
            pause_split,
            resume_split,
            stop_split
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
