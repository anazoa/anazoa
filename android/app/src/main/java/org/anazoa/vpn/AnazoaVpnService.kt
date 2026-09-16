package org.anazoa.vpn

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageManager
import android.net.VpnService
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat
import java.io.File
import java.util.concurrent.Executors
import org.json.JSONException
import org.json.JSONObject

class AnazoaVpnService : VpnService() {

    companion object {
        const val ACTION_CONNECT = "org.anazoa.vpn.action.CONNECT"
        const val ACTION_DISCONNECT = "org.anazoa.vpn.action.DISCONNECT"
        const val EXTRA_CONFIG = "config"
        const val EXTRA_LOCAL_ADDRESS = "local_address"
        const val EXTRA_PREFIX_LENGTH = "prefix_length"
        const val EXTRA_ROUTES = "routes"

        /** Shared with MainActivity's "Load media file..." picker. */
        const val MEDIA_FILE_NAME = "media.opus"

        private const val TAG = "AnazoaVpnService"
        private const val CHANNEL_ID = "anazoa_vpn_status"
        private const val NOTIFICATION_ID = 1
        private val mediaLineRegex = Regex("""^\s*media\s*=.*$""")
        private const val STATUS_POLL_MS = 2000L

        /**
         * Parses a [TunnelNative.nativeStatus] blob into its `state`, or
         * null if the engine isn't running (`state == "error"` is
         * android.rs's "no session" sentinel, also returned once a session
         * has died and been reaped). Shared with MainActivity so both sides
         * agree on what "running" means.
         */
        fun engineState(status: String?): String? {
            if (status.isNullOrBlank()) return null
            return try {
                JSONObject(status).optString("state").takeIf { it.isNotEmpty() && it != "error" }
            } catch (e: JSONException) {
                null
            }
        }

        fun engineRunning(): Boolean =
            engineState(runCatching { TunnelNative.nativeStatus() }.getOrNull()) != null

        /**
         * The one thread that calls nativeStart/nativeStop. Neither belongs
         * on the main thread: nativeStop joins the engine for up to its
         * STOP_TIMEOUT (5 s) — a Disconnect while the network was gone
         * froze the UI for all of it, an ANR if the user touched the screen
         * — and nativeStart spins up the runtime. Single-threaded and
         * process-wide (not per service instance) so calls stay strictly
         * ordered: the engine's shutdown signal is a process global, and a
         * Connect racing a still-running Stop would otherwise tear down the
         * *new* session.
         */
        private val native = Executors.newSingleThreadExecutor { r -> Thread(r, "anazoa-native") }
    }

    private val handler = Handler(Looper.getMainLooper())

    // Keeps the notification in step with the engine and, more importantly,
    // notices when the engine has died on its own (bad media file, tunnel
    // bridge failure, ...) after nativeStart already returned true. A
    // local "running" boolean can't see that: it stayed true, the
    // notification kept saying Connected, and the next CONNECT was ignored
    // as "already running" — with the UI already showing a Connect button,
    // there was no way to send the DISCONNECT that would have reset it.
    private val statusPoll = object : Runnable {
        override fun run() {
            val state = engineState(runCatching { TunnelNative.nativeStatus() }.getOrNull())
            if (state == null) {
                Log.w(TAG, "engine stopped on its own: ${lastNativeError()}")
                fail("${getString(R.string.status_failed)}: ${lastNativeError()}")
                return
            }
            updateNotification(describeState(state))
            handler.postDelayed(this, STATUS_POLL_MS)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_CONNECT -> {
                val config = intent.getStringExtra(EXTRA_CONFIG)
                if (config.isNullOrBlank()) {
                    Log.w(TAG, "CONNECT with no config, ignoring")
                    stopSelf()
                    return START_NOT_STICKY
                }
                val localAddress = intent.getStringExtra(EXTRA_LOCAL_ADDRESS) ?: "10.77.0.2"
                val prefixLength = intent.getIntExtra(EXTRA_PREFIX_LENGTH, 30)
                val routes = intent.getStringExtra(EXTRA_ROUTES)
                    ?.split(",")
                    ?.map { it.trim() }
                    ?.filter { it.isNotEmpty() }
                    ?: emptyList()
                startTunnel(config, localAddress, prefixLength, routes)
            }
            ACTION_DISCONNECT -> stopTunnel()
        }
        return START_STICKY
    }

    private fun startTunnel(
        config: String,
        localAddress: String,
        prefixLength: Int,
        routes: List<String>,
    ) {
        ensureNotificationChannel()
        startForeground(NOTIFICATION_ID, buildNotification(getString(R.string.status_connecting)))

        // The Rust side just needs a path it can load_config() from; reuse
        // the existing desktop TOML format unchanged rather than inventing a
        // parse-from-string entry point.
        val configFile = File(filesDir, "anazoa.toml")
        configFile.writeText(withMediaOverride(config))

        val builder = Builder()
            .setSession(getString(R.string.app_name))
            .addAddress(localAddress, prefixLength)
            // Matches scripts/setup-tun-test.sh's default MTU for this
            // tunnel; the VP9-carrier framing has its own overhead budget
            // tuned around this.
            .setMtu(1280)

        // Without this, a 0.0.0.0/0 route would also capture this app's own
        // outbound sockets — including the Max/WebRTC signaling connection
        // that anazoa_tun opens to *carry* the tunnel. That traffic would
        // loop back into the TUN device it's supposed to be feeding, which
        // never resolves: the call can't reach the real internet, so the
        // tunnel it's meant to carry never comes up either. Excluding our
        // own package keeps everything this process opens on the real
        // network while everything else (once routed via 0.0.0.0/0) goes
        // through the tunnel.
        try {
            builder.addDisallowedApplication(packageName)
        } catch (e: PackageManager.NameNotFoundException) {
            // Can't happen for our own packageName, but addDisallowedApplication
            // declares it.
            Log.e(TAG, "addDisallowedApplication(self) failed: $e")
        }

        for (route in routes) {
            val parts = route.split("/")
            val addr = parts.getOrNull(0)?.trim().orEmpty()
            val routePrefix = parts.getOrNull(1)?.trim()?.toIntOrNull() ?: 32
            if (addr.isNotEmpty()) {
                builder.addRoute(addr, routePrefix)
            }
        }

        native.execute {
            // Ask the engine rather than a local flag: nativeStatus() reaps
            // a session whose engine already exited, so a dead session
            // doesn't block reconnecting (see statusPoll). Checked here, on
            // the native thread, so it runs *after* any Stop queued ahead of
            // us — a quick Disconnect→Connect must not see the old session.
            if (engineRunning()) {
                Log.w(TAG, "startTunnel called while already running, ignoring")
                return@execute
            }

            val pfd = try {
                builder.establish()
            } catch (e: Exception) {
                Log.e(TAG, "VpnService.Builder.establish() threw", e)
                null
            }
            if (pfd == null) {
                Log.e(TAG, "VpnService.Builder.establish() returned null")
                handler.post { fail(getString(R.string.status_failed)) }
                return@execute
            }

            // Ownership of the fd transfers to the Rust side (tun_rs::AsyncDevice
            // closes it on drop, triggered from nativeStop). detachFd() so this
            // ParcelFileDescriptor doesn't also try to close it — a double-close
            // on a possibly-already-reused fd is exactly the kind of native bug
            // that's miserable to track down.
            val fd = pfd.detachFd()

            // nativeStart reports failure (e.g. a config that doesn't parse) by
            // throwing across the JNI boundary (see android.rs's throw_and), not
            // by some Kotlin-side error type — an uncaught exception here would
            // crash the whole app instead of just failing to connect, which is
            // exactly what happened before this try/catch existed.
            val started = try {
                TunnelNative.nativeStart(configFile.absolutePath, fd, applicationContext)
            } catch (e: Exception) {
                Log.e(TAG, "nativeStart threw", e)
                false
            }
            handler.post {
                if (!started) {
                    Log.e(TAG, "nativeStart failed")
                    fail("${getString(R.string.status_failed)}: ${lastNativeError()}")
                } else {
                    handler.removeCallbacks(statusPoll)
                    handler.postDelayed(statusPoll, STATUS_POLL_MS)
                }
            }
        }
    }

    // The tunnel only carries traffic in `in_call`; everything before it
    // (session connect, dialing, signaling setup, and the redial backoff
    // that shows up as `idle` after a call drops) is still "connecting"
    // from the user's point of view.
    private fun describeState(state: String): String = when (state) {
        "in_call" -> getString(R.string.status_connected)
        else -> getString(R.string.status_connecting)
    }

    // Rewrites/adds the config's `media` line to point at the file
    // MainActivity's "Load media file..." picker copied to app-private
    // storage, if any. Without a real video (raylib's Memory platform only
    // ever renders a synthetic audio-reactive visualizer, never a camera
    // feed), the media file is what makes the tunnel's cover video call
    // carry actual encoded content instead of silence/a black frame — see
    // engine.rs's drive_call for why the tunnel itself still works either
    // way, and RaylibMedia::next_video_frame for what the file *does* drive.
    // Any `media = ...` already in the pasted config is intentionally
    // overridden: a desktop-only path in it would just fail to open here.
    private fun withMediaOverride(config: String): String {
        val mediaFile = File(filesDir, MEDIA_FILE_NAME)
        if (!mediaFile.exists()) return config
        val withoutMediaLine = config.lineSequence()
            .filterNot { mediaLineRegex.matches(it) }
            .joinToString("\n")
        // Must be *prepended*, not appended: this config ends with a
        // [fingerprint] table, and TOML scopes every key to the nearest
        // preceding [table] header — appending here silently made this
        // fingerprint.media instead of the top-level media the Config struct
        // actually reads, with no parse error to show for it (FingerprintConfig
        // doesn't reject unknown fields, so it just vanished). A key is only
        // valid at the top level before the first [table] header, so
        // prepending is the only placement that's safe regardless of what
        // tables the rest of the file has.
        return "media = \"${mediaFile.absolutePath}\"\n$withoutMediaLine\n"
    }

    private fun lastNativeError(): String =
        runCatching { TunnelNative.nativeRecentLog() }
            .getOrNull()
            ?.lines()
            ?.lastOrNull { it.isNotBlank() }
            ?: "see log"

    private fun fail(status: String) {
        handler.removeCallbacks(statusPoll)
        // A session that failed partway (e.g. the engine died after
        // nativeStart) may still hold the TUN fd; nativeStop is a no-op
        // when nothing is running, so always let it clean up.
        stopNative()
        updateNotification(status)
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun stopTunnel() {
        handler.removeCallbacks(statusPoll)
        // Unconditional: nativeStop is a no-op when nothing is running, and
        // gating it on a local flag is what left dead sessions unreaped.
        stopNative()
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    // Queued, not awaited: the service can go away while the engine is
    // still winding down; the `native` thread outlives it and a later
    // Connect queues behind this on the same thread.
    private fun stopNative() {
        native.execute {
            runCatching { TunnelNative.nativeStop() }
                .onFailure { Log.e(TAG, "nativeStop threw", it) }
        }
    }

    /** Called when the user revokes VPN permission (e.g. another VPN app took over). */
    override fun onRevoke() {
        stopTunnel()
        super.onRevoke()
    }

    override fun onDestroy() {
        stopTunnel()
        super.onDestroy()
    }

    private fun ensureNotificationChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    getString(R.string.app_name),
                    NotificationManager.IMPORTANCE_LOW,
                )
            )
        }
    }

    private fun buildNotification(status: String): Notification {
        val openApp = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(status)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentIntent(openApp)
            .setOngoing(true)
            .build()
    }

    private fun updateNotification(status: String) {
        ensureNotificationChannel()
        val manager = getSystemService(NotificationManager::class.java)
        manager.notify(NOTIFICATION_ID, buildNotification(status))
    }
}
