//! Independent AV1 conformance smoke test for the experimental native encoder.
//! FFmpeg/libdav1d is used ONLY as an external test oracle, never as product code.
//! This intentionally tests changing images and P-frames, not only all-128 input.
use std::process::{Command, Stdio};

use filmcraft_av1enc::{Encoder, EncoderConfig, YuvFrame};

fn ivf(w: u16, h: u16, fps: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut ivf = Vec::new();
    ivf.extend_from_slice(b"DKIF");
    ivf.extend_from_slice(&0u16.to_le_bytes());
    ivf.extend_from_slice(&32u16.to_le_bytes());
    ivf.extend_from_slice(b"AV01");
    ivf.extend_from_slice(&w.to_le_bytes());
    ivf.extend_from_slice(&h.to_le_bytes());
    ivf.extend_from_slice(&fps.to_le_bytes());
    ivf.extend_from_slice(&1u32.to_le_bytes());
    ivf.extend_from_slice(&(packets.len() as u32).to_le_bytes());
    ivf.extend_from_slice(&0u32.to_le_bytes());
    for (i, packet) in packets.iter().enumerate() {
        ivf.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        ivf.extend_from_slice(&(i as u64).to_le_bytes());
        ivf.extend_from_slice(packet);
    }
    ivf
}

#[test]
fn native_changing_frames_decode_independently() {
    let Some(ffmpeg) = filmcraft_testkit::ffmpeg_or_skip("native AV1 changing-frame conformance") else {
        assert_ne!(std::env::var("FILMCRAFT_REQUIRE_AV1_ORACLE").as_deref(), Ok("1"), "FFmpeg with libdav1d is mandatory for this test");
        return;
    };
    let (w, h) = (128usize, 128usize);
    let mut encoder = Encoder::new(EncoderConfig::new(w as u32, h as u32, 24, 10)).unwrap();
    let mut packets = Vec::new();
    let mut input_planes = Vec::new();
    for i in 0..5usize {
        let mut y = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                // Gradient plus a moving edge and a scene change.
                y[row * w + col] = match i {
                    0 => ((col * 2 + row) % 256) as u8,
                    1 => ((col * 2 + row + 17) % 256) as u8,
                    2 => {
                        if col < 64 {
                            40
                        } else {
                            220
                        }
                    }
                    3 => {
                        if col < 80 {
                            40
                        } else {
                            220
                        }
                    }
                    _ => ((row * 2 + col) % 256) as u8,
                };
            }
        }
        let u = vec![96u8; w * h / 4];
        let v = vec![160u8; w * h / 4];
        let frame = YuvFrame { y: &y, u: &u, v: &v, y_stride: w, uv_stride: w / 2 };
        let packet = encoder.encode(&frame, (i * 1000) as i64).unwrap();
        packets.push(packet.data);
        input_planes.push(y);
    }
    let path = filmcraft_testkit::temp_path(&filmcraft_testkit::fixtures_dir("av1enc-external-oracle").join("changing.ivf"));
    let frame_size = w * h * 3 / 2;
    for count in 1..=packets.len() {
        std::fs::write(&path, ivf(w as u16, h as u16, 24, &packets[..count])).unwrap();
        let decoded = Command::new(&ffmpeg)
            .args(["-hide_banner", "-v", "error", "-xerror", "-c:v", "libdav1d", "-f", "ivf", "-i"])
            .arg(&path)
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            decoded.status.success(),
            "independent libdav1d first rejected native AV1 at prefix {count}, packet lengths {:?}: {}",
            packets.iter().map(Vec::len).collect::<Vec<_>>(),
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert_eq!(decoded.stdout.len(), count * frame_size, "independent decoder must produce exactly {count} frames");
        for (i, frame) in decoded.stdout.chunks_exact(frame_size).enumerate() {
            let psnr = filmcraft_av1enc::compute_psnr(&input_planes[i], &frame[..w * h]).unwrap();
            assert!(psnr > 9.0, "independently decoded native AV1 frame {i} has poor luma quality: {psnr:.2} dB");
        }
    }
}

#[test]
fn diagnose_first_gradient_frame_coding_modes() {
    use filmcraft_av1enc::Preset;

    let Some(ffmpeg) = filmcraft_testkit::ffmpeg_or_skip("native AV1 coding-mode oracle") else { return };
    let (w, h) = (128usize, 128usize);
    let y: Vec<u8> = (0..w * h).map(|i| (((i % w) * 2 + (i / w)) % 256) as u8).collect();
    let uv = vec![128u8; w * h / 4];
    let mut valid_modes = 0usize;
    for preset in [Preset::Speed, Preset::Balanced, Preset::Quality] {
        for cdef in [false, true] {
            for qindex in [0u8, 12, 100, 220] {
                let mut cfg = EncoderConfig::new(w as u32, h as u32, 24, 1);
                cfg.preset = preset;
                cfg.qindex = qindex;
                cfg.enable_cdef = cdef;
                let mut encoder = Encoder::new(cfg).unwrap();
                let packet = encoder.encode(&YuvFrame { y: &y, u: &uv, v: &uv, y_stride: w, uv_stride: w / 2 }, 0).unwrap();
                let path = filmcraft_testkit::temp_path(&filmcraft_testkit::fixtures_dir("av1enc-external-oracle").join("mode.ivf"));
                std::fs::write(&path, ivf(w as u16, h as u16, 24, &[packet.data.clone()])).unwrap();
                let output = Command::new(&ffmpeg)
                    .args(["-hide_banner", "-v", "error", "-xerror", "-c:v", "libdav1d", "-f", "ivf", "-i"])
                    .arg(&path)
                    .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                let _ = std::fs::remove_file(&path);
                let valid = output.status.success() && output.stdout.len() == w * h * 3 / 2;
                valid_modes += usize::from(valid);
                println!("av1 mode={preset:?} cdef={cdef} qindex={qindex} bytes={} independent_decode={valid}", packet.data.len());
            }
        }
    }
    println!("AV1 coding-mode oracle: {valid_modes}/24 combinations decode independently");
}

#[test]
fn diagnose_first_gradient_chroma_values() {
    let Some(ffmpeg) = filmcraft_testkit::ffmpeg_or_skip("native AV1 chroma oracle") else { return };
    let (w, h) = (128usize, 128usize);
    let y: Vec<u8> = (0..w * h).map(|i| (((i % w) * 2 + (i / w)) % 256) as u8).collect();
    for (u_val, v_val) in [
        (128u8, 128u8),
        (127, 128),
        (126, 128),
        (112, 128),
        (96, 128),
        (128, 129),
        (128, 130),
        (128, 160),
        (96, 160),
        (96, 96),
        (160, 160),
        (0, 0),
        (255, 255),
    ] {
        let u = vec![u_val; w * h / 4];
        let v = vec![v_val; w * h / 4];
        let mut encoder = Encoder::new(EncoderConfig::new(w as u32, h as u32, 24, 10)).unwrap();
        let packet = encoder.encode(&YuvFrame { y: &y, u: &u, v: &v, y_stride: w, uv_stride: w / 2 }, 0).unwrap();
        let path = filmcraft_testkit::temp_path(&filmcraft_testkit::fixtures_dir("av1enc-external-oracle").join("chroma.ivf"));
        std::fs::write(&path, ivf(w as u16, h as u16, 24, &[packet.data.clone()])).unwrap();
        let output = Command::new(&ffmpeg)
            .args(["-hide_banner", "-v", "error", "-xerror", "-c:v", "libdav1d", "-f", "ivf", "-i"])
            .arg(&path)
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        let valid = output.status.success() && output.stdout.len() == w * h * 3 / 2;
        println!("chroma U={u_val} V={v_val} bytes={} independent_decode={valid}", packet.data.len());
    }
}
