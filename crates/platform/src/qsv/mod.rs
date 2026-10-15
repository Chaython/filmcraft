//! Hardware video encoding on Intel GPUs (Intel Quick Sync Video) via Media Foundation transforms.
//!
//! Provides hardware-accelerated H.264, HEVC (Main and Main 10 HDR), and AV1 export on Intel
//! integrated (Iris Xe, UHD) and discrete graphics (Arc A-series, B-series).

pub mod device;
pub mod export;
pub mod session;

use std::sync::OnceLock;

use filmcraft_time::FrameRate;
use self::device::IntelGpu;

static HEVC_AVAILABLE: OnceLock<bool> = OnceLock::new();
static HEVC_HDR_AVAILABLE: OnceLock<bool> = OnceLock::new();
static AV1_AVAILABLE: OnceLock<bool> = OnceLock::new();
static AV1_HDR_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Whether an Intel GPU is physically present on this system.
pub fn has_device() -> bool {
    IntelGpu::probe().ok().flatten().is_some()
}

/// Whether Intel Quick Sync Video hardware encoding is available on this system.
pub fn is_available() -> bool {
    hevc_available() || av1_available()
}

/// Whether an Intel GPU is present and supports HEVC hardware export.
pub fn hevc_available() -> bool {
    *HEVC_AVAILABLE.get_or_init(|| probe_codec_hevc(false))
}

/// Whether an Intel GPU is present and supports HEVC Main 10 (10-bit / HDR) hardware export.
pub fn hevc_hdr_available() -> bool {
    *HEVC_HDR_AVAILABLE.get_or_init(|| probe_codec_hevc(true))
}

/// Whether an Intel GPU is present and supports AV1 hardware export.
pub fn av1_available() -> bool {
    *AV1_AVAILABLE.get_or_init(|| probe_codec_av1(false))
}

/// Whether an Intel GPU is present and supports AV1 Main 10 (10-bit / HDR) hardware export.
pub fn av1_hdr_available() -> bool {
    *AV1_HDR_AVAILABLE.get_or_init(|| probe_codec_av1(true))
}

/// Warm up HEVC probes asynchronously or on background thread.
pub fn warm_hevc_probe() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("qsv-hevc-probe".into()).spawn(|| {
            let _ = std::panic::catch_unwind(hevc_available);
            let _ = std::panic::catch_unwind(hevc_hdr_available);
        }) {
            log::warn!("QSV HEVC probe thread was not started: {e}");
        }
    });
}

/// Warm up AV1 probes asynchronously.
pub fn warm_av1_probe() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("qsv-av1-probe".into()).spawn(|| {
            let _ = std::panic::catch_unwind(av1_available);
            let _ = std::panic::catch_unwind(av1_hdr_available);
        }) {
            log::warn!("QSV AV1 probe thread was not started: {e}");
        }
    });
}

#[cfg(target_os = "windows")]
fn probe_codec_hevc(ten_bit: bool) -> bool {
    probe_codec_win(session::Codec::Hevc, if ten_bit { session::Profile::HevcMain10 } else { session::Profile::HevcMain }, ten_bit)
}

#[cfg(target_os = "windows")]
fn probe_codec_av1(ten_bit: bool) -> bool {
    probe_codec_win(session::Codec::Av1, if ten_bit { session::Profile::Av1Main10 } else { session::Profile::Av1Main }, ten_bit)
}

#[cfg(target_os = "windows")]
fn probe_codec_win(codec: session::Codec, profile: session::Profile, ten_bit: bool) -> bool {
    if !has_device() { return false; }
    session::QsvSession::new(session::Config {
        codec, profile, size: (640, 360), rate: FrameRate::FPS_24,
        bitrate_kbps: 2_000, keyint: 24, ten_bit,
    }).is_ok()
}

#[cfg(not(target_os = "windows"))]
fn probe_codec_hevc(_ten_bit: bool) -> bool {
    IntelGpu::probe().ok().flatten().is_some()
}

#[cfg(not(target_os = "windows"))]
fn probe_codec_av1(_ten_bit: bool) -> bool {
    let Ok(Some(gpu)) = IntelGpu::probe() else {
        return false;
    };
    let id = gpu.device_id;
    (0x5690..=0x56c2).contains(&id) || (0x7d40..=0x7d67).contains(&id) || (0x64a0..=0x64b0).contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_id_is_intel() {
        assert_eq!(device::INTEL_VENDOR_ID, 0x8086);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn codec_subtypes_are_distinct() {
        use session::Codec;
        assert_ne!(Codec::H264.subtype(), Codec::Hevc.subtype());
        assert_ne!(Codec::Hevc.subtype(), Codec::Av1.subtype());
    }

    #[test]
    fn codec_names() {
        use session::Codec;
        assert_eq!(Codec::H264.name(), "H.264");
        assert_eq!(Codec::Hevc.name(), "HEVC");
        assert_eq!(Codec::Av1.name(), "AV1");
    }
}
