//! Opt-in real-GPU AV1 acceptance tests. Hosted GitHub runners generally do not
//! have vendor video-encoding GPUs, so these require explicit environment opt-in.
//! FILMCRAFT_TEST_NVENC_AV1=1, FILMCRAFT_TEST_AMD_AV1=1 or
//! FILMCRAFT_TEST_INTEL_AV1=1 on a suitable Windows GPU host.
#[cfg(target_os = "windows")]
mod windows_gpu {
    use std::io::Cursor;
    use std::process::Command;

    use filmcraft_export::{Av1PixelFormat, ColorSignal, EncoderFrame, ExportSettings, Format, HardwareEncoding, VideoEncoder};
    use filmcraft_isobmff::{Brand, CodecConfig, Mp4Writer, TrackConfig, WriteSample, WriterOptions};
    use filmcraft_time::FrameRate;

    type HardwareFactory = fn(Format, u32, u32, FrameRate, &ExportSettings) -> Option<filmcraft_export::Result<Box<dyn VideoEncoder>>>;

    fn run_gpu_encoder(name: &str, factory: HardwareFactory, ten_bit: bool, hdr_signal: Option<ColorSignal>) {
        let hdr_input = hdr_signal.is_some();
        let w = 320u32;
        let h = 240u32;
        let settings = ExportSettings {
            format: Format::Av1,
            av1_pixel_format: if ten_bit { Av1PixelFormat::Yuv420p10 } else { Av1PixelFormat::Yuv420p8 },
            signal: hdr_signal.unwrap_or_default(),
            hardware_encoding: HardwareEncoding::Auto,
            ..Default::default()
        };
        let mut encoder = match factory(Format::Av1, w, h, FrameRate::FPS_24, &settings) {
            Some(Ok(enc)) => enc,
            Some(Err(e)) => panic!("{name} AV1 encoder creation failed: {e}"),
            None => panic!("{name} AV1 hardware session unavailable or unsupported"),
        };
        // A flat gray frame can decode successfully even when RGB-to-YUV
        // conversion is broken. Exercise spatially varying R/G, chroma and
        // opaque alpha in both the RGBA8 and float-RGB hardware inputs.
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let mut hdr = vec![0.0f32; (w * h * 3) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let red = x as f32 / (w - 1) as f32;
                let green = y as f32 / (h - 1) as f32;
                let i = y * w as usize + x;
                rgba[i * 4] = (red * 255.0).round() as u8;
                rgba[i * 4 + 1] = (green * 255.0).round() as u8;
                rgba[i * 4 + 2] = 128;
                rgba[i * 4 + 3] = 255;
                hdr[i * 3] = red;
                hdr[i * 3 + 1] = green;
                hdr[i * 3 + 2] = 0.5;
            }
        }
        let mut packets = Vec::new();
        for i in 0..16u64 {
            let frame = EncoderFrame { width: w, height: h, rgba: if hdr_input { &[] } else { &rgba }, hdr: hdr_input.then_some(hdr.as_slice()), index: i };
            packets.extend(encoder.encode(&frame).expect("encode 16 hardware AV1 frames"));
        }
        packets.extend(encoder.flush().expect("drain hardware encoder"));
        assert!(!packets.is_empty(), "{name} never emitted encoded packets");
        let entry = encoder.final_sample_entry().expect("validate hardware AV1 sequence header").expect("final AV1 sample entry");
        let CodecConfig::Av1(ref av1) = entry.codec else { panic!("{name} returned a non-AV1 sample entry") };
        assert!(!av1.config_obus.is_empty(), "{name} must output a genuine sequence-header OBU");
        assert_eq!(av1.high_bitdepth, ten_bit, "{name} output bit depth does not match requested Main/Main10");
        assert!(!av1.twelve_bit, "{name} unexpectedly wrote 12-bit AV1");
        assert!(av1.chroma_subsampling_x && av1.chroma_subsampling_y);
        let (_, header) = filmcraft_platform::nvenc::av1::extract_sequence_header_obu(&av1.config_obus)
            .expect("hardware AV1 config must contain an authentic Sequence Header OBU");
        let seq = filmcraft_av1::SequenceHeader::parse(header).expect("parse actual hardware AV1 sequence header");
        assert_eq!(seq.color.bit_depth, if ten_bit { 10 } else { 8 });
        if let Some(hdr) = hdr_signal {
            assert_eq!(
                (seq.color.color_primaries, seq.color.transfer_characteristics, seq.color.matrix_coefficients),
                (hdr.primaries, hdr.transfer, hdr.matrix),
                "{name} Main10 HDR bitstream signaling is missing or incorrect"
            );
        } else {
            assert_eq!(
                (seq.color.color_primaries, seq.color.transfer_characteristics, seq.color.matrix_coefficients),
                (1, 1, 1),
                "{name} BT.709 SDR bitstream is mislabeled"
            );
        }
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = Mp4Writer::new(&mut bytes, WriterOptions::new(Brand::Mp4)).expect("create hardware AV1 MP4");
            let track = writer.add_track(TrackConfig::new(entry, 24)).expect("AV1 track");
            for packet in &packets {
                writer
                    .write_sample(
                        track,
                        WriteSample { data: &packet.data, duration: packet.duration, is_sync: packet.key, composition_offset: packet.composition_offset },
                    )
                    .expect("mux hardware AV1 packet");
            }
            writer.finish().expect("finalize hardware AV1 MP4");
        }
        let bytes = bytes.into_inner();
        let parsed = filmcraft_isobmff::open(&bytes).expect("read hardware AV1 MP4");
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].samples.len(), packets.len());
        let path = std::env::temp_dir().join(format!("filmcraft-{name}-av1-gpu-{}.mp4", std::process::id()));
        std::fs::write(&path, &bytes).expect("temporary AV1 hardware MP4");
        let decoded = Command::new("ffmpeg")
            .args(["-hide_banner", "-v", "error", "-xerror", "-c:v", "libdav1d", "-i"])
            .arg(&path)
            .args(["-pix_fmt", if ten_bit { "yuv420p10le" } else { "yuv420p" }, "-f", "rawvideo", "-"])
            .output()
            .expect("independent ffmpeg/libdav1d must be installed for hardware acceptance");
        assert!(decoded.status.success(), "{name} hardware AV1 MP4 failed independent decoding: {}", String::from_utf8_lossy(&decoded.stderr));
        // Matroska is a lossless remux of the same finalized hardware AV1 packets:
        // assert that its decoded pixels are identical to the original MP4.
        let mkv = path.with_extension("mkv");
        let remux = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-v", "error", "-xerror", "-y", "-i"])
            .arg(&path)
            .args(["-map", "0", "-c", "copy", "-f", "matroska"])
            .arg(&mkv)
            .output()
            .expect("FFmpeg Matroska muxer must be installed for hardware AV1 acceptance");
        assert!(remux.status.success(), "{name} AV1 hardware MKV stream copy failed: {}", String::from_utf8_lossy(&remux.stderr));
        let mkv_decoded = Command::new("ffmpeg")
            .args(["-hide_banner", "-v", "error", "-xerror", "-c:v", "libdav1d", "-i"])
            .arg(&mkv)
            .args(["-pix_fmt", if ten_bit { "yuv420p10le" } else { "yuv420p" }, "-f", "rawvideo", "-"])
            .output()
            .expect("independent libdav1d Matroska decode");
        assert!(mkv_decoded.status.success(), "{name} MKV decoding failed: {}", String::from_utf8_lossy(&mkv_decoded.stderr));
        assert_eq!(mkv_decoded.stdout, decoded.stdout, "{name} changed decoded AV1 pixels during MP4-to-MKV stream copy");
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,pix_fmt,color_primaries,color_transfer,color_space",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&mkv)
            .output()
            .expect("ffprobe is required for real GPU acceptance");
        assert!(probe.status.success(), "{name} Matroska stream inspection failed");
        let metadata = String::from_utf8_lossy(&probe.stdout);
        assert!(metadata.contains("codec_name=av1"), "{name} MKV does not contain AV1: {metadata}");
        assert!(metadata.contains(if ten_bit { "pix_fmt=yuv420p10le" } else { "pix_fmt=yuv420p" }), "{name} MKV pixel format wrong: {metadata}");
        if let Some(hdr) = hdr_signal {
            let transfer = if hdr.transfer == 18 { "color_transfer=arib-std-b67" } else { "color_transfer=smpte2084" };
            for expected in ["color_primaries=bt2020", transfer, "color_space=bt2020nc"] {
                assert!(metadata.contains(expected), "{name} Matroska lost HDR signaling {expected}: {metadata}");
            }
        }
        let _ = std::fs::remove_file(&mkv);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            decoded.stdout.len(),
            (w * h * 3 / 2 * 16 * if ten_bit { 2 } else { 1 }) as usize,
            "{name} returned truncated or incorrectly framed AV1 video"
        );
        let row = (h / 2 * w) as usize;
        let left = row + 16;
        let right = row + (w - 17) as usize;
        let y_code = |index: usize| -> u16 {
            if ten_bit {
                let at = index * 2;
                u16::from_le_bytes([decoded.stdout[at], decoded.stdout[at + 1]])
            } else {
                u16::from(decoded.stdout[index])
            }
        };
        let (left_y, right_y) = (y_code(left), y_code(right));
        assert!(
            right_y > left_y + if ten_bit { 40 } else { 10 },
            "{name} encoded AV1 lost horizontal color-gradient luminance: left={left_y}, right={right_y}"
        );
    }

    #[test]
    fn amd_amf_av1_real_gpu_acceptance() {
        if std::env::var("FILMCRAFT_TEST_AMD_AV1").as_deref() != Ok("1") {
            eprintln!("SKIPPED: set FILMCRAFT_TEST_AMD_AV1=1 on a Windows AMD AV1 encoding GPU");
            return;
        }
        run_gpu_encoder("amd", filmcraft_platform::amf::export::factory, false, None);
    }

    #[test]
    fn intel_qsv_av1_real_gpu_acceptance() {
        if std::env::var("FILMCRAFT_TEST_INTEL_AV1").as_deref() != Ok("1") {
            eprintln!("SKIPPED: set FILMCRAFT_TEST_INTEL_AV1=1 on a Windows Intel AV1 encoding GPU");
            return;
        }
        run_gpu_encoder("intel", filmcraft_platform::qsv::export::factory, false, None);
    }
    #[test]
    fn nvidia_nvenc_main_and_main10_real_gpu_acceptance() {
        if std::env::var("FILMCRAFT_TEST_NVENC_AV1").as_deref() != Ok("1") {
            eprintln!("SKIPPED: set FILMCRAFT_TEST_NVENC_AV1=1 on an AV1-capable Windows NVIDIA GPU");
            return;
        }
        run_gpu_encoder("nvidia-main", filmcraft_platform::nvenc::export::factory, false, None);
        run_gpu_encoder("nvidia-main10-sdr", filmcraft_platform::nvenc::export::factory, true, None);
        run_gpu_encoder("nvidia-main10-pq", filmcraft_platform::nvenc::export::factory, true, Some(ColorSignal::PQ));
        run_gpu_encoder("nvidia-main10-hlg", filmcraft_platform::nvenc::export::factory, true, Some(ColorSignal::HLG));
    }
}
