package com.xdisplay

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.PorterDuff
import android.graphics.PorterDuffXfermode
import android.graphics.RectF
import android.graphics.Typeface
import android.view.MotionEvent
import android.view.View
import com.xdisplay.NativeEngine.BITRATE_AUTO
import kotlin.math.roundToInt

/**
 * High-performance interactive ink canvas and diagnostic HUD overlay.
 * Renders local predicted stylus strokes in < 3ms with zero-allocation ring buffer
 * and displays live hardware digitizer diagnostics (pressure, tilt, Hz, link status).
 */
class InkOverlayView(context: Context) : View(context) {

    // Canvas & Offscreen Bitmap Cache for persistent ink (only allocated when inkFadeEnabled == false)
    private var canvasBitmap: Bitmap? = null
    private var drawCanvas: Canvas? = null

    // Active pointer tracking (zero-allocation fixed array instead of HashMap)
    private class CachedPointer {
        var active: Boolean = false
        var x: Float = 0f
        var y: Float = 0f
        var pressure: Float = 0f
        var toolType: Int = 0
    }
    private val cachedPointers = Array(16) { CachedPointer() }

    // Colors
    private val colorStylus = Color.rgb(0, 240, 255)    // Cyber Cyan
    private val colorFinger = Color.rgb(0, 230, 118)    // Neon Mint Green
    private val colorEraser = Color.rgb(255, 82, 82)    // Coral Red
    private val colorHudBg = Color.argb(220, 14, 18, 27) // Dark obsidian glass
    private val colorHudBorder = Color.argb(90, 0, 240, 255)

    // Pre-allocated Paints (reused every frame to prevent GC pause / stutter at 120Hz)
    private val inkPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
        strokeJoin = Paint.Join.ROUND
    }

    private val eraserPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
        strokeJoin = Paint.Join.ROUND
        xfermode = PorterDuffXfermode(PorterDuff.Mode.CLEAR)
    }

    private val fadePaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
        strokeJoin = Paint.Join.ROUND
    }

    private val cursorPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 2f
    }

    // Decoupled 120Hz Local Hardware Cursor State & Paints
    private val cursorStateBuf = FloatArray(4)
    private var cursorVisible = false
    private var cursorScreenX = 0f
    private var cursorScreenY = 0f
    private var cursorShape = NativeEngine.CURSOR_SHAPE_ARROW
    private var vsyncRunning = false
    private val arrowPath = android.graphics.Path()

    private val cursorFillPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = Color.WHITE
    }

    private val cursorStrokePaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        color = Color.BLACK
        strokeWidth = 2f
        strokeJoin = Paint.Join.ROUND
        strokeCap = Paint.Cap.ROUND
    }

    private val cursorShadowPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = Color.argb(90, 0, 0, 0)
    }

    private val choreographerFrameCallback = object : android.view.Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (!vsyncRunning) return
            val changed = updateCursorFromNative()
            if (changed) {
                invalidate()
            }
            android.view.Choreographer.getInstance().postFrameCallback(this)
        }
    }

    private val hudBgPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = colorHudBg
    }

    private val hudBorderPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 2f
        color = colorHudBorder
    }

    private val textTitlePaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.WHITE
        textSize = 34f
        typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
    }

    private val textBodyPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.rgb(200, 215, 230)
        textSize = 26f
        typeface = Typeface.create(Typeface.MONOSPACE, Typeface.NORMAL)
    }

    private val textAccentPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = colorStylus
        textSize = 26f
        typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
    }

    private val barBgPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.argb(100, 40, 50, 70)
        style = Paint.Style.FILL
    }

    private val barFillPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = colorStylus
        style = Paint.Style.FILL
    }

    private val btnPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.argb(200, 25, 35, 50)
        style = Paint.Style.FILL
    }

    private val btnActivePaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.argb(220, 0, 180, 216) // Active Cyan glow
        style = Paint.Style.FILL
    }

    private val btnTextPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.WHITE
        textSize = 26f
        textAlign = Paint.Align.CENTER
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
    }

    // Zero-allocation Circular Ring Buffer for fading stroke segments (Feature #10)
    // 1024 slots is plenty for 160ms of 120-240Hz digitizer points without any GC pressure
    private val MAX_SEGMENTS = 1024
    private val segX1 = FloatArray(MAX_SEGMENTS)
    private val segY1 = FloatArray(MAX_SEGMENTS)
    private val segX2 = FloatArray(MAX_SEGMENTS)
    private val segY2 = FloatArray(MAX_SEGMENTS)
    private val segWidth = FloatArray(MAX_SEGMENTS)
    private val segColor = IntArray(MAX_SEGMENTS)
    private val segIsEraser = BooleanArray(MAX_SEGMENTS)
    private val segTimestamp = LongArray(MAX_SEGMENTS)
    private var segCount = 0
    private var segHead = 0
    private var segTail = 0

    private val FADE_DURATION_MS = 160L
    enum class InkMode(val label: String) {
        FADE("INK: FADE ⚡"),
        OFF("INK: OFF 🚫"),
        PERSIST("INK: PERSIST ✏️")
    }
    var inkMode: InkMode = InkMode.FADE
        set(value) {
            field = value
            if (value == InkMode.PERSIST) {
                ensurePersistentBitmap()
            } else {
                releasePersistentBitmap()
            }
            clearCanvas()
        }
    var inkFadeEnabled: Boolean
        get() = inkMode == InkMode.FADE
        set(value) {
            inkMode = if (value) InkMode.FADE else InkMode.PERSIST
        }
    var onToggleInkFade: (() -> Boolean)? = null
    var onCycleInkMode: (() -> InkMode)? = null

    // Pen & Touch Settings (Feature #3)
    var penOnlyMode: Boolean = false
    var onTogglePenOnly: (() -> Boolean)? = null

    var pressureCurveName: String = "LINEAR"
    var onCyclePressureCurve: (() -> String)? = null

    var barrelMappingName: String = "R-CLICK"
    var onCycleBarrelMapping: (() -> String)? = null

    // Live Diagnostics State
    var linkStatus: Int = NativeEngine.STATUS_DISCONNECTED

    /** Human-readable connection target shown in the HUD, e.g. "USB" or "Desk PC (192.168.1.20)". */
    var targetLabel: String = "USB"

    /** Selects the wording of the waiting screen: cable instructions vs. network ones. */
    var isUsbLink: Boolean = true
    var packetsSent: Long = 0L
    var rttUs: Long = 0L

    var lastToolType: Int = MotionEvent.TOOL_TYPE_UNKNOWN
    var lastPressure: Float = 0f
    var lastTiltX: Float = 0f
    var lastTiltY: Float = 0f
    var sampleRateHz: Int = 0

    var isHovering: Boolean = false
    var hoverX: Float = 0f
    var hoverY: Float = 0f

    // Sample rate calculation
    private var sampleCountWindow = 0
    private var windowStartMs = System.currentTimeMillis()

    // Menu & HUD Visibility State (All options stay hidden until menu button is clicked)
    var showMenu: Boolean = false
    var showHud: Boolean = false
    var showSidebar: Boolean = false
    private val menuToggleBtnRect = RectF()
    private val menuPanelRect = RectF()
    private val keysToggleBtnRect = RectF()
    private val clearBtnRect = RectF()
    private val hudToggleBtnRect = RectF()
    private val pinchBtnRect = RectF()
    private val inkFadeBtnRect = RectF()
    private val penModeBtnRect = RectF()
    private val pressureCurveBtnRect = RectF()
    private val barrelBtnRect = RectF()
    private val settingsBtnRect = RectF()

    // Pre-allocated HUD Rects and Paint
    private val hudCardRect = RectF()
    private val hudBarBgRect = RectF()
    private val hudBarFillRect = RectF()
    private val hudDotPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
    }

    // Landing Screen (Phase 1.3 - Cyberpunk Obsidian Landing Screen)
    private val landingRingPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingIconPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingTitlePaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingSubPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingBadgePaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingStatusTextPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingTipPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val landingIconRect = RectF()

    // Stream & Gesture Settings Modal (Feature #2 & #4)
    var showSettingsModal: Boolean = false
    var selectedFps: Int = 120
    var selectedBitrateKbps: Int = BITRATE_AUTO
    var currentTouchMode: TouchMode = TouchMode.ALL_TOUCH
    var onSelectTouchMode: ((TouchMode) -> Unit)? = null
    var onSetPressureCurve: ((PressureCurve) -> Unit)? = null
    var onApplyStreamSettings: ((fps: Int, bitrateKbps: Int) -> Unit)? = null

    private val modalCardRect = RectF()
    private val modalCloseBtnRect = RectF()
    private val modalApplyBtnRect = RectF()
    private val modalFps60Rect = RectF()
    private val modalFps90Rect = RectF()
    private val modalFps120Rect = RectF()
    private val modalBitrateAutoRect = RectF()
    private val modalBitrate15Rect = RectF()
    private val modalBitrate25Rect = RectF()
    private val modalBitrate35Rect = RectF()
    private val modalBitrate50Rect = RectF()
    private val modalTouchAllRect = RectF()
    private val modalTouchPenOnlyRect = RectF()
    private val modalTouchScrollRect = RectF()
    private val modalCurveLinearRect = RectF()
    private val modalCurveSoftRect = RectF()
    private val modalCurveFirmRect = RectF()
    private val modalCurveHardRect = RectF()

    private val modalBgPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.argb(245, 13, 18, 30)
        style = Paint.Style.FILL
    }
    private val modalBorderPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.argb(190, 0, 180, 216)
        style = Paint.Style.STROKE
    }
    private val modalHeaderPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.rgb(240, 248, 255)
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
    }
    private val modalLabelPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.rgb(140, 180, 215)
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
    }
    private val modalApplyPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.argb(230, 0, 180, 216)
        style = Paint.Style.FILL
    }
    private val modalApplyTextPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.WHITE
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        textAlign = Paint.Align.CENTER
    }

    // Express Keys Sidebar (Feature #5)
    var sidebarExpanded = true
    private val sidebarToggleBtnRect = RectF()
    private val sidebarRect = RectF()

    var isCtrlLatched = false
    var isShiftLatched = false
    var isAltLatched = false
    var isSpaceLatched = false

    private val undoBtnRect = RectF()
    private val redoBtnRect = RectF()
    private val ctrlBtnRect = RectF()
    private val shiftBtnRect = RectF()
    private val altBtnRect = RectF()
    private val spaceBtnRect = RectF()
    private val brushMinusBtnRect = RectF()
    private val brushPlusBtnRect = RectF()
    private val fitBtnRect = RectF()
    private val tabUiBtnRect = RectF()

    private class SidebarItem(
        val rect: RectF,
        val label: String,
        val isLatched: () -> Boolean
    )
    private val sidebarItems = arrayOf(
        SidebarItem(undoBtnRect, "↶ UNDO") { false },
        SidebarItem(redoBtnRect, "↷ REDO") { false },
        SidebarItem(ctrlBtnRect, "CTRL") { isCtrlLatched },
        SidebarItem(shiftBtnRect, "SHIFT") { isShiftLatched },
        SidebarItem(altBtnRect, "ALT") { isAltLatched },
        SidebarItem(spaceBtnRect, "SPACE") { isSpaceLatched },
        SidebarItem(brushMinusBtnRect, "BRUSH -") { false },
        SidebarItem(brushPlusBtnRect, "BRUSH +") { false },
        SidebarItem(fitBtnRect, "FIT [0]") { false },
        SidebarItem(tabUiBtnRect, "TAB UI") { false }
    )

    var onSendKeyEvent: ((vkCode: Int, action: Int, modifiers: Int) -> Unit)? = null

    private fun triggerKey(vkCode: Int, action: Int, modifiers: Int = 0) {
        onSendKeyEvent?.invoke(vkCode, action, modifiers)
    }

    // Zoom viewport (owned by MainActivity, mirrored here for the minimap)
    var pinchZoomEnabled: Boolean = true
    var onTogglePinchMode: (() -> Boolean)? = null
    var onResetZoom: (() -> Unit)? = null
    private var zoomScale = 1f
    private var zoomPanX = 0f
    private var zoomPanY = 0f
    private val minimapRect = RectF()
    private val minimapViewRect = RectF()

    private val minimapBgPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = Color.argb(170, 14, 18, 27)
    }

    private val minimapViewPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        color = colorStylus
    }

    fun setViewport(scale: Float, panX: Float, panY: Float) {
        zoomScale = scale
        zoomPanX = panX
        zoomPanY = panY
        invalidate()
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        vsyncRunning = true
        android.view.Choreographer.getInstance().postFrameCallback(choreographerFrameCallback)
    }

    override fun onDetachedFromWindow() {
        super.onDetachedFromWindow()
        vsyncRunning = false
    }

    private fun updateCursorFromNative(): Boolean {
        if (width <= 0 || height <= 0) return false
        val active = NativeEngine.getCursorState(cursorStateBuf)
        if (!active) {
            if (cursorVisible) {
                cursorVisible = false
                return true
            }
            return false
        }
        val normX = cursorStateBuf[0]
        val normY = cursorStateBuf[1]
        val shape = cursorStateBuf[2].toInt()
        val vis = cursorStateBuf[3] > 0.5f

        val sx = (normX * width) * zoomScale + zoomPanX
        val sy = (normY * height) * zoomScale + zoomPanY

        val changed = (vis != cursorVisible) || (shape != cursorShape) ||
                (kotlin.math.abs(sx - cursorScreenX) > 0.5f) || (kotlin.math.abs(sy - cursorScreenY) > 0.5f)

        cursorVisible = vis
        cursorShape = shape
        cursorScreenX = sx
        cursorScreenY = sy
        return changed
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        if (!inkFadeEnabled) {
            ensurePersistentBitmap(w, h)
        }
    }

    private fun ensurePersistentBitmap(w: Int = width, h: Int = height) {
        if (w > 0 && h > 0 && canvasBitmap == null) {
            val newBmp = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
            drawCanvas = Canvas(newBmp)
            canvasBitmap = newBmp
        }
    }

    private fun releasePersistentBitmap() {
        canvasBitmap?.recycle()
        canvasBitmap = null
        drawCanvas = null
    }

    fun setInkFade(enabled: Boolean) {
        inkMode = if (enabled) InkMode.FADE else InkMode.PERSIST
    }

    fun clearCanvas() {
        canvasBitmap?.eraseColor(Color.TRANSPARENT)
        for (i in cachedPointers.indices) {
            cachedPointers[i].active = false
        }
        segHead = 0
        segTail = 0
        segCount = 0
        invalidate()
    }

    private fun addFadeSegment(
        x1: Float,
        y1: Float,
        x2: Float,
        y2: Float,
        strokeWidth: Float,
        color: Int,
        isEraser: Boolean,
        now: Long
    ) {
        if (segCount < MAX_SEGMENTS) {
            segX1[segHead] = x1
            segY1[segHead] = y1
            segX2[segHead] = x2
            segY2[segHead] = y2
            segWidth[segHead] = strokeWidth
            segColor[segHead] = color
            segIsEraser[segHead] = isEraser
            segTimestamp[segHead] = now
            segHead = (segHead + 1) % MAX_SEGMENTS
            segCount++
        } else {
            // Overwrite oldest segment
            segTail = (segTail + 1) % MAX_SEGMENTS
            segX1[segHead] = x1
            segY1[segHead] = y1
            segX2[segHead] = x2
            segY2[segHead] = y2
            segWidth[segHead] = strokeWidth
            segColor[segHead] = color
            segIsEraser[segHead] = isEraser
            segTimestamp[segHead] = now
            segHead = (segHead + 1) % MAX_SEGMENTS
        }
    }

    /**
     * Called on each historical and current touch/stylus point from MainActivity.
     */
    fun processSample(
        pointerId: Int,
        action: Int,
        toolType: Int,
        x: Float,
        y: Float,
        pressure: Float,
        tiltX: Float,
        tiltY: Float
    ) {
        // Update sample rate
        sampleCountWindow++
        val now = System.currentTimeMillis()
        if (now - windowStartMs >= 1000) {
            sampleRateHz = ((sampleCountWindow * 1000f) / (now - windowStartMs)).roundToInt()
            sampleCountWindow = 0
            windowStartMs = now
        }

        lastToolType = toolType
        lastPressure = pressure
        lastTiltX = tiltX
        lastTiltY = tiltY

        val p = if (pointerId in 0 until 16) cachedPointers[pointerId] else null

        when (action) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                isHovering = false
                p?.apply {
                    active = true
                    this.x = x
                    this.y = y
                    this.pressure = pressure
                    this.toolType = toolType
                }
                // Draw initial contact dab
                drawStrokeSegment(x, y, x + 0.1f, y + 0.1f, pressure, toolType)
            }

            MotionEvent.ACTION_MOVE -> {
                isHovering = false
                if (p != null && p.active) {
                    drawStrokeSegment(p.x, p.y, x, y, (p.pressure + pressure) * 0.5f, toolType)
                    p.x = x
                    p.y = y
                    p.pressure = pressure
                } else {
                    p?.apply {
                        active = true
                        this.x = x
                        this.y = y
                        this.pressure = pressure
                        this.toolType = toolType
                    }
                }
            }

            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_CANCEL -> {
                p?.active = false
                isHovering = false
            }

            MotionEvent.ACTION_HOVER_MOVE, MotionEvent.ACTION_HOVER_ENTER -> {
                isHovering = true
                hoverX = x
                hoverY = y
            }

            MotionEvent.ACTION_HOVER_EXIT -> {
                isHovering = false
            }
        }

        invalidate()
    }

    private fun drawStrokeSegment(
        x1: Float,
        y1: Float,
        x2: Float,
        y2: Float,
        pressure: Float,
        toolType: Int
    ) {
        val density = resources.displayMetrics.density
        val isEraser = toolType == MotionEvent.TOOL_TYPE_ERASER
        val strokeW = if (isEraser) {
            48f * density
        } else {
            when (toolType) {
                MotionEvent.TOOL_TYPE_STYLUS -> (2.5f + pressure.coerceIn(0.01f, 1f) * 18f) * density
                else -> 6f * density
            }
        }
        val strokeColor = when (toolType) {
            MotionEvent.TOOL_TYPE_STYLUS -> colorStylus
            MotionEvent.TOOL_TYPE_ERASER -> colorEraser
            else -> colorFinger
        }

        if (inkMode == InkMode.OFF) {
            return
        }
        if (inkMode == InkMode.FADE) {
            val now = android.os.SystemClock.uptimeMillis()
            addFadeSegment(x1, y1, x2, y2, strokeW, strokeColor, isEraser, now)
        } else {
            val canvas = drawCanvas ?: return
            if (isEraser) {
                eraserPaint.strokeWidth = strokeW
                canvas.drawLine(x1, y1, x2, y2, eraserPaint)
            } else {
                inkPaint.color = strokeColor
                inkPaint.strokeWidth = strokeW
                canvas.drawLine(x1, y1, x2, y2, inkPaint)
            }
        }
    }

    /**
     * Checks if touch falls inside UI action buttons (Clear, Toggle HUD, Settings).
     * Returns true if consumed so it doesn't draw ink.
     */
    fun handleButtonTouch(x: Float, y: Float): Boolean {
        // 1. Settings modal has top priority if visible
        if (showSettingsModal) {
            if (modalCloseBtnRect.contains(x, y) || !modalCardRect.contains(x, y)) {
                showSettingsModal = false
                invalidate()
                return true
            }
            if (modalFps60Rect.contains(x, y)) {
                selectedFps = 60
                invalidate()
                return true
            }
            if (modalFps90Rect.contains(x, y)) {
                selectedFps = 90
                invalidate()
                return true
            }
            if (modalFps120Rect.contains(x, y)) {
                selectedFps = 120
                invalidate()
                return true
            }
            if (modalBitrateAutoRect.contains(x, y)) {
                selectedBitrateKbps = BITRATE_AUTO
                invalidate()
                return true
            }
            if (modalBitrate15Rect.contains(x, y)) {
                selectedBitrateKbps = 15000
                invalidate()
                return true
            }
            if (modalBitrate25Rect.contains(x, y)) {
                selectedBitrateKbps = 25000
                invalidate()
                return true
            }
            if (modalBitrate35Rect.contains(x, y)) {
                selectedBitrateKbps = 35000
                invalidate()
                return true
            }
            if (modalBitrate50Rect.contains(x, y)) {
                selectedBitrateKbps = 50000
                invalidate()
                return true
            }
            if (modalTouchAllRect.contains(x, y)) {
                currentTouchMode = TouchMode.ALL_TOUCH
                onSelectTouchMode?.invoke(TouchMode.ALL_TOUCH)
                invalidate()
                return true
            }
            if (modalTouchPenOnlyRect.contains(x, y)) {
                currentTouchMode = TouchMode.PEN_ONLY
                onSelectTouchMode?.invoke(TouchMode.PEN_ONLY)
                invalidate()
                return true
            }
            if (modalTouchScrollRect.contains(x, y)) {
                currentTouchMode = TouchMode.TWO_FINGER_SCROLL
                onSelectTouchMode?.invoke(TouchMode.TWO_FINGER_SCROLL)
                invalidate()
                return true
            }
            if (modalCurveLinearRect.contains(x, y)) {
                onSetPressureCurve?.invoke(PressureCurve.LINEAR)
                pressureCurveName = "LINEAR"
                invalidate()
                return true
            }
            if (modalCurveSoftRect.contains(x, y)) {
                onSetPressureCurve?.invoke(PressureCurve.SOFT)
                pressureCurveName = "SOFT"
                invalidate()
                return true
            }
            if (modalCurveFirmRect.contains(x, y)) {
                onSetPressureCurve?.invoke(PressureCurve.FIRM)
                pressureCurveName = "FIRM"
                invalidate()
                return true
            }
            if (modalCurveHardRect.contains(x, y)) {
                onSetPressureCurve?.invoke(PressureCurve.HARD)
                pressureCurveName = "HARD"
                invalidate()
                return true
            }
            if (modalApplyBtnRect.contains(x, y)) {
                onApplyStreamSettings?.invoke(selectedFps, selectedBitrateKbps)
                showSettingsModal = false
                invalidate()
                return true
            }
            return true
        }

        // 2. Menu Toggle Button (The ONE button)
        if (menuToggleBtnRect.contains(x, y)) {
            showMenu = !showMenu
            invalidate()
            return true
        }

        // 3. If Menu is open, process option clicks or dismiss on outside tap
        if (showMenu) {
            if (clearBtnRect.contains(x, y)) {
                clearCanvas()
                return true
            }
            if (hudToggleBtnRect.contains(x, y)) {
                showHud = !showHud
                invalidate()
                return true
            }
            if (pinchBtnRect.contains(x, y)) {
                onTogglePinchMode?.let { pinchZoomEnabled = it() }
                invalidate()
                return true
            }
            if (inkFadeBtnRect.contains(x, y)) {
                val nextMode = onCycleInkMode?.invoke() ?: when (inkMode) {
                    InkMode.FADE -> InkMode.OFF
                    InkMode.OFF -> InkMode.PERSIST
                    InkMode.PERSIST -> InkMode.FADE
                }
                inkMode = nextMode
                return true
            }
            if (barrelBtnRect.contains(x, y)) {
                onCycleBarrelMapping?.let { barrelMappingName = it() }
                invalidate()
                return true
            }
            if (pressureCurveBtnRect.contains(x, y)) {
                onCyclePressureCurve?.let { pressureCurveName = it() }
                invalidate()
                return true
            }
            if (penModeBtnRect.contains(x, y)) {
                onTogglePenOnly?.let { penOnlyMode = it() } ?: run { penOnlyMode = !penOnlyMode }
                invalidate()
                return true
            }
            if (keysToggleBtnRect.contains(x, y)) {
                showSidebar = !showSidebar
                invalidate()
                return true
            }
            if (settingsBtnRect.contains(x, y)) {
                showSettingsModal = true
                showMenu = false
                invalidate()
                return true
            }

            // Tapping inside menu card background consumes touch
            if (menuPanelRect.contains(x, y)) {
                return true
            }

            // Tapping anywhere outside the menu card dismisses the menu!
            showMenu = false
            invalidate()
            return true
        }

        // 4. Express Keys Sidebar Toggle & Buttons (Only active if showSidebar is true)
        if (showSidebar) {
            if (sidebarToggleBtnRect.contains(x, y)) {
                sidebarExpanded = !sidebarExpanded
                invalidate()
                return true
            }
            if (sidebarExpanded) {
                if (undoBtnRect.contains(x, y)) {
                    triggerKey(NativeEngine.VK_Z, NativeEngine.KEY_ACTION_TAP, NativeEngine.MOD_CTRL)
                    return true
                }
                if (redoBtnRect.contains(x, y)) {
                    triggerKey(NativeEngine.VK_Y, NativeEngine.KEY_ACTION_TAP, NativeEngine.MOD_CTRL)
                    return true
                }
                if (ctrlBtnRect.contains(x, y)) {
                    isCtrlLatched = !isCtrlLatched
                    val act = if (isCtrlLatched) NativeEngine.KEY_ACTION_DOWN else NativeEngine.KEY_ACTION_UP
                    triggerKey(NativeEngine.VK_CONTROL, act)
                    invalidate()
                    return true
                }
                if (shiftBtnRect.contains(x, y)) {
                    isShiftLatched = !isShiftLatched
                    val act = if (isShiftLatched) NativeEngine.KEY_ACTION_DOWN else NativeEngine.KEY_ACTION_UP
                    triggerKey(NativeEngine.VK_SHIFT, act)
                    invalidate()
                    return true
                }
                if (altBtnRect.contains(x, y)) {
                    isAltLatched = !isAltLatched
                    val act = if (isAltLatched) NativeEngine.KEY_ACTION_DOWN else NativeEngine.KEY_ACTION_UP
                    triggerKey(NativeEngine.VK_MENU, act)
                    invalidate()
                    return true
                }
                if (spaceBtnRect.contains(x, y)) {
                    isSpaceLatched = !isSpaceLatched
                    val act = if (isSpaceLatched) NativeEngine.KEY_ACTION_DOWN else NativeEngine.KEY_ACTION_UP
                    triggerKey(NativeEngine.VK_SPACE, act)
                    invalidate()
                    return true
                }
                if (brushMinusBtnRect.contains(x, y)) {
                    triggerKey(NativeEngine.VK_OEM_4, NativeEngine.KEY_ACTION_TAP)
                    return true
                }
                if (brushPlusBtnRect.contains(x, y)) {
                    triggerKey(NativeEngine.VK_OEM_6, NativeEngine.KEY_ACTION_TAP)
                    return true
                }
                if (fitBtnRect.contains(x, y)) {
                    triggerKey(NativeEngine.VK_0, NativeEngine.KEY_ACTION_TAP, NativeEngine.MOD_CTRL)
                    return true
                }
                if (tabUiBtnRect.contains(x, y)) {
                    triggerKey(NativeEngine.VK_TAB, NativeEngine.KEY_ACTION_TAP)
                    return true
                }
                if (sidebarRect.contains(x, y)) {
                    return true
                }
            }
        }

        // 5. Minimap reset if zoomed
        if (zoomScale > 1.01f && minimapRect.contains(x, y)) {
            onResetZoom?.invoke()
            return true
        }

        return false
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val density = resources.displayMetrics.density

        // 0. Render Standalone Cyberpunk Landing Screen if waiting for USB connection
        if (linkStatus != NativeEngine.STATUS_CONNECTED) {
            drawLandingScreen(canvas, density)
            postInvalidateOnAnimation()
            return
        }

        // 1. Render ink: either fading predictive overlay (<160ms ring buffer) or persistent canvas
        if (inkFadeEnabled) {
            val now = android.os.SystemClock.uptimeMillis()
            var hasRemaining = false
            var idx = segTail
            val count = segCount
            for (step in 0 until count) {
                val age = now - segTimestamp[idx]
                if (age >= FADE_DURATION_MS) {
                    segTail = (segTail + 1) % MAX_SEGMENTS
                    segCount--
                } else {
                    hasRemaining = true
                    val fraction = (age.toFloat() / FADE_DURATION_MS).coerceIn(0f, 1f)
                    val alpha = ((1f - fraction) * 255).roundToInt().coerceIn(0, 255)
                    if (segIsEraser[idx]) {
                        eraserPaint.strokeWidth = segWidth[idx]
                        canvas.drawLine(segX1[idx], segY1[idx], segX2[idx], segY2[idx], eraserPaint)
                    } else {
                        fadePaint.color = segColor[idx]
                        fadePaint.alpha = alpha
                        fadePaint.strokeWidth = segWidth[idx]
                        canvas.drawLine(segX1[idx], segY1[idx], segX2[idx], segY2[idx], fadePaint)
                    }
                }
                idx = (idx + 1) % MAX_SEGMENTS
            }
            if (hasRemaining) {
                postInvalidateOnAnimation()
            }
        } else {
            canvasBitmap?.let { bmp ->
                canvas.drawBitmap(bmp, 0f, 0f, null)
            }
        }

        // 2. Render live stylus hover / pressure reticle
        if (isHovering) {
            cursorPaint.color = Color.argb(160, 0, 240, 255)
            cursorPaint.style = Paint.Style.STROKE
            cursorPaint.strokeWidth = 2f * density
            canvas.drawCircle(hoverX, hoverY, 12f * density, cursorPaint)
        } else {
            for (i in cachedPointers.indices) {
                val pt = cachedPointers[i]
                if (!pt.active) continue
                val radius = (4f + pt.pressure * 16f) * density
                cursorPaint.color = when (pt.toolType) {
                    MotionEvent.TOOL_TYPE_STYLUS -> colorStylus
                    MotionEvent.TOOL_TYPE_ERASER -> colorEraser
                    else -> colorFinger
                }
                cursorPaint.style = Paint.Style.STROKE
                cursorPaint.strokeWidth = 2f * density
                canvas.drawCircle(pt.x, pt.y, radius, cursorPaint)

                cursorPaint.style = Paint.Style.FILL
                cursorPaint.alpha = 100
                canvas.drawCircle(pt.x, pt.y, radius * 0.4f, cursorPaint)
            }
        }

        // 2b. Render Decoupled 120Hz Local Hardware Cursor
        if (cursorVisible && cursorShape != NativeEngine.CURSOR_SHAPE_HIDDEN) {
            drawHardwareCursor(canvas, density)
        }

        // 3. Render Top-Right Menu Button (The ONE button visible by default)
        val menuBtnW = 86f * density
        val menuBtnH = 34f * density
        val topMargin = 14f * density
        val rightMargin = width - 14f * density

        menuToggleBtnRect.set(rightMargin - menuBtnW, topMargin, rightMargin, topMargin + menuBtnH)

        val menuPaint = if (showMenu) btnActivePaint else btnPaint
        canvas.drawRoundRect(menuToggleBtnRect, 10f * density, 10f * density, menuPaint)
        canvas.drawRoundRect(menuToggleBtnRect, 10f * density, 10f * density, hudBorderPaint)
        val menuLabel = if (showMenu) "✕ CLOSE" else "⚙ MENU"
        btnTextPaint.textSize = 12f * density
        canvas.drawText(menuLabel, menuToggleBtnRect.centerX(), menuToggleBtnRect.centerY() + 5f * density, btnTextPaint)

        // 4. Render Options Panel (ALL options remain hidden until showMenu is true)
        if (showMenu) {
            val optBtnW = 126f * density
            val optBtnH = 34f * density
            val optSpacing = 8f * density
            val panelPad = 10f * density

            val panelW = (5 * optBtnW) + (4 * optSpacing) + (panelPad * 2)
            val panelH = (2 * optBtnH) + optSpacing + (panelPad * 2)
            val panelRight = rightMargin
            val panelTop = menuToggleBtnRect.bottom + 8f * density
            val panelLeft = panelRight - panelW
            val panelBottom = panelTop + panelH

            menuPanelRect.set(panelLeft, panelTop, panelRight, panelBottom)
            canvas.drawRoundRect(menuPanelRect, 14f * density, 14f * density, hudBgPaint)
            canvas.drawRoundRect(menuPanelRect, 14f * density, 14f * density, hudBorderPaint)

            // Row 1: CLEAR, HUD, PINCH, INK FADE
            val r1Top = panelTop + panelPad
            val r1Bottom = r1Top + optBtnH
            var curX = panelLeft + panelPad

            clearBtnRect.set(curX, r1Top, curX + optBtnW, r1Bottom)
            curX += optBtnW + optSpacing
            hudToggleBtnRect.set(curX, r1Top, curX + optBtnW, r1Bottom)
            curX += optBtnW + optSpacing
            pinchBtnRect.set(curX, r1Top, curX + optBtnW, r1Bottom)
            curX += optBtnW + optSpacing
            inkFadeBtnRect.set(curX, r1Top, curX + optBtnW, r1Bottom)

            drawMenuOptionBtn(canvas, clearBtnRect, "CLEAR 🗑", false, density)
            drawMenuOptionBtn(canvas, hudToggleBtnRect, if (showHud) "HUD: ON 👁" else "HUD: OFF ✕", showHud, density)
            drawMenuOptionBtn(canvas, pinchBtnRect, if (pinchZoomEnabled) "PINCH: ZOOM" else "PINCH: PC", pinchZoomEnabled, density)
            drawMenuOptionBtn(canvas, inkFadeBtnRect, inkMode.label, inkMode != InkMode.OFF, density)

            // Row 2: BARREL, CURVE, PEN MODE, KEYS, SETTINGS
            val r2Top = r1Bottom + optSpacing
            val r2Bottom = r2Top + optBtnH
            curX = panelLeft + panelPad

            barrelBtnRect.set(curX, r2Top, curX + optBtnW, r2Bottom)
            curX += optBtnW + optSpacing
            pressureCurveBtnRect.set(curX, r2Top, curX + optBtnW, r2Bottom)
            curX += optBtnW + optSpacing
            penModeBtnRect.set(curX, r2Top, curX + optBtnW, r2Bottom)
            curX += optBtnW + optSpacing
            keysToggleBtnRect.set(curX, r2Top, curX + optBtnW, r2Bottom)
            curX += optBtnW + optSpacing
            settingsBtnRect.set(curX, r2Top, curX + optBtnW, r2Bottom)

            drawMenuOptionBtn(canvas, barrelBtnRect, "BARREL: $barrelMappingName", false, density)
            drawMenuOptionBtn(canvas, pressureCurveBtnRect, "CURVE: $pressureCurveName", false, density)
            drawMenuOptionBtn(canvas, penModeBtnRect, if (penOnlyMode) "PEN: ONLY 🖊" else "PEN: ALL 🖐", penOnlyMode, density)
            drawMenuOptionBtn(canvas, keysToggleBtnRect, if (showSidebar) "KEYS: ON ◀" else "KEYS: OFF ▶", showSidebar, density)
            drawMenuOptionBtn(canvas, settingsBtnRect, "SETTINGS ⚙", showSettingsModal, density)
        }

        // 5. Render Minimap if zoomed
        if (zoomScale > 1.01f) {
            drawMinimap(canvas, density)
        }

        // 6. Render Express Keys Sidebar (only if showSidebar == true)
        if (showSidebar) {
            drawShortcutSidebar(canvas, density)
        }

        // 7. Render Diagnostic HUD Dashboard (only if showHud == true)
        if (showHud) {
            drawDiagnosticHud(canvas, density)
        }

        // 8. Render Settings Modal Dialog (only if showSettingsModal == true)
        if (showSettingsModal) {
            drawSettingsModal(canvas, density)
        }
    }

    private fun drawMenuOptionBtn(canvas: Canvas, rect: RectF, text: String, isActive: Boolean, density: Float) {
        val paint = if (isActive) btnActivePaint else btnPaint
        canvas.drawRoundRect(rect, 8f * density, 8f * density, paint)
        canvas.drawRoundRect(rect, 8f * density, 8f * density, hudBorderPaint)
        btnTextPaint.textSize = 11f * density
        canvas.drawText(text, rect.centerX(), rect.centerY() + 4.5f * density, btnTextPaint)
    }

    /** Express keys vertical sidebar for drawing shortcuts (Undo, Redo, Modifiers, Brush). */
    private fun drawShortcutSidebar(canvas: Canvas, density: Float) {
        if (!showSidebar) return
        val margin = 14f * density
        val btnH = 34f * density
        val btnW = 86f * density
        val spacing = 6f * density

        val toggleW = if (sidebarExpanded) 86f * density else 78f * density
        sidebarToggleBtnRect.set(margin, margin + 44f * density, margin + toggleW, margin + 44f * density + btnH)

        canvas.drawRoundRect(sidebarToggleBtnRect, 8f * density, 8f * density, btnPaint)
        canvas.drawRoundRect(sidebarToggleBtnRect, 8f * density, 8f * density, hudBorderPaint)
        val toggleText = if (sidebarExpanded) "◀ KEYS" else "KEYS ▶"
        btnTextPaint.textSize = 12f * density
        canvas.drawText(toggleText, sidebarToggleBtnRect.centerX(), sidebarToggleBtnRect.centerY() + 5f * density, btnTextPaint)

        if (!sidebarExpanded) return

        var top = sidebarToggleBtnRect.bottom + 8f * density
        val totalHeight = (sidebarItems.size * (btnH + spacing)) + 8f * density
        sidebarRect.set(margin - 4f * density, top - 4f * density, margin + btnW + 4f * density, top + totalHeight)
        canvas.drawRoundRect(sidebarRect, 12f * density, 12f * density, hudBgPaint)
        canvas.drawRoundRect(sidebarRect, 12f * density, 12f * density, hudBorderPaint)

        for (item in sidebarItems) {
            item.rect.set(margin, top, margin + btnW, top + btnH)
            val fillPaint = if (item.isLatched()) btnActivePaint else btnPaint
            canvas.drawRoundRect(item.rect, 8f * density, 8f * density, fillPaint)
            canvas.drawRoundRect(item.rect, 8f * density, 8f * density, hudBorderPaint)
            btnTextPaint.textSize = 11f * density
            canvas.drawText(item.label, item.rect.centerX(), item.rect.centerY() + 5f * density, btnTextPaint)
            top += btnH + spacing
        }
        btnTextPaint.textSize = 26f
    }

    /** Bottom-right thumbnail of the whole display with the visible region outlined. */
    @Suppress("DefaultLocale")
    private fun drawMinimap(canvas: Canvas, density: Float) {
        if (width <= 0 || height <= 0) return
        val mapW = 180f * density
        val mapH = mapW * height / width
        val margin = 20f * density
        minimapRect.set(width - margin - mapW, height - margin - mapH, width - margin, height - margin)

        canvas.drawRoundRect(minimapRect, 8f * density, 8f * density, minimapBgPaint)
        canvas.drawRoundRect(minimapRect, 8f * density, 8f * density, hudBorderPaint)

        // Visible content rect in normalized display coordinates
        val visLeft = (-zoomPanX / zoomScale) / width
        val visTop = (-zoomPanY / zoomScale) / height
        val visSize = 1f / zoomScale
        minimapViewRect.set(
            minimapRect.left + visLeft * mapW,
            minimapRect.top + visTop * mapH,
            minimapRect.left + (visLeft + visSize) * mapW,
            minimapRect.top + (visTop + visSize) * mapH
        )
        minimapViewPaint.strokeWidth = 2f * density
        canvas.drawRect(minimapViewRect, minimapViewPaint)

        btnTextPaint.textSize = 14f * density
        canvas.drawText(
            String.format("%.1fx · tap to reset", zoomScale),
            minimapRect.centerX(), minimapRect.top - 8f * density, btnTextPaint
        )
        btnTextPaint.textSize = 26f
    }

    @Suppress("DefaultLocale")
    private fun drawDiagnosticHud(canvas: Canvas, density: Float) {
        val hudLeft = 24f * density
        val hudTop = 20f * density
        val hudW = 420f * density
        val hudH = 305f * density

        hudCardRect.set(hudLeft, hudTop, hudLeft + hudW, hudTop + hudH)
        canvas.drawRoundRect(hudCardRect, 16f * density, 16f * density, hudBgPaint)
        canvas.drawRoundRect(hudCardRect, 16f * density, 16f * density, hudBorderPaint)

        var curY = hudTop + 32f * density
        val curX = hudLeft + 20f * density

        // Header: Link status
        val statusDotColor: Int
        val statusText: String
        when (linkStatus) {
            NativeEngine.STATUS_CONNECTED -> {
                statusDotColor = Color.rgb(0, 255, 136)
                statusText = "CONNECTED ($targetLabel)"
            }
            NativeEngine.STATUS_CONNECTING -> {
                statusDotColor = Color.rgb(255, 184, 0)
                statusText = "CONNECTING ($targetLabel)..."
            }
            else -> {
                statusDotColor = Color.rgb(255, 82, 82)
                statusText = "DISCONNECTED (${if (isUsbLink) "waiting for USB" else "reconnecting"})"
            }
        }

        hudDotPaint.color = statusDotColor
        canvas.drawCircle(curX + 6f * density, curY - 10f * density, 6f * density, hudDotPaint)

        textTitlePaint.textSize = 20f * density
        textTitlePaint.color = statusDotColor
        canvas.drawText(statusText, curX + 18f * density, curY - 2f * density, textTitlePaint)

        curY += 28f * density

        // Row 1: Tool Type
        val toolStr = when (lastToolType) {
            MotionEvent.TOOL_TYPE_STYLUS -> "STYLUS (Pen Digitizer)"
            MotionEvent.TOOL_TYPE_ERASER -> "ERASER (Pen Inverted)"
            MotionEvent.TOOL_TYPE_FINGER -> "FINGER (Multi-touch)"
            else -> "IDLE (Ready)"
        }
        textBodyPaint.textSize = 15f * density
        textAccentPaint.textSize = 15f * density
        canvas.drawText("TOOL: ", curX, curY, textBodyPaint)
        textAccentPaint.color = when (lastToolType) {
            MotionEvent.TOOL_TYPE_STYLUS -> colorStylus
            MotionEvent.TOOL_TYPE_ERASER -> colorEraser
            else -> colorFinger
        }
        canvas.drawText(toolStr, curX + 60f * density, curY, textAccentPaint)

        curY += 24f * density

        // Row 2: Pressure Bar & Value
        canvas.drawText("PRESSURE: ", curX, curY, textBodyPaint)
        val pressurePct = (lastPressure * 100f).roundToInt()
        val pressureStr = String.format("%.3f (%d%%)", lastPressure, pressurePct)
        canvas.drawText(pressureStr, curX + 96f * density, curY, textAccentPaint)

        // Visual pressure progress bar
        val barW = 160f * density
        val barH = 10f * density
        val barX = curX + 220f * density
        val barY = curY - 10f * density
        hudBarBgRect.set(barX, barY, barX + barW, barY + barH)
        canvas.drawRoundRect(hudBarBgRect, 5f * density, 5f * density, barBgPaint)
        val fillW = (barW * lastPressure.coerceIn(0f, 1f))
        if (fillW > 0f) {
            barFillPaint.color = when (lastToolType) {
                MotionEvent.TOOL_TYPE_ERASER -> colorEraser
                else -> colorStylus
            }
            hudBarFillRect.set(barX, barY, barX + fillW, barY + barH)
            canvas.drawRoundRect(hudBarFillRect, 5f * density, 5f * density, barFillPaint)
        }

        curY += 24f * density

        // Row 3: Tilt & Digitizer Rate
        val tiltStr = String.format("TILT: X %+.0f° Y %+.0f° | RATE: %d Hz", lastTiltX, lastTiltY, sampleRateHz)
        canvas.drawText(tiltStr, curX, curY, textBodyPaint)

        curY += 24f * density

        // Row 4: Streamed packets & USB Roundtrip Latency (RTT)
        val rttMs = rttUs / 1000.0f
        val rttStr = if (rttUs > 0) String.format("%.2f ms", rttMs) else "< 1 ms"
        val packetsStr = String.format("INPUT: %,d evts | RTT: %s", packetsSent, rttStr)
        canvas.drawText(packetsStr, curX, curY, textBodyPaint)

        curY += 24f * density

        // Row 5: Pen & Touch Settings Status
        val modeStr = if (penOnlyMode) "PEN ONLY" else "ALL TOUCH"
        val configStr = String.format("SETTINGS: %s | %s curve | %s barrel", modeStr, pressureCurveName, barrelMappingName)
        canvas.drawText(configStr, curX, curY, textBodyPaint)

        curY += 24f * density

        // Row 6: Synthetic PC injection status badge
        textBodyPaint.textSize = 13f * density
        textBodyPaint.color = Color.rgb(130, 160, 190)
        canvas.drawText("✨ Windows Synthetic Pen: Open MS Paint / Krita to draw!", curX, curY, textBodyPaint)
    }

    /** Renders the glassmorphic Settings modal dialog (Feature #2). */
    private fun drawSettingsModal(canvas: Canvas, density: Float) {
        // Dark overlay backdrop
        canvas.drawColor(Color.argb(175, 4, 8, 16))

        val modalW = 560f * density
        val modalH = 460f * density
        val modalLeft = (width - modalW) / 2f
        val modalTop = (height - modalH) / 2f
        modalCardRect.set(modalLeft, modalTop, modalLeft + modalW, modalTop + modalH)

        modalBorderPaint.strokeWidth = 2.5f * density
        canvas.drawRoundRect(modalCardRect, 20f * density, 20f * density, modalBgPaint)
        canvas.drawRoundRect(modalCardRect, 20f * density, 20f * density, modalBorderPaint)

        // Close button (X)
        modalCloseBtnRect.set(modalCardRect.right - 54f * density, modalCardRect.top + 8f * density, modalCardRect.right - 8f * density, modalCardRect.top + 54f * density)
        canvas.drawRoundRect(modalCloseBtnRect, 10f * density, 10f * density, btnPaint)
        canvas.drawRoundRect(modalCloseBtnRect, 10f * density, 10f * density, hudBorderPaint)
        canvas.drawText("✕", modalCloseBtnRect.centerX(), modalCloseBtnRect.centerY() + 8f * density, btnTextPaint)

        // Title
        modalHeaderPaint.textSize = 20f * density
        canvas.drawText("⚙ XDisplay Stream & Input Settings", modalCardRect.left + 24f * density, modalCardRect.top + 34f * density, modalHeaderPaint)

        var curY = modalCardRect.top + 60f * density
        modalLabelPaint.textSize = 13f * density

        // Section 1: Target Refresh Rate (FPS)
        canvas.drawText("TARGET REFRESH RATE (FPS)", modalCardRect.left + 24f * density, curY, modalLabelPaint)
        curY += 10f * density
        val chipH = 34f * density
        val chipW = 90f * density
        val chipSpacing = 10f * density

        modalFps60Rect.set(modalCardRect.left + 24f * density, curY, modalCardRect.left + 24f * density + chipW, curY + chipH)
        modalFps90Rect.set(modalFps60Rect.right + chipSpacing, curY, modalFps60Rect.right + chipSpacing + chipW, curY + chipH)
        modalFps120Rect.set(modalFps90Rect.right + chipSpacing, curY, modalFps90Rect.right + chipSpacing + chipW, curY + chipH)

        drawChip(canvas, modalFps60Rect, "60 Hz", selectedFps == 60, density)
        drawChip(canvas, modalFps90Rect, "90 Hz", selectedFps == 90, density)
        drawChip(canvas, modalFps120Rect, "120 Hz", selectedFps == 120, density)

        curY += chipH + 20f * density

        // Section 2: Video Bitrate
        canvas.drawText("VIDEO BITRATE (QUALITY)", modalCardRect.left + 24f * density, curY, modalLabelPaint)
        curY += 10f * density
        val bitChipW = 94f * density
        modalBitrateAutoRect.set(modalCardRect.left + 24f * density, curY, modalCardRect.left + 24f * density + bitChipW, curY + chipH)
        modalBitrate15Rect.set(modalBitrateAutoRect.right + chipSpacing, curY, modalBitrateAutoRect.right + chipSpacing + bitChipW, curY + chipH)
        modalBitrate25Rect.set(modalBitrate15Rect.right + chipSpacing, curY, modalBitrate15Rect.right + chipSpacing + bitChipW, curY + chipH)
        modalBitrate35Rect.set(modalBitrate25Rect.right + chipSpacing, curY, modalBitrate25Rect.right + chipSpacing + bitChipW, curY + chipH)
        modalBitrate50Rect.set(modalBitrate35Rect.right + chipSpacing, curY, modalBitrate35Rect.right + chipSpacing + bitChipW, curY + chipH)

        drawChip(canvas, modalBitrateAutoRect, "AUTO", selectedBitrateKbps == BITRATE_AUTO, density)
        drawChip(canvas, modalBitrate15Rect, "15 Mbps", selectedBitrateKbps == 15000, density)
        drawChip(canvas, modalBitrate25Rect, "25 Mbps", selectedBitrateKbps == 25000, density)
        drawChip(canvas, modalBitrate35Rect, "35 Mbps", selectedBitrateKbps == 35000, density)
        drawChip(canvas, modalBitrate50Rect, "50 Mbps", selectedBitrateKbps == 50000, density)

        curY += chipH + 20f * density

        // Section 3: Touch & Gesture Mode
        canvas.drawText("TOUCH & GESTURE MODE", modalCardRect.left + 24f * density, curY, modalLabelPaint)
        curY += 10f * density
        val touchChipW = 135f * density
        modalTouchAllRect.set(modalCardRect.left + 24f * density, curY, modalCardRect.left + 24f * density + touchChipW, curY + chipH)
        modalTouchPenOnlyRect.set(modalTouchAllRect.right + chipSpacing, curY, modalTouchAllRect.right + chipSpacing + touchChipW, curY + chipH)
        modalTouchScrollRect.set(modalTouchPenOnlyRect.right + chipSpacing, curY, modalTouchPenOnlyRect.right + chipSpacing + touchChipW, curY + chipH)

        drawChip(canvas, modalTouchAllRect, "TOUCH + PEN 🖐", currentTouchMode == TouchMode.ALL_TOUCH, density)
        drawChip(canvas, modalTouchPenOnlyRect, "PEN ONLY 🖊", currentTouchMode == TouchMode.PEN_ONLY, density)
        drawChip(canvas, modalTouchScrollRect, "2F SCROLL 📜", currentTouchMode == TouchMode.TWO_FINGER_SCROLL, density)

        curY += chipH + 20f * density

        // Section 4: Pressure Curve
        canvas.drawText("PRESSURE CURVE", modalCardRect.left + 24f * density, curY, modalLabelPaint)
        curY += 10f * density
        val curveChipW = 90f * density
        modalCurveLinearRect.set(modalCardRect.left + 24f * density, curY, modalCardRect.left + 24f * density + curveChipW, curY + chipH)
        modalCurveSoftRect.set(modalCurveLinearRect.right + chipSpacing, curY, modalCurveLinearRect.right + chipSpacing + curveChipW, curY + chipH)
        modalCurveFirmRect.set(modalCurveSoftRect.right + chipSpacing, curY, modalCurveSoftRect.right + chipSpacing + curveChipW, curY + chipH)
        modalCurveHardRect.set(modalCurveFirmRect.right + chipSpacing, curY, modalCurveFirmRect.right + chipSpacing + curveChipW, curY + chipH)

        drawChip(canvas, modalCurveLinearRect, "LINEAR", pressureCurveName == "LINEAR", density)
        drawChip(canvas, modalCurveSoftRect, "SOFT", pressureCurveName == "SOFT", density)
        drawChip(canvas, modalCurveFirmRect, "FIRM", pressureCurveName == "FIRM", density)
        drawChip(canvas, modalCurveHardRect, "HARD", pressureCurveName == "HARD", density)

        curY += chipH + 26f * density

        // Bottom Action Button: APPLY STREAM SETTINGS
        val applyW = modalW - 48f * density
        val applyH = 42f * density
        modalApplyBtnRect.set(modalCardRect.left + 24f * density, curY, modalCardRect.left + 24f * density + applyW, curY + applyH)
        modalApplyTextPaint.textSize = 15f * density
        canvas.drawRoundRect(modalApplyBtnRect, 10f * density, 10f * density, modalApplyPaint)
        val bitrateLabel = if (selectedBitrateKbps == BITRATE_AUTO) "auto bitrate" else "${selectedBitrateKbps / 1000} Mbps"
        canvas.drawText("APPLY STREAM SETTINGS ⚡ ($selectedFps Hz @ $bitrateLabel)", modalApplyBtnRect.centerX(), modalApplyBtnRect.centerY() + 6f * density, modalApplyTextPaint)
    }

    private fun drawChip(canvas: Canvas, rect: RectF, text: String, isSelected: Boolean, density: Float) {
        val bgPaint = if (isSelected) btnActivePaint else btnPaint
        canvas.drawRoundRect(rect, 8f * density, 8f * density, bgPaint)
        canvas.drawRoundRect(rect, 8f * density, 8f * density, hudBorderPaint)
        btnTextPaint.color = if (isSelected) Color.WHITE else Color.rgb(200, 220, 240)
        canvas.drawText(text, rect.centerX(), rect.centerY() + 5f * density, btnTextPaint)
    }

    private fun drawHardwareCursor(canvas: Canvas, density: Float) {
        when (cursorShape) {
            NativeEngine.CURSOR_SHAPE_IBEAM -> {
                canvas.save()
                canvas.translate(cursorScreenX, cursorScreenY)
                val h = 10f * density
                val w = 4f * density
                cursorStrokePaint.color = Color.BLACK
                cursorStrokePaint.strokeWidth = 3f * density
                canvas.drawLine(-w, -h, w, -h, cursorStrokePaint)
                canvas.drawLine(0f, -h, 0f, h, cursorStrokePaint)
                canvas.drawLine(-w, h, w, h, cursorStrokePaint)

                cursorStrokePaint.color = Color.WHITE
                cursorStrokePaint.strokeWidth = 1.5f * density
                canvas.drawLine(-w, -h, w, -h, cursorStrokePaint)
                canvas.drawLine(0f, -h, 0f, h, cursorStrokePaint)
                canvas.drawLine(-w, h, w, h, cursorStrokePaint)
                canvas.restore()
            }
            NativeEngine.CURSOR_SHAPE_CROSS -> {
                canvas.save()
                canvas.translate(cursorScreenX, cursorScreenY)
                val arm = 8f * density
                cursorStrokePaint.color = Color.BLACK
                cursorStrokePaint.strokeWidth = 3f * density
                canvas.drawLine(-arm, 0f, arm, 0f, cursorStrokePaint)
                canvas.drawLine(0f, -arm, 0f, arm, cursorStrokePaint)

                cursorStrokePaint.color = Color.WHITE
                cursorStrokePaint.strokeWidth = 1.5f * density
                canvas.drawLine(-arm, 0f, arm, 0f, cursorStrokePaint)
                canvas.drawLine(0f, -arm, 0f, arm, cursorStrokePaint)
                canvas.restore()
            }
            NativeEngine.CURSOR_SHAPE_SIZE_ALL -> {
                canvas.save()
                canvas.translate(cursorScreenX, cursorScreenY)
                val arm = 9f * density
                cursorStrokePaint.color = Color.BLACK
                cursorStrokePaint.strokeWidth = 3f * density
                canvas.drawLine(-arm, 0f, arm, 0f, cursorStrokePaint)
                canvas.drawLine(0f, -arm, 0f, arm, cursorStrokePaint)

                cursorStrokePaint.color = Color.WHITE
                cursorStrokePaint.strokeWidth = 1.5f * density
                canvas.drawLine(-arm, 0f, arm, 0f, cursorStrokePaint)
                canvas.drawLine(0f, -arm, 0f, arm, cursorStrokePaint)
                canvas.restore()
            }
            else -> {
                // Precision Arrow Pointer (Arrow, Hand, or fallback)
                canvas.save()
                canvas.translate(cursorScreenX, cursorScreenY)

                arrowPath.reset()
                arrowPath.moveTo(0f, 0f)
                arrowPath.lineTo(0f, 20f * density)
                arrowPath.lineTo(5f * density, 15.5f * density)
                arrowPath.lineTo(8.5f * density, 23f * density)
                arrowPath.lineTo(11.5f * density, 21.5f * density)
                arrowPath.lineTo(8.2f * density, 14.5f * density)
                arrowPath.lineTo(14.5f * density, 14.5f * density)
                arrowPath.close()

                // Soft drop shadow
                canvas.save()
                canvas.translate(1.5f * density, 1.5f * density)
                canvas.drawPath(arrowPath, cursorShadowPaint)
                canvas.restore()

                // White body with crisp black border
                cursorStrokePaint.color = Color.BLACK
                cursorStrokePaint.strokeWidth = 1.5f * density
                canvas.drawPath(arrowPath, cursorFillPaint)
                canvas.drawPath(arrowPath, cursorStrokePaint)
                canvas.restore()
            }
        }
    }

    private fun drawLandingScreen(canvas: Canvas, density: Float) {
        val w = width.toFloat()
        val h = height.toFloat()
        if (w <= 0f || h <= 0f) return

        // Deep obsidian midnight background
        canvas.drawColor(Color.rgb(10, 14, 23))

        val centerX = w / 2f
        val centerY = h / 2f

        val now = android.os.SystemClock.uptimeMillis()
        val pulse = (kotlin.math.sin(now / 350.0) * 0.5 + 0.5).toFloat()

        // Glowing ambient pulse ring in the center
        val ringRadius = (54f + pulse * 14f) * density
        val ringAlpha = (40 + (1f - pulse) * 120).roundToInt().coerceIn(0, 255)
        landingRingPaint.color = Color.argb(ringAlpha, 0, 240, 255)
        landingRingPaint.style = Paint.Style.STROKE
        landingRingPaint.strokeWidth = 2.5f * density
        canvas.drawCircle(centerX, centerY - 60f * density, ringRadius, landingRingPaint)

        // Core icon: Stylized Monitor / Tablet
        val iconW = 64f * density
        val iconH = 44f * density
        val iconLeft = centerX - iconW / 2f
        val iconTop = centerY - 60f * density - iconH / 2f
        landingIconRect.set(iconLeft, iconTop, iconLeft + iconW, iconTop + iconH)

        landingIconPaint.color = Color.argb(220, 16, 26, 42)
        landingIconPaint.style = Paint.Style.FILL
        canvas.drawRoundRect(landingIconRect, 8f * density, 8f * density, landingIconPaint)

        landingIconPaint.color = Color.rgb(0, 240, 255)
        landingIconPaint.style = Paint.Style.STROKE
        landingIconPaint.strokeWidth = 2.5f * density
        canvas.drawRoundRect(landingIconRect, 8f * density, 8f * density, landingIconPaint)

        // Stand / base
        canvas.drawLine(centerX, iconTop + iconH, centerX, iconTop + iconH + 10f * density, landingIconPaint)
        canvas.drawLine(centerX - 16f * density, iconTop + iconH + 10f * density, centerX + 16f * density, iconTop + iconH + 10f * density, landingIconPaint)

        // App Brand Title
        landingTitlePaint.textSize = 34f * density
        landingTitlePaint.color = Color.WHITE
        landingTitlePaint.textAlign = Paint.Align.CENTER
        landingTitlePaint.typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        canvas.drawText("X D I S P L A Y", centerX, centerY + 30f * density, landingTitlePaint)

        // Subtitle
        landingSubPaint.textSize = 13f * density
        landingSubPaint.color = Color.rgb(0, 240, 255)
        landingSubPaint.textAlign = Paint.Align.CENTER
        landingSubPaint.typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
        canvas.drawText("LOW-LATENCY SECOND SCREEN", centerX, centerY + 52f * density, landingSubPaint)

        val isConnecting = (linkStatus == NativeEngine.STATUS_CONNECTING)
        val statusText = when {
            isConnecting -> "⚡ CONNECTING TO $targetLabel..."
            isUsbLink -> "🔌 WAITING FOR USB CONNECTION"
            else -> "📡 RECONNECTING TO $targetLabel..."
        }
        landingStatusTextPaint.textSize = 12f * density
        landingStatusTextPaint.typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)

        // Connection status badge, sized to its text (long PC names) but never wider than the screen.
        val badgeW = (landingStatusTextPaint.measureText(statusText) + 52f * density)
            .coerceIn(220f * density, width - 32f * density)
        val badgeH = 38f * density
        val badgeRect = RectF(centerX - badgeW / 2f, centerY + 78f * density, centerX + badgeW / 2f, centerY + 78f * density + badgeH)
        val statusColor = if (isConnecting) Color.rgb(255, 184, 0) else Color.rgb(0, 240, 255)

        landingBadgePaint.color = Color.argb(190, 18, 25, 38)
        landingBadgePaint.style = Paint.Style.FILL
        canvas.drawRoundRect(badgeRect, 19f * density, 19f * density, landingBadgePaint)

        landingBadgePaint.color = Color.argb(120, 0, 240, 255)
        landingBadgePaint.style = Paint.Style.STROKE
        landingBadgePaint.strokeWidth = 1.5f * density
        canvas.drawRoundRect(badgeRect, 19f * density, 19f * density, landingBadgePaint)

        // Status indicator dot
        landingBadgePaint.style = Paint.Style.FILL
        landingBadgePaint.color = statusColor
        canvas.drawCircle(centerX - badgeW / 2f + 20f * density, badgeRect.centerY(), 5f * density, landingBadgePaint)

        landingStatusTextPaint.color = Color.WHITE
        landingStatusTextPaint.textAlign = Paint.Align.LEFT
        canvas.save()
        canvas.clipRect(badgeRect.left, badgeRect.top, badgeRect.right - 14f * density, badgeRect.bottom)
        canvas.drawText(statusText, centerX - badgeW / 2f + 32f * density, badgeRect.centerY() + 4.5f * density, landingStatusTextPaint)
        canvas.restore()

        // Device specifications footer chips
        val chipY = centerY + 145f * density
        val chipText1 = "🖥  ${width} × ${height}"
        val chipText2 = "⚡ HEVC / H.264 hardware decoding"
        val chipText3 = "🖊  Pen pressure & tilt"

        landingTipPaint.textSize = 11f * density
        landingTipPaint.color = Color.rgb(140, 165, 195)
        landingTipPaint.textAlign = Paint.Align.CENTER
        landingTipPaint.typeface = Typeface.create(Typeface.MONOSPACE, Typeface.NORMAL)
        canvas.drawText("$chipText1   ·   $chipText2   ·   $chipText3", centerX, chipY, landingTipPaint)

        landingTipPaint.color = Color.rgb(90, 115, 145)
        val hint = if (isUsbLink) {
            "Connect the USB cable to a Windows PC running XDisplay to start"
        } else {
            "Make sure XDisplay is running on the PC and this device is on the same Wi-Fi"
        }
        canvas.drawText(hint, centerX, chipY + 22f * density, landingTipPaint)
    }
}

