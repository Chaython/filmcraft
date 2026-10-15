# AV1 integration status (experimental)

This document tracks the integration work in fork pull request #1, based on upstream FilmCraft 0.8.0. **A successful merge and an AV1 format label do not prove a valid, lossless or HDR-correct encoded stream.** The independent acceptance gates below must pass before this work is released.

## Source provenance

| Upstream PR | Imported scope | Notes |
| --- | --- | --- |
| storytold/filmcraft#1185 | NVENC AV1 Main/Main10 encode, MP4/MOV, PQ/HLG, static HDR metadata and tests | Windows NVIDIA hardware only; claimed upstream tests must be rerun on this integration |
| storytold/filmcraft#1811 | Windows Media Foundation AV1 color-header accommodation and replay/reset tests | Requires GPU/driver acceptance; structural changes still decline |
| storytold/filmcraft#1795 | AV1 frame-allocation/chroma validation and hostile-input tests | Additional codec hardening in the original PR concerns other codecs and is not ported here |
| storytold/filmcraft#1182 | Pure Rust experimental AV1 baseline encoder and tests, integrated into export | 8-bit BT.709 4:2:0 software path only; not production equivalent to SVT-AV1 |
| storytold/filmcraft#880 | AMD AMF module and tests | Imported but **not registered** until pre-encode valid codec configuration is proved |
| storytold/filmcraft#1128 | Intel QSV module and tests | Imported but **not registered** until pre-encode valid codec configuration and 8/10-bit session capability are proved |
| storytold/filmcraft#1742 | AV1 av1C and Matroska pixel-format labels, tests | Correct labels do not imply decoding/encoding support for every named profile |

AV1 format recognition/MP4 export infrastructure (upstream #738 and #739) and parallel decoder postfilter (upstream #886) were already merged in the base and are not duplicated. The older, closed #829 patch overlaps newer focused changes, so it is not applied as a second competing NVENC implementation. Linux VA-API AV1 encoding (#409) is **not imported**: it requires an obsolete VA-API integration architecture, is 8-bit 4:2:0 SDR only, and explicitly declines certain dimensions because a driver produced incorrect coded dimensions.

## AV1 capabilities (do not overclaim)

| Feature | Encoding | Decoding |
| --- | --- | --- |
| 8-bit 4:2:0 BT.709 SDR | NVIDIA NVENC Windows; experimental Rust software fallback | Built-in Rust decoder; Windows hardware when available |
| 10-bit 4:2:0 BT.2020 PQ or HLG | NVIDIA NVENC Windows if hardware accepts Main10 | Built-in decoder; Windows hardware when available; monitor/color pipeline still needs display verification |
| 10-bit SDR, extended primaries/matrix/range | No general validated export path | Codec and metadata paths need matrix-specific fixture validation |
| 12-bit 4:2:0 / 4:2:2 / 4:4:4 | **Not implemented** | Not claimed; codec labels may recognize them |
| 8/10-bit 4:2:2 / 4:4:4, monochrome | **Not implemented** | Not claimed without independent conformance fixtures |
| Film grain | No complete encoder parity | Decoder has film-grain implementation; fixture validation needed |
| HDR10+ / Dolby Vision dynamic metadata | **Not implemented** | Not guaranteed preserved |
| Arbitrary ICC, Display-P3, ACES, full-range RGB | No generalized bit-exact export path | Not guaranteed preserved |

For AV1 export the Hardware Encoding Off toggle selects the 8-bit SDR software encoder. Auto prefers compatible NVENC on Windows and falls back to software for SDR when NVENC declines. On HDR export, an unavailable hardware Main10 encoder must return an actionable error or force a deliberately chosen SDR export; do not silently attach PQ/HLG signaling to the 8-bit software output.

## Safety gates

1. `cargo fmt --all -- --check`
2. `cargo check -p filmcraft-av1 -p filmcraft-av1enc -p filmcraft-codecs -p filmcraft-export -p filmcraft-platform --all-targets`
3. `cargo clippy -p filmcraft-av1 -p filmcraft-av1enc -p filmcraft-codecs -p filmcraft-export -p filmcraft-platform --all-targets -- -D warnings`
4. `cargo test --release -p filmcraft-av1 -p filmcraft-av1enc -p filmcraft-codecs -p filmcraft-export -p filmcraft-platform`
5. `cargo xtask ci` (all workspace gates)
6. Decode each exported AV1 stream with the built-in decoder AND an independent oracle (external ffmpeg/dav1d) and assert decoded frames, duration, dimensions, keyframe cadence, chroma, bit depth, color matrix/transfer/primaries, and metadata. Include MP4 and MOV, even/odd source dimensions, seek/reset/fallback, high bitrate, hostile input, SDR BT.709, PQ and HLG.
7. Run real RTX NVENC hardware export tests on Windows and verify actual 8-bit Main and 10-bit Main10 output; separately test AMD AMF and Intel QSV only after their factories generate a valid `av1C` **before** the MP4 muxer requests the sample entry.
8. Test 12-bit and Profile 1/2 with independent conformance vectors; report **unsupported** until actual bit-exact decode and encode are established.

### Known integration blockers

- CI may expose formatting/compilation regressions across upstream changes; keep PR unmerged until all checks pass.
- AMF and QSV currently construct `av1C` before their encoder has extracted a valid sequence header and may write an empty/default record, so their factories are deliberately disabled.
- NVENC support is restricted to the profile(s), bit depths and chroma formats the real GPU and driver actually accept; the source does not implement arbitrary AV1 profiles.
- The Rust software encoder is an early-stage implementation requiring quality and independent bitstream conformance testing.
- Linux VA-API AV1 encoding should be rebuilt against the current VA-API device architecture rather than replacing the current newer decoder stack with an old PR branch.
