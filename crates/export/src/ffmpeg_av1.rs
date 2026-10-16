//! Optional libaom AV1 software encoder for high bit depths and chroma profiles.
//! Uses a local FFmpeg executable; never downloads code or invokes a shell.
//! Frames are spooled to a temporary raw-video file instead of held in memory.
use std::fs::{File, OpenOptions, remove_file};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Av1PixelFormat, BitrateMode, ColorSignal, EncodedPacket, EncoderFrame, ExportError, ExportSettings, Format, Result, VideoEncoder};
use filmcraft_av1::SequenceHeader;
use filmcraft_isobmff::{Av1Config, ColorInfo, SampleEntry};
use filmcraft_time::FrameRate;

static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
// An 8K 12-bit 4:4:4 raw frame is ~212 MiB. Keep a bounded per-packet
// allocation that still permits high-entropy Professional-profile frames.
const MAX_FRAME_BYTES: usize = 384 * 1024 * 1024;

fn unique_temp_file() -> Result<(File, PathBuf, PathBuf)> {
    // Long 4K/8K high-precision exports can spool hundreds of GiB. Allow a
    // dedicated scratch volume instead of silently filling the system drive.
    let dir = std::env::var_os("FILMCRAFT_AV1_TEMP_DIR").filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    unique_temp_file_in(&dir)
}

fn unique_temp_file_in(dir: &Path) -> Result<(File, PathBuf, PathBuf)> {
    for _ in 0..32 {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| ExportError::Io(e.to_string()))?.as_nanos();
        let id = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let base = dir.join(format!("filmcraft-av1-{}-{stamp}-{id}", std::process::id()));
        let raw = base.with_extension("raw");
        match OpenOptions::new().create_new(true).write(true).open(&raw) {
            Ok(file) => return Ok((file, raw, base.with_extension("ivf"))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(ExportError::Io(format!("AV1 temporary input file in {}: {e}", dir.display()))),
        }
    }
    Err(ExportError::Io("could not create unique AV1 spool file".into()))
}

fn checked_pixels(w: u32, h: u32) -> Result<usize> {
    let pixels = usize::try_from(w).ok().and_then(|w| usize::try_from(h).ok().and_then(|h| w.checked_mul(h)));
    pixels.filter(|&n| n > 0 && n <= 8192 * 4320).ok_or_else(|| ExportError::Unsupported("AV1 frame size exceeds encoder allocation limits".into()))
}

fn quantize(value: f32, depth: u8, chroma: bool, full: bool) -> u16 {
    let max = (1u32 << depth) - 1;
    let scale = (1u32 << (depth - 8)) as f32;
    let value = if value.is_finite() { value.clamp(if chroma { -0.5 } else { 0.0 }, if chroma { 0.5 } else { 1.0 }) } else { 0.0 };
    let encoded = if full {
        if chroma { (max as f32) * (value + 0.5) } else { (max as f32) * value }
    } else if chroma {
        (128.0 + 224.0 * value) * scale
    } else {
        (16.0 + 219.0 * value) * scale
    };
    encoded.round().clamp(0.0, max as f32) as u16
}

fn matrix(signal: ColorSignal) -> Result<(f32, f32)> {
    match signal.matrix {
        1 => Ok((0.2126, 0.0722)),     // BT.709
        5 | 6 => Ok((0.2990, 0.1140)), // BT.470BG / SMPTE 170M
        9 => Ok((0.2627, 0.0593)),     // BT.2020 non-constant luminance
        _ => Err(ExportError::Unsupported(format!("AV1 external encoder needs a supported YCbCr matrix (709/601/2020 nonconstant), got {}", signal.matrix))),
    }
}

fn write_planar(frame: &EncoderFrame<'_>, format: Av1PixelFormat, signal: ColorSignal, full_range: bool, raw: &mut File) -> Result<()> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let count = checked_pixels(frame.width, frame.height)?;
    if signal.is_hdr() && format.bit_depth() < 10 {
        return Err(ExportError::Unsupported("HDR AV1 requires a 10- or 12-bit output format".into()));
    }
    if let Some(rgb) = frame.hdr {
        if rgb.len() != count * 3 {
            return Err(ExportError::Unsupported("HDR AV1 needs exactly three float RGB channels per pixel".into()));
        }
    } else if frame.rgba.len() < count * 4 {
        return Err(ExportError::Unsupported("AV1 RGBA frame data is truncated".into()));
    } else if signal.is_hdr() {
        return Err(ExportError::Unsupported("HDR AV1 requires a high-precision HDR frame, not 8-bit RGBA".into()));
    }
    let (kr, kb) = matrix(signal)?;
    let kg = 1.0 - kr - kb;
    let (sx, sy) = format.chroma_shift();
    let (cw, ch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
    let mut y = vec![0u16; count];
    let mut u = vec![0u16; cw * ch];
    let mut v = vec![0u16; cw * ch];
    let rgb_at = |x: usize, yy: usize| -> (f32, f32, f32) {
        let n = yy * w + x;
        if let Some(rgb) = frame.hdr {
            let i = n * 3;
            (rgb[i], rgb[i + 1], rgb[i + 2])
        } else {
            let i = n * 4;
            (f32::from(frame.rgba[i]) / 255.0, f32::from(frame.rgba[i + 1]) / 255.0, f32::from(frame.rgba[i + 2]) / 255.0)
        }
    };
    let color = |x: usize, yy: usize| {
        let (r, g, b) = rgb_at(x, yy);
        let clamp = |v: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let (r, g, b) = (clamp(r), clamp(g), clamp(b));
        let luma = kr * r + kg * g + kb * b;
        (luma, (b - luma) / (2.0 * (1.0 - kb)), (r - luma) / (2.0 * (1.0 - kr)))
    };
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = quantize(color(col, row).0, format.bit_depth(), false, full_range);
        }
    }
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut cb, mut cr, mut n) = (0.0f32, 0.0f32, 0u32);
            for yy in (cy << sy)..((cy + 1) << sy).min(h) {
                for xx in (cx << sx)..((cx + 1) << sx).min(w) {
                    let (_, uval, vval) = color(xx, yy);
                    cb += uval;
                    cr += vval;
                    n += 1;
                }
            }
            if n > 0 {
                u[cy * cw + cx] = quantize(cb / n as f32, format.bit_depth(), true, full_range);
                v[cy * cw + cx] = quantize(cr / n as f32, format.bit_depth(), true, full_range);
            }
        }
    }
    for plane in [&y, &u, &v] {
        if format.bit_depth() == 8 {
            let bytes: Vec<u8> = plane.iter().map(|&v| v as u8).collect();
            raw.write_all(&bytes).map_err(|e| ExportError::Io(e.to_string()))?;
        } else {
            let mut bytes = Vec::with_capacity(plane.len() * 2);
            for &value in plane {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            raw.write_all(&bytes).map_err(|e| ExportError::Io(e.to_string()))?;
        }
    }
    Ok(())
}

/// Find a sequence header OBU in low-overhead AV1 access units (bounded LEB128).
fn sequence_header(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut pos = 0usize;
    while let Some(&header) = data.get(pos) {
        if header & 0x81 != 0 {
            return None;
        }
        let start = pos;
        pos += 1;
        if header & 4 != 0 {
            pos = pos.checked_add(1)?;
            data.get(pos - 1)?;
        }
        if header & 2 == 0 {
            return None;
        }
        let mut size = 0usize;
        let mut shift = 0;
        for _ in 0..8 {
            let b = *data.get(pos)?;
            pos += 1;
            size = size.checked_add(usize::from(b & 0x7f).checked_shl(shift)?)?;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let end = pos.checked_add(size)?;
        let payload = data.get(pos..end)?;
        if (header >> 3) & 15 == 1 {
            return Some((data.get(start..end)?, payload));
        }
        pos = end;
    }
    None
}

fn av1_config(packet: &[u8], requested: Av1PixelFormat, signal: ColorSignal, full_range: bool) -> Result<Av1Config> {
    let (obu, payload) = sequence_header(packet).ok_or_else(|| ExportError::Encode("libaom produced AV1 frames without a sequence header OBU".into()))?;
    let seq = SequenceHeader::parse(payload).map_err(|e| ExportError::Encode(format!("invalid FFmpeg AV1 sequence header: {e}")))?;
    let (sx, sy) = requested.chroma_shift();
    let color = seq.color;
    if color.bit_depth != requested.bit_depth() || color.subsampling_x as usize != sx || color.subsampling_y as usize != sy || color.mono_chrome {
        return Err(ExportError::Encode(format!("libaom emitted a different AV1 pixel format than requested {}", requested.label())));
    }
    if (color.color_primaries, color.transfer_characteristics, color.matrix_coefficients, color.color_range)
        != (signal.primaries, signal.transfer, signal.matrix, full_range)
    {
        return Err(ExportError::Encode("libaom AV1 color metadata differs from requested primaries/transfer/matrix/range".into()));
    }
    Ok(Av1Config {
        seq_profile: seq.profile,
        seq_level_idx_0: seq.seq_level_idx[0],
        seq_tier_0: false,
        high_bitdepth: color.bit_depth > 8,
        twelve_bit: color.bit_depth == 12,
        monochrome: false,
        chroma_subsampling_x: sx != 0,
        chroma_subsampling_y: sy != 0,
        chroma_sample_position: color.chroma_sample_position,
        initial_presentation_delay_minus_one: None,
        config_obus: obu.to_vec(),
    })
}

pub struct FfmpegAv1Encoder {
    ffmpeg: std::ffi::OsString,
    raw: Option<File>,
    raw_path: PathBuf,
    ivf_path: PathBuf,
    format: Av1PixelFormat,
    signal: ColorSignal,
    full_range: bool,
    width: u32,
    height: u32,
    rate: FrameRate,
    bitrate_mode: BitrateMode,
    bitrate_kbps: u32,
    max_bitrate_kbps: Option<u32>,
    crf: f32,
    keyint: u32,
    frames: u64,
    first_index: Option<u64>,
    config: Option<Av1Config>,
    finished: bool,
}
impl Drop for FfmpegAv1Encoder {
    fn drop(&mut self) {
        self.raw.take();
        let _ = remove_file(&self.raw_path);
        let _ = remove_file(&self.ivf_path);
    }
}
impl FfmpegAv1Encoder {
    fn new(w: u32, h: u32, rate: FrameRate, settings: &ExportSettings) -> Result<Self> {
        checked_pixels(w, h)?;
        if rate.num <= 0 || rate.den <= 0 {
            return Err(ExportError::Unsupported("AV1 frame rate must be positive".into()));
        }
        if settings.signal.is_hdr() && settings.av1_pixel_format.bit_depth() < 10 {
            return Err(ExportError::Unsupported("8-bit AV1 cannot carry HDR10/HLG".into()));
        }
        matrix(settings.signal)?;
        let ffmpeg = std::env::var_os("FILMCRAFT_FFMPEG").filter(|s| !s.is_empty()).unwrap_or_else(|| "ffmpeg".into());
        let status = Command::new(&ffmpeg)
            .args(["-hide_banner", "-version"])
            .output()
            .map_err(|e| ExportError::Unsupported(format!("advanced AV1 export needs installed FFmpeg/libaom (set FILMCRAFT_FFMPEG): {e}")))?;
        if !status.status.success() {
            return Err(ExportError::Unsupported("FFmpeg did not start successfully".into()));
        }
        // Fail before rendering/spooling a long export when the installed FFmpeg is
        // missing the required libaom encoder or AV1 sequence-header metadata filter.
        for (flag, required) in [("-encoders", "libaom-av1"), ("-bsfs", "av1_metadata")] {
            let capabilities = Command::new(&ffmpeg)
                .args(["-hide_banner", flag])
                .output()
                .map_err(|e| ExportError::Unsupported(format!("FFmpeg AV1 capability probe failed: {e}")))?;
            if !capabilities.status.success() || !String::from_utf8_lossy(&capabilities.stdout).contains(required) {
                return Err(ExportError::Unsupported(format!(
                    "the installed FFmpeg does not provide required {required}; install a build with libaom AV1 and av1_metadata"
                )));
            }
        }
        let (raw, raw_path, ivf_path) = unique_temp_file()?;
        Ok(Self {
            ffmpeg,
            raw: Some(raw),
            raw_path,
            ivf_path,
            format: settings.av1_pixel_format,
            signal: settings.signal,
            full_range: settings.av1_full_range,
            width: w,
            height: h,
            rate,
            bitrate_mode: settings.bitrate_mode,
            bitrate_kbps: settings.bitrate_kbps.max(100),
            max_bitrate_kbps: settings.max_bitrate_kbps.filter(|&max| max >= settings.bitrate_kbps.max(100)),
            crf: settings.crf,
            keyint: settings.keyframe_distance.unwrap_or(48).max(1),
            frames: 0,
            first_index: None,
            config: None,
            finished: false,
        })
    }
    fn encode_ivf_into(&mut self, emit: &mut dyn FnMut(EncodedPacket) -> Result<()>) -> Result<()> {
        if self.frames == 0 {
            return Err(ExportError::Unsupported("cannot encode empty AV1 sequence".into()));
        }
        self.raw.take();
        let frame_rate = format!("{}/{}", self.rate.num, self.rate.den);
        let size = format!("{}x{}", self.width, self.height);
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            self.format.ffmpeg_name(),
            "-video_size",
            &size,
            "-framerate",
            &frame_rate,
            "-i",
        ])
        .arg(&self.raw_path)
        .args([
            "-an",
            "-c:v",
            "libaom-av1",
            "-pix_fmt",
            self.format.ffmpeg_name(),
            "-cpu-used",
            "6",
            "-lag-in-frames",
            "0",
            "-auto-alt-ref",
            "0",
            "-color_primaries",
        ])
        .arg(self.signal.primaries.to_string())
        .arg("-color_trc")
        .arg(self.signal.transfer.to_string())
        .arg("-colorspace")
        .arg(self.signal.matrix.to_string())
        .arg("-color_range")
        .arg(if self.full_range { "pc" } else { "tv" })
        .arg("-g")
        .arg(self.keyint.to_string());
        match self.bitrate_mode {
            BitrateMode::Crf => {
                cmd.args(["-b:v", "0", "-crf"]).arg(format!("{}", self.crf.clamp(0.0, 63.0).round()));
            }
            BitrateMode::Cbr => {
                // A target alone chooses VBR in libaom. Apply explicit VBV bounds
                // when the user requests constant bitrate; short GOPs may still
                // fluctuate, but the rate-control mode is no longer silently ignored.
                let kbps = format!("{}k", self.bitrate_kbps);
                let buffer = format!("{}k", self.bitrate_kbps.saturating_mul(2));
                cmd.args(["-b:v", &kbps, "-minrate", &kbps, "-maxrate", &kbps, "-bufsize", &buffer]);
            }
            _ => {
                cmd.arg("-b:v").arg(format!("{}k", self.bitrate_kbps));
                if let Some(max) = self.max_bitrate_kbps {
                    cmd.arg("-maxrate").arg(format!("{max}k"));
                    cmd.arg("-bufsize").arg(format!("{}k", max.saturating_mul(2)));
                }
            }
        }
        // libaom does not consistently copy the input stream's color fields into the
        // AV1 sequence header. Rewrite those fields inside the actual encoded bitstream;
        // av1_config() below then independently validates them before MP4 finalization.
        let bsf = format!(
            "av1_metadata=color_primaries={}:transfer_characteristics={}:matrix_coefficients={}:color_range={}",
            self.signal.primaries,
            self.signal.transfer,
            self.signal.matrix,
            u8::from(self.full_range)
        );
        let result = cmd
            .args(["-bsf:v"])
            .arg(bsf)
            .args(["-f", "ivf"])
            .arg(&self.ivf_path)
            .output()
            .map_err(|e| ExportError::Encode(format!("cannot launch FFmpeg/libaom: {e}")))?;
        if !result.status.success() {
            return Err(ExportError::Encode(format!(
                "FFmpeg/libaom AV1 export failed (format {}): {}",
                self.format.label(),
                String::from_utf8_lossy(&result.stderr).chars().take(2000).collect::<String>()
            )));
        }
        let mut file = File::open(&self.ivf_path).map_err(|e| ExportError::Io(e.to_string()))?;
        let len = file.metadata().map_err(|e| ExportError::Io(e.to_string()))?.len();
        let mut header = [0u8; 32];
        file.read_exact(&mut header).map_err(|e| ExportError::Encode(format!("truncated AV1 IVF header: {e}")))?;
        if &header[..4] != b"DKIF" || &header[8..12] != b"AV01" {
            return Err(ExportError::Encode("libaom did not produce an AV1 IVF stream".into()));
        }
        let (ew, eh) = (u16::from_le_bytes([header[12], header[13]]) as u32, u16::from_le_bytes([header[14], header[15]]) as u32);
        if (ew, eh) != (self.width, self.height) {
            return Err(ExportError::Encode("libaom returned mismatched AV1 frame dimensions".into()));
        }
        let mut emitted = 0u64;
        let mut accounted = 32u64;
        while accounted < len {
            let mut fh = [0u8; 12];
            file.read_exact(&mut fh).map_err(|e| ExportError::Encode(format!("truncated AV1 IVF frame header: {e}")))?;
            let size = u32::from_le_bytes([fh[0], fh[1], fh[2], fh[3]]) as usize;
            if size == 0 || size > MAX_FRAME_BYTES {
                return Err(ExportError::Unsupported("AV1 encoded packet outside accepted size bounds".into()));
            }
            accounted = accounted.saturating_add(12 + size as u64);
            if accounted > len {
                return Err(ExportError::Encode("AV1 IVF frame size exceeds file length".into()));
            }
            let timestamp = u64::from_le_bytes([fh[4], fh[5], fh[6], fh[7], fh[8], fh[9], fh[10], fh[11]]);
            if timestamp != emitted {
                return Err(ExportError::Encode(format!("libaom IVF timeline is not consecutive: frame {emitted} has timestamp {timestamp}")));
            }
            let mut data = vec![0u8; size];
            file.read_exact(&mut data).map_err(|e| ExportError::Encode(format!("truncated AV1 IVF packet: {e}")))?;
            if self.config.is_none() {
                self.config = Some(av1_config(&data, self.format, self.signal, self.full_range)?);
            }
            emit(EncodedPacket {
                key: filmcraft_av1::is_key_frame_unit(&data),
                data,
                duration: u32::try_from(self.rate.den).map_err(|_| ExportError::Unsupported("AV1 timescale overflow".into()))?,
                composition_offset: 0,
            })?;
            emitted += 1;
        }
        if emitted != self.frames {
            return Err(ExportError::Encode(format!("libaom emitted {emitted} AV1 samples for {} input frames; refuse incorrect timestamps", self.frames)));
        }
        Ok(())
    }
}
impl VideoEncoder for FfmpegAv1Encoder {
    fn sample_entry(&self) -> SampleEntry {
        let (sx, sy) = self.format.chroma_shift();
        let initial = self.config.clone().unwrap_or(Av1Config {
            seq_profile: if sx == 1 && sy == 1 {
                0
            } else if sx == 0 && self.format.bit_depth() < 12 {
                1
            } else {
                2
            },
            seq_level_idx_0: 0,
            seq_tier_0: false,
            high_bitdepth: self.format.bit_depth() > 8,
            twelve_bit: self.format.bit_depth() == 12,
            monochrome: false,
            chroma_subsampling_x: sx != 0,
            chroma_subsampling_y: sy != 0,
            chroma_sample_position: 0,
            initial_presentation_delay_minus_one: None,
            config_obus: Vec::new(),
        });
        let (w, h) = (u16::try_from(self.width).unwrap_or(u16::MAX), u16::try_from(self.height).unwrap_or(u16::MAX));
        let mut entry = SampleEntry::av1(initial, w, h);
        self.signal.apply_to(&mut entry, false);
        if let Some(video) = entry.video.as_mut() {
            video.color = Some(ColorInfo::Nclx {
                primaries: self.signal.primaries as u16,
                transfer: self.signal.transfer as u16,
                matrix: self.signal.matrix as u16,
                full_range: self.full_range,
            });
        }
        entry
    }
    fn final_sample_entry(&self) -> Result<Option<SampleEntry>> {
        self.config.as_ref().ok_or_else(|| ExportError::Encode("libaom produced no validated AV1 sequence header".into())).map(|_| Some(self.sample_entry()))
    }
    fn timescale(&self) -> u32 {
        u32::try_from(self.rate.num).unwrap_or(1)
    }
    fn encode(&mut self, frame: &EncoderFrame<'_>) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(ExportError::Encode("AV1 encoder has already finished".into()));
        }
        if frame.width != self.width || frame.height != self.height {
            return Err(ExportError::Unsupported("AV1 frame dimensions changed during export".into()));
        }
        if let Some(first) = self.first_index
            && first.checked_add(self.frames) != Some(frame.index)
        {
            return Err(ExportError::Unsupported("AV1 input frame indices must be consecutive to preserve timestamps".into()));
        }
        let raw = self.raw.as_mut().ok_or_else(|| ExportError::Io("AV1 raw spool has closed".into()))?;
        write_planar(frame, self.format, self.signal, self.full_range, raw)?;
        if self.first_index.is_none() {
            self.first_index = Some(frame.index);
        }
        self.frames = self.frames.checked_add(1).ok_or_else(|| ExportError::Unsupported("too many AV1 frames".into()))?;
        Ok(Vec::new())
    }
    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut packets = Vec::new();
        self.flush_into(&mut |packet| {
            packets.push(packet);
            Ok(())
        })?;
        Ok(packets)
    }
    fn flush_into(&mut self, emit: &mut dyn FnMut(EncodedPacket) -> Result<()>) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.encode_ivf_into(emit)
    }
}
pub fn factory(format: Format, w: u32, h: u32, rate: FrameRate, settings: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    if format != Format::Av1 {
        return None;
    }
    Some(FfmpegAv1Encoder::new(w, h, rate, settings).map(|v| Box::new(v) as Box<dyn VideoEncoder>))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn av1_temp_spool_uses_requested_scratch_directory_and_unique_files() {
        let directory = std::env::temp_dir();
        let (a, first_raw, first_ivf) = unique_temp_file_in(&directory).expect("first AV1 scratch");
        let (b, second_raw, second_ivf) = unique_temp_file_in(&directory).expect("second AV1 scratch");
        assert_ne!(first_raw, second_raw);
        assert_ne!(first_ivf, second_ivf);
        assert_eq!(first_raw.parent(), Some(directory.as_path()));
        assert_eq!(second_raw.parent(), Some(directory.as_path()));
        drop(a);
        drop(b);
        std::fs::remove_file(first_raw).unwrap();
        std::fs::remove_file(second_raw).unwrap();
    }

    #[test]
    fn professional_8k_packet_limit_covers_twelve_bit_444() {
        let max_pixels = 8192usize * 4320;
        let bytes_per_12bit_444_frame = max_pixels * 3 * 2;
        assert!(MAX_FRAME_BYTES > bytes_per_12bit_444_frame, "8K 12-bit 4:4:4 must not be rejected for exceeding the old 128 MiB packet cap");
        const {
            assert!(MAX_FRAME_BYTES <= 512 * 1024 * 1024, "untrusted IVF packets still need a bounded allocation");
        }
    }

    #[test]
    fn libaom_flush_into_streams_packets_and_preserves_av1_configuration() {
        let strict = std::env::var("FILMCRAFT_REQUIRE_AV1_ORACLE").as_deref() == Ok("1");
        let ffmpeg = std::env::var_os("FILMCRAFT_FFMPEG").filter(|v| !v.is_empty()).unwrap_or_else(|| "ffmpeg".into());
        if !Command::new(ffmpeg).arg("-version").output().is_ok_and(|o| o.status.success()) {
            assert!(!strict, "libaom streaming acceptance requires FFmpeg");
            return;
        }
        let settings = ExportSettings {
            format: Format::Av1,
            av1_pixel_format: Av1PixelFormat::Yuv420p10,
            bitrate_mode: BitrateMode::Crf,
            crf: 38.0,
            ..Default::default()
        };
        let mut encoder = FfmpegAv1Encoder::new(64, 64, FrameRate::FPS_24, &settings).expect("create libaom encoder");
        let rgba = vec![123u8; 64 * 64 * 4];
        for i in 0..5 {
            let frame = EncoderFrame { width: 64, height: 64, rgba: &rgba, hdr: None, index: i };
            assert!(encoder.encode(&frame).expect("spool frame").is_empty());
        }
        let mut count = 0u64;
        let mut max_packet = 0usize;
        encoder
            .flush_into(&mut |packet| {
                assert!(packet.key || count != 0, "first AV1 frame must be a random access point");
                assert_eq!(packet.duration, 1);
                max_packet = max_packet.max(packet.data.len());
                count += 1;
                Ok(())
            })
            .expect("stream encoded packets to callback");
        assert_eq!(count, 5);
        assert!(max_packet > 0);
        assert!(encoder.flush().expect("idempotent flush").is_empty());
        let entry = encoder.final_sample_entry().expect("validated sequence header").expect("AV1 sample entry");
        let filmcraft_isobmff::CodecConfig::Av1(av1) = entry.codec else { panic!("libaom must emit an AV1 sample entry") };
        assert!(av1.high_bitdepth);
        assert!(!av1.config_obus.is_empty());
    }

    #[test]
    fn precision_and_range_codes_are_bounded() {
        for depth in [8, 10, 12] {
            assert_eq!(quantize(0.0, depth, false, false), 16u16 << (depth - 8));
            assert_eq!(quantize(1.0, depth, false, false), 235u16 << (depth - 8));
            assert_eq!(quantize(0.0, depth, true, false), 128u16 << (depth - 8));
            assert_eq!(quantize(1.0, depth, false, true), ((1u32 << depth) - 1) as u16);
        }
    }

    #[test]
    fn libaom_high_bitdepth_and_chroma_profiles_produce_verified_av1c() {
        let strict = std::env::var("FILMCRAFT_REQUIRE_AV1_ORACLE").as_deref() == Ok("1");
        let ffmpeg = std::env::var_os("FILMCRAFT_FFMPEG").filter(|s| !s.is_empty()).unwrap_or_else(|| "ffmpeg".into());
        if Command::new(ffmpeg).arg("-version").output().is_err() {
            assert!(!strict, "FFmpeg/libaom AV1 is required by Linux conformance CI");
            return;
        }
        let rgba = vec![128u8; 64 * 64 * 4];
        for fmt in Av1PixelFormat::ALL {
            let settings = ExportSettings { format: Format::Av1, av1_pixel_format: fmt, bitrate_mode: BitrateMode::Crf, crf: 35.0, ..Default::default() };
            let mut enc = FfmpegAv1Encoder::new(64, 64, FrameRate::FPS_24, &settings).expect("FFmpeg encoder");
            for i in 0..2u64 {
                let f = EncoderFrame { width: 64, height: 64, rgba: &rgba, hdr: None, index: i };
                assert!(enc.encode(&f).expect("spool AV1 frame").is_empty());
            }
            let packets = enc.flush().expect("libaom AV1 encode");
            assert_eq!(packets.len(), 2, "one encoded sample per input frame");
            let entry = enc.final_sample_entry().expect("validated av1C").expect("video entry");
            // Test the actual muxed MP4, not just the intermediate IVF packet and av1C.
            let mut cursor = std::io::Cursor::new(Vec::new());
            {
                let mut writer =
                    filmcraft_isobmff::Mp4Writer::new(&mut cursor, filmcraft_isobmff::WriterOptions::new(filmcraft_isobmff::Brand::Mp4)).expect("MP4 writer");
                let track = writer.add_track(filmcraft_isobmff::TrackConfig::new(entry.clone(), 24)).expect("AV1 MP4 track");
                for packet in &packets {
                    writer
                        .write_sample(
                            track,
                            filmcraft_isobmff::WriteSample {
                                data: &packet.data,
                                duration: packet.duration,
                                composition_offset: packet.composition_offset,
                                is_sync: packet.key,
                            },
                        )
                        .expect("AV1 MP4 write");
                }
                writer.finish().expect("finalize MP4");
            }
            let mp4 = cursor.into_inner();
            let parsed = filmcraft_isobmff::open(&mp4).expect("read exported AV1 MP4");
            assert_eq!(parsed.tracks.len(), 1);
            assert_eq!(parsed.tracks[0].samples.len(), 2);
            // Independent decoder: verify libdav1d accepts the container and produces
            // exactly the intended number of pixels at the intended sample depth.
            let path = enc.ivf_path.with_extension("mp4");
            std::fs::write(&path, &mp4).expect("temporary AV1 MP4");
            let decoded = Command::new(&enc.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-xerror", "-c:v", "libdav1d", "-i"])
                .arg(&path)
                .args(["-pix_fmt", fmt.ffmpeg_name(), "-f", "rawvideo", "-"])
                .output()
                .expect("independent libdav1d");
            let _ = std::fs::remove_file(&path);
            assert!(decoded.status.success(), "{} independent libdav1d decode failed: {}", fmt.label(), String::from_utf8_lossy(&decoded.stderr));
            let (sx, sy) = fmt.chroma_shift();
            let samples = 64 * 64 + 2 * (64usize.div_ceil(1 << sx)) * (64usize.div_ceil(1 << sy));
            let bytes_per_sample = if fmt.bit_depth() > 8 { 2 } else { 1 };
            assert_eq!(decoded.stdout.len(), samples * bytes_per_sample * 2, "{}: wrong independent decoded frame count/plane sizes", fmt.label());
            match entry.codec {
                filmcraft_isobmff::CodecConfig::Av1(config) => {
                    assert_eq!((config.high_bitdepth, config.twelve_bit), (fmt.bit_depth() > 8, fmt.bit_depth() == 12));
                    let (sx, sy) = fmt.chroma_shift();
                    assert_eq!((config.chroma_subsampling_x, config.chroma_subsampling_y), (sx != 0, sy != 0));
                    assert!(!config.config_obus.is_empty(), "real AV1 sequence OBU required");
                }
                _ => panic!("expected av1C sample entry"),
            }
        }
    }
    #[test]
    fn libaom_hdr_pq_hlg_and_full_range_match_bitstream() {
        let strict = std::env::var("FILMCRAFT_REQUIRE_AV1_ORACLE").as_deref() == Ok("1");
        let ffmpeg = std::env::var_os("FILMCRAFT_FFMPEG").filter(|s| !s.is_empty()).unwrap_or_else(|| "ffmpeg".into());
        if Command::new(ffmpeg).arg("-version").output().is_err() {
            assert!(!strict, "FFmpeg/libaom is mandatory for Linux HDR AV1 conformance");
            return;
        }
        let hdr = vec![0.5f32; 64 * 64 * 3];
        for (signal, full_range, pixel_format) in [(ColorSignal::PQ, false, Av1PixelFormat::Yuv420p10), (ColorSignal::HLG, true, Av1PixelFormat::Yuv444p12)] {
            let settings = ExportSettings {
                format: Format::Av1,
                av1_pixel_format: pixel_format,
                av1_full_range: full_range,
                signal,
                bitrate_mode: BitrateMode::Crf,
                ..Default::default()
            };
            let mut encoder = FfmpegAv1Encoder::new(64, 64, FrameRate::FPS_24, &settings).expect("HDR libaom instance");
            let input = EncoderFrame { width: 64, height: 64, rgba: &[], hdr: Some(&hdr), index: 0 };
            encoder.encode(&input).expect("high precision HDR buffer");
            assert_eq!(encoder.flush().expect("libaom PQ/HLG stream").len(), 1);
            let entry = encoder.final_sample_entry().expect("validated HDR sequence header").expect("AV1 entry");
            let color = entry.video.as_ref().and_then(|v| v.color.as_ref()).expect("nclx");
            assert!(matches!(
                color,
                ColorInfo::Nclx { primaries, transfer, matrix, full_range: range }
                    if *primaries == 9 && *transfer == signal.transfer as u16 && *matrix == 9 && *range == full_range
            ));
        }
    }
}
