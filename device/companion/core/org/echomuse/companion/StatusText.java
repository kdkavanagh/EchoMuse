package org.echomuse.companion;

/**
 * What the overlay and the notification say. The firmware sends only state
 * (§3 {@code status}) and both texts come from here, so they cannot disagree.
 *
 * <p>Times are durations in milliseconds. {@code unlinkedMs} is how long the
 * local link to the firmware has been down (since the service started when it
 * never came up), negative while it is up. {@code controllerDownMs} is how long
 * {@code status} has said {@code down}.
 */
final class StatusText {
    /** The local link down this long: the bar shows nothing and EchoMuse is not running. */
    static final long LINK_GRACE_MS = 2000;
    /** The controller link down this long before the overlay says so. */
    static final long DOWN_TEXT_AFTER_MS = 10000;

    static final String NOT_RUNNING = "EchoMuse is not running";
    static final String CONNECTING = "Connecting to EchoMuse…";
    private static final String SEP = " · ";

    private StatusText() {}

    /** True once the local link has been down long enough that the bar shows nothing. */
    static boolean blank(long unlinkedMs) {
        return unlinkedMs >= LINK_GRACE_MS;
    }

    /** The text above the bar, or null for none. */
    static String overlay(Link.Status status, long unlinkedMs, long controllerDownMs) {
        if (blank(unlinkedMs) || status == null) {
            return null;
        }
        switch (status.link) {
            case PENDING:
                return line(status);
            case DOWN:
                return controllerDownMs >= DOWN_TEXT_AFTER_MS ? line(status) : null;
            default:
                return null;
        }
    }

    /** The notification's title: the link state line. */
    static String title(Link.Status status, long unlinkedMs) {
        if (blank(unlinkedMs)) {
            return NOT_RUNNING;
        }
        return status == null ? CONNECTING : line(status);
    }

    /** The notification's text: the firmware and app versions. */
    static String versions(String firmware, String app) {
        return firmware == null ? "App " + app : "Firmware " + firmware + SEP + "App " + app;
    }

    /**
     * Milliseconds until overlay() or title() next change with time alone,
     * or -1 when only a message can change them.
     */
    static long nextChangeMs(Link.Status status, long unlinkedMs, long controllerDownMs) {
        if (unlinkedMs >= 0) {
            return blank(unlinkedMs) ? -1 : LINK_GRACE_MS - unlinkedMs;
        }
        if (status != null && status.link == Link.State.DOWN && controllerDownMs < DOWN_TEXT_AFTER_MS) {
            return DOWN_TEXT_AFTER_MS - controllerDownMs;
        }
        return -1;
    }

    /** The firmware's controller link as one line. */
    static String line(Link.Status s) {
        switch (s.link) {
            case UP:
                return with("Connected", s.controller);
            case PENDING:
                return with("Waiting for approval", s.deviceId);
            default:
                break;
        }
        String reason = s.reason == null ? "" : s.reason;
        switch (reason) {
            case "starting":
                return with("Looking for the EchoMuse controller", s.controller);
            case "closed":
            case "session_lost":
                return with("Lost the controller", s.controller);
            case "unauthorized":
                return with("Not authorised by the controller", s.controller);
            case "protocol":
                return with("Controller link error", s.controller);
            default:
                return with(with("Not connected", s.controller), s.reason);
        }
    }

    private static String with(String text, String detail) {
        return detail == null || detail.isEmpty() ? text : text + SEP + detail;
    }
}
