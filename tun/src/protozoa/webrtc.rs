use anyhow::{Context, Result, anyhow};
use cxx::{SharedPtr, UniquePtr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use anazoa_auth::TurnServer;
use webrtc_sys::audio_track::AudioSinkWrapper;
use webrtc_sys::audio_track::ffi::{
    AudioSourceOptions, AudioTrack, AudioTrackSource, NativeAudioSink, audio_to_media,
    media_to_audio, new_audio_track_source, new_native_audio_sink,
};
use webrtc_sys::jsep::ffi::{
    IceCandidate, SdpType, SessionDescription, create_session_description,
};
use webrtc_sys::media_stream::ffi::MediaStream;
use webrtc_sys::peer_connection::PeerContext;
use webrtc_sys::peer_connection::ffi::{
    ContinualGatheringPolicy, IceConnectionState, IceGatheringState, IceServer, IceTransportsType,
    PeerConnection, PeerConnectionState, RtcConfiguration, RtcOfferAnswerOptions, SignalingState,
};
use webrtc_sys::peer_connection_factory::ffi::{
    CandidatePairChangeEvent, PeerConnectionFactory, create_peer_connection_factory,
};
use webrtc_sys::peer_connection_factory::{PeerConnectionObserver, PeerConnectionObserverWrapper};
use webrtc_sys::rtc_error::ffi::RtcError;
use webrtc_sys::rtp_parameters::ffi::RtpCodecCapability;
use webrtc_sys::rtp_receiver::ffi::RtpReceiver;
use webrtc_sys::rtp_transceiver::ffi::{RtpTransceiver, RtpTransceiverInit};
use webrtc_sys::video_frame::ffi::VideoFrame;
use webrtc_sys::video_track::VideoSinkWrapper;
use webrtc_sys::video_track::ffi::{
    ContentHint, NativeVideoSink, VideoResolution, VideoTrack, VideoTrackSource,
    VideoTrackSourceConstraints, media_to_video, new_native_video_sink, new_video_track_source,
    video_to_media,
};
use webrtc_sys::webrtc::ffi::{MediaType, RtpTransceiverDirection};

use super::media::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::tunnel::TUNNEL_STATE;

pub enum LocalEvent {
    IceCandidate {
        candidate: String,
        sdp_mid: String,
        sdp_mline_index: i32,
    },
    IceState(IceConnectionState),
    ConnectionState(PeerConnectionState),
    RemoteTrack(SharedPtr<RtpTransceiver>),
}

pub struct Observer {
    pub tx: mpsc::UnboundedSender<LocalEvent>,
}

impl PeerConnectionObserver for Observer {
    fn on_signaling_change(&self, _new_state: SignalingState) {}
    fn on_add_stream(&self, _stream: SharedPtr<MediaStream>) {}
    fn on_remove_stream(&self, _stream: SharedPtr<MediaStream>) {}
    fn on_data_channel(
        &self,
        _data_channel: SharedPtr<webrtc_sys::data_channel::ffi::DataChannel>,
    ) {
    }
    fn on_renegotiation_needed(&self) {}
    fn on_negotiation_needed_event(&self, _event: u32) {}

    fn on_ice_connection_change(&self, new_state: IceConnectionState) {
        let _ = self.tx.send(LocalEvent::IceState(new_state));
    }

    fn on_standardized_ice_connection_change(&self, _new_state: IceConnectionState) {}

    fn on_connection_change(&self, new_state: PeerConnectionState) {
        let _ = self.tx.send(LocalEvent::ConnectionState(new_state));
    }

    fn on_ice_gathering_change(&self, _new_state: IceGatheringState) {}

    fn on_ice_candidate(&self, candidate: SharedPtr<IceCandidate>) {
        let candidate_sdp = candidate.candidate();
        if candidate_sdp.is_empty() {
            return;
        }
        let _ = self.tx.send(LocalEvent::IceCandidate {
            candidate: candidate_sdp,
            sdp_mid: candidate.sdp_mid(),
            sdp_mline_index: candidate.sdp_mline_index(),
        });
    }

    fn on_ice_candidate_error(
        &self,
        address: String,
        port: i32,
        url: String,
        error_code: i32,
        error_text: String,
    ) {
        warn!(
            "ICE candidate error address={address}:{port} url={url} code={error_code}: {error_text}"
        );
    }

    fn on_ice_candidates_removed(
        &self,
        _removed: Vec<SharedPtr<webrtc_sys::candidate::ffi::Candidate>>,
    ) {
    }
    fn on_ice_connection_receiving_change(&self, _receiving: bool) {}
    fn on_ice_selected_candidate_pair_changed(&self, _event: CandidatePairChangeEvent) {}
    fn on_add_track(
        &self,
        _receiver: SharedPtr<RtpReceiver>,
        _streams: Vec<SharedPtr<MediaStream>>,
    ) {
    }

    fn on_track(&self, transceiver: SharedPtr<RtpTransceiver>) {
        let _ = self.tx.send(LocalEvent::RemoteTrack(transceiver));
    }

    fn on_remove_track(&self, _receiver: SharedPtr<RtpReceiver>) {}
    fn on_interesting_usage(&self, _usage_pattern: i32) {}
}

/// Mean-square threshold for remote VAD (−40 dBFS, i16 scale).
const REMOTE_VAD_THRESHOLD_SQ: i64 = 107_334; // (0.01 × 32768)²

pub struct VadAudioSink;

impl webrtc_sys::audio_track::AudioSink for VadAudioSink {
    fn on_data(&self, data: &[i16], _sample_rate: i32, _nb_channels: usize, _nb_frames: usize) {
        let n = data.len();
        if n == 0 {
            return;
        }
        let sum_sq: i64 = data.iter().map(|&s| (s as i64) * (s as i64)).sum();
        if sum_sq / n as i64 >= REMOTE_VAD_THRESHOLD_SQ
            && let Some(state) = TUNNEL_STATE.get()
        {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            state
                .remote_vad_last_speech_ms
                .store(now_ms, Ordering::Relaxed);
        }
    }
}

pub struct DropVideoSink;

impl webrtc_sys::video_track::VideoSink for DropVideoSink {
    fn on_frame(&self, _frame: UniquePtr<VideoFrame>) {}
    fn on_discarded_frame(&self) {}
    fn on_constraints_changed(&self, _constraints: VideoTrackSourceConstraints) {}
}

pub struct MediaHandles {
    pub audio_source: SharedPtr<AudioTrackSource>,
    pub video_source: SharedPtr<VideoTrackSource>,
    pub audio_track: SharedPtr<AudioTrack>,
    pub video_track: SharedPtr<VideoTrack>,
    pub _audio_sink: SharedPtr<NativeAudioSink>,
    pub _video_sink: SharedPtr<NativeVideoSink>,
}

pub struct WebrtcCall {
    pub factory: SharedPtr<PeerConnectionFactory>,
    pub pc: SharedPtr<PeerConnection>,
    pub events: mpsc::UnboundedReceiver<LocalEvent>,
    pub media: MediaHandles,
    pub local_ufrag: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Caller,
    Calltaker,
}

impl WebrtcCall {
    pub fn new(turn: &TurnServer, role: Role, video_width: u32, video_height: u32) -> Result<Self> {
        let (tx, events) = mpsc::unbounded_channel();
        let factory = create_peer_connection_factory();
        let config = RtcConfiguration {
            ice_servers: vec![IceServer {
                urls: turn.urls.clone(),
                username: turn.username.clone(),
                password: turn.credential.clone(),
            }],
            continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
            ice_transport_type: IceTransportsType::Relay,
        };
        let observer = PeerConnectionObserverWrapper::new(Arc::new(Observer { tx }));
        let pc = factory
            .create_peer_connection(config, Box::new(observer))
            .map_err(|e| anyhow!("create peer connection: {e}"))?;

        let audio_source = new_audio_track_source(
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            AUDIO_SAMPLE_RATE as i32,
            AUDIO_CHANNELS as i32,
            100,
        );
        let audio_track =
            factory.create_audio_track("anazoa-audio".to_string(), audio_source.clone());
        let video_source = new_video_track_source(
            &VideoResolution {
                width: video_width,
                height: video_height,
            },
            false,
        );
        let video_track =
            factory.create_video_track("anazoa-video".to_string(), video_source.clone());
        video_track.set_content_hint(ContentHint::Fluid);

        if role == Role::Caller {
            let audio_transceiver = pc
                .add_transceiver(
                    audio_to_media(audio_track.clone()),
                    transceiver_init(RtpTransceiverDirection::SendRecv),
                )
                .map_err(|e| anyhow!("add audio transceiver: {e}"))?;
            let video_transceiver = pc
                .add_transceiver(
                    video_to_media(video_track.clone()),
                    transceiver_init(RtpTransceiverDirection::SendRecv),
                )
                .map_err(|e| anyhow!("add video transceiver: {e}"))?;
            prefer_codec(
                factory.rtp_sender_capabilities(MediaType::Audio).codecs,
                &audio_transceiver,
                "opus",
            )?;
            prefer_codec(
                factory.rtp_sender_capabilities(MediaType::Video).codecs,
                &video_transceiver,
                "VP9",
            )?;
        }

        Ok(Self {
            factory,
            pc,
            events,
            media: MediaHandles {
                audio_source,
                video_source,
                audio_track,
                video_track,
                _audio_sink: new_native_audio_sink(
                    Box::new(AudioSinkWrapper::new(Arc::new(VadAudioSink))),
                    AUDIO_SAMPLE_RATE as i32,
                    AUDIO_CHANNELS as i32,
                ),
                _video_sink: new_native_video_sink(Box::new(VideoSinkWrapper::new(Arc::new(
                    DropVideoSink,
                )))),
            },
            local_ufrag: None,
        })
    }
}

pub fn transceiver_init(direction: RtpTransceiverDirection) -> RtpTransceiverInit {
    RtpTransceiverInit {
        direction,
        stream_ids: vec!["anazoa".to_string()],
        send_encodings: Vec::new(),
    }
}

pub fn prefer_codec(
    codecs: Vec<RtpCodecCapability>,
    transceiver: &SharedPtr<RtpTransceiver>,
    name: &str,
) -> Result<()> {
    let wanted = name.to_ascii_lowercase();
    let (mut selected, rest): (Vec<_>, Vec<_>) = codecs.into_iter().partition(|codec| {
        codec.name.eq_ignore_ascii_case(name)
            || codec
                .mime_type
                .to_ascii_lowercase()
                .ends_with(&format!("/{wanted}"))
    });
    if selected.is_empty() {
        warn!("codec {name} not found in libwebrtc capabilities");
        return Ok(());
    }
    selected.extend(rest);
    transceiver
        .set_codec_preferences(selected)
        .map_err(|e| anyhow!("set {name} codec preferences: {e}"))
}

pub fn ensure_local_senders(call: &WebrtcCall, context: &str) -> Result<()> {
    for transceiver in call.pc.get_transceivers() {
        let transceiver = transceiver.ptr;
        let media_type = transceiver.media_type();
        transceiver
            .set_direction(RtpTransceiverDirection::SendRecv)
            .map_err(|e| anyhow!("set {media_type:?} transceiver sendrecv {context}: {e}"))?;

        let sender = transceiver.sender();
        let attached = match media_type {
            MediaType::Audio => {
                prefer_codec(
                    call.factory
                        .rtp_sender_capabilities(MediaType::Audio)
                        .codecs,
                    &transceiver,
                    "opus",
                )?;
                sender.set_track(audio_to_media(call.media.audio_track.clone()))
            }
            MediaType::Video => {
                prefer_codec(
                    call.factory
                        .rtp_sender_capabilities(MediaType::Video)
                        .codecs,
                    &transceiver,
                    "VP9",
                )?;
                sender.set_track(video_to_media(call.media.video_track.clone()))
            }
            _ => true,
        };
        if !attached {
            warn!("failed to attach local {media_type:?} track {context}");
        }

        sender.set_streams(&vec!["anazoa".to_string()]);
        log_transceiver_sender(&transceiver, context);
    }
    Ok(())
}

pub fn log_local_senders(call: &WebrtcCall, context: &str) {
    for transceiver in call.pc.get_transceivers() {
        log_transceiver_sender(&transceiver.ptr, context);
    }
}

pub fn log_transceiver_sender(transceiver: &SharedPtr<RtpTransceiver>, context: &str) {
    let sender = transceiver.sender();
    let mid = transceiver.mid().unwrap_or_else(|_| "<none>".to_string());
    debug!(
        media = ?transceiver.media_type(),
        direction = ?transceiver.direction(),
        mid,
        ssrc = sender.ssrc(),
        id = sender.id(),
        "{context}: local sender"
    );
}

pub fn attach_remote_sinks(transceiver: SharedPtr<RtpTransceiver>, media: &mut MediaHandles) {
    let receiver = transceiver.receiver();
    match receiver.media_type() {
        MediaType::Audio => unsafe {
            let track = media_to_audio(receiver.track());
            track.add_sink(&media._audio_sink);
            info!("attached VAD audio sink");
        },
        MediaType::Video => unsafe {
            let track = media_to_video(receiver.track());
            track.add_sink(&media._video_sink);
            track.set_should_receive(true);
            info!("attached dropping video sink");
        },
        other => debug!("ignoring remote track type {other:?}"),
    }
}

pub fn attach_existing_remote_tracks(call: &mut WebrtcCall) {
    for transceiver in call.pc.get_transceivers() {
        attach_remote_sinks(transceiver.ptr, &mut call.media);
    }
}

pub fn ice_state_name(state: IceConnectionState) -> &'static str {
    match state {
        IceConnectionState::IceConnectionNew => "new",
        IceConnectionState::IceConnectionChecking => "checking",
        IceConnectionState::IceConnectionConnected => "connected",
        IceConnectionState::IceConnectionCompleted => "completed",
        IceConnectionState::IceConnectionFailed => "failed",
        IceConnectionState::IceConnectionDisconnected => "disconnected",
        IceConnectionState::IceConnectionClosed => "closed",
        IceConnectionState::IceConnectionMax => "max",
        _ => "unknown",
    }
}

pub fn pc_state_name(state: PeerConnectionState) -> &'static str {
    match state {
        PeerConnectionState::New => "new",
        PeerConnectionState::Connecting => "connecting",
        PeerConnectionState::Connected => "connected",
        PeerConnectionState::Disconnected => "disconnected",
        PeerConnectionState::Failed => "failed",
        PeerConnectionState::Closed => "closed",
        _ => "unknown",
    }
}

pub async fn create_offer(pc: &SharedPtr<PeerConnection>) -> Result<UniquePtr<SessionDescription>> {
    let (tx, rx) = oneshot::channel::<std::result::Result<UniquePtr<SessionDescription>, String>>();
    pc.create_offer(
        RtcOfferAnswerOptions::default(),
        Box::new(PeerContext(Box::new(tx))),
        sdp_success,
        sdp_error,
    );
    rx.await
        .context("create_offer callback dropped")?
        .map_err(|e| anyhow!(e))
}

pub async fn create_answer(
    pc: &SharedPtr<PeerConnection>,
) -> Result<UniquePtr<SessionDescription>> {
    let (tx, rx) = oneshot::channel::<std::result::Result<UniquePtr<SessionDescription>, String>>();
    pc.create_answer(
        RtcOfferAnswerOptions::default(),
        Box::new(PeerContext(Box::new(tx))),
        sdp_success,
        sdp_error,
    );
    rx.await
        .context("create_answer callback dropped")?
        .map_err(|e| anyhow!(e))
}

pub async fn set_local_description(
    pc: &SharedPtr<PeerConnection>,
    desc: UniquePtr<SessionDescription>,
) -> Result<()> {
    let (tx, rx) = oneshot::channel::<std::result::Result<(), String>>();
    pc.set_local_description(desc, Box::new(PeerContext(Box::new(tx))), op_complete);
    rx.await
        .context("set_local_description callback dropped")?
        .map_err(|e| anyhow!(e))
}

pub async fn set_remote_sdp(
    pc: &SharedPtr<PeerConnection>,
    sdp_type: SdpType,
    sdp: &str,
) -> Result<()> {
    let desc = create_session_description(sdp_type, sdp.to_string())
        .map_err(|e| anyhow!("parse remote SDP: {e}"))?;
    let (tx, rx) = oneshot::channel::<std::result::Result<(), String>>();
    pc.set_remote_description(desc, Box::new(PeerContext(Box::new(tx))), op_complete);
    rx.await
        .context("set_remote_description callback dropped")?
        .map_err(|e| anyhow!(e))
}

pub async fn add_ice_candidate(
    pc: &SharedPtr<PeerConnection>,
    candidate: SharedPtr<IceCandidate>,
) -> Result<()> {
    let (tx, rx) = oneshot::channel::<std::result::Result<(), String>>();
    pc.add_ice_candidate(candidate, Box::new(PeerContext(Box::new(tx))), op_complete);
    rx.await
        .context("add_ice_candidate callback dropped")?
        .map_err(|e| anyhow!(e))
}

#[allow(clippy::boxed_local)]
fn sdp_success(ctx: Box<PeerContext>, sdp: UniquePtr<SessionDescription>) {
    let PeerContext(inner) = *ctx;
    if let Ok(tx) = inner
        .downcast::<oneshot::Sender<std::result::Result<UniquePtr<SessionDescription>, String>>>()
    {
        let _ = tx.send(Ok(sdp));
    }
}

#[allow(clippy::boxed_local)]
fn sdp_error(ctx: Box<PeerContext>, error: RtcError) {
    let PeerContext(inner) = *ctx;
    if let Ok(tx) = inner
        .downcast::<oneshot::Sender<std::result::Result<UniquePtr<SessionDescription>, String>>>()
    {
        let _ = tx.send(Err(error.to_string()));
    }
}

#[allow(clippy::boxed_local)]
fn op_complete(ctx: Box<PeerContext>, error: RtcError) {
    let PeerContext(inner) = *ctx;
    if let Ok(tx) = inner.downcast::<oneshot::Sender<std::result::Result<(), String>>>() {
        let _ = tx.send(if error.ok() {
            Ok(())
        } else {
            Err(error.to_string())
        });
    }
}

pub extern "C" fn audio_capture_complete(_ctx: *const webrtc_sys::audio_track::SourceContext) {}
