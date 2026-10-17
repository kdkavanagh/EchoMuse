package slmic

import (
	"math/rand/v2"
	"testing"
)

// halStream synthesises findings §8.4 Q6: frame f is captured at
// t0 + f/16 kHz; AAudio's pairs land 5 ms late with 15–16 ms of scatter
// (one ALSA period), and each 1,280-frame read completes 1–15 ms after its
// last frame was captured.
type halStream struct {
	t0  int64
	rng *rand.Rand
}

func (h halStream) captured(frame int64) int64 { return h.t0 + frame*sampleNs }

// pair is a timestamp AAudio would report once frames have been read.
func (h halStream) pair(frames int64) (pos, ns int64) {
	pos = frames - 1 - int64(h.rng.IntN(320))
	return pos, h.captured(pos) + 5*ms + int64(h.rng.IntN(16_000))*1000 - 8*ms
}

func (h halStream) done(frames int64) int64 {
	return h.captured(frames-1) + ms + int64(h.rng.IntN(14_000))*1000
}

// Once pairs arrive the stamps follow the true capture time of each
// period's last frame within a few ms, bias removed, though single pairs
// scatter ±8 ms; they never pass the read completion and never step back.
func TestFitClockFollowsCaptureTime(t *testing.T) {
	h := halStream{t0: 5_000 * 1_000 * ms, rng: rand.New(rand.NewPCG(1, 2))}
	c := newFitClock()
	var prev int64
	for p := int64(1); p <= 2000; p++ { // 160 s
		frames := p * periodFrames
		done := h.done(frames)
		pos, ns := h.pair(frames)
		got, kind := c.stamp(done, frames, pos, ns, true)
		if kind != stampAnchored {
			t.Fatalf("period %d: kind %d, want anchored", p, kind)
		}
		if got > done || got < prev {
			t.Fatalf("period %d: stamp %d outside [prev %d, done %d]", p, got, prev, done)
		}
		prev = got
		if p < 20 { // the fit is settling on its first pairs
			continue
		}
		if err := got - h.captured(frames-1); err < -3*ms || err > 3*ms {
			t.Fatalf("period %d: stamp off the capture time by %.1f ms", p, float64(err)/float64(ms))
		}
	}
}

// Before AAudio has a pair, a period is stamped at its read completion;
// the first pair then takes over.
func TestFitClockWithoutPairsUsesCompletion(t *testing.T) {
	h := halStream{t0: 1_000 * 1_000 * ms, rng: rand.New(rand.NewPCG(3, 4))}
	c := newFitClock()
	done := h.done(periodFrames)
	if got, kind := c.stamp(done, periodFrames, 0, 0, false); got != done || kind != stampUnlocked {
		t.Fatalf("no pair: stamp %d (%d), want completion %d", got, kind, done)
	}
	done = h.done(2 * periodFrames)
	got, kind := c.stamp(done, 2*periodFrames, 2*periodFrames-1, h.captured(2*periodFrames-1)+5*ms, true)
	if kind != stampAnchored || got != h.captured(2*periodFrames-1) {
		t.Fatalf("first pair: stamp %d (%d), want %d anchored", got, kind, h.captured(2*periodFrames-1))
	}
}

// A re-opened stream counts frames from zero: the old fit is dropped, so
// the new stream's frames are never mapped onto the old line, and the
// stamps stay monotonic across the gap.
func TestFitClockReopen(t *testing.T) {
	rng := rand.New(rand.NewPCG(5, 6))
	first := halStream{t0: 2_000 * 1_000 * ms, rng: rng}
	c := newFitClock()
	var last int64
	for p := int64(1); p <= 100; p++ {
		frames := p * periodFrames
		pos, ns := first.pair(frames)
		last, _ = c.stamp(first.done(frames), frames, pos, ns, true)
	}
	// The input dies; a new stream starts 3 s later, from frame 0.
	c.reset()
	second := halStream{t0: first.captured(100*periodFrames) + 3_000*ms, rng: rng}
	for p := int64(1); p <= 50; p++ {
		frames := p * periodFrames
		pos, ns := second.pair(frames)
		got, _ := c.stamp(second.done(frames), frames, pos, ns, true)
		if got < last {
			t.Fatalf("period %d after the re-open stepped back: %d < %d", p, got, last)
		}
		last = got
		if err := got - second.captured(frames-1); p > 1 && (err < -9*ms || err > 9*ms) {
			t.Fatalf("period %d after the re-open: off by %.1f ms", p, float64(err)/float64(ms))
		}
	}
}

// A fitted time far before the read completion, which only a stale fit
// produces, is not trusted: the period is stamped at its completion.
func TestFitClockImplausibleFit(t *testing.T) {
	c := newFitClock()
	t0 := 1_000 * 1_000 * ms
	c.stamp(t0+80*ms, periodFrames, periodFrames-1, t0+80*ms, true)
	// The reader comes back 2 s late with a pair AAudio did not refresh.
	done := t0 + 2_080*ms
	if got, kind := c.stamp(done, 2*periodFrames, periodFrames-1, t0+80*ms, true); kind != stampImplausible || got != done {
		t.Fatalf("stale fit: stamp %d (%d), want completion %d implausible", got, kind, done)
	}
}
