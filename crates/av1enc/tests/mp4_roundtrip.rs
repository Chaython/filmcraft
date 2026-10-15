//! Test packaging encoded AV1 frames into an ISOBMFF / MP4 container.

use std::io::Cursor;

use filmcraft_av1enc::{Encoder, EncoderConfig, YuvFrame};
use filmcraft_isobmff::{Av1Config, Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};

#[test]
fn test_av1enc_mp4_mux_and_decode() {
    let (w, h) = (320u32, 240u32);
    let mut enc = Encoder::new(EncoderConfig::new(w, h, 24, 1)).unwrap();
    let y_plane = vec![128u8; (w * h) as usize];
    let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];
    let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

    let mut packets = Vec::new();
    for i in 0..5 {
        let p = enc.encode(&frame, i * 1000).unwrap();
        packets.push(p);
    }

    let av1c = Av1Config {
        seq_profile: 0,
        seq_level_idx_0: enc.sequence_level_idx(),
        seq_tier_0: false,
        high_bitdepth: false,
        twelve_bit: false,
        monochrome: false,
        chroma_subsampling_x: true,
        chroma_subsampling_y: true,
        chroma_sample_position: 0,
        initial_presentation_delay_minus_one: None,
        config_obus: enc.sequence_header_obu(),
    };

    let sample_entry = SampleEntry::av1(av1c, w as u16, h as u16);
    let track_cfg = TrackConfig::new(sample_entry, 24);

    let mut cursor = Cursor::new(Vec::new());
    let mut writer = Mp4Writer::new(&mut cursor, WriterOptions::new(Brand::Mp4)).unwrap();
    let track_id = writer.add_track(track_cfg).unwrap();

    for p in &packets {
        writer.write_sample(track_id, WriteSample { data: &p.data, duration: 1, is_sync: p.keyframe, composition_offset: 0 }).unwrap();
    }
    writer.finish().unwrap();

    let mp4_bytes = cursor.into_inner();
    assert!(!mp4_bytes.is_empty(), "MP4 bytes must not be empty");

    // Independent decoder oracle: acceptance must not rely solely on FilmCraft's own AV1 decoder.
    // ffmpeg is a test-only executable, not linked, bundled or shipped with FilmCraft.
    if std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        let path = filmcraft_testkit::temp_path(&filmcraft_testkit::fixtures_dir("av1enc-external-oracle").join("flat-main.mp4"));
        std::fs::write(&path, &mp4_bytes).unwrap();
        let decoded = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-v", "error", "-xerror", "-c:v", "libdav1d", "-i"])
            .arg(&path)
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(decoded.status.success(), "independent libdav1d AV1 decode failed: {}", String::from_utf8_lossy(&decoded.stderr));
        let plane_size = (w * h) as usize;
        let uv_size = (w / 2 * (h / 2)) as usize;
        let frame_size = plane_size + 2 * uv_size;
        assert_eq!(decoded.stdout.len(), frame_size * 5, "external decoder must output exactly five complete AV1 frames");
        for raw in decoded.stdout.chunks_exact(frame_size) {
            assert!(raw.iter().all(|&sample| sample == 128), "external AV1 decoder must reconstruct the exact constant YUV test image");
        }
    } else {
        assert!(std::env::var_os("FILMCRAFT_REQUIRE_AV1_ORACLE").is_none(), "FFmpeg with libdav1d is required by AV1 CI");
        eprintln!("SKIPPED independent AV1 oracle: ffmpeg is not installed");
    }

    // Open MP4 and verify demux
    let mp4 = filmcraft_isobmff::open(&mp4_bytes).unwrap();
    let tracks = &mp4.tracks;
    assert_eq!(tracks.len(), 1);
    let trk = &tracks[0];
    assert_eq!(trk.samples.len(), 5);

    // Decode all frames with AV1 decoder
    let mut dec = filmcraft_av1::Decoder::new();
    let mut decoded_pics = Vec::new();
    for s in &trk.samples {
        let sample_data = &mp4_bytes[s.offset as usize..s.offset as usize + s.size as usize];
        decoded_pics.extend(dec.decode(sample_data).unwrap());
    }
    decoded_pics.extend(dec.flush());

    assert_eq!(decoded_pics.len(), 5, "Decoded all 5 frames from MP4");
    assert_eq!(decoded_pics[0].width, w);
    assert_eq!(decoded_pics[0].height, h);
    assert_eq!(decoded_pics[0].planes[0][0], 128);

    let psnr = filmcraft_av1enc::compute_psnr_u16(&y_plane, &decoded_pics[0].planes[0]).unwrap();
    assert_eq!(psnr, 100.0, "Identical flat plane must have infinite/100dB PSNR");
}

#[test]
fn test_av1enc_mp4_mux_and_decode_gradient_psnr() {
    let (w, h) = (128u32, 128u32);
    let mut enc = Encoder::new(EncoderConfig::new(w, h, 30, 1)).unwrap();
    let mut y_plane = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            y_plane[(y * w + x) as usize] = (y * 2).min(255) as u8;
        }
    }
    let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];
    let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

    let p = enc.encode(&frame, 0).unwrap();
    assert!(!p.data.is_empty());

    let mut dec = filmcraft_av1::Decoder::new();
    let mut pics = dec.decode(&p.data).unwrap();
    pics.extend(dec.flush());
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].width, w);
    assert_eq!(pics[0].height, h);

    let psnr = filmcraft_av1enc::compute_psnr_u16(&y_plane, &pics[0].planes[0]).unwrap();
    println!("psnr: {psnr:.2} dB");
    for by in 0..4 {
        let mut line = String::new();
        for bx in 0..4 {
            let val = pics[0].planes[0][(by * 32 + 16) * w as usize + (bx * 32 + 16)];
            line.push_str(&format!("{val:4} "));
        }
        println!("grid by={by}: {line}");
    }
    // In intra mode with transform residuals and directional prediction, reconstructed PSNR improves.
    assert!(psnr > 10.0, "Reconstruction PSNR should be > 10 dB, got {psnr:.2} dB");
}

#[test]
fn test_av1enc_inter_p_frames_roundtrip() {
    let (w, h) = (320u32, 240u32);
    // key_int = 10, so frame 0 is KeyFrame, frames 1..5 are InterFrames
    let mut enc = Encoder::new(EncoderConfig::new(w, h, 24, 10)).unwrap();
    let mut y_plane = vec![100u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            y_plane[(y * w + x) as usize] = ((x + y) % 256) as u8;
        }
    }
    let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];
    let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

    let mut packets = Vec::new();
    for i in 0..6 {
        let p = enc.encode(&frame, i * 1000).unwrap();
        packets.push(p);
    }

    assert!(packets[0].keyframe, "Frame 0 must be a keyframe");
    println!("Frame 0 (KeyFrame) size = {} bytes", packets[0].data.len());
    for (i, p) in packets.iter().enumerate().skip(1) {
        assert!(!p.keyframe, "Frame {i} must be an inter (P) frame");
        println!("Frame {i} (Inter P-frame) size = {} bytes", p.data.len());
        // Static scene inter frames should be much smaller than keyframe
        assert!(p.data.len() < packets[0].data.len(), "Inter frame {i} size ({}) must be smaller than keyframe size ({})", p.data.len(), packets[0].data.len());
    }

    // Decode all frames with AV1 decoder
    let mut dec = filmcraft_av1::Decoder::new();
    let mut decoded = Vec::new();
    for p in &packets {
        decoded.extend(dec.decode(&p.data).unwrap());
    }
    decoded.extend(dec.flush());

    assert_eq!(decoded.len(), 6, "All 6 frames (1 Key + 5 Inter) must decode successfully");
    for (i, pic) in decoded.iter().enumerate() {
        assert_eq!(pic.width, w);
        assert_eq!(pic.height, h);
        let psnr = filmcraft_av1enc::compute_psnr_u16(&y_plane, &pic.planes[0]).unwrap();
        println!("Frame {i} decoded PSNR = {psnr:.2} dB");
        assert!(psnr > 9.0, "Frame {i} decoded PSNR must be > 9 dB, got {psnr:.2} dB");
    }
}

#[test]
fn test_av1enc_scene_cut_detection() {
    let (w, h) = (128u32, 128u32);
    // key_int is 50, so normally no keyframe until frame 50
    let mut enc = Encoder::new(EncoderConfig::new(w, h, 24, 50)).unwrap();

    let scene_a_y = vec![40u8; (w * h) as usize];
    let scene_b_y = vec![220u8; (w * h) as usize];
    let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];

    let frame_a = YuvFrame { y: &scene_a_y, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };
    let frame_b = YuvFrame { y: &scene_b_y, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

    let p0 = enc.encode(&frame_a, 0).unwrap();
    let p1 = enc.encode(&frame_a, 1000).unwrap();
    let p2 = enc.encode(&frame_a, 2000).unwrap();
    // Scene cut to Scene B at frame 3!
    let p3 = enc.encode(&frame_b, 3000).unwrap();
    let p4 = enc.encode(&frame_b, 4000).unwrap();

    assert!(p0.keyframe, "Frame 0 must be keyframe");
    assert!(!p1.keyframe, "Frame 1 must be P-frame");
    assert!(!p2.keyframe, "Frame 2 must be P-frame");
    assert!(p3.keyframe, "Frame 3 must be detected as SCENE CUT and encoded as KEYFRAME!");
    assert!(!p4.keyframe, "Frame 4 must be P-frame referencing Scene B keyframe");

    // All frames decode successfully
    let mut dec = filmcraft_av1::Decoder::new();
    let mut pics = Vec::new();
    for p in [&p0, &p1, &p2, &p3, &p4] {
        pics.extend(dec.decode(&p.data).unwrap());
    }
    pics.extend(dec.flush());

    assert_eq!(pics.len(), 5);
    assert_eq!(pics[0].planes[0][0], 40);
    assert_eq!(pics[3].planes[0][0], 220);
    assert_eq!(pics[4].planes[0][0], 220);
}
