package mixerapi

import "testing"

const (
	testChunkBytes = 4800  // 50 ms at 48 kHz mono S16
	testByteRate   = 96000 // 48 kHz × 2 B
	ms             = int64(1_000_000)
)

func TestPlayClockFillIsContiguousFromFreshAnchor(t *testing.T) {
	c := newPlayClock(testChunkBytes, testByteRate)
	// The fill allowance is taken in well under a millisecond: playout is
	// sample accounting from the first chunk, not handoff time.
	for k, h := range []int64{1000 * ms, 1000*ms + 30_000, 1000*ms + 60_000} {
		got := c.chunk(h, false)
		want := 1000*ms + playFreshNs + int64(k)*50*ms
		if got != want {
			t.Fatalf("chunk %d start = %d, want %d", k, got, want)
		}
	}
}

func TestPlayClockBlockedHandoffReanchorsBothWays(t *testing.T) {
	for _, tc := range []struct {
		name   string
		skewNs int64 // DAC slower (+) or faster (−) than accounting expects
	}{{"dac slow", 40 * ms}, {"dac fast", -40 * ms}} {
		c := newPlayClock(testChunkBytes, testByteRate)
		c.chunk(0, false)
		h := 10_000*ms + tc.skewNs
		if got, want := c.chunk(h, true), h+playSteadyNs; got != want {
			t.Errorf("%s: blocked start = %d, want handoff+steady %d", tc.name, got, want)
		}
	}
}

func TestPlayClockUnderrunReanchorsToFresh(t *testing.T) {
	c := newPlayClock(testChunkBytes, testByteRate)
	c.chunk(0, true) // steady: plays 780–830 ms
	// Writer stalled: the next handoff comes after the queue drained, so it
	// does not block and cannot follow the old audio contiguously.
	h := 2000 * ms
	if got, want := c.chunk(h, false), h+playFreshNs; got != want {
		t.Errorf("start after underrun = %d, want %d", got, want)
	}
	// A late but non-blocking handoff while audio is still queued follows on.
	c2 := newPlayClock(testChunkBytes, testByteRate)
	c2.chunk(0, true)
	if got, want := c2.chunk(300*ms, false), int64(playSteadyNs)+50*ms; got != want {
		t.Errorf("start with audio still queued = %d, want contiguous %d", got, want)
	}
}

func TestPlayClockCompletionOffsetAndMonotonic(t *testing.T) {
	c := newPlayClock(testChunkBytes, testByteRate)
	start := c.chunk(0, false)
	// A write ending 2,048 bytes into the chunk completes 2048/96000 s in.
	if got, want := c.completion(start, 2048), start+2048*1_000_000_000/testByteRate; got != want {
		t.Errorf("completion = %d, want %d", got, want)
	}
	end := c.completion(start, testChunkBytes)
	// A re-anchor can place the next chunk slightly before the previous one
	// ended (handoff jitter at the fill→steady transition); its completions
	// must not move backwards.
	if got := c.completion(end-20*ms, 100); got != end {
		t.Errorf("completion after an earlier re-anchor = %d, want clamped to %d", got, end)
	}
}
