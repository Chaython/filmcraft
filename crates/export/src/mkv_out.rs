//! AV1 Matroska export: lossless container conversion after the existing MP4 muxer
//! has received the encoder's final, validated AV1 sequence header.
//!
//! Some hardware encoders cannot provide av1C until their first frames. Matroska
//! needs AV1CodecConfigurationRecord at track creation, so never invent one.
//! A temporary MP4 is produced with the normal streaming pipeline; FFmpeg
//! stream-copies the finished tracks into MKV without decoding or re-encoding.
use std::fs::{self, File, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{ExportError, ExportSettings, Out, Result};

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// Guard intermediate files even when rendering, remuxing or output delivery fails.
pub(crate) struct PendingMkv {
    intermediate: PathBuf,
}
impl Drop for PendingMkv {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.intermediate);
    }
}
struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Reserve a file with create_new rather than overwriting some other process' temporary file.
fn reserve(dir: &Path, ext: &str) -> Result<(PathBuf, File)> {
    for _ in 0..32 {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| ExportError::Io(e.to_string()))?.as_nanos();
        let serial = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".filmcraft-av1-mkv-{}-{stamp}-{serial}.{ext}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(ExportError::Io(format!("cannot reserve Matroska temporary file: {e}"))),
        }
    }
    Err(ExportError::Io("cannot find unused Matroska temporary file name".into()))
}

fn target_dir(settings: &ExportSettings) -> &Path {
    if settings.sink.is_some() {
        // Web/in-memory sinks use a real temporary file for stream-copy conversion.
        // Caller should not assume the logical sink name is a filesystem path.
        Path::new("")
    } else {
        Path::new(&settings.path).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
    }
}

fn ffmpeg() -> std::ffi::OsString {
    std::env::var_os("FILMCRAFT_FFMPEG").filter(|v| !v.is_empty()).unwrap_or_else(|| "ffmpeg".into())
}

pub(crate) fn begin(settings: &ExportSettings) -> Result<(PendingMkv, Out)> {
    let tool = ffmpeg();
    let result = Command::new(&tool)
        .args(["-hide_banner", "-version"])
        .output()
        .map_err(|e| ExportError::Unsupported(format!("AV1 MKV export requires FFmpeg on PATH or FILMCRAFT_FFMPEG: {e}")))?;
    if !result.status.success() {
        return Err(ExportError::Unsupported("FFmpeg could not start: MKV export needs its Matroska muxer".into()));
    }
    let muxers = Command::new(&tool)
        .args(["-hide_banner", "-muxers"])
        .output()
        .map_err(|e| ExportError::Unsupported(format!("cannot check FFmpeg Matroska muxer: {e}")))?;
    if !muxers.status.success() || !String::from_utf8_lossy(&muxers.stdout).lines().any(|line| line.trim_start().starts_with("E") && line.contains("matroska"))
    {
        return Err(ExportError::Unsupported("this FFmpeg build has no Matroska writer; install FFmpeg with the matroska muxer".into()));
    }
    let dir = if settings.sink.is_some() { std::env::temp_dir() } else { target_dir(settings).to_path_buf() };
    let (path, file) = reserve(&dir, "mp4")?;
    Ok((PendingMkv { intermediate: path }, Out::File(BufWriter::new(file))))
}

/// Finalize the intermediate, losslessly stream-copy its tracks, then deliver
/// the finished MKV to the output path or OutputSink. Never report success on
/// FFmpeg failure or expose a half-written destination file.
pub(crate) fn finish(intermediate_writer: Out, settings: &ExportSettings, pending: &PendingMkv) -> Result<u64> {
    let input_path = pending.intermediate.to_string_lossy();
    intermediate_writer.finish_path(settings, &input_path)?;
    let dir = if settings.sink.is_some() { std::env::temp_dir() } else { target_dir(settings).to_path_buf() };
    let (output_path, reserved) = reserve(&dir, "mkv")?;
    drop(reserved);
    let guard = TemporaryFile(output_path.clone());
    let output = Command::new(ffmpeg())
        .args(["-nostdin", "-hide_banner", "-v", "error", "-xerror", "-y", "-i"])
        .arg(&pending.intermediate)
        .args(["-map", "0", "-map_metadata", "0", "-c", "copy", "-f", "matroska"])
        .arg(&output_path)
        .output()
        .map_err(|e| ExportError::Unsupported(format!("FFmpeg Matroska muxer could not run: {e}")))?;
    if !output.status.success() {
        return Err(ExportError::Encode(format!(
            "AV1/AAC/FLAC MKV stream-copy failed (source was not transcoded): {}",
            String::from_utf8_lossy(&output.stderr).chars().take(2000).collect::<String>()
        )));
    }
    let len = fs::metadata(&output_path).map_err(|e| ExportError::Io(e.to_string()))?.len();
    if len == 0 {
        return Err(ExportError::Encode("MKV muxer generated an empty file".into()));
    }
    if settings.sink.is_some() {
        let data = fs::read(&output_path).map_err(|e| ExportError::Io(e.to_string()))?;
        crate::write_output(settings, &settings.path, data)?;
    } else {
        let target = Path::new(&settings.path);
        // The complete new file exists before an older export can be replaced.
        match fs::rename(&output_path, target) {
            Ok(()) => {}
            Err(_) if target.is_file() => {
                // Windows rename cannot replace an existing file. Only remove the old
                // destination after FFmpeg has completed the new MKV successfully.
                fs::remove_file(target).map_err(|e| ExportError::Io(format!("cannot replace existing MKV: {e}")))?;
                fs::rename(&output_path, target).map_err(|e| ExportError::Io(format!("cannot move completed MKV into place: {e}")))?;
            }
            Err(e) => return Err(ExportError::Io(format!("cannot move completed MKV into place: {e}"))),
        }
    }
    drop(guard);
    Ok(len)
}
