pub mod privdrop;

use serde::Deserialize;
use std::sync::{Arc, OnceLock};
use tokio::signal;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, MissedTickBehavior, interval, sleep};

use anazoa_auth::oneme::SessionClient;
use anazoa_config::AuthConfig;

#[derive(Debug, Deserialize)]
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
    pub tun_name: Option<String>,
    #[serde(default)]
    pub privdrop: Option<String>,
    #[serde(
        rename = "oneme-keepalive-secs",
        default = "default_oneme_keepalive_secs"
    )]
    pub oneme_keepalive_secs: u64,
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

static SHUTDOWN_TX: OnceLock<watch::Sender<bool>> = OnceLock::new();

pub fn init_shutdown_watcher() {
    let (tx, _) = watch::channel(false);
    if SHUTDOWN_TX.set(tx).is_err() {
        return;
    }
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
        let _ = SHUTDOWN_TX.get().unwrap().send(true);
    });
}

pub async fn shutdown_signal() {
    let tx = SHUTDOWN_TX.get().expect("init_shutdown_watcher not called");
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
