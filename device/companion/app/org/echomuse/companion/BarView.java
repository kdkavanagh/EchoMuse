package org.echomuse.companion;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.LinearGradient;
import android.graphics.Paint;
import android.graphics.Shader;
import android.os.SystemClock;
import android.view.MotionEvent;
import android.view.View;

/**
 * The light bar: the ring's twelve colours as a horizontal gradient along
 * the bottom edge (index 0 at the left), the "Stop" label in the stop zone,
 * and the status text above the bar. The window it sits in is sized from
 * {@link #windowHeight()} and is touchable only while a zone is active.
 */
final class BarView extends View {
    /** A press in an active zone: down, or up after heldMs. */
    interface PressListener {
        void press(boolean down, long heldMs);
    }

    private static final float BAR_DP = 8;
    /** The touchable band of decision 9. */
    private static final float ZONE_DP = 72;
    private static final float TEXT_DP = 24;
    private static final float[] POSITIONS = new float[Link.SEGMENTS];

    static {
        for (int k = 0; k < Link.SEGMENTS; k++) {
            POSITIONS[k] = (k + 0.5f) / Link.SEGMENTS;
        }
    }

    private final PressListener listener;
    private final float density;
    private final Paint barPaint = new Paint();
    private final Paint textPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint labelPaint = new Paint(Paint.ANTI_ALIAS_FLAG);

    /** ARGB per segment, or null to draw no bar. */
    private int[] colours;
    private Link.Zone zone = Link.Zone.NONE;
    private String status;
    private boolean pressed;
    private long downTime;

    BarView(Context context, PressListener listener) {
        super(context);
        this.listener = listener;
        density = context.getResources().getDisplayMetrics().density;
        float scaled = context.getResources().getDisplayMetrics().scaledDensity;
        textPaint.setColor(0xffffffff);
        textPaint.setTextSize(13 * scaled);
        textPaint.setTextAlign(Paint.Align.CENTER);
        textPaint.setShadowLayer(3 * density, 0, 0, 0xff000000);
        labelPaint.setColor(0xffffffff);
        labelPaint.setTextSize(22 * scaled);
        labelPaint.setFakeBoldText(true);
        labelPaint.setTextAlign(Paint.Align.CENTER);
        labelPaint.setShadowLayer(4 * density, 0, 0, 0xff000000);
    }

    /** The frame to draw; null or all-black draws no bar. */
    void setFrame(Link.Frame frame) {
        colours = frame == null || frame.dark() ? null : frame.argb();
        shade();
        invalidate();
    }

    void setStatus(String text) {
        if (text == null ? status == null : text.equals(status)) {
            return;
        }
        status = text;
        invalidate();
    }

    /**
     * Moves to zone z. A press still down when the zone ends is released
     * here: the window stops taking touches, so its up may never arrive.
     */
    void setZone(Link.Zone z) {
        if (z == zone) {
            return;
        }
        zone = z;
        if (z == Link.Zone.NONE && pressed) {
            pressed = false;
            listener.press(false, SystemClock.uptimeMillis() - downTime);
        }
        invalidate();
    }

    boolean touchable() {
        return zone != Link.Zone.NONE;
    }

    /** The overlay window's height in pixels for the current state. */
    int windowHeight() {
        if (touchable()) {
            return Math.round(ZONE_DP * density);
        }
        return Math.round((BAR_DP + (status != null ? TEXT_DP : 0)) * density);
    }

    @Override
    protected void onSizeChanged(int w, int h, int oldw, int oldh) {
        shade();
    }

    /** Builds the bar's shader once per frame or size, not per draw. */
    private void shade() {
        barPaint.setShader(colours == null || getWidth() == 0 ? null
                : new LinearGradient(0, 0, getWidth(), 0, colours, POSITIONS, Shader.TileMode.CLAMP));
    }

    @Override
    protected void onDraw(Canvas canvas) {
        float w = getWidth();
        float h = getHeight();
        float barTop = h - BAR_DP * density;
        if (barPaint.getShader() != null) {
            canvas.drawRect(0, barTop, w, h, barPaint);
        }
        if (zone == Link.Zone.STOP) {
            float y = barTop / 2 - (labelPaint.descent() + labelPaint.ascent()) / 2;
            canvas.drawText("Stop", w / 2, y, labelPaint);
        }
        if (status != null) {
            canvas.drawText(status, w / 2, barTop - textPaint.descent() - 2 * density, textPaint);
        }
    }

    @Override
    public boolean onTouchEvent(MotionEvent ev) {
        if (!touchable()) {
            return false;
        }
        switch (ev.getActionMasked()) {
            case MotionEvent.ACTION_DOWN:
                pressed = true;
                downTime = ev.getDownTime();
                listener.press(true, 0);
                return true;
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_CANCEL:
                if (pressed) {
                    pressed = false;
                    listener.press(false, ev.getEventTime() - ev.getDownTime());
                }
                return true;
            default:
                return true;
        }
    }
}
