package com.xdisplay

import android.app.Activity
import android.app.AlertDialog
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputFilter
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.view.inputmethod.EditorInfo
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.core.content.ContextCompat

/**
 * Connection picker: USB Tethering (works on every Android device with zero USB debugging),
 * Wi-Fi auto-discovery, direct USB (ADB/AOA), or manual IP address.
 */
class ConnectActivity : Activity() {
    private lateinit var store: ConnectionStore
    private lateinit var messageView: TextView
    private lateinit var usbTetherHolder: LinearLayout
    private lateinit var hostList: LinearLayout
    private lateinit var lastButtonHolder: LinearLayout
    private lateinit var addressInput: EditText

    private val handler = Handler(Looper.getMainLooper())
    private var multicastLock: WifiManager.MulticastLock? = null
    // null until the first render, so an empty list still draws the initial hint.
    private var shownRenderKey: String? = null
    private var lastIfaceSpec: String? = null
    private var pinDialog: AlertDialog? = null
    private var usbAttachedReceiver: BroadcastReceiver? = null
    /** Set when the user explicitly backed out of a session so auto-connect does not yank them back in. */
    private var suppressAutoConnect = false
    private var autoConnectTriggered = false

    private fun checkAndLaunchAoa(intentToCheck: Intent? = null): Boolean {
        val usbManager = getSystemService(Context.USB_SERVICE) as? UsbManager ?: return false
        val accessory: UsbAccessory? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intentToCheck?.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
        } else {
            @Suppress("DEPRECATION")
            intentToCheck?.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
        } ?: usbManager.accessoryList?.firstOrNull()

        if (accessory != null) {
            // Android grants the permission itself only when it launched us from its "Open
            // XDisplay?" prompt; opened any other way, ask once per accessory.
            if (!usbManager.hasPermission(accessory)) {
                if (permissionAskedFor != accessory) {
                    permissionAskedFor = accessory
                    usbManager.requestPermission(accessory, permissionIntent())
                }
                return false
            }
            startActivity(
                Intent(this, MainActivity::class.java)
                    .setAction(UsbManager.ACTION_USB_ACCESSORY_ATTACHED)
                    .putExtra(UsbManager.EXTRA_ACCESSORY, accessory)
            )
            finish()
            return true
        }
        return false
    }

    private var permissionAskedFor: UsbAccessory? = null

    private fun permissionIntent(): PendingIntent {
        // Mutable: the system adds EXTRA_ACCESSORY and EXTRA_PERMISSION_GRANTED to it.
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
        return PendingIntent.getBroadcast(this, 0, Intent(ACTION_USB_PERMISSION).setPackage(packageName), flags)
    }

    private val refreshRunnable = object : Runnable {
        override fun run() {
            val ifaces = NetworkUtils.activeInterfaces()
            val spec = NetworkUtils.toNativeSpec(ifaces)
            if (spec != lastIfaceSpec) {
                lastIfaceSpec = spec
                NativeEngine.setDiscoveryInterfaces(spec)
            }
            val tetherActive = NetworkUtils.isUsbTetheringActive(ifaces)
            renderDiscovered(NativeEngine.getDiscoveredHosts(), tetherActive)
            handler.postDelayed(this, REFRESH_MS)
        }
    }

    private data class Host(
        val ip: String,
        val port: Int,
        val name: String,
        val hostId: String,
        val isUsbTether: Boolean
    )

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = ConnectionStore(this)
        window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
        window.statusBarColor = BG
        window.navigationBarColor = BG
        NativeEngine.initNativeLogging()
        setContentView(buildLayout())
        if (checkAndLaunchAoa(intent)) return
        handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (checkAndLaunchAoa(intent)) return
        handleIntent(intent)
    }

    override fun onResume() {
        super.onResume()
        autoConnectTriggered = false
        if (checkAndLaunchAoa()) return
        // Some devices drop broadcast datagrams unless a multicast lock is held.
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        multicastLock = wifi?.createMulticastLock("xdisplay-discovery")?.apply {
            setReferenceCounted(false)
            acquire()
        }
        val ifaces = NetworkUtils.activeInterfaces()
        val spec = NetworkUtils.toNativeSpec(ifaces)
        lastIfaceSpec = spec
        NativeEngine.setDiscoveryInterfaces(spec)
        NativeEngine.startDiscovery()
        val usbFilter = IntentFilter(ACTION_USB_PERMISSION)
        usbAttachedReceiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                    checkAndLaunchAoa(intent)
                }
            }
        }
        // Only our own PendingIntent sends this action, so the receiver is not exported.
        ContextCompat.registerReceiver(this, usbAttachedReceiver, usbFilter, ContextCompat.RECEIVER_NOT_EXPORTED)
        refreshLastTarget()
        shownRenderKey = null
        handler.post(refreshRunnable)
    }

    override fun onPause() {
        super.onPause()
        handler.removeCallbacksAndMessages(null)
        NativeEngine.stopDiscovery()
        multicastLock?.takeIf { it.isHeld }?.release()
        multicastLock = null
        pinDialog?.dismiss()
        usbAttachedReceiver?.let {
            try { unregisterReceiver(it) } catch (_: Exception) {}
            usbAttachedReceiver = null
        }
    }

    /** Shows why we are back here and, when the PC asked for a PIN, prompts for it. */
    private fun handleIntent(intent: Intent) {
        // Only MainActivity may tell us what to show or which host to ask a PIN for; this
        // activity is exported, so anything else could pop a PIN prompt for a fake PC.
        if (!InternalIntents.isInternal(intent)) return
        if (intent.getBooleanExtra(EXTRA_NO_AUTO_CONNECT, false)) {
            suppressAutoConnect = true
            intent.removeExtra(EXTRA_NO_AUTO_CONNECT)
        }
        val message = intent.getStringExtra(EXTRA_MESSAGE)
        if (message != null) {
            suppressAutoConnect = true
            showMessage(message, error = intent.getBooleanExtra(EXTRA_ERROR, false))
        }
        if (intent.getBooleanExtra(EXTRA_NEED_PIN, false)) {
            suppressAutoConnect = true
            val host = intent.getStringExtra(EXTRA_HOST) ?: return
            val target = Target(
                host,
                intent.getIntExtra(EXTRA_PORT, Target.DEFAULT_PORT),
                intent.getStringExtra(EXTRA_NAME) ?: host,
                intent.getBooleanExtra(EXTRA_USB_TETHER, false)
            )
            handler.post { askPin(target, intent.getStringExtra(EXTRA_HOST_ID)) }
        }
        intent.removeExtra(EXTRA_NEED_PIN)
        intent.removeExtra(EXTRA_MESSAGE)
    }

    // ---------------------------------------------------------------------------------------
    // Connecting
    // ---------------------------------------------------------------------------------------

    private fun connect(target: Target, hostId: String?) {
        suppressAutoConnect = false
        val unpaired = !target.isUsb && hostId != null && !store.hasCredentialFor(hostId)
        if (unpaired) askPin(target, hostId) else launch(target, hostId, pin = "")
    }

    private fun launch(target: Target, hostId: String?, pin: String) {
        store.lastTarget = target
        store.lastHostId = hostId
        startActivity(
            InternalIntents.mark(Intent(this, MainActivity::class.java))
                .putExtra(MainActivity.EXTRA_HOST, target.host)
                .putExtra(MainActivity.EXTRA_PORT, target.port)
                .putExtra(MainActivity.EXTRA_NAME, target.name)
                .putExtra(MainActivity.EXTRA_PIN, pin)
                .putExtra(MainActivity.EXTRA_USB_TETHER, target.isUsbTether)
                .putExtra(MainActivity.EXTRA_HOST_ID, hostId)
        )
    }

    private fun askPin(target: Target, hostId: String?) {
        if (isFinishing || isDestroyed) return
        pinDialog?.dismiss()
        val input = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD
            filters = arrayOf(InputFilter.LengthFilter(PIN_LENGTH))
            hint = "6-digit PIN"
            textSize = 28f
            gravity = Gravity.CENTER
            typeface = Typeface.MONOSPACE
            imeOptions = EditorInfo.IME_ACTION_DONE
        }
        val pad = dp(24)
        val holder = LinearLayout(this).apply {
            setPadding(pad, dp(8), pad, 0)
            addView(input, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        }
        pinDialog = AlertDialog.Builder(this)
            .setTitle("Pair with ${target.name}")
            .setMessage("Open XDisplay on the PC and enter the 6-digit PIN shown on its dashboard. You only need to do this once.")
            .setView(holder)
            .setPositiveButton("Pair") { _, _ -> launch(target, hostId, input.text.toString()) }
            .setNegativeButton("Cancel", null)
            .create()
            .also { dialog ->
                dialog.window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
                dialog.show()
                val pair = dialog.getButton(AlertDialog.BUTTON_POSITIVE)
                pair.isEnabled = false
                input.addTextChangedListener(object : android.text.TextWatcher {
                    override fun afterTextChanged(s: android.text.Editable?) {
                        pair.isEnabled = (s?.length ?: 0) == PIN_LENGTH
                    }
                    override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
                    override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
                })
                input.requestFocus()
            }
    }

    private fun connectToTypedAddress() {
        val text = addressInput.text.toString().trim()
        val host = text.substringBefore(':')
        val port = if (text.contains(':')) text.substringAfter(':').toIntOrNull() else Target.DEFAULT_PORT
        val octets = host.split('.')
        val validHost = octets.size == 4 && octets.all { part -> part.toIntOrNull()?.let { it in 0..255 } == true }
        if (!validHost || port == null || port !in 1..65535) {
            showMessage("Enter the PC's IPv4 address, for example 192.168.1.20", error = true)
            return
        }
        showMessage("", error = false)
        connect(Target(host, port, host), hostId = null)
    }

    // ---------------------------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------------------------

    private fun showMessage(text: String, error: Boolean) {
        messageView.text = text
        messageView.setTextColor(if (error) Color.rgb(255, 120, 120) else Color.rgb(140, 165, 195))
        messageView.visibility = if (text.isEmpty()) View.GONE else View.VISIBLE
    }

    private fun refreshLastTarget() {
        lastButtonHolder.removeAllViews()
        val last = store.lastTarget ?: return
        val subtitle = when {
            last.isUsb -> "Direct USB (ADB / Accessory)"
            last.isUsbTether -> "USB Tethering · ${last.host}"
            else -> last.host
        }
        lastButtonHolder.addView(
            card("Reconnect to ${last.name}", subtitle, accent = false) {
                connect(last, store.lastHostId)
            }
        )
    }

    private fun parseHosts(snapshot: String): List<Host> = snapshot.lineSequence().mapNotNull { line ->
        val parts = line.split('|')
        if (parts.size < 4) return@mapNotNull null
        val port = parts[1].toIntOrNull() ?: return@mapNotNull null
        val isUsbTether = parts.getOrNull(4) == "usb"
        Host(parts[0], port, parts[2], parts[3], isUsbTether)
    }.toList()

    @Suppress("SetTextI18n")
    private fun renderDiscovered(snapshot: String, tetherActive: Boolean) {
        val hosts = parseHosts(snapshot)
        val usbHosts = hosts.filter { it.isUsbTether }
        val wifiHosts = hosts.filter { !it.isUsbTether }

        // If a paired PC is discovered over USB Tethering, auto-connect immediately for zero-tap setup.
        if (!suppressAutoConnect && !autoConnectTriggered && pinDialog?.isShowing != true) {
            val pairedUsb = usbHosts.firstOrNull { store.hasCredentialFor(it.hostId) }
            if (pairedUsb != null) {
                autoConnectTriggered = true
                launch(Target(pairedUsb.ip, pairedUsb.port, pairedUsb.name, isUsbTether = true), pairedUsb.hostId, pin = "")
                return
            }
        }

        val renderKey = "$tetherActive|$snapshot"
        if (renderKey == shownRenderKey) return
        shownRenderKey = renderKey

        // 1. USB Tethering hero card(s)
        usbTetherHolder.removeAllViews()
        if (usbHosts.isNotEmpty()) {
            for (host in usbHosts) {
                val paired = store.hasCredentialFor(host.hostId)
                val detail = "${host.ip}  ·  " + if (paired) "USB Tethering · paired (tap to connect)" else "USB Tethering · enter 6-digit PIN once"
                usbTetherHolder.addView(card("⚡ ${host.name}", detail, accent = true) {
                    connect(Target(host.ip, host.port, host.name, isUsbTether = true), host.hostId)
                })
            }
        } else if (tetherActive) {
            usbTetherHolder.addView(
                card(
                    "USB Tethering active — finding your PC…",
                    "Make sure XDisplay is running on your Windows PC. Tap here to open Tethering settings.",
                    accent = true
                ) {
                    NetworkUtils.openUsbTetheringSettings(this)
                }
            )
        } else {
            usbTetherHolder.addView(
                card(
                    "Turn on USB Tethering",
                    "Plug in a USB cable, tap here, and toggle USB Tethering ON. Works on every Android device — no USB debugging needed.",
                    accent = true
                ) {
                    NetworkUtils.openUsbTetheringSettings(this)
                }
            )
        }

        // 2. Wi-Fi discovered PCs
        hostList.removeAllViews()
        if (wifiHosts.isEmpty()) {
            hostList.addView(
                TextView(this).apply {
                    text = "Searching for PCs on this Wi-Fi…\nMake sure XDisplay is running on the PC and both are on the same network."
                    setTextColor(Color.rgb(110, 135, 165))
                    textSize = 14f
                    setPadding(dp(4), dp(8), dp(4), dp(8))
                }
            )
        } else {
            for (host in wifiHosts) {
                val paired = store.hasCredentialFor(host.hostId)
                val detail = "${host.ip}  ·  " + if (paired) "paired" else "PIN required"
                hostList.addView(card(host.name, detail, accent = false) {
                    connect(Target(host.ip, host.port, host.name, isUsbTether = false), host.hostId)
                })
            }
        }
    }

    private fun buildLayout(): View {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(28), dp(24), dp(28))
        }

        root.addView(label("XDisplay", 30f, Color.WHITE, bold = true))
        root.addView(label("Use this phone or tablet as a low-latency second screen and pen display", 15f, Color.rgb(140, 165, 195)))

        messageView = label("", 14f, Color.rgb(255, 120, 120)).apply {
            visibility = View.GONE
            setPadding(0, dp(14), 0, 0)
        }
        root.addView(messageView)

        root.addView(section("USB Tethering (Recommended)"))
        usbTetherHolder = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(usbTetherHolder, spaced())

        lastButtonHolder = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(lastButtonHolder, spaced(top = 4))

        root.addView(section("PCs on Wi-Fi"))
        hostList = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(hostList, spaced())

        root.addView(section("Other USB modes"))
        root.addView(
            card("Direct USB (ADB / Accessory)", "For devices with USB debugging enabled or legacy USB setup.", accent = false) {
                connect(Target.USB, hostId = null)
            },
            spaced()
        )

        root.addView(section("Enter an address"))
        addressInput = EditText(this).apply {
            hint = "192.168.1.20"
            setHintTextColor(Color.rgb(90, 110, 135))
            setTextColor(Color.WHITE)
            textSize = 17f
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            imeOptions = EditorInfo.IME_ACTION_GO
            setSingleLine()
            background = rounded(CARD, dp(12), stroke = CARD_BORDER)
            setPadding(dp(16), dp(14), dp(16), dp(14))
            setOnEditorActionListener { _, actionId, _ ->
                if (actionId == EditorInfo.IME_ACTION_GO) connectToTypedAddress()
                true
            }
        }
        root.addView(addressInput, spaced())
        root.addView(
            Button(this).apply {
                text = getString(R.string.connect)
                isAllCaps = false
                setTextColor(Color.BLACK)
                textSize = 16f
                background = rounded(ACCENT, dp(12))
                setOnClickListener { connectToTypedAddress() }
            },
            spaced(top = 10)
        )

        return ScrollView(this).apply {
            setBackgroundColor(BG)
            isFillViewport = true
            addView(root)
        }
    }

    private fun card(title: String, detail: String, accent: Boolean, onClick: () -> Unit): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = rounded(if (accent) Color.rgb(0, 70, 95) else CARD, dp(14), stroke = if (accent) ACCENT else CARD_BORDER)
            setPadding(dp(18), dp(14), dp(18), dp(14))
            isClickable = true
            isFocusable = true
            setOnClickListener { onClick() }
            addView(label(title, 18f, Color.WHITE, bold = true))
            addView(label(detail, 13f, Color.rgb(165, 190, 215)))
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
                .apply { bottomMargin = dp(10) }
        }

    private fun section(text: String): TextView =
        label(text.uppercase(), 12f, ACCENT, bold = true).apply { setPadding(0, dp(22), 0, dp(6)) }

    private fun label(text: String, size: Float, color: Int, bold: Boolean = false): TextView =
        TextView(this).apply {
            this.text = text
            textSize = size
            setTextColor(color)
            if (bold) typeface = Typeface.DEFAULT_BOLD
        }

    private fun spaced(top: Int = 0) =
        LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply { topMargin = dp(top) }

    private fun rounded(fill: Int, radius: Int, stroke: Int? = null) = GradientDrawable().apply {
        setColor(fill)
        cornerRadius = radius.toFloat()
        if (stroke != null) setStroke(dp(1), stroke)
    }

    private fun dp(value: Int): Int = (value * resources.displayMetrics.density).toInt()

    companion object {
        private const val ACTION_USB_PERMISSION = "com.xdisplay.USB_ACCESSORY_PERMISSION"
        const val EXTRA_MESSAGE = "message"
        const val EXTRA_ERROR = "error"
        const val EXTRA_NEED_PIN = "need_pin"
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        const val EXTRA_NAME = "name"
        const val EXTRA_HOST_ID = "host_id"
        const val EXTRA_USB_TETHER = "usb_tether"
        const val EXTRA_NO_AUTO_CONNECT = "no_auto_connect"

        private const val REFRESH_MS = 500L
        private const val PIN_LENGTH = 6
        private val BG = Color.rgb(14, 18, 27)
        private val CARD = Color.rgb(22, 28, 42)
        private val CARD_BORDER = Color.rgb(38, 51, 74)
        private val ACCENT = Color.rgb(0, 240, 255)
    }
}
