package com.xdisplay

import android.content.Context
import android.content.Intent
import android.provider.Settings
import java.net.Inet4Address
import java.net.NetworkInterface

/**
 * Inspects Android's live IPv4 network interfaces for USB Tethering (`rndis*`, `ncm*`, `usb*`)
 * and Wi-Fi/LAN discovery, and provides a one-tap shortcut to Android's USB Tethering settings.
 */
object NetworkUtils {
    data class InterfaceInfo(
        val name: String,
        val localIp: String,
        val broadcastIp: String,
        val prefixLen: Int,
        val isUsbTether: Boolean
    )

    /** Returns active non-loopback, non-cellular IPv4 interfaces on this device. */
    fun activeInterfaces(): List<InterfaceInfo> {
        val result = ArrayList<InterfaceInfo>()
        val interfaces = try {
            NetworkInterface.getNetworkInterfaces()
        } catch (_: Exception) {
            null
        } ?: return result

        while (interfaces.hasMoreElements()) {
            val iface = interfaces.nextElement()
            val up = try {
                iface.isUp && !iface.isLoopback
            } catch (_: Exception) {
                false
            }
            if (!up) continue

            val name = iface.name.lowercase()
            if (isCellularOrTunnel(name)) continue
            val isUsbTether = isUsbTetherName(name)

            for (addr in iface.interfaceAddresses) {
                val inet = addr.address as? Inet4Address ?: continue
                if (inet.isLoopbackAddress || inet.isLinkLocalAddress || inet.isAnyLocalAddress) continue
                val prefixLen = addr.networkPrefixLength.toInt()
                if (prefixLen !in 1..32) continue
                val localIp = inet.hostAddress ?: continue
                val broadcastIp = addr.broadcast?.hostAddress ?: computeBroadcast(inet.address, prefixLen)
                result.add(InterfaceInfo(name, localIp, broadcastIp, prefixLen, isUsbTether))
            }
        }
        return result
    }

    fun isUsbTetheringActive(ifaces: List<InterfaceInfo> = activeInterfaces()): Boolean =
        ifaces.any { it.isUsbTether }

    /** Serializes active interfaces for `NativeEngine.setDiscoveryInterfaces`. */
    fun toNativeSpec(ifaces: List<InterfaceInfo>): String =
        ifaces.joinToString("\n") { iface ->
            val kind = if (iface.isUsbTether) "usb" else "net"
            "${iface.localIp}|${iface.broadcastIp}|${iface.prefixLen}|$kind"
        }

    /** Opens Android's USB Tethering screen directly when available, falling back to Network settings. */
    fun openUsbTetheringSettings(context: Context) {
        val candidates = listOf(
            Intent().setClassName("com.android.settings", "com.android.settings.TetherSettings"),
            Intent(Settings.ACTION_WIRELESS_SETTINGS),
            Intent(Settings.ACTION_SETTINGS)
        )
        for (intent in candidates) {
            try {
                intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                context.startActivity(intent)
                return
            } catch (_: Exception) {
                // OEM moved or restricted this activity; try next fallback.
            }
        }
    }

    private fun isUsbTetherName(name: String): Boolean =
        name.startsWith("rndis") || name.startsWith("ncm") || name.startsWith("usb")

    private fun isCellularOrTunnel(name: String): Boolean =
        name.startsWith("rmnet") ||
            name.startsWith("ccmni") ||
            name.startsWith("pdp") ||
            name.startsWith("wwan") ||
            name.startsWith("clat") ||
            name.startsWith("v4-") ||
            name.startsWith("tun") ||
            name.startsWith("dummy")

    private fun computeBroadcast(bytes: ByteArray, prefixLen: Int): String {
        val ip = ((bytes[0].toInt() and 0xFF) shl 24) or
            ((bytes[1].toInt() and 0xFF) shl 16) or
            ((bytes[2].toInt() and 0xFF) shl 8) or
            (bytes[3].toInt() and 0xFF)
        val mask = if (prefixLen >= 32) -1 else (-1 shl (32 - prefixLen))
        val bcast = ip or mask.inv()
        return "${(bcast ushr 24) and 0xFF}.${(bcast ushr 16) and 0xFF}.${(bcast ushr 8) and 0xFF}.${bcast and 0xFF}"
    }
}
