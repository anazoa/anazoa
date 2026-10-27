package org.anazoa.vpn

import android.annotation.SuppressLint
import android.content.Context
import android.content.res.Resources
import android.os.Build
import android.provider.Settings
import android.util.DisplayMetrics
import java.util.Locale
import java.util.TimeZone

/**
 * This phone's values for anazoa.toml's `[fingerprint]` section, built the
 * way Max 26.30.1 builds its session hello `userAgent` (`pyi.a()` in the
 * decompile), so a config copied from here matches what Max itself would
 * report on this phone.
 */
object DeviceFingerprint {

    /** Languages Max's UI supports (`kg9.a`). */
    private val SUPPORTED_LANGUAGES = listOf("ru", "en", "es", "pt-BR", "uz", "fr")

    /** Unsupported languages Max falls back to Russian for (`og9.a`). */
    private val RUSSIAN_FALLBACK = setOf("ru", "be", "bg", "az", "hy", "kk", "ky", "tg", "uz", "tk", "ro")

    fun toml(context: Context): String {
        val systemLocale = Resources.getSystem().configuration.locales[0] ?: Locale.forLanguageTag("ru")
        return buildString {
            appendLine("[fingerprint]")
            appendLine("os-version = ${quote("Android ${Build.VERSION.RELEASE}")}")
            appendLine("os-api-level = ${Build.VERSION.SDK_INT}")
            appendLine("screen = ${quote(screen(context.applicationContext.resources.displayMetrics))}")
            appendLine("device-name = ${quote("${Build.MANUFACTURER} ${Build.MODEL}")}")
            appendLine("device-locale = ${quote(systemLocale.language)}")
            // Max uses its in-app language when one is set; the system
            // locale is its default.
            appendLine("locale = ${quote(normalizeLanguage(systemLocale.toLanguageTag()))}")
            appendLine("timezone = ${quote(TimeZone.getDefault().id)}")
            // Max sends its own ANDROID_ID, which Android scopes per app
            // signing key: this one has the same shape but isn't Max's.
            append("device-id = ${quote(androidId(context))}")
        }
    }

    /** `"<bucket> <dpi>dpi <width>x<height>"`, the bucket being `<dpi>dpi` off the standard densities. */
    private fun screen(metrics: DisplayMetrics): String {
        val dpi = metrics.densityDpi
        val bucket = when (dpi) {
            120 -> "ldpi"
            160 -> "mdpi"
            240 -> "hdpi"
            320 -> "xhdpi"
            480 -> "xxhdpi"
            640 -> "xxxhdpi"
            else -> "${dpi}dpi"
        }
        return "$bucket ${dpi}dpi ${metrics.widthPixels}x${metrics.heightPixels}"
    }

    /** Max's `og9.f`: a supported tag as is, else one with the same language, else a fallback. */
    private fun normalizeLanguage(tag: String): String {
        if (tag in SUPPORTED_LANGUAGES) return tag
        val language = Locale.forLanguageTag(tag).language
        if (language.isEmpty()) return "ru"
        return SUPPORTED_LANGUAGES.firstOrNull { Locale.forLanguageTag(it).language == language }
            ?: if (language in RUSSIAN_FALLBACK) "ru" else "en"
    }

    @SuppressLint("HardwareIds")
    private fun androidId(context: Context): String =
        Settings.Secure.getString(context.contentResolver, Settings.Secure.ANDROID_ID).orEmpty()

    private fun quote(value: String): String =
        "\"" + value.replace("\\", "\\\\").replace("\"", "\\\"") + "\""
}
