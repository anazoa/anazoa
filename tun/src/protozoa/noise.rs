use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

const NOISE_PATTERN: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// Each KK handshake message: 32-byte ephemeral pubkey + 16-byte poly1305 MAC.
pub const NOISE_MSG_SIZE: usize = 48;
/// Outgoing audio frames to carry the Noise message (~300 ms at 50 fps).
const NOISE_INJECT_FRAMES: u32 = 15;
/// Incoming audio frames to scan before declaring auth failure (~4 s at 50 fps).
const NOISE_CHECK_FRAMES: u32 = 200;

/// A minimal valid Opus packet: CELT-NB, 1 frame, 20 ms, 1 channel, no data.
/// Substituted for token-carrying frames so the decoder sees a clean silence
/// rather than a corrupted TOC byte.
const CN_FRAME: &[u8] = &[0xf8];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoiseRole {
    Initiator, // Caller
    Responder, // Calltaker
}

pub struct NoiseKeys {
    pub privkey: Vec<u8>,
    pub peer_pubkey: Vec<u8>,
}

pub struct NoiseCallState {
    role: NoiseRole,
    /// Consumed once handshake completes (set to None).
    handshake: Mutex<Option<snow::HandshakeState>>,
    /// Pre-computed outgoing message. For Initiator: msg1, ready at call start.
    /// For Responder: msg2, filled after msg1 is received.
    outgoing_msg: Mutex<[u8; NOISE_MSG_SIZE]>,
    outgoing_ready: AtomicBool,
    outgoing_sent: AtomicU32,
    auth_failed: AtomicBool,
    /// Frames received after authentication succeeded.
    /// Closes the CN-replacement window after NOISE_INJECT_FRAMES even under loss.
    frames_since_auth: AtomicU32,
    frames_checked: AtomicU32,
}

impl NoiseCallState {
    /// Process an incoming audio frame in-place. Drives the handshake state machine.
    ///
    /// Returns the new payload size and whether authentication just completed.
    /// A size of 0 means the frame should be passed through unchanged.
    /// Caller is responsible for updating `TunnelState::authenticated` when the bool is true.
    pub fn process_incoming(&self, payload: &mut [u8], authenticated: bool) -> (usize, bool) {
        if !authenticated {
            let idx = self.frames_checked.fetch_add(1, Ordering::Relaxed);
            let mut hs_guard = self.handshake.lock().unwrap();
            if let Some(ref mut hs) = *hs_guard {
                let mut buf = [];
                if hs
                    .read_message(&payload[..NOISE_MSG_SIZE], &mut buf)
                    .is_ok()
                {
                    if self.role == NoiseRole::Responder {
                        let mut msg2 = [0u8; NOISE_MSG_SIZE];
                        if hs.write_message(&[], &mut msg2).is_ok() {
                            *self.outgoing_msg.lock().unwrap() = msg2;
                            self.outgoing_ready.store(true, Ordering::Relaxed);
                        }
                    }
                    *hs_guard = None; // handshake complete, drop state
                    drop(hs_guard);
                    tracing::info!("Noise KK authentication successful");
                    payload[..CN_FRAME.len()].copy_from_slice(CN_FRAME);
                    return (CN_FRAME.len(), true);
                }
            }
            drop(hs_guard);
            if idx + 1 == NOISE_CHECK_FRAMES {
                tracing::warn!(
                    "Noise KK authentication failed: no valid message in {} frames",
                    NOISE_CHECK_FRAMES
                );
                self.auth_failed.store(true, Ordering::Relaxed);
            }
            return (0, false);
        }

        // Post-auth: silence frames in the sender's remaining inject window.
        let since = self.frames_since_auth.fetch_add(1, Ordering::Relaxed);
        if since < NOISE_INJECT_FRAMES {
            payload[..CN_FRAME.len()].copy_from_slice(CN_FRAME);
            return (CN_FRAME.len(), false);
        }
        (0, false)
    }

    /// If a Noise message should be injected, overwrites the first `NOISE_MSG_SIZE` bytes of
    /// `payload` with it and returns true. Otherwise returns false and leaves `payload` unchanged.
    pub fn try_inject(&self, payload: &mut [u8]) -> bool {
        if !self.outgoing_ready.load(Ordering::Relaxed)
            || self.outgoing_sent.load(Ordering::Relaxed) >= NOISE_INJECT_FRAMES
            || payload.len() < NOISE_MSG_SIZE
        {
            return false;
        }
        self.outgoing_sent.fetch_add(1, Ordering::Relaxed);
        payload[..NOISE_MSG_SIZE].copy_from_slice(&*self.outgoing_msg.lock().unwrap());
        true
    }
}

pub static NOISE_KEYS: OnceLock<NoiseKeys> = OnceLock::new();
/// Initialized once; inner Option replaced by reset_noise_state each call.
pub static NOISE_STATE: OnceLock<Mutex<Option<NoiseCallState>>> = OnceLock::new();

pub fn init_noise_keys(privkey: Vec<u8>, peer_pubkey: Vec<u8>) {
    let _ = NOISE_KEYS.set(NoiseKeys {
        privkey,
        peer_pubkey,
    });
    let _ = NOISE_STATE.set(Mutex::new(None));
}

/// Build a fresh handshake state for a new call. Does not touch TunnelState —
/// callers are responsible for re-arming TunnelState::authenticated.
pub fn noise_reset_state(role: NoiseRole) {
    let Some(keys) = NOISE_KEYS.get() else { return };
    let Some(state_mu) = NOISE_STATE.get() else {
        return;
    };

    let pattern: snow::params::NoiseParams = NOISE_PATTERN.parse().expect("valid noise pattern");
    let builder = snow::Builder::new(pattern)
        .local_private_key(&keys.privkey)
        .expect("set local private key")
        .remote_public_key(&keys.peer_pubkey)
        .expect("set remote public key");

    let mut outgoing_msg = [0u8; NOISE_MSG_SIZE];
    let outgoing_ready;
    let hs = match role {
        NoiseRole::Initiator => {
            let mut hs = builder.build_initiator().expect("build noise initiator");
            hs.write_message(&[], &mut outgoing_msg)
                .expect("write noise msg1");
            outgoing_ready = true;
            hs
        }
        NoiseRole::Responder => {
            let hs = builder.build_responder().expect("build noise responder");
            outgoing_ready = false;
            hs
        }
    };

    *state_mu.lock().unwrap() = Some(NoiseCallState {
        role,
        handshake: Mutex::new(Some(hs)),
        outgoing_msg: Mutex::new(outgoing_msg),
        outgoing_ready: AtomicBool::new(outgoing_ready),
        outgoing_sent: AtomicU32::new(0),
        auth_failed: AtomicBool::new(false),
        frames_since_auth: AtomicU32::new(0),
        frames_checked: AtomicU32::new(0),
    });
}

pub fn noise_try_inject(payload: &mut [u8]) -> bool {
    NOISE_STATE
        .get()
        .and_then(|mu| mu.lock().ok())
        .and_then(|g| g.as_ref().map(|s| s.try_inject(payload)))
        .unwrap_or(false)
}

pub fn noise_process_incoming(payload: &mut [u8], authenticated: bool) -> Option<(usize, bool)> {
    let guard = NOISE_STATE.get()?.lock().ok()?;
    Some(guard.as_ref()?.process_incoming(payload, authenticated))
}

pub fn noise_auth_failed() -> bool {
    NOISE_STATE
        .get()
        .and_then(|mu| mu.lock().ok())
        .and_then(|g| g.as_ref().map(|s| s.auth_failed.load(Ordering::Relaxed)))
        .unwrap_or(false)
}
