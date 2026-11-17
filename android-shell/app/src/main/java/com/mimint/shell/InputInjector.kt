package com.mimint.shell

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.os.Bundle
import android.view.accessibility.AccessibilityNodeInfo
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Input injection through the AccessibilityService. Every
 * method here is honest about what Android can and cannot do:
 *
 * SUPPORTED (via dispatchGesture):
 * - click: a tap stroke.
 * - drag: a stroke from A to B over 300ms.
 * - scroll: a swipe stroke. One notch = 120px. Scrolling down
 *   (dy > 0) swipes UP (the finger moves up, content follows);
 *   scrolling right (dx > 0) swipes LEFT. This matches the
 *   platform's swipe-to-scroll direction.
 * - double_click: two tap strokes, ~60ms apart.
 *
 * NO-OP (documented, not faked):
 * - move: Android has no pointer to move on a touch-only
 *   device. The call succeeds and moves nothing; the next
 *   gesture starts at its own point.
 *
 * UNSUPPORTED (typed `unsupported` errors, never silence):
 * - right_click: no right button on touch screens; a
 *   long-press is NOT a right-click and is not substituted.
 * - key / hold_key: an AccessibilityService cannot inject
 *   arbitrary key events (INJECT_EVENTS is signature-level).
 *   Global actions (back/home/recents) are NOT the Key enum
 *   and are not substituted.
 *
 * type_text uses ACTION_SET_TEXT on the focused editable node:
 * it REPLACES the field's content. When no editable node has
 * input focus, the call fails with `untypeable_text`.
 */
class InputInjector(private val service: AccessibilityService) {

    sealed class Outcome {
        data object Ok : Outcome()
        data class Error(val code: String, val message: String) : Outcome()
    }

    fun click(x: Int, y: Int): Outcome {
        val path = Path().apply { moveTo(x.toFloat(), y.toFloat()) }
        return gesture(path, 1)
    }

    fun drag(fromX: Int, fromY: Int, toX: Int, toY: Int): Outcome {
        val path = Path().apply {
            moveTo(fromX.toFloat(), fromY.toFloat())
            lineTo(toX.toFloat(), toY.toFloat())
        }
        return gesture(path, 300)
    }

    fun scroll(x: Int, y: Int, dx: Int, dy: Int): Outcome {
        if (dx == 0 && dy == 0) return Outcome.Ok
        // Swipe opposite to the scroll direction: scrolling
        // down moves the finger up; scrolling right moves it
        // left. Distance: 120px per notch.
        val toX = x - dx * 120
        val toY = y - dy * 120
        val path = Path().apply {
            moveTo(x.toFloat(), y.toFloat())
            lineTo(toX.toFloat(), toY.toFloat())
        }
        return gesture(path, 200)
    }

    fun doubleClick(x: Int, y: Int): Outcome {
        val first = click(x, y)
        if (first !is Outcome.Ok) return first
        Thread.sleep(60)
        return click(x, y)
    }

    fun typeText(text: String): Outcome {
        val root = service.rootInActiveWindow
            ?: return Outcome.Error("untypeable_text", "no active window")
        val focused = root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            ?: return Outcome.Error(
                "untypeable_text",
                "no focused input field; tap a text field first",
            )
        if (!focused.isEditable) {
            return Outcome.Error("untypeable_text", "focused node is not editable")
        }
        val args = Bundle().apply {
            putCharSequence(
                AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                text,
            )
        }
        val applied = focused.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
        return if (applied) Outcome.Ok
        else Outcome.Error("untypeable_text", "the field rejected the text")
    }

    private fun gesture(path: Path, durationMs: Long): Outcome {
        val latch = CountDownLatch(1)
        var cancelled = false
        val description = GestureDescription.Builder()
            .addStroke(GestureDescription.StrokeDescription(path, 0, durationMs))
            .build()
        val dispatched = service.dispatchGesture(
            description,
            object : AccessibilityService.GestureResultCallback() {
                override fun onCompleted(gestureDescription: GestureDescription) {
                    latch.countDown()
                }

                override fun onCancelled(gestureDescription: GestureDescription) {
                    cancelled = true
                    latch.countDown()
                }
            },
            null,
        )
        if (!dispatched) {
            return Outcome.Error("backend", "the system refused the gesture")
        }
        val done = latch.await(5, TimeUnit.SECONDS)
        if (!done) return Outcome.Error("backend", "gesture timed out after 5s")
        if (cancelled) return Outcome.Error("backend", "gesture was cancelled")
        return Outcome.Ok
    }
}
