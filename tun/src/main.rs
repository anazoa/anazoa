use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use anazoa_auth::TurnServer;
use anazoa_auth::oneme::{IncomingCall, SessionClient};
use anazoa_auth::signaling::SignalingClient;
use anazoa_config::{FingerprintConfig, ServiceEndpoints, load_config};
use anazoa_tun::{call_keepalive_loop, shutdown_signal, wait_or_shutdown};

mod daemon;
mod protozoa;

use daemon::DaemonCmd;
use protozoa::media::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE, RaylibMedia, VIDEO_FPS,
    send_i420_video_frame,
};
use protozoa::tunnel::{
    NoiseRole, OUTBOUND_QUEUE_MAX_PACKETS, TUNNEL_STATE, finalize_webm, init_noise_keys,
    keep_opus_hooks_linked, keep_vp9_hooks_linked, noise_auth_failed, prepare_noise_for_call,
    start_tun_bridge, unpack_resolution,
};
use protozoa::vp9::{VP9_BLACK_KEYFRAMES, parse_resolution};
use protozoa::webrtc::{
    LocalEvent, Role, WebrtcCall, add_ice_candidate, attach_existing_remote_tracks, create_answer,
    create_offer, ensure_local_senders, ice_state_name, log_local_senders, pc_state_name,
    set_local_description, set_remote_sdp,
};
use std::time::{SystemTime, UNIX_EPOCH};
use webrtc_sys::jsep::ffi::SdpType;
use webrtc_sys::peer_connection::ffi::PeerConnectionState;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(60);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
// NOISE_CHECK_FRAMES covers ~4 s at 50 fps; 10 s leaves headroom for slow ICE/DTLS
// before audio begins. noise_auth_failed() via custom_tick is the normal failure path.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

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
                        webrtc_sys::audio_track::CompleteCallback(protozoa::webrtc::audio_capture_complete),
                    );
                }
            }

            _ = video_tick.tick() => {
                let authenticated = TUNNEL_STATE
                    .get()
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
                    || TUNNEL_STATE
                        .get()
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

fn utilization_window_json(data: u64, total: u64, secs: u64) -> Value {
    let ratio = if total > 0 {
        (data as f64 / total as f64 * 1000.0).round() / 1000.0
    } else {
        0.0
    };
    json!({ "data_frames": data, "total_frames": total, "ratio": ratio, "window_secs": secs })
}

fn tunnel_stats_json() -> Value {
    let Some(state) = TUNNEL_STATE.get() else {
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

fn answer_window_open(deadline: &Option<Option<Instant>>) -> bool {
    match deadline {
        None => false,
        Some(None) => true,
        Some(Some(d)) => Instant::now() <= *d,
    }
}

// Returns true if the call should end (hangup received).
fn handle_call_cmd(cmd: Option<DaemonCmd>, peer_id: i64, started_at: Instant) -> bool {
    match cmd {
        Some(DaemonCmd::Status { resp }) => {
            let status = json!({
                "state": "in_call",
                "peer_id": peer_id,
                "uptime_secs": started_at.elapsed().as_secs(),
                "tunnel": tunnel_stats_json(),
            });
            info!("status: {status}");
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
    answer_deadline: &mut Option<Option<Instant>>,
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
                            *answer_deadline = None;
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
                        let status = json!({"state": "idle"});
                        info!("status: {status}");
                        let _ = resp.send(Ok(status));
                    }
                    DaemonCmd::Hangup { resp } => {
                        let _ = resp.send(Err(anyhow!("not in a call")));
                    }
                    DaemonCmd::Answer { secs, resp } => {
                        *answer_deadline = Some(
                            secs.map(|s| Instant::now() + Duration::from_secs(s)),
                        );
                        let result = match secs {
                            Some(s) => json!({"state": "armed", "secs": s}),
                            None => json!({"state": "always"}),
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
) {
    info!("incoming call from peer {}", incoming.caller_id);
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
    cfg: &anazoa_tun::Config,
    endpoints: &ServiceEndpoints,
    media: &mut Option<RaylibMedia>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    video_width: u32,
    video_height: u32,
) {
    info!("placing call to {peer_id}");
    prepare_noise_for_call(NoiseRole::Initiator);
    let started = match oneme.lock().await.start_outgoing_call(peer_id).await {
        Ok(s) => s,
        Err(err) => {
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

async fn connect_session_with_retry(
    cfg: &anazoa_tun::Config,
    endpoints: &ServiceEndpoints,
) -> Option<SessionClient> {
    loop {
        let session = tokio::select! {
            _ = shutdown_signal() => return None,
            r = SessionClient::establish(
                &cfg.token,
                endpoints,
                cfg.oneme_keepalive_secs,
                &cfg.auth.fingerprint,
            ) => r,
        };
        match session {
            Ok((client, _)) => return Some(client),
            Err(err) => {
                warn!("session: {err:#}");
                if wait_or_shutdown(RECONNECT_DELAY).await {
                    return None;
                }
            }
        }
    }
}

async fn run_daemon(
    cfg: &anazoa_tun::Config,
    media: &mut Option<RaylibMedia>,
    cmd_rx: &mut mpsc::Receiver<DaemonCmd>,
    video_width: u32,
    video_height: u32,
) -> Result<()> {
    let endpoints = cfg.auth.endpoints.clone();

    // Establish the OneMe session once and keep it alive across calls,
    // matching Android behavior (single WebSocket for the full app session).
    let Some(client) = connect_session_with_retry(cfg, &endpoints).await else {
        return Ok(());
    };
    info!("OneMe session established");
    let oneme = Arc::new(Mutex::new(client));

    let mut answer_deadline: Option<Option<Instant>> = None;
    loop {
        match idle_loop(
            Arc::clone(&oneme),
            cfg.remote_peer_id,
            cmd_rx,
            &mut answer_deadline,
        )
        .await
        {
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
                )
                .await;
            }
            IdleOutcome::Shutdown => return Ok(()),
            IdleOutcome::SessionError => {
                warn!("OneMe session lost, re-establishing");
                if wait_or_shutdown(RECONNECT_DELAY).await {
                    return Ok(());
                }
                let Some(client) = connect_session_with_retry(cfg, &endpoints).await else {
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
    let Some(state) = TUNNEL_STATE.get() else {
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
            protozoa::webrtc::attach_remote_sinks(transceiver, &mut call.media)
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

// ── entry point ───────────────────────────────────────────────────────────────

fn parse_noise_key(b64: &str) -> Result<[u8; 32]> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("noise key is not valid base64")?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow!("noise key must decode to 32 bytes, got {}", v.len()))
}

fn usage() -> ! {
    eprintln!("usage: anazoa-tun [-c config.toml]");
    std::process::exit(1);
}

fn version() -> ! {
    println!("anazoa-tun {}", env!("CARGO_PKG_VERSION"));
    std::process::exit(0);
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = "anazoa.toml".to_string();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => version(),
            "-c" => {
                config_path = args
                    .next()
                    .ok_or_else(|| anyhow!("-c requires an argument"))?;
            }
            _ => usage(),
        }
    }

    let cfg: anazoa_tun::Config = load_config(&config_path)?;
    anazoa_config::init_logging(&cfg.auth.debug.level);
    anazoa_tun::init_shutdown_watcher();
    if cfg.auth.debug.log_signaling_ws {
        anazoa_auth::signaling::set_log_signaling_ws(true);
    }
    keep_vp9_hooks_linked();
    keep_opus_hooks_linked();

    match (
        cfg.noise_privkey.as_deref().filter(|s| !s.is_empty()),
        cfg.noise_peer_pubkey.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(priv_b64), Some(pub_b64)) => {
            let privkey = parse_noise_key(priv_b64)?.to_vec();
            let peer_pubkey = parse_noise_key(pub_b64)?.to_vec();
            init_noise_keys(privkey, peer_pubkey);
            info!("Noise KK authentication enabled");
        }
        (None, None) => {}
        _ => bail!("noise-privkey and noise-peer-pubkey must both be set or both omitted"),
    }

    let log_dir = cfg.auth.debug.log_dir.as_deref().map(std::path::Path::new);
    let tun_name = cfg.tun_name.as_str();
    if tun_name.is_empty() {
        bail!("tun-name must not be empty");
    }
    let tun = tun_rs::DeviceBuilder::new()
        .name(tun_name)
        .layer(tun_rs::Layer::L3)
        .build_async()
        .context("create TUN device")?;
    let (video_width, video_height) =
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
        })?;

    let log_prefix = cfg.auth.debug.log_prefix.as_deref().unwrap_or(tun_name);
    let _tun_bridge = start_tun_bridge(tun, log_dir, log_prefix, video_width, video_height).await?;

    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

    // Bind the daemon socket while still privileged so the path (e.g. /run/)
    // is writable even when privdrop is configured.
    let jsonrpc_listener = match daemon::bind_socket(&cfg.ctl_socket) {
        Ok(l) => Some(l),
        Err(e) => {
            warn!("JSON-RPC server: {e:#}");
            None
        }
    };

    anazoa_tun::privdrop::maybe_drop_privileges(cfg.privdrop.as_deref())?;

    let mut media = cfg
        .media
        .as_deref()
        .map(|p| RaylibMedia::spawn(p, video_width, video_height))
        .transpose()?;

    if let Some(listener) = jsonrpc_listener {
        tokio::spawn({
            let cmd_tx = cmd_tx.clone();
            async move {
                if let Err(e) = daemon::run_jsonrpc_server(listener, cmd_tx).await {
                    warn!("JSON-RPC server: {e:#}");
                }
            }
        });
    }

    run_daemon(&cfg, &mut media, &mut cmd_rx, video_width, video_height).await?;

    finalize_webm();
    Ok(())
}
