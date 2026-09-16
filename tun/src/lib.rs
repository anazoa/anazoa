pub mod daemon;
pub mod engine;
pub mod host;
pub mod privdrop;
pub mod protozoa;

#[cfg(target_os = "android")]
pub mod android;

use serde::Deserialize;
use std::sync::{Arc, RwLock};
use tokio::signal;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, MissedTickBehavior, interval, sleep};

use anazoa_auth::oneme::SessionClient;
use anazoa_config::AuthConfig;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(flatten)]
    pub auth: AuthConfig,
    pub token: String,
    #[serde(rename = "signaling-user-id")]
    pub signaling_user_id: String,
    #[serde(rename = "remote-peer-id")]
    pub remote_peer_id: i64,
    pub media: Option<String>,
    #[serde(rename = "media-video-resolution")]
    pub media_video_resolution: String,
    #[serde(rename = "ctl-socket", default = "default_socket_path")]
    pub ctl_socket: String,
    #[serde(rename = "tun-name")]
    pub tun_name: String,
    #[serde(default)]
    pub privdrop: Option<String>,
    #[serde(
        rename = "oneme-keepalive-secs",
        default = "default_oneme_keepalive_secs"
    )]
    pub oneme_keepalive_secs: u64,
    /// Base64-encoded 32-byte X25519 private key for Noise KK authentication.
    #[serde(rename = "noise-privkey", default)]
    pub noise_privkey: Option<String>,
    /// Base64-encoded 32-byte X25519 public key of the remote peer.
    #[serde(rename = "noise-peer-pubkey", default)]
    pub noise_peer_pubkey: Option<String>,
}

pub const DEFAULT_ONEME_KEEPALIVE_SECS: u64 = 25;

fn default_oneme_keepalive_secs() -> u64 {
    DEFAULT_ONEME_KEEPALIVE_SECS
}

fn default_socket_path() -> String {
    "/run/anazoa.sock".to_string()
}

pub async fn wait_or_shutdown(duration: Duration) -> bool {
    tokio::select! {
        _ = shutdown_signal() => true,
        _ = sleep(duration) => false,
    }
}

pub async fn call_keepalive_loop(
    oneme: Arc<Mutex<SessionClient>>,
    keepalive_interval: Duration,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut tick = interval(keepalive_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await;

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    return;
                }
            }
            _ = tick.tick() => {
                let Ok(mut client) = oneme.try_lock() else {
                    tracing::debug!(
                        "skipping OneMe interactive heartbeat because the session client is busy"
                    );
                    continue;
                };
                if let Err(err) = client.send_interactive_heartbeat().await {
                    tracing::warn!(
                        "OneMe interactive heartbeat during call failed: {err:#}; calltaker session will be re-established after the call ends"
                    );
                    return;
                }
            }
        }
    }
}

// Same lifetime mismatch as TUNNEL_STATE (see protozoa::tunnel): a plain
// OnceLock can only ever be set once, but Android's nativeStart/nativeStop
// can run this many times in the same process. Worse than TUNNEL_STATE's
// failure mode: `.set()` silently no-opped on a re-init, so the *previous*
// session's channel stuck around — and since watch channels retain their
// last value, a leftover `true` from the prior nativeStop's trigger_shutdown()
// made the new session's very first shutdown_signal() poll resolve
// immediately, exiting run_with_tun right away with no error at all.
static SHUTDOWN_TX: RwLock<Option<watch::Sender<bool>>> = RwLock::new(None);

fn shutdown_tx() -> Option<watch::Sender<bool>> {
    SHUTDOWN_TX
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn init_shutdown_watcher() {
    let (tx, _) = watch::channel(false);
    *SHUTDOWN_TX.write().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    tokio::spawn(async {
        #[cfg(unix)]
        {
            let mut sigterm = unix_signal(SignalKind::terminate()).unwrap();
            let mut sighup = unix_signal(SignalKind::hangup()).unwrap();
            tokio::select! {
                _ = signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
                _ = sighup.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = signal::ctrl_c().await;
        }
        trigger_shutdown();
    });
}

/// Non-blocking check of the shutdown flag, for callers that want to bail
/// out early rather than start (or log) work that `shutdown_signal()` would
/// cancel on its very first poll anyway.
pub fn shutdown_requested() -> bool {
    shutdown_tx().is_some_and(|tx| *tx.borrow())
}

pub async fn shutdown_signal() {
    let tx = shutdown_tx().expect("init_shutdown_watcher not called");
    let mut rx = tx.subscribe();
    if *rx.borrow() {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        if *rx.borrow() {
            return;
        }
    }
}

/// Programmatic counterpart to the signal-driven shutdown in
/// [`init_shutdown_watcher`]. Used where there's no process signal to catch,
/// e.g. Android calling back into the library to tear down the tunnel.
///
/// Deliberately `send_replace` rather than `send`: `send` is a no-op (silently
/// drops the value, doesn't even store it) when there are zero live receivers
/// at that exact instant, which `shutdown_signal()` guarantees for an instant
/// on every `idle_loop` iteration (each one subscribes fresh and drops that
/// subscription if some other select! branch wins the race first). A
/// same-instant `trigger_shutdown()` could land in that gap and be silently
/// swallowed, leaving nothing to shut anything down — this was observed
/// firsthand as a ~10s stall (until an unrelated later event happened to
/// create a receiver) before switching to `send_replace`, which stores the
/// value unconditionally regardless of receiver count.
pub fn trigger_shutdown() {
    if let Some(tx) = shutdown_tx() {
        tx.send_replace(true);
    }
}
