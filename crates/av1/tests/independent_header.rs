//! Synthetic uncompressed headers from AV1 v1.0.0 + Errata 1, sections 5.5/5.9.
use filmcraft_av1::{ColorConfig, SequenceHeader, independent_frame_header_info};

fn sequence(reduced: bool) -> SequenceHeader {
    SequenceHeader {
        profile: 0,
        still_picture: reduced,
        reduced_still_picture_header: reduced,
        frame_width_bits: 6,
        frame_height_bits: 6,
        max_frame_width: 64,
        max_frame_height: 64,
        seq_force_screen_content_tools: if reduced { 2 } else { 0 },
        seq_force_integer_mv: 2,
        color: ColorConfig { bit_depth: 8, num_planes: 3, subsampling_x: 1, subsampling_y: 1, ..Default::default() },
        ..Default::default()
    }
}

#[test]
fn checked_independent_header_regular_and_reduced_still() {
    for (reduced, payload) in [(false, [0x19, 0, 0]), (true, [0x90, 0, 0])] {
        let h = independent_frame_header_info(&sequence(reduced), &payload, 0, 0).unwrap().unwrap();
        assert_eq!(h.size, (64, 64));
    }
}

#[test]
fn checked_independent_header_declines_dependent_or_nonbase_frames() {
    let s = sequence(false);
    for payload in [[0x80], [0x30], [0x00]] {
        assert!(independent_frame_header_info(&s, &payload, 0, 0).unwrap().is_none());
    }
    assert!(independent_frame_header_info(&s, &[0x19, 0, 0], 1, 0).unwrap().is_none());
    let mut scalable = s;
    scalable.operating_point_idc[0] = 0x101;
    assert!(independent_frame_header_info(&scalable, &[0x19, 0, 0], 0, 0).unwrap().is_none());
}

#[test]
fn checked_independent_header_requires_the_complete_header() {
    let s = sequence(false);
    for payload in [&[][..], &[0x19][..], &[0x19, 0][..]] {
        assert!(independent_frame_header_info(&s, payload, 0, 0).is_err());
    }
}

#[test]
fn checked_independent_header_hostile_public_sequence_and_mutations_do_not_panic() {
    let mut s = sequence(false);
    s.max_frame_width = u32::MAX;
    s.frame_width_bits = u32::MAX;
    assert!(independent_frame_header_info(&s, &[0x19, 0, 0], 0, 0).is_err());
    for bit in 0..24 {
        let mut payload = [0x19, 0, 0];
        payload[bit / 8] ^= 1 << (bit % 8);
        assert!(std::panic::catch_unwind(|| independent_frame_header_info(&sequence(false), &payload, 0, 0)).is_ok());
    }
}

/// An AV1 sequence header written MSB-first, with enough structure to validate
/// operating-point level/tier parsing and all trailing bits independently.
fn sequence_with_operating_points(points: &[(u32, bool)]) -> Vec<u8> {
    assert!(!points.is_empty() && points.len() <= 32);
    let mut bits = Vec::new();
    let mut write = |value: u32, width: usize| {
        for shift in (0..width).rev() {
            bits.push(((value >> shift) & 1) != 0);
        }
    };
    write(0, 3); // Main profile
    write(0, 1); // not a still picture
    write(0, 1); // full (not reduced) sequence header
    write(0, 1); // no timing info
    write(0, 1); // no initial display delay
    write((points.len() - 1) as u32, 5);
    for &(level, high_tier) in points {
        write(0, 12); // operating point idc
        write(level, 5);
        if level > 7 {
            write(u32::from(high_tier), 1);
        }
    }
    write(5, 4); // six frame-width bits
    write(5, 4); // six frame-height bits
    write(63, 6); // 64 pixel maximum width
    write(63, 6); // 64 pixel maximum height
    write(0, 1); // no frame IDs
    for _ in 0..8 {
        write(0, 1); // all sequence prediction tools disabled
    }
    write(0, 1); // select_screen_content_tools is explicit
    write(0, 1); // screen-content tools disabled
    for _ in 0..3 {
        write(0, 1); // superres, CDEF and restoration disabled
    }
    write(0, 1); // 8-bit
    write(0, 1); // not monochrome
    write(1, 1); // color description present
    for _ in 0..3 {
        write(1, 8); // BT.709 primaries, transfer and matrix
    }
    write(0, 1); // limited range
    write(0, 2); // chroma sample position unknown
    write(0, 1); // no separate UV delta-q
    write(0, 1); // no film grain
    write(1, 1); // required trailing_one_bit
    bits.resize(bits.len().div_ceil(8) * 8, false);
    bits.chunks(8).map(|chunk| chunk.iter().fold(0u8, |byte, &bit| (byte << 1) | u8::from(bit))).collect()
}

#[test]
fn sequence_header_preserves_high_tier_for_each_operating_point() {
    let header = SequenceHeader::parse_checked(&sequence_with_operating_points(&[(8, true), (9, false)])).unwrap();
    assert_eq!(&header.seq_level_idx[..2], &[8, 9]);
    assert_eq!(&header.seq_tier[..2], &[true, false]);
    assert_eq!(header.color.bit_depth, 8);
    let header = SequenceHeader::parse_checked(&sequence_with_operating_points(&[(7, true), (8, true), (9, false)])).unwrap();
    assert_eq!(&header.seq_level_idx[..3], &[7, 8, 9]);
    assert_eq!(&header.seq_tier[..3], &[false, true, false], "levels below 4.0 cannot signal high tier");
}
