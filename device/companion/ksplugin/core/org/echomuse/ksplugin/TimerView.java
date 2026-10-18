package org.echomuse.ksplugin;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Locale;

import org.echomuse.companion.Link;

/**
 * What the timer view draws: one face per HA timer, from the controller's
 * timer.view. Pure Java with the time passed in, so the image build tests it
 * on the JDK; the plugin drives it from the main thread and re-reads
 * {@link #faces} after every call.
 *
 * <ul>
 * <li>Each line replaces every timer; the timers are anchored at its
 * receipt, and active ones count down locally until the next.
 * <li>The controller resends the list every second while the speaker has a
 * timer, so one not heard of for {@link #STALE_MS} is cleared, as is
 * everything on link loss.
 * <li>The faces keep the controller's order (active, then paused, each
 * soonest first); past {@link #MAX_FACES} timers the last face counts the
 * rest.
 * </ul>
 */
public final class TimerView {
    /** No deadline. */
    public static final long NEVER = Long.MAX_VALUE;
    /** Faces at most, the overflow face included. */
    public static final int MAX_FACES = 4;
    /** Five of the controller's once-a-second lines missed. */
    static final long STALE_MS = 5000;
    /** About a frame at 60 Hz: the soonest a redraw is asked for. */
    static final long MIN_FRAME_MS = 16;

    /** One circle to draw. */
    public static final class Face {
        /** The time remaining, or "+N" on the overflow face. */
        public final String text;
        /** The timer's name, or null. */
        public final String label;
        /** Remaining over total, in [0, 1]: the progress ring's extent. */
        public final float fraction;
        public final boolean paused;
        /** Stands for the timers past the others; draws only its text. */
        public final boolean overflow;

        Face(String text, String label, float fraction, boolean paused, boolean overflow) {
            this.text = text;
            this.label = label;
            this.fraction = fraction;
            this.paused = paused;
            this.overflow = overflow;
        }
    }

    private List<Link.Timer> timers = Collections.emptyList();
    /** Receipt of the last line, on the caller's clock. */
    private long anchor;

    /** No timer held: nothing to draw. */
    public boolean isEmpty() {
        return timers.isEmpty();
    }

    /** When {@link #tick} next has something to do, on the caller's clock; NEVER for no deadline. */
    public long staleAt() {
        return timers.isEmpty() ? NEVER : anchor + STALE_MS;
    }

    public void timers(Link.Timers t, long now) {
        timers = t.timers;
        anchor = now;
    }

    public void linkLost() {
        timers = Collections.emptyList();
    }

    /** Applies a deadline that has passed. */
    public void tick(long now) {
        if (now >= staleAt()) {
            timers = Collections.emptyList();
        }
    }

    /** The faces to draw at now, in order; empty when nothing is held. */
    public List<Face> faces(long now) {
        int n = timers.size();
        int shown = n > MAX_FACES ? MAX_FACES - 1 : n;
        List<Face> out = new ArrayList<>(Math.min(n, MAX_FACES));
        for (int k = 0; k < shown; k++) {
            Link.Timer t = timers.get(k);
            long left = remaining(t, now);
            float fraction = t.totalMs == 0 ? 0 : Math.max(0, Math.min(1, (float) left / t.totalMs));
            out.add(new Face(format(left), t.name, fraction, !t.active, false));
        }
        if (shown < n) {
            out.add(new Face("+" + (n - shown), null, 0, false, true));
        }
        return out;
    }

    /**
     * When a face next changes at circumferencePx of ring: the soonest of an
     * active timer's text turning over and its arc moving one pixel, never
     * sooner than {@link #MIN_FRAME_MS} after now; NEVER while nothing shown
     * moves.
     */
    public long nextFrameAt(long now, float circumferencePx) {
        int n = timers.size();
        int shown = n > MAX_FACES ? MAX_FACES - 1 : n;
        long next = NEVER;
        for (int k = 0; k < shown; k++) {
            Link.Timer t = timers.get(k);
            long left = remaining(t, now);
            if (!t.active || left == 0) {
                continue;
            }
            // The text shows whole seconds rounded up: it turns over when
            // left reaches the next lower multiple of 1000.
            long in = left - (left - 1) / 1000 * 1000;
            if (t.totalMs > 0 && circumferencePx > 0) {
                in = Math.min(in, Math.max(1, (long) Math.ceil(t.totalMs / (double) circumferencePx)));
            }
            next = Math.min(next, now + Math.max(MIN_FRAME_MS, in));
        }
        return next;
    }

    /** Remaining at now: an active timer counts down from the line's receipt. */
    private long remaining(Link.Timer t, long now) {
        if (!t.active) {
            return t.remainingMs;
        }
        return Math.max(0, t.remainingMs - Math.max(0, now - anchor));
    }

    /** Whole seconds rounded up, so it reads 0:00 exactly at zero: M:SS, or H:MM:SS from an hour. */
    static String format(long ms) {
        long s = (Math.max(0, ms) + 999) / 1000;
        if (s < 3600) {
            return String.format(Locale.ROOT, "%d:%02d", s / 60, s % 60);
        }
        return String.format(Locale.ROOT, "%d:%02d:%02d", s / 3600, s / 60 % 60, s % 60);
    }
}
