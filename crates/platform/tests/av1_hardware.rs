//! Opt-in real-GPU AV1 acceptance tests. Hosted GitHub runners generally do not
//! have AMD/Intel encoding GPUs, so these require explicit environment opt-in.
//! FILMCRAFT_TEST_AMD_AV1=1 / FILMCRAFT_TEST_INTEL_AV1=1 on a Windows GPU host.
#[cfg(target_os = "windows")]
mod windows_gpu {
    use std::io::Cursor;
    use std::process::Command;

    use filmcraft_export::{Av1PixelFormat, EncoderFrame, ExportSettings, Format, HardwareEncoding, VideoEncoder};
    use filmcraft_isobmff::{Brand, CodecConfig, Mp4Writer, TrackConfig, WriteSample, WriterOptions};
    use filmcraft_time::FrameRate;

    type HardwareFactory = fn(Format, u32, u32, FrameRate, &ExportSettings) -> Option<filmcraft_export::Result<Box<dyn VideoEncoder>>>;

    fn run_gpu_encoder(name: &str, factory: HardwareFactory) {
        let w = 320u32;
        let h = 240u32;
        let settings = ExportSettings {
            format: Format::Av1,
            av1_pixel_format: Av1PixelFormat::Yuv420p8,
            hardware_encoding: HardwareEncoding::Auto,
            ..Default::default()
        };
        let mut encoder = match factory(Format::Av1, w, h, FrameRate::FPS_24, &settings) {
            Some(Ok(enc)) => enc,
            Some(Err(e)) => panic!("{name} AV1 encoder creation failed: {e}"),
            None => panic!("{name} AV1 hardware session unavailable or unsupported"),
        };
        let rgba = vec![128u8; (w * h * 4) as usize];
        let mut packets = Vec::new();
        for i in 0..16u64 {
            let frame = EncoderFrame { width: w, height: h, rgba: &rgba, hdr: None, index: i };
            packets.extend(encoder.encode(&frame).expect("encode 16 hardware AV1 frames"));
        }
        packets.extend(encoder.flush().expect("drain hardware encoder"));
        assert!(!packets.is_empty(), "{name} never emitted encoded packets");
        let entry = encoder.final_sample_entry().expect("validate hardware AV1 sequence header").expect("final AV1 sample entry");
        let CodecConfig::Av1(ref av1) = entry.codec else { panic!("{name} returned a non-AV1 sample entry") };
        assert!(!av1.config_obus.is_empty(), "{name} must output a genuine sequence-header OBU");
        assert!(!av1.high_bitdepth, "8-bit hardware AV1 must not be labeled Main10");
        assert!(av1.chroma_subsampling_x && av1.chroma_subsampling_y);
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
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
            .output()
            .expect("independent ffmpeg/libdav1d must be installed for hardware acceptance");
        let _ = std::fs::remove_file(&path);
        assert!(decoded.status.success(), "{name} hardware AV1 MP4 failed independent decoding: {}", String::from_utf8_lossy(&decoded.stderr));
        assert_eq!(decoded.stdout.len(), (w * h * 3 / 2 * 16) as usize, "{name} returned truncated or incorrectly framed AV1 video");
    }

    #[test]
    fn amd_amf_av1_real_gpu_acceptance() {
        if std::env::var("FILMCRAFT_TEST_AMD_AV1").as_deref() != Ok("1") {
            eprintln!("SKIPPED: set FILMCRAFT_TEST_AMD_AV1=1 on a Windows AMD AV1 encoding GPU");
            return;
        }
        run_gpu_encoder("amd", filmcraft_platform::amf::export::factory);
    }

    #[test]
    fn intel_qsv_av1_real_gpu_acceptance() {
        if std::env::var("FILMCRAFT_TEST_INTEL_AV1").as_deref() != Ok("1") {
            eprintln!("SKIPPED: set FILMCRAFT_TEST_INTEL_AV1=1 on a Windows Intel AV1 encoding GPU");
            return;
        }
        run_gpu_encoder("intel", filmcraft_platform::qsv::export::factory);
    }
}
