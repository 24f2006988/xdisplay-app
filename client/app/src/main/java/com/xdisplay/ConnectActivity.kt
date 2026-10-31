package com.xdisplay

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.IntentFilter
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.Build
import android.app.AlertDialog
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.net.wifi.WifiManager
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
 * Connection picker: reconnect to the last PC, use the USB cable, pick a PC found on the Wi-Fi,
 * or type an address. PCs are discovered by the native layer; this screen only renders them.
 */
class ConnectActivity : Activity() {
    private lateinit var store: ConnectionStore
    private lateinit var messageView: TextView
    private lateinit var hostList: LinearLayout
    private lateinit var lastButtonHolder: LinearLayout
    private lateinit var addressInput: EditText

    private val handler = Handler(Looper.getMainLooper())
    private var multicastLock: WifiManager.MulticastLock? = null
    // null until the first render, so an empty list still draws the "searching" hint.
    private var shownHosts: String? = null
    private var pinDialog: AlertDialog? = null
    private var usbAttachedReceiver: BroadcastReceiver? = null

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
            renderHosts(NativeEngine.getDiscoveredHosts())
            handler.postDelayed(this, REFRESH_MS)
        }
    }

    private data class Host(val ip: String, val port: Int, val name: String, val hostId: String)

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
        // Some devices drop broadcast datagrams unless a multicast lock is held.
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        multicastLock = wifi?.createMulticastLock("xdisplay-discovery")?.apply {
            setReferenceCounted(false)
            acquire()
        }
        NativeEngine.startDiscovery()
        if (checkAndLaunchAoa()) return
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
        handler.post(refreshRunnable)
    }

    override fun onPause() {
        super.onPause()
        handler.removeCallbacks(refreshRunnable)
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
        val message = intent.getStringExtra(EXTRA_MESSAGE)
        if (message != null) showMessage(message, error = intent.getBooleanExtra(EXTRA_ERROR, false))
        if (intent.getBooleanExtra(EXTRA_NEED_PIN, false)) {
            val host = intent.getStringExtra(EXTRA_HOST) ?: return
            val target = Target(host, intent.getIntExtra(EXTRA_PORT, Target.DEFAULT_PORT), intent.getStringExtra(EXTRA_NAME) ?: host)
            handler.post { askPin(target, intent.getStringExtra(EXTRA_HOST_ID)) }
        }
        intent.removeExtra(EXTRA_NEED_PIN)
        intent.removeExtra(EXTRA_MESSAGE)
    }

    // ---------------------------------------------------------------------------------------
    // Connecting
    // ---------------------------------------------------------------------------------------

    private fun connect(target: Target, hostId: String?) {
        val unpaired = !target.isUsb && hostId != null && !store.hasCredentialFor(hostId)
        if (unpaired) askPin(target, hostId) else launch(target, hostId, pin = "")
    }

    private fun launch(target: Target, hostId: String?, pin: String) {
        store.lastTarget = target
        store.lastHostId = hostId
        startActivity(
            Intent(this, MainActivity::class.java)
                .putExtra(MainActivity.EXTRA_HOST, target.host)
                .putExtra(MainActivity.EXTRA_PORT, target.port)
                .putExtra(MainActivity.EXTRA_NAME, target.name)
                .putExtra(MainActivity.EXTRA_PIN, pin)
        )
    }

    private fun askPin(target: Target, hostId: String?) {
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
            .setMessage("Open XDisplay on the PC and enter the PIN shown on its dashboard. You only need to do this once.")
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
        lastButtonHolder.addView(
            card("Reconnect to ${last.name}", if (last.isUsb) "USB cable" else last.host, accent = true) {
                connect(last, store.lastHostId)
            }
        )
    }

    private fun parseHosts(snapshot: String): List<Host> = snapshot.lineSequence().mapNotNull { line ->
        val parts = line.split('|')
        if (parts.size != 4) return@mapNotNull null
        Host(parts[0], parts[1].toIntOrNull() ?: return@mapNotNull null, parts[2], parts[3])
    }.toList()

    @Suppress("SetTextI18n")
    private fun renderHosts(snapshot: String) {
        if (snapshot == shownHosts) return
        shownHosts = snapshot
        hostList.removeAllViews()
        val hosts = parseHosts(snapshot)
        if (hosts.isEmpty()) {
            hostList.addView(
                TextView(this).apply {
                    text = "Searching for PCs on this Wi-Fi…\nMake sure XDisplay is running on the PC and both are on the same network."
                    setTextColor(Color.rgb(110, 135, 165))
                    textSize = 14f
                    setPadding(dp(4), dp(8), dp(4), dp(8))
                }
            )
            return
        }
        for (host in hosts) {
            val paired = store.hasCredentialFor(host.hostId)
            val detail = "${host.ip}  ·  " + if (paired) "paired" else "PIN required"
            hostList.addView(card(host.name, detail, accent = false) {
                connect(Target(host.ip, host.port, host.name), host.hostId)
            })
        }
    }

    private fun buildLayout(): View {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(28), dp(24), dp(28))
        }

        root.addView(label("XDisplay", 30f, Color.WHITE, bold = true))
        root.addView(label("Use this phone or tablet as a second screen and pen display", 15f, Color.rgb(140, 165, 195)))

        messageView = label("", 14f, Color.rgb(255, 120, 120)).apply {
            visibility = View.GONE
            setPadding(0, dp(14), 0, 0)
        }
        root.addView(messageView)

        lastButtonHolder = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(lastButtonHolder, spaced())

        root.addView(section("USB"))
        root.addView(card("USB cable", "Fastest and needs no network. Plug in and run XDisplay on the PC.", accent = false) {
            connect(Target.USB, hostId = null)
        }, spaced())

        root.addView(section("PCs on this Wi-Fi"))
        hostList = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(hostList, spaced())

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
            addView(label(detail, 13f, Color.rgb(140, 165, 195)))
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

        private const val REFRESH_MS = 1000L
        private const val PIN_LENGTH = 6
        private val BG = Color.rgb(14, 18, 27)
        private val CARD = Color.rgb(22, 28, 42)
        private val CARD_BORDER = Color.rgb(38, 51, 74)
        private val ACCENT = Color.rgb(0, 240, 255)
    }
}
