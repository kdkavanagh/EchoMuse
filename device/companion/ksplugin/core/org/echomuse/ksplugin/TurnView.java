package org.echomuse.ksplugin;

import org.echomuse.companion.Link;

/**
 * When the turn view shows, and what (decision 9 of
 * docs/echo-show-companion-plan.md §11.2). Pure Java with the time passed in,
 * so the image build tests it on the JDK; the plugin drives it from the main
 * thread and re-reads {@link #shown()} after every call.
 *
 * <ul>
 * <li>A view of a turn in progress shows it; a new turn_id replaces the turn
 * on screen.
 * <li>{@code done} keeps it up for the linger (0: hides at once). With
 * {@code follow_up} it stays up for the next turn's {@code listening}, for at
 * most {@link #FOLLOW_UP_MS} or the linger, whichever is longer. With neither
 * heard nor answer (no_input, cancelled, error) it hides at once.
 * <li>A {@code done} for a turn not on screen is ignored: it is the replay
 * after the plugin's hello of a turn already gone.
 * <li>Link loss hides the view after the linger, as a {@code done} would.
 * <li>A turn whose {@code done} never comes is hidden {@link #STUCK_MS} after
 * its last view. The firmware ends the turn itself when it loses the
 * controller, so this guards only a {@code done} lost in a live session.
 * </ul>
 */
public final class TurnView {
    /** No deadline. */
    public static final long NEVER = Long.MAX_VALUE;
    /** turnViewLinger before the firmware's config arrives (§11.7). */
    static final long DEFAULT_LINGER_MS = 6000;
    /** How long a follow-up waits for its next turn at least. */
    static final long FOLLOW_UP_MS = 15_000;
    /** Longer than any reply HA reads out in practice; reset by every view. */
    static final long STUCK_MS = 300_000;

    private long lingerMs = DEFAULT_LINGER_MS;
    private Link.View shown;
    private long hideAt = NEVER;

    /** The view on screen, or null when hidden. */
    public Link.View shown() {
        return shown;
    }

    /** When {@link #tick} next has something to do, on the caller's clock; NEVER for no deadline. */
    public long hideAt() {
        return hideAt;
    }

    /** turnViewLinger from the firmware's config; applies from the next done. */
    public void linger(double seconds) {
        lingerMs = Math.round(Math.max(0, seconds) * 1000);
    }

    public void view(Link.View v, long now) {
        if (v.phase != Link.Phase.DONE) {
            shown = v;
            hideAt = now + STUCK_MS;
            return;
        }
        if (shown == null || !shown.turnId.equals(v.turnId)) {
            return;
        }
        boolean wasDone = shown.phase == Link.Phase.DONE;
        shown = v;
        if (v.heard == null && v.answer == null) {
            hide();
        } else if (wasDone) {
            // The same done again (a replay after a reconnect): the countdown
            // already running stands.
            return;
        } else if (v.followUp) {
            hideAt = now + Math.max(lingerMs, FOLLOW_UP_MS);
        } else {
            after(now, lingerMs);
        }
    }

    public void linkLost(long now) {
        if (shown != null) {
            after(now, Math.min(lingerMs, hideAt - now));
        }
    }

    /** Applies a deadline that has passed. */
    public void tick(long now) {
        if (now >= hideAt) {
            hide();
        }
    }

    private void after(long now, long ms) {
        if (ms <= 0) {
            hide();
        } else {
            hideAt = now + ms;
        }
    }

    private void hide() {
        shown = null;
        hideAt = NEVER;
    }
}
