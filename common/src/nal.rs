//! Minimal H.264 and HEVC Annex-B helpers shared by the host encoder path and the Android decoder.

use crate::protocol::VideoCodec;

pub mod h264 {
    pub const NAL_SLICE: u8 = 1;
    pub const NAL_IDR: u8 = 5;
    pub const NAL_SEI: u8 = 6;
    pub const NAL_SPS: u8 = 7;
    pub const NAL_PPS: u8 = 8;
}

pub mod hevc {
    // VCL NAL units
    pub const NAL_TRAIL_N: u8 = 0;
    pub const NAL_TRAIL_R: u8 = 1;
    pub const NAL_TSA_N: u8 = 2;
    pub const NAL_TSA_R: u8 = 3;
    pub const NAL_STSA_N: u8 = 4;
    pub const NAL_STSA_R: u8 = 5;
    pub const NAL_RADL_N: u8 = 6;
    pub const NAL_RADL_R: u8 = 7;
    pub const NAL_RASL_N: u8 = 8;
    pub const NAL_RASL_R: u8 = 9;

    // IRAP (Keyframe) NAL units
    pub const NAL_BLA_W_LP: u8 = 16;
    pub const NAL_BLA_W_RADL: u8 = 17;
    pub const NAL_BLA_N_LP: u8 = 18;
    pub const NAL_IDR_W_RADL: u8 = 19;
    pub const NAL_IDR_N_LP: u8 = 20;
    pub const NAL_CRA_NUT: u8 = 21;

    // Non-VCL Parameter Sets
    pub const NAL_VPS: u8 = 32;
    pub const NAL_SPS: u8 = 33;
    pub const NAL_PPS: u8 = 34;
    pub const NAL_AUD: u8 = 35;
    pub const NAL_PREFIX_SEI: u8 = 39;
    pub const NAL_SUFFIX_SEI: u8 = 40;
}

// Re-export common H.264 constants for backwards compatibility
pub const NAL_SLICE: u8 = h264::NAL_SLICE;
pub const NAL_IDR: u8 = h264::NAL_IDR;
pub const NAL_SPS: u8 = h264::NAL_SPS;
pub const NAL_PPS: u8 = h264::NAL_PPS;

/// One NAL unit located inside an Annex-B buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalRef {
    /// Offset of the first byte of the start code (`00 00 01` or `00 00 00 01`).
    pub start: usize,
    /// `nal_unit_type` (low 5 bits for H.264, bits 1..6 for HEVC).
    pub kind: u8,
}

/// Iterates over every H.264 NAL unit in `data`, handling 3- and 4-byte start codes.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = NalRef> + '_ {
    nal_units_h264(data)
}

pub fn nal_units_h264(data: &[u8]) -> impl Iterator<Item = NalRef> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                let kind = data[i + 3] & 0x1F;
                i += 3;
                return Some(NalRef { start, kind });
            }
            i += 1;
        }
        None
    })
}

/// Iterates over every HEVC NAL unit in `data`, extracting (byte >> 1) & 0x3F.
/// Validates forbidden_zero_bit, base layer nuh_layer_id, and temporal_id_plus1.
pub fn nal_units_hevc(data: &[u8]) -> impl Iterator<Item = NalRef> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 4 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                let header = data[i + 3];
                let header2 = data[i + 4];
                // forbidden_zero_bit must be 0, base layer nuh_layer_id bit 0 must be 0
                if (header & 0x81) != 0 {
                    i += 3;
                    continue;
                }
                // nuh_temporal_id_plus1 (low 3 bits of header2) cannot be 0 in valid HEVC
                if (header2 & 0x07) == 0 {
                    i += 3;
                    continue;
                }
                let kind = (header >> 1) & 0x3F;
                i += 3;
                return Some(NalRef { start, kind });
            }
            i += 1;
        }
        None
    })
}

/// Summary of what a packet from the encoder contains.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketInfo {
    /// Contains SPS/PPS (and VPS for HEVC).
    pub has_config: bool,
    /// Contains an IDR/IRAP keyframe slice.
    pub has_idr: bool,
    /// Byte offset where the first slice NAL (IDR or non-IDR) begins, if any.
    pub first_slice: Option<usize>,
}

/// Inspection stops at the first slice: parameter sets come before the slices of an access unit,
/// and every slice of a picture has the same IDR-ness, so nothing after it changes the result.
/// (Scanning the whole frame cost about 0.36 us per KB, on the PC and again on the tablet, for
/// every frame.)
pub fn inspect_h264(data: &[u8]) -> PacketInfo {
    let mut info = PacketInfo::default();
    for nal in nal_units_h264(data) {
        match nal.kind {
            h264::NAL_SPS | h264::NAL_PPS => info.has_config = true,
            h264::NAL_IDR => {
                info.has_idr = true;
                info.first_slice.get_or_insert(nal.start);
            }
            h264::NAL_SLICE => {
                info.first_slice.get_or_insert(nal.start);
            }
            _ => {}
        }
        // Do not pull the next NAL: finding it would scan the rest of the frame.
        if info.first_slice.is_some() {
            break;
        }
    }
    info
}

/// See [`inspect_h264`]: stops at the first slice.
pub fn inspect_hevc(data: &[u8]) -> PacketInfo {
    let mut info = PacketInfo::default();
    for nal in nal_units_hevc(data) {
        match nal.kind {
            hevc::NAL_VPS | hevc::NAL_SPS | hevc::NAL_PPS => {
                info.has_config = true;
            }
            hevc::NAL_IDR_W_RADL | hevc::NAL_IDR_N_LP | hevc::NAL_CRA_NUT | 16..=23 => {
                info.has_idr = true;
                info.first_slice.get_or_insert(nal.start);
            }
            0..=9 => {
                info.first_slice.get_or_insert(nal.start);
            }
            _ => {}
        }
        // Do not pull the next NAL: finding it would scan the rest of the frame.
        if info.first_slice.is_some() {
            break;
        }
    }
    info
}

/// Codec-specific packet inspector.
pub fn inspect_for_codec(data: &[u8], codec: VideoCodec) -> PacketInfo {
    match codec {
        VideoCodec::Hevc => inspect_hevc(data),
        VideoCodec::H264 => inspect_h264(data),
        _ => inspect(data),
    }
}

/// Generic inspect function with automatic H.264 vs HEVC detection.
pub fn inspect(data: &[u8]) -> PacketInfo {
    let hevc_info = inspect_hevc(data);
    let h264_info = inspect_h264(data);

    // If HEVC detected an IDR keyframe, prefer HEVC (H.264 parser can falsely match 0x28 IDR as PPS)
    if hevc_info.has_idr {
        return hevc_info;
    }
    if h264_info.has_idr {
        return h264_info;
    }

    if hevc_info.has_config {
        return hevc_info;
    }
    if h264_info.has_config {
        return h264_info;
    }

    if hevc_info.first_slice.is_some() {
        return hevc_info;
    }
    if h264_info.first_slice.is_some() {
        return h264_info;
    }

    PacketInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aud_sps_pps_idr_splits_before_idr() {
        let pkt = [
            0, 0, 0, 1, 0x09, 0x10, // AUD
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 0, 1, 0x68, 3, // PPS
            0, 0, 0, 1, 0x65, 9, 9, // IDR
        ];
        let info = inspect(&pkt);
        assert!(info.has_config && info.has_idr);
        assert_eq!(info.first_slice, Some(19));
    }

    #[test]
    fn three_byte_start_codes_and_p_frames() {
        let pkt = [0, 0, 1, 0x41, 5, 5];
        let info = inspect_h264(&pkt);
        assert!(!info.has_config && !info.has_idr);
        assert_eq!(info.first_slice, Some(0));

        let generic_info = inspect(&pkt);
        assert!(!generic_info.has_config && !generic_info.has_idr);
        assert_eq!(generic_info.first_slice, Some(0));
    }

    #[test]
    fn hevc_vps_sps_pps_idr_inspection() {
        let pkt = [
            0, 0, 0, 1, 0x40, 1, 0x0c, // VPS (32)
            0, 0, 0, 1, 0x42, 1, 1,    // SPS (33)
            0, 0, 0, 1, 0x44, 2,       // PPS (34)
            0, 0, 0, 1, 0x26, 1, 9, 9, // IDR_W_RADL (19)
        ];
        let info = inspect_hevc(&pkt);
        assert!(info.has_config);
        assert!(info.has_idr);
        assert_eq!(info.first_slice, Some(20));

        let generic_info = inspect(&pkt);
        assert!(generic_info.has_config && generic_info.has_idr);
        assert_eq!(generic_info.first_slice, Some(20));
    }

    #[test]
    fn hevc_idr_n_lp_type_20_not_confused_with_h264_pps() {
        // IDR_N_LP (type 20): (20 << 1) = 0x28. Temporal id plus 1 = 1 (0x01).
        // In H.264, 0x28 & 0x1f = 8 (NAL_PPS), which previously caused inspect() to return
        // an H.264 PPS packet with has_idr = false.
        let pkt = [
            0, 0, 0, 1, 0x40, 1, 0x0c, // VPS (32)
            0, 0, 0, 1, 0x42, 1, 1,    // SPS (33)
            0, 0, 0, 1, 0x44, 1,       // PPS (34)
            0, 0, 0, 1, 0x28, 1, 9, 9, // IDR_N_LP (20)
        ];
        let info = inspect_hevc(&pkt);
        assert!(info.has_config);
        assert!(info.has_idr);
        assert_eq!(info.first_slice, Some(20));

        let generic_info = inspect(&pkt);
        assert!(generic_info.has_config && generic_info.has_idr);
        assert_eq!(generic_info.first_slice, Some(20));

        let codec_info = inspect_for_codec(&pkt, VideoCodec::Hevc);
        assert!(codec_info.has_config && codec_info.has_idr);
        assert_eq!(codec_info.first_slice, Some(20));
    }

    #[test]
    fn inspection_stops_at_the_first_slice_with_the_same_result() {
        // IDR picture in two slices with a large body: same answer as a full scan, and the
        // config split point is unchanged.
        let mut pkt = vec![0, 0, 0, 1, 0x40, 1, 0x0c, 0, 0, 0, 1, 0x42, 1, 1, 0, 0, 0, 1, 0x44, 2, 0, 0, 0, 1, 0x26, 1];
        pkt.extend(std::iter::repeat_n(0x55u8, 100_000));
        pkt.extend_from_slice(&[0, 0, 0, 1, 0x26, 1, 7, 7]); // second slice of the same IDR
        let info = inspect_hevc(&pkt);
        assert!(info.has_config && info.has_idr);
        assert_eq!(info.first_slice, Some(20));

        let mut p = vec![0, 0, 0, 1, 0x46, 1, 0x50, 0, 0, 0, 1, 0x02, 1];
        p.extend(std::iter::repeat_n(0x33u8, 100_000));
        let info = inspect_hevc(&p);
        assert!(!info.has_config && !info.has_idr);
        assert_eq!(info.first_slice, Some(7));

        let mut h = vec![0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9];
        h.extend(std::iter::repeat_n(0x77u8, 50_000));
        h.extend_from_slice(&[0, 0, 0, 1, 0x65, 1]);
        let info = inspect_h264(&h);
        assert!(info.has_config && info.has_idr);
        assert_eq!(info.first_slice, Some(18));
    }

    #[test]
    fn hevc_p_frame_inspection() {
        // TRAIL_R (type 1): (1 << 1) = 0x02. Temporal id = 1.
        let pkt = [0, 0, 0, 1, 0x02, 1, 5, 5];
        let info = inspect_hevc(&pkt);
        assert!(!info.has_config && !info.has_idr);
        assert_eq!(info.first_slice, Some(0));

        let generic_info = inspect(&pkt);
        assert!(!generic_info.has_config && !generic_info.has_idr);
        assert_eq!(generic_info.first_slice, Some(0));

        let codec_info = inspect_for_codec(&pkt, VideoCodec::Hevc);
        assert!(!codec_info.has_config && !codec_info.has_idr);
        assert_eq!(codec_info.first_slice, Some(0));
    }
}
