use anyhow::{Context, Result, anyhow, bail};
use cxx::SharedPtr;
use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use std::fs::File;
use std::io::BufReader;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

use webrtc_sys::video_frame::ffi::{VideoRotation, new_video_frame_builder};
use webrtc_sys::video_frame_buffer::ffi::{
    I420Buffer, VideoFrameBuffer, i420_to_yuv8, new_i420_buffer, yuv_to_vfb, yuv8_to_yuv,
};
use webrtc_sys::video_track::ffi::{FrameMetadata, VideoTrackSource};

use super::raylib::{Canvas, Color};
use super::tunnel::TUNNEL_STATE;

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u32 = 1;
pub const AUDIO_FRAME_SAMPLES: usize = 960;
pub const VIDEO_WIDTH: u32 = 640;
pub const VIDEO_HEIGHT: u32 = 360;
pub const VIDEO_FPS: u64 = 30;

/// Exponential rate (1/s) for inter-fragment silence duration; mean silence = 1/rate seconds.
const SILENCE_RATE: f64 = 0.5;
/// Exponential rate at which a speaker yields the floor during overlap (mean ~500 ms).
const YIELD_RATE: f64 = 2.0;

const CN_INTERVAL_MIN: Duration = Duration::from_millis(160);
const CN_INTERVAL_MAX: Duration = Duration::from_millis(480);

/// How long after the last observed speech packet the remote is still considered active.
const REMOTE_VAD_WINDOW_MS: u64 = 300;

fn remote_is_speaking() -> bool {
    let Some(state) = TUNNEL_STATE.get() else {
        return false;
    };
    let last_ms = state.remote_vad_last_speech_ms.load(Ordering::Relaxed);
    if last_ms == 0 {
        return false;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    now_ms.saturating_sub(last_ms) < REMOTE_VAD_WINDOW_MS
}

// ---------------------------------------------------------------------------
// Playback state machine
// ---------------------------------------------------------------------------

enum PlaybackState {
    Silent {
        speak_at: Instant,
        cn_next: Instant,
        next_frag: usize,
    },
    Speaking {
        frag_idx: usize,
        end_frame: usize,
        /// Deadline to yield the floor when the remote is also active.
        /// None until the first frame where remote_is_speaking() is true.
        yield_at: Option<Instant>,
    },
}

// ---------------------------------------------------------------------------
// OpusMedia
// ---------------------------------------------------------------------------

pub struct OpusMedia {
    path: String,
    reader: ogg::PacketReader<BufReader<File>>,
    decoder: opus::Decoder,
    /// Absolute frame index the reader is currently positioned at.
    reader_frame: usize,
    /// Speech intervals as (start_frame, end_frame) inclusive frame indices.
    vad_map: Vec<(usize, usize)>,
    state: PlaybackState,
    rng: SmallRng,
}

impl OpusMedia {
    pub fn spawn(path: &str) -> Result<Self> {
        let vad_map = build_vad_map(path)?;
        if vad_map.is_empty() {
            bail!("no speech detected in {path}");
        }
        info!("{} speech fragments detected in {}", vad_map.len(), path);

        let file = File::open(path).with_context(|| format!("open {path}"))?;
        let mut reader = ogg::PacketReader::new(BufReader::new(file));
        reader
            .read_packet()?
            .ok_or_else(|| anyhow!("missing opus id header"))?;
        reader
            .read_packet()?
            .ok_or_else(|| anyhow!("missing opus comment header"))?;

        let decoder = opus::Decoder::new(AUDIO_SAMPLE_RATE, opus::Channels::Mono)?;
        let mut rng = SmallRng::seed_from_u64(rand::random());
        let now = Instant::now();

        Ok(Self {
            path: path.to_string(),
            reader,
            decoder,
            reader_frame: 0,
            vad_map,
            state: PlaybackState::Silent {
                speak_at: now + exp_duration(&mut rng, SILENCE_RATE),
                cn_next: now + cn_interval(&mut rng),
                next_frag: 0,
            },
            rng,
        })
    }

    pub fn next_audio_frame(&mut self) -> Result<Vec<i16>> {
        let now = Instant::now();

        // Silent → Speaking: Instant and usize are both Copy, so the pattern
        // binding ends the borrow before the body executes.
        if let PlaybackState::Silent {
            speak_at,
            next_frag,
            ..
        } = self.state
            && now >= speak_at
        {
            if remote_is_speaking() {
                // Yield the floor: postpone start by another silence interval.
                debug!("remote active, deferring speech start");
                if let PlaybackState::Silent {
                    ref mut speak_at, ..
                } = self.state
                {
                    *speak_at = now + exp_duration(&mut self.rng, SILENCE_RATE);
                }
            } else {
                let (start_frame, end_frame) = self.vad_map[next_frag];
                self.seek_to_frame(start_frame)?;
                self.decoder.reset_state()?;
                self.state = PlaybackState::Speaking {
                    frag_idx: next_frag,
                    end_frame,
                    yield_at: None,
                };
            }
        }

        // Copy out Speaking fields (all Copy) so the borrow on self.state ends
        // before we call read_audio_packet / decode_packet or mutate state.
        let speaking = match self.state {
            PlaybackState::Speaking {
                frag_idx,
                end_frame,
                yield_at,
            } => Some((frag_idx, end_frame, yield_at)),
            _ => None,
        };
        if let Some((frag_idx, end_frame, mut yield_at)) = speaking {
            let remote_active = remote_is_speaking();
            // Arm the yield timer the first time we observe the remote is active.
            if remote_active && yield_at.is_none() {
                let t = now + exp_duration(&mut self.rng, YIELD_RATE);
                if let PlaybackState::Speaking {
                    yield_at: ref mut ya,
                    ..
                } = self.state
                {
                    *ya = Some(t);
                }
                yield_at = Some(t);
            } else if !remote_active && yield_at.is_some() {
                // Remote went silent before the timer fired; disarm.
                if let PlaybackState::Speaking {
                    yield_at: ref mut ya,
                    ..
                } = self.state
                {
                    *ya = None;
                }
                yield_at = None;
            }

            // Yield the floor when the timer fires.
            if yield_at.is_some_and(|t| now >= t) {
                debug!("yielding floor mid-fragment");
                let next_frag = (frag_idx + 1) % self.vad_map.len();
                self.state = PlaybackState::Silent {
                    speak_at: now + exp_duration(&mut self.rng, SILENCE_RATE),
                    cn_next: now + cn_interval(&mut self.rng),
                    next_frag,
                };
                return Ok(vec![0i16; AUDIO_FRAME_SAMPLES]);
            }

            let packet = self.read_audio_packet()?;
            let samples = decode_packet(&mut self.decoder, &packet)?;
            if self.reader_frame > end_frame {
                let next_frag = (frag_idx + 1) % self.vad_map.len();
                let now = Instant::now();
                self.state = PlaybackState::Silent {
                    speak_at: now + exp_duration(&mut self.rng, SILENCE_RATE),
                    cn_next: now + cn_interval(&mut self.rng),
                    next_frag,
                };
            }
            return Ok(samples);
        }

        // Silent: emit CN or zeros.
        let emit_cn = match self.state {
            PlaybackState::Silent { cn_next, .. } => now >= cn_next,
            _ => unreachable!(),
        };
        if emit_cn {
            if let PlaybackState::Silent {
                ref mut cn_next, ..
            } = self.state
            {
                *cn_next = now + cn_interval(&mut self.rng);
            }
            return Ok(gen_cn_frame(&mut self.rng));
        }
        Ok(vec![0i16; AUDIO_FRAME_SAMPLES])
    }

    /// Position the reader at `target` frame, re-opening the file if we need to seek backward.
    fn seek_to_frame(&mut self, target: usize) -> Result<()> {
        if target < self.reader_frame {
            let file = File::open(&self.path).with_context(|| format!("reopen {}", self.path))?;
            self.reader = ogg::PacketReader::new(BufReader::new(file));
            self.reader
                .read_packet()?
                .ok_or_else(|| anyhow!("missing id header on reopen"))?;
            self.reader
                .read_packet()?
                .ok_or_else(|| anyhow!("missing comment header on reopen"))?;
            self.reader_frame = 0;
        }
        while self.reader_frame < target {
            self.reader
                .read_packet()?
                .ok_or_else(|| anyhow!("EOF while seeking to frame {target}"))?;
            self.reader_frame += 1;
        }
        Ok(())
    }

    fn read_audio_packet(&mut self) -> Result<Vec<u8>> {
        let packet = self
            .reader
            .read_packet()?
            .ok_or_else(|| anyhow!("unexpected EOF at frame {}", self.reader_frame))?;
        self.reader_frame += 1;
        Ok(packet.data)
    }
}

// ---------------------------------------------------------------------------
// VAD map — loaded from the OpusTags comment header (vadmap= field).
// Use vad.sh to embed the map before using a file with this binary.
// ---------------------------------------------------------------------------

fn build_vad_map(path: &str) -> Result<Vec<(usize, usize)>> {
    let file = File::open(path).with_context(|| format!("open {path}"))?;
    let mut reader = ogg::PacketReader::new(BufReader::new(file));
    reader
        .read_packet()?
        .ok_or_else(|| anyhow!("missing opus id header"))?;
    let comment = reader
        .read_packet()?
        .ok_or_else(|| anyhow!("missing opus comment header"))?;

    let data = &comment.data;
    if data.len() < 8 || &data[..8] != b"OpusTags" {
        bail!("invalid OpusTags magic in {path}");
    }
    let mut pos = 8usize;

    let vendor_len = read_u32_le(data, &mut pos)? as usize;
    pos = pos
        .checked_add(vendor_len)
        .ok_or_else(|| anyhow!("vendor string overflow"))?;
    if pos > data.len() {
        bail!("truncated OpusTags vendor string");
    }

    let comment_count = read_u32_le(data, &mut pos)?;
    for _ in 0..comment_count {
        let entry_len = read_u32_le(data, &mut pos)? as usize;
        let end = pos
            .checked_add(entry_len)
            .ok_or_else(|| anyhow!("comment length overflow"))?;
        if end > data.len() {
            bail!("truncated OpusTags comment entry");
        }
        let entry =
            std::str::from_utf8(&data[pos..end]).with_context(|| "non-UTF-8 comment entry")?;
        pos = end;

        if let Some(eq) = entry.find('=')
            && entry[..eq].eq_ignore_ascii_case("vadmap")
        {
            return parse_vadmap(&entry[eq + 1..], path);
        }
    }

    bail!("no vadmap tag in {path} — run vad.sh to embed one")
}

fn read_u32_le(data: &[u8], pos: &mut usize) -> Result<u32> {
    let end = pos
        .checked_add(4)
        .ok_or_else(|| anyhow!("u32 read overflow"))?;
    if end > data.len() {
        bail!("truncated OpusTags header");
    }
    let val = u32::from_le_bytes(data[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(val)
}

fn parse_vadmap(value: &str, path: &str) -> Result<Vec<(usize, usize)>> {
    let mut map = Vec::new();
    for pair in value.split(',') {
        let (s, e) = pair
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid vadmap entry {pair:?} in {path}"))?;
        let start: usize = s
            .trim()
            .parse()
            .with_context(|| format!("bad start in {pair:?}"))?;
        let end: usize = e
            .trim()
            .parse()
            .with_context(|| format!("bad end in {pair:?}"))?;
        map.push((start, end));
    }
    if map.is_empty() {
        bail!("vadmap in {path} is empty");
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Audio helpers
// ---------------------------------------------------------------------------

fn decode_packet(decoder: &mut opus::Decoder, data: &[u8]) -> Result<Vec<i16>> {
    let mut buf = vec![0i16; 5760]; // max Opus frame at 48 kHz (120 ms)
    let len = decoder.decode(data, &mut buf, false)?;
    buf.truncate(len);
    buf.resize(AUDIO_FRAME_SAMPLES, 0); // pad if shorter, truncate if longer
    Ok(buf)
}

fn gen_cn_frame(rng: &mut SmallRng) -> Vec<i16> {
    (0..AUDIO_FRAME_SAMPLES)
        .map(|_| rng.random::<i16>() >> 9) // ~−48 dBFS comfort noise
        .collect()
}

fn exp_duration(rng: &mut SmallRng, rate: f64) -> Duration {
    let u: f64 = rng.random::<f64>().max(f64::MIN_POSITIVE);
    Duration::from_secs_f64(-u.ln() / rate)
}

fn cn_interval(rng: &mut SmallRng) -> Duration {
    let u: f64 = rng.random::<f64>();
    let min = CN_INTERVAL_MIN.as_secs_f64();
    let max = CN_INTERVAL_MAX.as_secs_f64();
    Duration::from_secs_f64(min + u * (max - min))
}

// ---------------------------------------------------------------------------
// RaylibMedia
// ---------------------------------------------------------------------------

pub struct RaylibMedia {
    opus: OpusMedia,
    smoothed_rms: f32,
    pub frame_count: u64,
    pub width: u32,
    pub height: u32,
}

impl RaylibMedia {
    pub fn spawn(path: &str, width: u32, height: u32) -> Result<Self> {
        if !path.ends_with(".opus") {
            bail!("only .opus files are supported");
        }
        Ok(Self {
            opus: OpusMedia::spawn(path)?,
            smoothed_rms: 0.0,
            frame_count: 0,
            width,
            height,
        })
    }

    pub fn next_audio_frame(&mut self) -> Result<Vec<i16>> {
        let samples = self.opus.next_audio_frame()?;

        let sum_sq = samples
            .iter()
            .map(|&s| (s as f32 / 32768.0).powi(2))
            .sum::<f32>();
        let rms = (sum_sq / samples.len() as f32).sqrt();
        self.smoothed_rms = self.smoothed_rms * 0.8 + rms * 0.2;

        Ok(samples)
    }

    pub fn next_video_frame(&mut self) -> Result<Vec<u8>> {
        self.frame_count += 1;
        let w = self.width as f32;
        let h = self.height as f32;
        let cx = self.width as i32 / 2;
        let cy = self.height as i32 / 2;

        let mut canvas = Canvas::new(
            self.width as i32,
            self.height as i32,
            Color {
                r: 10,
                g: 10,
                b: 20,
                a: 255,
            },
        );

        let bg_pulse = (self.frame_count as f32 * 0.05).sin() * 0.5 + 0.5;
        let bg_color = Color {
            r: (20.0 + bg_pulse * 30.0) as u8,
            g: (20.0 + bg_pulse * 10.0) as u8,
            b: (40.0 + bg_pulse * 40.0) as u8,
            a: 255,
        };
        canvas.rect(0, 0, self.width as i32, self.height as i32, bg_color);

        canvas.rect_outline(
            20,
            20,
            (self.width - 40) as i32,
            (self.height - 40) as i32,
            2,
            Color {
                r: 100,
                g: 100,
                b: 100,
                a: 150,
            },
        );

        let base_radius = h * 80.0 / 360.0;
        let pulse_radius = base_radius + self.smoothed_rms * h * 400.0 / 360.0;

        let circle_color = Color {
            r: (100.0 + self.smoothed_rms * 155.0).min(255.0) as u8,
            g: (150.0 - self.smoothed_rms * 50.0).max(0.0) as u8,
            b: (200.0 + self.smoothed_rms * 55.0).min(255.0) as u8,
            a: 255,
        };

        canvas.circle(cx, cy, pulse_radius as i32, circle_color);
        canvas.circle_outline(
            cx,
            cy,
            (pulse_radius + 5.0) as i32,
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 200,
            },
        );

        let wave_y = (h - h / 6.0) as i32;
        let bar_spacing = (w * 50.0 / 640.0) as i32;
        let bar_width = (w * 30.0 / 640.0).max(2.0) as i32;
        let bar_x0 = (w * 60.0 / 640.0) as i32;
        for i in 0..10i32 {
            let bar_h = (self.smoothed_rms * h * 200.0 / 360.0 * (1.0 + (i as f32 * 0.2).sin()))
                .max(5.0) as i32;
            canvas.rect(
                bar_x0 + i * bar_spacing,
                wave_y - bar_h,
                bar_width,
                bar_h,
                Color {
                    r: 0,
                    g: 200,
                    b: 255,
                    a: 200,
                },
            );
        }

        let orbit_angle = self.frame_count as f32 * 0.05;
        let orbit_x = cx + (orbit_angle.cos() * w * 200.0 / 640.0) as i32;
        let orbit_y = cy + (orbit_angle.sin() * h * 80.0 / 360.0) as i32;
        let orbit_r = (h * 25.0 / 360.0).max(5.0) as i32;
        canvas.circle(
            orbit_x,
            orbit_y,
            orbit_r,
            Color {
                r: 200,
                g: 100,
                b: 100,
                a: 255,
            },
        );

        let pixels = canvas.rgba_pixels();
        let frame_bytes = (self.width * self.height * 3 / 2) as usize;
        let mut i420 = vec![0u8; frame_bytes];
        rgba_to_i420(
            &pixels,
            &mut i420,
            self.width as usize,
            self.height as usize,
        );

        Ok(i420)
    }
}

fn rgba_to_i420(rgba: &[Color], i420: &mut [u8], width: usize, height: usize) {
    let y_start = 0;
    let u_start = width * height;
    let v_start = u_start + (width * height) / 4;

    for y in 0..height {
        for x in 0..width {
            let color = rgba[y * width + x];
            let r = color.r as f32;
            let g = color.g as f32;
            let b = color.b as f32;

            let luma = 0.299 * r + 0.587 * g + 0.114 * b;
            let luma_u8 = luma.clamp(0.0, 255.0) as u8;
            i420[y_start + y * width + x] = luma_u8;

            if y % 2 == 0 && x % 2 == 0 {
                let u: f32 = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
                let v: f32 = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;

                let uv_idx = (y / 2) * (width / 2) + (x / 2);
                i420[u_start + uv_idx] = u.clamp(0.0, 255.0) as u8;
                i420[v_start + uv_idx] = v.clamp(0.0, 255.0) as u8;
            }
        }
    }
}

pub fn send_i420_video_frame(
    source: &SharedPtr<VideoTrackSource>,
    data: Option<&[u8]>,
    width: u32,
    height: u32,
) -> Result<()> {
    let mut buffer = new_i420_buffer(
        width as i32,
        height as i32,
        width as i32,
        (width / 2) as i32,
        (width / 2) as i32,
    );
    fill_i420_buffer(buffer.pin_mut(), data, width, height)?;

    let vfb: &VideoFrameBuffer = unsafe {
        &*yuv_to_vfb(yuv8_to_yuv(i420_to_yuv8(
            buffer
                .as_ref()
                .ok_or_else(|| anyhow!("I420 buffer allocation failed"))?,
        )))
    };
    let mut builder = new_video_frame_builder();
    builder.pin_mut().set_video_frame_buffer(vfb);
    builder
        .pin_mut()
        .set_rotation(VideoRotation::VideoRotation0);
    builder.pin_mut().set_timestamp_us(now_us());
    let frame = builder.pin_mut().build();
    let ok = source.on_captured_frame(
        &frame,
        &FrameMetadata {
            has_packet_trailer: false,
            user_timestamp: 0,
            frame_id: 0,
        },
    );
    if !ok {
        debug!("libwebrtc rejected captured video frame");
    }
    Ok(())
}

fn fill_i420_buffer(
    buffer: std::pin::Pin<&mut I420Buffer>,
    data: Option<&[u8]>,
    width: u32,
    height: u32,
) -> Result<()> {
    let y_len = (width * height) as usize;
    let uv_len = ((width / 2) * (height / 2)) as usize;
    let expected = y_len + uv_len * 2;
    let yuv8 = unsafe { i420_to_yuv8(buffer.as_ref().get_ref()) };
    let (py, pu, pv) = unsafe {
        (
            (*yuv8).data_y() as *mut u8,
            (*yuv8).data_u() as *mut u8,
            (*yuv8).data_v() as *mut u8,
        )
    };
    if let Some(data) = data {
        if data.len() != expected {
            bail!(
                "raw I420 frame has {} bytes, expected {expected}",
                data.len()
            );
        }
        let (y, rest) = data.split_at(y_len);
        let (u, v) = rest.split_at(uv_len);
        unsafe {
            std::ptr::copy_nonoverlapping(y.as_ptr(), py, y_len);
            std::ptr::copy_nonoverlapping(u.as_ptr(), pu, uv_len);
            std::ptr::copy_nonoverlapping(v.as_ptr(), pv, uv_len);
        }
        return Ok(());
    }
    unsafe {
        std::ptr::write_bytes(py, 0, y_len);
        std::ptr::write_bytes(pu, 128, uv_len);
        std::ptr::write_bytes(pv, 128, uv_len);
    }
    Ok(())
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}
