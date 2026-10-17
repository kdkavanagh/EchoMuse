package org.echomuse.companion;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

/**
 * Tests of the link codec and the status text, run on the JDK by the image's
 * companion stage (no JUnit: a failure prints and exits nonzero, failing the
 * build).
 */
public final class LinkTest {
    private static final String CMP = "cmp-0123456789ab";
    private static final String FW = "fw-ba9876543210";
    private static final String BLACK = repeat("000000", 12);

    private static final List<String> failures = new ArrayList<>();
    private static int checks;

    private interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        run("encode app hello", LinkTest::encodeHello);
        run("encode press", LinkTest::encodePress);
        run("encode state", LinkTest::encodeState);
        run("encode escapes strings", LinkTest::encodeEscapes);
        run("decode firmware hello", LinkTest::decodeHello);
        run("decode hello of another link version", LinkTest::decodeHelloMismatch);
        run("decode frame", LinkTest::decodeFrame);
        run("decode frame rejects bad rgb", LinkTest::decodeFrameBad);
        run("frame dark and argb", LinkTest::frameColours);
        run("decode status", LinkTest::decodeStatus);
        run("decode status rejects bad fields", LinkTest::decodeStatusBad);
        run("decode touch", LinkTest::decodeTouch);
        run("decode screen", LinkTest::decodeScreen);
        run("unknown type is ignored", LinkTest::unknownType);
        run("malformed lines are protocol errors", LinkTest::malformed);
        run("json values", LinkTest::jsonValues);
        run("line reader splits and caps lines", LinkTest::lineReader);
        run("status line per link state", LinkTest::statusLine);
        run("overlay text", LinkTest::overlayText);
        run("notification title and versions", LinkTest::notification);
        run("next change", LinkTest::nextChange);

        if (!failures.isEmpty()) {
            for (String f : failures) {
                System.out.println("FAIL " + f);
            }
            System.out.println(failures.size() + " of the companion link tests failed");
            System.exit(1);
        }
        System.out.println("companion link tests: all passed (" + checks + " checks)");
    }

    private static void run(String name, Case c) {
        int before = failures.size();
        try {
            c.run();
        } catch (Exception | AssertionError e) {
            failures.add(name + ": " + e);
        }
        System.out.println((failures.size() == before ? "ok   " : "FAIL ") + name);
    }

    // ── app → firmware ──────────────────────────────────────────────────────

    private static void encodeHello() {
        eq("{\"t\":\"hello\",\"link\":1,\"companion\":\"" + CMP + "\",\"overlay\":true,\"screen_on\":false}",
                Link.hello(CMP, true, false));
    }

    private static void encodePress() {
        eq("{\"t\":\"press\",\"down\":true}", Link.pressDown());
        eq("{\"t\":\"press\",\"down\":false,\"held_ms\":812}", Link.pressUp(812));
        eq("{\"t\":\"press\",\"down\":false,\"held_ms\":0}", Link.pressUp(-3));
    }

    private static void encodeState() {
        eq("{\"t\":\"state\",\"overlay\":false,\"screen_on\":true}", Link.state(false, true));
    }

    private static void encodeEscapes() throws Exception {
        String odd = "a\"b\\c\nd\u0001é";
        String line = Link.hello(odd, false, false);
        eq("{\"t\":\"hello\",\"link\":1,\"companion\":\"a\\\"b\\\\c\\nd\\u0001é\",\"overlay\":false,\"screen_on\":false}", line);
        eq(odd, Json.parseObject(line).get("companion"));
    }

    // ── firmware → app ──────────────────────────────────────────────────────

    private static void decodeHello() throws Exception {
        Link.Hello h = (Link.Hello) Link.decode("{\"t\":\"hello\",\"link\":1,\"firmware\":\"" + FW + "\",\"companion\":\"" + CMP + "\"}");
        eq(1L, h.link);
        eq(FW, h.firmware);
        eq(CMP, h.companion);
        // Field order and whitespace do not matter; extra fields are ignored.
        h = (Link.Hello) Link.decode(" { \"companion\" : \"" + CMP + "\", \"future\": [1, {\"x\": null}], \"firmware\":\"" + FW + "\",\"link\":1,\"t\":\"hello\" } ");
        eq(FW, h.firmware);
        protocolError("{\"t\":\"hello\",\"link\":1,\"companion\":\"" + CMP + "\"}");
        protocolError("{\"t\":\"hello\",\"link\":\"1\",\"firmware\":\"" + FW + "\",\"companion\":\"" + CMP + "\"}");
        protocolError("{\"t\":\"hello\",\"link\":1.5,\"firmware\":\"" + FW + "\",\"companion\":\"" + CMP + "\"}");
    }

    private static void decodeHelloMismatch() throws Exception {
        // Another version's hello may carry other fields: it decodes, and the
        // service closes on the version alone.
        Link.Hello h = (Link.Hello) Link.decode("{\"t\":\"hello\",\"link\":2}");
        eq(2L, h.link);
        eq(null, h.firmware);
    }

    private static void decodeFrame() throws Exception {
        String rgb = "ff0000" + "00ff00" + "0000ff" + "ABCDEF" + repeat("000000", 7) + "010203";
        Link.Frame f = (Link.Frame) Link.decode("{\"t\":\"frame\",\"rgb\":\"" + rgb + "\"}");
        eq(12, f.rgb.length);
        eq(0xff0000, f.rgb[0]);
        eq(0x00ff00, f.rgb[1]);
        eq(0x0000ff, f.rgb[2]);
        eq(0xabcdef, f.rgb[3]);
        eq(0, f.rgb[4]);
        eq(0x010203, f.rgb[11]);
    }

    private static void decodeFrameBad() {
        protocolError("{\"t\":\"frame\",\"rgb\":\"" + repeat("000000", 11) + "\"}");
        protocolError("{\"t\":\"frame\",\"rgb\":\"" + repeat("000000", 12) + "0\"}");
        protocolError("{\"t\":\"frame\",\"rgb\":\"" + repeat("000000", 11) + "00000g\"}");
        protocolError("{\"t\":\"frame\",\"rgb\":null}");
        protocolError("{\"t\":\"frame\"}");
    }

    private static void frameColours() throws Exception {
        Link.Frame black = (Link.Frame) Link.decode("{\"t\":\"frame\",\"rgb\":\"" + BLACK + "\"}");
        check(black.dark(), "all-black frame is dark");
        eq(0, black.argb()[0]);
        Link.Frame f = (Link.Frame) Link.decode("{\"t\":\"frame\",\"rgb\":\"" + "ffffff" + "400000" + "202040" + repeat("000000", 9) + "\"}");
        check(!f.dark(), "lit frame is not dark");
        int[] c = f.argb();
        eq(0xffffffff, c[0]);
        eq(0x40ff0000, c[1]); // dim red: faint, full-strength red
        eq(0x407f7fff, c[2]);
        eq(0, c[3]); // off: transparent
    }

    private static void decodeStatus() throws Exception {
        Link.Status s = (Link.Status) Link.decode("{\"t\":\"status\",\"link\":\"pending\",\"reason\":\"pending_approval\",\"device_id\":\"G6G16D12143203LD\",\"controller\":\"192.168.1.5:8767\"}");
        eq(Link.State.PENDING, s.link);
        eq("pending_approval", s.reason);
        eq("G6G16D12143203LD", s.deviceId);
        eq("192.168.1.5:8767", s.controller);
        s = (Link.Status) Link.decode("{\"t\":\"status\",\"link\":\"down\",\"reason\":\"starting\",\"device_id\":\"X\",\"controller\":null}");
        eq(Link.State.DOWN, s.link);
        eq(null, s.controller);
        s = (Link.Status) Link.decode("{\"t\":\"status\",\"link\":\"up\",\"reason\":null,\"device_id\":\"X\",\"controller\":\"h:1\"}");
        eq(Link.State.UP, s.link);
        eq(null, s.reason);
    }

    private static void decodeStatusBad() {
        protocolError("{\"t\":\"status\",\"link\":\"sideways\",\"reason\":null,\"device_id\":\"X\",\"controller\":null}");
        protocolError("{\"t\":\"status\",\"reason\":null,\"device_id\":\"X\",\"controller\":null}");
        protocolError("{\"t\":\"status\",\"link\":\"up\",\"reason\":5,\"device_id\":\"X\",\"controller\":null}");
        protocolError("{\"t\":\"status\",\"link\":\"up\",\"reason\":null,\"device_id\":\"X\",\"controller\":false}");
    }

    private static void decodeTouch() throws Exception {
        eq(Link.Zone.NONE, ((Link.Touch) Link.decode("{\"t\":\"touch\",\"zone\":\"none\"}")).zone);
        eq(Link.Zone.CANCEL, ((Link.Touch) Link.decode("{\"t\":\"touch\",\"zone\":\"cancel\"}")).zone);
        eq(Link.Zone.STOP, ((Link.Touch) Link.decode("{\"t\":\"touch\",\"zone\":\"stop\"}")).zone);
        protocolError("{\"t\":\"touch\",\"zone\":\"everywhere\"}");
        protocolError("{\"t\":\"touch\"}");
    }

    private static void decodeScreen() throws Exception {
        Link.Screen s = (Link.Screen) Link.decode("{\"t\":\"screen\",\"wake\":true}");
        check(s.wake, "wake");
        eq(null, s.hold);
        s = (Link.Screen) Link.decode("{\"t\":\"screen\",\"hold\":true}");
        check(!s.wake, "hold is not wake");
        eq(Boolean.TRUE, s.hold);
        s = (Link.Screen) Link.decode("{\"t\":\"screen\",\"hold\":false}");
        eq(Boolean.FALSE, s.hold);
        protocolError("{\"t\":\"screen\"}");
        protocolError("{\"t\":\"screen\",\"wake\":1}");
        protocolError("{\"t\":\"screen\",\"hold\":\"yes\"}");
    }

    private static void unknownType() throws Exception {
        eq(null, Link.decode("{\"t\":\"captions\",\"text\":\"hi\"}"));
        eq(null, Link.decode("{\"t\":\"press\",\"down\":true}")); // app → fw only
    }

    private static void malformed() {
        protocolError("");
        protocolError("not json");
        protocolError("[]");
        protocolError("{\"t\":\"touch\",\"zone\":\"none\"");
        protocolError("{\"t\":\"touch\",\"zone\":\"none\"} x");
        protocolError("{\"t\":\"touch\",\"zone\":\"none\",}");
        protocolError("{\"t\":\"touch\" \"zone\":\"none\"}");
        protocolError("{\"t\":\"touch\",\"zone\":\"no\u0001ne\"}");
        protocolError("{\"t\":\"touch\",\"zone\":\"\\x\"}");
        protocolError("{\"t\":5}");
        protocolError("{\"zone\":\"none\"}");
        protocolError("{\"t\":\"x\",\"n\":01}");
        protocolError("{\"t\":\"x\",\"n\":99999999999999999999}");
        protocolError("{\"t\":\"x\",\"n\":tru}");
        protocolError("{\"t\":\"x\",\"n\":" + repeat("[", 40) + repeat("]", 40) + "}");
    }

    private static void jsonValues() throws Exception {
        Map<String, Object> m = Json.parseObject("{\"a\":-12,\"b\":0.5e1,\"c\":\"\\u00e9\\ud83d\\ude00\\/\",\"d\":null,\"e\":[],\"f\":{}}");
        eq(-12L, m.get("a"));
        eq(5.0, m.get("b"));
        eq("é\uD83D\uDE00/", m.get("c"));
        check(m.get("d") == Json.NULL, "null is Json.NULL");
        check(m.containsKey("e") && m.containsKey("f"), "empty containers");
    }

    private static void lineReader() throws Exception {
        String max = "{\"t\":\"x\",\"p\":\"" + repeat("a", Link.MAX_LINE - 1 - 16) + "\"}";
        eq(Link.MAX_LINE - 1, max.length());
        Link.LineReader r = reader("{\"t\":\"touch\",\"zone\":\"stop\"}\n" + max + "\n\nlast");
        eq("{\"t\":\"touch\",\"zone\":\"stop\"}", r.readLine());
        eq(max, r.readLine()); // 4096 bytes with its newline: accepted
        eq("", r.readLine());
        eq(null, r.readLine()); // unterminated last line dropped
        eq(null, r.readLine());

        r = reader(max + "a\n");
        try {
            r.readLine();
            check(false, "a 4097-byte line is refused");
        } catch (Link.ProtocolException expected) {
            // ok
        }

        // UTF-8 across bytes, and the cap counts bytes, not characters.
        r = reader("{\"s\":\"é\"}\n");
        eq("{\"s\":\"é\"}", r.readLine());
        r = reader(repeat("é", Link.MAX_LINE / 2) + "\n");
        try {
            r.readLine();
            check(false, "a 4097-byte line of 2048 two-byte characters is refused");
        } catch (Link.ProtocolException expected) {
            // ok
        }
    }

    // ── status text ─────────────────────────────────────────────────────────

    private static Link.Status status(Link.State link, String reason, String controller) {
        return new Link.Status(link, reason, "G6G16D12143203LD", controller);
    }

    private static void statusLine() {
        eq("Waiting for approval · G6G16D12143203LD", StatusText.line(status(Link.State.PENDING, "pending_approval", "h:1")));
        eq("Waiting for approval", StatusText.line(new Link.Status(Link.State.PENDING, null, null, null)));
        eq("Connected · 192.168.1.5:8767", StatusText.line(status(Link.State.UP, null, "192.168.1.5:8767")));
        eq("Looking for the EchoMuse controller", StatusText.line(status(Link.State.DOWN, "starting", null)));
        eq("Lost the controller · h:1", StatusText.line(status(Link.State.DOWN, "closed", "h:1")));
        eq("Lost the controller · h:1", StatusText.line(status(Link.State.DOWN, "session_lost", "h:1")));
        eq("Not authorised by the controller · h:1", StatusText.line(status(Link.State.DOWN, "unauthorized", "h:1")));
        eq("Controller link error · h:1", StatusText.line(status(Link.State.DOWN, "protocol", "h:1")));
        eq("Not connected · h:1 · weather", StatusText.line(status(Link.State.DOWN, "weather", "h:1")));
        eq("Not connected", StatusText.line(status(Link.State.DOWN, null, null)));
    }

    private static void overlayText() {
        Link.Status pending = status(Link.State.PENDING, "pending_approval", "h:1");
        Link.Status down = status(Link.State.DOWN, "closed", "h:1");
        Link.Status up = status(Link.State.UP, null, "h:1");
        // Pending shows at once; down only after 10 s; up never.
        eq("Waiting for approval · G6G16D12143203LD", StatusText.overlay(pending, -1, 0));
        eq(null, StatusText.overlay(down, -1, 9999));
        eq("Lost the controller · h:1", StatusText.overlay(down, -1, 10000));
        eq(null, StatusText.overlay(up, -1, 0));
        eq(null, StatusText.overlay(null, -1, 0));
        // The last status stands through the local grace, then nothing.
        eq("Waiting for approval · G6G16D12143203LD", StatusText.overlay(pending, 1999, 0));
        eq(null, StatusText.overlay(pending, 2000, 0));
        eq(null, StatusText.overlay(down, 2000, 60000));
        check(!StatusText.blank(-1) && !StatusText.blank(1999) && StatusText.blank(2000), "blank after 2 s");
    }

    private static void notification() {
        eq(StatusText.CONNECTING, StatusText.title(null, 0));
        eq(StatusText.CONNECTING, StatusText.title(null, -1));
        eq("Connected · h:1", StatusText.title(status(Link.State.UP, null, "h:1"), -1));
        eq("Connected · h:1", StatusText.title(status(Link.State.UP, null, "h:1"), 1500));
        eq("EchoMuse is not running", StatusText.title(status(Link.State.UP, null, "h:1"), 2000));
        eq("EchoMuse is not running", StatusText.title(null, 5000));
        // The notification says down at once; only the overlay waits 10 s.
        eq("Lost the controller · h:1", StatusText.title(status(Link.State.DOWN, "closed", "h:1"), -1));
        eq("App " + CMP, StatusText.versions(null, CMP));
        eq("Firmware " + FW + " · App " + CMP, StatusText.versions(FW, CMP));
    }

    private static void nextChange() {
        Link.Status down = status(Link.State.DOWN, "closed", "h:1");
        eq(1500L, StatusText.nextChangeMs(null, 500, 0));
        eq(-1L, StatusText.nextChangeMs(down, 2000, 0));
        eq(7000L, StatusText.nextChangeMs(down, -1, 3000));
        eq(-1L, StatusText.nextChangeMs(down, -1, 10000));
        eq(-1L, StatusText.nextChangeMs(status(Link.State.UP, null, "h:1"), -1, 0));
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static Link.LineReader reader(String s) {
        return new Link.LineReader(new ByteArrayInputStream(s.getBytes(StandardCharsets.UTF_8)));
    }

    private static void protocolError(String line) {
        checks++;
        try {
            Link.decode(line);
        } catch (Link.ProtocolException expected) {
            return;
        }
        throw new AssertionError("no protocol error for " + line);
    }

    private static void eq(Object want, Object got) {
        checks++;
        boolean same = want == null ? got == null
                : want instanceof int[] ? Arrays.equals((int[]) want, (int[]) got) : want.equals(got);
        if (!same) {
            throw new AssertionError("want <" + want + "> got <" + got + ">");
        }
    }

    private static void check(boolean ok, String what) {
        checks++;
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    private static String repeat(String s, int n) {
        StringBuilder b = new StringBuilder();
        for (int k = 0; k < n; k++) {
            b.append(s);
        }
        return b.toString();
    }
}
