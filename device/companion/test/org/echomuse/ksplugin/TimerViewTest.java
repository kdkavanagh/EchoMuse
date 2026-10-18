package org.echomuse.ksplugin;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

import org.echomuse.companion.Link;

/**
 * Tests of the timer view's faces, countdown and clearing rules, run on the
 * JDK by the image's companion stage (no JUnit: a failure prints and exits
 * nonzero, failing the build).
 */
public final class TimerViewTest {
    private static final List<String> failures = new ArrayList<>();
    private static int checks;

    private interface Case {
        void run();
    }

    public static void main(String[] args) {
        run("time text", TimerViewTest::format);
        run("active timers count down between lines", TimerViewTest::countdown);
        run("a new line re-anchors", TimerViewTest::reanchor);
        run("a paused timer is frozen", TimerViewTest::paused);
        run("fraction is clamped", TimerViewTest::fraction);
        run("more than four timers share an overflow face", TimerViewTest::overflow);
        run("timers not resent are cleared after the stale guard", TimerViewTest::stale);
        run("link loss clears", TimerViewTest::linkLoss);
        run("next frame at text turnover or arc movement", TimerViewTest::nextFrame);

        if (!failures.isEmpty()) {
            for (String f : failures) {
                System.out.println("FAIL " + f);
            }
            System.out.println(failures.size() + " of the timer view tests failed");
            System.exit(1);
        }
        System.out.println("timer view tests: all passed (" + checks + " checks)");
    }

    private static void run(String name, Case c) {
        int before = failures.size();
        try {
            c.run();
        } catch (RuntimeException | AssertionError e) {
            failures.add(name + ": " + e);
        }
        System.out.println((failures.size() == before ? "ok   " : "FAIL ") + name);
    }

    // ── cases ───────────────────────────────────────────────────────────────

    private static void format() {
        eq("0:00", TimerView.format(0));
        eq("0:01", TimerView.format(1)); // rounded up: 0:00 only at zero
        eq("0:01", TimerView.format(1000));
        eq("0:09", TimerView.format(8001));
        eq("4:05", TimerView.format(245_000));
        eq("59:59", TimerView.format(3_599_000));
        eq("1:00:00", TimerView.format(3_599_001));
        eq("1:00:00", TimerView.format(3_600_000));
        eq("1:02:03", TimerView.format(3_723_000));
        eq("12:00:00", TimerView.format(43_200_000));
        eq("0:00", TimerView.format(-5));
    }

    private static void countdown() {
        TimerView s = new TimerView();
        check(s.isEmpty(), "nothing held");
        eq(0, s.faces(0).size());
        s.timers(timers(timer("a", "pasta", 300_000, 123_456, true)), 1000);
        texts(s, 1000, "2:04");
        TimerView.Face f = s.faces(1000).get(0);
        eq("pasta", f.label);
        check(!f.paused && !f.overflow, "an active timer's face");
        texts(s, 1456, "2:03");
        texts(s, 1457, "2:03");
        texts(s, 1000 + 123_455, "0:01");
        texts(s, 1000 + 123_456, "0:00");
        texts(s, 1000 + 200_000, "0:00"); // never below zero
        eq(0f, s.faces(1000 + 200_000).get(0).fraction);
    }

    private static void reanchor() {
        TimerView s = new TimerView();
        s.timers(timers(timer("a", null, 60_000, 30_000, true)), 0);
        texts(s, 900, "0:30");
        // The controller's next line, a little behind the local count: it wins.
        s.timers(timers(timer("a", null, 60_000, 29_500, true)), 1000);
        texts(s, 1000, "0:30");
        texts(s, 1500, "0:29");
        eq(null, s.faces(1500).get(0).label);
        // A list without the timer drops it.
        s.timers(timers(), 2000);
        check(s.isEmpty(), "an empty list clears");
        eq(TimerView.NEVER, s.staleAt());
    }

    private static void paused() {
        TimerView s = new TimerView();
        s.timers(timers(timer("a", "tea", 240_000, 60_000, true), timer("b", "eggs", 600_000, 245_000, false)), 0);
        TimerView.Face f = s.faces(10_000).get(1);
        eq("4:05", f.text);
        check(f.paused, "paused");
        eq(245_000f / 600_000, f.fraction);
        eq("4:05", s.faces(4000).get(1).text);
        texts(s, 4000, "0:56", "4:05");
    }

    private static void fraction() {
        TimerView s = new TimerView();
        s.timers(timers(timer("a", null, 100_000, 25_000, true), timer("b", null, 1000, 5000, false),
                timer("c", null, 0, 0, false)), 0);
        eq(0.25f, s.faces(0).get(0).fraction);
        eq(0.125f, s.faces(12_500).get(0).fraction);
        eq(1f, s.faces(0).get(1).fraction); // more left than total
        eq(0f, s.faces(0).get(2).fraction); // no total
    }

    private static void overflow() {
        TimerView s = new TimerView();
        s.timers(timers(timer("a", null, 60_000, 10_000, true), timer("b", null, 60_000, 20_000, true),
                timer("c", null, 60_000, 30_000, true), timer("d", null, 60_000, 40_000, true)), 0);
        texts(s, 0, "0:10", "0:20", "0:30", "0:40");
        s.timers(timers(timer("a", null, 60_000, 10_000, true), timer("b", null, 60_000, 20_000, true),
                timer("c", null, 60_000, 30_000, true), timer("d", null, 60_000, 40_000, true),
                timer("e", "pizza", 60_000, 50_000, false)), 0);
        texts(s, 0, "0:10", "0:20", "0:30", "+2");
        TimerView.Face f = s.faces(0).get(3);
        check(f.overflow && !f.paused, "an overflow face");
        eq(null, f.label);
        eq(0f, f.fraction);
    }

    private static void stale() {
        TimerView s = new TimerView();
        eq(TimerView.NEVER, s.staleAt());
        s.timers(timers(timer("a", null, 60_000, 30_000, true)), 1000);
        eq(6000L, s.staleAt());
        s.tick(5999);
        check(!s.isEmpty(), "held before the stale guard");
        // A new line restarts it.
        s.timers(timers(timer("a", null, 60_000, 25_000, true)), 5999);
        eq(5999 + TimerView.STALE_MS, s.staleAt());
        s.tick(10_998);
        check(!s.isEmpty(), "held 1 ms before");
        s.tick(10_999);
        check(s.isEmpty(), "cleared at exactly 5000 ms");
        eq(0, s.faces(10_999).size());
        eq(TimerView.NEVER, s.staleAt());
    }

    private static void linkLoss() {
        TimerView s = new TimerView();
        s.linkLost(); // nothing held: nothing to do
        check(s.isEmpty(), "empty");
        s.timers(timers(timer("a", null, 60_000, 30_000, true)), 0);
        s.linkLost();
        check(s.isEmpty(), "cleared on link loss");
        eq(TimerView.NEVER, s.staleAt());
        eq(TimerView.NEVER, s.nextFrameAt(0, 400));
    }

    private static void nextFrame() {
        TimerView s = new TimerView();
        eq(TimerView.NEVER, s.nextFrameAt(0, 400));
        // 5 min over 1000 px: the arc moves a pixel every 300 ms, sooner
        // than the text's turnover 456 ms on.
        s.timers(timers(timer("a", null, 300_000, 123_456, true)), 0);
        eq(300L, s.nextFrameAt(0, 1000));
        // An hour over 1000 px: 3.6 s a pixel, so the second boundary.
        s.timers(timers(timer("a", null, 3_600_000, 123_456, true)), 0);
        eq(456L, s.nextFrameAt(0, 1000));
        eq(1456L, s.nextFrameAt(456, 1000)); // on a boundary: the next one
        eq(1456L, s.nextFrameAt(457, 1000));
        // A one-second timer moves a pixel every millisecond: one frame on.
        s.timers(timers(timer("a", null, 1000, 1000, true)), 0);
        eq(TimerView.MIN_FRAME_MS, s.nextFrameAt(0, 1000));
        eq(TimerView.NEVER, s.nextFrameAt(1000, 1000)); // at zero nothing moves
        // The soonest of several.
        s.timers(timers(timer("a", null, 3_600_000, 10_700, true), timer("b", null, 3_600_000, 5_200, true)), 0);
        eq(200L, s.nextFrameAt(0, 1000));
        // Only paused timers: nothing moves.
        s.timers(timers(timer("a", null, 60_000, 30_000, false), timer("b", null, 60_000, 40_000, false)), 0);
        eq(TimerView.NEVER, s.nextFrameAt(0, 1000));
        // Timers behind the overflow face do not move it.
        s.timers(timers(timer("a", null, 60_000, 30_000, false), timer("b", null, 60_000, 40_000, false),
                timer("c", null, 60_000, 45_000, false), timer("d", null, 60_000, 50_000, true),
                timer("e", null, 60_000, 55_000, true)), 0);
        texts(s, 0, "0:30", "0:40", "0:45", "+2");
        eq(TimerView.NEVER, s.nextFrameAt(0, 1000));
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static Link.Timer timer(String id, String name, long totalMs, long remainingMs, boolean active) {
        return new Link.Timer(id, name, totalMs, remainingMs, active);
    }

    private static Link.Timers timers(Link.Timer... t) {
        return new Link.Timers(Arrays.asList(t));
    }

    private static void texts(TimerView s, long now, String... want) {
        List<String> got = new ArrayList<>();
        for (TimerView.Face f : s.faces(now)) {
            got.add(f.text);
        }
        eq(Arrays.asList(want), got);
    }

    private static void check(boolean ok, String what) {
        checks++;
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    private static void eq(Object want, Object got) {
        checks++;
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError("want <" + want + "> got <" + got + ">");
        }
    }
}
