package org.anazoa.vpn

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.SharedPreferences
import android.content.res.ColorStateList
import android.graphics.Color
import android.net.VpnService
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.Selection
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import com.google.android.material.bottomnavigation.BottomNavigationView
import org.json.JSONObject
import java.io.File
import java.util.concurrent.Executors

class MainActivity : AppCompatActivity() {

    private lateinit var prefs: SharedPreferences
    private lateinit var bottomNav: BottomNavigationView
    private lateinit var tabs: Map<Int, View>

    // Main tab
    private lateinit var statusText: TextView
    private lateinit var connectButton: Button
    private lateinit var statsTable: View
    private lateinit var statsDuration: TextView
    private lateinit var statsUp: TextView
    private lateinit var statsDown: TextView
    private lateinit var statsCapacity: TextView
    private lateinit var statsLatency: TextView
    private lateinit var statsVideo: TextView
    private lateinit var connectionStats: ConnectionStats

    // Settings tab
    private lateinit var cidrInput: EditText
    private lateinit var configInput: EditText
    private lateinit var mediaStatusText: TextView
    private lateinit var loadMediaButton: Button

    // Log tab
    private lateinit var logScroll: ScrollView
    private lateinit var logText: TextView

    private val handler = Handler(Looper.getMainLooper())
    private var connected = false
    private var currentTab = R.id.nav_main

    private val cidrRegex = Regex("""^(\d{1,3}(?:\.\d{1,3}){3})/(\d{1,2})$""")

    // A single Runnable instance, so stopPolling() can remove exactly the
    // tick that is queued. The previous per-startPolling() object plus a
    // `polling` flag leaked one poller per pause/resume: onPause cleared the
    // flag but left the old tick queued, onResume re-set the flag before that
    // tick ran, so it passed its own `if (!polling) return` and kept going
    // alongside the new one.
    private val statusTick = object : Runnable {
        override fun run() {
            val state = AnazoaVpnService.currentState()
            if (state == null) {
                setConnected(false)
            } else {
                setStatus(describeState(state))
                updateStats()
                handler.postDelayed(this, STATUS_POLL_MS)
            }
        }
    }

    // Same single-instance rule as statusTick. Runs only on the Log tab.
    private val logTick = object : Runnable {
        override fun run() {
            updateLog()
            handler.postDelayed(this, LOG_POLL_MS)
        }
    }

    // Back from Settings or Log returns to Main instead of closing the app.
    private val backToMain = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() {
            bottomNav.selectedItemId = R.id.nav_main
        }
    }

    companion object {
        private const val STATUS_POLL_MS = 2000L
        private const val LOG_POLL_MS = 2000L
        private const val STATE_TAB = "tab"

        private val mediaCopier = Executors.newSingleThreadExecutor { r -> Thread(r, "anazoa-media-copy") }

        // Process-wide, not per instance: a rotation mid-copy recreates the
        // activity, and the new one must still show the copy as running.
        @Volatile
        private var mediaCopyInProgress = false

        // The resumed instance, for the copy's completion to report to.
        // Main thread only; cleared in onPause, so it never outlives one.
        private var resumed: MainActivity? = null
    }

    private val vpnPermission =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            if (result.resultCode == RESULT_OK) {
                startTunnelService()
            } else {
                setStatus(getString(R.string.status_failed))
            }
        }

    private val loadConfigFile =
        registerForActivityResult(ActivityResultContracts.GetContent()) { uri ->
            if (uri == null) return@registerForActivityResult
            val text = runCatching {
                contentResolver.openInputStream(uri)?.bufferedReader()?.use { it.readText() }
            }.getOrNull()
            if (text.isNullOrEmpty()) {
                Toast.makeText(this, getString(R.string.error_load_config), Toast.LENGTH_LONG).show()
            } else {
                configInput.setText(text)
                Toast.makeText(this, getString(R.string.toast_config_imported), Toast.LENGTH_SHORT).show()
            }
        }

    // Copies the picked file's bytes to a fixed app-private path rather than
    // keeping the content:// URI: the engine opens the file by path, and a
    // picker grant doesn't outlive the process anyway. Off the main thread,
    // since a recording is easily 100 MB, and into a temp file renamed into
    // place, so a Connect during the copy uses the previous file whole
    // rather than a truncated one.
    private val loadMediaFile =
        registerForActivityResult(ActivityResultContracts.GetContent()) { uri ->
            if (uri == null || mediaCopyInProgress) return@registerForActivityResult
            mediaCopyInProgress = true
            updateMediaStatus()
            val resolver = applicationContext.contentResolver
            val dir = filesDir
            mediaCopier.execute {
                val tmp = File(dir, "${AnazoaVpnService.MEDIA_FILE_NAME}.tmp")
                val copied = runCatching {
                    checkNotNull(resolver.openInputStream(uri)).use { input ->
                        tmp.outputStream().use { output -> input.copyTo(output) }
                    }
                    check(tmp.renameTo(File(dir, AnazoaVpnService.MEDIA_FILE_NAME)))
                }.isSuccess
                if (!copied) tmp.delete()
                handler.post {
                    mediaCopyInProgress = false
                    resumed?.onMediaCopied(copied)
                }
            }
        }

    private fun onMediaCopied(copied: Boolean) {
        updateMediaStatus()
        if (!copied) Toast.makeText(this, getString(R.string.error_load_media), Toast.LENGTH_LONG).show()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        prefs = getSharedPreferences("anazoa_vpn", MODE_PRIVATE)

        tabs = mapOf(
            R.id.nav_main to findViewById(R.id.tab_main),
            R.id.nav_settings to findViewById(R.id.tab_settings),
            R.id.nav_log to findViewById(R.id.tab_log),
        )

        statusText = findViewById(R.id.status_text)
        connectButton = findViewById(R.id.connect_button)
        statsTable = findViewById(R.id.stats_table)
        statsDuration = findViewById(R.id.stats_duration)
        statsUp = findViewById(R.id.stats_up)
        statsDown = findViewById(R.id.stats_down)
        statsCapacity = findViewById(R.id.stats_capacity)
        statsLatency = findViewById(R.id.stats_latency)
        statsVideo = findViewById(R.id.stats_video)
        connectionStats = ConnectionStats(this)
        cidrInput = findViewById(R.id.cidr_input)
        configInput = findViewById(R.id.config_input)
        mediaStatusText = findViewById(R.id.media_status_text)
        logScroll = findViewById(R.id.log_scroll)
        logText = findViewById(R.id.log_text)

        connectButton.setOnClickListener {
            if (connected) disconnect() else connect()
        }

        findViewById<Button>(R.id.log_clear_button).setOnClickListener {
            runCatching { TunnelNative.nativeClearLog() }
            logText.text = ""
        }

        findViewById<Button>(R.id.log_copy_button).setOnClickListener { copyLog() }

        val address = prefs.getString("local_address", null) ?: getString(R.string.default_cidr).substringBefore("/")
        val prefixLength = prefs.getString("prefix_length", null) ?: getString(R.string.default_cidr).substringAfter("/")
        cidrInput.setText("$address/$prefixLength")
        configInput.setText(prefs.getString("config", ""))

        findViewById<Button>(R.id.import_config_button).setOnClickListener {
            loadConfigFile.launch("*/*")
        }

        val fingerprintText = findViewById<EditText>(R.id.fingerprint_text)
        fingerprintText.keyListener = null
        fingerprintText.setText(DeviceFingerprint.toml(this))
        findViewById<Button>(R.id.copy_fingerprint_button).setOnClickListener {
            copyText(fingerprintText.text.toString(), R.string.toast_fingerprint_copied)
        }

        loadMediaButton = findViewById(R.id.load_media_button)
        loadMediaButton.setOnClickListener {
            loadMediaFile.launch("*/*")
        }

        updateMediaStatus()

        onBackPressedDispatcher.addCallback(this, backToMain)

        bottomNav = findViewById(R.id.bottom_nav)
        bottomNav.setOnItemSelectedListener { item ->
            showTab(item.itemId)
            true
        }
        // Setting selectedItemId runs the listener above, which the
        // BottomNavigationView's own state restore does not.
        bottomNav.selectedItemId = savedInstanceState?.getInt(STATE_TAB) ?: R.id.nav_main
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putInt(STATE_TAB, currentTab)
    }

    override fun onResume() {
        super.onResume()
        // The service may still be running from a previous activity
        // instance (e.g. after rotation, or the app was reopened); probe it
        // rather than assuming we're disconnected.
        val state = AnazoaVpnService.currentState()
        setConnected(state != null)
        if (state != null) {
            setStatus(describeState(state))
            updateStats()
        }
        startLogPolling()
        resumed = this
        updateMediaStatus()
    }

    override fun onPause() {
        super.onPause()
        resumed = null
        stopPolling()
        handler.removeCallbacks(logTick)
        if (currentTab == R.id.nav_settings) savePrefs()
    }

    private fun showTab(id: Int) {
        if (currentTab == R.id.nav_settings && id != R.id.nav_settings) savePrefs()
        currentTab = id
        for ((tabId, view) in tabs) {
            view.visibility = if (tabId == id) View.VISIBLE else View.GONE
        }
        backToMain.isEnabled = id != R.id.nav_main
        startLogPolling()
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
        AnazoaVpnService.startPending = true
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
            statsTable.visibility = View.INVISIBLE
            // A session that died on its own says why; a Disconnect, or a
            // fresh start, has no failure to show.
            val failure = AnazoaVpnService.lastFailure()
            setStatus(failure?.let { getString(R.string.status_failed_reason, it) } ?: getString(R.string.status_idle))
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

    private fun startLogPolling() {
        handler.removeCallbacks(logTick)
        if (currentTab == R.id.nav_log) handler.post(logTick)
    }

    private fun setStatus(text: String) {
        statusText.text = text
    }

    private fun updateLog() {
        val log = runCatching { TunnelNative.nativeRecentLog() }.getOrDefault("")

        // Replacing the text wipes any in-progress selection (the old
        // Spannable buffer is discarded outright), which is why a copy
        // gesture kept getting cut off mid-selection. Skip this tick
        // entirely while text is selected; refresh resumes once it's
        // cleared (e.g. right after the copy).
        if (Selection.getSelectionStart(logText.text) != Selection.getSelectionEnd(logText.text)) return

        // Only stick to the bottom if the view was already there. Otherwise
        // every update — even while the user has scrolled up to read
        // history — yanks the scroll position back to the end, which is
        // what read as "scrolls back and forth on itself".
        val wasAtBottom = !logScroll.canScrollVertically(1)
        logText.text = log
        if (wasAtBottom) {
            logScroll.post { logScroll.fullScroll(View.FOCUS_DOWN) }
        }
    }

    // Shown only while in a call; INVISIBLE rather than GONE keeps the
    // Connect button from jumping when it appears.
    private fun updateStats() {
        val status = runCatching { JSONObject(TunnelNative.nativeStatus()) }.getOrNull()
        val rows = status?.let { connectionStats.format(it, System.currentTimeMillis()) }
        if (rows == null) {
            statsTable.visibility = View.INVISIBLE
            return
        }
        statsDuration.text = rows.duration
        statsUp.text = rows.up
        statsDown.text = rows.down
        statsCapacity.text = rows.capacity
        statsLatency.text = rows.latency
        statsVideo.text = rows.video
        statsTable.visibility = View.VISIBLE
    }

    private fun copyLog() = copyText(logText.text.toString(), R.string.toast_log_copied)

    private fun copyText(text: String, toast: Int) {
        if (text.isEmpty()) return
        getSystemService(ClipboardManager::class.java)
            .setPrimaryClip(ClipData.newPlainText(getString(R.string.app_name), text))
        // Android 13+ confirms clipboard writes itself.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            Toast.makeText(this, getString(toast), Toast.LENGTH_SHORT).show()
        }
    }

    private fun updateMediaStatus() {
        val mediaFile = File(filesDir, AnazoaVpnService.MEDIA_FILE_NAME)
        loadMediaButton.isEnabled = !mediaCopyInProgress
        mediaStatusText.text = if (mediaCopyInProgress) {
            getString(R.string.media_status_copying)
        } else if (mediaFile.exists()) {
            getString(R.string.media_status_loaded, mediaFile.length().let { "%.1f MB".format(it / 1_000_000.0) })
        } else {
            getString(R.string.media_status_none)
        }
    }

    private fun savePrefs() {
        val editor = prefs.edit().putString("config", configInput.text.toString())

        val match = cidrRegex.matchEntire(cidrInput.text.toString().trim())
        if (match != null) {
            editor.putString("local_address", match.groupValues[1])
            editor.putString("prefix_length", match.groupValues[2])
        } else if (cidrInput.text.isNotBlank()) {
            // Keep whatever was previously saved rather than writing a value
            // connect() can't parse.
            Toast.makeText(this, getString(R.string.error_invalid_cidr), Toast.LENGTH_LONG).show()
        }

        editor.apply()
    }

    // Only `in_call` means the tunnel is actually carrying traffic. The
    // engine passes through `connecting` (OneMe session) and `dialing` /
    // `answering` (signaling setup); mapping all of those to "Connected"
    // hid the difference between a working tunnel and one still coming up. The engine is running throughout (the button stays
    // Disconnect), it just isn't connected yet.
    private fun describeState(state: String): String = when (state) {
        "in_call" -> getString(R.string.status_connected)
        else -> getString(R.string.status_connecting)
    }
}
