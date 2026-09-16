package org.anazoa.vpn

import android.content.Intent
import android.content.SharedPreferences
import android.content.res.ColorStateList
import android.graphics.Color
import android.net.VpnService
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.widget.Button
import android.widget.TextView
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat

class MainActivity : AppCompatActivity() {

    private lateinit var prefs: SharedPreferences
    private lateinit var statusText: TextView
    private lateinit var connectButton: Button

    private val handler = Handler(Looper.getMainLooper())
    private var connected = false

    // A single Runnable instance, so stopPolling() can remove exactly the
    // tick that is queued. The previous per-startPolling() object plus a
    // `polling` flag leaked one poller per pause/resume: onPause cleared the
    // flag but left the old tick queued, onResume re-set the flag before that
    // tick ran, so it passed its own `if (!polling) return` and kept going
    // alongside the new one.
    private val statusTick = object : Runnable {
        override fun run() {
            val state = AnazoaVpnService.engineState(runCatching { TunnelNative.nativeStatus() }.getOrNull())
            if (state == null) {
                setConnected(false)
            } else {
                setStatus(describeState(state))
                handler.postDelayed(this, STATUS_POLL_MS)
            }
        }
    }

    companion object {
        private const val STATUS_POLL_MS = 2000L
    }

    private val vpnPermission =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            if (result.resultCode == RESULT_OK) {
                startTunnelService()
            } else {
                setStatus(getString(R.string.status_failed))
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        prefs = getSharedPreferences("anazoa_vpn", MODE_PRIVATE)

        statusText = findViewById(R.id.status_text)
        connectButton = findViewById(R.id.connect_button)

        connectButton.setOnClickListener {
            if (connected) disconnect() else connect()
        }

        findViewById<Button>(R.id.config_button).setOnClickListener {
            startActivity(Intent(this, ConfigActivity::class.java))
        }

        findViewById<Button>(R.id.log_button).setOnClickListener {
            startActivity(Intent(this, LogActivity::class.java))
        }
    }

    override fun onResume() {
        super.onResume()
        // The service may still be running from a previous activity
        // instance (e.g. after rotation, or the app was reopened); probe it
        // rather than assuming we're disconnected.
        val state = AnazoaVpnService.engineState(runCatching { TunnelNative.nativeStatus() }.getOrNull())
        setConnected(state != null)
        if (state != null) setStatus(describeState(state))
    }

    override fun onPause() {
        super.onPause()
        stopPolling()
    }

    private fun connect() {
        val config = prefs.getString("config", "")
        if (config.isNullOrBlank()) {
            Toast.makeText(this, getString(R.string.error_no_config), Toast.LENGTH_LONG).show()
            return
        }
        val consent = VpnService.prepare(this)
        if (consent != null) {
            vpnPermission.launch(consent)
        } else {
            startTunnelService()
        }
    }

    private fun startTunnelService() {
        setStatus(getString(R.string.status_connecting))
        val intent = Intent(this, AnazoaVpnService::class.java).apply {
            action = AnazoaVpnService.ACTION_CONNECT
            putExtra(AnazoaVpnService.EXTRA_CONFIG, prefs.getString("config", ""))
            putExtra(AnazoaVpnService.EXTRA_LOCAL_ADDRESS, prefs.getString("local_address", "10.77.0.2"))
            putExtra(
                AnazoaVpnService.EXTRA_PREFIX_LENGTH,
                prefs.getString("prefix_length", "30")?.toIntOrNull() ?: 30,
            )
            // Always the default route: anything not addressed to this
            // device's own signaling connection goes through the tunnel.
            // AnazoaVpnService.addDisallowedApplication(packageName) is what
            // keeps that signaling connection itself off the TUN device.
            putExtra(AnazoaVpnService.EXTRA_ROUTES, "0.0.0.0/0")
        }
        ContextCompat.startForegroundService(this, intent)
        setConnected(true)
    }

    private fun disconnect() {
        startService(Intent(this, AnazoaVpnService::class.java).setAction(AnazoaVpnService.ACTION_DISCONNECT))
        setConnected(false)
    }

    private fun setConnected(value: Boolean) {
        connected = value
        connectButton.text = getString(if (value) R.string.action_disconnect else R.string.action_connect)
        ViewCompat.setBackgroundTintList(
            connectButton,
            ColorStateList.valueOf(if (value) Color.parseColor("#c62828") else Color.parseColor("#2e7d32")),
        )
        if (value) {
            startPolling()
        } else {
            stopPolling()
            setStatus(getString(R.string.status_idle))
        }
    }

    private fun startPolling() {
        // Remove-then-post keeps exactly one tick queued regardless of how
        // many times this is called.
        handler.removeCallbacks(statusTick)
        handler.postDelayed(statusTick, STATUS_POLL_MS)
    }

    private fun stopPolling() {
        handler.removeCallbacks(statusTick)
    }

    private fun setStatus(text: String) {
        statusText.text = text
    }

    // Only `in_call` means the tunnel is actually carrying traffic. The
    // engine passes through `connecting` (OneMe session), `dialing` /
    // `answering` (signaling setup) and — after a call drops — `idle` for
    // the redial backoff; mapping all of those to "Connected" hid the
    // difference between a working tunnel and one that was silently
    // waiting to retry. The engine is running throughout (the button stays
    // Disconnect), it just isn't connected yet.
    private fun describeState(state: String): String = when (state) {
        "in_call" -> getString(R.string.status_connected)
        else -> getString(R.string.status_connecting)
    }
}
