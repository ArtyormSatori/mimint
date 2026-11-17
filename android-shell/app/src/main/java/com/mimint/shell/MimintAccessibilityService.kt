package com.mimint.shell

import android.accessibilityservice.AccessibilityService
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import android.view.accessibility.AccessibilityEvent

/**
 * The accessibility service: the only component that can
 * inject gestures and read window content. It owns the
 * [ProtocolServer], the [ScreenCapture], and the
 * [InputInjector], and runs as a foreground service so the
 * user always sees it running — with a Stop action that kills
 * the whole session.
 *
 * Lifecycle: the user enables the service in Settings >
 * Accessibility. [onServiceConnected] starts the protocol
 * server. [onDestroy] / [kill] stops everything. The service
 * never restarts itself: if it dies, the session is over
 * until the user starts it again.
 *
 * The MediaProjection permission result arrives from
 * MainActivity via [pendingCaptureResult], set before the
 * service is started. If capture was never granted, the
 * screenshot method answers `permission_denied`.
 */
class MimintAccessibilityService : AccessibilityService() {

    companion object {
        const val ACTION_KILL = "com.mimint.shell.KILL"
        const val EXTRA_PORT = "port"
        const val EXTRA_TLS = "tls"
        const val EXTRA_RESULT_CODE = "capture_result_code"

        /** Set by MainActivity before starting the service. */
        @Volatile var pendingCaptureResult: Intent? = null
        @Volatile var pendingCaptureResultCode: Int = 0

        @Volatile var instance: MimintAccessibilityService? = null
    }

    private lateinit var capture: ScreenCapture
    private lateinit var injector: InputInjector
    private var server: ProtocolServer? = null

    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        capture = ScreenCapture(this)
        injector = InputInjector(this)
        startForegroundWithKillAction()
        // The port/TLS settings come from the app's
        // preferences, written by MainActivity.
        val prefs = getSharedPreferences("mimint-shell", MODE_PRIVATE)
        val port = prefs.getInt("port", 18711)
        val tls = prefs.getBoolean("tls", true)
        val server = ProtocolServer(
            context = this,
            port = port,
            tlsEnabled = tls,
            capture = capture,
            injector = injector,
            focusedWindow = { focusedWindow() },
            onKill = { kill() },
        )
        this.server = server
        // Capture permission, if MainActivity obtained it.
        val captureIntent = pendingCaptureResult
        if (captureIntent != null) {
            try {
                capture.start(pendingCaptureResultCode, captureIntent)
            } catch (_: Exception) {
                // Capture stays ungranted; screenshots will
                // answer permission_denied. The server still
                // runs: input and the kill switch work.
            }
            pendingCaptureResult = null
        }
        server.start()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent) {
        // The shell does not react to accessibility events.
        // Gestures and content reads are driven only by
        // protocol requests, never by observed events.
    }

    override fun onInterrupt() {
        // The system is interrupting the service: stop serving.
        kill()
    }

    override fun onDestroy() {
        server?.stop()
        server = null
        capture.stop()
        instance = null
        super.onDestroy()
    }

    /** The kill switch: stop the server, the capture, and the service. */
    fun kill() {
        server?.stop()
        server = null
        try { capture.stop() } catch (_: Exception) {}
        stopForeground(STOP_FOREGROUND_REMOVE)
        disableSelf()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_KILL) {
            kill()
            return START_NOT_STICKY
        }
        return START_NOT_STICKY
    }

    private fun startForegroundWithKillAction() {
        val channelId = "mimint-shell"
        val manager = getSystemService(NotificationManager::class.java)
        val channel = NotificationChannel(
            channelId,
            "Mimint Shell",
            NotificationManager.IMPORTANCE_LOW,
        ).apply { description = "Mimint Shell is serving the paired controller." }
        manager.createNotificationChannel(channel)
        val killIntent = Intent(this, MimintAccessibilityService::class.java)
            .setAction(ACTION_KILL)
        val killPending = PendingIntent.getService(
            this, 0, killIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val openIntent = Intent(this, MainActivity::class.java)
        val openPending = PendingIntent.getActivity(
            this, 0, openIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, channelId)
            .setContentTitle("Mimint Shell is running")
            .setContentText("The paired controller can view the screen and send input.")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentIntent(openPending)
            .addAction(
                Notification.Action.Builder(
                    android.R.drawable.ic_delete,
                    "Stop",
                    killPending,
                ).build(),
            )
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                1, notification,
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION,
            )
        } else {
            startForeground(1, notification)
        }
    }

    /**
     * The focused window for the `focused_window` protocol
     * method. The id is FNV-1a 64-bit over
     * `package + "\0" + class + "\0" + title`, stable for the
     * same window across calls (the contract the Rust client
     * relies on for focus verification).
     */
    private fun focusedWindow(): ProtocolServer.FocusedWindow? {
        val windows = try { windows } catch (_: Exception) { return null }
        val focused = windows.firstOrNull { it.isFocused } ?: return null
        val root = focused.root ?: return null
        val pkg = root.packageName?.toString() ?: ""
        val clazz = root.className?.toString() ?: ""
        val title = try { focused.title?.toString() } catch (_: Exception) { null } ?: ""
        val id = fnv1a64("$pkg\u0000$clazz\u0000$title")
        return ProtocolServer.FocusedWindow(id, clazz, title)
    }

    private fun fnv1a64(text: String): Long {
        var hash = -0x340D631B7BDDDCDBL // FNV offset basis
        for (byte in text.toByteArray(Charsets.UTF_8)) {
            hash = hash xor (byte.toLong() and 0xFF)
            hash *= 0x100000001B3L // FNV prime
        }
        return hash
    }
}
