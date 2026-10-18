package org.echomuse.companion;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;

/**
 * The local link, version 2 (docs/echo-show-companion-plan.md §3 and §11.3):
 * one JSON object per line between the firmware on 127.0.0.1:8771 and its two
 * clients, this app and the Kiosk Satellite plugin, the type in field
 * {@code t}. Pure Java, so the image build tests it on the JDK; the plugin
 * (org.echomuse.ksplugin) compiles this same codec in, hence the public parts.
 *
 * <p>Unknown types are ignored, for a newer firmware; a malformed line or a
 * known type with a missing or mistyped field is a protocol error, and the
 * connection closes.
 */
public final class Link {
    public static final int VERSION = 2;
    /** Longest line in bytes, newline included (decision 6 of §11.2). */
    public static final int MAX_LINE = 131072;
    /** Colours in a frame, as LEDs on the Dot's ring. */
    public static final int SEGMENTS = 12;

    private Link() {}

    public static final class ProtocolException extends Exception {
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

    /** A firmware → app or firmware → plugin message. */
    public abstract static class Message {
        private Message() {}
    }

    /**
     * The firmware's reply to a hello. It carries the embedded version of the
     * client's role: {@code companion} for the app, {@code plugin} for the
     * plugin, the other one null. Every field but link is null when link is
     * not VERSION.
     */
    public static final class Hello extends Message {
        public final long link;
        public final String firmware;
        public final String companion;
        public final String plugin;

        Hello(long link, String firmware, String companion, String plugin) {
            this.link = link;
            this.firmware = firmware;
            this.companion = companion;
            this.plugin = plugin;
        }
    }

    public static final class Frame extends Message {
        /** 0xRRGGBB per segment, index 0 at the left. */
        public final int[] rgb;

        Frame(int[] rgb) {
            this.rgb = rgb;
        }

        public boolean dark() {
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
        public int[] argb() {
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

    /** A turn's phase in the controller's turn.view (decision 8 of §11.2). */
    public enum Phase {
        LISTENING("listening"),
        THINKING("thinking"),
        ANSWERING("answering"),
        DONE("done");

        final String wire;

        Phase(String wire) {
            this.wire = wire;
        }
    }

    /** The controller's turn.view, relayed whole by the firmware; never a delta. */
    public static final class View extends Message {
        public final String turnId;
        public final Phase phase;
        /** The request as sent to HA; null until STT ends. */
        public final String heard;
        /** The answer so far; null until the first text. */
        public final String answer;
        /** The streamed text has been replaced by intent-end's final speech. */
        public final boolean isFinal;
        /** HA continues the conversation: the next turn follows this one. */
        public final boolean followUp;

        public View(String turnId, Phase phase, String heard, String answer, boolean isFinal, boolean followUp) {
            this.turnId = turnId;
            this.phase = phase;
            this.heard = heard;
            this.answer = answer;
            this.isFinal = isFinal;
            this.followUp = followUp;
        }
    }

    /** One of the speaker's HA timers in the controller's timer.view. */
    public static final class Timer {
        /** HA's timer id. */
        public final String id;
        /** The name HA gave the timer, or null for none. */
        public final String name;
        public final long totalMs;
        /** Remaining when the controller sent the list. */
        public final long remainingMs;
        /** False while paused: remainingMs is then frozen. */
        public final boolean active;

        public Timer(String id, String name, long totalMs, long remainingMs, boolean active) {
            this.id = id;
            this.name = name;
            this.totalMs = totalMs;
            this.remainingMs = remainingMs;
            this.active = active;
        }
    }

    /**
     * The controller's timer.view, relayed whole by the firmware; never a
     * delta. Active timers first, then paused ones, each soonest first.
     */
    public static final class Timers extends Message {
        /** Unmodifiable; empty when the speaker has no timer. */
        public final List<Timer> timers;

        public Timers(List<Timer> timers) {
            this.timers = Collections.unmodifiableList(new ArrayList<>(timers));
        }
    }

    /** The plugin's settings from the device config. */
    public static final class Config extends Message {
        /** How long a finished turn stays on screen, in seconds; 0 hides it at once. */
        public final double turnViewLinger;

        Config(double turnViewLinger) {
            this.turnViewLinger = turnViewLinger;
        }
    }

    /** Decodes one line; null for a type this version does not know. */
    public static Message decode(String line) throws ProtocolException {
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
                    return new Hello(link, null, null, null);
                }
                String companion = optString(m, "companion");
                String plugin = optString(m, "plugin");
                if (companion == null && plugin == null) {
                    throw new ProtocolException("hello: neither companion nor plugin");
                }
                return new Hello(link, string(m, "firmware"), companion, plugin);
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
            case "view":
                return new View(string(m, "turn_id"), phase(string(m, "phase")), optString(m, "heard"),
                        optString(m, "answer"), bool(m, "final"), bool(m, "follow_up"));
            case "timers":
                return new Timers(timers(m.get("timers")));
            case "config": {
                double linger = number(m, "turn_view_linger");
                if (linger < 0) {
                    throw new ProtocolException("config: turn_view_linger is negative");
                }
                return new Config(linger);
            }
            default:
                return null;
        }
    }

    /** The app's hello, the first line on every connection. */
    static String hello(String companion, boolean overlay, boolean screenOn) {
        return new Json.Writer().put("t", "hello").put("link", VERSION).put("role", "app")
                .put("companion", companion).put("overlay", overlay).put("screen_on", screenOn).end();
    }

    /** The plugin's hello, the first and only line it sends. */
    public static String pluginHello(String plugin) {
        return new Json.Writer().put("t", "hello").put("link", VERSION).put("role", "plugin")
                .put("plugin", plugin).end();
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
    public static final class LineReader {
        private final InputStream in;
        private final byte[] line = new byte[MAX_LINE - 1];

        public LineReader(InputStream in) {
            this.in = in;
        }

        /** The next line without its newline, or null at the end of the stream; an unterminated last line is dropped. */
        public String readLine() throws IOException, ProtocolException {
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

    private static Phase phase(String wire) throws ProtocolException {
        for (Phase p : Phase.values()) {
            if (p.wire.equals(wire)) {
                return p;
            }
        }
        throw new ProtocolException("view: unknown phase " + wire);
    }

    private static List<Timer> timers(Object v) throws ProtocolException {
        if (!(v instanceof List)) {
            throw new ProtocolException("timers: timers is not an array");
        }
        List<Timer> out = new ArrayList<>();
        for (Object o : (List<?>) v) {
            if (!(o instanceof Map)) {
                throw new ProtocolException("timers: a timer is not an object");
            }
            @SuppressWarnings("unchecked")
            Map<String, Object> t = (Map<String, Object>) o;
            long total = integer(t, "total_ms");
            long remaining = integer(t, "remaining_ms");
            if (total < 0 || remaining < 0) {
                throw new ProtocolException("timers: total_ms or remaining_ms is negative");
            }
            out.add(new Timer(string(t, "id"), optString(t, "name"), total, remaining, bool(t, "active")));
        }
        return out;
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

    private static boolean bool(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (!(v instanceof Boolean)) {
            throw new ProtocolException("field " + key + " is not a boolean");
        }
        return (Boolean) v;
    }

    private static double number(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (!(v instanceof Long) && !(v instanceof Double)) {
            throw new ProtocolException("field " + key + " is not a number");
        }
        return ((Number) v).doubleValue();
    }

    private static long integer(Map<String, Object> m, String key) throws ProtocolException {
        Object v = m.get(key);
        if (!(v instanceof Long)) {
            throw new ProtocolException("field " + key + " is not an integer");
        }
        return (Long) v;
    }
}
