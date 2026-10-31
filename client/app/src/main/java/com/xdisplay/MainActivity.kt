package com.xdisplay

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.IntentFilter
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.util.Log
import android.hardware.display.DisplayManager
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.Toast
import androidx.core.content.ContextCompat
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.pow

enum class PressureCurve(val label: String, val gamma: Float) {
    LINEAR("LINEAR", 1.0f),
    SOFT("SOFT", 0.65f),
    FIRM("FIRM", 1.35f),
    HARD("HARD", 1.8f)
}

enum class BarrelButtonMapping(val label: String) {
    RIGHT_CLICK("R-CLICK"),
    ERASER("ERASER")
}

enum class TouchMode(val label: String) {
    ALL_TOUCH("ALL TOUCH"),
    PEN_ONLY("PEN ONLY"),
    TWO_FINGER_SCROLL("2F SCROLL")
}

class MainActivity : Activity(), SurfaceHolder.Callback, View.OnTouchListener, View.OnGenericMotionListener {
    private lateinit var surfaceView: SurfaceView
    private lateinit var inkOverlayView: InkOverlayView
    private val handler = Handler(Looper.getMainLooper())

    private lateinit var store: ConnectionStore
    private var target: Target = Target.USB
    private var pinUsed = ""
    private var wifiLock: WifiManager.WifiLock? = null
    private var lastBackPress = 0L
    private var leaving = false
    private var aoaBridge: AoaBridge? = null
    private var usbReceiver: BroadcastReceiver? = null

    /**
     * Frame rate the host streams at and the panel is asked to run at: the panel's highest
     * refresh rate at its current size (144 Hz on the Pad 7). A stream below the panel's rate
     * makes it repeat frames unevenly, which reads as jitter.
     */
    private var reportedFps = 0

    /** Tells the host when the panel starts running faster than it was told (the PC follows). */
    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
        override fun onDisplayChanged(displayId: Int) {
            if (displayId != windowManager.defaultDisplay.displayId) return
            val now = windowManager.defaultDisplay.refreshRate.toInt()
            // Only upward: the panel idles at lower rates when the picture is still, and the
            // PC's monitor must not be switched back and forth for that.
            if (reportedFps in 1 until now) {
                reportedFps = now
                NativeEngine.updateStreamSettings(now, 0)
            }
        }
    }

    private val panelFps: Float by lazy {
        val display = windowManager.defaultDisplay
        val size = display.mode
        val best = display.supportedModes
            .filter { it.physicalWidth == size.physicalWidth && it.physicalHeight == size.physicalHeight }
            .maxOfOrNull { it.refreshRate } ?: display.refreshRate
        minOf(best, MAX_STREAM_FPS)
    }

    // Client-side viewport: the decoded video is scaled/translated locally, so zoom costs no
    // bandwidth or latency. Input is mapped back through the inverse transform before it is
    // sent to Windows, so taps and pen strokes land where they appear on the zoomed picture.
    private var zoomScale = 1f
    private var panX = 0f
    private var panY = 0f
    private var pinchZoomEnabled = true

    // Pen & Touch settings (Feature #3, #2, #4, #10)
    private var touchMode = TouchMode.ALL_TOUCH
    private var penOnlyMode = false
    private var currentPressureCurve = PressureCurve.LINEAR
    private var barrelMapping = BarrelButtonMapping.RIGHT_CLICK
    private var inkMode = InkOverlayView.InkMode.FADE
    private var scrollPrevY = 0f
    private var currentFps = 120
    private var currentBitrateKbps = NativeEngine.BITRATE_AUTO

    private fun setPressureCurve(curve: PressureCurve) {
        currentPressureCurve = curve
        inkOverlayView.pressureCurveName = curve.label
    }

    private fun cyclePressureCurve(): String {
        val values = PressureCurve.values()
        currentPressureCurve = values[(currentPressureCurve.ordinal + 1) % values.size]
        return currentPressureCurve.label
    }

    private fun cycleBarrelMapping(): String {
        val values = BarrelButtonMapping.values()
        barrelMapping = values[(barrelMapping.ordinal + 1) % values.size]
        return barrelMapping.label
    }

    private fun cycleInkMode(): InkOverlayView.InkMode {
        val values = InkOverlayView.InkMode.values()
        inkMode = values[(inkMode.ordinal + 1) % values.size]
        inkOverlayView.inkMode = inkMode
        return inkMode
    }

    private fun applyPressureCurve(rawPressure: Float): Float {
        if (rawPressure <= 0f) return 0f
        val clamped = rawPressure.coerceIn(0f, 1f)
        return clamped.toDouble().pow(currentPressureCurve.gamma.toDouble()).toFloat()
    }

    // Two-finger gesture state
    private var gestureActive = false
    private var gestureNeedsAnchor = false
    private var anchorSpan = 1f
    private var anchorFocusX = 0f
    private var anchorFocusY = 0f
    private var anchorScale = 1f
    private var anchorPanX = 0f
    private var anchorPanY = 0f
    private var gestureStartTime = 0L
    private var gestureTravel = 0f
    private var lastTwoFingerTapTime = 0L

    private val statusUpdateRunnable = object : Runnable {
        override fun run() {
            if (::inkOverlayView.isInitialized) {
                val status = NativeEngine.getConnectionStatus()
                inkOverlayView.linkStatus = status
                inkOverlayView.packetsSent = NativeEngine.getSentPacketsCount()
                inkOverlayView.rttUs = NativeEngine.getRoundTripLatencyUs()
                inkOverlayView.invalidate()
                persistNewCredential()
                if (status == NativeEngine.STATUS_NEEDS_PIN || status == NativeEngine.STATUS_AUTH_FAILED ||
                    status == NativeEngine.STATUS_TAKEN_OVER) {
                    returnToConnect(status)
                    return
                }
            }
            handler.postDelayed(this, 250)
        }
    }

    /** A completed pairing hands over the secret once; losing it would force re-pairing. */
    private fun persistNewCredential() {
        val credential = NativeEngine.takeNewCredential()
        if (credential.isNotEmpty()) store.saveCredential(credential)
    }

    /** Goes back to the connection picker, explaining why when the PC refused us. */
    private fun returnToConnect(status: Int?) {
        if (leaving) return
        leaving = true
        aoaBridge?.close()
        aoaBridge = null
        val message = if (status != null) NativeEngine.getLastError() else null
        val needPin = status == NativeEngine.STATUS_NEEDS_PIN ||
            (status == NativeEngine.STATUS_AUTH_FAILED && pinUsed.isNotEmpty())
        NativeEngine.stopClient()
        startActivity(
            InternalIntents.mark(Intent(this, ConnectActivity::class.java))
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
                .putExtra(ConnectActivity.EXTRA_MESSAGE, message)
                .putExtra(ConnectActivity.EXTRA_ERROR, status == NativeEngine.STATUS_AUTH_FAILED)
                .putExtra(ConnectActivity.EXTRA_NEED_PIN, needPin)
                .putExtra(ConnectActivity.EXTRA_HOST, target.host)
                .putExtra(ConnectActivity.EXTRA_PORT, target.port)
                .putExtra(ConnectActivity.EXTRA_NAME, NativeEngine.getHostName().ifEmpty { target.name })
        )
        finish()
    }

    /** Wi-Fi power saving adds tens of milliseconds of jitter; hold the radio awake while streaming. */
    @Suppress("DEPRECATION")
    private fun updateWifiLock() {
        releaseWifiLock()
        if (target.isUsb) return
        val wifi = applicationContext.getSystemService(WIFI_SERVICE) as? WifiManager ?: return
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        wifiLock = wifi.createWifiLock(mode, "xdisplay-stream").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseWifiLock() {
        wifiLock?.takeIf { it.isHeld }?.release()
        wifiLock = null
    }

    /**
     * Starts (or restarts) the link for the target in [intent]. Launching with no target, for
     * example from `adb shell am start`, means USB, so the cable workflow is unchanged.
     */
    private fun connectFrom(intent: Intent) {
        val usbManager = getSystemService(Context.USB_SERVICE) as? UsbManager
        val accessory: UsbAccessory? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
        } ?: usbManager?.accessoryList?.firstOrNull()

        aoaBridge?.close()
        aoaBridge = null

        // A network target and PIN are accepted only from our own ConnectActivity: this activity
        // is exported, and another app must not be able to point it at a PC it controls.
        val internal = InternalIntents.isInternal(intent)
        if (!internal && (intent.hasExtra(EXTRA_HOST) || intent.hasExtra(EXTRA_PIN))) {
            Log.w("MainActivity", "Ignoring connection extras from an external caller")
        }
        val host = if (internal) intent.getStringExtra(EXTRA_HOST) else null
        target = if (accessory != null && usbManager != null) {
            try {
                val bridge = AoaBridge(usbManager, accessory)
                aoaBridge = bridge
                Log.i("MainActivity", "AOA Direct USB mode initialized on port ${bridge.port}")
                Target("127.0.0.1", bridge.port, "Direct USB (Plug & Play)")
            } catch (e: Exception) {
                Log.e("MainActivity", "Failed to start AOA bridge: ${e.message}; falling back to default USB")
                Target.USB
            }
        } else if (host != null) {
            Target(host, intent.getIntExtra(EXTRA_PORT, Target.DEFAULT_PORT), intent.getStringExtra(EXTRA_NAME) ?: host)
        } else {
            Target.USB
        }
        pinUsed = if (internal) intent.getStringExtra(EXTRA_PIN) ?: "" else ""
        leaving = false

        inkOverlayView.targetLabel = target.label
        inkOverlayView.isUsbLink = target.isUsb
        updateWifiLock()

        // The whole panel, not `displayMetrics`: that excludes the cutout/status area (3200x2006 on a
        // 3200x2136 panel), so the host would pick a virtual monitor with the wrong aspect ratio.
        val metrics = resources.displayMetrics
        val display = (getSystemService(WINDOW_SERVICE) as WindowManager).defaultDisplay
        val panel = android.util.DisplayMetrics().also {
            @Suppress("DEPRECATION")
            display.getRealMetrics(it)
        }
        // What the panel actually runs at, not what it could: the system may cap apps below the
        // panel's top mode (120 of 144 Hz here), and a faster stream would then be dropped unevenly.
        val refreshRate = display.refreshRate.toInt().coerceAtLeast(60)
        reportedFps = refreshRate
        NativeEngine.startClient(
            host = target.host,
            port = target.port,
            deviceName = "${Build.MANUFACTURER} ${Build.MODEL}",
            screenWidth = panel.widthPixels,
            screenHeight = panel.heightPixels,
            maxRefreshRate = refreshRate,
            dpi = metrics.densityDpi,
            supportsStylus = true,
            clientId = store.clientId,
            pin = pinUsed,
            credentials = store.credentials
        )
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        connectFrom(intent)
    }

    @Deprecated("Back is guarded: an edge swipe while drawing must not drop the connection")
    override fun onBackPressed() {
        val now = SystemClock.elapsedRealtime()
        if (now - lastBackPress < BACK_CONFIRM_MS) {
            returnToConnect(null)
        } else {
            lastBackPress = now
            Toast.makeText(this, "Swipe back again to change connection", Toast.LENGTH_SHORT).show()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = ConnectionStore(this)

        val usbFilter = IntentFilter(UsbManager.ACTION_USB_ACCESSORY_DETACHED)
        usbReceiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (intent.action == UsbManager.ACTION_USB_ACCESSORY_DETACHED) {
                    val detached: UsbAccessory? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                        intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
                    } else {
                        @Suppress("DEPRECATION")
                        intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
                    }
                    if (detached == null || detached == aoaBridge?.accessory) {
                        Log.i("MainActivity", "USB accessory detached; returning to connect screen")
                        Toast.makeText(this@MainActivity, "USB cable disconnected", Toast.LENGTH_SHORT).show()
                        returnToConnect(null)
                    }
                }
            }
        }
        // ACCESSORY_DETACHED is a protected system broadcast; it reaches non-exported receivers.
        ContextCompat.registerReceiver(this, usbReceiver, usbFilter, ContextCompat.RECEIVER_NOT_EXPORTED)

        // Keep screen awake & full immersive fullscreen mode
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O_MR1) {
            setShowWhenLocked(true)
            setTurnScreenOn(true)
        } else {
            @Suppress("DEPRECATION")
            window.addFlags(
                WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED
                or WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON
                or WindowManager.LayoutParams.FLAG_DISMISS_KEYGUARD
            )
        }
        // Draw under the camera cutout so the video covers the full panel and matches what the host streams.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            window.attributes = window.attributes.apply {
                layoutInDisplayCutoutMode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
                } else {
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
                }
                preferredRefreshRate = panelFps
            }
        }
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            or View.SYSTEM_UI_FLAG_FULLSCREEN
            or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
            or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
            or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
            or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
        )

        window.setBackgroundDrawable(android.graphics.drawable.ColorDrawable(android.graphics.Color.BLACK))
        NativeEngine.initNativeLogging()

        val rootLayout = FrameLayout(this)

        surfaceView = SurfaceView(this).apply {
            setZOrderMediaOverlay(true)
            holder.addCallback(this@MainActivity)
            pivotX = 0f
            pivotY = 0f
        }

        inkOverlayView = InkOverlayView(this).apply {
            setBackgroundColor(android.graphics.Color.TRANSPARENT)
            setOnTouchListener(this@MainActivity)
            setOnGenericMotionListener(this@MainActivity)
            pinchZoomEnabled = this@MainActivity.pinchZoomEnabled
            onTogglePinchMode = {
                this@MainActivity.pinchZoomEnabled = !this@MainActivity.pinchZoomEnabled
                if (!this@MainActivity.pinchZoomEnabled) resetZoom()
                this@MainActivity.pinchZoomEnabled
            }
            onResetZoom = { resetZoom() }

            penOnlyMode = this@MainActivity.penOnlyMode
            onTogglePenOnly = {
                this@MainActivity.penOnlyMode = !this@MainActivity.penOnlyMode
                this@MainActivity.penOnlyMode
            }

            pressureCurveName = this@MainActivity.currentPressureCurve.label
            onCyclePressureCurve = {
                this@MainActivity.cyclePressureCurve()
            }

            barrelMappingName = this@MainActivity.barrelMapping.label
            onCycleBarrelMapping = {
                this@MainActivity.cycleBarrelMapping()
            }

            inkMode = this@MainActivity.inkMode
            onCycleInkMode = {
                this@MainActivity.cycleInkMode()
            }

            onSendKeyEvent = { vkCode, action, modifiers ->
                NativeEngine.sendKeyEvent(vkCode, action, modifiers)
            }

            currentTouchMode = this@MainActivity.touchMode
            onSelectTouchMode = { mode ->
                this@MainActivity.touchMode = mode
                this@MainActivity.penOnlyMode = (mode == TouchMode.PEN_ONLY)
                this@MainActivity.inkOverlayView.penOnlyMode = this@MainActivity.penOnlyMode
                this@MainActivity.inkOverlayView.currentTouchMode = mode
            }

            onSetPressureCurve = { curve ->
                this@MainActivity.setPressureCurve(curve)
            }

            onApplyStreamSettings = { fps, bitrateKbps ->
                this@MainActivity.currentFps = fps
                this@MainActivity.currentBitrateKbps = bitrateKbps
                NativeEngine.updateStreamSettings(fps, bitrateKbps)
            }
        }

        rootLayout.addView(
            surfaceView,
            FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT)
        )
        rootLayout.addView(
            inkOverlayView,
            FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT)
        )
        inkOverlayView.bringToFront()

        setContentView(rootLayout)

        connectFrom(intent)
    }

    override fun onResume() {
        super.onResume()
        (getSystemService(DISPLAY_SERVICE) as DisplayManager).registerDisplayListener(displayListener, handler)
        handler.post(statusUpdateRunnable)
    }

    override fun onPause() {
        super.onPause()
        (getSystemService(DISPLAY_SERVICE) as DisplayManager).unregisterDisplayListener(displayListener)
        handler.removeCallbacks(statusUpdateRunnable)
        persistNewCredential()
    }

    override fun onDestroy() {
        usbReceiver?.let {
            try { unregisterReceiver(it) } catch (_: Exception) {}
            usbReceiver = null
        }
        aoaBridge?.close()
        aoaBridge = null
        super.onDestroy()
        persistNewCredential()
        releaseWifiLock()
        NativeEngine.stopClient()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        // Without this the panel idles at its adaptive minimum (50 Hz seen on the Pad 7) and the
        // 120 fps stream is presented unevenly, which reads as jitter.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            runCatching { holder.surface.setFrameRate(panelFps, android.view.Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE) }
        }
        NativeEngine.setSurface(holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        applyViewport()
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        NativeEngine.setSurface(null)
    }

    // ---------------------------------------------------------------------------------------
    // Viewport (pinch-zoom / pan)
    // ---------------------------------------------------------------------------------------

    /** Screen X on the tablet -> normalized X on the Windows display, through the zoom. */
    private fun toNormX(x: Float, width: Float): Float = (((x - panX) / zoomScale) / width).coerceIn(0f, 1f)

    private fun toNormY(y: Float, height: Float): Float = (((y - panY) / zoomScale) / height).coerceIn(0f, 1f)

    private fun applyViewport() {
        val w = surfaceView.width.toFloat()
        val h = surfaceView.height.toFloat()
        if (w <= 0f || h <= 0f) return

        zoomScale = zoomScale.coerceIn(1f, MAX_ZOOM)
        // Keep the picture covering the whole screen: no black gaps at the edges.
        panX = panX.coerceIn(w - w * zoomScale, 0f)
        panY = panY.coerceIn(h - h * zoomScale, 0f)

        surfaceView.scaleX = zoomScale
        surfaceView.scaleY = zoomScale
        surfaceView.translationX = panX
        surfaceView.translationY = panY

        inkOverlayView.setViewport(zoomScale, panX, panY)
    }

    private fun resetZoom() {
        zoomScale = 1f
        panX = 0f
        panY = 0f
        inkOverlayView.clearCanvas()
        applyViewport()
    }

    private fun anchorGesture(event: MotionEvent, skipIndex: Int = -1) {
        val idx = twoPointerIndices(event, skipIndex) ?: return
        val (a, b) = idx
        anchorSpan = hypot(event.getX(a) - event.getX(b), event.getY(a) - event.getY(b)).coerceAtLeast(1f)
        anchorFocusX = (event.getX(a) + event.getX(b)) / 2f
        anchorFocusY = (event.getY(a) + event.getY(b)) / 2f
        anchorScale = zoomScale
        anchorPanX = panX
        anchorPanY = panY
        gestureNeedsAnchor = false
    }

    private fun twoPointerIndices(event: MotionEvent, skipIndex: Int): Pair<Int, Int>? {
        var first = -1
        for (i in 0 until event.pointerCount) {
            if (i == skipIndex) continue
            if (first < 0) first = i else return Pair(first, i)
        }
        return null
    }

    private fun updateGesture(event: MotionEvent) {
        if (gestureNeedsAnchor) {
            anchorGesture(event)
            return
        }
        val (a, b) = twoPointerIndices(event, -1) ?: return
        val span = hypot(event.getX(a) - event.getX(b), event.getY(a) - event.getY(b))
        val focusX = (event.getX(a) + event.getX(b)) / 2f
        val focusY = (event.getY(a) + event.getY(b)) / 2f

        gestureTravel = maxOf(
            gestureTravel,
            hypot(focusX - anchorFocusX, focusY - anchorFocusY),
            abs(span - anchorSpan)
        )

        // Keep the content point that was under the fingers' midpoint under it while
        // scaling, and let the midpoint's movement pan the view.
        val newScale = (anchorScale * span / anchorSpan).coerceIn(1f, MAX_ZOOM)
        val contentX = (anchorFocusX - anchorPanX) / anchorScale
        val contentY = (anchorFocusY - anchorPanY) / anchorScale
        zoomScale = newScale
        panX = focusX - contentX * newScale
        panY = focusY - contentY * newScale
        applyViewport()
    }

    private fun endGesture(eventTime: Long) {
        gestureActive = false
        if (zoomScale < 1.05f) resetZoom()

        // Two-finger double-tap resets zoom.
        val density = resources.displayMetrics.density
        val wasTap = eventTime - gestureStartTime < TAP_MAX_MS && gestureTravel < TAP_SLOP_DP * density
        if (wasTap) {
            if (eventTime - lastTwoFingerTapTime < DOUBLE_TAP_MS) {
                resetZoom()
                lastTwoFingerTapTime = 0L
            } else {
                lastTwoFingerTapTime = eventTime
            }
        }
    }

    /**
     * Consumes two-finger gestures for local zoom. Returns true when the event must not be
     * forwarded to Windows.
     */
    private fun handleZoomGesture(event: MotionEvent, width: Float, height: Float): Boolean {
        val masked = event.actionMasked

        if (!gestureActive) {
            if (!pinchZoomEnabled || masked != MotionEvent.ACTION_POINTER_DOWN || event.pointerCount != 2) {
                return false
            }
            if (event.getToolType(0) != MotionEvent.TOOL_TYPE_FINGER ||
                event.getToolType(1) != MotionEvent.TOOL_TYPE_FINGER) {
                return false
            }

            // The first finger's DOWN already reached Windows: cancel it so the gesture
            // does not turn into a click or drag on the desktop.
            val first = if (event.actionIndex == 0) 1 else 0
            val firstId = event.getPointerId(first)
            val fx = event.getX(first)
            val fy = event.getY(first)
            inkOverlayView.processSample(firstId, MotionEvent.ACTION_CANCEL, MotionEvent.TOOL_TYPE_FINGER, fx, fy, 0f, 0f, 0f)
            NativeEngine.sendPointerEvent(
                pointerId = firstId,
                action = 6, // actions::CANCEL
                toolType = MotionEvent.TOOL_TYPE_FINGER,
                normX = toNormX(fx, width),
                normY = toNormY(fy, height),
                pressure = 0f,
                tiltX = 0f,
                tiltY = 0f,
                buttons = 0,
                timestampUs = event.eventTime * 1000L
            )

            // Local ink is drawn in screen space; once the picture moves it no longer lines up.
            inkOverlayView.clearCanvas()

            gestureActive = true
            gestureStartTime = event.eventTime
            gestureTravel = 0f
            anchorGesture(event)
            return true
        }

        when (masked) {
            MotionEvent.ACTION_POINTER_DOWN -> anchorGesture(event)
            MotionEvent.ACTION_MOVE -> if (event.pointerCount >= 2) updateGesture(event)
            MotionEvent.ACTION_POINTER_UP -> {
                // Re-anchor on the remaining pair so the view does not jump.
                if (event.pointerCount > 2) anchorGesture(event, event.actionIndex) else gestureNeedsAnchor = true
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> endGesture(event.eventTime)
        }
        return true
    }

    // ---------------------------------------------------------------------------------------
    // Input forwarding
    // ---------------------------------------------------------------------------------------

    override fun onTouch(v: View, event: MotionEvent): Boolean {
        v.requestUnbufferedDispatch(event)
        val width = v.width.toFloat()
        val height = v.height.toFloat()
        if (width <= 0f || height <= 0f) return false

        // Check button clicks (Clear / Toggle HUD / Pinch mode / Minimap)
        if (event.actionMasked == MotionEvent.ACTION_DOWN && !gestureActive) {
            if (inkOverlayView.handleButtonTouch(event.x, event.y)) {
                return true
            }
        }

        if (touchMode == TouchMode.TWO_FINGER_SCROLL && event.pointerCount >= 2) {
            val yMid = (event.getY(0) + event.getY(1)) / 2f
            when (event.actionMasked) {
                MotionEvent.ACTION_POINTER_DOWN -> {
                    scrollPrevY = yMid
                    inkOverlayView.clearCanvas()
                    return true
                }
                MotionEvent.ACTION_MOVE -> {
                    val deltaY = yMid - scrollPrevY
                    if (abs(deltaY) >= 2f) {
                        val scrollDelta = deltaY / 12f
                        NativeEngine.sendScrollEvent(scrollDelta)
                        scrollPrevY = yMid
                    }
                    return true
                }
                MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    return true
                }
            }
        }

        if (handleZoomGesture(event, width, height)) {
            return true
        }

        val historySize = event.historySize
        val pointerCount = event.pointerCount

        // 1. Process historical high-rate stylus / touch samples (120Hz - 240Hz digitizer pacing)
        for (h in 0 until historySize) {
            val histTimeUs = event.getHistoricalEventTime(h) * 1000L
            for (p in 0 until pointerCount) {
                val toolType = event.getToolType(p)
                if (penOnlyMode && toolType == MotionEvent.TOOL_TYPE_FINGER) {
                    continue
                }

                val pointerId = event.getPointerId(p)
                val hx = event.getHistoricalX(p, h)
                val hy = event.getHistoricalY(p, h)
                val rawPressure = event.getHistoricalPressure(p, h)
                val tiltRad = event.getHistoricalAxisValue(MotionEvent.AXIS_TILT, p, h)
                val orientRad = event.getHistoricalAxisValue(MotionEvent.AXIS_ORIENTATION, p, h)
                val tiltDeg = Math.toDegrees(tiltRad.toDouble()).toFloat()
                val tiltX = (-Math.sin(orientRad.toDouble()) * tiltDeg).toFloat()
                val tiltY = (Math.cos(orientRad.toDouble()) * tiltDeg).toFloat()

                var effectiveToolType = toolType
                var effectiveButtons = event.buttonState
                val hasBarrel = (event.buttonState and (MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_SECONDARY)) != 0
                if (hasBarrel) {
                    when (barrelMapping) {
                        BarrelButtonMapping.RIGHT_CLICK -> effectiveButtons = 0x02
                        BarrelButtonMapping.ERASER -> effectiveToolType = MotionEvent.TOOL_TYPE_ERASER
                    }
                }

                val pressure = if (effectiveToolType == MotionEvent.TOOL_TYPE_STYLUS) {
                    applyPressureCurve(rawPressure)
                } else {
                    rawPressure
                }

                // Instant local ink rendering
                inkOverlayView.processSample(
                    pointerId = pointerId,
                    action = MotionEvent.ACTION_MOVE,
                    toolType = effectiveToolType,
                    x = hx,
                    y = hy,
                    pressure = pressure,
                    tiltX = tiltX,
                    tiltY = tiltY
                )

                // High-speed USB packet stream to Windows
                NativeEngine.sendPointerEvent(
                    pointerId = pointerId,
                    action = 1, // actions::MOVE
                    toolType = effectiveToolType,
                    normX = toNormX(hx, width),
                    normY = toNormY(hy, height),
                    pressure = pressure,
                    tiltX = tiltX,
                    tiltY = tiltY,
                    buttons = effectiveButtons,
                    timestampUs = histTimeUs
                )
            }
        }

        // 2. Process current sample
        val actionMasked = event.actionMasked
        val actionIndex = event.actionIndex
        val action = when (actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> 0 // DOWN
            MotionEvent.ACTION_MOVE -> 1 // MOVE
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> 2 // UP
            MotionEvent.ACTION_HOVER_ENTER -> 3 // HOVER_ENTER
            MotionEvent.ACTION_HOVER_MOVE -> 4 // HOVER_MOVE
            MotionEvent.ACTION_HOVER_EXIT -> 5 // HOVER_EXIT
            MotionEvent.ACTION_CANCEL -> 6 // CANCEL (Palm rejection)
            else -> 1 // Default to MOVE
        }

        val eventTimeUs = event.eventTime * 1000L

        for (p in 0 until pointerCount) {
            if ((actionMasked == MotionEvent.ACTION_POINTER_DOWN || actionMasked == MotionEvent.ACTION_POINTER_UP)
                && p != actionIndex) {
                continue
            }

            val toolType = event.getToolType(p)
            if (penOnlyMode && toolType == MotionEvent.TOOL_TYPE_FINGER) {
                continue
            }

            val pointerId = event.getPointerId(p)
            val x = event.getX(p)
            val y = event.getY(p)
            val rawPressure = event.getPressure(p)
            val tiltRad = event.getAxisValue(MotionEvent.AXIS_TILT, p)
            val orientRad = event.getAxisValue(MotionEvent.AXIS_ORIENTATION, p)
            val tiltDeg = Math.toDegrees(tiltRad.toDouble()).toFloat()
            val tiltX = (-Math.sin(orientRad.toDouble()) * tiltDeg).toFloat()
            val tiltY = (Math.cos(orientRad.toDouble()) * tiltDeg).toFloat()

            var effectiveToolType = toolType
            var effectiveButtons = event.buttonState
            val hasBarrel = (event.buttonState and (MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_SECONDARY)) != 0
            if (hasBarrel) {
                when (barrelMapping) {
                    BarrelButtonMapping.RIGHT_CLICK -> effectiveButtons = 0x02
                    BarrelButtonMapping.ERASER -> effectiveToolType = MotionEvent.TOOL_TYPE_ERASER
                }
            }

            val pressure = if (effectiveToolType == MotionEvent.TOOL_TYPE_STYLUS) {
                applyPressureCurve(rawPressure)
            } else {
                rawPressure
            }

            inkOverlayView.processSample(
                pointerId = pointerId,
                action = actionMasked,
                toolType = effectiveToolType,
                x = x,
                y = y,
                pressure = pressure,
                tiltX = tiltX,
                tiltY = tiltY
            )

            NativeEngine.sendPointerEvent(
                pointerId = pointerId,
                action = action,
                toolType = effectiveToolType,
                normX = toNormX(x, width),
                normY = toNormY(y, height),
                pressure = pressure,
                tiltX = tiltX,
                tiltY = tiltY,
                buttons = effectiveButtons,
                timestampUs = eventTimeUs
            )
        }

        if (event.actionMasked == MotionEvent.ACTION_UP) v.performClick()
        return true
    }

    override fun onGenericMotion(v: View, event: MotionEvent): Boolean {
        v.requestUnbufferedDispatch(event)
        // Handles stylus hover movement (in-air stylus tracking)
        val actionMasked = event.actionMasked
        if (actionMasked == MotionEvent.ACTION_HOVER_MOVE
            || actionMasked == MotionEvent.ACTION_HOVER_ENTER
            || actionMasked == MotionEvent.ACTION_HOVER_EXIT) {
            val width = v.width.toFloat()
            val height = v.height.toFloat()
            if (width <= 0f || height <= 0f) return false

            val toolType = event.getToolType(0)
            if (penOnlyMode && toolType == MotionEvent.TOOL_TYPE_FINGER) {
                return false
            }

            val x = event.x
            val y = event.y
            val tiltRad = event.getAxisValue(MotionEvent.AXIS_TILT, 0)
            val orientRad = event.getAxisValue(MotionEvent.AXIS_ORIENTATION, 0)
            val tiltDeg = Math.toDegrees(tiltRad.toDouble()).toFloat()
            val tiltX = (-Math.sin(orientRad.toDouble()) * tiltDeg).toFloat()
            val tiltY = (Math.cos(orientRad.toDouble()) * tiltDeg).toFloat()

            var effectiveToolType = toolType
            var effectiveButtons = event.buttonState
            val hasBarrel = (event.buttonState and (MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_SECONDARY)) != 0
            if (hasBarrel) {
                when (barrelMapping) {
                    BarrelButtonMapping.RIGHT_CLICK -> effectiveButtons = 0x02
                    BarrelButtonMapping.ERASER -> effectiveToolType = MotionEvent.TOOL_TYPE_ERASER
                }
            }

            val action = when (actionMasked) {
                MotionEvent.ACTION_HOVER_ENTER -> 3
                MotionEvent.ACTION_HOVER_MOVE -> 4
                MotionEvent.ACTION_HOVER_EXIT -> 5
                else -> 4
            }

            inkOverlayView.processSample(
                pointerId = event.getPointerId(0),
                action = actionMasked,
                toolType = effectiveToolType,
                x = x,
                y = y,
                pressure = 0f,
                tiltX = tiltX,
                tiltY = tiltY
            )

            NativeEngine.sendPointerEvent(
                pointerId = event.getPointerId(0),
                action = action,
                toolType = effectiveToolType,
                normX = toNormX(x, width),
                normY = toNormY(y, height),
                pressure = 0f,
                tiltX = tiltX,
                tiltY = tiltY,
                buttons = effectiveButtons,
                timestampUs = event.eventTime * 1000L
            )
            return true
        }
        return false
    }

    companion object {
        /** The stream never runs faster than this, whatever the panel can do. */
        const val MAX_STREAM_FPS = 144f
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        const val EXTRA_NAME = "name"
        const val EXTRA_PIN = "pin"

        private const val BACK_CONFIRM_MS = 2000L
        private const val MAX_ZOOM = 5f
        private const val TAP_MAX_MS = 250L
        private const val DOUBLE_TAP_MS = 400L
        private const val TAP_SLOP_DP = 24f
    }
}
