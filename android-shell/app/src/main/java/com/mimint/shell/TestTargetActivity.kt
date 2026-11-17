package com.mimint.shell

import android.app.Activity
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.view.WindowMetrics
import java.io.File
import kotlin.concurrent.thread

/**
 * The conformance target: a full-screen Activity that draws
 * the same content variants the Rust suite grades, and logs
 * the same oracle lines as the X11 testwin:
 *
 * - Variant 0: diagonal stripes (non-uniform, so C1 passes).
 * - Variant 1: solid green. Variant 2: solid blue.
 * - Variant 3: the C9 quadrant pattern in EXACT colors
 *   (red / green / blue / yellow).
 *
 * The variant comes from the control file
 * (`conformance-control.txt` in the app's files dir), polled
 * every 50ms — the same arrangement as testwin: the
 * `FileOracle` truncates and rewrites the file with a single
 * number, and the hook script pushes it to the device.
 *
 * On start the Activity logs `GEOM` (its real window bounds
 * in display pixels) and `READY` to the oracle log, and
 * `COLOR <name>` on every variant change. Input events are
 * logged by the ProtocolServer (it knows what it dispatched),
 * not here.
 *
 * This Activity is a TEST TARGET, not a product surface: it
 * draws patterns, nothing else.
 */
class TestTargetActivity : Activity() {

    private lateinit var targetView: TargetView
    private lateinit var oracle: OracleLog
    private lateinit var controlFile: File
    @Volatile private var polling = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        oracle = OracleLog(this)
        controlFile = File(filesDir, "conformance-control.txt")
        targetView = TargetView(this)
        setContentView(targetView)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || polling) return
        polling = true
        // Real window bounds, from the window metrics: the
        // suite aims and samples from the GEOM line.
        val metrics: WindowMetrics = windowManager.currentWindowMetrics
        val bounds = metrics.bounds
        val geom = OracleLog.Geom(bounds.left, bounds.top, bounds.width(), bounds.height())
        oracle.line("GEOM x=${geom.x} y=${geom.y} w=${geom.w} h=${geom.h}")
        oracle.line("READY")
        startPolling()
    }

    override fun onDestroy() {
        polling = false
        super.onDestroy()
    }

    private fun startPolling() {
        val handler = Handler(Looper.getMainLooper())
        thread(name = "mimint-control-poll", isDaemon = true) {
            var last = -1
            while (polling) {
                val variant = try {
                    controlFile.readText().trim().toIntOrNull()?.rem(4)
                } catch (_: Exception) {
                    null
                }
                if (variant != null && variant != last) {
                    last = variant
                    handler.post {
                        targetView.variant = variant
                        targetView.invalidate()
                    }
                    oracle.line("COLOR ${variantName(variant)}")
                }
                Thread.sleep(50)
            }
        }
    }

    private fun variantName(variant: Int): String = when (variant) {
        0 -> "stripes"
        1 -> "green"
        2 -> "blue"
        3 -> "pattern"
        else -> "unknown"
    }

    private class TargetView(context: Context) : View(context) {
        var variant: Int = 0
        private val paint = Paint()

        override fun onDraw(canvas: Canvas) {
            super.onDraw(canvas)
            val w = width.toFloat()
            val h = height.toFloat()
            when (variant) {
                1 -> canvas.drawColor(Color.GREEN)
                2 -> canvas.drawColor(Color.BLUE)
                3 -> {
                    paint.color = Color.RED
                    canvas.drawRect(0f, 0f, w / 2, h / 2, paint)
                    paint.color = Color.GREEN
                    canvas.drawRect(w / 2, 0f, w, h / 2, paint)
                    paint.color = Color.BLUE
                    canvas.drawRect(0f, h / 2, w / 2, h, paint)
                    paint.color = Color.YELLOW
                    canvas.drawRect(w / 2, h / 2, w, h, paint)
                }
                else -> {
                    // Diagonal stripes: non-uniform, and
                    // visibly different from every other variant.
                    canvas.drawColor(Color.WHITE)
                    paint.color = Color.BLACK
                    paint.strokeWidth = 24f
                    var x = -h
                    while (x < w) {
                        canvas.drawLine(x, h, x + h, 0f, paint)
                        x += 96
                    }
                }
            }
        }
    }
}
