package org.echomuse.ksplugin;

import java.util.ArrayList;
import java.util.List;

import org.echomuse.companion.Link;

/**
 * Tests of the turn view's show and hide rules, run on the JDK by the image's
 * companion stage (no JUnit: a failure prints and exits nonzero, failing the
 * build).
 */
public final class TurnViewTest {
    private static final String A = "turn-a";
    private static final String B = "turn-b";
    private static final String HEARD = "What's the weather";
    private static final String ANSWER = "Sunny and 21 degrees.";

    private static final List<String> failures = new ArrayList<>();
    private static int checks;

    private interface Case {
        void run();
    }

    public static void main(String[] args) {
        run("a turn shows from listening and lingers after done", TurnViewTest::phases);
        run("linger from config", TurnViewTest::linger);
        run("linger 0 hides at done", TurnViewTest::lingerZero);
        run("a new turn replaces the one on screen", TurnViewTest::replacement);
        run("follow-up stays up into the next turn", TurnViewTest::followUp);
        run("follow-up waits at most its timeout", TurnViewTest::followUpTimeout);
        run("done with nothing heard hides at once", TurnViewTest::noInput);
        run("done for a turn not on screen is ignored", TurnViewTest::doneUnknown);
        run("a replayed done does not extend the linger", TurnViewTest::replayedDone);
        run("link loss hides after the linger", TurnViewTest::linkLoss);
        run("a turn without done is hidden by the safety timeout", TurnViewTest::stuck);

        if (!failures.isEmpty()) {
            for (String f : failures) {
                System.out.println("FAIL " + f);
            }
            System.out.println(failures.size() + " of the turn view tests failed");
            System.exit(1);
        }
        System.out.println("turn view tests: all passed (" + checks + " checks)");
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

    private static void phases() {
        TurnView s = new TurnView();
        eq(null, s.shown());
        eq(TurnView.NEVER, s.hideAt());
        s.view(view(A, Link.Phase.LISTENING, null, null, false), 1000);
        shows(s, A, Link.Phase.LISTENING);
        s.view(view(A, Link.Phase.THINKING, HEARD, null, false), 3000);
        shows(s, A, Link.Phase.THINKING);
        eq(HEARD, s.shown().heard);
        s.view(view(A, Link.Phase.ANSWERING, HEARD, "Sunny", false), 4000);
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 4100);
        eq(ANSWER, s.shown().answer);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 9000);
        // done keeps the text up for the default 6 s, then hides.
        shows(s, A, Link.Phase.DONE);
        eq(ANSWER, s.shown().answer);
        eq(15000L, s.hideAt());
        s.tick(14999);
        shows(s, A, Link.Phase.DONE);
        s.tick(15000);
        eq(null, s.shown());
        eq(TurnView.NEVER, s.hideAt());
    }

    private static void linger() {
        TurnView s = new TurnView();
        s.linger(2.5);
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 1000);
        eq(3500L, s.hideAt());
        // A change applies from the next done, not to the countdown running.
        s.linger(30);
        eq(3500L, s.hideAt());
        s.view(view(B, Link.Phase.ANSWERING, HEARD, ANSWER, false), 2000);
        s.view(view(B, Link.Phase.DONE, HEARD, ANSWER, false), 2000);
        eq(32000L, s.hideAt());
    }

    private static void lingerZero() {
        TurnView s = new TurnView();
        s.linger(0);
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 1000);
        eq(null, s.shown());
        eq(TurnView.NEVER, s.hideAt());
    }

    private static void replacement() {
        TurnView s = new TurnView();
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(B, Link.Phase.LISTENING, null, null, false), 500);
        shows(s, B, Link.Phase.LISTENING);
        eq(null, s.shown().answer);
        // A's late done does not touch B's view.
        long deadline = s.hideAt();
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 600);
        shows(s, B, Link.Phase.LISTENING);
        eq(deadline, s.hideAt());
        // A new turn also replaces one that is lingering after its done.
        s.view(view(B, Link.Phase.DONE, HEARD, ANSWER, false), 1000);
        s.view(view(A, Link.Phase.LISTENING, null, null, false), 2000);
        shows(s, A, Link.Phase.LISTENING);
        s.tick(7000); // B's linger has passed; A is in progress
        shows(s, A, Link.Phase.LISTENING);
    }

    private static void followUp() {
        TurnView s = new TurnView();
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, true), 1000);
        shows(s, A, Link.Phase.DONE);
        s.tick(1000 + TurnView.DEFAULT_LINGER_MS);
        shows(s, A, Link.Phase.DONE); // past the plain linger: still up
        s.view(view(B, Link.Phase.LISTENING, null, null, false), 9000);
        shows(s, B, Link.Phase.LISTENING);
        // The follow-up turn ends as any turn does.
        s.view(view(B, Link.Phase.DONE, "And tomorrow", "Rain.", false), 12000);
        eq(18000L, s.hideAt());
    }

    private static void followUpTimeout() {
        TurnView s = new TurnView();
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, true), 0); // not shown: ignored
        eq(null, s.shown());
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, true), 1000);
        eq(1000 + TurnView.FOLLOW_UP_MS, s.hideAt());
        s.tick(1000 + TurnView.FOLLOW_UP_MS);
        eq(null, s.shown());
        // A linger longer than the follow-up wait wins.
        s.linger(40);
        s.view(view(B, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(B, Link.Phase.DONE, HEARD, ANSWER, true), 1000);
        eq(41000L, s.hideAt());
    }

    private static void noInput() {
        TurnView s = new TurnView();
        s.view(view(A, Link.Phase.LISTENING, null, null, false), 0);
        s.view(view(A, Link.Phase.DONE, null, null, false), 8000);
        eq(null, s.shown());
        eq(TurnView.NEVER, s.hideAt());
        // Even when HA asked to continue: nothing to show, nothing to wait for.
        s.view(view(B, Link.Phase.LISTENING, null, null, false), 9000);
        s.view(view(B, Link.Phase.DONE, null, null, true), 10000);
        eq(null, s.shown());
        // A cancelled turn after STT keeps its request up for the linger.
        s.view(view(A, Link.Phase.THINKING, HEARD, null, false), 11000);
        s.view(view(A, Link.Phase.DONE, HEARD, null, false), 12000);
        eq(HEARD, s.shown().heard);
        eq(null, s.shown().answer);
        eq(18000L, s.hideAt());
    }

    private static void doneUnknown() {
        TurnView s = new TurnView();
        // The firmware replays its last view after the plugin's hello: a done
        // for a turn long gone must not bring it back.
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 0);
        eq(null, s.shown());
        eq(TurnView.NEVER, s.hideAt());
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 1000);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 2000);
        s.tick(8000);
        eq(null, s.shown());
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 9000);
        eq(null, s.shown());
    }

    private static void replayedDone() {
        TurnView s = new TurnView();
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 1000);
        s.linkLost(3000);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, false), 4000);
        eq(7000L, s.hideAt());
        s.tick(7000);
        eq(null, s.shown());
    }

    private static void linkLoss() {
        TurnView s = new TurnView();
        s.linkLost(0); // nothing shown: nothing to do
        eq(null, s.shown());
        eq(TurnView.NEVER, s.hideAt());

        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 0);
        s.linkLost(2000);
        shows(s, A, Link.Phase.ANSWERING);
        eq(8000L, s.hideAt());
        s.tick(8000);
        eq(null, s.shown());

        // Lost during a follow-up wait: the linger, if sooner.
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 10000);
        s.view(view(A, Link.Phase.DONE, HEARD, ANSWER, true), 11000);
        s.linkLost(12000);
        eq(18000L, s.hideAt());

        // Lost near the end of a linger: the earlier deadline stands.
        s.view(view(B, Link.Phase.ANSWERING, HEARD, ANSWER, false), 20000);
        s.view(view(B, Link.Phase.DONE, HEARD, ANSWER, false), 21000);
        s.linkLost(26000);
        eq(27000L, s.hideAt());

        // Reconnected mid-turn: the replayed view brings it back.
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 30000);
        s.linkLost(31000);
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, false), 32000);
        eq(32000 + TurnView.STUCK_MS, s.hideAt());

        // With linger 0, at once.
        s.linger(0);
        s.linkLost(33000);
        eq(null, s.shown());
    }

    private static void stuck() {
        TurnView s = new TurnView();
        s.view(view(A, Link.Phase.LISTENING, null, null, false), 0);
        s.view(view(A, Link.Phase.ANSWERING, HEARD, ANSWER, true), 100_000);
        s.tick(TurnView.STUCK_MS);
        shows(s, A, Link.Phase.ANSWERING); // every view restarts it
        s.tick(100_000 + TurnView.STUCK_MS);
        eq(null, s.shown());
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static Link.View view(String turn, Link.Phase phase, String heard, String answer, boolean followUp) {
        return new Link.View(turn, phase, heard, answer, false, followUp);
    }

    private static void shows(TurnView s, String turn, Link.Phase phase) {
        checks++;
        Link.View v = s.shown();
        if (v == null) {
            throw new AssertionError("hidden, want " + turn + " " + phase);
        }
        if (!v.turnId.equals(turn) || v.phase != phase) {
            throw new AssertionError("want " + turn + " " + phase + " got " + v.turnId + " " + v.phase);
        }
    }

    private static void eq(Object want, Object got) {
        checks++;
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError("want <" + want + "> got <" + got + ">");
        }
    }
}
