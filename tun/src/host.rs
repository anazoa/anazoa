//! Hosts one tunnel connect/run/teardown session in its own Tokio runtime.
//!
//! This is the reusable shape behind `android.rs`'s `nativeStart`/
//! `nativeStop`: build a runtime, enter it, watch for shutdown, hand a TUN
//! device to [`crate::engine::run_with_tun`], and expose a command channel
//! for the RPC-style calls (`call`/`hangup`/`answer`/`status`). Factored out
//! so both that JNI layer and anything else that needs to start/stop this
//! cycle repeatedly *in one process* — e.g. a test harness reproducing
//! Android's nativeStart/nativeStop reuse of process-global statics like
//! `TUNNEL_STATE`/`SHUTDOWN_TX` — share one implementation instead of two
//! copies drifting apart. The desktop CLI (`main.rs`) doesn't use this: a
//! plain one-shot process never needs more than one session, so it just
//! inlines the equivalent steps directly.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::Config;
use crate::daemon::{self, DaemonCmd};
use crate::engine::{self, EngineState};

/// Upper bound on how long `stop()` blocks the calling thread. Engine
/// teardown is normally sub-second, but the engine can be stuck in a
/// blocking section (`RaylibMedia::spawn` over a large file) when the
/// shutdown lands, and Android's main thread must not hang past this.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TunnelSession {
    runtime: Runtime,
    cmd_tx: mpsc::Sender<DaemonCmd>,
    /// The engine task itself; `stop()` joins it (bounded) before tearing
    /// the runtime down so its post-call cleanup actually runs.
    engine: JoinHandle<()>,
    /// Engine state snapshot, updated by the engine task at each transition.
    /// `status()` reads this instead of round-tripping a `DaemonCmd::Status`
    /// through `cmd_tx` — the engine doesn't poll `cmd_rx` while it's busy
    /// in blocking media setup, call signaling setup, or post-call
    /// teardown, and the Android UI thread polls status.
    status_rx: watch::Receiver<EngineState>,
    /// Set by the spawned engine task when it exits on its own (error, or a
    /// clean shutdown observed before `stop()` got around to tearing down
    /// explicitly). That task runs ON `runtime`'s own worker threads — it
    /// must never touch this in a way that could drop `runtime` itself,
    /// only flag itself done and let the owning thread (never the runtime's
    /// own async context) reap it via `is_finished`/`stop`. Dropping a
    /// Runtime from inside its own async context is exactly what Tokio
    /// detects and aborts the process over ("Cannot drop a runtime in a
    /// context where blocking is not allowed").
    finished: Arc<AtomicBool>,
}

impl TunnelSession {
    /// `build_tun` runs after the runtime is entered, so it can rely on
    /// ambient reactor context — needed by both `tun_rs::AsyncDevice::from_fd`
    /// (Android, wrapping a `VpnService`-provided fd) and
    /// `DeviceBuilder::build_async` (opening a named interface) — regardless
    /// of which one the caller uses.
    /// `ctl_socket`, if given, serves the same JSON-RPC protocol the desktop
    /// CLI binds (see `daemon::run_jsonrpc_server`) against this session's
    /// command channel — for a host that wants external tools (`anazoa-ctl`,
    /// a test harness) to drive it rather than calling `send_cmd` directly
    /// in-process. `android.rs` passes `None`: Kotlin drives it via direct
    /// JNI-to-`send_cmd` calls instead, with no socket in between.
    pub fn start(
        cfg: Config,
        build_tun: impl FnOnce() -> Result<tun_rs::AsyncDevice>,
        log_dir: Option<String>,
        log_prefix: String,
        media_path: Option<String>,
        auto_call: bool,
        ctl_socket: Option<&str>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        // init_shutdown_watcher() calls tokio::spawn internally, which panics
        // ("there is no reactor running") without an ambient runtime context.
        let _enter = runtime.enter();
        crate::init_shutdown_watcher();

        let tun = build_tun()?;

        let (cmd_tx, cmd_rx) = mpsc::channel(8);

        if let Some(ctl_socket) = ctl_socket {
            let listener = daemon::bind_socket(ctl_socket)?;
            let cmd_tx = cmd_tx.clone();
            runtime.spawn(async move {
                if let Err(e) = daemon::run_jsonrpc_server(listener, cmd_tx).await {
                    warn!("JSON-RPC server: {e:#}");
                }
            });
        }

        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let (status_tx, status_rx) = watch::channel(EngineState::Connecting);
        let engine = runtime.spawn(async move {
            let log_dir = log_dir.as_deref().map(Path::new);
            tracing::info!("media_path from config = {media_path:?}");
            if let Err(err) = engine::run_with_tun(
                &cfg,
                tun,
                log_dir,
                &log_prefix,
                media_path.as_deref(),
                cmd_rx,
                auto_call,
                &status_tx,
            )
            .await
            {
                warn!("tunnel engine exited: {err:#}");
            }

            // Otherwise a dead-but-unreaped session leaves send_cmd() sending
            // commands into a cmd_rx nobody drains anymore, hanging the
            // calling thread on the response that never comes. Must not drop
            // `runtime` from here — see `finished`'s doc comment.
            task_finished.store(true, Ordering::Relaxed);
        });

        Ok(Self {
            runtime,
            cmd_tx,
            engine,
            status_rx,
            finished,
        })
    }

    /// True once the engine task has exited on its own — the caller should
    /// treat this session as unusable (drop it, or call `stop()`) rather
    /// than issue further `send_cmd` calls, which would otherwise hang
    /// waiting for a response that will never come.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// Signals shutdown, waits (bounded by [`STOP_TIMEOUT`]) for the engine
    /// task to finish, then tears down the background runtime.
    ///
    /// The join is what makes the engine's own teardown happen at all:
    /// `Runtime::shutdown_timeout` doesn't run tasks to completion, it
    /// drops them at their next yield. Going straight from
    /// `trigger_shutdown()` to runtime shutdown skipped the post-call
    /// `signaling.hangup()`/`close()`, `pc.close()` and `finalize_webm()`
    /// entirely — the remote peer was left with a dangling call until its
    /// own signaling timeout fired.
    pub fn stop(self) {
        crate::trigger_shutdown();
        let started = std::time::Instant::now();
        // The timeout's timer must be created inside the runtime context,
        // hence the async block rather than a bare timeout(...) argument.
        let engine = self.engine;
        let joined = self
            .runtime
            .block_on(async move { tokio::time::timeout(STOP_TIMEOUT, engine).await });
        if joined.is_err() {
            warn!("engine did not stop within {STOP_TIMEOUT:?}; tearing runtime down anyway");
        }
        self.runtime
            .shutdown_timeout(STOP_TIMEOUT.saturating_sub(started.elapsed()));
    }

    /// Current engine status as the same JSON `DaemonCmd::Status` would
    /// return, read from the engine's published snapshot rather than
    /// through `cmd_tx` — so it never blocks on the engine being ready
    /// to service commands. Safe to call from a UI thread.
    pub fn status(&self) -> Value {
        engine::status_json(&self.status_rx.borrow())
    }

    pub fn send_cmd(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<Value>>) -> DaemonCmd,
    ) -> Result<Value> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .blocking_send(make(resp_tx))
            .map_err(|_| anyhow!("engine not accepting commands"))?;
        self.runtime
            .block_on(resp_rx)
            .map_err(|_| anyhow!("engine dropped response"))?
    }
}
