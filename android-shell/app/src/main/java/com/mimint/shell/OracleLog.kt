package com.mimint.shell

import android.content.Context
import android.os.SystemClock
import java.io.File

/**
 * The conformance-hook oracle log: the shell writes the same
 * line protocol the Rust `FileOracle` parses for the X11
 * testwin, so `conformance --body android` grades the shell
 * with the same suite. Lines:
 *
 *   GEOM x=<i> y=<i> w=<u> h=<u>   (target geometry, from the
 *                                   TestTargetActivity)
 *   READY                        (target is up)
 *   COLOR <name>                 (variant changed; informational)
 *   BUTTON root_x=.. root_y=.. win_x=.. win_y=.. detail=.. t=..
 *   RELEASE root_x=.. root_y=.. detail=.. t=..
 *   MOTION root_x=.. root_y=.. t=..
 *   KEY sym=0x.. tok=<token> t=..
 *   KEYUP sym=0x.. tok=<token> t=..
 *
 * Button details follow the X11 convention the parser expects:
 * 1 = primary, 3 = right, 4 = wheel up, 5 = wheel down,
 * 6 = wheel left, 7 = wheel right. `t=` is a wrapping u32 of
 * milliseconds (here: uptimeMillis), matching X server time
 * semantics for the double-click/hold reconstructions.
 *
 * HONEST LIMIT: the shell logs what it was ASKED to do (the
 * gesture it dispatched), not where the touch physically
 * landed — the same arrangement as MockBody/MockOracle, which
 * share state. The hook verifies the protocol + dispatch, not
 * touch accuracy. Touch accuracy is verified by eye on the
 * device, never claimed from this log.
 */
class OracleLog(context: Context) {
    private val file = File(context.filesDir, "conformance.log")

    /** Wrapping millisecond clock for `t=` fields. */
    fun nowMs(): Long = SystemClock.uptimeMillis() and 0xFFFFFFFFL

    @Synchronized
    fun line(text: String) {
        file.appendText(text + "\n")
    }

    fun click(x: Int, y: Int, geom: Geom) {
        val t = nowMs()
        line("BUTTON root_x=$x root_y=$y win_x=${x - geom.x} win_y=${y - geom.y} detail=1 t=$t")
        line("RELEASE root_x=$x root_y=$y detail=1 t=${t + 1}")
    }

    fun doubleClick(x: Int, y: Int, geom: Geom) {
        val t = nowMs()
        line("BUTTON root_x=$x root_y=$y win_x=${x - geom.x} win_y=${y - geom.y} detail=1 t=$t")
        line("RELEASE root_x=$x root_y=$y detail=1 t=${t + 1}")
        line("BUTTON root_x=$x root_y=$y win_x=${x - geom.x} win_y=${y - geom.y} detail=1 t=${t + 60}")
        line("RELEASE root_x=$x root_y=$y detail=1 t=${t + 61}")
    }

    fun drag(fromX: Int, fromY: Int, toX: Int, toY: Int, geom: Geom) {
        val t = nowMs()
        line("BUTTON root_x=$fromX root_y=$fromY win_x=${fromX - geom.x} win_y=${fromY - geom.y} detail=1 t=$t")
        // Interpolated motion points, like the X11 body's drag.
        for (i in 1..9) {
            val x = fromX + (toX - fromX) * i / 10
            val y = fromY + (toY - fromY) * i / 10
            line("MOTION root_x=$x root_y=$y t=${t + i}")
        }
        line("RELEASE root_x=$toX root_y=$toY detail=1 t=${t + 10}")
    }

    fun scroll(x: Int, y: Int, dx: Int, dy: Int, geom: Geom) {
        val t = nowMs()
        var tick = t
        // dy > 0 scrolls down (wheel detail 5); dy < 0 up (4).
        repeat(kotlin.math.abs(dy)) {
            val detail = if (dy > 0) 5 else 4
            line("BUTTON root_x=$x root_y=$y win_x=${x - geom.x} win_y=${y - geom.y} detail=$detail t=$tick")
            line("RELEASE root_x=$x root_y=$y detail=$detail t=$tick")
            tick += 1
        }
        // dx > 0 scrolls right (wheel detail 7); dx < 0 left (6).
        repeat(kotlin.math.abs(dx)) {
            val detail = if (dx > 0) 7 else 6
            line("BUTTON root_x=$x root_y=$y win_x=${x - geom.x} win_y=${y - geom.y} detail=$detail t=$tick")
            line("RELEASE root_x=$x root_y=$y detail=$detail t=$tick")
            tick += 1
        }
    }

    /** One KEY line per typed character, in the parser's token form. */
    fun typed(text: String) {
        val t = nowMs()
        var tick = t
        for (ch in text) {
            val tok = when (ch) {
                '\n' -> "<Enter>"
                '\t' -> "<Tab>"
                ' ' -> "<Space>"
                else -> if (ch.code in 0x20..0x7E) ch.toString()
                else "<U+${ch.code.toString(16).uppercase()}>"
            }
            line("KEY sym=0x0 tok=$tok t=$tick")
            tick += 1
        }
    }

    data class Geom(val x: Int, val y: Int, val w: Int, val h: Int)
}
