package com.xdisplay

/**
 * JNI bindings to the high-performance Rust streaming engine.
 */
object NativeEngine {
    init {
        System.loadLibrary("xdisplay_client_native")
    }

    const val STATUS_DISCONNECTED = 0
    const val STATUS_CONNECTING = 1
    const val STATUS_CONNECTED = 2

    /** The PC wants a pairing PIN; the link is idle until startClient is called again with one. */
    const val STATUS_NEEDS_PIN = 3

    /** Authentication was refused; see [getLastError]. The link is idle until restarted. */
    const val STATUS_AUTH_FAILED = 4
    /** Another device took over the PC's screen; [getLastError] names it. Not retried. */
    const val STATUS_TAKEN_OVER = 5

    external fun initNativeLogging()
    external fun setSurface(surface: android.view.Surface?)

    external fun startClient(
        host: String,
        port: Int,
        deviceName: String,
        screenWidth: Int,
        screenHeight: Int,
        maxRefreshRate: Int,
        dpi: Int,
        supportsStylus: Boolean,
        clientId: String,
        pin: String,
        credentials: String
    )

    external fun stopClient()

    external fun getConnectionStatus(): Int

    /** Why the link is in [STATUS_NEEDS_PIN] / [STATUS_AUTH_FAILED]; empty otherwise. */
    external fun getLastError(): String

    /** Name of the PC the link is (or was last) connected to. */
    external fun getHostName(): String

    /**
     * Returns `hostIdHex:secretHex` exactly once after a pairing completes, otherwise "".
     * Persist it: it lets the tablet reconnect to that PC without a PIN.
     */
    external fun takeNewCredential(): String

    external fun startDiscovery()
    external fun stopDiscovery()

    /** Hosts currently announcing themselves: one `ip|port|name|hostIdHex` per line. */
    external fun getDiscoveredHosts(): String

    external fun getSentPacketsCount(): Long
    external fun getRoundTripLatencyUs(): Long

    external fun sendPointerEvent(
        pointerId: Int,
        action: Int,
        toolType: Int,
        normX: Float,
        normY: Float,
        pressure: Float,
        tiltX: Float,
        tiltY: Float,
        buttons: Int,
        timestampUs: Long
    )

    const val KEY_ACTION_DOWN = 0
    const val KEY_ACTION_UP = 1
    const val KEY_ACTION_TAP = 2

    const val MOD_NONE = 0
    const val MOD_CTRL = 1
    const val MOD_SHIFT = 2
    const val MOD_ALT = 4

    const val VK_SHIFT = 0x10
    const val VK_CONTROL = 0x11
    const val VK_MENU = 0x12 // Alt
    const val VK_SPACE = 0x20
    const val VK_0 = 0x30
    const val VK_Y = 0x59
    const val VK_Z = 0x5A
    const val VK_OEM_4 = 0xDB // '[' decrease brush
    const val VK_OEM_6 = 0xDD // ']' increase brush
    const val VK_TAB = 0x09
    const val VK_ESCAPE = 0x1B

    external fun sendKeyEvent(
        vkCode: Int,
        action: Int,
        modifiers: Int
    )

    /** `bitrateKbps` for [updateStreamSettings]: let the host pick and adapt the bitrate. */
    const val BITRATE_AUTO = -1

    /** `targetFps`/`bitrateKbps` of 0 leave that setting unchanged. */
    external fun updateStreamSettings(
        targetFps: Int,
        bitrateKbps: Int
    )

    external fun sendScrollEvent(
        deltaY: Float
    )

    const val CURSOR_SHAPE_ARROW = 0
    const val CURSOR_SHAPE_IBEAM = 1
    const val CURSOR_SHAPE_HAND = 2
    const val CURSOR_SHAPE_CROSS = 3
    const val CURSOR_SHAPE_SIZE_ALL = 4
    const val CURSOR_SHAPE_HIDDEN = 5

    external fun getCursorState(outState: FloatArray): Boolean
}
