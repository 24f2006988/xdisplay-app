package com.xdisplay

import android.content.Context
import android.content.SharedPreferences
import java.security.SecureRandom

/** Where the tablet connects: a PC over USB (adb reverse) or over Wi-Fi. */
data class Target(
    val host: String,
    val port: Int,
    val name: String
) {
    /** USB goes through `adb reverse`, which arrives on this device's own loopback. */
    val isUsb: Boolean get() = host == USB_HOST

    val label: String get() = if (isUsb) "USB" else "$name ($host)"

    companion object {
        const val USB_HOST = "127.0.0.1"
        const val DEFAULT_PORT = 59100

        val USB = Target(USB_HOST, DEFAULT_PORT, "USB cable")
    }
}

/**
 * Persistent connection state: this tablet's identity, the secrets issued by PCs it has paired
 * with, and the last target. Secrets are what authenticate a reconnect, so the store lives in
 * app-private storage and the manifest disables backups.
 */
class ConnectionStore(context: Context) {
    private val prefs: SharedPreferences = context.applicationContext.getSharedPreferences("connection", Context.MODE_PRIVATE)

    /** Random per-install identity the PC pairs with (32 hex chars). */
    val clientId: String
        get() = prefs.getString(KEY_CLIENT_ID, null) ?: newClientId().also { prefs.edit().putString(KEY_CLIENT_ID, it).apply() }

    /** All stored `hostIdHex:secretHex` entries, newline separated, ready to hand to the native layer. */
    val credentials: String
        get() = prefs.getString(KEY_CREDENTIALS, "") ?: ""

    fun hasCredentialFor(hostId: String): Boolean =
        credentials.lineSequence().any { it.startsWith("$hostId:") }

    /** Stores a pairing result (`hostIdHex:secretHex`), replacing any older secret for that PC. */
    fun saveCredential(entry: String) {
        val hostId = entry.substringBefore(':', "")
        if (hostId.isEmpty() || !entry.contains(':')) return
        val kept = credentials.lineSequence().filter { it.isNotBlank() && !it.startsWith("$hostId:") }
        prefs.edit().putString(KEY_CREDENTIALS, (kept + entry).joinToString("\n")).apply()
    }

    fun forgetCredential(hostId: String) {
        val kept = credentials.lineSequence().filter { it.isNotBlank() && !it.startsWith("$hostId:") }
        prefs.edit().putString(KEY_CREDENTIALS, kept.joinToString("\n")).apply()
    }

    var lastTarget: Target?
        get() {
            val host = prefs.getString(KEY_LAST_HOST, null) ?: return null
            return Target(host, prefs.getInt(KEY_LAST_PORT, Target.DEFAULT_PORT), prefs.getString(KEY_LAST_NAME, host) ?: host)
        }
        set(value) {
            val e = prefs.edit()
            if (value == null) {
                e.remove(KEY_LAST_HOST)
            } else {
                e.putString(KEY_LAST_HOST, value.host).putInt(KEY_LAST_PORT, value.port).putString(KEY_LAST_NAME, value.name)
            }
            e.apply()
        }

    /** Host-id of the PC behind [lastTarget], when it was reached through discovery. */
    var lastHostId: String?
        get() = prefs.getString(KEY_LAST_HOST_ID, null)
        set(value) = prefs.edit().putString(KEY_LAST_HOST_ID, value).apply()

    private fun newClientId(): String {
        val bytes = ByteArray(16).also { SecureRandom().nextBytes(it) }
        return bytes.joinToString("") { "%02x".format(it) }
    }

    private companion object {
        const val KEY_CLIENT_ID = "client_id"
        const val KEY_CREDENTIALS = "credentials"
        const val KEY_LAST_HOST = "last_host"
        const val KEY_LAST_PORT = "last_port"
        const val KEY_LAST_NAME = "last_name"
        const val KEY_LAST_HOST_ID = "last_host_id"
    }
}
