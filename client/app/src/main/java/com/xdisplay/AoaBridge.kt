package com.xdisplay

import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.ParcelFileDescriptor
import android.os.Process
import android.util.Log
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.atomic.AtomicBoolean

/**
 * USB link without USB debugging: bridges the PC's Android Open Accessory (AOA 2.0) bulk
 * endpoints to a loopback TCP port, so the native engine connects to 127.0.0.1 exactly as it
 * does over adb.
 *
 * The accessory stays open for as long as the cable is plugged in. The native client may
 * reconnect any number of times (dead link, app resumed); each connection is accepted in turn
 * and becomes the one the USB stream is delivered to. The PC side opens a fresh session when
 * it sees the new client's hello.
 */
class AoaBridge(
    usbManager: UsbManager,
    val accessory: UsbAccessory
) : AutoCloseable {

    companion object {
        private const val TAG = "XDisplayAoaBridge"
        // Accessory reads shorter than the gadget's 16 KB request size lose data on some kernels.
        private const val BUFFER_SIZE = 32768
    }

    private val isRunning = AtomicBoolean(true)
    private val pfd: ParcelFileDescriptor
    private val serverSocket = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
    private val usbIn: FileInputStream
    private val usbOut: FileOutputStream
    private val usbOutLock = Any()

    /** The native client connection currently receiving the USB stream. */
    @Volatile
    private var client: Socket? = null

    val port: Int = serverSocket.localPort

    init {
        pfd = try {
            usbManager.openAccessory(accessory) ?: throw IOException("openAccessory returned null")
        } catch (e: Exception) {
            serverSocket.close()
            throw IOException("Failed to open USB accessory: ${e.message}", e)
        }
        usbIn = FileInputStream(pfd.fileDescriptor)
        usbOut = FileOutputStream(pfd.fileDescriptor)

        daemon("xdisplay-aoa-accept") { acceptLoop() }
        daemon("xdisplay-aoa-rx") { usbToClient() }
        Log.i(TAG, "AOA bridge listening on 127.0.0.1:$port")
    }

    private fun daemon(name: String, body: () -> Unit) {
        Thread(body, name).apply {
            isDaemon = true
            start()
        }
    }

    private fun acceptLoop() {
        while (isRunning.get()) {
            val socket = try {
                serverSocket.accept()
            } catch (e: IOException) {
                break
            }
            socket.tcpNoDelay = true
            client?.let { closeQuietly(it) }
            client = socket
            Log.i(TAG, "Native client connected to the AOA bridge")
            daemon("xdisplay-aoa-tx") { clientToUsb(socket) }
        }
    }

    /** PC -> tablet: video, cursor, config. Runs for the accessory's lifetime. */
    private fun usbToClient() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY)
        val buffer = ByteArray(BUFFER_SIZE)
        try {
            while (isRunning.get()) {
                val n = usbIn.read(buffer)
                if (n < 0) break
                if (n == 0) continue
                val target = client ?: continue // nobody connected yet: stale bytes are dropped
                try {
                    target.getOutputStream().write(buffer, 0, n)
                } catch (e: IOException) {
                    if (client === target) client = null
                    closeQuietly(target)
                }
            }
        } catch (e: IOException) {
            if (isRunning.get()) Log.w(TAG, "USB accessory read ended: ${e.message}")
        } finally {
            close() // cable unplugged or the PC left accessory mode
        }
    }

    /** Tablet -> PC: hello, stylus, touch, keys, pings. One per client connection. */
    private fun clientToUsb(socket: Socket) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY)
        val buffer = ByteArray(BUFFER_SIZE)
        try {
            val input = socket.getInputStream()
            while (isRunning.get()) {
                val n = input.read(buffer)
                if (n < 0) break
                synchronized(usbOutLock) { usbOut.write(buffer, 0, n) }
            }
        } catch (e: IOException) {
            if (isRunning.get() && client === socket) Log.w(TAG, "USB accessory write ended: ${e.message}")
        } finally {
            if (client === socket) client = null
            closeQuietly(socket)
        }
    }

    private fun closeQuietly(c: AutoCloseable) {
        try { c.close() } catch (_: Exception) {}
    }

    override fun close() {
        if (!isRunning.compareAndSet(true, false)) return
        Log.i(TAG, "Closing AOA bridge on port $port")
        client?.let { closeQuietly(it) }
        client = null
        closeQuietly(serverSocket)
        closeQuietly(pfd)
    }
}
