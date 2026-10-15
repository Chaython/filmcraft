# filmcraft-av1enc

Clean-room pure-Rust baseline AV1 software encoder according to the AOMedia AV1 Bitstream &
Decoding Process Specification v1.0.0 with Errata 1.

## Conformance and Architecture

- **Specification**: AOMedia AV1 Specification v1.0.0 with Errata 1.
- **Pure safe Rust**: `#![deny(unsafe_code)]` with zero panics, no `unwrap` or `expect` in production code paths, returning `Result<T, Error>`.
- **Clean-room**: Pure arithmetic coding and bitstream synthesis implemented directly from mathematical specifications and official syntax tables without linking, bundling, or copying third-party code.
- **Verified decoding**: The generated Open Bitstream Units (Temporal Delimiter, Sequence Header, Frame Header, and Tile Group) are validated and bit-exact decoded by `filmcraft-av1::Decoder` and packaged by `filmcraft-isobmff` into standard MP4 files.

## Current Scope (experimental 8-bit software AV1)

This crate is a clean-room baseline AV1 encoder for **8-bit BT.709 SDR, 4:2:0** source frames. It integrates with FilmCraft's AV1 export factory as a software fallback. Its actual emitted packets, frame quality and independent decoder conformance must be verified before production use.

- **Bitstream:** Sequence headers, frame headers, tile groups, OBU framing and AV1 entropy coding.
- **Prediction:** Intra modes and a preliminary P-frame/zero-motion reference path. This is not a complete AV1 motion search implementation.
- **Residuals:** Transform/quantization and coefficient coding paths are present. Existing software-oracle tests are not sufficient to claim all AV1 modes conform.
- **Rate control:** Baseline CBR/VBR/CRF-like and direct qindex selection, with provisional scene-cut logic and adaptive quantization.
- **Filters:** Experimental in-loop CDEF; filter coverage and reconstruction equivalence require independent test vectors.
- **Concurrency:** Rayon is optional through the `threads` feature. No thread count or high-resolution performance guarantee has been established on this integration.

## Known limitations

- The encoder uses **Profile 0 8-bit 4:2:0 only**. It cannot write 10-bit, 12-bit, 4:2:2, 4:4:4, monochrome, full-range RGB, PQ or HLG output.
- It does not provide the complete AV1 partition/motion-estimation/tool set of a production encoder.
- Fidelity, compression ratios and decoder interoperability require independent ffmpeg/dav1d testing of multiple real-frame and hostile-input fixtures; passing a proprietary/in-tree decoder test alone is insufficient.
- Hardware AV1 Main/Main10 exports are a separate NVENC implementation. Failure to encode HDR must not silently fall back to this 8-bit path and retain HDR signaling.

## Layering

As an L0 codec crate in FilmCraft's architecture, `filmcraft-av1enc` depends only on `filmcraft-bitstream` plus its optional Rayon concurrency dependency, with no platform FFI.
