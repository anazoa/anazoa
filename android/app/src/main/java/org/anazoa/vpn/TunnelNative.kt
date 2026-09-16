package org.anazoa.vpn

import android.content.Context

/**
 * JNI bridge to the `anazoa_tun` Rust library (tun/src/android.rs).
 * Symbol names are derived from this package + class name — if either is
 * renamed, the `Java_org_anazoa_vpn_TunnelNative_*` exports in android.rs
 * must be renamed to match.
 */
object TunnelNative {
    init {
        System.loadLibrary("anazoa_tun")
    }

    /**
     * Starts the tunnel engine. `tunFd` must be a fresh, valid fd from
     * [android.net.VpnService.Builder.establish] — ownership transfers to
     * Rust, which closes it on [nativeStop]. Do not close `tunFd` yourself
     * after calling this (detach it from any ParcelFileDescriptor first).
     *
     * `appContext` is handed to libwebrtc's Android init (JavaVM +
     * ContextUtils) before any PeerConnection gets built — without it,
     * WebRTC's JNI ClassLoader cache is never populated, and calling into it
     * from a thread Java didn't spawn (every Tokio worker thread) segfaults.
     * Pass `applicationContext`, not an Activity/Service instance — this is
     * cached for the process lifetime, and holding a shorter-lived Context
     * here would leak it.
     */
    external fun nativeStart(configPath: String, tunFd: Int, appContext: Context): Boolean

    /** Signals shutdown and blocks briefly while the engine tears down. */
    external fun nativeStop()

    /** peerId == 0 uses the config's configured remote-peer-id. */
    external fun nativeCall(peerId: Long): Boolean

    /**
     * `forever = true` auto-answers every incoming call until re-armed or
     * closed (ignores [secs]). Otherwise arms a single incoming call: `secs
     * < 0` with no expiry ("anytime"), `secs >= 0` for a window of that many
     * seconds.
     */
    external fun nativeAnswer(secs: Long, forever: Boolean): Boolean

    external fun nativeHangup(): Boolean

    /** JSON status blob (see tun/src/daemon.rs DaemonCmd::Status). */
    external fun nativeStatus(): String

    /**
     * Recent engine log lines, newline-joined, oldest first. Safe to call
     * any time, including before [nativeStart] (returns "" until then).
     */
    external fun nativeRecentLog(): String
}
