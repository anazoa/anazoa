use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use hyper::Uri;
use serde::Deserialize;
use serde_json::{Map, Number, Value};
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::timeout;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::debug;
use unicode_truncate::UnicodeTruncateStr;
use uuid::Uuid;
use webpki_roots::TLS_SERVER_ROOTS;

use crate::calls::StartedConversationInfo;
use crate::{FingerprintConfig, ServiceEndpoints, TurnServer, ensure_rustls_provider};

const ONE_ME_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_ID_MIN: u32 = 30;
const SESSION_ID_RANGE: u32 = 171;
const MAX_FRAME_LEN: usize = 1 << 20;
const MAX_DECOMPRESSED_LEN: usize = 64 << 10;
const CMD_EVENT: u8 = 0;
const CMD_SUCCESS: u8 = 1;
const CMD_ERROR: u8 = 3;
const OPCODE_INTERACTIVE_HEARTBEAT: u16 = 1;
const OPCODE_CLIENT_HELLO: u16 = 6;
const OPCODE_START_AUTH: u16 = 17;
const OPCODE_CHECK_CODE: u16 = 18;
const OPCODE_CHAT_SYNC: u16 = 19;
const OPCODE_LOGIN_CHECK_PASSWORD: u16 = 115;
const OPCODE_START_OUTGOING_CALL: u16 = 78;
const OPCODE_INCOMING_CALL: u16 = 137;
const OPCODE_CALL_TOKEN_REQUEST: u16 = 158;

pub struct SessionClient {
    inner: OnemeClient,
}

impl SessionClient {
    pub async fn connect(
        endpoints: &ServiceEndpoints,
        keepalive_interval: Duration,
        fingerprint: &FingerprintConfig,
    ) -> Result<Self> {
        Ok(Self {
            inner: OnemeClient::with_endpoints(endpoints, keepalive_interval, fingerprint).await?,
        })
    }

    pub async fn establish(
        auth_token: &str,
        endpoints: &ServiceEndpoints,
        oneme_keepalive_secs: u64,
        fingerprint: &FingerprintConfig,
    ) -> Result<(Self, Option<i64>)> {
        let mut client = Self::connect(
            endpoints,
            Duration::from_secs(oneme_keepalive_secs),
            fingerprint,
        )
        .await?;
        client.do_chat_sync(auth_token).await?;
        let user_id = client.user_id();
        Ok((client, user_id))
    }

    pub async fn do_chat_sync(&mut self, token: &str) -> Result<()> {
        self.inner.do_chat_sync(token).await
    }

    pub async fn do_call_token_request(&mut self) -> Result<String> {
        self.inner.do_call_token_request().await
    }

    pub async fn do_verification_request(&mut self, phone: &str) -> Result<String> {
        self.inner.do_verification_request(phone).await
    }

    pub async fn do_code_enter(&mut self, token: &str, code: &str) -> Result<CodeOutcome> {
        self.inner.do_code_enter(token, code).await
    }

    pub async fn do_password_check(&mut self, track_id: &str, password: &str) -> Result<String> {
        self.inner.do_password_check(track_id, password).await
    }

    pub async fn wait_for_incoming_call(&mut self) -> Result<IncomingCall> {
        self.inner.wait_for_incoming_call().await
    }

    pub async fn send_interactive_heartbeat(&mut self) -> Result<()> {
        self.inner.send_interactive_heartbeat().await
    }

    pub async fn start_outgoing_call(
        &mut self,
        calltaker_id: i64,
    ) -> Result<StartedConversationInfo> {
        self.inner.start_outgoing_call(calltaker_id).await
    }

    pub fn user_id(&self) -> Option<i64> {
        self.inner.user_id
    }
}

#[derive(Debug)]
struct Packet {
    cmd: u8,
    seq: u16,
    opcode: u16,
    payload: Value,
}

/// Outcome of [`SessionClient::do_code_enter`].
pub enum CodeOutcome {
    /// The SMS code was enough — this is the LOGIN token.
    LoggedIn(String),
    /// The account has a login password ("2FA"); call
    /// [`SessionClient::do_password_check`] with `track_id` to finish.
    PasswordRequired {
        track_id: String,
        hint: Option<String>,
    },
}

pub struct OnemeClient {
    tls: TlsStream<TcpStream>,
    seq: u16,
    fingerprint: FingerprintConfig,
    client_session_id: u32,
    auth_token: Option<String>,
    user_id: Option<i64>,
    /// `callsSeed` from the SESSION_INIT response — the `seed` input to the
    /// START_AUTH integrity token (see `integrity`).
    calls_seed: Option<i64>,
    queue: VecDeque<Packet>,
    keepalive_interval: Duration,
}

impl OnemeClient {
    pub async fn with_endpoints(
        endpoints: &ServiceEndpoints,
        keepalive_interval: Duration,
        fingerprint: &FingerprintConfig,
    ) -> Result<Self> {
        ensure_rustls_provider();

        let (host, port) = oneme_host_port(&endpoints.oneme_api_url)?;
        let tcp = connect_tcp_stream(&host, port).await?;

        let config = build_tls_config(endpoints.skip_tls_verify);
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| anyhow!("invalid TLS server name: {host}"))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .with_context(|| format!("start TLS to {host}:{port}"))?;

        let client_session_id = rand::random::<u32>() % SESSION_ID_RANGE + SESSION_ID_MIN;
        let mut client = Self {
            tls,
            seq: 1,
            fingerprint: fingerprint.clone(),
            client_session_id,
            auth_token: None,
            user_id: None,
            calls_seed: None,
            queue: VecDeque::new(),
            keepalive_interval,
        };
        client.do_client_hello().await?;
        Ok(client)
    }

    async fn send(&mut self, opcode: u16, payload: Value) -> Result<u16> {
        let payload = encode_json_to_msgpack(&payload)?;
        self.send_bytes(opcode, payload).await
    }

    async fn send_bytes(&mut self, opcode: u16, payload: Vec<u8>) -> Result<u16> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);

        let len_field = payload.len() as u32;
        let mut buf = Vec::with_capacity(10 + payload.len());
        buf.push(10);
        buf.push(0);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&opcode.to_be_bytes());
        buf.extend_from_slice(&len_field.to_be_bytes());
        buf.extend_from_slice(&payload);
        timeout(ONE_ME_WRITE_TIMEOUT, self.tls.write_all(&buf))
            .await
            .map_err(|_| {
                anyhow!(
                    "OneMe write timed out after {}s",
                    ONE_ME_WRITE_TIMEOUT.as_secs()
                )
            })?
            .context("OneMe write")?;
        timeout(ONE_ME_WRITE_TIMEOUT, self.tls.flush())
            .await
            .map_err(|_| {
                anyhow!(
                    "OneMe flush timed out after {}s",
                    ONE_ME_WRITE_TIMEOUT.as_secs()
                )
            })?
            .context("OneMe flush")?;
        Ok(seq)
    }

    async fn recv_seq(&mut self, target_seq: u16, target_opcode: u16) -> Result<Packet> {
        if let Some(pos) = self
            .queue
            .iter()
            .position(|pkt| pkt.seq == target_seq && pkt.opcode == target_opcode)
        {
            return self
                .queue
                .remove(pos)
                .ok_or_else(|| anyhow!("queued packet disappeared"));
        }

        loop {
            let pkt = self.recv_packet().await?;
            if pkt.seq == target_seq && pkt.opcode == target_opcode {
                return Ok(pkt);
            }
            self.queue.push_back(pkt);
        }
    }

    fn expect_success_packet(packet: Packet) -> Result<Value> {
        match packet.cmd {
            CMD_SUCCESS => Ok(packet.payload),
            CMD_ERROR => bail!(
                "OneMe server error for opcode {}: {}",
                packet.opcode,
                format_server_error(&packet.payload)
            ),
            cmd => bail!(
                "unexpected OneMe response cmd={} for opcode {} payload={}",
                cmd,
                packet.opcode,
                truncate_json(&packet.payload)
            ),
        }
    }

    async fn recv_any(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.queue.pop_front() {
            return Ok(pkt);
        }
        self.recv_packet().await
    }

    async fn recv_packet(&mut self) -> Result<Packet> {
        loop {
            match timeout(self.keepalive_interval, self.read_packet_once()).await {
                Ok(pkt) => return pkt,
                Err(_) => {
                    debug!(
                        "sending OneMe idle interactive heartbeat after {:?} of inactivity",
                        self.keepalive_interval
                    );
                    self.send_interactive_heartbeat()
                        .await
                        .context("send idle OneMe interactive heartbeat")?;
                }
            }
        }
    }

    async fn read_packet_once(&mut self) -> Result<Packet> {
        let mut header = [0u8; 10];
        self.tls
            .read_exact(&mut header)
            .await
            .context("read OneMe header")?;

        let cmd = header[1];
        let seq = u16::from_be_bytes([header[2], header[3]]);
        let opcode = u16::from_be_bytes([header[4], header[5]]);
        let len_field = u32::from_be_bytes([header[6], header[7], header[8], header[9]]);
        let cof = ((len_field >> 24) as u8) as i8;
        let wire_len = (len_field & 0x00ff_ffff) as usize;
        if wire_len > MAX_FRAME_LEN {
            bail!("OneMe frame length {wire_len} exceeds limit of {MAX_FRAME_LEN} bytes");
        }

        let mut wire_payload = vec![0u8; wire_len];
        if wire_len > 0 {
            self.tls
                .read_exact(&mut wire_payload)
                .await
                .context("read OneMe payload")?;
        }

        let payload = if wire_payload.is_empty() {
            Value::Null
        } else {
            let decoded = decode_payload(cof, &wire_payload)?;
            parse_msgpack_json(&decoded)?
        };

        Ok(Packet {
            cmd,
            seq,
            opcode,
            payload,
        })
    }

    async fn do_client_hello(&mut self) -> Result<()> {
        let fp = &self.fingerprint;
        let seq = self
            .send(
                OPCODE_CLIENT_HELLO,
                serde_json::json!({
                    "mt_instanceid": Uuid::new_v4().to_string(),
                    "userAgent": {
                        "deviceType": "ANDROID",
                        "appVersion": crate::integrity::MAX_APP_VERSION,
                        "osVersion": fp.os_version,
                        "timezone": fp.timezone,
                        "screen": fp.screen,
                        "pushDeviceType": "GCM",
                        "arch": crate::integrity::MAX_ARCH,
                        "locale": fp.locale,
                        "buildNumber": crate::integrity::MAX_BUILD_NUMBER,
                        "deviceName": fp.device_name,
                        "deviceLocale": fp.device_locale
                    },
                    "clientSessionId": self.client_session_id,
                    "deviceId": fp.device_id,
                }),
            )
            .await?;
        let payload = Self::expect_success_packet(self.recv_seq(seq, OPCODE_CLIENT_HELLO).await?)?;
        debug!("SESSION_INIT response: {}", truncate_json(&payload));
        // `callsSeed` feeds the START_AUTH integrity token; `isVpn` is the
        // server's own verdict on our egress IP (logged for the record).
        self.calls_seed = payload.get("callsSeed").and_then(|v| v.as_i64());
        match self.calls_seed {
            Some(seed) => debug!("callsSeed = {seed} (0x{seed:016x})"),
            None => debug!("SESSION_INIT response carries no callsSeed"),
        }
        if let Some(is_vpn) = payload.get("isVpn").and_then(|v| v.as_bool()) {
            debug!("server isVpn verdict = {is_vpn}");
        }
        Ok(())
    }

    pub async fn do_chat_sync(&mut self, token: &str) -> Result<()> {
        let seq = self
            .send(
                OPCODE_CHAT_SYNC,
                serde_json::json!({
                    "token": token,
                    "bannersSync": 0,
                    "draftsSync": 0,
                    "callsSync": 0,
                    "presenceSync": -1,
                    "configHash": "",
                    "contactsSync": 0,
                    "chatsSync": 0,
                    "interactive": true,
                    "lastLogin": 0
                }),
            )
            .await?;
        let payload = Self::expect_success_packet(self.recv_seq(seq, OPCODE_CHAT_SYNC).await?)?;
        self.auth_token = Some(token.to_string());
        match extract_profile_id(&payload) {
            Ok(id) => self.user_id = Some(id),
            Err(err) => debug!("chat sync: {err:#}"),
        }
        Ok(())
    }

    pub async fn do_verification_request(&mut self, phone: &str) -> Result<String> {
        // `oe0.java`: START_AUTH is `{phone, type, mode}`. `mode` is a native
        // integrity token the server validates for ANDROID sessions (a WEB hello
        // is exempt). Reconstructed in `integrity` from the SESSION_INIT
        // `callsSeed` + our fingerprint.
        let calls_seed = self.calls_seed.ok_or_else(|| {
            anyhow!(
                "SESSION_INIT gave no callsSeed, cannot build the START_AUTH integrity token \
                 (see reveng/max-mode-token.md)"
            )
        })?;
        let fp = &self.fingerprint;
        let mode = crate::integrity::mode(calls_seed, &fp.device_id);
        debug!("START_AUTH mode = {}", bytes_to_hex(&mode));

        let payload = encode_start_auth_payload(phone, &mode)?;
        let seq = self.send_bytes(OPCODE_START_AUTH, payload).await?;
        let payload = Self::expect_success_packet(self.recv_seq(seq, OPCODE_START_AUTH).await?)?;
        debug!("START_AUTH response: {}", truncate_json(&payload));
        map_string(&payload, "token")
    }

    pub async fn do_code_enter(&mut self, token: &str, code: &str) -> Result<CodeOutcome> {
        let seq = self
            .send(
                OPCODE_CHECK_CODE,
                serde_json::json!({
                    "verifyCode": code,
                    "token": token,
                    "authTokenType": "CHECK_CODE",
                }),
            )
            .await?;
        let payload = Self::expect_success_packet(self.recv_seq(seq, OPCODE_CHECK_CODE).await?)?;
        debug!("CHECK_CODE response: {}", truncate_json(&payload));
        // The CHECK_CODE response carries `profile.contact.id` (the freshly
        // authenticated user) — the subsequent LOGIN sync omits it for a brand
        // new account.
        match extract_profile_id(&payload) {
            Ok(id) => self.user_id = Some(id),
            Err(err) => debug!("check code: {err:#}"),
        }

        if let Ok(login_token) = extract_login_token(&payload) {
            return Ok(CodeOutcome::LoggedIn(login_token));
        }
        // `pd0.java`: when the account has a login password, the response instead
        // carries `passwordChallenge = {trackId, hint?, email?}` and no LOGIN
        // token — the caller must answer it with opcode 115.
        if let Some(challenge) = payload.get("passwordChallenge").filter(|v| v.is_object()) {
            let track_id = map_string(challenge, "trackId")
                .context("CHECK_CODE passwordChallenge without a trackId")?;
            let hint = challenge
                .get("hint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            return Ok(CodeOutcome::PasswordRequired { track_id, hint });
        }
        Err(extract_login_token(&payload).unwrap_err())
    }

    pub async fn do_password_check(&mut self, track_id: &str, password: &str) -> Result<String> {
        let seq = self
            .send(
                OPCODE_LOGIN_CHECK_PASSWORD,
                serde_json::json!({
                    "trackId": track_id,
                    "password": password,
                }),
            )
            .await?;
        let payload =
            Self::expect_success_packet(self.recv_seq(seq, OPCODE_LOGIN_CHECK_PASSWORD).await?)?;
        debug!("LOGIN_CHECK_PASSWORD response: {}", truncate_json(&payload));
        match extract_profile_id(&payload) {
            Ok(id) => self.user_id = Some(id),
            Err(err) => debug!("password check: {err:#}"),
        }
        extract_login_token(&payload)
    }

    pub async fn do_call_token_request(&mut self) -> Result<String> {
        let auth_token = self
            .auth_token
            .as_deref()
            .ok_or_else(|| anyhow!("OneMe auth token is unavailable before call token request"))?;
        let user_id = self
            .user_id
            .ok_or_else(|| anyhow!("OneMe user ID is unavailable before call token request"))?;
        let seq = self
            .send(
                OPCODE_CALL_TOKEN_REQUEST,
                serde_json::json!({
                    "value": auth_token,
                    "userId": user_id,
                }),
            )
            .await?;
        let payload =
            Self::expect_success_packet(self.recv_seq(seq, OPCODE_CALL_TOKEN_REQUEST).await?)?;
        map_string(&payload, "token")
    }

    pub async fn wait_for_incoming_call(&mut self) -> Result<IncomingCall> {
        loop {
            let pkt = self.recv_any().await?;
            if pkt.cmd != CMD_EVENT || pkt.opcode != OPCODE_INCOMING_CALL {
                continue;
            }
            return parse_incoming_call_payload(pkt.payload);
        }
    }

    pub async fn send_interactive_heartbeat(&mut self) -> Result<()> {
        self.send(
            OPCODE_INTERACTIVE_HEARTBEAT,
            serde_json::json!({ "interactive": true }),
        )
        .await
        .context("OneMe interactive heartbeat")?;
        Ok(())
    }

    pub async fn start_outgoing_call(
        &mut self,
        calltaker_id: i64,
    ) -> Result<StartedConversationInfo> {
        let conversation_id = Uuid::new_v4().to_string();
        let internal_params = serde_json::to_string(&serde_json::json!({
            "platform": "ANDROID",
            "sdkVersion": "0.1.10.1",
            "clientAppKey": "CGPGAGLGDIHBABABA",
            "deviceId": self.fingerprint.device_id,
            "protocolVersion": 5,
            "onlyAdminCanRecord": false,
            "waitForAdmin": false,
            "capabilities": "3c57f"
        }))?;
        let seq = self
            .send(
                OPCODE_START_OUTGOING_CALL,
                serde_json::json!({
                    "conversationId": conversation_id,
                    "calleeIds": [calltaker_id],
                    "internalParams": internal_params,
                    "isVideo": false
                }),
            )
            .await?;
        let payload =
            Self::expect_success_packet(self.recv_seq(seq, OPCODE_START_OUTGOING_CALL).await?)?;
        if let Some(rejected) = payload
            .get("rejectedParticipants")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        {
            let summary = rejected
                .iter()
                .map(format_rejected_participant)
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            if summary.is_empty() {
                bail!("start call rejected: payload={}", truncate_json(&payload));
            }
            bail!("start call rejected: {summary}");
        }
        let caller_params = map_string(&payload, "internalCallerParams")
            .map_err(|err| anyhow!("{}; opcode 78 payload={}", err, truncate_json(&payload)))?;
        serde_json::from_str(&caller_params).context("decode internalCallerParams JSON")
    }
}

fn extract_login_token(payload: &Value) -> Result<String> {
    payload
        .get("tokenAttrs")
        .and_then(|attrs| attrs.get("LOGIN"))
        .and_then(|login| login.get("token"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "missing tokenAttrs.LOGIN.token in OneMe payload: {}",
                truncate_json(payload)
            )
        })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingCallJson {
    vcp: String,
    caller_id: i64,
    conversation_id: String,
}

#[derive(Debug, Deserialize)]
struct VcpDecoded {
    #[serde(rename = "tkn")]
    signaling_token: String,
    #[serde(rename = "wse")]
    signaling_server: String,
    #[serde(rename = "wte")]
    wt_endpoint: Option<String>,
    #[serde(rename = "stne")]
    stun_server: String,
    #[serde(rename = "trne")]
    turn_servers: String,
    #[serde(rename = "trnu")]
    turn_user: String,
    #[serde(rename = "trnp")]
    turn_password: String,
}

#[derive(Debug, Clone)]
pub struct SignalingServer {
    pub token: String,
    /// WebSocket endpoint (`wss://…/ws2`).
    pub url: String,
    /// WebTransport endpoint (`https://host:port/wt`) when the server provides one.
    pub wt_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IncomingCall {
    pub turn: TurnServer,
    pub signaling: SignalingServer,
    pub stun: String,
    pub caller_id: i64,
    pub conversation_id: String,
}

impl IncomingCall {
    fn from_raw(raw: IncomingCallJson) -> Result<Self> {
        let decoded = decode_vcp(&raw.vcp)?;
        Ok(Self {
            turn: TurnServer {
                urls: decoded
                    .turn_servers
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect(),
                username: decoded.turn_user,
                credential: decoded.turn_password,
            },
            signaling: SignalingServer {
                token: decoded.signaling_token,
                url: decoded.signaling_server,
                wt_url: decoded.wt_endpoint,
            },
            stun: decoded.stun_server,
            caller_id: raw.caller_id,
            conversation_id: raw.conversation_id,
        })
    }
}

pub(crate) fn parse_incoming_call_payload(payload: Value) -> Result<IncomingCall> {
    let raw: IncomingCallJson =
        serde_json::from_value(payload).context("deserialize incoming call payload")?;
    IncomingCall::from_raw(raw)
}

fn decode_vcp(vcp: &str) -> Result<VcpDecoded> {
    let (uncompressed_size, compressed_b64) = vcp
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid vcp format"))?;
    let expected_size: usize = uncompressed_size.parse().context("parse vcp size")?;
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(compressed_b64)
        .context("base64 decode vcp")?;
    let decompressed =
        lz4_flex::block::decompress(&compressed, expected_size).context("lz4 decompress vcp")?;
    debug!("decoded vcp: {}", String::from_utf8_lossy(&decompressed));
    serde_json::from_slice(&decompressed).context("deserialize vcp")
}

fn build_tls_config(skip_tls_verify: bool) -> rustls::ClientConfig {
    if skip_tls_verify {
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(SkipTlsVerifier));
        return config;
    }

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

use crate::SkipTlsVerifier;

fn oneme_host_port(url: &str) -> Result<(String, u16)> {
    let uri = url
        .parse::<Uri>()
        .map_err(|e| anyhow!("parse OneMe URI {url}: {e}"))?;
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("missing host in OneMe URL: {url}"))?;
    Ok((host.to_string(), uri.port_u16().unwrap_or(443)))
}

async fn connect_tcp_stream(host: &str, port: u16) -> Result<TcpStream> {
    let remote_addr = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow!("no TCP addresses resolved for {host}:{port}"))?;
    let socket = new_tcp_socket(remote_addr)?;
    socket
        .set_keepalive(true)
        .context("enable TCP keepalive for OneMe socket")?;
    socket
        .set_nodelay(true)
        .context("enable TCP_NODELAY for OneMe socket")?;
    socket
        .connect(remote_addr)
        .await
        .with_context(|| format!("connect to {remote_addr}"))
}

fn new_tcp_socket(remote_addr: SocketAddr) -> Result<TcpSocket> {
    match remote_addr {
        SocketAddr::V4(_) => TcpSocket::new_v4().context("create IPv4 TCP socket"),
        SocketAddr::V6(_) => TcpSocket::new_v6().context("create IPv6 TCP socket"),
    }
}

fn map_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("missing string field {key} in OneMe payload"))
}

fn extract_profile_id(value: &Value) -> Result<i64> {
    i64_at_path(value, &["profile", "contact", "id"])
        .or_else(|| i64_at_path(value, &["profile", "contactInfo", "id"]))
        .or_else(|| i64_at_path(value, &["profile", "id"]))
        .or_else(|| i64_at_path(value, &["contact", "id"]))
        .or_else(|| i64_at_path(value, &["userId"]))
        .or_else(|| i64_at_path(value, &["uid"]))
        .ok_or_else(|| {
            anyhow!(
                "missing profile/contact user ID in OneMe login payload: {}",
                truncate_json(value)
            )
        })
}

fn format_rejected_participant(value: &Value) -> String {
    let id = value
        .get("id")
        .and_then(Value::as_i64)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let error = value
        .get("errorCode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("participant {id} error={error}")
}

fn i64_at_path(value: &Value, path: &[&str]) -> Option<i64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    match current {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|v| i64::try_from(v).ok())),
        _ => None,
    }
}

fn format_server_error(value: &Value) -> String {
    let error = value.get("error").and_then(Value::as_str);
    let message = value
        .get("message")
        .or_else(|| value.get("localizedMessage"))
        .and_then(Value::as_str);
    match (error, message) {
        (Some(error), Some(message)) => format!("{error}: {message}"),
        (Some(error), None) => error.to_string(),
        (None, Some(message)) => message.to_string(),
        (None, None) => truncate_json(value),
    }
}

fn truncate_json(value: &Value) -> String {
    let s = value.to_string();
    let (short, len) = s.unicode_truncate(200);
    if len < s.len() {
        format!("{}...", short)
    } else {
        short.to_string()
    }
}

fn decode_payload(cof: i8, payload: &[u8]) -> Result<Vec<u8>> {
    if cof > 0 {
        return lz4_decompress_block(payload).context("decompress OneMe payload");
    }
    Ok(payload.to_vec())
}

pub fn parse_msgpack_json(payload: &[u8]) -> Result<Value> {
    let mut parser = MsgParser::new(payload);
    let value = parser
        .parse_value()
        .map_err(|err| anyhow!("failed to decode OneMe msgpack payload: {err}"))?;
    Ok(msg_value_to_json(&value))
}

pub fn encode_json_to_msgpack(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_json_value(value, &mut out)?;
    Ok(out)
}

fn encode_start_auth_payload(phone: &str, mode: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_msgpack_map_len(3, &mut out)?;
    encode_msgpack_string("type", &mut out);
    encode_msgpack_string("START_AUTH", &mut out);
    encode_msgpack_string("phone", &mut out);
    encode_msgpack_string(phone, &mut out);
    encode_msgpack_string("mode", &mut out);
    encode_msgpack_bin(mode, &mut out);
    Ok(out)
}

fn encode_msgpack_bin(bytes: &[u8], out: &mut Vec<u8>) {
    let len = bytes.len();
    if len <= u8::MAX as usize {
        out.push(0xc4);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0xc5);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xc6);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn encode_json_value(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Null => out.push(0xc0),
        Value::Bool(false) => out.push(0xc2),
        Value::Bool(true) => out.push(0xc3),
        Value::String(s) => encode_msgpack_string(s, out),
        Value::Array(items) => {
            encode_msgpack_array_len(items.len(), out)?;
            for item in items {
                encode_json_value(item, out)?;
            }
        }
        Value::Object(map) => {
            encode_msgpack_map_len(map.len(), out)?;
            for (key, val) in map {
                encode_msgpack_string(key, out);
                encode_json_value(val, out)?;
            }
        }
        Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                encode_msgpack_i64(v, out);
            } else if let Some(v) = n.as_u64() {
                encode_msgpack_u64(v, out);
            } else if let Some(v) = n.as_f64() {
                out.push(0xcb);
                out.extend_from_slice(&v.to_bits().to_be_bytes());
            } else {
                bail!("unsupported JSON number for msgpack");
            }
        }
    }
    Ok(())
}

fn encode_msgpack_string(s: &str, out: &mut Vec<u8>) {
    let len = s.len();
    if len <= 31 {
        out.push(0xa0 | (len as u8));
    } else if len <= u8::MAX as usize {
        out.push(0xd9);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0xda);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(s.as_bytes());
}

fn encode_msgpack_array_len(len: usize, out: &mut Vec<u8>) -> Result<()> {
    if len <= 15 {
        out.push(0x90 | (len as u8));
    } else if len <= u16::MAX as usize {
        out.push(0xdc);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        bail!("msgpack array too large");
    }
    Ok(())
}

fn encode_msgpack_map_len(len: usize, out: &mut Vec<u8>) -> Result<()> {
    if len <= 15 {
        out.push(0x80 | (len as u8));
    } else if len <= u16::MAX as usize {
        out.push(0xde);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        bail!("msgpack map too large");
    }
    Ok(())
}

fn encode_msgpack_i64(v: i64, out: &mut Vec<u8>) {
    if (0..=127).contains(&v) {
        out.push(v as u8);
    } else if (-32..=-1).contains(&v) {
        out.push(v as i8 as u8);
    } else if (i8::MIN as i64..=i8::MAX as i64).contains(&v) {
        out.push(0xd0);
        out.push(v as i8 as u8);
    } else if (i16::MIN as i64..=i16::MAX as i64).contains(&v) {
        out.push(0xd1);
        out.extend_from_slice(&(v as i16).to_be_bytes());
    } else if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        out.push(0xd2);
        out.extend_from_slice(&(v as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn encode_msgpack_u64(v: u64, out: &mut Vec<u8>) {
    if v <= 127 {
        out.push(v as u8);
    } else if u8::try_from(v).is_ok() {
        out.push(0xcc);
        out.push(v as u8);
    } else if u16::try_from(v).is_ok() {
        out.push(0xcd);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if u32::try_from(v).is_ok() {
        out.push(0xce);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

#[derive(Debug, Clone)]
enum MsgValue {
    Nil,
    Bool(bool),
    Int(i128),
    Float(f64),
    String(String),
    Binary(Vec<u8>),
    Array(Vec<MsgValue>),
    Map(Vec<(MsgValue, MsgValue)>),
    Ext(i8, Vec<u8>),
}

fn msg_value_to_json(value: &MsgValue) -> Value {
    match value {
        MsgValue::Nil => Value::Null,
        MsgValue::Bool(v) => Value::Bool(*v),
        MsgValue::Int(v) => {
            if let Ok(v) = i64::try_from(*v) {
                Value::Number(Number::from(v))
            } else if let Ok(v) = u64::try_from(*v) {
                Value::Number(Number::from(v))
            } else {
                Value::String(v.to_string())
            }
        }
        MsgValue::Float(v) => Number::from_f64(*v)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MsgValue::String(v) => Value::String(v.clone()),
        MsgValue::Binary(v) => Value::String(bytes_to_hex(v)),
        MsgValue::Ext(typ, v) => {
            let mut obj = Map::new();
            obj.insert("$ext_type".to_string(), Value::Number(Number::from(*typ)));
            obj.insert("$bin".to_string(), Value::String(bytes_to_hex(v)));
            Value::Object(obj)
        }
        MsgValue::Array(values) => Value::Array(values.iter().map(msg_value_to_json).collect()),
        MsgValue::Map(entries) => {
            let mut obj = Map::new();
            for (k, v) in entries {
                obj.insert(msg_key_to_string(k), msg_value_to_json(v));
            }
            Value::Object(obj)
        }
    }
}

fn msg_key_to_string(value: &MsgValue) -> String {
    match value {
        MsgValue::String(v) => v.clone(),
        MsgValue::Int(v) => v.to_string(),
        MsgValue::Bool(v) => v.to_string(),
        _ => format!("{value:?}"),
    }
}

struct MsgParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> MsgParser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn parse_value(&mut self) -> Result<MsgValue, String> {
        let marker = self.read_u8()?;
        match marker {
            0x00..=0x7f => Ok(MsgValue::Int(marker as i128)),
            0x80..=0x8f => self.parse_map((marker & 0x0f) as usize),
            0x90..=0x9f => self.parse_array((marker & 0x0f) as usize),
            0xa0..=0xbf => self.parse_string((marker & 0x1f) as usize),
            0xc0 => Ok(MsgValue::Nil),
            0xc2 => Ok(MsgValue::Bool(false)),
            0xc3 => Ok(MsgValue::Bool(true)),
            0xc4 => {
                let len = self.read_u8()? as usize;
                self.parse_binary(len)
            }
            0xc5 => {
                let len = self.read_u16()? as usize;
                self.parse_binary(len)
            }
            0xc6 => {
                let len = self.read_u32()? as usize;
                self.parse_binary(len)
            }
            0xc7 => {
                let len = self.read_u8()? as usize;
                let typ = self.read_u8()? as i8;
                Ok(MsgValue::Ext(typ, self.read_bytes(len)?.to_vec()))
            }
            0xc8 => {
                let len = self.read_u16()? as usize;
                let typ = self.read_u8()? as i8;
                Ok(MsgValue::Ext(typ, self.read_bytes(len)?.to_vec()))
            }
            0xc9 => {
                let len = self.read_u32()? as usize;
                let typ = self.read_u8()? as i8;
                Ok(MsgValue::Ext(typ, self.read_bytes(len)?.to_vec()))
            }
            0xca => Ok(MsgValue::Float(f32::from_bits(self.read_u32()?) as f64)),
            0xcb => Ok(MsgValue::Float(f64::from_bits(self.read_u64()?))),
            0xcc => Ok(MsgValue::Int(self.read_u8()? as i128)),
            0xcd => Ok(MsgValue::Int(self.read_u16()? as i128)),
            0xce => Ok(MsgValue::Int(self.read_u32()? as i128)),
            0xcf => Ok(MsgValue::Int(self.read_u64()? as i128)),
            0xd0 => Ok(MsgValue::Int(self.read_u8()? as i8 as i128)),
            0xd1 => Ok(MsgValue::Int(self.read_u16()? as i16 as i128)),
            0xd2 => Ok(MsgValue::Int(self.read_u32()? as i32 as i128)),
            0xd3 => Ok(MsgValue::Int(self.read_u64()? as i64 as i128)),
            0xd9 => {
                let len = self.read_u8()? as usize;
                self.parse_string(len)
            }
            0xda => {
                let len = self.read_u16()? as usize;
                self.parse_string(len)
            }
            0xdb => {
                let len = self.read_u32()? as usize;
                self.parse_string(len)
            }
            0xdc => {
                let len = self.read_u16()? as usize;
                self.parse_array(len)
            }
            0xdd => {
                let len = self.read_u32()? as usize;
                self.parse_array(len)
            }
            0xde => {
                let len = self.read_u16()? as usize;
                self.parse_map(len)
            }
            0xdf => {
                let len = self.read_u32()? as usize;
                self.parse_map(len)
            }
            0xe0..=0xff => Ok(MsgValue::Int((marker as i8) as i128)),
            other => Err(format!("unsupported msgpack marker 0x{other:02x}")),
        }
    }

    fn parse_array(&mut self, len: usize) -> Result<MsgValue, String> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.parse_value()?);
        }
        Ok(MsgValue::Array(out))
    }

    fn parse_map(&mut self, len: usize) -> Result<MsgValue, String> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push((self.parse_value()?, self.parse_value()?));
        }
        Ok(MsgValue::Map(out))
    }

    fn parse_string(&mut self, len: usize) -> Result<MsgValue, String> {
        Ok(MsgValue::String(
            String::from_utf8_lossy(self.read_bytes(len)?).into_owned(),
        ))
    }

    fn parse_binary(&mut self, len: usize) -> Result<MsgValue, String> {
        Ok(MsgValue::Binary(self.read_bytes(len)?.to_vec()))
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        let Some(&b) = self.input.get(self.pos) else {
            return Err("unexpected eof".to_string());
        };
        self.pos += 1;
        Ok(b)
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes(
            bytes.try_into().map_err(|_| "unexpected eof".to_string())?,
        ))
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().map_err(|_| "unexpected eof".to_string())?,
        ))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().map_err(|_| "unexpected eof".to_string())?,
        ))
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.pos + len > self.input.len() {
            return Err("unexpected eof".to_string());
        }
        let slice = &self.input[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }
}

fn lz4_decompress_block(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0u8; MAX_DECOMPRESSED_LEN];
    let len = lz4_flex::block::decompress_into(input, &mut out)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    out.truncate(len);
    Ok(out)
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{b:02x}"));
    }
    out
}
