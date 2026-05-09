use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_rustls::TlsAcceptor;
use wtransport::Endpoint;
use wtransport::Identity;
use wtransport::ServerConfig;
use wtransport::endpoint::endpoint_side::Server as WtServer;
use wtransport::tls::{Certificate as WtCertificate, CertificateChain, PrivateKey as WtPrivateKey};

const CALLTAKER_PARTICIPANT_ID: i64 = 1;
const CALLER_PARTICIPANT_ID: i64 = 2;
const CALLTAKER_EXTERNAL_ID: &str = "1001";
const CALLER_EXTERNAL_ID: &str = "1002";
const MOCK_SIGNALING_TOKEN: &str = "mock-token";
const MOCK_ONEME_VERSION: u8 = 10;
const MOCK_ONEME_CMD_EVENT: u8 = 0;
const MOCK_ONEME_CMD_SUCCESS: u8 = 1;
const MOCK_ONEME_OPCODE_CLIENT_HELLO: u16 = 6;
const MOCK_ONEME_OPCODE_CHAT_SYNC: u16 = 19;
const MOCK_ONEME_OPCODE_START_OUTGOING_CALL: u16 = 78;
const MOCK_ONEME_OPCODE_INCOMING_CALL: u16 = 137;

pub struct MockServerConfig {
    pub signaling_listen: String,
    pub oneme_listen: String,
    pub calls_listen: String,
    pub signaling_public_addr: String,
    pub turn_public_addr: String,
    pub turn_username: String,
    pub turn_password: String,
}

pub async fn run_mock_server(config: MockServerConfig) -> Result<()> {
    let state = Arc::new(MockServerState::new(
        config.signaling_public_addr,
        config.turn_public_addr,
        config.turn_username,
        config.turn_password,
    ));
    tokio::try_join!(
        run_mock_signaling_server(&config.signaling_listen),
        run_mock_oneme_server(&config.oneme_listen, Arc::clone(&state)),
        run_mock_calls_server(&config.calls_listen, state),
    )?;
    Ok(())
}

struct MockServerState {
    signaling_public_addr: String,
    turn_public_addr: String,
    turn_username: String,
    turn_password: String,
    pending_incoming_call: AsyncMutex<Option<Value>>,
    pending_notify: Notify,
}

impl MockServerState {
    fn new(
        signaling_public_addr: String,
        turn_public_addr: String,
        turn_username: String,
        turn_password: String,
    ) -> Self {
        Self {
            signaling_public_addr,
            turn_public_addr,
            turn_username,
            turn_password,
            pending_incoming_call: AsyncMutex::new(None),
            pending_notify: Notify::new(),
        }
    }

    fn turn_url(&self) -> String {
        format!("turn:{}?transport=udp", self.turn_public_addr)
    }

    fn stun_url(&self) -> String {
        format!("stun:{}", self.turn_public_addr)
    }

    fn caller_signaling_endpoint(&self, conversation_id: &str) -> String {
        format!(
            "https://{}/mock/caller/{}",
            self.signaling_public_addr, conversation_id
        )
    }

    fn calltaker_signaling_endpoint(&self, conversation_id: &str) -> String {
        format!(
            "https://{}/mock/calltaker/{}",
            self.signaling_public_addr, conversation_id
        )
    }

    async fn store_incoming_call(&self, conversation_id: &str) -> Result<()> {
        let vcp = encode_vcp(
            &self.calltaker_signaling_endpoint(conversation_id),
            &self.stun_url(),
            &self.turn_url(),
            &self.turn_username,
            &self.turn_password,
        )?;
        let incoming = json!({
            "vcp": vcp,
            "callerId": CALLER_EXTERNAL_ID.parse::<i64>().expect("valid caller id"),
            "conversationId": conversation_id,
        });
        *self.pending_incoming_call.lock().await = Some(incoming);
        self.pending_notify.notify_waiters();
        Ok(())
    }

    async fn wait_for_incoming_call(&self) -> Value {
        loop {
            if let Some(call) = self.pending_incoming_call.lock().await.take() {
                return call;
            }
            self.pending_notify.notified().await;
        }
    }
}

async fn run_mock_calls_server(listen: &str, state: Arc<MockServerState>) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind mock calls server at {listen}"))?;
    tracing::info!("Mock calls server listening on http://{listen}");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accept mock calls connection")?;
        let io = TokioIo::new(stream);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let service = service_fn(move |req| handle_calls_request(req, Arc::clone(&state)));
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                tracing::warn!("mock calls connection error: {err:#}");
            }
        });
    }
}

async fn handle_calls_request(
    req: Request<Incoming>,
    state: Arc<MockServerState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = match inner_handle_calls_request(req, state).await {
        Ok(response) => response,
        Err(err) => text_response(StatusCode::BAD_REQUEST, err.to_string()),
    };
    Ok(response)
}

async fn inner_handle_calls_request(
    req: Request<Incoming>,
    state: Arc<MockServerState>,
) -> Result<Response<Full<Bytes>>> {
    if req.method() != Method::POST || req.uri().path() != "/fb.do" {
        return Ok(text_response(StatusCode::NOT_FOUND, "not found"));
    }

    let body = req
        .into_body()
        .collect()
        .await
        .context("read mock calls body")?
        .to_bytes();
    let form: CallsRequest = serde_urlencoded::from_bytes(&body).context("decode calls form")?;

    if form.method != "vchat.startConversation" {
        bail!("unsupported mock calls method: {}", form.method);
    }

    state.store_incoming_call(&form.conversation_id).await?;
    let response = json!({
        "turn": {
            "urls": [state.turn_url()],
            "username": state.turn_username.clone(),
            "credential": state.turn_password.clone(),
        },
        "stun": {
            "urls": [state.stun_url()],
        },
        "endpoint": state.caller_signaling_endpoint(&form.conversation_id),
    });
    json_response(response)
}

#[derive(Deserialize)]
struct CallsRequest {
    method: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
}

async fn run_mock_oneme_server(listen: &str, state: Arc<MockServerState>) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind mock Oneme server at {listen}"))?;
    anazoa_auth::ensure_rustls_provider();
    let cert = generate_simple_self_signed(vec!["mock-oneme".to_string()])
        .context("generate mock Oneme TLS certificate")?;
    let cert_chain = vec![cert.cert.der().clone()];
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .context("build mock Oneme TLS config")?;
    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));
    tracing::info!("Mock Oneme server listening on tls://{listen}");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accept mock Oneme connection")?;
        let state = Arc::clone(&state);
        let tls_acceptor = tls_acceptor.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_oneme_peer(stream, tls_acceptor, state).await {
                tracing::warn!("mock Oneme connection ended: {err:#}");
            }
        });
    }
}

async fn handle_oneme_peer(
    stream: TcpStream,
    tls_acceptor: TlsAcceptor,
    state: Arc<MockServerState>,
) -> Result<()> {
    let mut tls = tls_acceptor
        .accept(stream)
        .await
        .context("accept mock Oneme TLS connection")?;
    let (seq, opcode, payload) = read_oneme_packet(&mut tls).await?;
    if opcode != MOCK_ONEME_OPCODE_CLIENT_HELLO {
        bail!("expected mock Oneme client_hello opcode 6, got opcode={opcode}");
    }
    let _ = payload;
    let mut user_id = None;
    write_oneme_packet(
        &mut tls,
        MOCK_ONEME_CMD_SUCCESS,
        seq,
        MOCK_ONEME_OPCODE_CLIENT_HELLO,
        Value::Null,
    )
    .await
    .context("send mock Oneme client_hello ack")?;

    let mut next_packet =
        match tokio::time::timeout(Duration::from_secs(1), read_oneme_packet(&mut tls)).await {
            Ok(packet) => Some(packet?),
            Err(_) => {
                let incoming = state.wait_for_incoming_call().await;
                write_oneme_packet(
                    &mut tls,
                    MOCK_ONEME_CMD_EVENT,
                    0,
                    MOCK_ONEME_OPCODE_INCOMING_CALL,
                    incoming,
                )
                .await
                .context("send mock incoming call")?;
                return Ok(());
            }
        };

    loop {
        let (seq, opcode, payload) = match next_packet.take() {
            Some(packet) => packet,
            None => read_oneme_packet(&mut tls).await?,
        };
        match opcode {
            MOCK_ONEME_OPCODE_CHAT_SYNC => {
                let sync_user_id = mock_user_id_from_chat_sync(&payload);
                user_id = Some(sync_user_id);
                write_oneme_packet(
                    &mut tls,
                    MOCK_ONEME_CMD_SUCCESS,
                    seq,
                    MOCK_ONEME_OPCODE_CHAT_SYNC,
                    json!({ "profile": { "id": sync_user_id } }),
                )
                .await
                .context("send mock chat sync response")?;
                if sync_user_id == CALLTAKER_EXTERNAL_ID.parse::<i64>().expect("valid id") {
                    let incoming = state.wait_for_incoming_call().await;
                    write_oneme_packet(
                        &mut tls,
                        MOCK_ONEME_CMD_EVENT,
                        0,
                        MOCK_ONEME_OPCODE_INCOMING_CALL,
                        incoming,
                    )
                    .await
                    .context("send mock incoming call")?;
                }
            }
            MOCK_ONEME_OPCODE_START_OUTGOING_CALL => {
                if user_id != Some(CALLER_EXTERNAL_ID.parse::<i64>().expect("valid id")) {
                    bail!("mock start outgoing call before caller chat sync");
                }
                let conversation_id = payload
                    .get("conversationId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("mock outgoing call missing conversationId"))?;
                state.store_incoming_call(conversation_id).await?;
                let response = json!({
                    "rejectedParticipants": [],
                    "internalCallerParams": serde_json::to_string(&json!({
                        "turn": {
                            "urls": [state.turn_url()],
                            "username": state.turn_username.clone(),
                            "credential": state.turn_password.clone(),
                        },
                        "stun": {
                            "urls": [state.stun_url()],
                        },
                        "endpoint": state.caller_signaling_endpoint(conversation_id),
                    })).context("serialize mock internalCallerParams")?,
                });
                write_oneme_packet(
                    &mut tls,
                    MOCK_ONEME_CMD_SUCCESS,
                    seq,
                    MOCK_ONEME_OPCODE_START_OUTGOING_CALL,
                    response,
                )
                .await
                .context("send mock outgoing call response")?;
            }
            _ => {
                tracing::debug!("ignoring mock Oneme opcode {opcode}");
                write_oneme_packet(&mut tls, MOCK_ONEME_CMD_SUCCESS, seq, opcode, Value::Null)
                    .await
                    .context("send generic mock Oneme response")?;
            }
        }
    }
}

fn mock_user_id_from_chat_sync(payload: &Value) -> i64 {
    match payload
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "mock-caller-token" => CALLER_EXTERNAL_ID.parse::<i64>().expect("valid caller id"),
        _ => CALLTAKER_EXTERNAL_ID
            .parse::<i64>()
            .expect("valid calltaker id"),
    }
}

async fn run_mock_signaling_server(listen: &str) -> Result<()> {
    let bind_addr = listen
        .parse()
        .with_context(|| format!("parse mock signaling listen address {listen}"))?;
    let cert = generate_simple_self_signed(vec!["mock-signaling".to_string()])
        .context("generate mock signaling certificate")?;
    let certificate = WtCertificate::from_der(cert.cert.der().to_vec())
        .context("parse mock signaling certificate")?;
    let private_key = WtPrivateKey::from_der_pkcs8(cert.signing_key.serialize_der());
    let identity = Identity::new(CertificateChain::single(certificate), private_key);
    let config = ServerConfig::builder()
        .with_bind_address(bind_addr)
        .with_identity(identity)
        .build();
    let endpoint = Endpoint::server(config).context("create mock signaling WT endpoint")?;
    tracing::info!("Mock signaling server listening on https://{listen}");

    loop {
        let first = accept_peer(&endpoint).await?;
        let second = accept_peer(&endpoint).await?;
        let (calltaker, caller) = validate_pair(first, second)?;
        tracing::info!(
            "Mock signaling paired conversation={}",
            calltaker.conversation_id
        );
        run_pair(calltaker, caller).await?;
    }
}

struct PeerConn {
    role: PeerRole,
    conversation_id: String,
    _conn: wtransport::Connection,
    send: wtransport::SendStream,
    recv: wtransport::RecvStream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerRole {
    Calltaker,
    Caller,
}

#[allow(clippy::result_large_err)]
async fn accept_peer(endpoint: &Endpoint<WtServer>) -> Result<PeerConn> {
    let incoming_session = endpoint.accept().await;
    let session_request = incoming_session
        .await
        .context("accept mock signaling session")?;
    let meta = PeerMeta::from_path(session_request.path())?;
    let connection = session_request
        .accept()
        .await
        .context("accept mock signaling WebTransport request")?;
    let (send, recv) = connection
        .accept_bi()
        .await
        .context("accept mock signaling bidi stream")?;
    tracing::info!(
        "Mock signaling peer connected role={:?} conversation={}",
        meta.role,
        meta.conversation_id
    );
    Ok(PeerConn {
        role: meta.role,
        conversation_id: meta.conversation_id,
        _conn: connection,
        send,
        recv,
    })
}

struct PeerMeta {
    role: PeerRole,
    conversation_id: String,
}

impl PeerMeta {
    fn from_path(path: &str) -> Result<Self> {
        let path = path.split('?').next().unwrap_or(path);
        let mut parts = path
            .trim_matches('/')
            .split('/')
            .filter(|part| !part.is_empty());
        let prefix = parts.next().unwrap_or_default();
        let role_part = parts.next().unwrap_or_default();
        let conversation_id = parts.next().unwrap_or_default();
        if prefix != "mock" {
            bail!("missing mock path prefix");
        }
        let role = match role_part {
            "calltaker" => PeerRole::Calltaker,
            "caller" => PeerRole::Caller,
            other => bail!("missing or invalid role path segment: {other}"),
        };
        if conversation_id.is_empty() {
            bail!("missing conversationId path segment");
        }
        Ok(Self {
            role,
            conversation_id: conversation_id.to_string(),
        })
    }
}

async fn read_oneme_packet<S>(stream: &mut S) -> Result<(u16, u16, Value)>
where
    S: AsyncReadExt + Unpin,
{
    let mut header = [0u8; 10];
    stream
        .read_exact(&mut header)
        .await
        .context("read mock Oneme header")?;
    let seq = u16::from_be_bytes([header[2], header[3]]);
    let opcode = u16::from_be_bytes([header[4], header[5]]);
    let len = u32::from_be_bytes([header[6], header[7], header[8], header[9]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream
            .read_exact(&mut payload)
            .await
            .context("read mock Oneme payload")?;
    }
    let payload =
        anazoa_auth::oneme::parse_msgpack_json(&payload).context("decode mock Oneme payload")?;
    Ok((seq, opcode, payload))
}

async fn write_oneme_packet<S>(
    stream: &mut S,
    cmd: u8,
    seq: u16,
    opcode: u16,
    payload: Value,
) -> Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let payload = anazoa_auth::oneme::encode_json_to_msgpack(&payload)?;
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.push(MOCK_ONEME_VERSION);
    frame.push(cmd);
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&opcode.to_be_bytes());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    stream
        .write_all(&frame)
        .await
        .context("write mock Oneme frame")?;
    stream.flush().await.context("flush mock Oneme frame")
}

fn validate_pair(first: PeerConn, second: PeerConn) -> Result<(PeerConn, PeerConn)> {
    if first.conversation_id != second.conversation_id {
        bail!(
            "mock signaling conversation mismatch: {} vs {}",
            first.conversation_id,
            second.conversation_id
        );
    }
    match (first.role, second.role) {
        (PeerRole::Calltaker, PeerRole::Caller) => Ok((first, second)),
        (PeerRole::Caller, PeerRole::Calltaker) => Ok((second, first)),
        _ => bail!("mock signaling peers reported the same role"),
    }
}

async fn run_pair(calltaker: PeerConn, caller: PeerConn) -> Result<()> {
    let conversation_id = calltaker.conversation_id.clone();
    let mut calltaker = calltaker;
    let mut caller = caller;

    send_json(
        &mut calltaker.send,
        connection_notification(&conversation_id),
    )
    .await?;
    send_json(&mut caller.send, connection_notification(&conversation_id)).await?;

    loop {
        tokio::select! {
            msg = read_client_message(&mut calltaker.recv) => {
                match msg? {
                    Some(value) => handle_message(PeerRole::Calltaker, value, &mut calltaker.send, &mut caller.send).await?,
                    None => {
                        send_json(&mut caller.send, hangup_notification()).await?;
                        return Ok(());
                    }
                }
            }
            msg = read_client_message(&mut caller.recv) => {
                match msg? {
                    Some(value) => handle_message(PeerRole::Caller, value, &mut caller.send, &mut calltaker.send).await?,
                    None => {
                        send_json(&mut calltaker.send, hangup_notification()).await?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn read_client_message(recv: &mut wtransport::RecvStream) -> Result<Option<Value>> {
    let Some(text) = recv_text(recv).await? else {
        return Ok(None);
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Some(json!({})));
    }
    if trimmed.eq_ignore_ascii_case("ping") || trimmed.eq_ignore_ascii_case("pong") {
        return Ok(Some(json!({"__ping__": true})));
    }
    let value = serde_json::from_str(trimmed)
        .with_context(|| format!("parse signaling json: {}", trimmed))?;
    Ok(Some(value))
}

async fn handle_message(
    sender_role: PeerRole,
    msg: Value,
    sender: &mut wtransport::SendStream,
    other: &mut wtransport::SendStream,
) -> Result<()> {
    if msg.get("__ping__").is_some() || msg.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(());
    }
    let command = msg
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("signaling command missing command field"))?;
    let sequence = msg.get("sequence").and_then(Value::as_i64).unwrap_or(0);

    match command {
        "update-media-modifiers" | "change-participant-state" => {
            send_json(sender, response(command, sequence, json!({}))).await?;
        }
        "accept-call" => {
            send_json(sender, response(command, sequence, json!({}))).await?;
            if sender_role == PeerRole::Calltaker {
                send_json(other, accepted_call_notification()).await?;
            }
        }
        "get-rooms" => {
            send_json(sender, response(command, sequence, json!({"rooms": []}))).await?;
        }
        "change-media-settings" | "enable-feature-for-roles" | "custom-data" => {
            send_json(sender, response(command, sequence, json!({}))).await?;
        }
        "transmit-data" => {
            let data = msg
                .get("data")
                .cloned()
                .ok_or_else(|| anyhow!("transmit-data missing data"))?;
            send_json(
                other,
                json!({
                    "type": "notification",
                    "notification": "transmitted-data",
                    "stamp": 0,
                    "data": data,
                }),
            )
            .await?;
            send_json(sender, response(command, sequence, json!({}))).await?;
        }
        "hangup" => {
            send_json(sender, response(command, sequence, json!({}))).await?;
            send_json(other, hangup_notification()).await?;
        }
        other_command => bail!("unsupported mock signaling command: {other_command}"),
    }
    Ok(())
}

fn connection_notification(conversation_id: &str) -> Value {
    json!({
        "type": "notification",
        "notification": "connection",
        "stamp": 0,
        "conversation": {
            "id": conversation_id,
            "participants": [
                {
                    "id": CALLTAKER_PARTICIPANT_ID,
                    "externalId": { "id": CALLTAKER_EXTERNAL_ID }
                },
                {
                    "id": CALLER_PARTICIPANT_ID,
                    "externalId": { "id": CALLER_EXTERNAL_ID }
                }
            ]
        }
    })
}

fn accepted_call_notification() -> Value {
    json!({
        "type": "notification",
        "notification": "accepted-call",
        "stamp": 0,
    })
}

fn hangup_notification() -> Value {
    json!({
        "type": "notification",
        "notification": "hangup",
        "stamp": 0,
    })
}

fn response(command: &str, sequence: i64, data: Value) -> Value {
    json!({
        "type": "response",
        "response": command,
        "sequence": sequence,
        "stamp": 0,
        "data": data,
    })
}

async fn send_json(sink: &mut wtransport::SendStream, value: Value) -> Result<()> {
    let text = serde_json::to_string(&value).context("serialize mock signaling message")?;
    send_text(sink, &text).await
}

async fn send_text(send: &mut wtransport::SendStream, text: &str) -> Result<()> {
    let compressed = deflate_compress(text.as_bytes())?;
    let len = encode_quic_varint(
        u64::try_from(compressed.len()).context("mock signaling WT frame too large")?,
    )?;
    send.write_all(&len)
        .await
        .context("write mock signaling WT frame length")?;
    send.write_all(&compressed)
        .await
        .context("write mock signaling WT frame payload")?;
    send.flush().await.context("flush mock signaling WT frame")
}

async fn recv_text(recv: &mut wtransport::RecvStream) -> Result<Option<String>> {
    let Some(len) = read_quic_varint(recv).await? else {
        return Ok(None);
    };
    let len = usize::try_from(len).context("mock signaling WT frame length too large")?;
    let mut buf = vec![0u8; len];
    read_exact_wt(recv, &mut buf).await?;
    let text = String::from_utf8(deflate_decompress(&buf)?)
        .context("mock signaling WT frame is not UTF-8")?;
    Ok(Some(text))
}

fn deflate_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data)
        .context("deflate-compress mock signaling")?;
    enc.finish()
        .context("finish deflate-compress mock signaling")
}

fn deflate_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut dec = flate2::read::DeflateDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .context("deflate-decompress mock signaling")?;
    Ok(out)
}

fn encode_quic_varint(value: u64) -> Result<Vec<u8>> {
    if value >= (1 << 62) {
        bail!("mock signaling WT frame too large for QUIC varint: {value}");
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

async fn read_quic_varint(recv: &mut wtransport::RecvStream) -> Result<Option<u64>> {
    let mut first = [0u8; 1];
    let Some(read) = recv
        .read(&mut first)
        .await
        .context("read mock signaling WT frame prefix")?
    else {
        return Ok(None);
    };
    if read != 1 {
        bail!("short mock signaling WT frame prefix read: {read}");
    }
    let size = match first[0] >> 6 {
        0 => 1usize,
        1 => 2usize,
        2 => 4usize,
        _ => 8usize,
    };
    let mut buf = [0u8; 8];
    buf[0] = first[0] & 0x3f;
    if size > 1 {
        read_exact_wt(recv, &mut buf[1..size]).await?;
    }
    let value = match size {
        1 => u64::from(buf[0]),
        2 => u64::from(u16::from_be_bytes([buf[0], buf[1]])),
        4 => u64::from(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])),
        8 => u64::from_be_bytes(buf),
        _ => unreachable!(),
    };
    Ok(Some(value))
}

async fn read_exact_wt(recv: &mut wtransport::RecvStream, buf: &mut [u8]) -> Result<()> {
    let mut offset = 0usize;
    while offset < buf.len() {
        let Some(read) = recv
            .read(&mut buf[offset..])
            .await
            .context("read mock signaling WT payload")?
        else {
            bail!(
                "unexpected EOF reading mock signaling WT payload: need {} more bytes",
                buf.len() - offset
            );
        };
        offset += read;
    }
    Ok(())
}

fn json_response(value: Value) -> Result<Response<Full<Bytes>>> {
    let text = serde_json::to_string(&value).context("serialize mock calls response")?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(text)))
        .expect("valid mock json response"))
}

fn text_response(status: StatusCode, text: impl Into<String>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(text.into())))
        .expect("valid mock text response")
}

fn encode_vcp(
    signaling_server: &str,
    stun_server: &str,
    turn_server: &str,
    turn_user: &str,
    turn_password: &str,
) -> Result<String> {
    let decoded = json!({
        "tkn": MOCK_SIGNALING_TOKEN,
        "wse": signaling_server,
        "stne": stun_server,
        "trne": turn_server,
        "trnu": turn_user,
        "trnp": turn_password,
    });
    let decompressed = serde_json::to_vec(&decoded).context("serialize mock vcp json")?;
    let compressed = lz4_flex::block::compress(&decompressed);
    let encoded = base64::engine::general_purpose::STANDARD.encode(compressed);
    Ok(format!("{}:{encoded}", decompressed.len()))
}
