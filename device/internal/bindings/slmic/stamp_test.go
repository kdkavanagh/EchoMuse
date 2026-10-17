package slmic

import (
	"math/rand/v2"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
)

const ms = int64(1_000_000)

// anchorAt is the anchor of a frame stamped tsMs (CLOCK_MONOTONIC ms, before
// the modulo) whose final bit has after samples of its period behind it.
func anchorAt(tsMs int64, after int) afe.Anchor {
	return afe.Anchor{Valid: true, TimestampMs: uint16(tsMs % afeTimestampMod), After: after}
}

func TestAnchoredStampIsTheFramesTimestamp(t *testing.T) {
	var c periodClock
	done := 1_000_000*ms + 400_000 // 1,000 s and 0.4 ms
	got, kind := c.stamp(done, anchorAt(1_000_000-18, 0))
	if want := (1_000_000 - 18) * ms; got != want || kind != stampAnchored {
		t.Fatalf("stamp = %d (%d), want %d anchored", got, kind, want)
	}
	// Samples after the frame's final bit advance the stamp at 16 kHz.
	got, _ = c.stamp(done+80*ms, anchorAt(1_000_080-18, 64))
	if want := (1_000_080-18)*ms + 64*sampleNs; got != want {
		t.Fatalf("stamp with 64 samples after = %d, want %d", got, want)
	}
}

// The unwrap takes the largest value at or before the completion that is
// congruent to AFE_TIMESTAMP, across the 65,536 ms wrap.
func TestUnwrapAcrossTheWrap(t *testing.T) {
	wrap := int64(17 * afeTimestampMod) // a wrap boundary, in ms
	for _, tc := range []struct{ doneMs, tsMs int64 }{
		{wrap + 5, wrap - 13},  // completion just past the wrap, frame before it
		{wrap, wrap - 1},       // completion on the wrap
		{wrap + 30, wrap + 2},  // both past it
		{wrap - 1, wrap - 41},  // both before it
		{wrap + 499, wrap - 1}, // the largest lag still believed
	} {
		var c periodClock
		got, kind := c.stamp(tc.doneMs*ms, anchorAt(tc.tsMs, 0))
		if got != tc.tsMs*ms || kind != stampAnchored {
			t.Errorf("done %d ms, AFE %d: stamp %d (%d), want %d anchored",
				tc.doneMs, tc.tsMs%afeTimestampMod, got, kind, tc.tsMs*ms)
		}
	}
}

// Without an accepted frame, or with one that cannot belong to this
// completion, the period is stamped at its read completion.
func TestFallsBackToReadCompletion(t *testing.T) {
	done := int64(5_000_123) * ms
	for name, tc := range map[string]struct {
		a    afe.Anchor
		kind stampKind
	}{
		"unlocked":          {afe.Anchor{}, stampUnlocked},
		"frame after done":  {anchorAt(5_000_124, 0), stampImplausible},
		"frame 501 ms back": {anchorAt(5_000_123-501, 0), stampImplausible},
	} {
		var c periodClock
		if got, kind := c.stamp(done, tc.a); got != done || kind != tc.kind {
			t.Errorf("%s: stamp %d (%d), want completion %d (%d)", name, got, kind, done, tc.kind)
		}
	}
}

// A frame 2 ms before a completion that has 127 samples (7.9 ms) after it
// would postdate the delivery: the stamp holds at the completion.
func TestNeverLaterThanCompletion(t *testing.T) {
	var c periodClock
	done := int64(9_000_000) * ms
	if got, kind := c.stamp(done, anchorAt(9_000_000-2, 127)); got != done || kind != stampAnchored {
		t.Fatalf("stamp %d (%d), want completion %d", got, kind, done)
	}
}

// A stamp never steps back, whether the anchor jumps back or the decoder
// loses lock after anchored periods.
func TestMonotonic(t *testing.T) {
	var c periodClock
	first, _ := c.stamp(100_000*ms, anchorAt(100_000-10, 0))
	// The next completion came 1 ms later (a burst) and its frame is 30 ms
	// older than the previous one's.
	if got, _ := c.stamp(100_001*ms, anchorAt(100_000-40, 0)); got != first {
		t.Fatalf("backwards anchor stamped %d, want held at %d", got, first)
	}
	// Unlocked periods fall back to completions, which are monotonic.
	if got, kind := c.stamp(100_081*ms, afe.Anchor{}); got != 100_081*ms || kind != stampUnlocked {
		t.Fatalf("unlocked stamp %d (%d)", got, kind)
	}
	// Re-locking with an anchor before the last completion stamp holds too.
	if got, _ := c.stamp(100_161*ms, anchorAt(100_161-90, 0)); got != 100_081*ms {
		t.Fatalf("re-locked stamp %d, want held at %d", got, 100_081*ms)
	}
}

// The §4.4 captures, synthesised: against a straight line through the
// completions (the fitted clock), AFE_TIMESTAMP sat 0–27 ms early, and raw
// completions scattered ±15 ms around it but trailed their frame by at
// least 7 ms. The stamps follow the line to within the AFE's own scatter,
// never pass the completion, never step back, and run straight through a
// wrap.
func TestJitteredStream(t *testing.T) {
	r := rand.New(rand.NewPCG(4, 4))
	var c periodClock
	start := int64(3*afeTimestampMod-4_000) * ms // the wrap falls 4 s in
	prev := int64(0)
	for n := int64(1); n <= 200; n++ {
		line := start + n*80*ms // the fitted completion of period n
		tsMs := line/ms - r.Int64N(28)
		done := max(line-15*ms+r.Int64N(31*ms), (tsMs+7)*ms)
		got, kind := c.stamp(done, anchorAt(tsMs, 0))
		if kind != stampAnchored {
			t.Fatalf("period %d: kind %d", n, kind)
		}
		if got > done || got < prev {
			t.Fatalf("period %d: stamp %d outside [previous %d, completion %d]", n, got, prev, done)
		}
		if d := line - got; d < 0 || d > 27*ms {
			t.Fatalf("period %d: stamp %.1f ms before the line", n, float64(d)/float64(ms))
		}
		prev = got
	}
}
