package org.echomuse.ksplugin;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.ComposeShader;
import android.graphics.LinearGradient;
import android.graphics.Paint;
import android.graphics.PorterDuff;
import android.graphics.Shader;
import android.os.SystemClock;
import android.text.Layout;
import android.text.StaticLayout;
import android.text.TextPaint;
import android.text.TextUtils;
import android.util.Log;
import android.util.TypedValue;
import android.view.View;

import java.util.Arrays;

import me.jxl.kiosk.plugins.KsTheme;
import org.echomuse.companion.Link;

/**
 * The full-screen turn view, after Kiosk Satellite's own voice overlay
 * (decision 9 of §11.2): a near-opaque backdrop, the request in grey, three
 * pulsing dots while thinking, the answer in large text that scrolls up as it
 * grows, and a soft glow along the bottom in the bar's colours. The bottom
 * band is the app's bar and its touch zones (decision 10), so no text is
 * drawn there. Main thread only; it takes no touches.
 */
final class ViewOverlay extends View {
    private static final String TAG = "EchoMuse";
    private static final float HEARD_SP = 28;
    private static final float ANSWER_SP = 34;
    private static final int HEARD_MAX_LINES = 3;
    private static final float SIDE_DP = 64;
    private static final float TOP_DP = 56;
    private static final float GAP_DP = 28;
    /** The app's bar window at its tallest, a touch zone (decision 9). */
    private static final float BAR_DP = 72;
    private static final float GLOW_DP = 160;
    /** The glow's opacity at the bottom edge, before the frame's own. */
    private static final int GLOW_ALPHA = 0xb0;
    /** Near-opaque: the kiosk shows through only faintly (no blur before API 31). */
    private static final int BACKDROP_ALPHA = 0xf2;
    private static final float DOT_DP = 7;
    private static final float DOT_GAP_DP = 26;
    private static final float DOT_PERIOD_MS = 1200;
    /** The slowest an answer scrolls; a long jump catches up faster. */
    private static final float SCROLL_DP_PER_S = 160;
    private static final float[] POSITIONS = new float[Link.SEGMENTS];

    static {
        for (int k = 0; k < Link.SEGMENTS; k++) {
            POSITIONS[k] = (k + 0.5f) / Link.SEGMENTS;
        }
    }

    private final TextPaint heardPaint = new TextPaint(Paint.ANTI_ALIAS_FLAG);
    private final TextPaint answerPaint = new TextPaint(Paint.ANTI_ALIAS_FLAG);
    private final Paint dotPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint glowPaint = new Paint();
    private KsTheme theme;
    private int backdrop;
    private int dotAlpha;

    private Link.View view;
    private int[] glow;
    private int glowWidth;
    private int glowHeight;
    private StaticLayout heardLayout;
    private StaticLayout answerLayout;
    private int layoutWidth;
    /** How far the answer is scrolled up, in pixels. */
    private float scroll;
    /** uptimeMillis of the last scroll step; 0 while the answer is at rest. */
    private long scrolledAt;
    private boolean failed;

    ViewOverlay(Context context, KsTheme theme) {
        super(context);
        setTheme(theme);
        setVisibility(INVISIBLE);
    }

    void setTheme(KsTheme t) {
        theme = t;
        backdrop = BACKDROP_ALPHA << 24 | t.color("surface") & 0xffffff;
        style(heardPaint, t, HEARD_SP, "onSurfaceVariant");
        style(answerPaint, t, ANSWER_SP, "onSurface");
        int dots = t.color("onSurfaceVariant");
        dotPaint.setColor(dots);
        dotAlpha = dots >>> 24;
        heardLayout = null;
        answerLayout = null;
        invalidate();
    }

    private void style(TextPaint p, KsTheme t, float sp, String role) {
        p.setColor(t.color(role));
        p.setTypeface(t.typeface(400));
        p.setTextSize(TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, sp, getResources().getDisplayMetrics()));
    }

    /** The turn to draw, or null to hide. */
    void show(Link.View v) {
        Link.View old = view;
        view = v;
        if (v == null) {
            heardLayout = null;
            answerLayout = null;
            setVisibility(INVISIBLE);
            return;
        }
        if (old == null || !old.turnId.equals(v.turnId)) {
            scroll = 0;
            scrolledAt = 0;
        }
        if (old == null || !TextUtils.equals(old.heard, v.heard)) {
            heardLayout = null;
        }
        if (old == null || !TextUtils.equals(old.answer, v.answer)) {
            answerLayout = null;
        }
        setVisibility(VISIBLE);
        invalidate();
    }

    /** The bar's colours as ARGB, index 0 at the left, or null for no glow. */
    void setGlow(int[] argb) {
        if (Arrays.equals(argb, glow)) {
            return;
        }
        glow = argb;
        glowPaint.setShader(null);
        if (view != null) {
            invalidate();
        }
    }

    @Override
    protected void onDraw(Canvas canvas) {
        try {
            paint(canvas, view);
        } catch (Throwable t) {
            // Escaping here would end Kiosk Satellite (SDK docs/overlays.md).
            if (!failed) {
                failed = true;
                Log.e(TAG, "turn view: draw failed", t);
            }
        }
    }

    private void paint(Canvas canvas, Link.View v) {
        if (v == null) {
            return;
        }
        int w = getWidth();
        int h = getHeight();
        canvas.drawColor(backdrop);
        drawGlow(canvas, w, h);

        float side = theme.px(SIDE_DP);
        int textWidth = Math.max(1, Math.round(w - 2 * side));
        if (textWidth != layoutWidth) {
            layoutWidth = textWidth;
            heardLayout = null;
            answerLayout = null;
        }
        float y = theme.px(TOP_DP);
        if (v.heard != null) {
            if (heardLayout == null) {
                heardLayout = layout(v.heard, heardPaint, textWidth, HEARD_MAX_LINES);
            }
            canvas.save();
            canvas.translate(side, y);
            heardLayout.draw(canvas);
            canvas.restore();
            y += heardLayout.getHeight() + theme.px(GAP_DP);
        }
        float bottom = h - theme.px(BAR_DP) - theme.px(GAP_DP);
        if (v.phase == Link.Phase.THINKING) {
            drawDots(canvas, side, y);
            postInvalidateOnAnimation();
        } else if (v.answer != null && bottom > y) {
            if (answerLayout == null) {
                answerLayout = layout(v.answer, answerPaint, textWidth, Integer.MAX_VALUE);
            }
            float target = Math.max(0, answerLayout.getHeight() - (bottom - y));
            boolean moving = step(target);
            canvas.save();
            canvas.clipRect(0, y, w, bottom);
            canvas.translate(side, y - scroll);
            answerLayout.draw(canvas);
            canvas.restore();
            if (moving) {
                postInvalidateOnAnimation();
            }
        }
    }

    /** Moves the scroll toward target; true while it has further to go. */
    private boolean step(float target) {
        float d = target - scroll;
        if (Math.abs(d) < 0.5f) {
            scroll = target;
            scrolledAt = 0;
            return false;
        }
        long now = SystemClock.uptimeMillis();
        float dt = scrolledAt == 0 ? 0.016f : Math.min(0.1f, (now - scrolledAt) / 1000f);
        scrolledAt = now;
        float by = Math.max(theme.px(SCROLL_DP_PER_S) * dt, Math.abs(d) * Math.min(1f, dt * 3));
        scroll += Math.signum(d) * Math.min(Math.abs(d), by);
        return true;
    }

    private void drawDots(Canvas canvas, float left, float top) {
        float r = theme.px(DOT_DP);
        float gap = theme.px(DOT_GAP_DP);
        float t = SystemClock.uptimeMillis() / DOT_PERIOD_MS;
        for (int k = 0; k < 3; k++) {
            double wave = 0.5 + 0.5 * Math.sin(2 * Math.PI * (t - k * 0.15));
            dotPaint.setAlpha((int) (dotAlpha * (0.25 + 0.75 * wave)));
            canvas.drawCircle(left + r + k * gap, top + 2 * r, r, dotPaint);
        }
    }

    /** The frame's colours across the bottom, fading out upward. */
    private void drawGlow(Canvas canvas, int w, int h) {
        if (glow == null) {
            return;
        }
        int height = Math.min(h, theme.px(GLOW_DP));
        if (glowPaint.getShader() == null || glowWidth != w || glowHeight != h) {
            glowWidth = w;
            glowHeight = h;
            Shader across = new LinearGradient(0, 0, w, 0, glow, POSITIONS, Shader.TileMode.CLAMP);
            Shader up = new LinearGradient(0, h - height, 0, h, 0, GLOW_ALPHA << 24, Shader.TileMode.CLAMP);
            glowPaint.setShader(new ComposeShader(across, up, PorterDuff.Mode.DST_IN));
        }
        canvas.drawRect(0, h - height, w, h, glowPaint);
    }

    private static StaticLayout layout(String text, TextPaint paint, int width, int maxLines) {
        StaticLayout.Builder b = StaticLayout.Builder.obtain(text, 0, text.length(), paint, width)
                .setAlignment(Layout.Alignment.ALIGN_NORMAL)
                .setIncludePad(false);
        if (maxLines != Integer.MAX_VALUE) {
            b.setMaxLines(maxLines).setEllipsize(TextUtils.TruncateAt.END);
        }
        return b.build();
    }
}
