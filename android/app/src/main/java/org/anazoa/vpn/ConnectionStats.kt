package org.anazoa.vpn

import android.content.Context
import org.json.JSONObject
import java.util.Locale

/**
 * Turns an `in_call` [TunnelNative.nativeStatus] blob (engine.rs's
 * `status_json`) into the Main tab's stats rows. The engine only reports
 * byte totals, so up/down rates come from the change between two polls,
 * and the previous poll is kept here.
 */
class ConnectionStats(private val context: Context) {

    class Rows(
        val duration: String,
        val up: String,
        val down: String,
        val capacity: String,
        val latency: String,
        val video: String,
    )

    private var lastAtMs = 0L
    private var lastUp = -1L
    private var lastDown = -1L

    /** Rows for an `in_call` status, or null for any other state. */
    fun format(status: JSONObject, nowMs: Long): Rows? {
        if (status.optString("state") != "in_call") {
            lastUp = -1
            lastDown = -1
            return null
        }
        val none = context.getString(R.string.stats_none)
        val tunnel = status.optJSONObject("tunnel")
        // The engine counts from the TUN device's side: what it reads from
        // the device (rx) is what this phone sends into the tunnel.
        val up = tunnel?.optLong("packet_bytes_rx") ?: 0
        val down = tunnel?.optLong("packet_bytes_tx") ?: 0
        val secs = (nowMs - lastAtMs) / 1000.0
        // A counter going backwards is a new call: no rate until next poll.
        val rated = lastUp in 0..up && lastDown in 0..down && secs > 0
        val upRate = if (rated) (up - lastUp) * 8 / secs else null
        val downRate = if (rated) (down - lastDown) * 8 / secs else null
        lastAtMs = nowMs
        lastUp = up
        lastDown = down

        val video = status.optJSONObject("video_send")
        val connection = status.optJSONObject("connection")

        val capacity = video?.optNumber("target_bitrate")?.let { bitrate ->
            val busy = tunnel?.optJSONObject("utilization")?.optJSONObject("5s")?.optNumber("ratio")
            if (busy == null) {
                formatRate(bitrate)
            } else {
                context.getString(R.string.stats_capacity_value, formatRate(bitrate), Math.round(busy * 100))
            }
        } ?: none

        val latency = connection?.let {
            val rtt = it.optNumber("rtt_ms")?.let { ms -> context.getString(R.string.stats_rtt, ms.toLong()) } ?: none
            val path = when (it.optString("path")) {
                "relay" -> context.getString(R.string.stats_path_relay)
                "direct" -> context.getString(R.string.stats_path_direct)
                else -> null
            }
            val protocol = it.optString("protocol").takeIf { p -> p.isNotEmpty() && p != "null" }
                ?.uppercase(Locale.ROOT)
            listOfNotNull(rtt, path, protocol).joinToString(SEPARATOR)
        } ?: none

        val videoText = video?.let {
            val parts = listOfNotNull(
                tunnel?.optString("local_resolution")?.takeIf { r -> r.isNotEmpty() },
                it.optNumber("fps")?.let { fps -> context.getString(R.string.stats_fps, fps.toLong()) },
                it.optString("encoder").takeIf { e -> e.isNotEmpty() && e != "null" },
            )
            val limit = it.optString("quality_limitation")
            val line = parts.joinToString(SEPARATOR)
            if (limit.isEmpty() || limit == "none" || limit == "null") {
                line
            } else {
                "$line\n${context.getString(R.string.stats_limited, limit)}"
            }
        } ?: none

        return Rows(
            duration = formatDuration(status.optLong("uptime_secs")),
            up = formatTraffic(upRate, up),
            down = formatTraffic(downRate, down),
            capacity = capacity,
            latency = latency,
            video = videoText,
        )
    }

    private fun formatTraffic(bitsPerSec: Double?, total: Long): String =
        if (bitsPerSec == null) {
            formatBytes(total)
        } else {
            formatRate(bitsPerSec) + SEPARATOR + formatBytes(total)
        }

    private companion object {
        const val SEPARATOR = " · "

        /** A JSON number, or null when absent or JSON null. */
        fun JSONObject.optNumber(name: String): Double? =
            if (isNull(name)) null else optDouble(name).takeUnless { it.isNaN() }

        fun formatRate(bitsPerSec: Double): String = when {
            bitsPerSec >= 1_000_000 -> String.format(Locale.ROOT, "%.1f Mbit/s", bitsPerSec / 1_000_000)
            bitsPerSec >= 1_000 -> String.format(Locale.ROOT, "%.0f kbit/s", bitsPerSec / 1_000)
            else -> String.format(Locale.ROOT, "%.0f bit/s", bitsPerSec)
        }

        fun formatBytes(bytes: Long): String = when {
            bytes >= 1_000_000_000 -> String.format(Locale.ROOT, "%.2f GB", bytes / 1e9)
            bytes >= 1_000_000 -> String.format(Locale.ROOT, "%.1f MB", bytes / 1e6)
            bytes >= 1_000 -> String.format(Locale.ROOT, "%.0f kB", bytes / 1e3)
            else -> "$bytes B"
        }

        fun formatDuration(secs: Long): String {
            val h = secs / 3600
            val m = secs % 3600 / 60
            val s = secs % 60
            return if (h > 0) {
                String.format(Locale.ROOT, "%d:%02d:%02d", h, m, s)
            } else {
                String.format(Locale.ROOT, "%d:%02d", m, s)
            }
        }
    }
}
