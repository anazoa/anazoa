//! H.264 carrier framing and decoder-replay for the video tunnel.
//!
//! VP9's tunnel is easy because libwebrtc's VP9 RTP packetizer treats the
//! encoded frame as an opaque blob and `show_existing_frame` (0x88) is a legal
//! 1-byte "repeat last frame".  H.264 has neither property:
//!
//!  * `RtpPacketizerH264` splits the encoded buffer on Annex-B start codes and
//!    the receiver's depacketizer / `H264SpsPpsTracker` parse every NAL header
//!    and slice header.  A carrier frame therefore has to keep the encoder's
//!    exact NAL layout (count, header bytes, lengths) and every slice NAL needs
//!    a parseable slice-header prefix, or the packet is dropped on receive.
//!  * FFmpeg's H.264 decoder hard-errors on an empty or malformed access unit
//!    (libvpx conceals), so the replacement stream fed to the peer's decoder
//!    must be a real, continuously decodable bitstream.
//!
//! ## Send side — [`pack_carrier`]
//!
//! The real openh264 output is rewritten in place, same total length:
//!  * SPS (7) / PPS (8) NALs pass through untouched — the depacketizer parses
//!    them and they carry the real picture size.
//!  * Every slice NAL (1 non-IDR, 5 IDR) keeps its header byte and length; its
//!    body becomes `[SLICE_PREFIX] COBS([u16 chunk_len][tunnel bytes])`,
//!    `0xFF`-padded to the original length.  COBS output has no zero bytes,
//!    so no start code or emulation-prevention sequence can appear in it.
//!    `SLICE_PREFIX` decodes as `first_mb_in_slice=0, slice_type=5,
//!    pic_parameter_set_id=0` so `PpsParser::ParseSliceHeader` succeeds and the
//!    frame is delimited correctly.
//!  * SEI (6) NALs carry `COBS([u16 chunk_len][tunnel bytes])` with no
//!    prefix (the depacketizer does not parse SEI bodies).
//!
//! ## Receive side — [`unpack_carrier`] + [`H264Replayer`]
//!
//! [`unpack_carrier`] reverses the above to recover the logical tunnel byte
//! stream.  [`H264Replayer`] then emits the frame handed to the peer's decoder:
//! a prebuilt static-picture GOP ([`H264_STATIC_STREAMS`]) replayed in decode
//! order, injecting a fresh IDR whenever the carrier frame is a keyframe or the
//! P-slice run wraps.

use super::media::{VIDEO_HEIGHT, VIDEO_WIDTH};

include!("h264_tables.rs");

/// 4-byte Annex-B start code.  openh264 always emits the 4-byte form.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Slice-header bits prepended to every rewritten slice NAL body:
/// `first_mb_in_slice` ue(0)=`1`, `slice_type` ue(5)=`00110`, `pic_parameter_set_id`
/// ue(0)=`1`, then a `0` pad bit → `1001 1010` = 0x9A.  `PpsParser::ParseSliceHeader`
/// reads exactly those three Exp-Golomb values and stops.
const SLICE_PREFIX: [u8; 1] = [0x9A];

/// Length field (u16 LE) stored at the front of each NAL's writable region.
const CHUNK_LEN_BYTES: usize = 2;

/// Byte used to pad a rewritten NAL body back up to its original length after
/// COBS encoding.  Nonzero, so it never participates in a start code.
const PAD_BYTE: u8 = 0xFF;

// H.264 NAL unit types we care about.
const NAL_SLICE_NON_IDR: u8 = 1;
const NAL_SLICE_IDR: u8 = 5;
const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
#[cfg(test)]
const NAL_PPS: u8 = 8;

#[inline]
fn nal_type(header: u8) -> u8 {
    header & 0x1F
}

/// A NAL unit located inside an Annex-B buffer.
#[derive(Debug, Clone, Copy)]
struct NalSpan {
    /// Offset of the NAL header byte (just past the start code).
    header: usize,
    /// Exclusive end of the NAL payload (start of the next start code, or EOF).
    end: usize,
}

impl NalSpan {
    fn header_byte(&self, buf: &[u8]) -> u8 {
        buf[self.header]
    }
    /// Byte range of the RBSP (everything after the 1-byte NAL header).
    fn body(&self) -> std::ops::Range<usize> {
        self.header + 1..self.end
    }
}

/// Scan an Annex-B buffer, returning one [`NalSpan`] per NAL unit.
///
/// Mirrors `webrtc::H264::FindNaluIndices`: a start code is `00 00 01`
/// optionally preceded by an extra `00`.
fn scan_nals(buf: &[u8]) -> Vec<NalSpan> {
    let mut nals: Vec<NalSpan> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let header = i + 3;
            if let Some(prev) = nals.last_mut() {
                // Trim a trailing 0x00 that belongs to this 4-byte start code.
                prev.end = if i > prev.header && buf[i - 1] == 0 {
                    i - 1
                } else {
                    i
                };
            }
            if header < buf.len() {
                nals.push(NalSpan {
                    header,
                    end: buf.len(),
                });
            }
            i = header;
        } else {
            i += 1;
        }
    }
    nals
}

// ---------------------------------------------------------------------------
// Emulation prevention (H.264 Annex-B, RBSP <-> SODB-ish)
// ---------------------------------------------------------------------------

/// Insert emulation-prevention bytes: `00 00 {00,01,02,03}` → `00 00 03 {..}`.
#[cfg(test)]
fn epb_encode(src: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0usize;
    for &b in src {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

/// Remove emulation-prevention bytes, as found in real encoder SPS NALs.
fn epb_decode(src: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0usize;
    let mut i = 0usize;
    while i < src.len() {
        let b = src[i];
        if zeros >= 2 && b == 0x03 && i + 1 < src.len() && src[i + 1] <= 0x03 {
            zeros = 0;
            i += 1;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
}

/// EPB-encoded length of `src` without materialising it.
#[cfg(test)]
fn epb_encoded_len(src: &[u8]) -> usize {
    let mut zeros = 0usize;
    let mut n = 0usize;
    for &b in src {
        if zeros >= 2 && b <= 0x03 {
            n += 1;
            zeros = 0;
        }
        n += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    n
}

// ---------------------------------------------------------------------------
// COBS — consistent overhead byte stuffing.  Removes every zero byte, so tunnel
// bytes can never form a start code, at a worst case of 1 byte per 254.
// ---------------------------------------------------------------------------

/// Encode `src` with no zero bytes: each block is a code byte `n` followed by
/// `n - 1` nonzero bytes and an implied zero, except `n == 0xFF` (no zero).
fn cobs_encode(src: &[u8], out: &mut Vec<u8>) {
    let mut code_pos = out.len();
    out.push(0);
    let mut code = 1u8;
    for &b in src {
        if b != 0 {
            out.push(b);
            code += 1;
        }
        if b == 0 || code == 0xFF {
            out[code_pos] = code;
            code_pos = out.len();
            out.push(0);
            code = 1;
        }
    }
    out[code_pos] = code;
}

/// Largest COBS output for `n` input bytes (no zeros to absorb a code byte).
fn cobs_max_len(n: usize) -> usize {
    n + 1 + n / 254
}

/// Decode the first `want` bytes of a [`cobs_encode`] output, ignoring
/// whatever follows (padding).  `None` if `src` is malformed or too short.
fn cobs_decode_prefix(src: &[u8], want: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(want + 1);
    let mut i = 0usize;
    while out.len() < want {
        let code = *src.get(i)? as usize;
        if code == 0 {
            return None;
        }
        out.extend_from_slice(src.get(i + 1..i + code)?);
        i += code;
        if code != 0xFF {
            out.push(0);
        }
    }
    out.truncate(want);
    Some(out)
}

// ---------------------------------------------------------------------------
// Per-NAL rewrite plan
// ---------------------------------------------------------------------------

struct RewritePlan {
    /// Index into `scan_nals` output.
    body: std::ops::Range<usize>,
    /// Bytes of `SLICE_PREFIX` at the front of the writable region (0 for SEI).
    prefix_len: usize,
    /// Max whitened tunnel bytes this NAL can carry.
    capacity: usize,
}

/// Build the rewrite plan for the slice / SEI NALs of one encoded frame.
fn plan_rewrites(buf: &[u8], nals: &[NalSpan]) -> Vec<RewritePlan> {
    let mut plans = Vec::new();
    for n in nals {
        let t = nal_type(n.header_byte(buf));
        let prefix_len = match t {
            NAL_SLICE_NON_IDR | NAL_SLICE_IDR => SLICE_PREFIX.len(),
            NAL_SEI => 0,
            // SPS / PPS carry the real picture size and must parse on receive;
            // everything else (AUD, filler, …) is left untouched too.
            _ => continue,
        };
        let body = n.body();
        // Largest raw length whose worst-case COBS encoding fits after the
        // prefix.  The length field is only u16.
        let region = body.len().saturating_sub(prefix_len);
        let mut raw = (region * 254 / 255).saturating_sub(1);
        while cobs_max_len(raw + 1) <= region {
            raw += 1;
        }
        let capacity = raw.saturating_sub(CHUNK_LEN_BYTES).min(u16::MAX as usize);
        if capacity == 0 {
            continue;
        }
        plans.push(RewritePlan {
            body,
            prefix_len,
            capacity,
        });
    }
    plans
}

/// Total tunnel bytes the given encoder output can carry.
pub fn carrier_capacity(encoder_output: &[u8]) -> usize {
    let nals = scan_nals(encoder_output);
    plan_rewrites(encoder_output, &nals)
        .iter()
        .map(|p| p.capacity)
        .sum()
}

/// Rewrite `buf` (a complete openh264 access unit, Annex-B) in place so its NAL
/// bodies carry `tunnel` while every NAL keeps its original type and length.
///
/// Returns the number of tunnel bytes actually stored (<= `tunnel.len()`).
/// Every payload up to [`carrier_capacity`] fits, including worst-case escaping.
/// Callers must limit the payload to that capacity before dequeuing data.
pub fn pack_carrier(buf: &mut [u8], tunnel: &[u8]) -> usize {
    let nals = scan_nals(buf);
    let plans = plan_rewrites(buf, &nals);
    let mut written = 0usize;
    let mut raw = Vec::new();

    for plan in &plans {
        let body_len = plan.body.len();
        let take = (tunnel.len() - written).min(plan.capacity);

        // [prefix] COBS([len u16 LE][chunk]) [padding]
        raw.clear();
        raw.extend_from_slice(&(take as u16).to_le_bytes());
        raw.extend_from_slice(&tunnel[written..written + take]);
        let mut enc = Vec::with_capacity(body_len);
        enc.extend_from_slice(&SLICE_PREFIX[..plan.prefix_len]);
        cobs_encode(&raw, &mut enc);
        debug_assert!(enc.len() <= body_len);
        enc.resize(body_len, PAD_BYTE);
        buf[plan.body.clone()].copy_from_slice(&enc);
        written += take;
    }

    written
}

/// Recover the logical tunnel byte stream from a carrier frame produced by a
/// peer's [`pack_carrier`].
pub fn unpack_carrier(buf: &[u8]) -> Vec<u8> {
    let nals = scan_nals(buf);
    let mut out = Vec::new();

    for plan in plan_rewrites(buf, &nals) {
        let region = &buf[plan.body][plan.prefix_len..];
        // Decode the u16 length first, then exactly that many chunk bytes.
        let Some(head) = cobs_decode_prefix(region, CHUNK_LEN_BYTES) else {
            return Vec::new();
        };
        let len = u16::from_le_bytes([head[0], head[1]]) as usize;
        if len > plan.capacity {
            return Vec::new();
        }
        let Some(chunk) = cobs_decode_prefix(region, CHUNK_LEN_BYTES + len) else {
            return Vec::new();
        };
        out.extend_from_slice(&chunk[CHUNK_LEN_BYTES..]);
    }
    out
}

// ---------------------------------------------------------------------------
// Decoder replay
// ---------------------------------------------------------------------------

/// Pick the prebuilt stream for `(w, h)`, falling back to the default capture
/// resolution and then to the first entry.
pub fn static_stream(w: u16, h: u16) -> &'static PrebuiltH264Stream<'static> {
    H264_STATIC_STREAMS
        .iter()
        .find(|s| s.width == w && s.height == h)
        .or_else(|| {
            H264_STATIC_STREAMS
                .iter()
                .find(|s| s.width == VIDEO_WIDTH as u16 && s.height == VIDEO_HEIGHT as u16)
        })
        .unwrap_or(&H264_STATIC_STREAMS[0])
}

fn push_nal(out: &mut Vec<u8>, nal: &PrebuiltH264Nal) {
    out.extend_from_slice(&START_CODE);
    out.push(nal.header);
    out.extend_from_slice(nal.body);
}

/// Emit an Annex-B keyframe access unit (SPS + PPS + IDR) for `stream`.
pub fn keyframe_bitstream(stream: &PrebuiltH264Stream) -> Vec<u8> {
    let mut out = Vec::new();
    push_nal(&mut out, &stream.sps);
    push_nal(&mut out, &stream.pps);
    push_nal(&mut out, &stream.idr);
    out
}

/// Emit the Annex-B P-slice access unit at `idx` within `stream`'s run.
pub fn delta_bitstream(stream: &PrebuiltH264Stream, idx: usize) -> Vec<u8> {
    let mut out = Vec::new();
    push_nal(&mut out, &stream.deltas[idx % stream.deltas.len()]);
    out
}

/// Tracks replay position so [`H264Replayer::next`] produces a continuously
/// decodable bitstream for the peer's FFmpeg decoder.
pub struct H264Replayer {
    width: u16,
    height: u16,
    /// Index of the next P-slice to emit; `None` means "emit an IDR next".
    pos: Option<usize>,
}

impl H264Replayer {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pos: None,
        }
    }

    /// Update the replay resolution from the carrier's SPS.
    pub fn set_resolution(&mut self, width: u16, height: u16) {
        if (width, height) != (self.width, self.height) && width > 0 && height > 0 {
            self.width = width;
            self.height = height;
            self.pos = None; // force an IDR at the new resolution
        }
    }

    /// Next replacement access unit.  `carrier_is_key` mirrors the frame type of
    /// the carrier; `need_key` forces an IDR (e.g. first frame of a call).
    /// Returns `(bitstream, is_key_frame)`.
    pub fn next(&mut self, carrier_is_key: bool, need_key: bool) -> (Vec<u8>, bool) {
        let stream = static_stream(self.width, self.height);
        let emit_key = need_key || carrier_is_key || self.pos.is_none();
        if emit_key {
            self.pos = Some(0);
            return (keyframe_bitstream(stream), true);
        }
        let idx = self.pos.unwrap();
        let bs = delta_bitstream(stream, idx);
        self.pos = if idx + 1 >= stream.deltas.len() {
            None // wrap → IDR next call
        } else {
            Some(idx + 1)
        };
        (bs, false)
    }
}

// ---------------------------------------------------------------------------
// SPS dimension parsing — track the real encoder resolution from the carrier's
// (untouched) SPS so the replay stream picks the matching prebuilt GOP.
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }
    fn read_bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.bit / 8)?;
        let b = (byte >> (7 - self.bit % 8)) & 1;
        self.bit += 1;
        Some(b as u32)
    }
    fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Some(v)
    }
    /// Unsigned Exp-Golomb, `ue(v)`.
    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u32 << zeros) - 1) + self.read_bits(zeros)?)
    }
    /// Signed Exp-Golomb, `se(v)`.
    fn read_se(&mut self) -> Option<i32> {
        let k = self.read_ue()?;
        let mag = k.div_ceil(2) as i32;
        Some(if k % 2 == 0 { -mag } else { mag })
    }
}

/// Parse dimensions from an SPS NAL payload, without its header byte.
/// Removes emulation-prevention bytes and handles chroma-dependent cropping.
pub fn sps_dimensions(sps: &[u8]) -> Option<(u16, u16)> {
    let mut rbsp = Vec::with_capacity(sps.len());
    epb_decode(sps, &mut rbsp);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.read_bits(8)?;
    r.read_bits(8)?; // constraint flags + reserved
    r.read_bits(8)?; // level_idc
    if r.read_ue()? > 31 {
        return None;
    }

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = r.read_bit()? != 0;
        }
        r.read_ue()?; // bit_depth_luma_minus8
        r.read_ue()?; // bit_depth_chroma_minus8
        r.read_bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.read_bit()? == 1 {
            // seq_scaling_matrix_present_flag — skip the scaling lists
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..count {
                if r.read_bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..size {
                        if next != 0 {
                            next =
                                (i64::from(last) + i64::from(r.read_se()?)).rem_euclid(256) as i32;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }

    if r.read_ue()? > 12 {
        return None;
    }
    let poc_type = r.read_ue()?;
    if poc_type == 0 {
        if r.read_ue()? > 12 {
            return None;
        }
    } else if poc_type == 1 {
        r.read_bit()?; // delta_pic_order_always_zero_flag
        r.read_se()?; // offset_for_non_ref_pic
        r.read_se()?; // offset_for_top_to_bottom_field
        let n = r.read_ue()?;
        if n > 255 {
            return None;
        }
        for _ in 0..n {
            r.read_se()?;
        }
    } else if poc_type != 2 {
        return None;
    }
    r.read_ue()?; // max_num_ref_frames
    r.read_bit()?; // gaps_in_frame_num_value_allowed_flag

    let width_mbs = r.read_ue()?.checked_add(1)?;
    let height_map_units = r.read_ue()?.checked_add(1)?;
    let frame_mbs_only = r.read_bit()?;
    if frame_mbs_only == 0 {
        r.read_bit()?; // mb_adaptive_frame_field_flag
    }
    r.read_bit()?; // direct_8x8_inference_flag

    let mut crop = (0u32, 0u32, 0u32, 0u32);
    if r.read_bit()? == 1 {
        crop = (r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?);
    }

    let frame_height_factor = 2 - frame_mbs_only;
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (crop_unit_x, chroma_height) = match chroma_array_type {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => return None,
    };
    let crop_unit_y = chroma_height * frame_height_factor;
    let cropped_x = crop.0.checked_add(crop.1)?.checked_mul(crop_unit_x)?;
    let cropped_y = crop.2.checked_add(crop.3)?.checked_mul(crop_unit_y)?;
    let width = width_mbs.checked_mul(16)?.checked_sub(cropped_x)?;
    let height = height_map_units
        .checked_mul(16)?
        .checked_mul(frame_height_factor)?
        .checked_sub(cropped_y)?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
}

/// Parse the encoder resolution from an Annex-B access unit containing an SPS.
pub fn access_unit_dimensions(buf: &[u8]) -> Option<(u16, u16)> {
    scan_nals(buf).iter().find_map(|n| {
        (nal_type(n.header_byte(buf)) == NAL_SPS).then(|| sps_dimensions(&buf[n.body()]))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_test_nal(frame: &mut Vec<u8>, start: &[u8], header: u8, body_len: usize) {
        frame.extend_from_slice(start);
        frame.push(header);
        frame.resize(frame.len() + body_len, 0x55);
    }

    #[test]
    fn carrier_small_nals_round_trip() {
        for header in [0x41, 0x06] {
            for small_len in 0..64 {
                let mut frame = Vec::new();
                append_test_nal(&mut frame, &START_CODE, header, small_len);
                append_test_nal(&mut frame, &START_CODE, 0x41, 1024);
                let payload = vec![0x5a; carrier_capacity(&frame)];
                assert_eq!(pack_carrier(&mut frame, &payload), payload.len());
                assert_eq!(
                    unpack_carrier(&frame),
                    payload,
                    "NAL {header:#x}, size {small_len}"
                );
            }
        }
    }

    #[test]
    fn carrier_caps_chunks_without_losing_later_nals() {
        let mut frame = Vec::new();
        append_test_nal(&mut frame, &START_CODE, 0x65, 120_000);
        assert_eq!(carrier_capacity(&frame), u16::MAX as usize);
        append_test_nal(&mut frame, &START_CODE, 0x41, 12_000);
        let payload: Vec<_> = (0..carrier_capacity(&frame)).map(|i| i as u8).collect();
        assert!(payload.len() > u16::MAX as usize);
        assert_eq!(pack_carrier(&mut frame, &payload), payload.len());
        assert_eq!(unpack_carrier(&frame), payload);
    }

    /// Body bytes after the NAL header, between start codes.
    fn nal_bodies(frame: &[u8]) -> Vec<&[u8]> {
        scan_nals(frame).iter().map(|n| &frame[n.body()]).collect()
    }

    #[test]
    fn advertised_capacity_is_exact_for_worst_case_cobs() {
        for body_len in 0..2048 {
            let mut frame = Vec::new();
            append_test_nal(&mut frame, &START_CODE, 0x41, body_len);
            let capacity = carrier_capacity(&frame);
            let region = body_len.saturating_sub(SLICE_PREFIX.len());
            // Tight: one more byte might not fit.
            if capacity > 0 {
                assert!(cobs_max_len(CHUNK_LEN_BYTES + capacity) <= region);
                assert!(cobs_max_len(CHUNK_LEN_BYTES + capacity + 1) > region);
            }
            append_test_nal(&mut frame, &[0, 0, 1], 0x41, 1024);
            // No zeros anywhere is the worst case; all zeros is the other extreme.
            for fill in [0x5a, 0x00] {
                let mut f = frame.clone();
                let payload = vec![fill; carrier_capacity(&f)];
                assert_eq!(pack_carrier(&mut f, &payload), payload.len());
                assert_eq!(f.len(), frame.len());
                let bodies = nal_bodies(&f);
                assert_eq!(bodies.len(), 2);
                if capacity > 0 {
                    assert!(!bodies[0].contains(&0), "body {body_len}");
                }
                assert_eq!(unpack_carrier(&f), payload, "body {body_len}");
            }
        }
    }

    #[test]
    fn carrier_rejects_malformed_cobs() {
        let mut frame = Vec::new();
        append_test_nal(&mut frame, &START_CODE, 0x41, 64);
        let payload = vec![7; 20];
        assert_eq!(pack_carrier(&mut frame, &payload), payload.len());
        let mut zero_code = frame.clone();
        zero_code[START_CODE.len() + 2] = 0;
        assert!(unpack_carrier(&zero_code).is_empty());
        let mut overrun = frame.clone();
        overrun[START_CODE.len() + 2] = 0xFE;
        assert!(unpack_carrier(&overrun).is_empty());
    }

    #[test]
    fn cobs_round_trips() {
        let long: Vec<u8> = (0..600).map(|i| (i % 255 + 1) as u8).collect();
        let mut mixed = long.clone();
        mixed[253] = 0;
        mixed[254] = 0;
        let cases: &[(&[u8], &[u8])] = &[
            (&[], &[1]),
            (&[0], &[1, 1]),
            (&[0x11, 0x22, 0, 0x33], &[3, 0x11, 0x22, 2, 0x33]),
            (
                &[0x11, 0x22, 0, 0x33, 0x44, 0x55, 0, 0x66],
                &[3, 0x11, 0x22, 4, 0x33, 0x44, 0x55, 2, 0x66],
            ),
            (&long, &[]),
            (&long[..254], &[]),
            (&mixed, &[]),
            (&[0; 300], &[]),
        ];
        for &(src, want) in cases {
            let mut enc = Vec::new();
            cobs_encode(src, &mut enc);
            if !want.is_empty() {
                assert_eq!(enc, want);
            }
            assert!(!enc.contains(&0), "{src:?}");
            assert!(enc.len() <= cobs_max_len(src.len()));
            // Trailing padding is ignored.
            enc.extend_from_slice(&[PAD_BYTE; 300]);
            assert_eq!(cobs_decode_prefix(&enc, src.len()).as_deref(), Some(src));
        }
        let mut enc = Vec::new();
        cobs_encode(&long, &mut enc);
        assert_eq!(enc.len(), cobs_max_len(long.len()));
    }

    fn test_sps(
        chroma: u32,
        separate_plane: bool,
        interlaced: bool,
        width_minus1: u32,
        height_minus1: u32,
        crop: [u32; 4],
    ) -> Vec<u8> {
        fn bits(out: &mut Vec<bool>, value: u64, width: u32) {
            for i in (0..width).rev() {
                out.push(value & (1 << i) != 0);
            }
        }
        fn ue(out: &mut Vec<bool>, value: u32) {
            let value = u64::from(value) + 1;
            let width = 64 - value.leading_zeros();
            out.extend(std::iter::repeat_n(false, (width - 1) as usize));
            bits(out, value, width);
        }
        let mut out = Vec::new();
        bits(&mut out, 100, 8); // high profile
        bits(&mut out, 0, 8);
        bits(&mut out, 31, 8);
        ue(&mut out, 0); // SPS id
        ue(&mut out, chroma);
        if chroma == 3 {
            out.push(separate_plane);
        }
        ue(&mut out, 0); // luma bit depth
        ue(&mut out, 0); // chroma bit depth
        out.extend([false, false]); // bypass/scaling matrix
        for value in [0, 0, 0, 3] {
            ue(&mut out, value);
        } // frame num, POC, refs
        out.push(false); // gaps allowed
        ue(&mut out, width_minus1);
        ue(&mut out, height_minus1);
        out.push(!interlaced);
        if interlaced {
            out.push(false);
        }
        out.extend([true, true]); // direct inference, crop present
        for value in crop {
            ue(&mut out, value);
        }
        out.push(true); // RBSP stop bit
        let mut rbsp = vec![0; out.len().div_ceil(8)];
        for (i, bit) in out.into_iter().enumerate() {
            if bit {
                rbsp[i / 8] |= 1 << (7 - i % 8);
            }
        }
        let mut escaped = Vec::new();
        epb_encode(&rbsp, &mut escaped);
        escaped
    }

    #[test]
    fn sps_rejects_overflow_underflow_zero_and_truncated_dimensions() {
        for (width, height, crop) in [
            (u32::MAX, 0, [0; 4]),
            (u32::MAX - 1, 0, [0; 4]),
            (0, u32::MAX - 1, [0; 4]),
            (0, 0, [100, 0, 0, 0]),
            (0, 0, [0, 0, 100, 0]),
            (0, 0, [u32::MAX - 1, u32::MAX - 1, 0, 0]),
            (0, 0, [8, 0, 0, 0]),
        ] {
            assert_eq!(
                sps_dimensions(&test_sps(1, false, false, width, height, crop)),
                None
            );
        }
        assert_eq!(sps_dimensions(&[100, 0, 31]), None);
        assert_eq!(
            sps_dimensions(&test_sps(4, false, false, 1, 1, [0; 4])),
            None
        );
    }

    #[test]
    fn sps_cropping_uses_chroma_and_field_units() {
        for (chroma, separate, interlaced, expected) in [
            (1, false, false, (28, 28)),
            (1, false, true, (28, 56)),
            (2, false, false, (28, 30)),
            (2, false, true, (28, 60)),
            (3, false, false, (30, 30)),
            (3, true, true, (30, 60)),
            (0, false, false, (30, 30)),
        ] {
            let sps = test_sps(chroma, separate, interlaced, 1, 1, [1; 4]);
            assert_eq!(sps_dimensions(&sps), Some(expected));
        }
    }

    #[test]
    fn sps_removes_emulation_prevention_before_reading_dimensions() {
        let sps = test_sps(3, false, false, 2047, 2047, [0; 4]);
        assert!(sps.windows(3).any(|w| w == [0, 0, 3]));
        assert_eq!(sps_dimensions(&sps), Some((32768, 32768)));
    }

    #[test]
    fn epb_round_trips() {
        let cases: &[&[u8]] = &[
            &[],
            &[0, 0, 0],
            &[0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 0],
            &[1, 2, 3, 0, 0, 0xFF, 0, 0, 0x00],
            &[0; 20],
        ];
        for c in cases {
            let mut enc = Vec::new();
            epb_encode(c, &mut enc);
            assert_eq!(epb_encoded_len(c), enc.len());
            // No start-code / spec-forbidden sequence remains: after any
            // `00 00` the next byte is `03` (escape) or `>= 0x04`, never
            // `00`/`01`/`02`.
            for w in enc.windows(3) {
                if w[0] == 0 && w[1] == 0 {
                    assert!(w[2] >= 0x03, "{enc:?}");
                }
            }
            let mut dec = Vec::new();
            epb_decode(&enc, &mut dec);
            assert_eq!(&dec, c);
        }
    }

    #[test]
    fn sps_dimensions_match_each_static_stream() {
        for s in H264_STATIC_STREAMS {
            assert_eq!(
                sps_dimensions(s.sps.body),
                Some((s.width, s.height)),
                "{}x{}",
                s.width,
                s.height
            );
            let kf = keyframe_bitstream(s);
            assert_eq!(access_unit_dimensions(&kf), Some((s.width, s.height)));
        }
    }

    #[test]
    fn all_static_streams_present_and_shaped() {
        let want = [
            (1280, 720),
            (960, 540),
            (640, 360),
            (480, 270),
            (480, 264),
            (320, 180),
            (240, 134),
        ];
        for (w, h) in want {
            let s = static_stream(w, h);
            assert_eq!((s.width, s.height), (w, h));
            assert_eq!(nal_type(s.sps.header), NAL_SPS);
            assert_eq!(nal_type(s.pps.header), NAL_PPS);
            assert_eq!(nal_type(s.idr.header), NAL_SLICE_IDR);
            assert!(s.deltas.len() >= 64);
            for d in s.deltas {
                assert_eq!(nal_type(d.header), NAL_SLICE_NON_IDR);
            }
        }
    }

    #[test]
    fn replayer_tracks_cropped_encoder_resolution_without_falling_back() {
        let mut replayer = H264Replayer::new(640, 360);
        let _ = replayer.next(false, false);
        replayer.set_resolution(480, 264);
        let (keyframe, is_key) = replayer.next(false, false);
        assert!(is_key);
        assert_eq!(access_unit_dimensions(&keyframe), Some((480, 264)));
        let (delta, is_key) = replayer.next(false, false);
        assert!(!is_key);
        assert_eq!(delta, delta_bitstream(static_stream(480, 264), 0));
        replayer.set_resolution(480, 270);
        let (keyframe, is_key) = replayer.next(false, false);
        assert!(is_key);
        assert_eq!(access_unit_dimensions(&keyframe), Some((480, 270)));
    }

    #[test]
    fn replayer_emits_key_then_deltas_then_wraps() {
        let s = static_stream(640, 360);
        let mut r = H264Replayer::new(640, 360);
        let (bs, key) = r.next(false, false);
        assert!(key);
        assert_eq!(bs, keyframe_bitstream(s));
        for i in 0..s.deltas.len() - 1 {
            let (bs, key) = r.next(false, false);
            assert!(!key);
            assert_eq!(bs, delta_bitstream(s, i));
        }
        // last delta of the run, then a wrap back to a keyframe
        let (_, key) = r.next(false, false);
        assert!(!key);
        let (_, key) = r.next(false, false);
        assert!(key);
    }

    #[test]
    fn carrier_round_trips_delta_frame() {
        // Minimal single-slice P access unit: start code + header + body.
        let mut frame = Vec::new();
        frame.extend_from_slice(&START_CODE);
        frame.push(0x41); // non-IDR slice, nri=2
        frame.extend_from_slice(&[0xAB; 400]);
        let orig_len = frame.len();

        let cap = carrier_capacity(&frame);
        assert!(cap > 200, "cap={cap}");

        let tunnel: Vec<u8> = (0..cap as u16).map(|i| (i * 7 + 1) as u8).collect();
        let stored = pack_carrier(&mut frame, &tunnel);
        assert_eq!(frame.len(), orig_len, "carrier length preserved");
        assert_eq!(stored, tunnel.len());

        // start code + header untouched, slice header parseable prefix in place
        assert_eq!(&frame[..4], &START_CODE);
        assert_eq!(frame[4], 0x41);

        // no spurious start codes inside the (single) NAL body
        assert_eq!(scan_nals(&frame).len(), 1);

        let recovered = unpack_carrier(&frame);
        assert!(
            recovered.starts_with(&tunnel),
            "recovered {} bytes",
            recovered.len()
        );
    }

    #[test]
    fn carrier_round_trips_keyframe() {
        let s = static_stream(640, 360);
        // Fake openh264 keyframe: real SPS/PPS + a large IDR slice.
        let mut frame = Vec::new();
        push_nal(&mut frame, &s.sps);
        push_nal(&mut frame, &s.pps);
        frame.extend_from_slice(&START_CODE);
        frame.push(0x65); // IDR slice
        frame.extend_from_slice(&[0x33; 1200]);
        let orig_len = frame.len();
        let sps_pps_prefix = frame[..8].to_vec();

        let cap = carrier_capacity(&frame);
        let tunnel: Vec<u8> = (0..cap as u32).map(|i| (i ^ 0x5A) as u8).collect();
        let stored = pack_carrier(&mut frame, &tunnel);

        assert_eq!(frame.len(), orig_len);
        assert_eq!(stored, tunnel.len());
        // SPS/PPS passed through untouched
        assert_eq!(&frame[..8], &sps_pps_prefix[..]);
        let nals = scan_nals(&frame);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[2].header_byte(&frame)), NAL_SLICE_IDR);

        let recovered = unpack_carrier(&frame);
        assert!(recovered.starts_with(&tunnel));
    }

    #[test]
    fn carrier_handles_zero_heavy_tunnel_payload() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&START_CODE);
        frame.push(0x41);
        frame.extend_from_slice(&[0x00; 600]);
        let orig_len = frame.len();

        let cap = carrier_capacity(&frame);
        let tunnel = vec![0u8; cap]; // pathological: all zeros
        let stored = pack_carrier(&mut frame, &tunnel);
        assert_eq!(frame.len(), orig_len);
        assert_eq!(
            scan_nals(&frame).len(),
            1,
            "no start codes from zero payload"
        );
        let recovered = unpack_carrier(&frame);
        assert_eq!(&recovered[..stored], &tunnel[..stored]);
    }
}
