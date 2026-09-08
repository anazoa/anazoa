use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use serde_json::json;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tracing::debug;
use unicode_truncate::UnicodeTruncateStr;
use wtransport::config::QuicTransportConfig;
use wtransport::tls::WEBTRANSPORT_ALPN;
use wtransport::{ClientConfig, Endpoint};

const CUSTOM_DATA_INTERVAL: Duration = Duration::from_secs(5);
const SIGNALING_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTION_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPTED_CALL_TIMEOUT: Duration = Duration::from_secs(60);

use crate::{
    FingerprintConfig, ServiceEndpoints, calls::StartedConversationInfo, oneme::IncomingCall,
};

// ── Wire abstraction ─────────────────────────────────────────────────────────

enum SignalingWire {
    Wt(WtConn),
}

struct WtConn {
    conn: wtransport::Connection,
    send: wtransport::SendStream,
    recv: wtransport::RecvStream,
}

impl SignalingWire {
    /// Send one text message (compressed with deflate-raw for WT).
    async fn send_text(&mut self, text: &str) -> Result<()> {
        match self {
            SignalingWire::Wt(wt) => {
                let compressed = deflate_compress(text.as_bytes())?;
                let len = encode_quic_varint(
                    u64::try_from(compressed.len())
                        .context("WT frame length does not fit into u64")?,
                )?;
                wt.send
                    .write_all(&len)
                    .await
                    .map_err(|e| anyhow!("WT write length: {e}"))?;
                wt.send
                    .write_all(&compressed)
                    .await
                    .map_err(|e| anyhow!("WT write payload: {e}"))?;
                wt.send
                    .flush()
                    .await
                    .map_err(|e| anyhow!("WT flush: {e}"))?;
            }
        }
        Ok(())
    }

    /// Receive the next text message over WebTransport.
    async fn recv_text(&mut self) -> Result<Option<String>> {
        match self {
            SignalingWire::Wt(wt) => {
                let len = read_quic_varint(&mut wt.recv).await?;
                let mut buf = vec![0u8; usize::try_from(len).context("WT frame length too large")?];
                wt.recv
                    .read_exact(&mut buf)
                    .await
                    .map_err(|e| anyhow!("WT recv read_exact: {e}"))?;
                let text = String::from_utf8(deflate_decompress(&buf)?)
                    .context("WT recv message not valid UTF-8")?;
                Ok(Some(text))
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        match self {
            SignalingWire::Wt(wt) => {
                wt.conn.close(wtransport::VarInt::from_u32(0), b"");
            }
        }
        Ok(())
    }
}

// ── Compression helpers ──────────────────────────────────────────────────────

fn deflate_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).context("deflate compress write")?;
    enc.finish().context("deflate compress finish")
}

fn deflate_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut dec = DeflateDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).context("deflate decompress")?;
    Ok(out)
}

fn encode_quic_varint(value: u64) -> Result<Vec<u8>> {
    if value >= (1 << 62) {
        bail!("WT frame too large for QUIC varint: {value}");
    }
    let (size, prefix) = if value < (1 << 6) {
        (1usize, 0x00)
    } else if value < (1 << 14) {
        (2usize, 0x40)
    } else if value < (1 << 30) {
        (4usize, 0x80)
    } else {
        (8usize, 0xc0)
    };

    let mut out = value.to_be_bytes()[8 - size..].to_vec();
    out[0] |= prefix;
    Ok(out)
}

async fn read_quic_varint(recv: &mut wtransport::RecvStream) -> Result<u64> {
    let mut first = [0u8; 1];
    recv.read_exact(&mut first)
        .await
        .map_err(|e| anyhow!("WT recv read frame prefix: {e}"))?;
    let size = match first[0] >> 6 {
        0 => 1usize,
        1 => 2usize,
        2 => 4usize,
        _ => 8usize,
    };
    let mut buf = [0u8; 8];
    buf[0] = first[0] & 0x3f;
    if size > 1 {
        recv.read_exact(&mut buf[1..size])
            .await
            .map_err(|e| anyhow!("WT recv read frame length: {e}"))?;
    }
    let value = match size {
        1 => u64::from(buf[0]),
        2 => u64::from(u16::from_be_bytes([buf[0], buf[1]])),
        4 => u64::from(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])),
        8 => u64::from_be_bytes(buf),
        _ => unreachable!(),
    };
    Ok(value)
}

// ── Connect helpers ──────────────────────────────────────────────────────────

async fn connect_signaling(url: &str, skip_tls_verify: bool) -> Result<SignalingWire> {
    let wt_url = if let Some(rest) = url.strip_prefix("wss://") {
        // Production WT signaling is on port 23432 at path /wt.
        // Extract just the host from the wss:// authority and carry over the query string.
        let (authority, path_and_query) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.split(':').next().unwrap_or(authority);
        let query = path_and_query.split_once('?').map(|(_, q)| q).unwrap_or("");
        let wt = if query.is_empty() {
            format!("https://{host}:23432/wt")
        } else {
            format!("https://{host}:23432/wt?{query}")
        };
        std::borrow::Cow::Owned(wt)
    } else {
        std::borrow::Cow::Borrowed(url)
    };
    if !wt_url.starts_with("https://") {
        bail!(
            "unsupported signaling transport: expected WebTransport https:// endpoint, got {url}"
        );
    }
    tracing::info!("connecting to signaling via WebTransport");
    let (conn, send, recv) = connect_wt(&wt_url, skip_tls_verify).await?;
    Ok(SignalingWire::Wt(WtConn { conn, send, recv }))
}

/// Build a WebTransport endpoint configured to match the Android Kwik QUIC fingerprint.
async fn connect_wt(
    url: &str,
    skip_tls_verify: bool,
) -> Result<(
    wtransport::Connection,
    wtransport::SendStream,
    wtransport::RecvStream,
)> {
    let mut tc = QuicTransportConfig::default();
    // Match Kwik 0.10.6 transport parameters observed in the Android Max pcap.
    tc.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_millis(60_000))
            .map_err(|_| anyhow!("invalid idle timeout"))?,
    ));
    tc.receive_window(quinn::VarInt::from_u32(2_500_000));
    tc.stream_receive_window(quinn::VarInt::from_u32(250_000));
    tc.max_concurrent_bidi_streams(quinn::VarInt::from_u32(100));
    tc.max_concurrent_uni_streams(quinn::VarInt::from_u32(103));
    // max_ack_delay=25ms is the QUIC default; no override needed

    crate::ensure_rustls_provider();
    let config = build_wt_client_config(tc, skip_tls_verify);

    let endpoint = Endpoint::client(config).context("create WT endpoint")?;

    let conn = timeout(SIGNALING_CONNECT_TIMEOUT, endpoint.connect(url))
        .await
        .map_err(|_| {
            anyhow!(
                "timed out connecting to WT endpoint after {}s",
                SIGNALING_CONNECT_TIMEOUT.as_secs()
            )
        })?
        .context("connect to WT endpoint")?;

    let (send, recv) = timeout(SIGNALING_CONNECT_TIMEOUT, async {
        conn.open_bi()
            .await
            .map_err(|e| anyhow!("WT open_bi: {e}"))?
            .await
            .map_err(|e| anyhow!("WT bidi stream open: {e}"))
    })
    .await
    .map_err(|_| {
        anyhow!(
            "timed out opening WT bidi stream after {}s",
            SIGNALING_CONNECT_TIMEOUT.as_secs()
        )
    })??;

    Ok((conn, send, recv))
}

fn build_wt_client_config(tc: QuicTransportConfig, skip_tls_verify: bool) -> ClientConfig {
    if skip_tls_verify {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("valid TLS 1.3 client config")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        tls.dangerous()
            .set_certificate_verifier(Arc::new(SkipTlsVerifier));
        tls.alpn_protocols = vec![WEBTRANSPORT_ALPN.to_vec()];
        return ClientConfig::builder()
            .with_bind_default()
            .with_custom_tls_and_transport(tls, tc)
            .build();
    }

    ClientConfig::builder()
        .with_bind_default()
        .with_custom_transport(tc)
        .build()
}

// ── Public client ────────────────────────────────────────────────────────────

pub struct SignalingClient {
    wire: SignalingWire,
    participant_id: i64,
    sequence: i32,
    pending_add_participant_enable: bool,
    pending_messages: VecDeque<Value>,
}

static LOG_SIGNALING_WS: AtomicBool = AtomicBool::new(false);

pub fn set_log_signaling_ws(enabled: bool) {
    LOG_SIGNALING_WS.store(enabled, Ordering::Relaxed);
}

fn log_signaling_ws(dir: &str, payload: &str) {
    if LOG_SIGNALING_WS.load(Ordering::Relaxed) {
        tracing::info!("signaling-ws {dir} {payload}");
    }
}

impl SignalingClient {
    pub async fn from_incoming(
        incoming_call: &IncomingCall,
        signaling_user_id: &str,
        endpoints: &ServiceEndpoints,
        fingerprint: &FingerprintConfig,
    ) -> Result<Self> {
        let peer_id = rand::random::<u64>() >> 1;
        let endpoint = calltaker_signaling_endpoint(
            &incoming_call.signaling.url,
            signaling_user_id,
            fingerprint,
            &incoming_call.conversation_id,
            peer_id,
            &incoming_call.signaling.token,
        );

        tracing::info!("Connecting to signaling (calltaker)");
        let wire = connect_signaling(&endpoint, endpoints.skip_tls_verify)
        .await?;
        let mut raw = RawSignalingClient {
            wire,
            pending_messages: VecDeque::new(),
        };
        let hello = raw.receive_connection_notification().await?;
        let participant_id = hello
            .conversation
            .participants
            .into_iter()
            .find(|p| p.external_id.id == incoming_call.caller_id.to_string())
            .map(|p| p.id)
            .ok_or_else(|| anyhow!("could not find remote signaling participant"))?;

        Ok(Self {
            wire: raw.wire,
            participant_id,
            sequence: 0,
            pending_add_participant_enable: false,
            pending_messages: raw.pending_messages,
        })
    }

    pub async fn from_outgoing(
        started: &StartedConversationInfo,
        calltaker_id: &str,
        signaling_user_id: &str,
        endpoints: &ServiceEndpoints,
        fingerprint: &FingerprintConfig,
    ) -> Result<Self> {
        let url = if let Some(wt) = &started.wt_endpoint {
            signaling_endpoint(wt, signaling_user_id, fingerprint)
        } else {
            signaling_endpoint(&started.endpoint, signaling_user_id, fingerprint)
        };
        tracing::debug!("signaling endpoint (caller): {url}");

        tracing::info!("Connecting to signaling (caller)");
        let wire = connect_signaling(&url, endpoints.skip_tls_verify)
        .await?;
        let mut raw = RawSignalingClient {
            wire,
            pending_messages: VecDeque::new(),
        };
        let hello = raw.receive_connection_notification().await?;
        let participant_id = hello
            .conversation
            .participants
            .into_iter()
            .find(|p| p.external_id.id == calltaker_id)
            .map(|p| p.id)
            .ok_or_else(|| anyhow!("could not find remote signaling participant"))?;

        Ok(Self {
            wire: raw.wire,
            participant_id,
            sequence: 0,
            pending_add_participant_enable: true,
            pending_messages: raw.pending_messages,
        })
    }

    /// Drain messages until `accepted-call` arrives and `enable-feature-for-roles` is sent.
    /// No-op if this client is not the caller (flag already false).
    pub async fn wait_for_accepted_call(&mut self) -> Result<()> {
        timeout(ACCEPTED_CALL_TIMEOUT, async {
            while self.pending_add_participant_enable {
                let msg = self.receive_json_value().await?;
                maybe_log_signaling_notification(&msg);
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            anyhow!(
                "timed out waiting for accepted-call notification after {}s",
                ACCEPTED_CALL_TIMEOUT.as_secs()
            )
        })?
    }

    pub async fn send_signal<T: Serialize>(&mut self, data: T) -> Result<()> {
        let text = serde_json::to_string(&TransmitData {
            command: "transmit-data",
            sequence: self.next_sequence(),
            participant_id: self.participant_id,
            data,
            participant_type: "USER",
        })?;
        log_signaling_ws("tx", &text);
        self.wire
            .send_text(&text)
            .await
            .context("send signaling message")?;
        Ok(())
    }

    pub async fn send_update_media_modifiers(&mut self) -> Result<()> {
        let text = self.command_text(&UpdateMediaModifiers {
            command: "update-media-modifiers",
            media_modifiers: MediaModifiers::default(),
        })?;
        self.send_command_text(text, "send update-media-modifiers")
            .await
    }

    pub async fn send_accept_call(&mut self) -> Result<()> {
        let text = self.command_text(&AcceptCall {
            command: "accept-call",
            media_settings: MediaSettings::with_video(),
        })?;
        self.send_command_text(text, "send accept-call").await
    }

    pub async fn send_get_rooms(&mut self) -> Result<()> {
        let text = self.command_text(&GetRooms {
            command: "get-rooms",
            with_participants: false,
        })?;
        self.send_command_text(text, "send get-rooms").await
    }

    pub async fn send_change_media_settings(&mut self) -> Result<()> {
        let text = self.command_text(&ChangeMediaSettings {
            command: "change-media-settings",
            media_settings: MediaSettings::with_video(),
        })?;
        self.send_command_text(text, "send change-media-settings")
            .await
    }

    pub async fn send_change_participant_state(&mut self) -> Result<()> {
        let text = self.command_text(&ChangeParticipantState {
            command: "change-participant-state",
            participant_state: ParticipantState::empty(),
        })?;
        self.send_command_text(text, "send change-participant-state")
            .await
    }

    pub async fn receive_signal_value(&mut self) -> Result<Value> {
        self.receive_signal().await
    }

    pub async fn receive_signal<T: DeserializeOwned>(&mut self) -> Result<T> {
        loop {
            let msg = self.receive_json_value().await?;
            let msg_type = msg.get("type").and_then(Value::as_str).unwrap_or("?");
            if msg_type != "notification" {
                let seq = msg.get("sequence").and_then(|v| v.as_i64()).unwrap_or(-1);
                tracing::debug!("signaling rx skipped: type={msg_type} seq={seq}");
                continue;
            }
            let notif = msg
                .get("notification")
                .and_then(Value::as_str)
                .unwrap_or("?");
            if notif != "transmitted-data" {
                tracing::debug!("signaling rx skipped: notification={notif}");
                continue;
            }
            let data = msg
                .get("data")
                .cloned()
                .ok_or_else(|| anyhow!("transmitted-data missing data"))?;
            return serde_json::from_value(data).context("deserialize signaling data");
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        self.wire.close().await
    }

    pub async fn hangup(&mut self) -> Result<()> {
        let text = serde_json::to_string(&json!({
            "command": "hangup",
            "sequence": self.next_sequence(),
            "reason": "HUNGUP",
        }))?;
        log_signaling_ws("tx", &text);
        self.wire
            .send_text(&text)
            .await
            .context("send signaling hangup")?;
        Ok(())
    }

    pub async fn wait_for_call_end(&mut self) -> Result<()> {
        loop {
            match timeout(CUSTOM_DATA_INTERVAL, self.receive_json_value()).await {
                Ok(Ok(msg)) => {
                    maybe_log_signaling_notification(&msg);
                    if is_call_end_message(&msg) {
                        return Ok(());
                    }
                }
                Ok(Err(err)) => {
                    debug!("signaling call-end watcher exiting: {err:#}");
                    return Ok(());
                }
                Err(_) => {
                    if let Err(err) = self.send_custom_data().await {
                        debug!("custom-data send failed: {err:#}");
                    }
                }
            }
        }
    }

    pub async fn send_custom_data(&mut self) -> Result<()> {
        let sequence = self.next_sequence();
        let text = serde_json::to_string(&json!({
            "command": "custom-data",
            "sequence": sequence,
            "data": {"sdk": {"type": "bad-net", "rtt": 2, "loss": 0}},
            "participantId": null,
        }))?;
        log_signaling_ws("tx", &text);
        self.wire
            .send_text(&text)
            .await
            .context("send custom-data")?;
        Ok(())
    }

    async fn receive_json_value(&mut self) -> Result<Value> {
        if let Some(value) = self.pending_messages.pop_front() {
            self.handle_post_receive(&value).await?;
            return Ok(value);
        }

        loop {
            let text = self
                .wire
                .recv_text()
                .await?
                .ok_or_else(|| anyhow!("signaling connection closed"))?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.eq_ignore_ascii_case("ping") {
                debug!("received signaling ping");
                log_signaling_ws("rx", "ping");
                self.wire
                    .send_text("pong")
                    .await
                    .context("send signaling pong")?;
                log_signaling_ws("tx", "pong");
                continue;
            }
            log_signaling_ws("rx", trimmed);
            let value: Value = serde_json::from_str(trimmed).with_context(|| {
                let (short, len) = trimmed.unicode_truncate(200);
                format!(
                    "parse signaling message: {}{}",
                    short,
                    if len < trimmed.len() { "..." } else { "" }
                )
            })?;
            self.handle_post_receive(&value).await?;
            return Ok(value);
        }
    }

    fn next_sequence(&mut self) -> i32 {
        self.sequence += 1;
        self.sequence
    }

    fn command_text<T: Serialize>(&mut self, value: &T) -> Result<String> {
        let mut json = serde_json::to_value(value).context("serialize signaling command")?;
        if let Some(obj) = json.as_object_mut() {
            obj.insert("sequence".to_string(), json!(self.next_sequence()));
        }
        serde_json::to_string(&json).context("serialize signaling command text")
    }

    async fn send_command_text(&mut self, text: String, context: &'static str) -> Result<()> {
        log_signaling_ws("tx", &text);
        self.wire.send_text(&text).await.context(context)
    }

    async fn handle_post_receive(&mut self, msg: &Value) -> Result<()> {
        if self.pending_add_participant_enable
            && msg.get("notification").and_then(Value::as_str) == Some("accepted-call")
        {
            let sequence = self.next_sequence();
            let text = serde_json::to_string(&EnableFeatureForRoles {
                command: "enable-feature-for-roles",
                sequence,
                feature: "ADD_PARTICIPANT",
                roles: Vec::new(),
            })?;
            self.wire
                .send_text(&text)
                .await
                .context("send enable-feature-for-roles")?;
            self.pending_add_participant_enable = false;
        }
        Ok(())
    }
}

fn signaling_endpoint(base: &str, signaling_user_id: &str, fp: &FingerprintConfig) -> String {
    let device = fp.device_name.replace(' ', "%2F");
    let os_api = fp.os_api_level;
    let suffix = format!(
        "appVersion=sdk-0.1.10.1&capabilities=3c57f&clientType=ONE_ME\
         &compression=deflate-raw&device={device}\
         &ispAsOrg=null&locCc=null&locReg=null\
         &osVersion={os_api}&platform=ANDROID&userId={signaling_user_id}&version=5"
    );
    if base.contains('?') {
        format!("{base}&{suffix}")
    } else {
        format!("{base}?{suffix}")
    }
}

fn calltaker_signaling_endpoint(
    base: &str,
    signaling_user_id: &str,
    fp: &FingerprintConfig,
    conversation_id: &str,
    // Server expects a non-negative Java long; generated as rand::random::<u64>() >> 1.
    peer_id: u64,
    token: &str,
) -> String {
    let device = fp.device_name.replace(' ', "%2F");
    let os_api = fp.os_api_level;
    let sep = if base.contains('?') { '&' } else { '?' };
    format!(
        "{base}{sep}appVersion=sdk-0.1.10.1&capabilities=3c57f&clientType=ONE_ME\
         &compression=deflate-raw&conversationId={conversation_id}&device={device}\
         &entityType=USER&ispAsOrg=null&locCc=null&locReg=null\
         &osVersion={os_api}&peerId={peer_id}&platform=ANDROID\
         &tgt=accept&token={token}&userId={signaling_user_id}&version=5"
    )
}

struct RawSignalingClient {
    wire: SignalingWire,
    pending_messages: VecDeque<Value>,
}

impl RawSignalingClient {
    async fn receive_json_value(&mut self) -> Result<Value> {
        if let Some(value) = self.pending_messages.pop_front() {
            return Ok(value);
        }

        loop {
            let text = self
                .wire
                .recv_text()
                .await?
                .ok_or_else(|| anyhow!("signaling connection closed"))?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.eq_ignore_ascii_case("ping") {
                log_signaling_ws("rx", "ping");
                self.wire
                    .send_text("pong")
                    .await
                    .context("send signaling pong")?;
                log_signaling_ws("tx", "pong");
                continue;
            }
            log_signaling_ws("rx", trimmed);
            return serde_json::from_str(trimmed).with_context(|| {
                let (short, len) = trimmed.unicode_truncate(200);
                format!(
                    "parse signaling json: {}{}",
                    short,
                    if len < trimmed.len() { "..." } else { "" }
                )
            });
        }
    }

    async fn receive_connection_notification(&mut self) -> Result<ConnectionNotification> {
        timeout(CONNECTION_NOTIFICATION_TIMEOUT, async {
            loop {
                let value = self.receive_json_value().await?;
                let msg_type = value.get("type").and_then(Value::as_str).unwrap_or("?");
                let msg_notif = value
                    .get("notification")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if msg_type == "error" {
                    let s = value.to_string();
                    let (short, len) = s.unicode_truncate(200);
                    bail!(
                        "signaling server returned error: {}{}",
                        short,
                        if len < s.len() { "..." } else { "" }
                    );
                }
                tracing::debug!(
                    "signaling rx before connection: type={msg_type} notification={msg_notif}"
                );
                if msg_type == "notification" && msg_notif == "connection" {
                    return serde_json::from_value(value)
                        .context("deserialize connection notification");
                }
            }
        })
        .await
        .map_err(|_| {
            anyhow!(
                "timed out waiting for signaling connection notification after {}s",
                CONNECTION_NOTIFICATION_TIMEOUT.as_secs()
            )
        })?
    }
}

fn is_call_end_message(msg: &Value) -> bool {
    matches!(msg.get("response").and_then(Value::as_str), Some("hangup"))
        || matches!(
            msg.get("notification").and_then(Value::as_str),
            Some(
                "hangup"
                    | "hungup"
                    | "closed-conversation"
                    | "call-ended"
                    | "participant-left"
                    | "left-call"
                    | "terminated-call"
            )
        )
}

fn maybe_log_signaling_notification(msg: &Value) {
    if let Some(
        "registered-peer" | "accepted-call" | "feature-set-changed" | "features-per-role-changed",
    ) = msg.get("notification").and_then(Value::as_str)
    {
        let s = msg.to_string();
        let (short, len) = s.unicode_truncate(200);
        debug!(
            "signaling notification: {}{}",
            short,
            if len < s.len() { "..." } else { "" }
        );
    }
}

use crate::SkipTlsVerifier;

#[derive(Debug, Deserialize)]
struct ConnectionNotification {
    conversation: Conversation,
}

#[derive(Debug, Deserialize)]
struct Conversation {
    participants: Vec<Participant>,
}

#[derive(Debug, Deserialize)]
struct Participant {
    #[serde(rename = "externalId")]
    external_id: ParticipantExternalId,
    id: i64,
}

#[derive(Debug, Deserialize)]
struct ParticipantExternalId {
    id: String,
}

#[derive(Serialize)]
struct TransmitData<T> {
    command: &'static str,
    sequence: i32,
    #[serde(rename = "participantId")]
    participant_id: i64,
    data: T,
    #[serde(rename = "participantType")]
    participant_type: &'static str,
}

#[derive(Serialize)]
struct UpdateMediaModifiers {
    command: &'static str,
    #[serde(rename = "mediaModifiers")]
    media_modifiers: MediaModifiers,
}

#[derive(Serialize)]
struct MediaModifiers {
    denoise: bool,
    #[serde(rename = "denoiseAnn")]
    denoise_ann: bool,
}

impl Default for MediaModifiers {
    fn default() -> Self {
        Self {
            denoise: true,
            denoise_ann: true,
        }
    }
}

#[derive(Serialize)]
struct AcceptCall {
    command: &'static str,
    #[serde(rename = "mediaSettings")]
    media_settings: MediaSettings,
}

#[derive(Serialize)]
struct ChangeMediaSettings {
    command: &'static str,
    #[serde(rename = "mediaSettings")]
    media_settings: MediaSettings,
}

#[derive(Serialize)]
struct GetRooms {
    command: &'static str,
    #[serde(rename = "withParticipants")]
    with_participants: bool,
}

#[derive(Serialize)]
struct ChangeParticipantState {
    command: &'static str,
    #[serde(rename = "participantState")]
    participant_state: ParticipantState,
}

#[derive(Serialize)]
struct ParticipantState {
    state: Value,
}

impl ParticipantState {
    fn empty() -> Self {
        Self {
            state: Value::Object(Default::default()),
        }
    }
}

#[derive(Serialize)]
struct EnableFeatureForRoles {
    command: &'static str,
    sequence: i32,
    feature: &'static str,
    roles: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaSettings {
    is_audio_enabled: bool,
    is_video_enabled: bool,
    is_screen_sharing_enabled: bool,
    is_fast_screen_sharing_enabled: bool,
    is_audio_sharing_enabled: bool,
    is_animoji_enabled: bool,
}

impl MediaSettings {
    fn with_video() -> Self {
        Self {
            is_audio_enabled: true,
            is_video_enabled: true,
            is_screen_sharing_enabled: false,
            is_fast_screen_sharing_enabled: false,
            is_audio_sharing_enabled: false,
            is_animoji_enabled: false,
        }
    }
}
