package org.anazoa.vpn

import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.Selection
import android.view.View
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity

class LogActivity : AppCompatActivity() {

    private lateinit var logScroll: ScrollView
    private lateinit var logText: TextView

    private val handler = Handler(Looper.getMainLooper())
    private var polling = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_log)

        logScroll = findViewById(R.id.log_scroll)
        logText = findViewById(R.id.log_text)
    }

    override fun onResume() {
        super.onResume()
        polling = true
        val tick = object : Runnable {
            override fun run() {
                if (!polling) return
                updateLog()
                handler.postDelayed(this, 2000)
            }
        }
        handler.post(tick)
    }

    override fun onPause() {
        super.onPause()
        polling = false
    }

    private fun updateLog() {
        // Replacing the text wipes any in-progress selection (the old
        // Spannable buffer is discarded outright), which is why a copy
        // gesture kept getting cut off mid-selection. Skip this tick
        // entirely while text is selected; refresh resumes once it's
        // cleared (e.g. right after the copy).
        if (Selection.getSelectionStart(logText.text) != Selection.getSelectionEnd(logText.text)) return

        val log = runCatching { TunnelNative.nativeRecentLog() }.getOrDefault("")
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
}
