//! Test videos made with ffmpeg, cached in the cargo target dir (never the real home or Downloads).

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

pub const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";

#[derive(Clone, Copy, Debug)]
pub enum Fx {
    /// H.264 MP4, `moov` at the end (ffmpeg default).
    Mp4End,
    /// H.264 MP4 with `-movflags +faststart`.
    Mp4Fast,
    /// H.264 in Matroska; Cues at the end.
    MkvCues,
}

static LOCK: Mutex<()> = Mutex::new(());

pub fn have_ffmpeg() -> bool {
    std::path::Path::new(FFMPEG).exists()
}

/// 1080p test pattern + sine audio of `secs` seconds. `None` when ffmpeg is not installed.
pub fn fixture(kind: Fx, secs: u32) -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found at {FFMPEG}: skipping");
        return None;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("trs-stream-fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let (name, ext, extra): (&str, &str, &[&str]) = match kind {
        Fx::Mp4End => ("mp4end", "mp4", &[]),
        Fx::Mp4Fast => ("mp4fast", "mp4", &["-movflags", "+faststart"]),
        Fx::MkvCues => ("mkvcues", "mkv", &[]),
    };
    let path = dir.join(format!("{name}-{secs}s.{ext}"));
    if path.exists() {
        return Some(path);
    }
    let tmp = dir.join(format!("{name}-{secs}s.partial.{ext}"));
    let st = Command::new(FFMPEG)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", &secs.to_string(), "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "4M", "-pix_fmt", "yuv420p", "-g", "60", "-c:a", "aac", "-b:a", "128k"])
        .args(extra)
        .arg(&tmp)
        .status()
        .expect("run ffmpeg");
    assert!(st.success(), "ffmpeg failed");
    std::fs::rename(&tmp, &path).unwrap();
    Some(path)
}
