use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::media::{VIDEO_HEIGHT, VIDEO_WIDTH};
use super::noise::{
    NOISE_KEYS, NOISE_MSG_SIZE, noise_process_incoming, noise_reset_state, noise_try_inject,
};
use super::vp9::VP9_BLACK_KEYFRAMES;
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

// Re-export the noise public API so callers only need to import from this module.
pub use super::noise::{NoiseRole, init_noise_keys, noise_auth_failed};

/// Reset noise and tunnel auth state for a new call.
pub fn prepare_noise_for_call(role: NoiseRole) {
    noise_reset_state(role);
    if let Some(ts) = TUNNEL_STATE.get()
        && NOISE_KEYS.get().is_some()
    {
        ts.authenticated.store(false, Ordering::Relaxed);
    }
}

use anyhow::{Context, Result, anyhow, bail};
use bytes::{Buf, BufMut};
use tokio::sync::mpsc;
use tracing::{info, warn};

use tun_rs::AsyncDevice;

use webm::mux::{
    AudioCodecId, AudioTrack, Segment, SegmentBuilder, SegmentMode, VideoCodecId, VideoTrack,
    Writer,
};

pub const MAX_REASSEMBLED_PACKET_LEN: usize = 65_535;
pub const MAX_REASSEMBLY_PACKETS: usize = 256;
pub const FRAGMENT_REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(2);
pub const OUTBOUND_QUEUE_MAX_PACKETS: usize = 64;
pub const INBOUND_QUEUE_MAX_PACKETS: usize = 256;

pub const FRAME_KIND_PADDING: u8 = 0x00;
pub const FRAME_KIND_WHOLE: u8 = 0x01;
pub const FRAME_KIND_FRAGMENT: u8 = 0x02;
pub const FRAME_FLAG_KEYFRAME: u8 = 0x01;
/// whole: kind(1) | flags(1) | len(2) | sender_width(2) | sender_height(2)
pub const WHOLE_HEADER_LEN: usize = 8;
/// fragment: kind(1) | flags(1) | packet_id(4) | frag_idx(2) | frag_cnt(2) | frag_len(2) | sender_width(2) | sender_height(2)
pub const FRAGMENT_HEADER_LEN: usize = 16;
pub const VP9_SHOW_EXISTING_FRAME_SLOT_0: &[u8] = &[0x88];

pub static TUNNEL_STATE: OnceLock<Arc<TunnelState>> = OnceLock::new();

/// Number of one-second buckets kept for utilization history (covers 5 minutes).
pub const UTILIZATION_HISTORY_SECS: usize = 300;

pub struct UtilizationBucket {
    pub second: u64,
    pub data_frames: u32,
    pub total_frames: u32,
}

#[derive(Default)]
pub struct UtilizationHistory {
    pub buckets: VecDeque<UtilizationBucket>,
}

impl UtilizationHistory {
    pub fn record(&mut self, now_sec: u64, is_data: bool) {
        match self.buckets.back_mut() {
            Some(b) if b.second == now_sec => {
                b.total_frames += 1;
                if is_data {
                    b.data_frames += 1;
                }
            }
            _ => {
                self.buckets.push_back(UtilizationBucket {
                    second: now_sec,
                    data_frames: is_data as u32,
                    total_frames: 1,
                });
                if self.buckets.len() > UTILIZATION_HISTORY_SECS {
                    self.buckets.pop_front();
                }
            }
        }
    }

    /// Returns `(data_frames, total_frames)` for the most recent `window_secs` seconds.
    pub fn window(&self, window_secs: u64, now_sec: u64) -> (u64, u64) {
        let cutoff = now_sec.saturating_sub(window_secs);
        self.buckets
            .iter()
            .filter(|b| b.second > cutoff)
            .fold((0u64, 0u64), |(d, t), b| {
                (d + b.data_frames as u64, t + b.total_frames as u64)
            })
    }
}

pub struct TunnelState {
    pub outbound_packets: Mutex<VecDeque<Vec<u8>>>,
    /// Semaphore with one permit per slot in `outbound_packets`.
    /// The TUN reader acquires a permit before pushing (blocks when full);
    /// `next_tunnel_frame` releases a permit after each packet is consumed.
    pub outbound_semaphore: tokio::sync::Semaphore,
    pub outbound_frames: Mutex<VecDeque<Vec<u8>>>,
    pub inbound_packets: mpsc::Sender<Vec<u8>>,
    pub reassembly: Mutex<ReassemblyState>,
    pub webm_mux: Mutex<Option<WebmMuxer>>,
    pub next_packet_id: AtomicU32,
    pub vp9_replacement_needs_keyframe: AtomicBool,
    /// Unix-epoch milliseconds of the last received speech-sized Opus frame.
    /// Zero until the first speech frame is observed.
    pub remote_vad_last_speech_ms: AtomicU64,
    /// Sender's current encoder resolution, packed as `(w as u64) << 16 | h`.
    /// Updated by `hook_after_vp9_encode` on every outbound keyframe.
    /// Read by `next_tunnel_frame` to embed resolution in tunnel frame headers.
    pub encoder_resolution: AtomicU64,
    /// Last sender resolution seen on inbound tunnel frames, packed as `(w as u64) << 16 | h`.
    /// Updated by `hook_before_vp9_reference_find` when parsing inbound frames.
    /// Used to select the correct keyframe replacement for the decoder.
    pub remote_vp9_resolution: AtomicU64,
    /// Keyframe cache: one VP9 black keyframe per resolution.
    /// Pre-populated at startup with the 4 known VideoAdapter down-step resolutions.
    /// Updated on every outbound keyframe with the actual encoder output.
    pub vp9_keyframe_cache: Mutex<HashMap<(u16, u16), Vec<u8>>>,

    /// Set to true once the Noise KK handshake completes successfully.
    /// Always true when Noise authentication is not configured.
    pub authenticated: AtomicBool,

    pub hook_encode_calls: AtomicU64,
    pub hook_reference_calls: AtomicU64,
    pub tun_read_packets: AtomicU64,
    pub tun_read_bytes: AtomicU64,
    pub tun_write_packets: AtomicU64,
    pub tun_write_bytes: AtomicU64,
    pub tunnel_frames_sent: AtomicU64,
    pub tunnel_frame_bytes_sent: AtomicU64,
    pub tunnel_frames_received: AtomicU64,
    pub tunnel_frame_bytes_received: AtomicU64,
    pub tunnel_packets_reassembled: AtomicU64,
    pub tunnel_fragments_dropped: AtomicU64,
    pub utilization: Mutex<UtilizationHistory>,
}

impl TunnelState {
    fn new(
        inbound_tx: mpsc::Sender<Vec<u8>>,
        webm_mux: Option<WebmMuxer>,
        video_width: u32,
        video_height: u32,
    ) -> Self {
        Self {
            outbound_packets: Mutex::new(VecDeque::new()),
            outbound_semaphore: tokio::sync::Semaphore::new(OUTBOUND_QUEUE_MAX_PACKETS),
            outbound_frames: Mutex::new(VecDeque::new()),
            inbound_packets: inbound_tx,
            reassembly: Mutex::new(ReassemblyState::default()),
            webm_mux: Mutex::new(webm_mux),
            next_packet_id: AtomicU32::new(rand::random()),
            vp9_replacement_needs_keyframe: AtomicBool::new(true),
            authenticated: AtomicBool::new(NOISE_KEYS.get().is_none()),
            hook_encode_calls: AtomicU64::new(0),
            hook_reference_calls: AtomicU64::new(0),
            tun_read_packets: AtomicU64::new(0),
            tun_read_bytes: AtomicU64::new(0),
            tun_write_packets: AtomicU64::new(0),
            tun_write_bytes: AtomicU64::new(0),
            tunnel_frames_sent: AtomicU64::new(0),
            tunnel_frame_bytes_sent: AtomicU64::new(0),
            tunnel_frames_received: AtomicU64::new(0),
            tunnel_frame_bytes_received: AtomicU64::new(0),
            tunnel_packets_reassembled: AtomicU64::new(0),
            tunnel_fragments_dropped: AtomicU64::new(0),
            utilization: Mutex::new(UtilizationHistory::default()),
            remote_vad_last_speech_ms: AtomicU64::new(0),
            encoder_resolution: AtomicU64::new(pack_resolution(
                video_width as u16,
                video_height as u16,
            )),
            remote_vp9_resolution: AtomicU64::new(pack_resolution(
                video_width as u16,
                video_height as u16,
            )),
            vp9_keyframe_cache: Mutex::new(
                VP9_BLACK_KEYFRAMES
                    .iter()
                    .map(|kf| ((kf.width, kf.height), kf.data.to_vec()))
                    .collect(),
            ),
        }
    }
}

#[derive(Default)]
pub struct ReassemblyState {
    pub packets: HashMap<u32, PartialPacket>,
}

pub struct PartialPacket {
    pub fragment_count: u16,
    pub slots: Vec<Option<(usize, usize)>>,
    pub buf: Vec<u8>,
    pub received: usize,
    pub total_len: usize,
    pub updated_at: StdInstant,
}

pub struct TunBridge {
    pub _tun: Arc<AsyncDevice>,
    pub reader: tokio::task::JoinHandle<()>,
    pub writer: tokio::task::JoinHandle<()>,
}

impl Drop for TunBridge {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

pub async fn start_tun_bridge(
    tun: AsyncDevice,
    log_dir: Option<&std::path::Path>,
    prefix: &str,
    video_width: u32,
    video_height: u32,
) -> Result<TunBridge> {
    let tun = Arc::new(tun);
    let (inbound_tx, mut inbound_rx) = mpsc::channel(INBOUND_QUEUE_MAX_PACKETS);

    let webm_mux = log_dir.and_then(|dir| {
        WebmMuxer::open(
            &dir.join(format!("{}.webm", prefix)),
            video_width,
            video_height,
        )
        .ok()
    });

    let state = Arc::new(TunnelState::new(
        inbound_tx,
        webm_mux,
        video_width,
        video_height,
    ));

    TUNNEL_STATE
        .set(Arc::clone(&state))
        .map_err(|_| anyhow!("tunnel state already initialized"))?;

    let reader_tun = Arc::clone(&tun);
    let reader_state = Arc::clone(&state);
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_REASSEMBLED_PACKET_LEN];
        loop {
            match reader_tun.recv(&mut buf).await {
                Ok(n) => {
                    reader_state
                        .tun_read_packets
                        .fetch_add(1, Ordering::Relaxed);
                    reader_state
                        .tun_read_bytes
                        .fetch_add(n as u64, Ordering::Relaxed);
                    reader_state
                        .outbound_semaphore
                        .acquire()
                        .await
                        .unwrap()
                        .forget();
                    reader_state
                        .outbound_packets
                        .lock()
                        .unwrap()
                        .push_back(buf[..n].to_vec());
                }
                Err(err) => {
                    warn!("TUN reader exiting: {err}");
                    return;
                }
            }
        }
    });

    let writer_tun = Arc::clone(&tun);
    let writer_state = Arc::clone(&state);
    let writer = tokio::spawn(async move {
        while let Some(packet) = inbound_rx.recv().await {
            let packet_len = packet.len();
            if let Err(err) = writer_tun.send(&packet).await {
                warn!("TUN writer exiting: {err}");
                return;
            }
            writer_state
                .tun_write_packets
                .fetch_add(1, Ordering::Relaxed);
            writer_state
                .tun_write_bytes
                .fetch_add(packet_len as u64, Ordering::Relaxed);
        }
    });

    let tun_name = tun.name().unwrap_or_else(|_| "<unknown>".to_string());
    info!("TUN bridge initialized on {tun_name}");
    Ok(TunBridge {
        _tun: tun,
        reader,
        writer,
    })
}

pub fn next_tunnel_frame(
    carrier_len: usize,
    is_key_frame: bool,
    sender_res: (u16, u16),
) -> Option<Vec<u8>> {
    let state = TUNNEL_STATE.get()?;
    if carrier_len == 0 {
        return None;
    }

    let mut cached_frames = state.outbound_frames.try_lock().ok()?;

    // Build first frame from cache or fresh packet.
    let mut out: Vec<u8> =
        if let Some(frame) = pop_cached_frame_that_fits(&mut cached_frames, carrier_len) {
            frame
        } else {
            let packet = {
                let mut packets = state.outbound_packets.try_lock().ok()?;
                packets.pop_front()
            }?;

            if packet.len() <= carrier_len.saturating_sub(WHOLE_HEADER_LEN) {
                let frame = encode_whole_frame(&packet, is_key_frame, sender_res)?;
                state.outbound_semaphore.add_permits(1);
                frame
            } else if carrier_len > FRAGMENT_HEADER_LEN {
                let frames = match encode_fragment_frame(
                    &packet,
                    carrier_len,
                    state.next_packet_id.fetch_add(1, Ordering::Relaxed),
                    is_key_frame,
                    sender_res,
                ) {
                    Ok(frames) => frames,
                    Err(err) => {
                        warn!(
                            packet_len = packet.len(),
                            carrier_len, "failed to fragment TUN packet for carrier: {err:#}"
                        );
                        if let Ok(mut packets) = state.outbound_packets.try_lock() {
                            packets.push_front(packet);
                        }
                        return None;
                    }
                };
                state.outbound_semaphore.add_permits(1);
                let mut remaining_frames = VecDeque::from(frames);
                let first = remaining_frames.pop_front()?;
                cached_frames.extend(remaining_frames);
                first
            } else {
                if let Ok(mut packets) = state.outbound_packets.try_lock() {
                    packets.push_front(packet);
                }
                return None;
            }
        };

    // Greedily pack additional whole frames into remaining carrier space.
    loop {
        let remaining = carrier_len.saturating_sub(out.len());
        if remaining < WHOLE_HEADER_LEN + 1 {
            break;
        }
        let Ok(mut packets) = state.outbound_packets.try_lock() else {
            break;
        };
        let Some(next) = packets.front() else {
            break;
        };
        if next.len() > remaining - WHOLE_HEADER_LEN {
            break;
        }
        let packet = packets.pop_front().unwrap();
        drop(packets);
        if let Some(frame) = encode_whole_frame(&packet, is_key_frame, sender_res) {
            out.extend_from_slice(&frame);
        }
        state.outbound_semaphore.add_permits(1);
    }

    Some(out)
}

pub fn pop_cached_frame_that_fits(
    cached_frames: &mut VecDeque<Vec<u8>>,
    carrier_len: usize,
) -> Option<Vec<u8>> {
    for idx in 0..cached_frames.len() {
        if cached_frames[idx].len() <= carrier_len {
            return cached_frames.remove(idx);
        }
    }
    None
}

/// Returns `Some((is_key_frame, sender_width, sender_height))` from the last data frame parsed.
pub fn handle_inbound_tunnel_frame(carrier: &[u8]) -> Option<(bool, u16, u16)> {
    let state = TUNNEL_STATE.get()?;
    let mut last_data: Option<(bool, u16, u16)> = None;
    let mut offset = 0usize;

    loop {
        if offset >= carrier.len() {
            break;
        }
        match parse_tunnel_frame(&carrier[offset..]) {
            None | Some(TunnelFrame::Padding) => break,
            Some(TunnelFrame::Whole {
                is_key_frame,
                sender_width,
                sender_height,
                packet,
            }) => {
                state
                    .tunnel_packets_reassembled
                    .fetch_add(1, Ordering::Relaxed);
                let packet_len = packet.len();
                let _ = state.inbound_packets.try_send(packet.to_vec());
                last_data = Some((is_key_frame, sender_width, sender_height));
                offset += WHOLE_HEADER_LEN + packet_len;
            }
            Some(TunnelFrame::Fragment {
                is_key_frame,
                sender_width,
                sender_height,
                packet_id,
                fragment_index,
                fragment_count,
                data,
            }) => {
                let data_len = data.len();
                let (reassembled, expired) = {
                    let Ok(mut reassembly) = state.reassembly.try_lock() else {
                        last_data = Some((is_key_frame, sender_width, sender_height));
                        offset += FRAGMENT_HEADER_LEN + data_len;
                        continue;
                    };
                    reassembly.insert_fragment(packet_id, fragment_index, fragment_count, data)
                };
                if expired > 0 {
                    state
                        .tunnel_fragments_dropped
                        .fetch_add(expired, Ordering::Relaxed);
                }
                if let Some(packet) = reassembled {
                    state
                        .tunnel_packets_reassembled
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = state.inbound_packets.try_send(packet);
                }
                last_data = Some((is_key_frame, sender_width, sender_height));
                offset += FRAGMENT_HEADER_LEN + data_len;
            }
        }
    }

    last_data
}

pub enum TunnelFrame<'a> {
    Padding,
    Whole {
        is_key_frame: bool,
        sender_width: u16,
        sender_height: u16,
        packet: &'a [u8],
    },
    Fragment {
        is_key_frame: bool,
        sender_width: u16,
        sender_height: u16,
        packet_id: u32,
        fragment_index: u16,
        fragment_count: u16,
        data: &'a [u8],
    },
}

pub fn frame_flags(is_key_frame: bool) -> u8 {
    if is_key_frame { FRAME_FLAG_KEYFRAME } else { 0 }
}

pub fn encode_whole_frame(
    packet: &[u8],
    is_key_frame: bool,
    (sender_w, sender_h): (u16, u16),
) -> Option<Vec<u8>> {
    let len = u16::try_from(packet.len()).ok()?;
    let mut frame = Vec::with_capacity(WHOLE_HEADER_LEN + packet.len());
    frame.put_u8(FRAME_KIND_WHOLE);
    frame.put_u8(frame_flags(is_key_frame));
    frame.put_u16(len);
    frame.put_u16(sender_w);
    frame.put_u16(sender_h);
    frame.put_slice(packet);
    Some(frame)
}

pub fn encode_fragment_frame(
    packet: &[u8],
    carrier_len: usize,
    packet_id: u32,
    is_key_frame: bool,
    (sender_w, sender_h): (u16, u16),
) -> Result<Vec<Vec<u8>>> {
    let fragment_payload_len = carrier_len.saturating_sub(FRAGMENT_HEADER_LEN);
    if fragment_payload_len == 0 {
        bail!("carrier frame too small for tunnel fragment header");
    }

    let fragment_count = packet.len().div_ceil(fragment_payload_len);
    if fragment_count > MAX_REASSEMBLY_PACKETS {
        bail!("packet requires too many fragments: {fragment_count}");
    }
    let fragment_count = u16::try_from(fragment_count).context("fragment count exceeds u16")?;

    let mut frames = Vec::with_capacity(usize::from(fragment_count));
    for (idx, chunk) in packet.chunks(fragment_payload_len).enumerate() {
        let fragment_index = u16::try_from(idx).context("fragment index exceeds u16")?;
        let fragment_len = u16::try_from(chunk.len()).context("fragment len exceeds u16")?;
        let mut frame = Vec::with_capacity(FRAGMENT_HEADER_LEN + chunk.len());
        frame.put_u8(FRAME_KIND_FRAGMENT);
        frame.put_u8(frame_flags(is_key_frame));
        frame.put_u32(packet_id);
        frame.put_u16(fragment_index);
        frame.put_u16(fragment_count);
        frame.put_u16(fragment_len);
        frame.put_u16(sender_w);
        frame.put_u16(sender_h);
        frame.put_slice(chunk);
        frames.push(frame);
    }
    Ok(frames)
}

pub fn parse_tunnel_frame(frame: &[u8]) -> Option<TunnelFrame<'_>> {
    let mut buf = frame;
    if !buf.has_remaining() {
        return None;
    }
    match buf.get_u8() {
        FRAME_KIND_PADDING => Some(TunnelFrame::Padding),
        FRAME_KIND_WHOLE => {
            // kind(1) | flags(1) | len(2) | sender_width(2) | sender_height(2)
            if buf.remaining() < WHOLE_HEADER_LEN - 1 {
                return None;
            }
            let flags = buf.get_u8();
            let len = usize::from(buf.get_u16());
            let sender_width = buf.get_u16();
            let sender_height = buf.get_u16();
            if buf.remaining() < len {
                return None;
            }
            Some(TunnelFrame::Whole {
                is_key_frame: flags & FRAME_FLAG_KEYFRAME != 0,
                sender_width,
                sender_height,
                packet: &buf[..len],
            })
        }
        FRAME_KIND_FRAGMENT => {
            // kind(1) | flags(1) | packet_id(4) | frag_idx(2) | frag_cnt(2) | frag_len(2) | sender_width(2) | sender_height(2)
            if buf.remaining() < FRAGMENT_HEADER_LEN - 1 {
                return None;
            }
            let flags = buf.get_u8();
            let packet_id = buf.get_u32();
            let fragment_index = buf.get_u16();
            let fragment_count = buf.get_u16();
            let len = usize::from(buf.get_u16());
            let sender_width = buf.get_u16();
            let sender_height = buf.get_u16();
            if fragment_count == 0 || fragment_index >= fragment_count {
                return None;
            }
            if buf.remaining() < len {
                return None;
            }
            Some(TunnelFrame::Fragment {
                is_key_frame: flags & FRAME_FLAG_KEYFRAME != 0,
                sender_width,
                sender_height,
                packet_id,
                fragment_index,
                fragment_count,
                data: &buf[..len],
            })
        }
        _ => None,
    }
}

impl ReassemblyState {
    /// Returns `(reassembled_packet, expired_packet_count)`.
    pub fn insert_fragment(
        &mut self,
        packet_id: u32,
        fragment_index: u16,
        fragment_count: u16,
        fragment_data: &[u8],
    ) -> (Option<Vec<u8>>, u64) {
        let expired = self.cleanup_expired();

        if fragment_count == 0
            || fragment_index >= fragment_count
            || usize::from(fragment_count) > MAX_REASSEMBLY_PACKETS
        {
            return (None, expired);
        }
        if !self.packets.contains_key(&packet_id) && self.packets.len() >= MAX_REASSEMBLY_PACKETS {
            self.drop_oldest();
        }

        let packet = self
            .packets
            .entry(packet_id)
            .or_insert_with(|| PartialPacket::new(fragment_count));
        if packet.fragment_count != fragment_count {
            return (None, expired);
        }
        if packet
            .store_fragment(fragment_index, fragment_data)
            .is_err()
        {
            self.packets.remove(&packet_id);
            return (None, expired);
        }
        if packet.received == usize::from(packet.fragment_count) {
            let reassembled = self.packets.remove(&packet_id).and_then(|p| p.reassemble());
            return (reassembled, expired);
        }
        (None, expired)
    }

    fn cleanup_expired(&mut self) -> u64 {
        let now = StdInstant::now();
        let before = self.packets.len();
        self.packets.retain(|_, packet| {
            now.duration_since(packet.updated_at) <= FRAGMENT_REASSEMBLY_TIMEOUT
        });
        (before - self.packets.len()) as u64
    }

    fn drop_oldest(&mut self) {
        if let Some(packet_id) = self
            .packets
            .iter()
            .min_by_key(|(_, packet)| packet.updated_at)
            .map(|(packet_id, _)| *packet_id)
        {
            self.packets.remove(&packet_id);
        }
    }
}

impl PartialPacket {
    pub fn new(fragment_count: u16) -> Self {
        Self {
            fragment_count,
            slots: vec![None; usize::from(fragment_count)],
            buf: Vec::new(),
            received: 0,
            total_len: 0,
            updated_at: StdInstant::now(),
        }
    }

    pub fn store_fragment(&mut self, fragment_index: u16, fragment_data: &[u8]) -> Result<()> {
        let Some(slot) = self.slots.get_mut(usize::from(fragment_index)) else {
            bail!("fragment index out of bounds");
        };
        self.updated_at = StdInstant::now();
        if slot.is_some() {
            return Ok(());
        }
        self.total_len = self
            .total_len
            .checked_add(fragment_data.len())
            .ok_or_else(|| anyhow!("reassembled packet length overflow"))?;
        if self.total_len > MAX_REASSEMBLED_PACKET_LEN {
            bail!("reassembled packet exceeds max length");
        }
        let start = self.buf.len();
        self.buf.extend_from_slice(fragment_data);
        *slot = Some((start, fragment_data.len()));
        self.received += 1;
        Ok(())
    }

    pub fn reassemble(self) -> Option<Vec<u8>> {
        let mut expected = 0usize;
        let in_order = self.slots.iter().all(|slot| match slot {
            &Some((start, len)) if start == expected => {
                expected += len;
                true
            }
            _ => false,
        });
        if in_order {
            return Some(self.buf);
        }

        let mut packet = Vec::with_capacity(self.total_len);
        for slot in self.slots {
            let (start, len) = slot?;
            packet.extend_from_slice(&self.buf[start..start + len]);
        }
        Some(packet)
    }
}

pub type Vp9EncodeHook = unsafe extern "C" fn(*mut u8, usize, u32, bool) -> bool;
pub type Vp9ReferenceHook = unsafe extern "C" fn(*mut u8, usize, u32, bool, *mut bool) -> usize;

#[used]
pub static KEEP_VP9_HOOKS: (Vp9EncodeHook, Vp9ReferenceHook) =
    (hook_after_vp9_encode, hook_before_vp9_reference_find);

/// # Safety
///
/// Called from patched libwebrtc VP9 encoder code. `payload` must point to a
/// readable and writable buffer of exactly `len` bytes; the hook modifies it
/// in place and always produces output of the same length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hook_after_vp9_encode(
    payload: *mut u8,
    len: usize,
    _rtp_timestamp: u32,
    is_key_frame: bool,
) -> bool {
    if payload.is_null() || len == 0 {
        return false;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(payload, len) };

    if let Some(state) = TUNNEL_STATE.get() {
        state.hook_encode_calls.fetch_add(1, Ordering::Relaxed);

        // Read the original frame before overwriting, for webm mux and keyframe cache.
        let orig = buf.to_vec();
        if let Ok(mut mux) = state.webm_mux.lock()
            && let Some(ref mut webm) = *mux
        {
            webm.write_video_frame(&orig, is_key_frame);
        }
        if is_key_frame && let Some((w, h)) = vp9_keyframe_dimensions(&orig) {
            let prev = unpack_resolution(
                state
                    .encoder_resolution
                    .swap(pack_resolution(w, h), Ordering::Relaxed),
            );
            if prev != (w, h) {
                info!("encoder resolution changed: {w}×{h}");
            }
            if let Ok(mut cache) = state.vp9_keyframe_cache.lock() {
                cache.insert((w, h), orig);
            }
        }

        buf.fill(FRAME_KIND_PADDING);
        let sender_res = unpack_resolution(state.encoder_resolution.load(Ordering::Relaxed));
        let is_data = if let Some(frame) = next_tunnel_frame(len, is_key_frame, sender_res)
            && frame.len() <= len
        {
            buf[..frame.len()].copy_from_slice(&frame);
            state.tunnel_frames_sent.fetch_add(1, Ordering::Relaxed);
            state
                .tunnel_frame_bytes_sent
                .fetch_add(len as u64, Ordering::Relaxed);
            true
        } else {
            false
        };

        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Ok(mut u) = state.utilization.try_lock() {
            u.record(now_sec, is_data);
        }
    }
    true
}

/// # Safety
///
/// Called from patched libwebrtc after VP9 RTP payloads are depacketized and
/// decrypted, but before the VP9 reference finder sees the frame. `payload`
/// must point to a readable and writable buffer of exactly `len` bytes; the
/// hook overwrites it in place with a replacement frame and returns the new
/// length. Returns 0 if no replacement was written. `output_is_key_frame`
/// may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hook_before_vp9_reference_find(
    payload: *mut u8,
    len: usize,
    _rtp_timestamp: u32,
    is_key_frame: bool,
    output_is_key_frame: *mut bool,
) -> usize {
    if !output_is_key_frame.is_null() {
        unsafe {
            *output_is_key_frame = is_key_frame;
        }
    }
    let Some(state) = TUNNEL_STATE.get() else {
        return 0;
    };
    state.hook_reference_calls.fetch_add(1, Ordering::Relaxed);

    if payload.is_null() || len == 0 {
        return 0;
    }

    let mut carrier_is_key_frame = None;
    {
        let frame = unsafe { std::slice::from_raw_parts(payload, len) };
        if let Some((kf, w, h)) = handle_inbound_tunnel_frame(frame) {
            carrier_is_key_frame = Some(kf);
            state.tunnel_frames_received.fetch_add(1, Ordering::Relaxed);
            state
                .tunnel_frame_bytes_received
                .fetch_add(len as u64, Ordering::Relaxed);
            if w > 0 && h > 0 {
                state
                    .remote_vp9_resolution
                    .store(pack_resolution(w, h), Ordering::Relaxed);
            }
        }
    } // immutable borrow of payload ends here

    let replacement_is_key_frame = state.vp9_replacement_needs_keyframe.load(Ordering::Relaxed)
        || carrier_is_key_frame.unwrap_or(is_key_frame);

    // Select replacement VP9 frame: correct-resolution black keyframe or show-existing-frame.
    let keyframe_buf: Vec<u8>;
    let replacement: &[u8] = if replacement_is_key_frame {
        let (w, h) = unpack_resolution(state.remote_vp9_resolution.load(Ordering::Relaxed));
        keyframe_buf = state
            .vp9_keyframe_cache
            .lock()
            .ok()
            .and_then(|cache| {
                cache
                    .get(&(w, h))
                    .or_else(|| cache.get(&(VIDEO_WIDTH as u16, VIDEO_HEIGHT as u16)))
                    .cloned()
            })
            .unwrap_or_else(|| {
                VP9_BLACK_KEYFRAMES
                    .iter()
                    .find(|kf| kf.width == VIDEO_WIDTH as u16 && kf.height == VIDEO_HEIGHT as u16)
                    .map(|kf| kf.data.to_vec())
                    .unwrap_or_default()
            });
        &keyframe_buf
    } else {
        VP9_SHOW_EXISTING_FRAME_SLOT_0
    };

    if !output_is_key_frame.is_null() {
        unsafe {
            *output_is_key_frame = replacement_is_key_frame;
        }
    }

    if replacement.is_empty() || replacement.len() > len {
        return 0;
    }

    let out = unsafe { std::slice::from_raw_parts_mut(payload, replacement.len()) };
    out.copy_from_slice(replacement);
    state
        .vp9_replacement_needs_keyframe
        .store(false, Ordering::Relaxed);
    replacement.len()
}

pub fn keep_vp9_hooks_linked() {
    let _ = &KEEP_VP9_HOOKS;
}

// ---------------------------------------------------------------------------
// Opus audio hooks
// ---------------------------------------------------------------------------

pub type OpusEncodeHook = unsafe extern "C" fn(*mut u8, usize, u32) -> bool;
pub type OpusDecodeHook = unsafe extern "C" fn(*mut u8, usize, u32) -> usize;

#[used]
pub static KEEP_OPUS_HOOKS: (OpusEncodeHook, OpusDecodeHook) =
    (hook_after_opus_encode, hook_before_opus_decode);

fn after_opus_encode(payload: &mut [u8]) -> bool {
    if noise_try_inject(payload) {
        return true;
    }
    // WebM logging (side effect only; no payload change).
    if let Some(state) = TUNNEL_STATE.get()
        && let Ok(mut mux) = state.webm_mux.lock()
        && let Some(ref mut webm) = *mux
    {
        webm.write_audio_packet(payload);
    }
    false
}

fn before_opus_decode(payload: &mut [u8]) -> usize {
    let Some(tunnel) = TUNNEL_STATE.get() else {
        return 0;
    };
    let authenticated = tunnel.authenticated.load(Ordering::Relaxed);
    let Some((new_size, just_authenticated)) = noise_process_incoming(payload, authenticated)
    else {
        return 0;
    };
    if just_authenticated {
        tunnel.authenticated.store(true, Ordering::Relaxed);
    }
    new_size
}

/// # Safety
///
/// Called from patched libwebrtc after Opus encoding, before RTP packetization.
/// Modifies `payload` in place; returns true if the payload was changed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hook_after_opus_encode(
    payload: *mut u8,
    len: usize,
    _rtp_timestamp: u32,
) -> bool {
    if payload.is_null() || len == 0 {
        return false;
    }
    after_opus_encode(unsafe { std::slice::from_raw_parts_mut(payload, len) })
}

/// # Safety
///
/// Called from patched libwebrtc before inserting a received Opus payload into NetEQ.
/// Modifies `payload` in place; returns the new size, or 0 if unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hook_before_opus_decode(
    payload: *mut u8,
    len: usize,
    _rtp_timestamp: u32,
) -> usize {
    if payload.is_null() || len < NOISE_MSG_SIZE {
        return 0;
    }
    before_opus_decode(unsafe { std::slice::from_raw_parts_mut(payload, len) })
}

pub fn keep_opus_hooks_linked() {
    let _ = &KEEP_OPUS_HOOKS;
}

pub fn pack_resolution(w: u16, h: u16) -> u64 {
    (w as u64) << 16 | h as u64
}

pub fn unpack_resolution(v: u64) -> (u16, u16) {
    ((v >> 16) as u16, v as u16)
}

/// Parses width and height from a VP9 keyframe bitstream (profile 0–3).
/// Returns `None` if `data` is not a valid VP9 keyframe.
pub fn vp9_keyframe_dimensions(data: &[u8]) -> Option<(u16, u16)> {
    let b0 = *data.first()?;
    // byte 0: frame_marker(2) | profile_low(1) | profile_high(1) |
    //         show_existing_frame(1) | frame_type(1) | show_frame(1) | error_resilient(1)
    if b0 >> 6 != 2 || (b0 >> 3) & 1 != 0 || (b0 >> 2) & 1 != 0 {
        return None; // frame_marker, show_existing_frame, or frame_type mismatch
    }
    let profile = (((b0 >> 4) & 1) << 1) | ((b0 >> 5) & 1);

    // Bytes 1–3: sync code
    if data.get(1..4) != Some(&[0x49, 0x83, 0x42]) {
        return None;
    }

    // Bit 32 (byte 4): color_config
    let mut pos = 32usize;
    if profile == 1 || profile == 2 {
        pos += 1; // ten_or_twelve_bit
    }
    let color_space = read_stream_bits(data, pos, 3)?;
    pos += 4; // color_space(3) + color_range(1)
    if profile == 1 || profile == 3 {
        pos += if color_space != 7 { 3 } else { 2 }; // subsampling bits
    }

    // frame_width_minus_1 (16 bits), frame_height_minus_1 (16 bits)
    let w = read_stream_bits(data, pos, 16)? as u16 + 1;
    let h = read_stream_bits(data, pos + 16, 16)? as u16 + 1;
    Some((w, h))
}

/// Reads `n` bits (up to 24) from the VP9 bitstream at bit offset `pos`, MSB-first.
fn read_stream_bits(data: &[u8], pos: usize, n: usize) -> Option<u32> {
    debug_assert!(n <= 24);
    let byte = pos / 8;
    let shift = pos % 8;
    let bytes_needed = (shift + n).div_ceil(8);
    let mut val = 0u32;
    for i in 0..bytes_needed {
        val = (val << 8) | u32::from(*data.get(byte + i)?);
    }
    let total = bytes_needed * 8;
    Some((val >> (total - shift - n)) & ((1u32 << n) - 1))
}

/// Debug dumping
pub struct WebmMuxer {
    segment: Segment<std::fs::File>,
    video_track: VideoTrack,
    audio_track: AudioTrack,
    start_time: StdInstant,
    max_ts_ns: u64,
}

impl WebmMuxer {
    fn open(path: &std::path::Path, video_width: u32, video_height: u32) -> Result<Self> {
        let file = std::fs::File::create(path)
            .with_context(|| format!("create webm dump {}", path.display()))?;
        let writer = Writer::new(file);
        let builder = SegmentBuilder::new(writer)?;
        let builder = builder.set_mode(SegmentMode::File)?;

        let (builder, video_track) =
            builder.add_video_track(video_width, video_height, VideoCodecId::VP9, None)?;
        let (builder, audio_track) = builder.add_audio_track(48000, 1, AudioCodecId::Opus, None)?;

        let opus_head = [
            0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, // "OpusHead"
            0x01, // Version
            0x01, // Channels (1)
            0x38, 0x01, // Pre-skip (312)
            0x80, 0xbb, 0x00, 0x00, // Original Sample Rate (48000)
            0x00, 0x00, // Output Gain (0)
            0x00, // Mapping Family (0)
        ];
        let builder = builder.set_codec_private(audio_track, &opus_head)?;

        let segment = builder.build();
        info!("logging session to {}", path.display());
        Ok(Self {
            segment,
            video_track,
            audio_track,
            start_time: StdInstant::now(),
            max_ts_ns: 0,
        })
    }

    fn write_video_frame(&mut self, data: &[u8], is_key_frame: bool) {
        let ts_ns = StdInstant::now().duration_since(self.start_time).as_nanos() as u64;
        self.max_ts_ns = self.max_ts_ns.max(ts_ns);
        let _ = self
            .segment
            .add_frame(self.video_track, data, ts_ns, is_key_frame);
    }

    fn write_audio_packet(&mut self, data: &[u8]) {
        let ts_ns = StdInstant::now().duration_since(self.start_time).as_nanos() as u64;
        self.max_ts_ns = self.max_ts_ns.max(ts_ns);
        let _ = self.segment.add_frame(self.audio_track, data, ts_ns, false);
    }

    fn finalize(self) -> Result<()> {
        let duration_ms = self.max_ts_ns / 1_000_000;
        self.segment
            .finalize(Some(duration_ms))
            .map(|_| ())
            .map_err(|_| anyhow!("failed to finalize webm segment"))
    }
}

pub fn finalize_webm() {
    if let Some(state) = TUNNEL_STATE.get()
        && let Ok(mut mux) = state.webm_mux.lock()
        && let Some(webm) = mux.take()
    {
        let duration_ms = webm.max_ts_ns / 1_000_000;
        info!("finalizing WebM with duration {} ms", duration_ms);
        if let Err(err) = webm.finalize() {
            warn!("failed to finalize WebM: {err:#}");
        } else {
            info!("WebM session finalized");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_frame_with_padding() {
        let payload = b"hello";
        let mut frame =
            encode_whole_frame(payload, true, (VIDEO_WIDTH as u16, VIDEO_HEIGHT as u16))
                .expect("whole frame");
        frame.resize(64, 0);

        match parse_tunnel_frame(&frame).expect("parse frame") {
            TunnelFrame::Whole {
                is_key_frame,
                sender_width,
                sender_height,
                packet,
            } => {
                assert!(is_key_frame);
                assert_eq!(sender_width, VIDEO_WIDTH as u16);
                assert_eq!(sender_height, VIDEO_HEIGHT as u16);
                assert_eq!(packet, payload);
            }
            _ => panic!("expected whole frame"),
        }
    }

    #[test]
    fn fragments_and_reassembles_with_padding() {
        let payload = (0..512).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let frames =
            encode_fragment_frame(&payload, 80, 7, false, (480, 270)).expect("fragment packet");
        assert!(frames.len() > 1);

        let mut reassembly = ReassemblyState::default();
        let mut result = None;
        for mut frame in frames.into_iter().rev() {
            frame.resize(80, 0);
            match parse_tunnel_frame(&frame).expect("parse fragment") {
                TunnelFrame::Fragment {
                    is_key_frame,
                    sender_width,
                    sender_height,
                    packet_id,
                    fragment_index,
                    fragment_count,
                    data,
                } => {
                    assert!(!is_key_frame);
                    assert_eq!(sender_width, 480);
                    assert_eq!(sender_height, 270);
                    (result, _) =
                        reassembly.insert_fragment(packet_id, fragment_index, fragment_count, data);
                }
                _ => panic!("expected fragment"),
            }
        }

        assert_eq!(result.expect("reassembled packet"), payload);
    }

    #[test]
    fn padding_frame_is_noop() {
        assert!(matches!(
            parse_tunnel_frame(&[0; 32]),
            Some(TunnelFrame::Padding)
        ));
    }

    #[test]
    fn vp9_keyframe_dimensions_parses_all_resolutions() {
        use crate::protozoa::vp9::VP9_BLACK_KEYFRAMES;
        for kf in VP9_BLACK_KEYFRAMES {
            assert_eq!(
                vp9_keyframe_dimensions(kf.data),
                Some((kf.width, kf.height)),
                "{}x{}",
                kf.width,
                kf.height
            );
        }
    }

    #[test]
    fn multi_frame_pack_and_parse() {
        let p1 = b"first-packet";
        let p2 = b"second-packet";
        let res = (640u16, 360u16);
        let frame1 = encode_whole_frame(p1, false, res).unwrap();
        let frame2 = encode_whole_frame(p2, true, res).unwrap();

        // Carrier: two frames back-to-back followed by implicit padding zeros.
        let mut carrier = frame1.clone();
        carrier.extend_from_slice(&frame2);
        carrier.resize(256, 0);

        // Parse first frame.
        let mut offset = 0usize;
        match parse_tunnel_frame(&carrier[offset..]).unwrap() {
            TunnelFrame::Whole {
                is_key_frame,
                packet,
                ..
            } => {
                assert!(!is_key_frame);
                assert_eq!(packet, p1);
                offset += WHOLE_HEADER_LEN + packet.len();
            }
            _ => panic!("expected whole frame 1"),
        }

        // Parse second frame.
        match parse_tunnel_frame(&carrier[offset..]).unwrap() {
            TunnelFrame::Whole {
                is_key_frame,
                packet,
                ..
            } => {
                assert!(is_key_frame);
                assert_eq!(packet, p2);
                offset += WHOLE_HEADER_LEN + packet.len();
            }
            _ => panic!("expected whole frame 2"),
        }

        // Remaining bytes are padding.
        assert!(matches!(
            parse_tunnel_frame(&carrier[offset..]),
            Some(TunnelFrame::Padding)
        ));
    }

    #[test]
    fn cached_frame_pop_skips_oversized_front() {
        let mut cached = VecDeque::from(vec![vec![1; 128], vec![2; 32], vec![3; 96]]);

        assert_eq!(
            pop_cached_frame_that_fits(&mut cached, 64).expect("fitting frame"),
            vec![2; 32]
        );
        assert_eq!(cached, VecDeque::from(vec![vec![1; 128], vec![3; 96]]));
    }
}
