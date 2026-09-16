//! The tunnel/call engine, shared by the desktop CLI (`main.rs`) and the
//! Android JNI entry point (`android.rs`). Platform-specific setup (opening
//! the TUN device, binding a control socket, dropping privileges, ...)
//! belongs to the caller; this module only knows how to drive calls once a
//! TUN device and a command channel exist.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use anazoa_auth::TurnServer;
use anazoa_auth::oneme::{IncomingCall, SessionClient};
use anazoa_auth::signaling::SignalingClient;
use anazoa_config::{FingerprintConfig, ServiceEndpoints};

use crate::daemon::DaemonCmd;
use crate::protozoa::media::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE, RaylibMedia, VIDEO_FPS,
    send_i420_video_frame,
};
use crate::protozoa::tunnel::{
    NoiseRole, OUTBOUND_QUEUE_MAX_PACKETS, finalize_webm, init_noise_keys, noise_auth_failed,
    prepare_noise_for_call, start_tun_bridge, tunnel_state, unpack_resolution,
};
use crate::protozoa::vp9::{VP9_BLACK_KEYFRAMES, parse_resolution};
use crate::protozoa::webrtc::{
    LocalEvent, Role, WebrtcCall, add_ice_candidate, attach_existing_remote_tracks, create_answer,
    create_offer, ensure_local_senders, ice_state_name, log_local_senders, pc_state_name,
    set_local_description, set_remote_sdp,
};
use crate::{Config, call_keepalive_loop, shutdown_requested, shutdown_signal, trigger_shutdown};
use webrtc_sys::jsep::ffi::SdpType;
use webrtc_sys::peer_connection::ffi::PeerConnectionState;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(60);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
// NOISE_CHECK_FRAMES covers ~4 s at 50 fps; 10 s leaves headroom for slow ICE/DTLS
// before audio begins. noise_auth_failed() via custom_tick is the normal failure path.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Parses `noise-privkey`/`noise-peer-pubkey` (if both set) and registers them
/// for Noise KK authentication. A no-op if neither is set.
pub fn init_noise_from_config(cfg: &Config) -> Result<()> {
    match (
        cfg.noise_privkey.as_deref().filter(|s| !s.is_empty()),
        cfg.noise_peer_pubkey.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(priv_b64), Some(pub_b64)) => {
            let privkey = parse_noise_key(priv_b64)?.to_vec();
            let peer_pubkey = parse_noise_key(pub_b64)?.to_vec();
            init_noise_keys(privkey, peer_pubkey);
            info!("Noise KK authentication enabled");
            Ok(())
        }
        (None, None) => Ok(()),
        _ => bail!("noise-privkey and noise-peer-pubkey must both be set or both omitted"),
    }
}

/// Resolves `media-video-resolution` to a (width, height) pair, or a
/// descriptive error listing the valid presets.
pub fn video_resolution(cfg: &Config) -> Result<(u32, u32)> {
    parse_resolution(&cfg.media_video_resolution).ok_or_else(|| {
        let valid = VP9_BLACK_KEYFRAMES
            .iter()
            .map(|kf| format!("{}x{}", kf.width, kf.height))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow!(
            "invalid media-video-resolution {:?}: must be one of {valid}",
            cfg.media_video_resolution
        )
    })
}

/// Bridges an already-open TUN device into the tunnel and runs the call
/// daemon loop until shutdown. The caller is responsible for opening the TUN
/// device (differs by platform) and for the command channel's producer side.
///
/// `auto_call`: place a call to `cfg.remote_peer_id` as soon as the daemon
/// reaches idle for the first time, instead of waiting for a `Call` command.
/// Android is always the caller side and has no `anazoa-ctl`-equivalent
/// separate step, so its single Connect button needs this; the desktop CLI
/// passes `false` and keeps using `anazoa-ctl call`/`answer` explicitly.
///
/// `media_path`: same role as the desktop CLI's `media` config field (an
/// on-disk `.opus` file) — takes a path rather than an already-spawned
/// `RaylibMedia` so callers that don't otherwise need `video_resolution()`
/// (i.e. android.rs) don't have to duplicate that computation just to build
/// one.
///
/// `status`: receives every [`EngineState`] transition; the host reads it
/// to answer status queries without going through `cmd_rx` (see
/// [`EngineState`]).
#[allow(clippy::too_many_arguments)]
pub async fn run_with_tun(
    cfg: &Config,
    tun: tun_rs::AsyncDevice,
    log_dir: Option<&std::path::Path>,
    log_prefix: &str,
    media_path: Option<&str>,
    mut cmd_rx: mpsc::Receiver<DaemonCmd>,
    auto_call: bool,
    status: &watch::Sender<EngineState>,
) -> Result<()> {
    status.send_replace(EngineState::Connecting);
    let (video_width, video_height) = video_resolution(cfg)?;
    let _tun_bridge = start_tun_bridge(tun, log_dir, log_prefix, video_width, video_height).await?;
    let mut media = match media_path {
        Some(p) => {
            info!(
                "spawning RaylibMedia from {p:?} (this blocks on building a VAD map over the whole file)"
            );
            let started = std::time::Instant::now();
            let m = RaylibMedia::spawn(p, video_width, video_height)?;
            info!("RaylibMedia ready in {:?}", started.elapsed());
            Some(m)
        }
        None => {
            info!("no media_path; audio ticks will send silence");
            None
        }
    };
    let result = run_daemon(
        cfg,
        &mut media,
        &mut cmd_rx,
        video_width,
        video_height,
        auto_call,
        status,
    )
    .await;
    status.send_replace(EngineState::Stopped);
    finalize_webm();
    result
}

// ── call execution ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_call(
    signaling: &mut SignalingClient,
    turn: TurnServer,
    role: Role,
    media: &mut Option<RaylibMedia>,
    shutdown_rx: watch::Receiver<bool>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    peer_id: i64,
    started_at: Instant,
    video_width: u32,
    video_height: u32,
) -> Result<()> {
    let mut call = WebrtcCall::new(&turn, role, video_width, video_height)?;

    match role {
        Role::Caller => {
            let offer = create_offer(&call.pc).await.context("create SDP offer")?;
            let local_sdp = offer.stringify();
            log_sdp_negotiation("local offer", &local_sdp);
            set_local_description(&call.pc, offer)
                .await
                .context("set local offer")?;
            call.local_ufrag = extract_ice_ufrag(&local_sdp);
            info!("sending SDP offer");
            send_sdp(signaling, "offer", &local_sdp).await?;
            signaling.send_change_media_settings().await?;
        }
        Role::Calltaker => {
            signaling.send_update_media_modifiers().await?;
            signaling.send_accept_call().await?;
            signaling.send_get_rooms().await?;
            signaling.send_change_media_settings().await?;
            signaling.send_change_participant_state().await?;
            info!("waiting for remote SDP offer");
            let sdp = wait_for_sdp(signaling).await?;
            log_sdp_negotiation("remote offer", &sdp);
            set_remote_sdp(&call.pc, SdpType::Offer, &sdp).await?;
            ensure_local_senders(&call, "after remote offer")?;
            let answer = create_answer(&call.pc).await.context("create SDP answer")?;
            let local_sdp = answer.stringify();
            log_sdp_negotiation("local answer", &local_sdp);
            set_local_description(&call.pc, answer)
                .await
                .context("set local answer")?;
            log_local_senders(&call, "after local answer");
            call.local_ufrag = extract_ice_ufrag(&local_sdp);
            info!("sending SDP answer");
            send_sdp(signaling, "answer", &local_sdp).await?;
        }
    }

    drive_call(
        signaling,
        call,
        role,
        media,
        shutdown_rx,
        cmd_rx,
        peer_id,
        started_at,
        video_width,
        video_height,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_call(
    signaling: &mut SignalingClient,
    mut call: WebrtcCall,
    role: Role,
    media: &mut Option<RaylibMedia>,
    mut shutdown_rx: watch::Receiver<bool>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    peer_id: i64,
    started_at: Instant,
    video_width: u32,
    video_height: u32,
) -> Result<()> {
    let mut audio_tick = interval(Duration::from_millis(20));
    audio_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut video_tick = interval(Duration::from_millis(1000 / VIDEO_FPS));
    video_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut custom_tick = interval(Duration::from_secs(5));
    custom_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let auth_deadline = tokio::time::sleep(AUTH_TIMEOUT);

    let silence = vec![0i16; AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS as usize];
    tokio::pin!(auth_deadline);

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                info!("call shutting down");
                break;
            }

            _ = shutdown_rx.changed() => {
                info!("call shutting down");
                break;
            }

            Some(event) = call.events.recv() => {
                if !handle_local_event(signaling, &mut call, event).await? {
                    break;
                }
            }

            received = tokio::time::timeout(SIGNAL_TIMEOUT, signaling.receive_signal_value()) => {
                match received {
                    Ok(Ok(data)) => handle_remote_signal(&mut call, data, role).await?,
                    Ok(Err(err)) => {
                        debug!("signaling receive failed: {err:#}");
                        break;
                    }
                    Err(_) => {
                        debug!("no signaling data for {}s", SIGNAL_TIMEOUT.as_secs());
                    }
                }
            }

            _ = audio_tick.tick() => {
                let audio_frame = if let Some(media) = media.as_mut() {
                    media.next_audio_frame().unwrap_or_else(|err| {
                        debug!("raylib audio read failed, sending silence: {err:#}");
                        silence.clone()
                    })
                } else {
                    silence.clone()
                };
                unsafe {
                    let _ = call.media.audio_source.capture_frame(
                        &audio_frame,
                        AUDIO_SAMPLE_RATE,
                        AUDIO_CHANNELS,
                        AUDIO_FRAME_SAMPLES,
                        std::ptr::null(),
                        webrtc_sys::audio_track::CompleteCallback(crate::protozoa::webrtc::audio_capture_complete),
                    );
                }
            }

            _ = video_tick.tick() => {
                let authenticated = tunnel_state()
                    .is_none_or(|s| s.authenticated.load(std::sync::atomic::Ordering::Relaxed));
                if !authenticated {
                    continue;
                }
                let result = if let Some(media) = media.as_mut() {
                    let frame_count = media.frame_count;
                    if frame_count % 150 == 0 {
                        debug!("capturing video frame {}", frame_count);
                    }
                    match media.next_video_frame() {
                        Ok(frame) => send_i420_video_frame(&call.media.video_source, Some(&frame), video_width, video_height),
                        Err(err) => {
                            debug!("raylib video frame failed, sending black: {err:#}");
                            send_i420_video_frame(&call.media.video_source, None, video_width, video_height)
                        }
                    }
                } else {
                    send_i420_video_frame(&call.media.video_source, None, video_width, video_height)
                };
                if let Err(err) = result {
                    debug!("video frame failed: {err:#}");
                }
            }

            _ = custom_tick.tick() => {
                if noise_auth_failed() {
                    warn!("Noise KK authentication failed, hanging up");
                    break;
                }
                if let Err(err) = signaling.send_custom_data().await {
                    debug!("custom-data send failed: {err:#}");
                }
                log_tunnel_stats();
            }

            _ = &mut auth_deadline => {
                if noise_auth_failed()
                    || tunnel_state()
                        .is_some_and(|s| !s.authenticated.load(std::sync::atomic::Ordering::Relaxed))
                {
                    warn!("Noise KK authentication deadline exceeded, hanging up");
                    break;
                }

                auth_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60 * 24 * 365));
            }

            cmd = cmd_rx.recv() => {
                if handle_call_cmd(cmd, peer_id, started_at) {
                    break;
                }
            }
        }
    }

    call.pc.close();
    Ok(())
}

/// Coarse engine state, published through a `watch` channel so a host can
/// answer status queries without a round-trip through `cmd_rx` — which the
/// engine doesn't poll while it's blocked in `RaylibMedia::spawn`, mid
/// call setup, or in post-call signaling teardown. Android's UI thread
/// polls status, so a blocking query there was an ANR waiting to happen.
#[derive(Clone, Copy, Debug)]
pub enum EngineState {
    /// No OneMe session yet (initial connect, or re-establishing after
    /// the session dropped). Also covers media loading before the first
    /// connect attempt.
    Connecting,
    /// Session up, no call in progress.
    Idle,
    /// Outgoing call requested, signaling not yet connected.
    Dialing {
        peer_id: i64,
    },
    /// Incoming call accepted, signaling not yet connected.
    Answering {
        peer_id: i64,
    },
    InCall {
        peer_id: i64,
        started_at: Instant,
    },
    /// The daemon loop has returned; nothing will update this again.
    Stopped,
}

/// The JSON `anazoa-ctl status` (and Android's `nativeStatus`) return for a
/// given state. Single source of truth for the shape — both the `cmd_rx`
/// responders and the `watch`-based host path go through here.
pub fn status_json(state: &EngineState) -> Value {
    match state {
        EngineState::Connecting => json!({"state": "connecting"}),
        EngineState::Idle => json!({"state": "idle"}),
        EngineState::Dialing { peer_id } => json!({"state": "dialing", "peer_id": peer_id}),
        EngineState::Answering { peer_id } => json!({"state": "answering", "peer_id": peer_id}),
        EngineState::InCall {
            peer_id,
            started_at,
        } => json!({
            "state": "in_call",
            "peer_id": peer_id,
            "uptime_secs": started_at.elapsed().as_secs(),
            "tunnel": tunnel_stats_json(),
        }),
        EngineState::Stopped => json!({"state": "stopped"}),
    }
}

fn utilization_window_json(data: u64, total: u64, secs: u64) -> Value {
    let ratio = if total > 0 {
        (data as f64 / total as f64 * 1000.0).round() / 1000.0
    } else {
        0.0
    };
    json!({ "data_frames": data, "total_frames": total, "ratio": ratio, "window_secs": secs })
}

fn tunnel_stats_json() -> Value {
    let Some(state) = tunnel_state() else {
        return json!(null);
    };
    let (local_w, local_h) = unpack_resolution(state.encoder_resolution.load(Ordering::Relaxed));
    let (remote_w, remote_h) =
        unpack_resolution(state.remote_vp9_resolution.load(Ordering::Relaxed));
    let queue_depth = OUTBOUND_QUEUE_MAX_PACKETS - state.outbound_semaphore.available_permits();

    let packets_per_frame = {
        let pkts = state.tun_read_packets.load(Ordering::Relaxed);
        let frames = state.tunnel_frames_sent.load(Ordering::Relaxed);
        if frames > 0 {
            (pkts as f64 / frames as f64 * 100.0).round() / 100.0
        } else {
            0.0
        }
    };

    let utilization = state.utilization.try_lock().ok().map(|u| {
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let total_frames = state.hook_encode_calls.load(Ordering::Relaxed);
        let data_frames = state.tunnel_frames_sent.load(Ordering::Relaxed);
        let (d5, t5) = u.window(5, now_sec);
        let (d30, t30) = u.window(30, now_sec);
        let (d300, t300) = u.window(300, now_sec);
        json!({
            "5s":    utilization_window_json(d5,          t5,          5),
            "30s":   utilization_window_json(d30,         t30,         30),
            "5min":  utilization_window_json(d300,        t300,        300),
            "total": utilization_window_json(data_frames, total_frames, 0),
        })
    });

    json!({
        "packets_rx": state.tun_read_packets.load(Ordering::Relaxed),
        "packets_tx": state.tun_write_packets.load(Ordering::Relaxed),
        "frames_tx": state.tunnel_frames_sent.load(Ordering::Relaxed),
        "frames_rx": state.tunnel_frames_received.load(Ordering::Relaxed),
        "packet_bytes_rx": state.tun_read_bytes.load(Ordering::Relaxed),
        "packet_bytes_tx": state.tun_write_bytes.load(Ordering::Relaxed),
        "frame_bytes_tx": state.tunnel_frame_bytes_sent.load(Ordering::Relaxed),
        "frame_bytes_rx": state.tunnel_frame_bytes_received.load(Ordering::Relaxed),
        "packets_reassembled": state.tunnel_packets_reassembled.load(Ordering::Relaxed),
        "fragments_dropped": state.tunnel_fragments_dropped.load(Ordering::Relaxed),
        "outbound_queue": queue_depth,
        "local_resolution": format!("{local_w}x{local_h}"),
        "remote_resolution": format!("{remote_w}x{remote_h}"),
        "utilization": utilization,
        "packets_per_frame": packets_per_frame,
    })
}

/// Idle-loop state for whether an incoming call should be auto-answered.
enum AnswerState {
    /// No incoming call is expected; ignore it.
    Closed,
    /// Auto-answer every incoming call until explicitly re-armed or closed.
    Forever,
    /// Auto-answer the next incoming call: `None` never expires (armed via
    /// `answer anytime`), `Some(d)` expires at `d` (armed via `answer <secs>`).
    /// Either way, consumed after one call — see its use in `idle_loop`.
    Timed(Option<Instant>),
}

fn answer_window_open(state: &AnswerState) -> bool {
    match state {
        AnswerState::Closed => false,
        AnswerState::Forever => true,
        AnswerState::Timed(None) => true,
        AnswerState::Timed(Some(d)) => Instant::now() <= *d,
    }
}

// Returns true if the call should end (hangup received).
fn handle_call_cmd(cmd: Option<DaemonCmd>, peer_id: i64, started_at: Instant) -> bool {
    match cmd {
        Some(DaemonCmd::Status { resp }) => {
            let status = status_json(&EngineState::InCall {
                peer_id,
                started_at,
            });
            debug!("status: {status}");
            let _ = resp.send(Ok(status));
            false
        }
        Some(DaemonCmd::Hangup { resp }) => {
            info!("hangup requested via RPC");
            let _ = resp.send(Ok(json!({"state": "ok"})));
            true
        }
        Some(DaemonCmd::Call { resp, .. }) => {
            let _ = resp.send(Err(anyhow!("already in a call")));
            false
        }
        Some(DaemonCmd::Answer { resp, .. }) => {
            let _ = resp.send(Err(anyhow!("cannot set answer mode during a call")));
            false
        }
        Some(DaemonCmd::Shutdown { resp }) => {
            info!("shutdown requested via RPC");
            let _ = resp.send(Ok(json!({"state": "ok"})));
            trigger_shutdown();
            true
        }
        None => true,
    }
}

// ── daemon idle loop ──────────────────────────────────────────────────────────

enum IdleOutcome {
    Incoming {
        call: IncomingCall,
    },
    OutgoingRequested {
        peer_id: i64,
        resp: tokio::sync::oneshot::Sender<Result<Value>>,
    },
    Shutdown,
    SessionError,
}

fn spawn_wait_task(
    oneme: Arc<Mutex<SessionClient>>,
    tx: mpsc::Sender<Result<IncomingCall>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = oneme.lock().await.wait_for_incoming_call().await;
        let _ = tx.send(result).await;
    })
}

async fn idle_loop(
    oneme: Arc<Mutex<SessionClient>>,
    peer_id: i64,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    answer_deadline: &mut AnswerState,
) -> IdleOutcome {
    let (call_tx, mut call_rx) = mpsc::channel(1);
    let mut wait_task = spawn_wait_task(Arc::clone(&oneme), call_tx.clone());

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                wait_task.abort();
                return IdleOutcome::Shutdown;
            }

            Some(result) = call_rx.recv() => {
                match result {
                    Ok(incoming) if incoming.caller_id == peer_id => {
                        if answer_window_open(answer_deadline) {
                            if !matches!(answer_deadline, AnswerState::Forever) {
                                *answer_deadline = AnswerState::Closed;
                            }
                            return IdleOutcome::Incoming { call: incoming };
                        }
                        warn!(
                            "ignoring call from peer {} outside answer window",
                            incoming.caller_id
                        );
                        wait_task = spawn_wait_task(Arc::clone(&oneme), call_tx.clone());
                    }
                    Ok(incoming) => {
                        warn!("ignoring call from unexpected peer {}", incoming.caller_id);
                        wait_task = spawn_wait_task(Arc::clone(&oneme), call_tx.clone());
                    }
                    Err(err) => {
                        warn!("wait_for_incoming_call: {err:#}");
                        return IdleOutcome::SessionError;
                    }
                }
            }

            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    DaemonCmd::Status { resp } => {
                        let status = status_json(&EngineState::Idle);
                        debug!("status: {status}");
                        let _ = resp.send(Ok(status));
                    }
                    DaemonCmd::Hangup { resp } => {
                        let _ = resp.send(Err(anyhow!("not in a call")));
                    }
                    DaemonCmd::Answer { secs, forever, resp } => {
                        *answer_deadline = if forever {
                            AnswerState::Forever
                        } else {
                            AnswerState::Timed(secs.map(|s| Instant::now() + Duration::from_secs(s)))
                        };
                        let result = match (forever, secs) {
                            (true, _) => json!({"state": "forever"}),
                            (false, Some(s)) => json!({"state": "armed", "secs": s}),
                            (false, None) => json!({"state": "anytime"}),
                        };
                        let _ = resp.send(Ok(result));
                    }
                    DaemonCmd::Call { peer_id: override_id, resp } => {
                        wait_task.abort();
                        let _ = wait_task.await;
                        return IdleOutcome::OutgoingRequested {
                            peer_id: override_id.unwrap_or(peer_id),
                            resp,
                        };
                    }
                    DaemonCmd::Shutdown { resp } => {
                        info!("shutdown requested via RPC");
                        let _ = resp.send(Ok(json!({"state": "ok"})));
                        trigger_shutdown();
                        // Let the next loop iteration's shutdown_signal() arm
                        // (above) pick this up, rather than duplicating its
                        // wait_task.abort()/IdleOutcome::Shutdown here.
                    }
                }
            }
        }
    }
}

// ── call runners ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_incoming_call(
    oneme: Arc<Mutex<SessionClient>>,
    incoming: IncomingCall,
    endpoints: &ServiceEndpoints,
    signaling_user_id: &str,
    oneme_keepalive_secs: u64,
    fingerprint: &FingerprintConfig,
    media: &mut Option<RaylibMedia>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    video_width: u32,
    video_height: u32,
    status: &watch::Sender<EngineState>,
) {
    info!("incoming call from peer {}", incoming.caller_id);
    status.send_replace(EngineState::Answering {
        peer_id: incoming.caller_id,
    });
    prepare_noise_for_call(NoiseRole::Responder);

    let turn = incoming.turn.clone();

    let (keepalive_stop_tx, keepalive_stop_rx) = watch::channel(false);
    let keepalive = tokio::spawn(call_keepalive_loop(
        Arc::clone(&oneme),
        Duration::from_secs(oneme_keepalive_secs.max(1)),
        keepalive_stop_rx,
    ));

    let mut signaling =
        match SignalingClient::from_incoming(&incoming, signaling_user_id, endpoints, fingerprint)
            .await
        {
            Ok(s) => s,
            Err(err) => {
                warn!("signaling connect: {err:#}");
                let _ = keepalive_stop_tx.send(true);
                let _ = keepalive.await;
                return;
            }
        };

    let (hangup_tx, hangup_rx) = watch::channel(false);
    let started_at = Instant::now();
    status.send_replace(EngineState::InCall {
        peer_id: incoming.caller_id,
        started_at,
    });

    let call_result = run_call(
        &mut signaling,
        turn,
        Role::Calltaker,
        media,
        hangup_rx,
        cmd_rx,
        incoming.caller_id,
        started_at,
        video_width,
        video_height,
    )
    .await;

    let _ = hangup_tx.send(true);
    let _ = signaling.hangup().await;
    let _ = signaling.close().await;
    let _ = keepalive_stop_tx.send(true);
    let _ = keepalive.await;

    match call_result {
        Ok(()) => info!("call ended"),
        Err(err) => warn!("call ended with error: {err:#}"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_outgoing_call(
    oneme: Arc<Mutex<SessionClient>>,
    peer_id: i64,
    call_resp: tokio::sync::oneshot::Sender<Result<Value>>,
    cfg: &Config,
    endpoints: &ServiceEndpoints,
    media: &mut Option<RaylibMedia>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    video_width: u32,
    video_height: u32,
    status: &watch::Sender<EngineState>,
) {
    info!("placing call to {peer_id}");
    status.send_replace(EngineState::Dialing { peer_id });
    prepare_noise_for_call(NoiseRole::Initiator);
    let started = match oneme.lock().await.start_outgoing_call(peer_id).await {
        Ok(s) => s,
        Err(err) => {
            // Logged as well as returned: with auto_call nobody reads the
            // response, so this would otherwise vanish without a trace.
            warn!("start_outgoing_call: {err:#}");
            let _ = call_resp.send(Err(anyhow!("start_outgoing_call: {err:#}")));
            return;
        }
    };

    // Acknowledge the RPC command — the call is now in progress.
    let _ = call_resp.send(Ok(json!({"state": "dialing", "peer_id": peer_id})));

    let turn = started.turn_server.clone();
    let mut signaling = match SignalingClient::from_outgoing(
        &started,
        &peer_id.to_string(),
        &cfg.signaling_user_id,
        endpoints,
        &cfg.auth.fingerprint,
    )
    .await
    {
        Ok(s) => s,
        Err(err) => {
            warn!("signaling connect: {err:#}");
            return;
        }
    };

    let (keepalive_stop_tx, keepalive_stop_rx) = watch::channel(false);
    let keepalive = tokio::spawn(call_keepalive_loop(
        Arc::clone(&oneme),
        Duration::from_secs(cfg.oneme_keepalive_secs.max(1)),
        keepalive_stop_rx,
    ));

    let (hangup_tx, hangup_rx) = watch::channel(false);
    let started_at = Instant::now();
    status.send_replace(EngineState::InCall {
        peer_id,
        started_at,
    });

    let call_result = run_call(
        &mut signaling,
        turn,
        Role::Caller,
        media,
        hangup_rx,
        cmd_rx,
        peer_id,
        started_at,
        video_width,
        video_height,
    )
    .await;

    let _ = hangup_tx.send(true);
    let _ = signaling.hangup().await;
    let _ = signaling.close().await;
    let _ = keepalive_stop_tx.send(true);
    let _ = keepalive.await;

    match call_result {
        Ok(()) => info!("call ended"),
        Err(err) => warn!("call ended with error: {err:#}"),
    }
}

// Answers a command with "we're not connected yet" instead of leaving the
// caller hanging. Without this, a Status/Call/Answer/Hangup request made
// while offline (no network, or the OneMe session dropped and hasn't
// reconnected) would just never get a response — cmd_rx wasn't polled at
// all during connect_session_with_retry. On the CLI that meant `anazoa-ctl
// status` would hang forever; on Android it's worse, since nativeStatus()'s
// JNI call blocks whichever thread called it, and the UI polls it on the
// main thread — so a phone with no signal would freeze the app, not just
// show a stale status.
fn reject_offline(cmd: DaemonCmd, state: &EngineState) {
    match cmd {
        DaemonCmd::Status { resp } => {
            let _ = resp.send(Ok(status_json(state)));
        }
        DaemonCmd::Hangup { resp } => {
            let _ = resp.send(Err(anyhow!("not connected")));
        }
        DaemonCmd::Call { resp, .. } => {
            let _ = resp.send(Err(anyhow!("not connected")));
        }
        DaemonCmd::Answer { resp, .. } => {
            let _ = resp.send(Err(anyhow!("not connected")));
        }
        // Unlike the above, shutdown must work regardless of connection
        // state — matches Android's nativeStop, which the user can invoke
        // even mid-reconnect-backoff.
        DaemonCmd::Shutdown { resp } => {
            let _ = resp.send(Ok(json!({"state": "ok"})));
            trigger_shutdown();
        }
    }
}

async fn connect_session_with_retry(
    cfg: &Config,
    endpoints: &ServiceEndpoints,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
) -> Option<SessionClient> {
    loop {
        // Pinned outside the select loop so answering a command doesn't
        // drop (and restart from scratch) the in-flight establish: Android
        // polls status every 2 s while connecting, and on a link where
        // TCP+TLS+hello+chat-sync takes longer than that, a select! that
        // `continue`d on every command would never let establish finish.
        let establish = SessionClient::establish(
            &cfg.token,
            endpoints,
            cfg.oneme_keepalive_secs,
            &cfg.auth.fingerprint,
        );
        tokio::pin!(establish);
        let result = loop {
            tokio::select! {
                _ = shutdown_signal() => return None,
                Some(cmd) = cmd_rx.recv() => reject_offline(cmd, &EngineState::Connecting),
                result = &mut establish => break result,
            }
        };
        match result {
            Ok((client, _)) => return Some(client),
            Err(err) => warn!("session: {err:#}"),
        }

        // Back off before retrying, but keep answering cmd_rx (and stay
        // responsive to shutdown) during the wait too — RECONNECT_DELAY is
        // only 2s, but the same hang applies to this window otherwise.
        if wait_answering_cmds(RECONNECT_DELAY, cmd_rx, &EngineState::Connecting).await {
            return None;
        }
    }
}

/// Sleeps for `delay` while still answering `cmd_rx` (with `state` as the
/// status) and watching for shutdown. Returns true if shutdown fired.
async fn wait_answering_cmds(
    delay: Duration,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    state: &EngineState,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            _ = shutdown_signal() => return true,
            _ = tokio::time::sleep_until(deadline) => return false,
            Some(cmd) = cmd_rx.recv() => reject_offline(cmd, state),
        }
    }
}

/// `auto_call`: see [`run_with_tun`]. Not one-shot — every time the daemon
/// would otherwise go idle (the call ended, or never got going: callee
/// rejected, signaling failed, session dropped) it redials after
/// `RECONNECT_DELAY`. Android has no other way to redial: its UI never
/// issues a `Call` command, so a single failed attempt would otherwise
/// leave the app sitting in `idle` with no tunnel until Disconnect/Connect.
pub async fn run_daemon(
    cfg: &Config,
    media: &mut Option<RaylibMedia>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    video_width: u32,
    video_height: u32,
    auto_call: bool,
    status: &watch::Sender<EngineState>,
) -> Result<()> {
    let endpoints = cfg.auth.endpoints.clone();

    // Establish the OneMe session once and keep it alive across calls,
    // matching Android behavior (single WebSocket for the full app session).
    status.send_replace(EngineState::Connecting);
    let Some(client) = connect_session_with_retry(cfg, &endpoints, cmd_rx).await else {
        return Ok(());
    };
    info!("OneMe session established");
    let oneme = Arc::new(Mutex::new(client));

    let mut answer_deadline = AnswerState::Closed;
    let mut first_dial = true;
    loop {
        status.send_replace(EngineState::Idle);
        let outcome = if auto_call {
            if !std::mem::take(&mut first_dial) {
                // The call usually ends *because* of shutdown (Android's
                // Disconnect); don't announce a redial that the wait below
                // would cancel on its first poll.
                if shutdown_requested() {
                    return Ok(());
                }
                info!("auto-call: redialing in {RECONNECT_DELAY:?}");
                if wait_answering_cmds(RECONNECT_DELAY, cmd_rx, &EngineState::Idle).await {
                    return Ok(());
                }
            }
            // No RPC caller is waiting on this ack (there was no Call
            // command at all), so the response end is just dropped.
            let (resp, _resp_rx) = tokio::sync::oneshot::channel();
            IdleOutcome::OutgoingRequested {
                peer_id: cfg.remote_peer_id,
                resp,
            }
        } else {
            idle_loop(
                Arc::clone(&oneme),
                cfg.remote_peer_id,
                cmd_rx,
                &mut answer_deadline,
            )
            .await
        };
        match outcome {
            IdleOutcome::Incoming { call } => {
                run_incoming_call(
                    Arc::clone(&oneme),
                    call,
                    &endpoints,
                    &cfg.signaling_user_id,
                    cfg.oneme_keepalive_secs,
                    &cfg.auth.fingerprint,
                    media,
                    cmd_rx,
                    video_width,
                    video_height,
                    status,
                )
                .await;
            }
            IdleOutcome::OutgoingRequested { peer_id, resp } => {
                run_outgoing_call(
                    Arc::clone(&oneme),
                    peer_id,
                    resp,
                    cfg,
                    &endpoints,
                    media,
                    cmd_rx,
                    video_width,
                    video_height,
                    status,
                )
                .await;
            }
            IdleOutcome::Shutdown => return Ok(()),
            IdleOutcome::SessionError => {
                warn!("OneMe session lost, re-establishing");
                status.send_replace(EngineState::Connecting);
                if wait_answering_cmds(RECONNECT_DELAY, cmd_rx, &EngineState::Connecting).await {
                    return Ok(());
                }
                let Some(client) = connect_session_with_retry(cfg, &endpoints, cmd_rx).await else {
                    return Ok(());
                };
                *oneme.lock().await = client;
                info!("OneMe session re-established");
            }
        }
    }
}

// ── SDP / signal helpers ──────────────────────────────────────────────────────

fn log_tunnel_stats() {
    let Some(state) = tunnel_state() else {
        return;
    };
    debug!(
        hook_encode = state.hook_encode_calls.load(Ordering::Relaxed),
        hook_ref = state.hook_reference_calls.load(Ordering::Relaxed),
        tun_read = state.tun_read_packets.load(Ordering::Relaxed),
        tun_write = state.tun_write_packets.load(Ordering::Relaxed),
        tunnel_sent = state.tunnel_frames_sent.load(Ordering::Relaxed),
        tunnel_recv = state.tunnel_frames_received.load(Ordering::Relaxed),
        tunnel_packets = state.tunnel_packets_reassembled.load(Ordering::Relaxed),
        "tunnel stats"
    );
}

async fn handle_local_event(
    signaling: &mut SignalingClient,
    call: &mut WebrtcCall,
    event: LocalEvent,
) -> Result<bool> {
    match event {
        LocalEvent::IceCandidate {
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            let mut body = json!({
                "candidate": {
                    "candidate": candidate,
                    "sdpMid": sdp_mid,
                    "sdpMLineIndex": sdp_mline_index,
                }
            });
            if let Some(ufrag) = &call.local_ufrag {
                body["candidate"]["usernameFragment"] = json!(ufrag);
            }
            signaling.send_signal(body).await?;
        }
        LocalEvent::IceState(state) => info!("ICE state = {}", ice_state_name(state)),
        LocalEvent::ConnectionState(state) => {
            info!("connection state = {}", pc_state_name(state));
            if matches!(
                state,
                PeerConnectionState::Failed | PeerConnectionState::Closed
            ) {
                return Ok(false);
            }
        }
        LocalEvent::RemoteTrack(transceiver) => {
            crate::protozoa::webrtc::attach_remote_sinks(transceiver, &mut call.media)
        }
    }
    Ok(true)
}

async fn handle_remote_signal(call: &mut WebrtcCall, data: Value, role: Role) -> Result<()> {
    if let Some(sdp) = data.get("sdp") {
        let sdp_type = sdp
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("remote SDP missing type"))?;
        let body = sdp
            .get("sdp")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("remote SDP missing sdp"))?;
        let ty = match sdp_type {
            "offer" => SdpType::Offer,
            "answer" => SdpType::Answer,
            other => bail!("unsupported remote SDP type {other}"),
        };
        info!("received remote SDP {sdp_type}");
        log_sdp_negotiation(&format!("remote {sdp_type}"), body);
        set_remote_sdp(&call.pc, ty, body).await?;
        attach_existing_remote_tracks(call);
        if role == Role::Caller && matches!(ty, SdpType::Answer) {
            log_local_senders(call, "after remote answer");
            info!("remote answer applied");
        }
        return Ok(());
    }

    if let Some(candidate) = data.get("candidate") {
        let cand = candidate
            .get("candidate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if cand.is_empty() {
            debug!("remote end-of-candidates");
            return Ok(());
        }
        let sdp_mid = candidate
            .get("sdpMid")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .to_string();
        let sdp_mline_index = candidate
            .get("sdpMLineIndex")
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32;
        let ice =
            webrtc_sys::jsep::ffi::create_ice_candidate(sdp_mid, sdp_mline_index, cand.to_string())
                .map_err(|e| anyhow!("create remote ICE candidate: {e}"))?;
        add_ice_candidate(&call.pc, ice).await?;
    }

    Ok(())
}

fn log_sdp_negotiation(label: &str, sdp: &str) {
    info!(
        label,
        bytes = sdp.len(),
        lines = sdp.lines().count(),
        "SDP negotiation"
    );
    debug!(label, sdp = %sdp, "SDP body");
    log_sdp_media_directions(label, sdp);
}

fn log_sdp_media_directions(label: &str, sdp: &str) {
    let mut current_media: Option<&str> = None;
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            current_media = rest.split_whitespace().next();
            continue;
        }
        if matches!(
            line,
            "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
        ) && let Some(media) = current_media
        {
            debug!("{label}: {media} {line}");
        }
    }
}

async fn send_sdp(signaling: &mut SignalingClient, ty: &str, sdp: &str) -> Result<()> {
    signaling
        .send_signal(json!({
            "sdp": {"type": ty, "sdp": sdp},
            "animojiVersion": 1,
        }))
        .await
}

async fn wait_for_sdp(signaling: &mut SignalingClient) -> Result<String> {
    loop {
        let data = tokio::time::timeout(SIGNAL_TIMEOUT, signaling.receive_signal_value())
            .await
            .map_err(|_| anyhow!("timed out waiting for SDP"))??;
        if let Some(sdp) = data
            .get("sdp")
            .and_then(|v| v.get("sdp"))
            .and_then(Value::as_str)
        {
            return Ok(sdp.to_string());
        }
    }
}

fn extract_ice_ufrag(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .map(str::to_string)
}

fn parse_noise_key(b64: &str) -> Result<[u8; 32]> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("noise key is not valid base64")?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow!("noise key must decode to 32 bytes, got {}", v.len()))
}
