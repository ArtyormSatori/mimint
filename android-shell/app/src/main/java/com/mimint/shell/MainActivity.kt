package com.mimint.shell

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.provider.Settings
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

/**
 * Pairing + control UI. Everything here is built in code (no
 * layout XML): the UI is four facts and four buttons, and a
 * layout file would only add a place to drift.
 *
 * Pairing flow:
 * 1. The user enables the accessibility service (button opens
 *    Settings > Accessibility).
 * 2. The user grants screen capture (button triggers the
 *    system dialog; the result is handed to the service).
 * 3. The user reads the pairing token + TLS fingerprint here
 *    and enters them on the Rust side (the conformance runner
 *    or the controller). Nothing is ever sent anywhere by
 *    this app: pairing is eyes + keyboard, on purpose.
 * 4. The user runs `adb reverse` (the command is shown) so the
 *    Rust side reaches the loopback server.
 *
 * The Stop button is the kill switch: it stops the service,
 * which stops the server and the capture.
 */
class MainActivity : Activity() {

    private val captureRequestCode = 1001

    private lateinit var statusView: TextView
    private lateinit var tokenView: TextView
    private lateinit var fingerprintView: TextView
    private lateinit var portEdit: EditText
    private lateinit var tlsCheck: CheckBox

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("mimint-shell", MODE_PRIVATE)

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 48, 48, 48)
        }

        statusView = TextView(this)
        tokenView = TextView(this).apply { setTextIsSelectable(true) }
        fingerprintView = TextView(this).apply { setTextIsSelectable(true) }
        portEdit = EditText(this).apply {
            setText(prefs.getInt("port", 18711).toString())
            hint = "Port (loopback only)"
        }
        tlsCheck = CheckBox(this).apply {
            text = "TLS with certificate pinning (recommended; plain mode is debug-only)"
            isChecked = prefs.getBoolean("tls", true)
        }

        val accessibilityButton = Button(this).apply {
            text = "1. Enable the accessibility service"
            setOnClickListener {
                startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
            }
        }
        val captureButton = Button(this).apply {
            text = "2. Grant screen capture"
            setOnClickListener { requestCapture() }
        }
        val saveButton = Button(this).apply {
            text = "3. Save port / TLS settings"
            setOnClickListener {
                val port = portEdit.text.toString().toIntOrNull() ?: 18711
                prefs.edit()
                    .putInt("port", port)
                    .putBoolean("tls", tlsCheck.isChecked)
                    .apply()
                Toast.makeText(this@MainActivity, "Saved. Restart the service to apply.", Toast.LENGTH_LONG).show()
            }
        }
        val targetButton = Button(this).apply {
            text = "Open the conformance target"
            setOnClickListener {
                startActivity(Intent(this@MainActivity, TestTargetActivity::class.java))
            }
        }
        val stopButton = Button(this).apply {
            text = "STOP — kill the server and the service"
            setOnClickListener {
                MimintAccessibilityService.instance?.kill()
                refreshStatus()
                Toast.makeText(this@MainActivity, "Stopped.", Toast.LENGTH_LONG).show()
            }
        }
        val adbHint = TextView(this).apply {
            text = "On the computer, forward the port:\n" +
                "  adb reverse tcp:18711 tcp:18711\n" +
                "Then run the conformance runner against 127.0.0.1:18711."
            setTextIsSelectable(true)
        }

        listOf(
            statusView, tokenView, fingerprintView, portEdit, tlsCheck,
            accessibilityButton, captureButton, saveButton, targetButton,
            stopButton, adbHint,
        ).forEach { layout.addView(it) }
        setContentView(ScrollView(this).apply { addView(layout) })
        refreshStatus()
    }

    override fun onResume() {
        super.onResume()
        refreshStatus()
    }

    private fun refreshStatus() {
        val running = MimintAccessibilityService.instance != null
        statusView.text = if (running) {
            "Service: RUNNING — the paired controller can connect."
        } else {
            "Service: STOPPED — enable it in Accessibility settings."
        }
        tokenView.text = "Pairing token:\n" + IdentityStore.pairingToken(this)
        fingerprintView.text = try {
            "TLS fingerprint (SHA-256):\n" + IdentityStore.fingerprintDisplay()
        } catch (e: Exception) {
            "TLS fingerprint: unavailable (${e.message})"
        }
    }

    private fun requestCapture() {
        val manager = getSystemService(MediaProjectionManager::class.java)
        startActivityForResult(manager.createScreenCaptureIntent(), captureRequestCode)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == captureRequestCode && resultCode == RESULT_OK && data != null) {
            MimintAccessibilityService.pendingCaptureResult = data
            MimintAccessibilityService.pendingCaptureResultCode = resultCode
            Toast.makeText(
                this,
                "Capture granted. (Re)start the accessibility service to apply it.",
                Toast.LENGTH_LONG,
            ).show()
        }
    }
}
