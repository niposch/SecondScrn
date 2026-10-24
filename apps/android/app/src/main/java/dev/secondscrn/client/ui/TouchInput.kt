package dev.secondscrn.client.ui

import android.os.Handler
import android.os.Looper
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import kotlin.math.hypot
import uniffi.secondscrn_client.PointerKind

/** Where translated input goes (normalised 0..1 coordinates). */
interface InputSink {
    fun pointer(kind: PointerKind, x: Float, y: Float, secondary: Boolean)
    fun scroll(x: Float, y: Float, dx: Float, dy: Float)
    fun toggleOverlay()
}

/**
 * Translates touch gestures into mouse input:
 *  - tap: left click
 *  - press and drag: left-button drag (text selection, moving windows)
 *  - long press: right click
 *  - two-finger drag: scroll (smooth, fractional wheel notches)
 *  - three-finger tap: show/hide the session overlay
 * Mouse and stylus hover move the pointer; a connected mouse maps directly.
 */
class TouchInput(private val view: View, private val sink: InputSink) : View.OnTouchListener,
    View.OnGenericMotionListener {
    private enum class Mode { Idle, Pending, Dragging, Scrolling, Consumed }

    private val slop = ViewConfiguration.get(view.context).scaledTouchSlop.toFloat()
    private val longPress = ViewConfiguration.getLongPressTimeout().toLong()
    private val pxPerNotch = 48f * view.resources.displayMetrics.density
    private val handler = Handler(Looper.getMainLooper())

    private var mode = Mode.Idle
    private var downX = 0f
    private var downY = 0f
    private var lastMidX = 0f
    private var lastMidY = 0f
    private var maxPointers = 0

    private fun nx(x: Float) = (x / view.width.coerceAtLeast(1)).coerceIn(0f, 1f)
    private fun ny(y: Float) = (y / view.height.coerceAtLeast(1)).coerceIn(0f, 1f)

    private val longPressAction = Runnable {
        if (mode == Mode.Pending) {
            view.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
            sink.pointer(PointerKind.DOWN, nx(downX), ny(downY), true)
            sink.pointer(PointerKind.UP, nx(downX), ny(downY), true)
            mode = Mode.Consumed
        }
    }

    override fun onTouch(v: View, e: MotionEvent): Boolean {
        if (e.isFromSource(InputDevice.SOURCE_MOUSE)) return onMouse(e)
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                mode = Mode.Pending
                downX = e.x
                downY = e.y
                maxPointers = 1
                handler.postDelayed(longPressAction, longPress)
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                maxPointers = maxOf(maxPointers, e.pointerCount)
                handler.removeCallbacks(longPressAction)
                when {
                    e.pointerCount == 2 && mode == Mode.Pending -> {
                        mode = Mode.Scrolling
                        lastMidX = (e.getX(0) + e.getX(1)) / 2
                        lastMidY = (e.getY(0) + e.getY(1)) / 2
                    }
                    e.pointerCount >= 3 && mode != Mode.Dragging -> mode = Mode.Consumed
                }
            }
            MotionEvent.ACTION_MOVE -> when (mode) {
                Mode.Pending -> if (hypot(e.x - downX, e.y - downY) > slop) {
                    handler.removeCallbacks(longPressAction)
                    sink.pointer(PointerKind.DOWN, nx(downX), ny(downY), false)
                    sink.pointer(PointerKind.MOVE, nx(e.x), ny(e.y), false)
                    mode = Mode.Dragging
                }
                Mode.Dragging -> sink.pointer(PointerKind.MOVE, nx(e.x), ny(e.y), false)
                Mode.Scrolling -> if (e.pointerCount >= 2) {
                    val mx = (e.getX(0) + e.getX(1)) / 2
                    val my = (e.getY(0) + e.getY(1)) / 2
                    // Natural scrolling: content follows the fingers.
                    val dx = (lastMidX - mx) / pxPerNotch
                    val dy = (lastMidY - my) / pxPerNotch
                    if (dx != 0f || dy != 0f) sink.scroll(nx(mx), ny(my), dx, dy)
                    lastMidX = mx
                    lastMidY = my
                }
                else -> {}
            }
            MotionEvent.ACTION_UP -> {
                handler.removeCallbacks(longPressAction)
                when (mode) {
                    Mode.Pending -> {
                        sink.pointer(PointerKind.DOWN, nx(downX), ny(downY), false)
                        sink.pointer(PointerKind.UP, nx(downX), ny(downY), false)
                    }
                    Mode.Dragging -> sink.pointer(PointerKind.UP, nx(e.x), ny(e.y), false)
                    Mode.Consumed -> if (maxPointers >= 3) sink.toggleOverlay()
                    else -> {}
                }
                mode = Mode.Idle
            }
            MotionEvent.ACTION_CANCEL -> {
                handler.removeCallbacks(longPressAction)
                if (mode == Mode.Dragging) sink.pointer(PointerKind.CANCEL, nx(e.x), ny(e.y), false)
                mode = Mode.Idle
            }
        }
        return true
    }

    private fun onMouse(e: MotionEvent): Boolean {
        val secondary = e.buttonState and MotionEvent.BUTTON_SECONDARY != 0 ||
            e.actionButton == MotionEvent.BUTTON_SECONDARY
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> sink.pointer(PointerKind.DOWN, nx(e.x), ny(e.y), secondary)
            MotionEvent.ACTION_MOVE -> sink.pointer(PointerKind.MOVE, nx(e.x), ny(e.y), secondary)
            MotionEvent.ACTION_UP -> sink.pointer(PointerKind.UP, nx(e.x), ny(e.y), secondary)
            MotionEvent.ACTION_CANCEL -> sink.pointer(PointerKind.CANCEL, nx(e.x), ny(e.y), secondary)
        }
        return true
    }

    /** Hover (mouse, stylus) and the mouse wheel. */
    override fun onGenericMotion(v: View, e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_HOVER_MOVE -> sink.pointer(PointerKind.MOVE, nx(e.x), ny(e.y), false)
            MotionEvent.ACTION_SCROLL -> sink.scroll(
                nx(e.x), ny(e.y),
                e.getAxisValue(MotionEvent.AXIS_HSCROLL),
                -e.getAxisValue(MotionEvent.AXIS_VSCROLL),
            )
            else -> return false
        }
        return true
    }
}
