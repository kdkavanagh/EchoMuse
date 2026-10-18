package org.echomuse.ksplugin;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.RectF;
import android.os.SystemClock;
import android.text.TextPaint;
import android.text.TextUtils;
import android.util.Log;
import android.util.TypedValue;
import android.view.View;

import java.util.List;

import me.jxl.kiosk.plugins.KsTheme;

/**
 * The timer view: a row of circles in the top-right corner over the kiosk's
 * page, one per HA timer, the soonest nearest the corner. Each is a disc with
 * the time remaining in the middle and the timer's name under it, ringed by
 * an arc of the time left that shrinks clockwise toward 12 o'clock. Main
 * thread only; it takes no touches.
 */
final class TimerOverlay extends View {
    private static final String TAG = "EchoMuse";
    static final int DIAMETER_DP = 128;
    static final int GAP_DP = 16;
    /** Room for {@link TimerView#MAX_FACES} faces. */
    static final int WIDTH_DP = TimerView.MAX_FACES * DIAMETER_DP + (TimerView.MAX_FACES - 1) * GAP_DP;
    static final int HEIGHT_DP = DIAMETER_DP;
    private static final float STROKE_DP = 8;
    private static final float TIME_SP = 30;
    /** H:MM:SS is two characters wider. */
    private static final float LONG_TIME_SP = 24;
    private static final float LABEL_SP = 13;
    private static final float LABEL_GAP_DP = 2;
    /** Of the diameter. */
    private static final float LABEL_WIDTH = 0.7f;
    /** Near-opaque, so the time reads over any page. */
    private static final int DISC_ALPHA = 0xe6;

    private final Paint discPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint trackPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint arcPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final TextPaint timePaint = new TextPaint(Paint.ANTI_ALIAS_FLAG);
    private final TextPaint labelPaint = new TextPaint(Paint.ANTI_ALIAS_FLAG);
    private final RectF oval = new RectF();
    private KsTheme theme;
    private int primary;
    private int onSurface;
    private int onSurfaceVariant;
    private float timeSize;
    private float longTimeSize;

    private TimerView view;
    private boolean failed;

    TimerOverlay(Context context, KsTheme theme) {
        super(context);
        setTheme(theme);
        setVisibility(INVISIBLE);
    }

    void setTheme(KsTheme t) {
        theme = t;
        primary = t.color("primary");
        onSurface = t.color("onSurface");
        onSurfaceVariant = t.color("onSurfaceVariant");
        discPaint.setColor(DISC_ALPHA << 24 | t.color("surface") & 0xffffff);
        float stroke = t.px(STROKE_DP);
        trackPaint.setStyle(Paint.Style.STROKE);
        trackPaint.setStrokeWidth(stroke);
        trackPaint.setColor(t.color("outlineVariant"));
        arcPaint.setStyle(Paint.Style.STROKE);
        arcPaint.setStrokeWidth(stroke);
        arcPaint.setStrokeCap(Paint.Cap.ROUND);
        timeSize = sp(TIME_SP);
        longTimeSize = sp(LONG_TIME_SP);
        timePaint.setTypeface(t.typeface(500));
        timePaint.setTextAlign(Paint.Align.CENTER);
        labelPaint.setTypeface(t.typeface(400));
        labelPaint.setTextSize(sp(LABEL_SP));
        labelPaint.setTextAlign(Paint.Align.CENTER);
        labelPaint.setColor(onSurfaceVariant);
        invalidate();
    }

    private float sp(float sp) {
        return TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, sp, getResources().getDisplayMetrics());
    }

    /** The timers to draw; suppressed (the turn view is up) or none hides. */
    void show(TimerView v, boolean suppressed) {
        view = v;
        if (suppressed || v.isEmpty()) {
            setVisibility(INVISIBLE);
            return;
        }
        setVisibility(VISIBLE);
        invalidate();
    }

    @Override
    protected void onDraw(Canvas canvas) {
        try {
            paint(canvas);
        } catch (Throwable t) {
            // Escaping here would end Kiosk Satellite (SDK docs/overlays.md).
            if (!failed) {
                failed = true;
                Log.e(TAG, "timer view: draw failed", t);
            }
        }
    }

    private void paint(Canvas canvas) {
        TimerView v = view;
        if (v == null) {
            return;
        }
        long now = SystemClock.uptimeMillis();
        List<TimerView.Face> faces = v.faces(now);
        float d = theme.px(DIAMETER_DP);
        float gap = theme.px(GAP_DP);
        float radius = d / 2;
        float ring = radius - arcPaint.getStrokeWidth() / 2;
        float cy = getHeight() / 2f;
        for (int k = 0; k < faces.size(); k++) {
            float cx = getWidth() - radius - k * (d + gap);
            drawFace(canvas, faces.get(k), cx, cy, radius, ring);
        }
        long next = v.nextFrameAt(now, (float) (2 * Math.PI * ring));
        if (next != TimerView.NEVER) {
            postInvalidateDelayed(next - now);
        }
    }

    private void drawFace(Canvas canvas, TimerView.Face f, float cx, float cy, float radius, float ring) {
        canvas.drawCircle(cx, cy, radius, discPaint);
        canvas.drawCircle(cx, cy, ring, trackPaint);
        int ink = f.paused ? onSurfaceVariant : onSurface;
        if (!f.overflow && f.fraction > 0) {
            arcPaint.setColor(f.paused ? onSurfaceVariant : primary);
            oval.set(cx - ring, cy - ring, cx + ring, cy + ring);
            canvas.drawArc(oval, -90, 360 * f.fraction, false, arcPaint);
        }
        timePaint.setTextSize(f.text.length() > 5 ? longTimeSize : timeSize);
        timePaint.setColor(ink);
        Paint.FontMetrics tm = timePaint.getFontMetrics();
        float timeHeight = tm.descent - tm.ascent;
        CharSequence label = f.label == null ? null
                : TextUtils.ellipsize(f.label, labelPaint, 2 * radius * LABEL_WIDTH, TextUtils.TruncateAt.END);
        if (label == null || label.length() == 0) {
            canvas.drawText(f.text, cx, cy - (tm.ascent + tm.descent) / 2, timePaint);
            return;
        }
        // The time and the label centred as one block.
        Paint.FontMetrics lm = labelPaint.getFontMetrics();
        float labelHeight = lm.descent - lm.ascent;
        float top = cy - (timeHeight + theme.px(LABEL_GAP_DP) + labelHeight) / 2;
        canvas.drawText(f.text, cx, top - tm.ascent, timePaint);
        float labelTop = top + timeHeight + theme.px(LABEL_GAP_DP);
        canvas.drawText(label, 0, label.length(), cx, labelTop - lm.ascent, labelPaint);
    }
}
