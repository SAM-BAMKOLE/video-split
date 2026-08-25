import { useEffect, useState, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Film,
  Sun,
  Moon,
  FileVideo,
  FolderOpen,
  FolderOutput,
  Clock,
  Play,
  Pause,
  Square,
  Plus,
  Minus,
  Sparkles,
  CheckCircle2,
  XCircle,
  OctagonX,
  Crop,
  FileType2,
  Scissors,
  RefreshCw,
  Target,
} from "lucide-react";

type Status = "running" | "paused" | "done" | "stopped" | "error";
type Page = "split" | "convert";
type OutputFormat = "source" | "mp4" | "mov" | "mkv";

type ProgressPayload = {
  file_name: string;
  file_index: number;
  file_count: number;
  chunk_index: number;
  chunk_count: number;
  status: Status;
  message: string;
};

type Theme = "dark" | "light";

type JobResult = {
  status: "done" | "stopped" | "error";
  filesProcessed: number;
  totalChunks: number;
  message?: string;
};

const MAX_SEGMENTS = 40;

const RATIO_PRESETS: { label: string; w: number; h: number }[] = [
  { label: "4:5", w: 4, h: 5 },
  { label: "9:16", w: 9, h: 16 },
  { label: "3:4", w: 3, h: 4 },
];

const FORMAT_OPTIONS: { label: string; value: OutputFormat; hint: string }[] = [
  { label: "Same as source", value: "source", hint: "No container change" },
  { label: "MP4", value: "mp4", hint: "Best for iPhone / CapCut" },
  { label: "MOV", value: "mov", hint: "Apple-native" },
  { label: "MKV", value: "mkv", hint: "Original movie-rip style" },
];

export default function App() {
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = localStorage.getItem("theme");
    if (saved === "light" || saved === "dark") return saved;
    return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
  });

  const [page, setPage] = useState<Page>("split");

  const [inputPath, setInputPath] = useState("");
  const [outputPath, setOutputPath] = useState("");
  const [chunkMinutes, setChunkMinutes] = useState(15);
  const [cropEnabled, setCropEnabled] = useState(true);
  const [ratioW, setRatioW] = useState(4);
  const [ratioH, setRatioH] = useState(5);
  const [cropOffset, setCropOffset] = useState(0); // -100 (left) .. 0 (center) .. 100 (right)
  const [dynamicTracking, setDynamicTracking] = useState(false);
  const [outputFormat, setOutputFormat] = useState<OutputFormat>("mp4");

  const [progress, setProgress] = useState<ProgressPayload | null>(null);
  const [isRunning, setIsRunning] = useState(false);
  const [isPaused, setIsPaused] = useState(false);
  const [isStopping, setIsStopping] = useState(false);
  const [log, setLog] = useState<string[]>([]);
  const [poppedIndex, setPoppedIndex] = useState<number | null>(null);
  const [jobResult, setJobResult] = useState<JobResult | null>(null);
  const logBodyRef = useRef<HTMLDivElement>(null);
  const chunksWrittenRef = useRef(0);
  const filesDoneRef = useRef(0);

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("theme", theme);
  }, [theme]);

  useEffect(() => {
    const unlisten = listen<ProgressPayload>("split-progress", (event) => {
      const p = event.payload;
      setProgress((prev) => {
        if (prev && p.chunk_index > prev.chunk_index) {
          setPoppedIndex(p.chunk_index - 1);
          setTimeout(() => setPoppedIndex(null), 350);
        }
        return p;
      });
      setLog((prev) => [...prev, `${p.file_name} — ${p.message}`].slice(-200));

      const isLastFile = p.file_index === p.file_count - 1;

      if (p.status === "done") {
        chunksWrittenRef.current += p.chunk_count;
        filesDoneRef.current += 1;
        if (isLastFile) {
          setIsRunning(false);
          setIsPaused(false);
          setJobResult({
            status: "done",
            filesProcessed: filesDoneRef.current,
            totalChunks: chunksWrittenRef.current,
          });
        }
      }

      if (p.status === "stopped") {
        setIsRunning(false);
        setIsPaused(false);
        setIsStopping(false);
        setJobResult({
          status: "stopped",
          filesProcessed: filesDoneRef.current,
          totalChunks: chunksWrittenRef.current,
        });
      }
      if (p.status === "error") {
        setIsRunning(false);
        setIsPaused(false);
        setIsStopping(false);
        setJobResult({
          status: "error",
          filesProcessed: filesDoneRef.current,
          totalChunks: chunksWrittenRef.current,
          message: p.message,
        });
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const el = logBodyRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [log]);

  const pickInputFile = useCallback(async () => {
    const path = await open({ multiple: false, directory: false });
    if (typeof path === "string") setInputPath(path);
  }, []);

  const pickInputFolder = useCallback(async () => {
    const path = await open({ directory: true });
    if (typeof path === "string") setInputPath(path);
  }, []);

  const pickOutputFolder = useCallback(async () => {
    const path = await open({ directory: true });
    if (typeof path === "string") setOutputPath(path);
  }, []);

  const start = async () => {
    if (!inputPath) return;
    setLog([]);
    setProgress(null);
    setJobResult(null);
    chunksWrittenRef.current = 0;
    filesDoneRef.current = 0;
    setIsRunning(true);
    setIsPaused(false);
    setIsStopping(false);
    try {
      if (page === "split") {
        await invoke("start_split", {
          inputPath,
          outputPath: outputPath || null,
          chunkMinutes,
          cropEnabled,
          cropRatioW: ratioW,
          cropRatioH: ratioH,
          cropOffsetPercent: cropOffset,
          dynamicTracking,
          outputFormat,
        });
      } else {
        await invoke("convert_files", {
          inputPath,
          outputPath: outputPath || null,
          outputFormat,
        });
      }
    } catch (e) {
      setLog((prev) => [...prev, `Error — ${e}`]);
      setIsRunning(false);
    }
  };

  const togglePause = async () => {
    if (isPaused) {
      await invoke("resume_split");
      setIsPaused(false);
    } else {
      await invoke("pause_split");
      setIsPaused(true);
    }
  };

  const stop = async () => {
    setIsStopping(true);
    await invoke("stop_split");
  };

  const percent =
    progress && progress.chunk_count > 0
      ? Math.round((progress.chunk_index / progress.chunk_count) * 100)
      : 0;

  const useSegments = progress && progress.chunk_count > 0 && progress.chunk_count <= MAX_SEGMENTS;

  const statusLabel: Record<Status, string> = {
    running: "Splitting",
    paused: "Paused",
    done: "Complete",
    stopped: "Stopped",
    error: "Error",
  };

  const switchPage = (p: Page) => {
    if (isRunning) return;
    setPage(p);
    setJobResult(null);
    setProgress(null);
    setLog([]);
  };

  return (
    <div className="app-shell">
      <div className="bg-glow" />
      <div className="app">
        <header className="header">
          <div className="brand">
            <div className="brand-mark">
              <Film size={17} strokeWidth={2.2} />
            </div>
            <div>
              <div className="brand-title">Video Splitter</div>
              <div className="brand-subtitle">Lossless chunking, sync preserved</div>
            </div>
          </div>
          <button
            className="theme-toggle"
            onClick={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
            aria-label="Toggle theme"
          >
            {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
          </button>
        </header>

        <div className="tabs">
          <button
            className={`tab ${page === "split" ? "active" : ""}`}
            onClick={() => switchPage("split")}
            disabled={isRunning}
          >
            <Scissors size={14} />
            Split
          </button>
          <button
            className={`tab ${page === "convert" ? "active" : ""}`}
            onClick={() => switchPage("convert")}
            disabled={isRunning}
          >
            <RefreshCw size={14} />
            Convert format
          </button>
        </div>

        <div className="card">
          <div className="card-label">
            <FileVideo size={13} />
            Source — file or folder
          </div>
          <div className="row">
            <input
              className="path-input"
              value={inputPath}
              readOnly
              placeholder="Nothing selected yet"
            />
            <button className="btn" onClick={pickInputFile} disabled={isRunning}>
              File
            </button>
            <button className="btn" onClick={pickInputFolder} disabled={isRunning}>
              Folder
            </button>
          </div>
        </div>

        <div className="card">
          <div className="card-label">
            <FolderOutput size={13} />
            Destination — optional
          </div>
          <div className="row">
            <input
              className="path-input"
              value={outputPath}
              readOnly
              placeholder="Same folder as source"
            />
            <button className="btn" onClick={pickOutputFolder} disabled={isRunning}>
              <FolderOpen size={14} />
              Folder
            </button>
          </div>
        </div>

        <div className="card">
          <div className="card-label">
            <FileType2 size={13} />
            Output format
          </div>
          <div className="format-options">
            {FORMAT_OPTIONS.map((f) => (
              <button
                key={f.value}
                className={`format-chip ${outputFormat === f.value ? "active" : ""}`}
                onClick={() => setOutputFormat(f.value)}
                disabled={isRunning}
              >
                <span className="format-chip-label">{f.label}</span>
                <span className="format-chip-hint">{f.hint}</span>
              </button>
            ))}
          </div>
        </div>

        {page === "split" && (
          <>
            <div className="card">
              <div className="card-label">
                <Clock size={13} />
                Chunk length
              </div>
              <div className="stepper">
                <button
                  className="stepper-btn"
                  onClick={() => setChunkMinutes((m) => Math.max(1, m - 1))}
                  disabled={isRunning || chunkMinutes <= 1}
                  aria-label="Decrease chunk length"
                >
                  <Minus size={14} />
                </button>
                <span className="stepper-value">{chunkMinutes}</span>
                <span className="stepper-unit">minutes</span>
                <button
                  className="stepper-btn"
                  onClick={() => setChunkMinutes((m) => m + 1)}
                  disabled={isRunning}
                  aria-label="Increase chunk length"
                >
                  <Plus size={14} />
                </button>
              </div>
            </div>

            <div className="card">
              <div className="crop-header">
                <div className="card-label" style={{ marginBottom: 0 }}>
                  <Crop size={13} />
                  Crop for reels
                </div>
                <label className="switch">
                  <input
                    type="checkbox"
                    checked={cropEnabled}
                    onChange={(e) => setCropEnabled(e.target.checked)}
                    disabled={isRunning}
                  />
                  <span className="switch-track">
                    <span className="switch-thumb" />
                  </span>
                </label>
              </div>

              {cropEnabled && (
                <div className="crop-body">
                  <div className="ratio-presets">
                    {RATIO_PRESETS.map((p) => (
                      <button
                        key={p.label}
                        className={`chip ${ratioW === p.w && ratioH === p.h ? "active" : ""}`}
                        onClick={() => {
                          setRatioW(p.w);
                          setRatioH(p.h);
                        }}
                        disabled={isRunning}
                      >
                        {p.label}
                      </button>
                    ))}
                    <div className="ratio-custom">
                      <input
                        type="number"
                        min={1}
                        className="ratio-input"
                        value={ratioW}
                        disabled={isRunning}
                        onChange={(e) => setRatioW(Math.max(1, Number(e.target.value)))}
                      />
                      <span className="ratio-sep">:</span>
                      <input
                        type="number"
                        min={1}
                        className="ratio-input"
                        value={ratioH}
                        disabled={isRunning}
                        onChange={(e) => setRatioH(Math.max(1, Number(e.target.value)))}
                      />
                    </div>
                  </div>

                  <div className="tracking-toggle-row">
                    <div className="tracking-toggle-label">
                      <Target size={13} />
                      <span>Follow subject automatically</span>
                      <span className="beta-tag">Experimental</span>
                    </div>
                    <label className="switch">
                      <input
                        type="checkbox"
                        checked={dynamicTracking}
                        onChange={(e) => setDynamicTracking(e.target.checked)}
                        disabled={isRunning}
                      />
                      <span className="switch-track">
                        <span className="switch-thumb" />
                      </span>
                    </label>
                  </div>

                  <div className={`offset-row ${dynamicTracking ? "disabled-block" : ""}`}>
                    <span className="offset-label">
                      {dynamicTracking
                        ? "Position follows the subject automatically"
                        : cropOffset === 0
                        ? "Centered"
                        : cropOffset < 0
                        ? `Left ${Math.abs(cropOffset)}%`
                        : `Right ${cropOffset}%`}
                    </span>
                    <input
                      type="range"
                      min={-100}
                      max={100}
                      step={5}
                      value={cropOffset}
                      disabled={isRunning || dynamicTracking}
                      onChange={(e) => setCropOffset(Number(e.target.value))}
                      className="offset-slider"
                      aria-label="Horizontal crop offset"
                    />
                  </div>
                  <div className="crop-note">
                    {dynamicTracking
                      ? "Detects the main subject and pans the crop to keep them in frame as they move — analyzes the video first, so this takes noticeably longer than a fixed crop."
                      : "Crops the sides only — top and bottom are always kept in full, and nothing is scaled or zoomed."}{" "}
                    Audio stays a lossless copy; only video is re-encoded.
                  </div>
                </div>
              )}
            </div>
          </>
        )}

        {page === "convert" && (
          <div className="card">
            <div className="crop-note" style={{ marginTop: 0 }}>
              Converts the whole file into the format above — no splitting, no
              cropping. This is a container remux, not a re-encode, so quality
              is identical to the source.
            </div>
          </div>
        )}

        <div className="controls">
          {!isRunning ? (
            <button className="btn btn-primary" onClick={start} disabled={!inputPath}>
              <Play size={15} fill="currentColor" />
              {jobResult ? "Run again" : page === "split" ? "Start splitting" : "Start converting"}
            </button>
          ) : (
            <>
              <button className="btn btn-secondary" onClick={togglePause} disabled={isStopping}>
                {isPaused ? <Play size={14} fill="currentColor" /> : <Pause size={14} />}
                {isPaused ? "Resume" : "Pause"}
              </button>
              <button className="btn btn-danger" onClick={stop} disabled={isStopping}>
                <Square size={13} fill="currentColor" />
                {isStopping ? "Stopping…" : "Stop"}
              </button>
            </>
          )}
        </div>

        {jobResult && (
          <div className={`card result-banner result-${jobResult.status}`}>
            <div className="result-icon">
              {jobResult.status === "done" && <CheckCircle2 size={22} />}
              {jobResult.status === "stopped" && <OctagonX size={22} />}
              {jobResult.status === "error" && <XCircle size={22} />}
            </div>
            <div className="result-text">
              <div className="result-title">
                {jobResult.status === "done" && "Done"}
                {jobResult.status === "stopped" && "Stopped"}
                {jobResult.status === "error" && "Something went wrong"}
              </div>
              <div className="result-sub">
                {jobResult.status === "done" && page === "split" &&
                  `${jobResult.filesProcessed} file${jobResult.filesProcessed === 1 ? "" : "s"} split into ${jobResult.totalChunks} chunk${jobResult.totalChunks === 1 ? "" : "s"}${outputPath ? ` — saved to ${outputPath}` : ""}.`}
                {jobResult.status === "done" && page === "convert" &&
                  `${jobResult.filesProcessed} file${jobResult.filesProcessed === 1 ? "" : "s"} converted${outputPath ? ` — saved to ${outputPath}` : ""}.`}
                {jobResult.status === "stopped" &&
                  `Stopped after ${jobResult.totalChunks} chunk${jobResult.totalChunks === 1 ? "" : "s"}. What's already done is saved — the rest wasn't started.`}
                {jobResult.status === "error" && (jobResult.message || "Check the activity log below for details.")}
              </div>
            </div>
          </div>
        )}

        {progress ? (
          <div className="card progress-card">
            <div className="progress-header">
              <span className="progress-filename">{progress.file_name}</span>
              <span className="progress-count">
                {progress.chunk_index}/{progress.chunk_count} · {percent}%
              </span>
            </div>

            <div className="filmstrip-wrap">
              <div className="sprocket-row" />
              {useSegments ? (
                <div className="filmstrip-track">
                  {Array.from({ length: progress.chunk_count }).map((_, i) => {
                    const filled = i < progress.chunk_index;
                    const current = i === progress.chunk_index && progress.status === "running";
                    return (
                      <div
                        key={i}
                        className={[
                          "filmstrip-segment",
                          filled ? "filled" : "",
                          current ? "current" : "",
                          poppedIndex === i ? "pop" : "",
                        ]
                          .filter(Boolean)
                          .join(" ")}
                      />
                    );
                  })}
                </div>
              ) : (
                <div className="progress-bar-track">
                  <div className="progress-bar-fill" style={{ width: `${percent}%` }} />
                </div>
              )}
              <div className="sprocket-row" />
            </div>

            <div className="progress-status">
              <span className={`status-dot ${isStopping ? "paused" : progress.status}`} />
              <span className="status-text">
                {isStopping ? "Stopping…" : statusLabel[progress.status]}
              </span>
            </div>
          </div>
        ) : (
          <div className="empty-state">
            <Sparkles size={26} />
            <div className="empty-state-title">Ready when you are</div>
            <div className="empty-state-sub">
              {page === "split"
                ? cropEnabled
                  ? "Pick a video or a folder, set your chunk length, and hit start. Audio stays a lossless copy while the video is cropped to your target ratio."
                  : "Pick a video or a folder, set your chunk length, and hit start. Nothing is re-encoded, so audio stays perfectly in sync."
                : "Pick a video or a folder and hit start to convert it — a lossless container remux, no chunking."}
            </div>
          </div>
        )}

        {log.length > 0 && (
          <div className="card log-card">
            <div className="card-label">Activity</div>
            <div className="log-body" ref={logBodyRef}>
              {log.map((line, i) => (
                <div className="log-line" key={i}>
                  {line}
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
