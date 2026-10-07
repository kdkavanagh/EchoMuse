package mixerapi

// playClock estimates when audio handed to the mixer is actually heard, on
// the same CLOCK_MONOTONIC scale as Recorder's capture stamps, so render
// completions and capture periods stay comparable (SPEC §4.3; the
// controller's echo comparison searches only 0–500 ms of mic-after-reference
// lag, em_attribution.MAX_LAG).
//
// The mixer's pacing, measured on device G090LF0965260F1J with throwaway
// spikes (2026-10-06; docs/fireos6-port.md §6):
//
//   - A fresh stream accepts exactly 16 chunks (800 ms at 4,800 B/chunk)
//     without blocking; every later Get/Release round-trip blocks 30–69 ms,
//     averaging exactly one chunk (40 chunks per 2,000 ms). A blocked handoff
//     is therefore paced by the DAC itself.
//   - A 1 kHz burst at the start of a chunk appeared in micAsr capture stamps
//     26–52 ms after its handoff on the first stream after capture opened
//     (cold output path, 2 runs), 89–130 ms after it on a stream opened right
//     after another closed (6 bursts), ~113 ms after the first handoff
//     following a 2.5 s writer stall, and 782–877 ms after a handoff that
//     blocked, after ≥3 s of continuous writes (6 bursts, plus 8 more at
//     +75–84 ms against this model end to end).
//
// So: the first chunk after open or an underrun starts freshNs after its
// handoff; chunks handed off while the queue fills follow contiguously; a
// blocked handoff re-anchors to handoff + steadyNs. Re-anchoring on every
// blocked handoff tracks the DAC clock, so pure sample accounting's drift
// (tens of ppm, hundreds of ms an hour) cannot build up. Both constants sit
// at or below the smallest measured value: a stamp may be early by up to
// ~100 ms (which the lag search absorbs) but is never late, which would
// make the mic appear to lead its own reference.
type playClock struct {
	freshNs  int64 // handoff → playout start when the queue was empty
	steadyNs int64 // handoff → playout start for a handoff that blocked
	chunkNs  int64 // playout duration of one chunk
	byteRate int64 // bytes per second of the stream

	endNs  int64 // estimated playout end of the last chunk; 0 before the first
	lastNs int64 // last completion returned, for monotonicity
}

const (
	playFreshNs  = 20_000_000
	playSteadyNs = 780_000_000
	// playBlockedNs separates a handoff that waited for the mixer (≥30 ms
	// measured) from one taken from the fill allowance (≤0.03 ms measured).
	playBlockedNs = 10_000_000
)

func newPlayClock(chunkBytes int, byteRate int64) playClock {
	return playClock{
		freshNs:  playFreshNs,
		steadyNs: playSteadyNs,
		chunkNs:  int64(chunkBytes) * 1_000_000_000 / byteRate,
		byteRate: byteRate,
	}
}

// chunk records a chunk handed off at handoffNs (taken after its Release
// returned) and returns its estimated playout start. blocked reports
// whether the round-trip waited for the mixer.
func (c *playClock) chunk(handoffNs int64, blocked bool) int64 {
	var start int64
	switch {
	case blocked:
		start = handoffNs + c.steadyNs
	case c.endNs == 0 || handoffNs+c.freshNs > c.endNs:
		start = handoffNs + c.freshNs // first chunk, or the queue ran dry
	default:
		start = c.endNs
	}
	c.endNs = start + c.chunkNs
	return start
}

// completion returns the estimated playout time of the byte that ends
// bytesIntoChunk bytes into a chunk starting at startNs. Completions never
// decrease: the handoff jitter around a re-anchor must not reorder them.
func (c *playClock) completion(startNs int64, bytesIntoChunk int64) int64 {
	t := startNs + bytesIntoChunk*1_000_000_000/c.byteRate
	if t < c.lastNs {
		t = c.lastNs
	}
	c.lastNs = t
	return t
}
