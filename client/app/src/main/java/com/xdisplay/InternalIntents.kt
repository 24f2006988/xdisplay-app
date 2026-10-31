package com.xdisplay

import android.content.Intent
import java.security.MessageDigest
import java.security.SecureRandom

/**
 * Marks intents this app sends to its own activities.
 *
 * MainActivity and ConnectActivity must stay exported (the launcher, the USB accessory prompt
 * and `adb shell am start` start them), so any installed app can start them too. Without a
 * check, another app could launch MainActivity with `host`/`pin` extras and make XDisplay pair
 * with, and send pen, touch and keyboard input to, a PC of the attacker's choosing, or open the
 * PIN dialog for a fake host. Extras are honoured only when they carry this per-process random
 * token, which no other app can know.
 */
internal object InternalIntents {
    private const val EXTRA_TOKEN = "com.xdisplay.internal_token"

    private val token: String = ByteArray(16).also { SecureRandom().nextBytes(it) }
        .joinToString("") { "%02x".format(it) }

    fun mark(intent: Intent): Intent = intent.putExtra(EXTRA_TOKEN, token)

    /** True only for intents built by [mark] in this process. */
    fun isInternal(intent: Intent?): Boolean {
        val presented = intent?.getStringExtra(EXTRA_TOKEN) ?: return false
        return MessageDigest.isEqual(presented.toByteArray(), token.toByteArray())
    }
}
