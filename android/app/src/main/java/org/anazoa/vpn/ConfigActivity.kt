package org.anazoa.vpn

import android.content.SharedPreferences
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import java.io.File

class ConfigActivity : AppCompatActivity() {

    private lateinit var prefs: SharedPreferences
    private lateinit var cidrInput: EditText
    private lateinit var configInput: EditText
    private lateinit var mediaStatusText: TextView

    private val cidrRegex = Regex("""^(\d{1,3}(?:\.\d{1,3}){3})/(\d{1,2})$""")

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

    // See MainActivity's loadMediaFile for why this copies bytes to a fixed
    // app-private path rather than keeping the picked content:// URI.
    private val loadMediaFile =
        registerForActivityResult(ActivityResultContracts.GetContent()) { uri ->
            if (uri == null) return@registerForActivityResult
            val copied = runCatching {
                contentResolver.openInputStream(uri)?.use { input ->
                    File(filesDir, AnazoaVpnService.MEDIA_FILE_NAME).outputStream().use { output ->
                        input.copyTo(output)
                    }
                }
            }.getOrNull()
            if (copied == null) {
                Toast.makeText(this, getString(R.string.error_load_media), Toast.LENGTH_LONG).show()
            } else {
                updateMediaStatus()
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_config)
        prefs = getSharedPreferences("anazoa_vpn", MODE_PRIVATE)

        cidrInput = findViewById(R.id.cidr_input)
        configInput = findViewById(R.id.config_input)
        mediaStatusText = findViewById(R.id.media_status_text)

        val address = prefs.getString("local_address", null) ?: getString(R.string.default_cidr).substringBefore("/")
        val prefixLength = prefs.getString("prefix_length", null) ?: getString(R.string.default_cidr).substringAfter("/")
        cidrInput.setText("$address/$prefixLength")
        configInput.setText(prefs.getString("config", ""))

        findViewById<Button>(R.id.import_config_button).setOnClickListener {
            loadConfigFile.launch("*/*")
        }

        findViewById<Button>(R.id.load_media_button).setOnClickListener {
            loadMediaFile.launch("*/*")
        }

        updateMediaStatus()
    }

    override fun onPause() {
        super.onPause()
        savePrefs()
    }

    private fun updateMediaStatus() {
        val mediaFile = File(filesDir, AnazoaVpnService.MEDIA_FILE_NAME)
        mediaStatusText.text = if (mediaFile.exists()) {
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
            // MainActivity's connect() can't parse.
            Toast.makeText(this, getString(R.string.error_invalid_cidr), Toast.LENGTH_LONG).show()
        }

        editor.apply()
    }
}
