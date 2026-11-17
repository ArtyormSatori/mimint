package com.mimint.shell

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Handler
import android.os.HandlerThread
import android.util.DisplayMetrics
import android.view.WindowManager

/**
 * Screen capture via MediaProjection. The user grants capture
 * once, in MainActivity (the system permission dialog); the
 * result is handed to the service, which holds the
 * MediaProjection for the session. Frames arrive on a
 * background thread into an ImageReader; [latestFrame] copies
 * the newest frame into a fresh RGBA byte array.
 *
 * Coordinates: frames are in DISPLAY pixels (widthPixels x
 * heightPixels). The protocol's framebuffer-pixel contract
 * holds: the shell reports the frame's own dimensions, and
 * input coordinates are in the same space. `scale_factor` is
 * the display density (informational: the frame is already in
 * physical pixels).
 *
 * If capture was never granted, [latestFrame] returns null
 * and the protocol layer answers `permission_denied` — never
 * a blank image.
 */
class ScreenCapture(private val context: Context) {
    private val metrics = DisplayMetrics()
    private var projection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var thread: HandlerThread? = null

    /** Display size in pixels. */
    val size: Pair<Int, Int>
        get() {
            val wm = context.getSystemService(WindowManager::class.java)
            wm.defaultDisplay.getRealMetrics(metrics)
            return metrics.widthPixels to metrics.heightPixels
        }

    /** Display density (the frame's scale_factor). */
    val density: Float
        get() {
            val wm = context.getSystemService(WindowManager::class.java)
            wm.defaultDisplay.getRealMetrics(metrics)
            return metrics.density
        }

    val granted: Boolean get() = projection != null

    /**
     * Start capture with the MediaProjection permission result
     * from MainActivity. Must be called at most once per
     * session; call [stop] first to re-grant.
     */
    fun start(resultCode: Int, data: Intent) {
        if (resultCode != Activity.RESULT_OK) {
            throw IllegalStateException("screen capture was not granted")
        }
        stop()
        val manager = context.getSystemService(MediaProjectionManager::class.java)
        val (w, h) = size
        val handlerThread = HandlerThread("mimint-capture").also { it.start() }
        thread = handlerThread
        val imageReader = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
        reader = imageReader
        val proj = manager.getMediaProjection(resultCode, data)
        projection = proj
        virtualDisplay = proj.createVirtualDisplay(
            "mimint",
            w, h, metrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            imageReader.surface, null, Handler(handlerThread.looper),
        )
    }

    fun stop() {
        try { virtualDisplay?.release() } catch (_: Exception) {}
        try { projection?.stop() } catch (_: Exception) {}
        try { reader?.close() } catch (_: Exception) {}
        thread?.quitSafely()
        virtualDisplay = null
        projection = null
        reader = null
        thread = null
    }

    /**
     * The newest frame as RGBA bytes, or null when capture is
     * not running or no frame has arrived yet. The caller owns
     * the returned array; it is never reused.
     */
    fun latestFrame(): ByteArray? {
        val imageReader = reader ?: return null
        val image = try {
            imageReader.acquireLatestImage()
        } catch (_: Exception) {
            null
        } ?: return null
        return try {
            val (w, h) = size
            val plane = image.planes[0]
            val buffer = plane.buffer
            val rowStride = plane.rowStride
            val pixelStride = plane.pixelStride
            val out = ByteArray(w * h * 4)
            if (pixelStride == 4 && rowStride == w * 4) {
                buffer.get(out)
            } else {
                // Copy row by row, honoring stride.
                val row = ByteArray(rowStride)
                for (y in 0 until h) {
                    buffer.position(y * rowStride)
                    buffer.get(row, 0, rowStride)
                    for (x in 0 until w) {
                        val src = x * pixelStride
                        val dst = (y * w + x) * 4
                        out[dst] = row[src]
                        out[dst + 1] = row[src + 1]
                        out[dst + 2] = row[src + 2]
                        out[dst + 3] = row[src + 3]
                    }
                }
            }
            out
        } finally {
            image.close()
        }
    }
}
