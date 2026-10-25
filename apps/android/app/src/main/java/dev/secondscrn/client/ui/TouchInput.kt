package dev.secondscrn.client.ui

import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import kotlin.math.atan
import kotlin.math.cos
import kotlin.math.hypot
import kotlin.math.sin
import kotlin.math.tan
import uniffi.secondscrn_client.PenKind
import uniffi.secondscrn_client.PointerKind
import uniffi.secondscrn_client.TouchPoint

/** Where translated input goes (normalised 0..1 coordinates). */
interface InputSink {
    fun pointer(kind: PointerKind, x: Float, y: Float, secondary: Boolean)
    fun scroll(x: Float, y: Float, dx: Float, dy: Float)
    fun touch(points: List<TouchPoint>)
    fun pen(kind: PenKind, x: Float, y: Float, pressure: Float, tiltX: Float, tiltY: Float, barrel: Boolean, eraser: Boolean)
    fun toggleOverlay()
    /** True: translate finger gestures into mouse input instead of touch. */
    val touchAsMouse: Boolean
}

/**
 * Routes tablet input to the PC:
 *  - fingers: real multi-touch (Windows handles tap, press-and-hold, pan,
 *    pinch), or, in mouse mode, gestures translated to mouse input;
 *  - stylus: pen with pressure, tilt, hover, barrel button and eraser
 *    (fingers are ignored while the pen is near the screen);
 *  - a mouse: mouse.
 * In touch mode every finger goes to the PC (up to ten, including Windows'
 * three- and four-finger gestures); the overlay opens from its handle or
 * Back. Mouse mode keeps a three-finger tap for it.
 */
class TouchInput(private val view: View, private val sink: InputSink) : View.OnTouchListener,
    View.OnGenericMotionListener {
    private val mouseGestures = MouseGestures(view, sink)
    private val handler = Handler(Looper.getMainLooper())

    private fun nx(x: Float) = (x / view.width.coerceAtLeast(1)).coerceIn(0f, 1f)
    private fun ny(y: Float) = (y / view.height.coerceAtLeast(1)).coerceIn(0f, 1f)

    // --- Touch state
    private var touchActive = false

    // --- Pen state
    private var penNear = false
    private var lastPenX = 0f
    private var lastPenY = 0f
    private var lastPenTime = 0L

    private fun isPen(e: MotionEvent, i: Int): Boolean {
        val t = e.getToolType(i)
        return t == MotionEvent.TOOL_TYPE_STYLUS || t == MotionEvent.TOOL_TYPE_ERASER
    }

    override fun onTouch(v: View, e: MotionEvent): Boolean {
        if (e.isFromSource(InputDevice.SOURCE_MOUSE) && e.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE) {
            return mouseGestures.onMouse(e)
        }
        if (isPen(e, e.actionIndex)) return onPen(e)
        // Palm rejection: a hand resting on the screen while writing. Some
        // pens leave without a hover-exit, so it also ends after a pause.
        if (penNear && SystemClock.uptimeMillis() - lastPenTime > PEN_STALE_MS) penNear = false
        if (penNear) return true
        return if (sink.touchAsMouse) mouseGestures.onTouch(e) else onFingers(e)
    }

    /** Sends every finger of the event as one touch frame. */
    private fun onFingers(e: MotionEvent): Boolean {
        val action = e.actionMasked
        when (action) {
            MotionEvent.ACTION_CANCEL -> sendFrame(e, PointerKind.CANCEL)
            else -> sendFrame(e, null)
        }
        touchActive = action != MotionEvent.ACTION_UP && action != MotionEvent.ACTION_CANCEL
        return true
    }

    /** `force` overrides every contact's action (cancel). */
    private fun sendFrame(e: MotionEvent, force: PointerKind?) {
        val points = ArrayList<TouchPoint>(e.pointerCount)
        for (i in 0 until e.pointerCount) {
            if (isPen(e, i)) continue
            val kind = force ?: when {
                i != e.actionIndex -> PointerKind.MOVE
                e.actionMasked == MotionEvent.ACTION_DOWN || e.actionMasked == MotionEvent.ACTION_POINTER_DOWN -> PointerKind.DOWN
                e.actionMasked == MotionEvent.ACTION_UP || e.actionMasked == MotionEvent.ACTION_POINTER_UP -> PointerKind.UP
                else -> PointerKind.MOVE
            }
            points.add(TouchPoint(e.getPointerId(i).toUInt(), kind, nx(e.getX(i)), ny(e.getY(i))))
        }
        if (points.isNotEmpty()) sink.touch(points)
    }

    private fun cancelFingers() {
        if (!touchActive) return
        // Fingers missing from a frame are lifted (cancelled) by the PC.
        sink.touch(emptyList())
        touchActive = false
    }

    // --- Pen

    private val penLeave = Runnable {
        penNear = false
        sink.pen(PenKind.LEAVE, nx(lastPenX), ny(lastPenY), 0f, 0f, 0f, false, false)
    }

    private fun sendPen(kind: PenKind, e: MotionEvent, i: Int, pos: Int = -1) {
        val x = if (pos < 0) e.getX(i) else e.getHistoricalX(i, pos)
        val y = if (pos < 0) e.getY(i) else e.getHistoricalY(i, pos)
        val pressure = if (pos < 0) e.getPressure(i) else e.getHistoricalPressure(i, pos)
        val tilt = if (pos < 0) e.getAxisValue(MotionEvent.AXIS_TILT, i)
        else e.getHistoricalAxisValue(MotionEvent.AXIS_TILT, i, pos)
        val orientation = if (pos < 0) e.getOrientation(i) else e.getHistoricalOrientation(i, pos)
        // Android: tilt from perpendicular plus the direction it leans
        // (0 = up, pi/2 = right). Windows: tilt per axis.
        val t = tan(tilt.coerceIn(0f, 1.5f))
        val tiltX = Math.toDegrees(atan(t * sin(orientation)).toDouble()).toFloat()
        val tiltY = Math.toDegrees(atan(-t * cos(orientation)).toDouble()).toFloat()
        val barrel = e.buttonState and (MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_SECONDARY) != 0
        val eraser = e.getToolType(i) == MotionEvent.TOOL_TYPE_ERASER
        lastPenX = x
        lastPenY = y
        lastPenTime = SystemClock.uptimeMillis()
        sink.pen(kind, nx(x), ny(y), if (kind == PenKind.DOWN || kind == PenKind.MOVE) pressure else 0f, tiltX, tiltY, barrel, eraser)
    }

    private fun onPen(e: MotionEvent): Boolean {
        handler.removeCallbacks(penLeave)
        if (!penNear) {
            penNear = true
            cancelFingers()
        }
        val i = e.actionIndex
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> sendPen(PenKind.DOWN, e, i)
            MotionEvent.ACTION_MOVE -> {
                // Batched samples keep fast strokes smooth.
                for (h in 0 until e.historySize) sendPen(PenKind.MOVE, e, i, h)
                sendPen(PenKind.MOVE, e, i)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> sendPen(PenKind.UP, e, i)
            MotionEvent.ACTION_CANCEL -> sendPen(PenKind.CANCEL, e, i)
        }
        return true
    }

    /** Hover (pen, mouse) and the mouse wheel. */
    override fun onGenericMotion(v: View, e: MotionEvent): Boolean {
        if (isPen(e, 0)) {
            when (e.actionMasked) {
                MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> {
                    handler.removeCallbacks(penLeave)
                    if (!penNear) {
                        penNear = true
                        cancelFingers()
                    }
                    sendPen(PenKind.HOVER, e, 0)
                }
                // Android also reports an exit right before the pen touches
                // down; only treat it as leaving if no touch follows.
                MotionEvent.ACTION_HOVER_EXIT -> {
                    lastPenX = e.x
                    lastPenY = e.y
                    handler.postDelayed(penLeave, PEN_LEAVE_DELAY_MS)
                }
                else -> return false
            }
            return true
        }
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

    companion object {
        private const val PEN_LEAVE_DELAY_MS = 80L
        private const val PEN_STALE_MS = 400L
    }
}

/**
 * Mouse mode: finger gestures as mouse input.
 *  - tap: left click
 *  - press and drag: left-button drag (text selection, moving windows)
 *  - long press: right click
 *  - two-finger drag: scroll (smooth, fractional wheel notches)
 *  - three-finger tap: show/hide the session overlay
 */
private class MouseGestures(private val view: View, private val sink: InputSink) {
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

    fun onTouch(e: MotionEvent): Boolean {
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

    fun onMouse(e: MotionEvent): Boolean {
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
}
