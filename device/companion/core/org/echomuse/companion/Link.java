package org.echomuse.companion;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Map;

/**
 * The local link, version 1 (docs/echo-show-companion-plan.md §3): one JSON
 * object per line between this app and the firmware on 127.0.0.1:8771, the
 * type in field {@code t}. Pure Java, so the image build tests it on the JDK.
 *
 * <p>Unknown types are ignored, for a newer firmware; a malformed line or a
 * known type with a missing or mistyped field is a protocol error, and the
 * connection closes.
 */
final class Link {
    static final int VERSION = 1;
    /** Longest line in bytes, newline included. */
    static final int MAX_LINE = 4096;
    /** Colours in a frame, as LEDs on the Dot's ring. */
    static final int SEGMENTS = 12;

    private Link() {}

    static final class ProtocolException extends Exception {
        ProtocolException(String message) {
            super(message);
        }
    }

    /** The firmware's link to the controller. */
    enum State {
        UP("up"),
        DOWN("down"),
        PENDING("pending");

        final String wire;

        State(String wire) {
            this.wire = wire;
        }
    }

    /** Where touches act: nowhere, or the bottom band (decision 9). */
    enum Zone {
        NONE("none"),
        CANCEL("cancel"),
        STOP("stop");

        final String wire;

        Zone(String wire) {
            this.wire = wire;
        }
    }

    /** A firmware → app message. */
    abstract static class Message {
        private Message() {}
    }

    static final class Hello extends Message {
        final long link;
        /** The firmware's fw- version and its embedded cmp- version; null when link is not VERSION. */
        final String firmware;
        final String companion;

        Hello(long link, String firmware, String companion) {
            this.link = link;
            this.firmware = firmware;
            this.companion = companion;
        }
    }

    static final class Frame extends Message {
        /** 0xRRGGBB per segment, index 0 at the left. */
        final int[] rgb;

        Frame(int[] rgb) {
            this.rgb = rgb;
        }

        boolean dark() {
            for (int c : rgb) {
                if (c != 0) {
                    return false;
                }
            }
            return true;
        }

        /**
         * The segments as screen colours. An LED adds light, so a segment's
         * brightest channel becomes its opacity and the hue is kept at full
         * strength: an off segment is transparent rather than a black block
         * over the kiosk, and a dim one is a faint tint.
         */
        int[] argb() {
            int[] out = new int[rgb.length];
            for (int k = 0; k < rgb.length; k++) {
                int r = rgb[k] >> 16 & 0xff, g = rgb[k] >> 8 & 0xff, b = rgb[k] & 0xff;
                int a = Math.max(r, Math.max(g, b));
                if (a != 0) {
                    out[k] = a << 24 | (r * 255 / a) << 16 | (g * 255 / a) << 8 | b * 255 / a;
                }
            }
            return out;
        }
    }

    static final class Status extends Message {
        final State link;
        final String reason;
        final String deviceId;
        /** host:port, or null until the firmware has reached a controller. */
        final String controller;

        Status(State link, String reason, String deviceId, String controller) {
            this.link = link;
            this.reason = reason;
            this.deviceId = deviceId;
            this.controller = controller;
        }
    }

    static final class Touch extends Message {
        final Zone zone;

        Touch(Zone zone) {
            this.zone = zone;
        }
    }

    /** {@code wake: true} lights the screen; {@code hold} keeps it lit, or lets it go. */
    static final class Screen extends Message {
        final boolean wake;
        /** null when the message does not carry it. */
        final Boolean hold;

        Screen(boolean wake, Boolean hold) {
            this.wake = wake;
            this.hold = hold;
        }
    }

    /** Decodes one line; null for a type this version does not know. */
    static Message decode(String line) throws ProtocolException {
        Map<String, Object> m;
        try {
            m = Json.parseObject(line);
        } catch (Json.SyntaxException e) {
            throw new ProtocolException("malformed line: " + e.getMessage());
        }
        String t = string(m, "t");
        switch (t) {
            case "hello": {
                long link = integer(m, "link");
                if (link != VERSION) {
                    return new Hello(link, null, null);
                }
                return new Hello(link, string(m, "firmware"), string(m, "companion"));
            }
            case "frame":
                return new Frame(rgb(string(m, "rgb")));
            case "status":
                return new Status(state(string(m, "link")), optString(m, "reason"),
                        optString(m, "device_id"), optString(m, "controller"));
            case "touch":
                return new Touch(zone(string(m, "zone")));
            case "screen": {
                Boolean wake = optBoolean(m, "wake");
                Boolean hold = optBoolean(m, "hold");
                if (wake == null && hold == null) {
                    throw new ProtocolException("screen: neither wake nor hold");
                }
                return new Screen(Boolean.TRUE.equals(wake), hold);
            }
            default:
                return null;
        }
    }

    /** The app's hello, the first line on every connection. */
    static String hello(String companion, boolean overlay, boolean screenOn) {
        return new Json.Writer().put("t", "hello").put("link", VERSION).put("companion", companion)
                .put("overlay", overlay).put("screen_on", screenOn).end();
    }

    static String pressDown() {
        return new Json.Writer().put("t", "press").put("down", true).end();
    }

    /** heldMs is MotionEvent eventTime - downTime; never negative on the wire. */
    static String pressUp(long heldMs) {
        return new Json.Writer().put("t", "press").put("down", false).put("held_ms", Math.max(0, heldMs)).end();
    }

    static String state(boolean overlay, boolean screenOn) {
        return new Json.Writer().put("t", "state").put("overlay", overlay).put("screen_on", screenOn).end();
    }

    /** Splits a stream into lines of at most MAX_LINE bytes, newline included. */
    static final class LineReader {
        private final InputStream in;
        private final byte[] line = new byte[MAX_LINE - 1];

        LineReader(InputStream in) {
            this.in = in;
        }

        /** The next line without its newline, or null at the end of the stream; an unterminated last line is dropped. */
        String readLine() throws IOException, ProtocolException {
            int n = 0;
            for (;;) {
                int b = in.read();
                if (b < 0) {
                    return null;
                }
                if (b == '\n') {
                    return new String(line, 0, n, StandardCharsets.UTF_8);
                }
                if (n == line.length) {
                    throw new ProtocolException("line longer than " + MAX_LINE + " bytes");
                }
                line[n++] = (byte) b;
            }
        }
    }

    private static int[] rgb(String hex) throws ProtocolException {
        if (hex.length() != SEGMENTS * 6) {
            throw new ProtocolException("frame: rgb is " + hex.length() + " characters, not " + SEGMENTS * 6);
        }
        int[] out = new int[SEGMENTS];
        for (int k = 0; k < hex.length(); k++) {
            int d = Character.digit(hex.charAt(k), 16);
            if (d < 0) {
                throw new ProtocolException("frame: rgb is not hex");
            }
            out[k / 6] = out[k / 6] << 4 | d;
        }
        return out;
    }

    private static State state(String wire) throws ProtocolException {
        for (State s : State.values()) {
            if (s.wire.equals(wire)) {
                return s;
            }
        }
        throw new ProtocolException("status: unknown link " + wire);
    }

    private static Zone zone(String wire) throws ProtocolException {
        for (Zone z : Zone.values()) {
            if (z.wire.equals(wire)) {
                return z;
            }
        }
        throw new ProtocolException("touch: unknown zone " + wire);
    }

    private static String string(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (!(v instanceof String)) {
            throw new ProtocolException("field " + key + " is not a string");
        }
        return (String) v;
    }

    /** A string, null or absent; absent reads as null. */
    private static String optString(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (v == null || v == Json.NULL) {
            return null;
        }
        if (!(v instanceof String)) {
            throw new ProtocolException("field " + key + " is not a string");
        }
        return (String) v;
    }

    private static Boolean optBoolean(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (v == null) {
            return null;
        }
        if (!(v instanceof Boolean)) {
            throw new ProtocolException("field " + key + " is not a boolean");
        }
        return (Boolean) v;
    }

    private static long integer(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (!(v instanceof Long)) {
            throw new ProtocolException("field " + key + " is not an integer");
        }
        return (Long) v;
    }
}
