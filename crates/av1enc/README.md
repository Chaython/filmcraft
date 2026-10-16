# filmcraft-av1enc — experimental native Rust AV1 encoder (review-only)

This crate is a **pure safe-Rust, clean-room AV1 encoder prototype**, targeting the AOMedia AV1 Bitstream & Decoding Process Specification v1.0.0 with Errata 1.

**Upstream separation:** this review-only branch contains no production FFmpeg/libaom exporter, no `ffmpeg` or `ffprobe` runtime invocation, and no FilmCraft export pipeline registration. It is a standalone library with tests, offered for comparison with upstream [#1182](https://github.com/storytold/filmcraft/pull/1182). It is **not** a replacement for, or independent proof of, that open focused proposal.

## Supported scope and status

- Experimental AV1 Profile 0, 8-bit, BT.709 SDR, 4:2:0 only.
- Pure-Rust bitstream syntax, intra prediction, preliminary P-frame/zero-motion reference path, experimental CDEF, transform/quantization and basic rate control.
- `#![deny(unsafe_code)]`; production library APIs return `Result`. Optional Rayon workers.
- `filmcraft-av1` and ISO-BMFF are used in **dev-tests** only; FFmpeg, when present, is strictly an optional external test oracle/fixture generator.
- **Not conformant or production ready:** an exported prototype stream from the combined integration was rejected by independent libdav1d decoding. In-tree decoder roundtrips and unit tests cannot substitute for external conformance. Further debugging and test vectors are needed before any product integration.
- No 10-/12-bit, 4:2:2, 4:4:4, PQ, HLG, dynamic HDR metadata, or hardware encode support in this crate.

## Separation from other upstream work

- Windows Media Foundation AV1 decode and Intel evidence remain solely in [#1811](https://github.com/storytold/filmcraft/pull/1811). This patch includes **none** of its files, generated evidence, or claims.
- NVENC hardware encoding belongs to [#1185](https://github.com/storytold/filmcraft/pull/1185), AMD AMF to [#880](https://github.com/storytold/filmcraft/pull/880), and Intel QSV to [#1128](https://github.com/storytold/filmcraft/pull/1128).
- The broad fork-only AV1 integration is [Chaython/filmcraft#1](https://github.com/Chaython/filmcraft/pull/1); it is not the source of production-policy claims for upstream.

## Local validation

```bash
cargo check -p filmcraft-av1enc --all-targets
cargo test -p filmcraft-av1enc
cargo clippy -p filmcraft-av1enc --all-targets -- -D warnings
```

These commands have **not yet** been rerun for the isolated review branch; results from the combined fork integration cannot be claimed as validation of this exact branch.
