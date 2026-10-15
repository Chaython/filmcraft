# AV1 integration status (experimental)

This document tracks the integration work in fork pull request #1, based on upstream FilmCraft 0.8.0. **A successful merge and an AV1 format label do not prove a valid, lossless or HDR-correct encoded stream.** The independent acceptance gates below must pass before this work is released.

## Source provenance

| Upstream PR | Imported scope | Notes |
| --- | --- | --- |
| storytold/filmcraft#1185 | NVENC AV1 Main/Main10 encode, MP4/MOV, PQ/HLG, static HDR metadata and tests | Windows NVIDIA hardware only; claimed upstream tests must be rerun on this integration |
| storytold/filmcraft#1811 | Windows Media Foundation AV1 color-header accommodation and replay/reset tests | Requires GPU/driver acceptance; structural changes still decline |
| storytold/filmcraft#1795 | AV1 frame-allocation/chroma validation and hostile-input tests | Additional codec hardening in the original PR concerns other codecs and is not ported here |
| storytold/filmcraft#1182 | Pure Rust experimental AV1 baseline encoder and tests, integrated into export | 8-bit BT.709 4:2:0 software path only; not production equivalent to SVT-AV1 |
| storytold/filmcraft#880 | AMD AMF module and tests | Imported and available behind an experimental opt-in; actual driver output must contain parseable AV1 sequence metadata |
| storytold/filmcraft#1128 | Intel QSV module and tests | Imported and available behind an experimental opt-in; actual driver/session validation remains outstanding |
| storytold/filmcraft#1742 | AV1 av1C and Matroska pixel-format labels, tests | Correct labels do not imply decoding/encoding support for every named profile |

AV1 format recognition/MP4 export infrastructure (upstream #738 and #739) and parallel decoder postfilter (upstream #886) were already merged in the base and are not duplicated. The older, closed #829 patch overlaps newer focused changes, so it is not applied as a second competing NVENC implementation. Linux VA-API AV1 encoding (#409) is **not imported**: it requires an obsolete VA-API integration architecture, is 8-bit 4:2:0 SDR only, and explicitly declines certain dimensions because a driver produced incorrect coded dimensions.

## AV1 capabilities and advanced encoder

The export panel now offers nine pixel formats: **8/10/12-bit × 4:2:0/4:2:2/4:4:4**, plus limited/full-range signaling. The native Rust encoder remains 8-bit 4:2:0 BT.709 SDR. The advanced profiles use a **local FFmpeg executable with libaom-av1**, not a built-in 12-bit AV1 implementation.

- Default 8-bit 4:2:0 with Hardware Auto: NVENC on supported Windows NVIDIA GPUs, otherwise the experimental Rust encoder. HDR-capable NVENC can select Main10 from an HDR working sequence.
- Other pixel formats or Full Range: FFmpeg/libaom software backend; 10/12-bit formats may use HDR PQ/HLG source floats when the timeline works in Rec.2100 and SDR conversion was not requested. Source clips/timeline in 8-bit Rec.709 cannot be upgraded to genuine 12-bit precision by selecting a 12-bit output.
- FFmpeg installation: set `FILMCRAFT_FFMPEG` to the full executable path, or place `ffmpeg` on PATH. This integration does not download FFmpeg or bundle codecs. Unsupported libaom build profiles fail with the encoder's real diagnostics.
- The advanced backend writes planar little-endian YUV, invokes `libaom-av1` without shell execution, then validates the AV1 sequence header's bit depth, subsampling, primaries, transfer, matrix, and range against the requested settings before finalizing MP4/MOV `av1C`. Temporary raw/IVF files are cleaned up. **Current limit:** its one-shot IVF flush is bounded to 1 GiB of encoded output; large/long exports need a streaming mux/encoder refactor.
- Windows AMD AMF and Intel QSV have opt-in runtime registration through `FILMCRAFT_EXPERIMENTAL_AV1_ENCODERS=1`. They extract actual output sequence headers, update `av1C` at mux finalization, and reject missing/mismatched headers. These are not yet hardware-validated or enabled by default.

| Feature | Current path | Verification |
| --- | --- | --- |
| 8-bit 4:2:0 SDR | Native Rust or NVENC; optional libaom when Full Range | Native round-trip; NVENC needs real GPU |
| 10-bit 4:2:0 PQ/HLG | NVENC Main10 when supported; FFmpeg/libaom | New high-depth backend requires real FFmpeg CI and color-metadata validation |
| 12-bit 4:2:0 | FFmpeg/libaom | Automated high-depth encode test added |
| 8/10/12-bit 4:2:2 and 4:4:4 | FFmpeg/libaom | Profile-specific output header validation; CI test added |
| AV1 8/10/12-bit decode | Built-in decoder, hardware when available | Bit-depth/chroma parity fixtures against libdav1d |
| Full range / matrix / primaries | Advanced FFmpeg YUV converter supports BT.709, BT.601, BT.2020 NCL and container metadata | Source color-space conversion beyond Rec.709 and Rec.2100 still limited |
| HDR10 PQ / HLG | Existing Rec.2100 timeline pipeline; 10/12-bit FFmpeg path, NVENC Main10 | Static mastering boxes; end-to-end external playback verification needed |
| HDR10+ / Dolby Vision dynamic metadata | Not implemented | Missing dynamic metadata OBU propagation |
| ICC profiles, arbitrary P3/ACES gamut conversions | Not implemented for AV1 export | Requires real gamut/transfer conversion, not just relabeling |
| 12-bit hardware encode | Not advertised | Depends on GPU/driver; do not claim unsupported profiles |
| Linux VA-API AV1 encoding | Not ported | Old upstream #409 targets obsolete platform internals |

FilmCraft does not silently label an 8-bit encode HDR or claim unsupported bit depths. The new external encoder also rejects malformed output or mismatched color information rather than writing an invalid AV1 sample entry.

## Safety gates

1. `cargo fmt --all -- --check`
2. `cargo check -p filmcraft-av1 -p filmcraft-av1enc -p filmcraft-codecs -p filmcraft-export -p filmcraft-platform --all-targets`
3. `cargo clippy -p filmcraft-av1 -p filmcraft-av1enc -p filmcraft-codecs -p filmcraft-export -p filmcraft-platform --all-targets -- -D warnings`
4. `cargo test --release -p filmcraft-av1 -p filmcraft-av1enc -p filmcraft-codecs -p filmcraft-export -p filmcraft-platform`
5. `cargo xtask ci` (all workspace gates)
6. Decode each exported AV1 stream with the built-in decoder AND an independent oracle (external ffmpeg/dav1d) and assert decoded frames, duration, dimensions, keyframe cadence, chroma, bit depth, color matrix/transfer/primaries, and metadata. Include MP4 and MOV, even/odd source dimensions, seek/reset/fallback, high bitrate, hostile input, SDR BT.709, PQ and HLG.
7. Run real RTX NVENC hardware export tests on Windows and verify actual 8-bit Main and 10-bit Main10 output; separately test AMD AMF and Intel QSV on real GPUs and verify the final `av1C` extracted from the actual encoded sequence header.
8. Test 12-bit and Profile 1/2 output using external libaom and independent libdav1d; do not confuse optional external encode support with native Rust encoding parity.

### Known integration blockers

- CI may expose formatting/compilation regressions across upstream changes; keep PR unmerged until all checks pass.
- AMF and QSV stage an interim track entry and must replace it with a sequence-header-derived `av1C` before mux finalization; they error if they cannot validate it. Registration is off by default pending real GPU acceptance.
- NVENC support is restricted to the profile(s), bit depths and chroma formats the real GPU and driver actually accept; the source does not implement arbitrary AV1 profiles.
- The Rust software encoder is an early-stage implementation requiring quality and independent bitstream conformance testing.
- Linux VA-API AV1 encoding should be rebuilt against the current VA-API device architecture rather than replacing the current newer decoder stack with an old PR branch.
