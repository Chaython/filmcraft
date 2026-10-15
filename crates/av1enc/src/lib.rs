//! Clean-room pure-Rust baseline AV1 software encoder.
//!
//! Encodes conformant AV1 streams according to the AOMedia AV1 Specification v1.0.0.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod coeff;
pub mod frame_header;
pub mod intra;
pub mod obu;
pub mod symbol;
pub mod tables;
pub mod transform;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid AV1 encoder configuration: {0}")]
    Invalid(&'static str),
    #[error("input frame buffer too small")]
    BufferTooSmall,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Rate control mode for AV1 software encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateControl {
    /// Constant Quality / CRF-like mode (crf 0.0..63.0, mapped to qindex 0..255).
    ConstantQuality(f32),
    /// Direct quantization index (0 = lossless, 255 = maximum compression).
    Qindex(u8),
    /// 1-pass adaptive CBR rate control targeting `kbps`.
    Cbr { kbps: u32 },
    /// Variable Bitrate with target and peak rate cap.
    Vbr { target_kbps: u32, max_kbps: u32 },
}

/// Encoding speed vs quality preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Preset {
    Speed,
    #[default]
    Balanced,
    Quality,
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub qindex: u8,
    pub key_int: u32,
    pub rate_control: Option<RateControl>,
    pub preset: Preset,
    pub spatial_aq: bool,
    pub enable_cdef: bool,
    pub scene_cut_detection: bool,
}

impl EncoderConfig {
    pub fn new(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Self {
        Self {
            width,
            height,
            fps_num,
            fps_den,
            qindex: 100,
            key_int: 30,
            rate_control: None,
            preset: Preset::Balanced,
            spatial_aq: false,
            enable_cdef: true,
            scene_cut_detection: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

pub struct YuvFrame<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

#[derive(Debug)]
pub struct Encoder {
    config: EncoderConfig,
    frame_count: u64,
    active_qindex: u8,
    bit_reservoir: i64,
    recon_y: Vec<u8>,
    recon_u: Vec<u8>,
    recon_v: Vec<u8>,
    ref_y: Vec<u8>,
    ref_u: Vec<u8>,
    ref_v: Vec<u8>,
    ref_src_y: Vec<u8>,
    last_key_frame: u64,
}

#[inline]
fn compute_block_sad(src: &[u8], src_stride: usize, ref_plane: &[u8], ref_stride: usize, x0: usize, y0: usize, w: usize, h: usize) -> u64 {
    let mut sad = 0u64;
    for r in 0..h {
        let Some(y) = y0.checked_add(r) else { continue };
        let Some(src_off) = y.checked_mul(src_stride).and_then(|o| o.checked_add(x0)) else { continue };
        let Some(ref_off) = y.checked_mul(ref_stride).and_then(|o| o.checked_add(x0)) else { continue };
        let (Some(src_end), Some(ref_end)) = (src_off.checked_add(w), ref_off.checked_add(w)) else { continue };
        if let (Some(s_row), Some(r_row)) = (src.get(src_off..src_end), ref_plane.get(ref_off..ref_end)) {
            let (chunks_s, rem_s) = s_row.as_chunks::<16>();
            let (chunks_r, rem_r) = r_row.as_chunks::<16>();
            for (cs, cr) in chunks_s.iter().zip(chunks_r.iter()) {
                let mut sum16 = 0u32;
                for i in 0..16 {
                    sum16 += cs[i].abs_diff(cr[i]) as u32;
                }
                sad += sum16 as u64;
            }
            for (&s, &re) in rem_s.iter().zip(rem_r.iter()) {
                sad += s.abs_diff(re) as u64;
            }
        }
    }
    sad
}

#[inline]
fn copy_block(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, x0: usize, y0: usize, w: usize, h: usize) {
    for r in 0..h {
        let Some(y) = y0.checked_add(r) else { continue };
        let Some(src_off) = y.checked_mul(src_stride).and_then(|o| o.checked_add(x0)) else { continue };
        let Some(dst_off) = y.checked_mul(dst_stride).and_then(|o| o.checked_add(x0)) else { continue };
        let (Some(src_end), Some(dst_end)) = (src_off.checked_add(w), dst_off.checked_add(w)) else { continue };
        if let (Some(s), Some(d)) = (src.get(src_off..src_end), dst.get_mut(dst_off..dst_end)) {
            d.copy_from_slice(s);
        }
    }
}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        if config.width == 0
            || config.height == 0
            || config.width > 8192
            || config.height > 8192
            || u64::from(config.width) * u64::from(config.height) > 8192 * 4320
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(Error::Invalid("dimensions must be even and within the 8K limit"));
        }
        if config.fps_num == 0 || config.fps_den == 0 || config.key_int == 0 {
            return Err(Error::Invalid("frame rate and keyframe interval must be positive"));
        }
        if let Some(rc) = config.rate_control {
            match rc {
                RateControl::Cbr { kbps: 0 } => {
                    return Err(Error::Invalid("CBR bitrate must be positive"));
                }
                RateControl::Vbr { target_kbps, max_kbps } if target_kbps == 0 || max_kbps < target_kbps => {
                    return Err(Error::Invalid("VBR target bitrate must be positive and <= max bitrate"));
                }
                _ => {}
            }
        }
        let initial_qindex = match config.rate_control {
            Some(RateControl::ConstantQuality(crf)) => (crf * 4.0).round().clamp(0.0, 255.0) as u8,
            Some(RateControl::Qindex(q)) => q,
            Some(RateControl::Cbr { .. } | RateControl::Vbr { .. }) => config.qindex,
            None => config.qindex,
        };
        let y_len = (config.width as usize).saturating_mul(config.height as usize);
        let uv_len = ((config.width as usize) / 2).saturating_mul((config.height as usize) / 2);
        Ok(Self {
            config,
            frame_count: 0,
            active_qindex: initial_qindex,
            bit_reservoir: 0,
            recon_y: vec![128; y_len],
            recon_u: vec![128; uv_len],
            recon_v: vec![128; uv_len],
            ref_y: vec![128; y_len],
            ref_u: vec![128; uv_len],
            ref_v: vec![128; uv_len],
            ref_src_y: vec![0; y_len],
            last_key_frame: 0,
        })
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    pub fn sequence_header_obu(&self) -> Vec<u8> {
        let seq_cfg = obu::SequenceConfig {
            width: self.config.width,
            height: self.config.height,
            bit_depth: 8,
            mono_chrome: false,
            enable_cdef: self.config.enable_cdef,
        };
        obu::write_sequence_header(&seq_cfg)
    }

    pub fn encode(&mut self, frame: &YuvFrame<'_>, pts: i64) -> Result<Packet> {
        let width = self.config.width as usize;
        let height = self.config.height as usize;

        if frame.y_stride < width {
            return Err(Error::Invalid("y_stride must be at least frame width"));
        }
        let uv_w = width / 2;
        let uv_h = height / 2;
        if frame.uv_stride < uv_w {
            return Err(Error::Invalid("uv_stride must be at least half frame width"));
        }

        let required_y = height
            .checked_sub(1)
            .and_then(|h| h.checked_mul(frame.y_stride))
            .and_then(|o| o.checked_add(width))
            .ok_or(Error::Invalid("frame buffer dimensions or stride overflow"))?;
        if frame.y.len() < required_y {
            return Err(Error::BufferTooSmall);
        }

        let required_uv = uv_h
            .checked_sub(1)
            .and_then(|h| h.checked_mul(frame.uv_stride))
            .and_then(|o| o.checked_add(uv_w))
            .ok_or(Error::Invalid("chroma buffer dimensions or stride overflow"))?;
        if frame.u.len() < required_uv || frame.v.len() < required_uv {
            return Err(Error::BufferTooSmall);
        }

        let mut is_key = self.frame_count == 0 || (self.frame_count.saturating_sub(self.last_key_frame)) >= u64::from(self.config.key_int);

        if !is_key && self.config.scene_cut_detection && self.frame_count > 0 {
            // Fast subsampled SAD across the frame (step by 4 rows and 4 cols: only 1/16th of pixels)
            let mut diff_sum = 0u64;
            let mut sample_count = 0u64;
            for r in (0..height).step_by(4) {
                let Some(curr_off) = r.checked_mul(frame.y_stride) else { continue };
                let Some(prev_off) = r.checked_mul(width) else { continue };
                let (Some(curr_end), Some(prev_end)) = (curr_off.checked_add(width), prev_off.checked_add(width)) else { continue };
                if let (Some(curr_row), Some(prev_row)) = (frame.y.get(curr_off..curr_end), self.ref_src_y.get(prev_off..prev_end)) {
                    for (&c, &p) in curr_row.iter().step_by(4).zip(prev_row.iter().step_by(4)) {
                        diff_sum += c.abs_diff(p) as u64;
                        sample_count += 1;
                    }
                }
            }
            if sample_count > 0 && (diff_sum / sample_count) > 42 {
                is_key = true;
            }
        }

        if is_key {
            self.last_key_frame = self.frame_count;
        }

        let frame_type = if is_key { frame_header::KEY_FRAME } else { frame_header::INTER_FRAME };
        let order_hint = (self.frame_count & 0x7f) as u32;
        let seq_cfg = obu::SequenceConfig {
            width: self.config.width,
            height: self.config.height,
            bit_depth: 8,
            mono_chrome: false,
            enable_cdef: self.config.enable_cdef,
        };

        // Determine base quantization index for this frame
        let mut qindex = match self.config.rate_control {
            Some(RateControl::ConstantQuality(crf)) => (crf * 4.0).round().clamp(0.0, 255.0) as u8,
            Some(RateControl::Qindex(q)) => q,
            Some(RateControl::Cbr { .. } | RateControl::Vbr { .. }) => self.active_qindex,
            None => self.config.qindex,
        };

        // IP offset: Boost Keyframe quality slightly to establish superior reference for downstream P-frames
        if is_key {
            qindex = qindex.saturating_sub(4).max(12);
        }

        if self.config.spatial_aq && !frame.y.is_empty() {
            let var = intra::block_variance(frame.y, frame.y_stride, self.config.width as usize, self.config.height as usize);
            if var > 3000 {
                qindex = qindex.saturating_add(6);
            } else if var > 1500 {
                qindex = qindex.saturating_add(3);
            } else if var < 80 {
                qindex = qindex.saturating_sub(6).max(12);
            } else if var < 250 {
                qindex = qindex.saturating_sub(3).max(16);
            }
        }

        let fh_cfg =
            frame_header::FrameHeaderConfig { frame_type, show_frame: true, error_resilient_mode: false, disable_cdf_update: true, qindex, order_hint };

        let mut tu = Vec::with_capacity(512);
        tu.extend_from_slice(&obu::write_temporal_delimiter());
        if is_key || self.frame_count == 0 {
            tu.extend_from_slice(&obu::write_sequence_header(&seq_cfg));
        }
        tu.extend_from_slice(&obu::write_obu(obu::OBU_FRAME_HEADER, &frame_header::write_frame_header(&fh_cfg, &seq_cfg)));

        let sb_cols = self.config.width.div_ceil(64);
        let sb_rows = self.config.height.div_ceil(64);
        let total_sbs = (sb_cols as usize).saturating_mul(sb_rows as usize);
        let mut sym = symbol::SymbolEncoder::with_capacity(total_sbs.saturating_mul(4).max(64));

        #[derive(Clone, Copy, Debug)]
        struct SbAnalysis {
            mode: u8,
            split: bool,
            is_flat: bool,
            is_inter: bool,
        }

        let ref_y_plane = &self.ref_y;
        let ref_src_y_plane = &self.ref_src_y;
        let has_prev_frame = self.frame_count > 0;

        // Row Analysis (Wavefront/Row parallelism):
        // Compute best intra mode, quad-tree split decision, and flat-skip flag concurrently across CPU cores.
        let analyze_row = |r: u32| -> Vec<SbAnalysis> {
            let y0 = (r as usize) * 64;
            let block_h = 64.min(height.saturating_sub(y0));
            let avail_u = r > 0;

            let mut row_analyses = Vec::with_capacity(sb_cols as usize);
            for c in 0..sb_cols {
                let avail_l = c > 0;
                let x0 = (c as usize) * 64;
                let block_w = 64.min(width.saturating_sub(x0));

                let is_inter = if !is_key && has_prev_frame {
                    let src_sad = compute_block_sad(frame.y, frame.y_stride, ref_src_y_plane, width, x0, y0, block_w, block_h);
                    let recon_sad = compute_block_sad(frame.y, frame.y_stride, ref_y_plane, width, x0, y0, block_w, block_h);
                    let src_thresh = ((block_w * block_h) as u64).saturating_mul(2);
                    let recon_thresh = ((block_w * block_h) as u64).saturating_mul(6);
                    src_sad <= src_thresh || recon_sad <= recon_thresh
                } else {
                    false
                };

                if is_inter {
                    row_analyses.push(SbAnalysis { mode: intra::DC_PRED, split: false, is_flat: false, is_inter: true });
                    continue;
                }

                let above_slice = if avail_u && y0 > 0 {
                    let row_offset = (y0 - 1) * frame.y_stride + x0;
                    frame.y.get(row_offset..row_offset + block_w)
                } else {
                    None
                };

                let mut left_col = [0u8; 64];
                let left_slice = if avail_l && x0 > 0 {
                    for i in 0..block_h {
                        if let Some(&p) = frame.y.get((y0 + i) * frame.y_stride + x0 - 1) {
                            left_col[i] = p;
                        }
                    }
                    Some(&left_col[..block_h])
                } else {
                    None
                };

                let src_sub = frame.y.get(y0 * frame.y_stride + x0..);
                let mode = if self.config.preset != Preset::Speed {
                    if let Some(src) = src_sub {
                        intra::choose_best_intra_mode(src, frame.y_stride, block_w, block_h, above_slice, left_slice)
                    } else {
                        intra::DC_PRED
                    }
                } else {
                    intra::DC_PRED
                };

                let split = should_split_64x64(frame.y, frame.y_stride, x0, y0, block_w, block_h, self.config.preset);

                let is_flat = if let Some(src) = src_sub {
                    let var = intra::block_variance(src, frame.y_stride, block_w, block_h);
                    var <= 16
                } else {
                    false
                };

                row_analyses.push(SbAnalysis { mode, split, is_flat, is_inter: false });
            }
            row_analyses
        };

        let precomputed_analyses: Vec<Vec<SbAnalysis>> = {
            #[cfg(feature = "threads")]
            {
                use rayon::prelude::*;
                (0..sb_rows).into_par_iter().map(analyze_row).collect()
            }
            #[cfg(not(feature = "threads"))]
            {
                (0..sb_rows).map(analyze_row).collect()
            }
        };

        let uv_width = width / 2;
        let uv_height = height / 2;

        if is_key || self.frame_count == 0 {
            self.recon_y.fill(128);
            self.recon_u.fill(128);
            self.recon_v.fill(128);
        }

        let cols32 = width.div_ceil(32);
        let rows32 = height.div_ceil(32);
        let mut grid_y_mode = vec![intra::DC_PRED; cols32 * rows32];
        let mut grid_skip = vec![false; cols32 * rows32];
        let mut grid_is_split = vec![false; cols32 * rows32];
        let mut grid_is_inter = vec![false; cols32 * rows32];
        let mut grid_ref_frame = vec![0i8; cols32 * rows32];

        let mut above_dc = [vec![0u8; width.div_ceil(4)], vec![0u8; uv_width.div_ceil(4)], vec![0u8; uv_width.div_ceil(4)]];
        let mut left_dc = [vec![0u8; height.div_ceil(4)], vec![0u8; uv_height.div_ceil(4)], vec![0u8; uv_height.div_ceil(4)]];

        let get_dc_sign_ctx = |x4: usize, y4: usize, w4: usize, h4: usize, above: &[u8], left: &[u8]| -> usize {
            let mut dc_sign = 0i32;
            for k in 0..w4 {
                if let Some(&cat) = above.get(x4 + k) {
                    match cat {
                        1 => dc_sign -= 1,
                        2 => dc_sign += 1,
                        _ => {}
                    }
                }
            }
            for k in 0..h4 {
                if let Some(&cat) = left.get(y4 + k) {
                    match cat {
                        1 => dc_sign -= 1,
                        2 => dc_sign += 1,
                        _ => {}
                    }
                }
            }
            match dc_sign.signum() {
                -1 => 1,
                1 => 2,
                _ => 0,
            }
        };

        let update_dc_sign = |above: &mut [u8], left: &mut [u8], x4: usize, y4: usize, w4: usize, h4: usize, level: u32, is_neg: bool| {
            let cat = if level > 0 { if is_neg { 1 } else { 2 } } else { 0 };
            for k in 0..w4 {
                if let Some(slot) = above.get_mut(x4 + k) {
                    *slot = cat;
                }
            }
            for k in 0..h4 {
                if let Some(slot) = left.get_mut(y4 + k) {
                    *slot = cat;
                }
            }
        };

        for sb_r in 0..sb_rows as usize {
            for sb_c in 0..sb_cols as usize {
                let r32 = sb_r * 2;
                let c32 = sb_c * 2;
                let x0 = sb_c * 64;
                let y0 = sb_r * 64;
                let block_w = 64.min(width.saturating_sub(x0));
                let block_h = 64.min(height.saturating_sub(y0));

                let avail_u = sb_r > 0;
                let avail_l = sb_c > 0;

                let above_is_split = avail_u && grid_is_split.get((r32 - 1) * cols32 + c32).copied().unwrap_or(false);
                let left_is_split = avail_l && grid_is_split.get(r32 * cols32 + c32.saturating_sub(1)).copied().unwrap_or(false);
                let part_ctx = ((left_is_split as usize) * 2) + (above_is_split as usize);

                let sb_info = precomputed_analyses.get(sb_r).and_then(|row| row.get(sb_c)).copied().unwrap_or(SbAnalysis {
                    mode: intra::DC_PRED,
                    split: false,
                    is_flat: false,
                    is_inter: false,
                });

                let split = sb_info.split;

                if !split {
                    sym.write_symbol_no_update(tables::PARTITION_NONE, &tables::DEFAULT_PARTITION_W64_CDF[part_ctx]);

                    let above_skip = if avail_u { grid_skip.get((r32 - 1) * cols32 + c32).copied().unwrap_or(false) } else { false };
                    let left_skip = if avail_l { grid_skip.get(r32 * cols32 + c32.saturating_sub(1)).copied().unwrap_or(false) } else { false };
                    let skip_ctx = (above_skip as usize) + (left_skip as usize);

                    if !is_key && sb_info.is_inter {
                        sym.write_symbol_no_update(1, &tables::DEFAULT_SKIP_CDF[skip_ctx]);

                        let above_intra = if avail_u { grid_ref_frame.get((r32 - 1) * cols32 + c32).copied().unwrap_or(0) == 0 } else { true };
                        let left_intra = if avail_l { grid_ref_frame.get(r32 * cols32 + c32.saturating_sub(1)).copied().unwrap_or(0) == 0 } else { true };
                        let is_inter_ctx = if avail_u && avail_l {
                            if left_intra && above_intra { 3 } else { (left_intra || above_intra) as usize }
                        } else if avail_u || avail_l {
                            2 * (if avail_u { above_intra } else { left_intra }) as usize
                        } else {
                            0
                        };
                        sym.write_symbol_no_update(1, &tables::DEFAULT_IS_INTER_CDF[is_inter_ctx]);

                        let count_last = {
                            let mut c = 0;
                            if avail_u && grid_ref_frame.get((r32 - 1) * cols32 + c32).copied().unwrap_or(0) == 1 {
                                c += 1;
                            }
                            if avail_l && grid_ref_frame.get(r32 * cols32 + c32.saturating_sub(1)).copied().unwrap_or(0) == 1 {
                                c += 1;
                            }
                            c
                        };
                        let ref_ctx = if count_last > 0 { 2 } else { 1 };
                        sym.write_symbol_no_update(0, &tables::DEFAULT_SINGLE_REF_CDF[ref_ctx][0]);
                        sym.write_symbol_no_update(0, &tables::DEFAULT_SINGLE_REF_CDF[ref_ctx][2]);
                        sym.write_symbol_no_update(0, &tables::DEFAULT_SINGLE_REF_CDF[ref_ctx][3]);

                        let new_mv_ctx = match count_last {
                            0 => 0,
                            1 => 3,
                            _ => 5,
                        };
                        sym.write_symbol_no_update(1, &tables::DEFAULT_NEW_MV_CDF[new_mv_ctx]);
                        sym.write_symbol_no_update(0, &tables::DEFAULT_ZERO_MV_CDF[0]);

                        copy_block(&self.ref_y, width, &mut self.recon_y, width, x0, y0, block_w, block_h);
                        copy_block(&self.ref_u, uv_width, &mut self.recon_u, uv_width, x0 / 2, y0 / 2, (block_w / 2).max(1), (block_h / 2).max(1));
                        copy_block(&self.ref_v, uv_width, &mut self.recon_v, uv_width, x0 / 2, y0 / 2, (block_w / 2).max(1), (block_h / 2).max(1));

                        update_dc_sign(&mut above_dc[0], &mut left_dc[0], x0 / 4, y0 / 4, 16, 16, 0, false);
                        update_dc_sign(&mut above_dc[1], &mut left_dc[1], (x0 / 2) / 4, (y0 / 2) / 4, 8, 8, 0, false);
                        update_dc_sign(&mut above_dc[2], &mut left_dc[2], (x0 / 2) / 4, (y0 / 2) / 4, 8, 8, 0, false);

                        for dr in 0..2 {
                            for dc in 0..2 {
                                let sub_r = r32 + dr;
                                let sub_c = c32 + dc;
                                if sub_r < rows32 && sub_c < cols32 {
                                    let idx = sub_r * cols32 + sub_c;
                                    grid_y_mode[idx] = intra::DC_PRED;
                                    grid_skip[idx] = true;
                                    grid_is_inter[idx] = true;
                                    grid_ref_frame[idx] = 1;
                                    grid_is_split[idx] = false;
                                }
                            }
                        }
                    } else {
                        let above_mode = if avail_u { grid_y_mode.get((r32 - 1) * cols32 + c32).copied().unwrap_or(intra::DC_PRED) } else { intra::DC_PRED };
                        let left_mode =
                            if avail_l { grid_y_mode.get(r32 * cols32 + c32.saturating_sub(1)).copied().unwrap_or(intra::DC_PRED) } else { intra::DC_PRED };

                        let actx = tables::INTRA_MODE_CONTEXT.get(above_mode as usize).copied().unwrap_or(0) as usize;
                        let lctx = tables::INTRA_MODE_CONTEXT.get(left_mode as usize).copied().unwrap_or(0) as usize;

                        let y_mode = sb_info.mode;

                        let uv_mode = if y_mode == intra::V_PRED || y_mode == intra::H_PRED { y_mode } else { intra::DC_PRED };

                        let (dc_level, is_neg, u_dc_level, u_is_neg, v_dc_level, v_is_neg) = if sb_info.is_flat && self.config.preset == Preset::Speed {
                            compute_and_reconstruct_plane_dc(
                                frame.y,
                                frame.y_stride,
                                &mut self.recon_y,
                                width,
                                x0,
                                y0,
                                block_w,
                                block_h,
                                y_mode,
                                avail_u,
                                avail_l,
                                qindex,
                                512,
                            );
                            compute_and_reconstruct_plane_dc(
                                frame.u,
                                frame.uv_stride,
                                &mut self.recon_u,
                                uv_width,
                                x0 / 2,
                                y0 / 2,
                                (block_w / 2).max(1),
                                (block_h / 2).max(1),
                                uv_mode,
                                avail_u,
                                avail_l,
                                qindex,
                                256,
                            );
                            compute_and_reconstruct_plane_dc(
                                frame.v,
                                frame.uv_stride,
                                &mut self.recon_v,
                                uv_width,
                                x0 / 2,
                                y0 / 2,
                                (block_w / 2).max(1),
                                (block_h / 2).max(1),
                                uv_mode,
                                avail_u,
                                avail_l,
                                qindex,
                                256,
                            );
                            (0, false, 0, false, 0, false)
                        } else {
                            let (dc, neg) = compute_and_reconstruct_plane_dc(
                                frame.y,
                                frame.y_stride,
                                &mut self.recon_y,
                                width,
                                x0,
                                y0,
                                block_w,
                                block_h,
                                y_mode,
                                avail_u,
                                avail_l,
                                qindex,
                                512,
                            );
                            let (u_dc, u_neg) = compute_and_reconstruct_plane_dc(
                                frame.u,
                                frame.uv_stride,
                                &mut self.recon_u,
                                uv_width,
                                x0 / 2,
                                y0 / 2,
                                (block_w / 2).max(1),
                                (block_h / 2).max(1),
                                uv_mode,
                                avail_u,
                                avail_l,
                                qindex,
                                256,
                            );
                            let (v_dc, v_neg) = compute_and_reconstruct_plane_dc(
                                frame.v,
                                frame.uv_stride,
                                &mut self.recon_v,
                                uv_width,
                                x0 / 2,
                                y0 / 2,
                                (block_w / 2).max(1),
                                (block_h / 2).max(1),
                                uv_mode,
                                avail_u,
                                avail_l,
                                qindex,
                                256,
                            );
                            (dc, neg, u_dc, u_neg, v_dc, v_neg)
                        };

                        let has_residual = dc_level > 0 || u_dc_level > 0 || v_dc_level > 0;

                        if is_key {
                            if has_residual {
                                sym.write_symbol_no_update(0, &tables::DEFAULT_SKIP_CDF[skip_ctx]);
                                sym.write_symbol_no_update(y_mode as usize, &tables::DEFAULT_INTRA_FRAME_Y_MODE_CDF[actx][lctx]);

                                if y_mode == intra::V_PRED || y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_NOT_ALLOWED_CDF[y_mode as usize]);

                                if uv_mode == intra::V_PRED || uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[uv_mode as usize - 1]);
                                }

                                let luma_dc_ctx = get_dc_sign_ctx(x0 / 4, y0 / 4, 16, 16, &above_dc[0], &left_dc[0]);
                                let u_dc_ctx = get_dc_sign_ctx((x0 / 2) / 4, (y0 / 2) / 4, 8, 8, &above_dc[1], &left_dc[1]);
                                let v_dc_ctx = get_dc_sign_ctx((x0 / 2) / 4, (y0 / 2) / 4, 8, 8, &above_dc[2], &left_dc[2]);

                                if dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 0, 4, 0, luma_dc_ctx, dc_level, is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 4, 0, true);
                                }

                                if u_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 1, 3, 7, u_dc_ctx, u_dc_level, u_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 3, 7, true);
                                }

                                if v_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 2, 3, 7, v_dc_ctx, v_dc_level, v_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 3, 7, true);
                                }
                            } else {
                                sym.write_symbol_no_update(1, &tables::DEFAULT_SKIP_CDF[skip_ctx]);
                                sym.write_symbol_no_update(y_mode as usize, &tables::DEFAULT_INTRA_FRAME_Y_MODE_CDF[actx][lctx]);

                                if y_mode == intra::V_PRED || y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_NOT_ALLOWED_CDF[y_mode as usize]);

                                if uv_mode == intra::V_PRED || uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[uv_mode as usize - 1]);
                                }
                            }
                        } else {
                            let above_intra = if avail_u { grid_ref_frame.get((r32 - 1) * cols32 + c32).copied().unwrap_or(0) == 0 } else { true };
                            let left_intra = if avail_l { grid_ref_frame.get(r32 * cols32 + c32.saturating_sub(1)).copied().unwrap_or(0) == 0 } else { true };
                            let is_inter_ctx = if avail_u && avail_l {
                                if left_intra && above_intra { 3 } else { (left_intra || above_intra) as usize }
                            } else if avail_u || avail_l {
                                2 * (if avail_u { above_intra } else { left_intra }) as usize
                            } else {
                                0
                            };
                            let size_grp = if block_w >= 64 && block_h >= 64 {
                                3
                            } else if block_w >= 32 && block_h >= 32 {
                                2
                            } else if block_w >= 16 && block_h >= 16 {
                                1
                            } else {
                                0
                            };

                            if has_residual {
                                sym.write_symbol_no_update(0, &tables::DEFAULT_SKIP_CDF[skip_ctx]);
                                sym.write_symbol_no_update(0, &tables::DEFAULT_IS_INTER_CDF[is_inter_ctx]);
                                sym.write_symbol_no_update(y_mode as usize, &tables::DEFAULT_Y_MODE_CDF[size_grp]);

                                if y_mode == intra::V_PRED || y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_NOT_ALLOWED_CDF[y_mode as usize]);

                                if uv_mode == intra::V_PRED || uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[uv_mode as usize - 1]);
                                }

                                let luma_dc_ctx = get_dc_sign_ctx(x0 / 4, y0 / 4, 16, 16, &above_dc[0], &left_dc[0]);
                                let u_dc_ctx = get_dc_sign_ctx((x0 / 2) / 4, (y0 / 2) / 4, 8, 8, &above_dc[1], &left_dc[1]);
                                let v_dc_ctx = get_dc_sign_ctx((x0 / 2) / 4, (y0 / 2) / 4, 8, 8, &above_dc[2], &left_dc[2]);

                                if dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 0, 4, 0, luma_dc_ctx, dc_level, is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 4, 0, true);
                                }

                                if u_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 1, 3, 7, u_dc_ctx, u_dc_level, u_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 3, 7, true);
                                }

                                if v_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 2, 3, 7, v_dc_ctx, v_dc_level, v_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 3, 7, true);
                                }
                            } else {
                                sym.write_symbol_no_update(1, &tables::DEFAULT_SKIP_CDF[skip_ctx]);
                                sym.write_symbol_no_update(0, &tables::DEFAULT_IS_INTER_CDF[is_inter_ctx]);
                                sym.write_symbol_no_update(y_mode as usize, &tables::DEFAULT_Y_MODE_CDF[size_grp]);

                                if y_mode == intra::V_PRED || y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_NOT_ALLOWED_CDF[y_mode as usize]);

                                if uv_mode == intra::V_PRED || uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[uv_mode as usize - 1]);
                                }
                            }
                        }

                        update_dc_sign(&mut above_dc[0], &mut left_dc[0], x0 / 4, y0 / 4, 16, 16, dc_level, is_neg);
                        update_dc_sign(&mut above_dc[1], &mut left_dc[1], (x0 / 2) / 4, (y0 / 2) / 4, 8, 8, u_dc_level, u_is_neg);
                        update_dc_sign(&mut above_dc[2], &mut left_dc[2], (x0 / 2) / 4, (y0 / 2) / 4, 8, 8, v_dc_level, v_is_neg);

                        for dr in 0..2 {
                            for dc in 0..2 {
                                let sub_r = r32 + dr;
                                let sub_c = c32 + dc;
                                if sub_r < rows32 && sub_c < cols32 {
                                    let idx = sub_r * cols32 + sub_c;
                                    grid_y_mode[idx] = y_mode;
                                    grid_skip[idx] = !has_residual;
                                    grid_is_inter[idx] = false;
                                    grid_ref_frame[idx] = 0;
                                    grid_is_split[idx] = false;
                                }
                            }
                        }
                    }
                } else {
                    sym.write_symbol_no_update(tables::PARTITION_SPLIT, &tables::DEFAULT_PARTITION_W64_CDF[part_ctx]);

                    for k in 0..4 {
                        let (dr, dc) = match k {
                            0 => (0, 0),
                            1 => (0, 1),
                            2 => (1, 0),
                            _ => (1, 1),
                        };
                        let sub_r32 = r32 + dr;
                        let sub_c32 = c32 + dc;
                        let sub_x0 = x0 + dc * 32;
                        let sub_y0 = y0 + dr * 32;
                        let sub_w = 32.min(width.saturating_sub(sub_x0));
                        let sub_h = 32.min(height.saturating_sub(sub_y0));

                        sym.write_symbol_no_update(tables::PARTITION_NONE, &tables::DEFAULT_PARTITION_W32_CDF[0]);

                        let sub_avail_u = sub_r32 > 0;
                        let sub_avail_l = sub_c32 > 0;

                        let sub_above_mode =
                            if sub_avail_u { grid_y_mode.get((sub_r32 - 1) * cols32 + sub_c32).copied().unwrap_or(intra::DC_PRED) } else { intra::DC_PRED };
                        let sub_left_mode = if sub_avail_l {
                            grid_y_mode.get(sub_r32 * cols32 + sub_c32.saturating_sub(1)).copied().unwrap_or(intra::DC_PRED)
                        } else {
                            intra::DC_PRED
                        };
                        let sub_above_skip = if sub_avail_u { grid_skip.get((sub_r32 - 1) * cols32 + sub_c32).copied().unwrap_or(false) } else { false };
                        let sub_left_skip =
                            if sub_avail_l { grid_skip.get(sub_r32 * cols32 + sub_c32.saturating_sub(1)).copied().unwrap_or(false) } else { false };

                        let sub_skip_ctx = (sub_above_skip as usize) + (sub_left_skip as usize);
                        let sub_actx = tables::INTRA_MODE_CONTEXT.get(sub_above_mode as usize).copied().unwrap_or(0) as usize;
                        let sub_lctx = tables::INTRA_MODE_CONTEXT.get(sub_left_mode as usize).copied().unwrap_or(0) as usize;

                        let mut above_buf = [128u8; 32];
                        let has_above_ref = if sub_avail_u && sub_y0 > 0 {
                            let off = (sub_y0 - 1) * width + sub_x0;
                            if let Some(row) = self.recon_y.get(off..off + sub_w) {
                                above_buf[..sub_w].copy_from_slice(row);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        let mut left_buf = [128u8; 32];
                        let has_left_ref = if sub_avail_l && sub_x0 > 0 {
                            for y in 0..sub_h {
                                if let Some(&p) = self.recon_y.get((sub_y0 + y) * width + sub_x0 - 1) {
                                    left_buf[y] = p;
                                }
                            }
                            true
                        } else {
                            false
                        };

                        let sub_y_mode = if self.config.preset != Preset::Speed {
                            let src_off = sub_y0 * frame.y_stride + sub_x0;
                            if let Some(src_sub) = frame.y.get(src_off..) {
                                intra::choose_best_intra_mode(
                                    src_sub,
                                    frame.y_stride,
                                    sub_w,
                                    sub_h,
                                    if has_above_ref { Some(&above_buf[..sub_w]) } else { None },
                                    if has_left_ref { Some(&left_buf[..sub_h]) } else { None },
                                )
                            } else {
                                intra::DC_PRED
                            }
                        } else {
                            intra::DC_PRED
                        };

                        let (dc_level, is_neg) = compute_and_reconstruct_plane_dc(
                            frame.y,
                            frame.y_stride,
                            &mut self.recon_y,
                            width,
                            sub_x0,
                            sub_y0,
                            sub_w,
                            sub_h,
                            sub_y_mode,
                            sub_avail_u,
                            sub_avail_l,
                            qindex,
                            256,
                        );

                        let sub_uv_mode = if sub_y_mode == intra::V_PRED || sub_y_mode == intra::H_PRED { sub_y_mode } else { intra::DC_PRED };

                        let (u_dc_level, u_is_neg) = compute_and_reconstruct_plane_dc(
                            frame.u,
                            frame.uv_stride,
                            &mut self.recon_u,
                            uv_width,
                            sub_x0 / 2,
                            sub_y0 / 2,
                            (sub_w / 2).max(1),
                            (sub_h / 2).max(1),
                            sub_uv_mode,
                            sub_avail_u,
                            sub_avail_l,
                            qindex,
                            128,
                        );

                        let (v_dc_level, v_is_neg) = compute_and_reconstruct_plane_dc(
                            frame.v,
                            frame.uv_stride,
                            &mut self.recon_v,
                            uv_width,
                            sub_x0 / 2,
                            sub_y0 / 2,
                            (sub_w / 2).max(1),
                            (sub_h / 2).max(1),
                            sub_uv_mode,
                            sub_avail_u,
                            sub_avail_l,
                            qindex,
                            128,
                        );

                        let has_residual = dc_level > 0 || u_dc_level > 0 || v_dc_level > 0;

                        if is_key {
                            if has_residual {
                                sym.write_symbol_no_update(0, &tables::DEFAULT_SKIP_CDF[sub_skip_ctx]);
                                sym.write_symbol_no_update(sub_y_mode as usize, &tables::DEFAULT_INTRA_FRAME_Y_MODE_CDF[sub_actx][sub_lctx]);

                                if sub_y_mode == intra::V_PRED || sub_y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(sub_uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_ALLOWED_CDF[sub_y_mode as usize]);

                                if sub_uv_mode == intra::V_PRED || sub_uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_uv_mode as usize - 1]);
                                }

                                let luma_dc_ctx = get_dc_sign_ctx(sub_x0 / 4, sub_y0 / 4, 8, 8, &above_dc[0], &left_dc[0]);
                                let u_dc_ctx = get_dc_sign_ctx((sub_x0 / 2) / 4, (sub_y0 / 2) / 4, 4, 4, &above_dc[1], &left_dc[1]);
                                let v_dc_ctx = get_dc_sign_ctx((sub_x0 / 2) / 4, (sub_y0 / 2) / 4, 4, 4, &above_dc[2], &left_dc[2]);

                                if dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 0, 3, 0, luma_dc_ctx, dc_level, is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 3, 0, true);
                                }

                                if u_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 1, 2, 7, u_dc_ctx, u_dc_level, u_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 2, 7, true);
                                }

                                if v_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 2, 2, 7, v_dc_ctx, v_dc_level, v_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 2, 7, true);
                                }
                            } else {
                                sym.write_symbol_no_update(1, &tables::DEFAULT_SKIP_CDF[sub_skip_ctx]);
                                sym.write_symbol_no_update(sub_y_mode as usize, &tables::DEFAULT_INTRA_FRAME_Y_MODE_CDF[sub_actx][sub_lctx]);

                                if sub_y_mode == intra::V_PRED || sub_y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(sub_uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_ALLOWED_CDF[sub_y_mode as usize]);

                                if sub_uv_mode == intra::V_PRED || sub_uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_uv_mode as usize - 1]);
                                }
                            }
                        } else {
                            let sub_above_intra =
                                if sub_avail_u { grid_ref_frame.get((sub_r32 - 1) * cols32 + sub_c32).copied().unwrap_or(0) == 0 } else { true };
                            let sub_left_intra =
                                if sub_avail_l { grid_ref_frame.get(sub_r32 * cols32 + sub_c32.saturating_sub(1)).copied().unwrap_or(0) == 0 } else { true };
                            let sub_is_inter_ctx = if sub_avail_u && sub_avail_l {
                                if sub_left_intra && sub_above_intra { 3 } else { (sub_left_intra || sub_above_intra) as usize }
                            } else if sub_avail_u || sub_avail_l {
                                2 * (if sub_avail_u { sub_above_intra } else { sub_left_intra }) as usize
                            } else {
                                0
                            };

                            if has_residual {
                                sym.write_symbol_no_update(0, &tables::DEFAULT_SKIP_CDF[sub_skip_ctx]);
                                sym.write_symbol_no_update(0, &tables::DEFAULT_IS_INTER_CDF[sub_is_inter_ctx]);
                                sym.write_symbol_no_update(sub_y_mode as usize, &tables::DEFAULT_Y_MODE_CDF[2]);

                                if sub_y_mode == intra::V_PRED || sub_y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(sub_uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_ALLOWED_CDF[sub_y_mode as usize]);

                                if sub_uv_mode == intra::V_PRED || sub_uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_uv_mode as usize - 1]);
                                }

                                let luma_dc_ctx = get_dc_sign_ctx(sub_x0 / 4, sub_y0 / 4, 8, 8, &above_dc[0], &left_dc[0]);
                                let u_dc_ctx = get_dc_sign_ctx((sub_x0 / 2) / 4, (sub_y0 / 2) / 4, 4, 4, &above_dc[1], &left_dc[1]);
                                let v_dc_ctx = get_dc_sign_ctx((sub_x0 / 2) / 4, (sub_y0 / 2) / 4, 4, 4, &above_dc[2], &left_dc[2]);

                                if dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 0, 3, 0, luma_dc_ctx, dc_level, is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 3, 0, true);
                                }

                                if u_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 1, 2, 7, u_dc_ctx, u_dc_level, u_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 2, 7, true);
                                }

                                if v_dc_level > 0 {
                                    coeff::encode_dc_residual(&mut sym, qindex, 2, 2, 7, v_dc_ctx, v_dc_level, v_is_neg);
                                } else {
                                    coeff::write_txb_skip(&mut sym, qindex, 2, 7, true);
                                }
                            } else {
                                sym.write_symbol_no_update(1, &tables::DEFAULT_SKIP_CDF[sub_skip_ctx]);
                                sym.write_symbol_no_update(0, &tables::DEFAULT_IS_INTER_CDF[sub_is_inter_ctx]);
                                sym.write_symbol_no_update(sub_y_mode as usize, &tables::DEFAULT_Y_MODE_CDF[2]);

                                if sub_y_mode == intra::V_PRED || sub_y_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_y_mode as usize - 1]);
                                }

                                sym.write_symbol_no_update(sub_uv_mode as usize, &tables::DEFAULT_UV_MODE_CFL_ALLOWED_CDF[sub_y_mode as usize]);

                                if sub_uv_mode == intra::V_PRED || sub_uv_mode == intra::H_PRED {
                                    sym.write_symbol_no_update(3, &tables::DEFAULT_ANGLE_DELTA_CDF[sub_uv_mode as usize - 1]);
                                }
                            }
                        }

                        update_dc_sign(&mut above_dc[0], &mut left_dc[0], sub_x0 / 4, sub_y0 / 4, 8, 8, dc_level, is_neg);
                        update_dc_sign(&mut above_dc[1], &mut left_dc[1], (sub_x0 / 2) / 4, (sub_y0 / 2) / 4, 4, 4, u_dc_level, u_is_neg);
                        update_dc_sign(&mut above_dc[2], &mut left_dc[2], (sub_x0 / 2) / 4, (sub_y0 / 2) / 4, 4, 4, v_dc_level, v_is_neg);

                        if sub_r32 < rows32 && sub_c32 < cols32 {
                            let idx = sub_r32 * cols32 + sub_c32;
                            grid_y_mode[idx] = sub_y_mode;
                            grid_skip[idx] = !has_residual;
                            grid_is_inter[idx] = false;
                            grid_ref_frame[idx] = 0;
                            grid_is_split[idx] = true;
                        }
                    }
                }
            }
        }

        let tile_payload = sym.finish();
        tu.extend_from_slice(&obu::write_obu(obu::OBU_TILE_GROUP, &tile_payload));

        // Update CBR/VBR bit reservoir and active qindex
        if let Some(rc) = self.config.rate_control {
            match rc {
                RateControl::Cbr { kbps } => {
                    let target_bits_128 = (u128::from(kbps) * 1000 * u128::from(self.config.fps_den)) / u128::from(self.config.fps_num).max(1);
                    let target_bits = target_bits_128.min((i64::MAX / 4) as u128) as i64;
                    let frame_bits = (tu.len() as i64).saturating_mul(8);
                    self.bit_reservoir = self.bit_reservoir.saturating_add(target_bits.saturating_sub(frame_bits));
                    let buffer_threshold = target_bits.saturating_mul(2);
                    if self.bit_reservoir < -buffer_threshold {
                        self.active_qindex = self.active_qindex.saturating_add(6);
                    } else if self.bit_reservoir > buffer_threshold {
                        self.active_qindex = self.active_qindex.saturating_sub(4).max(20);
                    }
                }
                RateControl::Vbr { target_kbps, max_kbps } => {
                    let target_bits_128 = (u128::from(target_kbps) * 1000 * u128::from(self.config.fps_den)) / u128::from(self.config.fps_num).max(1);
                    let target_bits = target_bits_128.min((i64::MAX / 4) as u128) as i64;
                    let max_bits_128 = (u128::from(max_kbps) * 1000 * u128::from(self.config.fps_den)) / u128::from(self.config.fps_num).max(1);
                    let max_bits = max_bits_128.min((i64::MAX / 4) as u128) as i64;
                    let frame_bits = (tu.len() as i64).saturating_mul(8);
                    if frame_bits > max_bits {
                        self.active_qindex = self.active_qindex.saturating_add(8);
                    } else {
                        self.bit_reservoir = self.bit_reservoir.saturating_add(target_bits.saturating_sub(frame_bits));
                        let buffer_threshold = target_bits.saturating_mul(3);
                        if self.bit_reservoir < -buffer_threshold {
                            self.active_qindex = self.active_qindex.saturating_add(4);
                        } else if self.bit_reservoir > buffer_threshold {
                            self.active_qindex = self.active_qindex.saturating_sub(2).max(20);
                        }
                    }
                }
                _ => {}
            }
        }

        self.ref_y.copy_from_slice(&self.recon_y);
        self.ref_u.copy_from_slice(&self.recon_u);
        self.ref_v.copy_from_slice(&self.recon_v);
        copy_block(frame.y, frame.y_stride, &mut self.ref_src_y, width, 0, 0, width, height);

        self.frame_count = self.frame_count.saturating_add(1);
        Ok(Packet { data: tu, keyframe: is_key, pts })
    }

    pub fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(Vec::new())
    }
}

fn should_split_64x64(y_plane: &[u8], stride: usize, x0: usize, y0: usize, block_w: usize, block_h: usize, preset: Preset) -> bool {
    if preset == Preset::Speed || block_w < 64 || block_h < 64 {
        return false;
    }

    let mut means = [0i64; 4];
    for (idx, &(dx, dy)) in [(0, 0), (32, 0), (0, 32), (32, 32)].iter().enumerate() {
        let mut sum = 0i64;
        for y in 0..32 {
            let Some(row_y) = y0.checked_add(dy).and_then(|v| v.checked_add(y)) else { continue };
            let Some(row_x) = x0.checked_add(dx) else { continue };
            let Some(row_off) = row_y.checked_mul(stride).and_then(|o| o.checked_add(row_x)) else { continue };
            let Some(row_end) = row_off.checked_add(32) else { continue };
            if let Some(row) = y_plane.get(row_off..row_end) {
                sum += row.iter().map(|&p| p as i64).sum::<i64>();
            }
        }
        means[idx] = sum / (32 * 32);
    }

    let min_m = means.iter().copied().min().unwrap_or(0);
    let max_m = means.iter().copied().max().unwrap_or(0);
    let range = max_m - min_m;

    let threshold = match preset {
        Preset::Quality => 12,
        Preset::Balanced => 18,
        Preset::Speed => i64::MAX,
    };

    if range >= threshold {
        return true;
    }

    let var_threshold = match preset {
        Preset::Quality => 120,
        Preset::Balanced => 220,
        Preset::Speed => u64::MAX,
    };

    let var = y0
        .checked_mul(stride)
        .and_then(|o| o.checked_add(x0))
        .and_then(|idx| y_plane.get(idx..))
        .map(|sub| intra::block_variance(sub, stride, block_w, block_h))
        .unwrap_or(0);

    var >= var_threshold
}

fn compute_and_reconstruct_plane_dc(
    src_plane: &[u8],
    src_stride: usize,
    recon_plane: &mut [u8],
    recon_stride: usize,
    x0: usize,
    y0: usize,
    block_w: usize,
    block_h: usize,
    mode: u8,
    avail_u: bool,
    avail_l: bool,
    qindex: u8,
    scale: i64,
) -> (u32, bool) {
    let mut src_sum = 0i64;
    let mut count = 0i64;
    for i in 0..block_h {
        let Some(row_off) = y0.checked_add(i).and_then(|y| y.checked_mul(src_stride)).and_then(|o| o.checked_add(x0)) else { continue };
        let Some(row_end) = row_off.checked_add(block_w) else { continue };
        if let Some(row) = src_plane.get(row_off..row_end) {
            for &p in row {
                src_sum += p as i64;
                count += 1;
            }
        }
    }
    if count == 0 {
        return (0, false);
    }
    let src_mean = src_sum / count;

    let pred_mean = match mode {
        intra::V_PRED if avail_u && y0 > 0 => {
            let row_offset = y0.saturating_sub(1).checked_mul(recon_stride).and_then(|o| o.checked_add(x0));
            row_offset
                .and_then(|off| off.checked_add(block_w).and_then(|end| recon_plane.get(off..end)))
                .map(|row| row.iter().map(|&p| p as i64).sum::<i64>() / (block_w.max(1) as i64))
                .unwrap_or(128)
        }
        intra::H_PRED if avail_l && x0 > 0 => {
            let mut left_sum = 0i64;
            for i in 0..block_h {
                let off = y0.checked_add(i).and_then(|y| y.checked_mul(recon_stride)).and_then(|o| o.checked_add(x0.saturating_sub(1)));
                if let Some(&p) = off.and_then(|idx| recon_plane.get(idx)) {
                    left_sum += p as i64;
                }
            }
            left_sum / (block_h.max(1) as i64)
        }
        _ => {
            let mut sum = 0i64;
            let mut n = 0i64;
            if avail_u && y0 > 0 {
                let row_offset = y0.saturating_sub(1).checked_mul(recon_stride).and_then(|o| o.checked_add(x0));
                if let Some(row) = row_offset.and_then(|off| off.checked_add(block_w).and_then(|end| recon_plane.get(off..end))) {
                    sum += row.iter().map(|&p| p as i64).sum::<i64>();
                    n += block_w as i64;
                }
            }
            if avail_l && x0 > 0 {
                for i in 0..block_h {
                    let off = y0.checked_add(i).and_then(|y| y.checked_mul(recon_stride)).and_then(|o| o.checked_add(x0.saturating_sub(1)));
                    if let Some(&p) = off.and_then(|idx| recon_plane.get(idx)) {
                        sum += p as i64;
                        n += 1;
                    }
                }
            }
            if n > 0 { (sum + (n / 2)) / n } else { 128 }
        }
    };

    let diff = src_mean - pred_mean;
    let dc_q = tables::DC_QLOOKUP_8BIT[qindex as usize] as i64;
    let dc_level = ((diff.abs().saturating_mul(scale) + (dc_q / 2)) / dc_q.max(1)) as u32;
    let is_negative = diff < 0;

    let recon_shift = if dc_level == 0 {
        0
    } else {
        let mag = (dc_level as i64 * dc_q) / scale.max(1);
        if is_negative { -mag } else { mag }
    };

    let recon_val = (pred_mean + recon_shift).clamp(0, 255) as u8;
    match mode {
        intra::V_PRED if avail_u && y0 > 0 => {
            let above_off = y0.saturating_sub(1).checked_mul(recon_stride).and_then(|o| o.checked_add(x0));
            if let Some(above_slice) = above_off.and_then(|off| off.checked_add(block_w).and_then(|end| recon_plane.get(off..end))) {
                let mut temp = [0u8; 64];
                let len = block_w.min(64);
                for (dst, &src) in temp[..len].iter_mut().zip(&above_slice[..len]) {
                    *dst = (src as i64 + recon_shift).clamp(0, 255) as u8;
                }
                for i in 0..block_h {
                    let row_off = y0.checked_add(i).and_then(|y| y.checked_mul(recon_stride)).and_then(|o| o.checked_add(x0));
                    if let Some(row) = row_off.and_then(|off| off.checked_add(len).and_then(|end| recon_plane.get_mut(off..end))) {
                        row.copy_from_slice(&temp[..len]);
                    }
                }
            }
        }
        intra::H_PRED if avail_l && x0 > 0 => {
            for i in 0..block_h {
                let left_off = y0.checked_add(i).and_then(|y| y.checked_mul(recon_stride)).and_then(|o| o.checked_add(x0.saturating_sub(1)));
                let left_val = left_off.and_then(|idx| recon_plane.get(idx)).copied().unwrap_or(128) as i64;
                let val = (left_val + recon_shift).clamp(0, 255) as u8;
                let row_off = y0.checked_add(i).and_then(|y| y.checked_mul(recon_stride)).and_then(|o| o.checked_add(x0));
                if let Some(row) = row_off.and_then(|off| off.checked_add(block_w).and_then(|end| recon_plane.get_mut(off..end))) {
                    row.fill(val);
                }
            }
        }
        _ => {
            for i in 0..block_h {
                let row_off = y0.checked_add(i).and_then(|y| y.checked_mul(recon_stride)).and_then(|o| o.checked_add(x0));
                if let Some(row) = row_off.and_then(|off| off.checked_add(block_w).and_then(|end| recon_plane.get_mut(off..end))) {
                    row.fill(recon_val);
                }
            }
        }
    }

    (dc_level, is_negative)
}

/// Compute Mean Squared Error (MSE) between two byte planes of identical length.
pub fn compute_mse(src: &[u8], dst: &[u8]) -> Option<f64> {
    if src.is_empty() || src.len() != dst.len() {
        return None;
    }
    let sum_sq_err: u64 = src.iter().zip(dst.iter()).map(|(&a, &b)| (a as i64 - b as i64).pow(2) as u64).sum();
    Some(sum_sq_err as f64 / src.len() as f64)
}

/// Compute Peak Signal-to-Noise Ratio (PSNR in dB) between two 8-bit byte planes.
///
/// Returns 100.0 dB when planes are identical (zero error).
pub fn compute_psnr(src: &[u8], dst: &[u8]) -> Option<f64> {
    let mse = compute_mse(src, dst)?;
    if mse <= 1e-10 {
        return Some(100.0);
    }
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

/// Compute PSNR comparing an 8-bit reference plane with 16-bit decoder picture samples.
pub fn compute_psnr_u16(src: &[u8], dst: &[u16]) -> Option<f64> {
    if src.is_empty() || src.len() != dst.len() {
        return None;
    }
    let sum_sq_err: u64 = src.iter().zip(dst.iter()).map(|(&a, &b)| (a as i64 - b as i64).pow(2) as u64).sum();
    let mse = sum_sq_err as f64 / src.len() as f64;
    if mse <= 1e-10 {
        return Some(100.0);
    }
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoder_lifecycle_and_decode() {
        let mut enc = Encoder::new(EncoderConfig::new(64, 64, 25, 1)).unwrap();
        let y_plane = vec![128u8; 64 * 64];
        let uv_plane = vec![128u8; 32 * 32];
        let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: 64, uv_stride: 32 };

        let packet1 = enc.encode(&frame, 0).unwrap();
        assert!(!packet1.data.is_empty());
        assert!(packet1.keyframe);
        let packet2 = enc.encode(&frame, 1).unwrap();
        assert!(!packet2.data.is_empty());

        let mut dec = filmcraft_av1::Decoder::new();
        let mut pics = dec.decode(&packet1.data).unwrap();
        pics.extend(dec.flush());
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].width, 64);
        assert_eq!(pics[0].height, 64);
        assert_eq!(pics[0].planes[0][0], 128);

        let flush_res = enc.flush().unwrap();
        assert!(flush_res.is_empty());
    }

    #[test]
    fn test_encoder_rate_control_modes() {
        for rc in [
            RateControl::ConstantQuality(20.0),
            RateControl::ConstantQuality(50.0),
            RateControl::Qindex(60),
            RateControl::Qindex(180),
            RateControl::Cbr { kbps: 2000 },
            RateControl::Vbr { target_kbps: 2000, max_kbps: 4000 },
        ] {
            let mut cfg = EncoderConfig::new(64, 64, 30, 1);
            cfg.rate_control = Some(rc);
            cfg.spatial_aq = true;
            let mut enc = Encoder::new(cfg).unwrap();
            let y_plane = vec![128u8; 64 * 64];
            let uv_plane = vec![128u8; 32 * 32];
            let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: 64, uv_stride: 32 };

            let p = enc.encode(&frame, 0).unwrap();
            let mut dec = filmcraft_av1::Decoder::new();
            let mut pics = dec.decode(&p.data).unwrap();
            pics.extend(dec.flush());
            assert_eq!(pics.len(), 1);
        }
    }

    #[test]
    fn test_encoder_smooth_and_paeth_modes() {
        let (w, h) = (128, 128);
        let mut enc = Encoder::new(EncoderConfig::new(w, h, 24, 1)).unwrap();
        let mut y_plane = vec![0u8; (w * h) as usize];
        // 2D diagonal ramp to exercise smooth and paeth prediction
        for y in 0..h {
            for x in 0..w {
                y_plane[(y * w + x) as usize] = ((x * 128 / w) + (y * 128 / h)) as u8;
            }
        }
        let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];
        let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

        let packet = enc.encode(&frame, 0).unwrap();
        let mut dec = filmcraft_av1::Decoder::new();
        let mut pics = dec.decode(&packet.data).unwrap();
        pics.extend(dec.flush());
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].width, w);
        assert_eq!(pics[0].height, h);
    }

    #[test]
    fn test_encoder_gradient_directional_modes() {
        let (w, h) = (128, 128);
        let mut enc = Encoder::new(EncoderConfig::new(w, h, 24, 1)).unwrap();
        let mut y_plane = vec![0u8; (w * h) as usize];
        // Vertical gradient
        for y in 0..h {
            for x in 0..w {
                y_plane[(y * w + x) as usize] = (y * 2).min(255) as u8;
            }
        }
        let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];
        let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

        let packet = enc.encode(&frame, 0).unwrap();
        let mut dec = filmcraft_av1::Decoder::new();
        let mut pics = dec.decode(&packet.data).unwrap();
        pics.extend(dec.flush());
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].width, w);
        assert_eq!(pics[0].height, h);
    }

    #[test]
    fn test_encoder_multi_superblock_decode() {
        let mut enc = Encoder::new(EncoderConfig::new(128, 128, 30, 1)).unwrap();
        let y_plane = vec![128u8; 128 * 128];
        let uv_plane = vec![128u8; 64 * 64];
        let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: 128, uv_stride: 64 };

        let packet = enc.encode(&frame, 0).unwrap();
        let mut dec = filmcraft_av1::Decoder::new();
        let mut pics = dec.decode(&packet.data).unwrap();
        pics.extend(dec.flush());
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].width, 128);
        assert_eq!(pics[0].height, 128);
        assert_eq!(pics[0].planes[0][0], 128);
    }

    #[test]
    fn test_encoder_non_multiple_of_64() {
        let mut enc = Encoder::new(EncoderConfig::new(320, 240, 24, 1)).unwrap();
        let y_plane = vec![128u8; 320 * 240];
        let uv_plane = vec![128u8; 160 * 120];
        let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: 320, uv_stride: 160 };

        let packet = enc.encode(&frame, 0).unwrap();
        let mut dec = filmcraft_av1::Decoder::new();
        let mut pics = dec.decode(&packet.data).unwrap();
        pics.extend(dec.flush());
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].width, 320);
        assert_eq!(pics[0].height, 240);
    }

    #[test]
    fn test_encoder_720p_and_1080p() {
        for (w, h) in [(1280, 720), (1920, 1080)] {
            let mut enc = Encoder::new(EncoderConfig::new(w, h, 24, 1)).unwrap();
            let y_plane = vec![128u8; (w * h) as usize];
            let uv_plane = vec![128u8; ((w / 2) * (h / 2)) as usize];
            let frame = YuvFrame { y: &y_plane, u: &uv_plane, v: &uv_plane, y_stride: w as usize, uv_stride: (w / 2) as usize };

            let packet = enc.encode(&frame, 0).unwrap();
            let mut dec = filmcraft_av1::Decoder::new();
            let mut pics = dec.decode(&packet.data).unwrap();
            pics.extend(dec.flush());
            assert_eq!(pics.len(), 1);
            assert_eq!(pics[0].width, w);
            assert_eq!(pics[0].height, h);
        }
    }

    #[test]
    fn hostile_configuration_returns_errors_without_panicking() {
        for (w, h, n, d) in [(0, 2, 25, 1), (2, 0, 25, 1), (3, 2, 25, 1), (131072, 2, 25, 1), (u32::MAX, u32::MAX, 25, 1), (64, 64, 0, 1), (64, 64, 25, 0)] {
            let result = std::panic::catch_unwind(|| Encoder::new(EncoderConfig::new(w, h, n, d)));
            assert!(matches!(result, Ok(Err(Error::Invalid(_)))));
        }
        let mut c = EncoderConfig::new(64, 64, 25, 1);
        c.key_int = 0;
        assert!(matches!(Encoder::new(c), Err(Error::Invalid(_))));

        // Hostile rate control configs
        let mut c_cbr_zero = EncoderConfig::new(64, 64, 25, 1);
        c_cbr_zero.rate_control = Some(RateControl::Cbr { kbps: 0 });
        assert!(matches!(Encoder::new(c_cbr_zero), Err(Error::Invalid(_))));

        let mut c_vbr_zero = EncoderConfig::new(64, 64, 25, 1);
        c_vbr_zero.rate_control = Some(RateControl::Vbr { target_kbps: 0, max_kbps: 1000 });
        assert!(matches!(Encoder::new(c_vbr_zero), Err(Error::Invalid(_))));

        let mut c_vbr_inv = EncoderConfig::new(64, 64, 25, 1);
        c_vbr_inv.rate_control = Some(RateControl::Vbr { target_kbps: 2000, max_kbps: 1000 });
        assert!(matches!(Encoder::new(c_vbr_inv), Err(Error::Invalid(_))));
    }

    #[test]
    fn test_hostile_buffers_and_strides_do_not_panic() {
        let mut enc = Encoder::new(EncoderConfig::new(64, 64, 24, 1)).unwrap();
        let valid_y = vec![128u8; 64 * 64];
        let valid_uv = vec![128u8; 32 * 32];

        // Empty buffers must return BufferTooSmall without panicking
        let empty_frame = YuvFrame { y: &[], u: &[], v: &[], y_stride: 64, uv_stride: 32 };
        assert!(matches!(enc.encode(&empty_frame, 0), Err(Error::BufferTooSmall)));

        // Undersized Y buffer
        let short_y = YuvFrame { y: &valid_y[..10], u: &valid_uv, v: &valid_uv, y_stride: 64, uv_stride: 32 };
        assert!(matches!(enc.encode(&short_y, 0), Err(Error::BufferTooSmall)));

        // Undersized UV buffer
        let short_uv = YuvFrame { y: &valid_y, u: &valid_uv[..5], v: &valid_uv, y_stride: 64, uv_stride: 32 };
        assert!(matches!(enc.encode(&short_uv, 0), Err(Error::BufferTooSmall)));

        // Hostile strides: usize::MAX must return Err without overflow panic
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let max_stride_frame = YuvFrame { y: &valid_y, u: &valid_uv, v: &valid_uv, y_stride: usize::MAX, uv_stride: 32 };
            enc.encode(&max_stride_frame, 0)
        }));
        assert!(res.is_ok());
        assert!(res.unwrap().is_err());

        // Stride smaller than width
        let narrow_stride_frame = YuvFrame { y: &valid_y, u: &valid_uv, v: &valid_uv, y_stride: 32, uv_stride: 32 };
        assert!(matches!(enc.encode(&narrow_stride_frame, 0), Err(Error::Invalid(_))));

        let narrow_uv_stride_frame = YuvFrame { y: &valid_y, u: &valid_uv, v: &valid_uv, y_stride: 64, uv_stride: 16 };
        assert!(matches!(enc.encode(&narrow_uv_stride_frame, 0), Err(Error::Invalid(_))));
    }

    #[test]
    fn test_hostile_rate_control_extremes_do_not_panic() {
        let mut cfg = EncoderConfig::new(64, 64, 1, u32::MAX);
        cfg.rate_control = Some(RateControl::Cbr { kbps: u32::MAX });
        let mut enc = Encoder::new(cfg).unwrap();

        let y = vec![128u8; 64 * 64];
        let uv = vec![128u8; 32 * 32];
        let frame = YuvFrame { y: &y, u: &uv, v: &uv, y_stride: 64, uv_stride: 32 };

        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = enc.encode(&frame, 0);
            let _ = enc.encode(&frame, 1);
        }));
        assert!(res.is_ok());

        let mut vbr_cfg = EncoderConfig::new(64, 64, 1, u32::MAX);
        vbr_cfg.rate_control = Some(RateControl::Vbr { target_kbps: u32::MAX, max_kbps: u32::MAX });
        let mut vbr_enc = Encoder::new(vbr_cfg).unwrap();
        let res_vbr = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = vbr_enc.encode(&frame, 0);
            let _ = vbr_enc.encode(&frame, 1);
        }));
        assert!(res_vbr.is_ok());
    }

    #[test]
    fn test_quality_metrics_psnr_and_mse() {
        let p1 = vec![128u8; 100];
        let p2 = vec![128u8; 100];
        assert_eq!(compute_mse(&p1, &p2), Some(0.0));
        assert_eq!(compute_psnr(&p1, &p2), Some(100.0));

        let p3 = vec![129u8; 100];
        assert_eq!(compute_mse(&p1, &p3), Some(1.0));
        let psnr = compute_psnr(&p1, &p3).unwrap();
        // 10 * log10(255^2 / 1) ≈ 48.13 dB
        assert!((psnr - 48.13).abs() < 0.1);

        assert_eq!(compute_mse(&[], &[]), None);
        assert_eq!(compute_mse(&[1], &[1, 2]), None);
    }
}
