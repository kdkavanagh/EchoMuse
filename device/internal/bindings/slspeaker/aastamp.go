package slspeaker

// aaClock estimates when each AAudio render write is heard, on the capture
// stamp scale (docs/echo-show-lineage-plan.md decision 5). AAudio's output
// getTimestamp pairs a frame position with the CLOCK_MONOTONIC time it is
// presented, so a write ending at frame endFrame is predicted to finish
// playing at
//
//	tsNs + (endFrame − pos) / rate + latencyNs
//
// where latencyNs is the rest of the way to the capture stamps: ASP's
// speaker pipeline, the amp, the air and the ASR pipeline, measured as a
// constant offset across opens (docs/echo-show-crown.md §8.4 Q6). It must
// never exceed the true figure: a late render stamp makes the mic appear to
// lead its own reference, outside the controller's 0–500 ms lag search.
//
// Two lower bounds keep the prediction honest where the pairs do not:
//   - The first pair after start is 72–96 ms off; every later one lies on a
//     single line. So no pair is used until the position has moved past
//     the first one seen.
//   - While the stream underruns (the writer stalled), the presented
//     position stops and the newest pair grows stale, which predicts too
//     early. The frames the server has not yet consumed cannot finish
//     before handoff + their duration, and are heard latencyNs after that
//     at the soonest, so the estimate never falls below it.
//
// Estimates never decrease, so completions never step backwards.
//
// It has no cgo or build tag, so host tests can use it.
type aaClock struct {
	rate      int64 // frames per second
	latencyNs int64

	firstPos  int64 // the first pair's position; -1 before any
	usable    bool  // a pair past the first has been seen
	lastEndNs int64
}

func newAAClock(rate, latencyNs int64) aaClock {
	return aaClock{rate: rate, latencyNs: latencyNs, firstPos: -1}
}

// framesNs is the playout duration of n frames.
func (c *aaClock) framesNs(n int64) int64 { return n * 1_000_000_000 / c.rate }

// end returns the estimated time the last frame of a write is heard: the
// write ends at frame endFrame (exclusive, counted from the stream's start)
// and was handed off at handoffNs, when the server had consumed consumed
// frames; pos was presented at tsNs (ok false when AAudio had no pair).
func (c *aaClock) end(handoffNs, endFrame, consumed, pos, tsNs int64, ok bool) int64 {
	if ok && pos >= 0 {
		if c.firstPos < 0 {
			c.firstPos = pos
		} else if pos != c.firstPos {
			c.usable = true
		}
	}
	end := handoffNs + c.framesNs(max(endFrame-consumed, 0)) + c.latencyNs
	if ok && c.usable {
		end = max(end, tsNs+c.framesNs(endFrame-pos)+c.latencyNs)
	}
	end = max(end, c.lastEndNs)
	c.lastEndNs = end
	return end
}
