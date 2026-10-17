package slspeaker

import "testing"

const (
	ms       = int64(1_000_000)
	rate     = 48000
	latency  = 60 * ms
	writeLen = 2048
)

// track synthesises AAudio's output side: frame f is presented at
// t0 + f/48 kHz once the stream runs steadily.
type track struct{ t0 int64 }

func (tr track) presented(frame int64) int64 { return tr.t0 + frame*1_000_000_000/rate }

// With a steady stream the estimate is the presentation time of the
// write's last frame plus the latency, from the pairs; the first pair after
// start (72–96 ms off) is never used.
func TestAAClockUsesPresentationAfterTheFirstPair(t *testing.T) {
	tr := track{t0: 10_000 * ms}
	c := newAAClock(rate, latency)
	// First write: the only pair is the misplaced first one, 90 ms early;
	// the estimate falls back to the consumption bound.
	handoff := tr.presented(0)
	got := c.end(handoff, writeLen, 0, 0, tr.presented(0)-90*ms, true)
	if want := handoff + writeLen*1_000_000_000/rate + latency; got != want {
		t.Fatalf("first write: %d, want the bound %d", got, want)
	}
	// Later writes: pairs on the line, the server consuming one write
	// ahead of presentation and two queued behind it.
	for w := int64(3); w <= 50; w++ {
		end := w * writeLen
		pos := end - 2*writeLen
		handoff := tr.presented(pos) + ms
		got := c.end(handoff, end, pos+writeLen, pos, tr.presented(pos), true)
		if want := tr.presented(end) + latency; got < want-1000 || got > want+1000 { // ns rounding
			t.Fatalf("write %d: %d, want presented end + latency %d", w, got, want)
		}
	}
}

// While the writer stalls the stream underruns: presentation stops and the
// last pair goes stale. A write after the stall cannot be heard before its
// unconsumed frames have played from the handoff, plus the latency.
func TestAAClockStaleAfterUnderrun(t *testing.T) {
	tr := track{t0: 10_000 * ms}
	c := newAAClock(rate, latency)
	c.end(tr.presented(0), writeLen, 0, 0, tr.presented(0), true)
	c.end(tr.presented(writeLen), 2*writeLen, writeLen, writeLen, tr.presented(writeLen), true)
	// 2.5 s later: everything written has been consumed and presented;
	// AAudio still reports the pair from before the stall.
	handoff := tr.presented(2*writeLen) + 2500*ms
	got := c.end(handoff, 3*writeLen, 2*writeLen, writeLen, tr.presented(writeLen), true)
	if want := handoff + writeLen*1_000_000_000/rate + latency; got != want {
		t.Fatalf("after the stall: %d, want the bound %d", got, want)
	}
}

// Estimates never step back, whatever the pairs do.
func TestAAClockMonotonic(t *testing.T) {
	tr := track{t0: 10_000 * ms}
	c := newAAClock(rate, latency)
	c.end(tr.presented(0), writeLen, 0, 0, tr.presented(0), true)
	a := c.end(tr.presented(writeLen), 2*writeLen, writeLen, writeLen, tr.presented(writeLen), true)
	// A pair 50 ms earlier than the line, and no pair at all.
	b := c.end(tr.presented(writeLen)+ms, 3*writeLen, writeLen, 2*writeLen, tr.presented(2*writeLen)-200*ms, true)
	d := c.end(tr.presented(writeLen)+2*ms, 3*writeLen, 3*writeLen, 0, 0, false)
	if b < a || d < b {
		t.Fatalf("estimates stepped back: %d, %d, %d", a, b, d)
	}
}
