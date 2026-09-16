//! JNI entry points for driving the tunnel engine from an Android
//! `VpnService`. Kotlin owns the TUN fd (from `VpnService.Builder.establish()`)
//! and is expected to look like:
//!
//! ```kotlin
//! object TunnelNative {
//!     init { System.loadLibrary("anazoa_tun") }
//!     external fun nativeStart(configPath: String, tunFd: Int, appContext: Context): Boolean
//!     external fun nativeStop()
//!     external fun nativeCall(peerId: Long): Boolean   // 0 = configured remote-peer-id
//!     external fun nativeAnswer(secs: Long, forever: Boolean): Boolean   // forever ignores secs; else secs < 0 = anytime, >= 0 = window in seconds
//!     external fun nativeHangup(): Boolean
//!     external fun nativeStatus(): String
//!     external fun nativeRecentLog(): String
//! }
//! ```
//!
//! `Java_org_anazoa_vpn_TunnelNative_*` below must match that package/class
//! exactly (JNI resolves by mangled symbol name); rename both sides together
//! if the app uses a different one.
//!
//! `config_path` points to a TOML file in the same shape as the desktop
//! `anazoa.toml`; the app is expected to write one to its private storage
//! (e.g. `context.filesDir`) before calling `nativeStart`.

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Result, anyhow, bail};
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jlong, jstring};
use tokio::sync::oneshot;
use tracing::warn;

use crate::daemon::DaemonCmd;
use crate::engine;
use crate::host::TunnelSession;
use crate::protozoa::tunnel::{keep_opus_hooks_linked, keep_vp9_hooks_linked};

static RUNNING: OnceLock<Mutex<Option<TunnelSession>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<TunnelSession>> {
    RUNNING.get_or_init(|| Mutex::new(None))
}

/// Poison-tolerant: a panic while holding the slot (now survivable, see
/// `catch_jni`) must not turn every later JNI call into another panic.
fn lock_slot() -> MutexGuard<'static, Option<TunnelSession>> {
    slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs a JNI entry point body, converting a Rust panic into a Java
/// `IllegalStateException` plus `fallback` instead of tearing down the app
/// process. Only meaningful because the Android library is built with
/// `panic = "unwind"` (`[profile.android]`); under the workspace release
/// profile's `panic = "abort"` a panic would never reach this.
fn catch_jni<T>(env: &mut JNIEnv, fallback: T, body: impl FnOnce(&mut JNIEnv) -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(env))) {
        Ok(value) => value,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            warn!("anazoa-tun panicked: {msg}");
            throw_and(env, anyhow!("anazoa-tun panicked: {msg}"), fallback)
        }
    }
}

/// Drops a `TunnelSession` whose engine task already finished on its own.
/// Only call this from a plain JNI-calling thread (any of the functions in
/// this file) — never from within the runtime's own async context (see
/// `TunnelSession::is_finished`'s doc comment for why).
fn reap_finished(guard: &mut MutexGuard<'_, Option<TunnelSession>>) {
    if guard.as_ref().is_some_and(TunnelSession::is_finished) {
        guard.take();
    }
}

fn throw_and<T>(env: &mut JNIEnv, err: anyhow::Error, fallback: T) -> T {
    let _ = env.throw_new("java/lang/IllegalStateException", format!("{err:#}"));
    fallback
}

/// Starts the tunnel using an already-established VPN TUN fd.
///
/// # Safety (JNI contract, not expressible in the signature)
/// `tun_fd` must be a valid, open file descriptor for the VPN TUN device;
/// ownership transfers to the Rust side, which closes it on `nativeStop`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    config_path: JString<'local>,
    tun_fd: jint,
    app_context: JObject<'local>,
) -> jboolean {
    catch_jni(&mut env, JNI_FALSE, |env| {
        match start(env, &config_path, tun_fd as RawFd, &app_context) {
            Ok(()) => JNI_TRUE,
            Err(err) => throw_and(env, err, JNI_FALSE),
        }
    })
}

fn start(
    env: &mut JNIEnv,
    config_path: &JString,
    tun_fd: RawFd,
    app_context: &JObject,
) -> Result<()> {
    // Take ownership right away so every early-return below closes the fd.
    // Kotlin detached it from its ParcelFileDescriptor before calling us and
    // will never close it; before this, a config that failed to parse (or
    // any other failure ahead of `from_fd`) leaked one VPN fd per attempt.
    // SAFETY: `start`'s caller contract (see nativeStart) guarantees tun_fd
    // is a valid, open fd whose ownership transfers here.
    let tun_fd = unsafe { OwnedFd::from_raw_fd(tun_fd) };

    let mut guard = lock_slot();
    reap_finished(&mut guard);
    if guard.is_some() {
        bail!("tunnel already running");
    }

    // libwebrtc's Android JNI helpers (including the ClassLoader::FindClass
    // path every PeerConnectionFactory/PeerConnection construction goes
    // through) cache a JavaVM reference the first time this runs; without
    // it, calling into them from a thread Java didn't spawn — every Tokio
    // worker thread here — segfaults on a null pointer inside WebRTC's own
    // class loader cache instead of failing gracefully. Must happen before
    // any WebRTC construction, so first thing here is as good as anywhere;
    // it's idempotent on the C++ side, so a second nativeStart call is fine.
    let vm_ptr = env
        .get_java_vm()
        .map_err(|e| anyhow!("get JavaVM: {e}"))?
        .get_java_vm_pointer() as *mut webrtc_sys::android::ffi::JavaVM;
    let context_ptr = app_context.as_raw() as usize;
    if !unsafe { webrtc_sys::android::ffi::init_android_context(vm_ptr, context_ptr) } {
        warn!("init_android_context failed; continuing (JVM init still happened)");
    }

    let config_path: String = env
        .get_string(config_path)
        .map_err(|e| anyhow!("invalid config path: {e}"))?
        .into();

    let cfg: crate::Config = anazoa_config::load_config(&config_path)?;
    // Both re-applied on every start: this process outlives sessions, so
    // config edits between them must take effect.
    anazoa_config::init_logging(&cfg.auth.debug.level);
    anazoa_auth::signaling::set_log_signaling_ws(cfg.auth.debug.log_signaling_ws);
    keep_vp9_hooks_linked();
    keep_opus_hooks_linked();
    engine::init_noise_from_config(&cfg)?;

    let log_dir = cfg.auth.debug.log_dir.clone();
    let log_prefix = cfg
        .auth
        .debug
        .log_prefix
        .clone()
        .unwrap_or_else(|| "anazoa".to_string());
    // media path comes straight from the config's `media` field, same as
    // the desktop CLI — AnazoaVpnService.kt rewrites it to point at
    // whatever the user picked via "Load media file..." before writing
    // the config file this was parsed from.
    let media_path = cfg.media.clone();

    let session = TunnelSession::start(
        cfg,
        // Ownership moves on into the AsyncDevice, which closes it on drop.
        // `from_fd` takes a raw fd, so the OwnedFd is released first; tun_rs
        // wraps it in its own owning handle before anything can fail, so
        // a failed `from_fd` closes it too — no cleanup needed here.
        move || {
            let raw = tun_fd.into_raw_fd();
            // SAFETY: `raw` is the valid, open fd we just took ownership of
            // via OwnedFd above, and nothing else closes it.
            unsafe { tun_rs::AsyncDevice::from_fd(raw) }
                .map_err(|err| anyhow!("wrap VPN tun fd {raw}: {err:#}"))
        },
        log_dir,
        log_prefix,
        media_path,
        // auto_call=true: Android is always the caller side, with no
        // anazoa-ctl-equivalent separate "call" step — Connect both starts
        // the daemon and places the call once it's idle.
        true,
        // No ctl socket: Kotlin drives this session via direct JNI calls
        // (see `send_cmd` below), not an external RPC socket.
        None,
    )?;

    *guard = Some(session);
    Ok(())
}

/// Signals shutdown and tears down the background runtime. Bounded wait:
/// engine teardown is normally sub-second, but this must never hang the
/// calling Java thread indefinitely.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeStop<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) {
    catch_jni(&mut env, (), |_| {
        let mut guard = lock_slot();
        reap_finished(&mut guard);
        let session = guard.take();
        drop(guard);
        if let Some(session) = session {
            session.stop();
        }
    })
}

fn send_cmd(
    make: impl FnOnce(oneshot::Sender<Result<serde_json::Value>>) -> DaemonCmd,
) -> Result<serde_json::Value> {
    let mut guard = lock_slot();
    reap_finished(&mut guard);
    let session = guard
        .as_ref()
        .ok_or_else(|| anyhow!("tunnel not running"))?;
    session.send_cmd(make)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeCall<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    peer_id: jlong,
) -> jboolean {
    let peer_id = if peer_id == 0 { None } else { Some(peer_id) };
    catch_jni(&mut env, JNI_FALSE, |env| {
        match send_cmd(|resp| DaemonCmd::Call { peer_id, resp }) {
            Ok(_) => JNI_TRUE,
            Err(err) => throw_and(env, err, JNI_FALSE),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeAnswer<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    secs: jlong,
    forever: jboolean,
) -> jboolean {
    let secs = if secs < 0 { None } else { Some(secs as u64) };
    let forever = forever == JNI_TRUE;
    catch_jni(&mut env, JNI_FALSE, |env| {
        match send_cmd(|resp| DaemonCmd::Answer {
            secs,
            forever,
            resp,
        }) {
            Ok(_) => JNI_TRUE,
            Err(err) => throw_and(env, err, JNI_FALSE),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeHangup<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    catch_jni(&mut env, JNI_FALSE, |env| {
        match send_cmd(|resp| DaemonCmd::Hangup { resp }) {
            Ok(_) => JNI_TRUE,
            Err(err) => throw_and(env, err, JNI_FALSE),
        }
    })
}

/// Reads the engine's published state snapshot rather than sending a
/// `DaemonCmd::Status` — the UI thread polls this, and the engine doesn't
/// service `cmd_rx` while it's blocked in media setup, call setup or
/// post-call teardown (long enough to ANR with a large media file).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeStatus<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    // try_lock, never lock: the UI thread polls this, and a concurrent
    // start() holds the slot for the whole runtime/engine spin-up (stop()
    // releases it before its bounded wait, but still takes it briefly).
    // Blocking here would be an ANR; report a transient "connecting" instead.
    let value = match slot().try_lock() {
        Ok(mut guard) => {
            reap_finished(&mut guard);
            guard.as_ref().map(TunnelSession::status)
        }
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            let mut guard = poisoned.into_inner();
            reap_finished(&mut guard);
            guard.as_ref().map(TunnelSession::status)
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            Some(serde_json::json!({"state": "connecting"}))
        }
    }
    .unwrap_or_else(|| serde_json::json!({"state": "error", "error": "tunnel not running"}));
    match env.new_string(value.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Recent log lines (see `anazoa_config::recent_log_lines`), newline-joined,
/// oldest first. Unlike the other commands this doesn't go through the
/// engine's command channel, so it's safe to call at any time — including
/// before the first `nativeStart`, when it just returns an empty string.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_anazoa_vpn_TunnelNative_nativeRecentLog<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let lines = anazoa_config::recent_log_lines().join("\n");
    match env.new_string(lines) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}
