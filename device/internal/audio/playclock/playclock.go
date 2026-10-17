// Package playclock estimates when audio handed to a render queue is
// actually heard, on the same CLOCK_MONOTONIC scale as the capture stamps,
// so render completions and capture periods stay comparable (SPEC §4.3; the
// controller's echo comparison searches only 0–500 ms of
// mic-after-reference lag, em_attribution.MAX_LAG).
//
// A render binding reads, at every handoff, how many fixed-size chunks are
// still queued ahead of the new one. The queue plays in order and
// contiguously, so a chunk cannot start before
//
//   - handoff + (ahead − 1) × chunk + pipeline: the chunks in front of it
//     must play first, except that the head one may already be partly
//     consumed, and the chunk then takes at least the binding's pipeline
//     latency to reach capture stamps; nor before
//   - the previous chunk's estimated end, while the queue has not run dry.
//
// Both are lower bounds, so the larger is one too: a stamp may be early by
// up to a chunk plus pipeline variance (which the lag search absorbs) but is
// never late, which would make the mic appear to lead its own reference.
// Reading the queue on every handoff re-anchors to the hardware, so sample
// accounting's drift (tens of ppm, hundreds of ms an hour) cannot build up.
//
// Each binding owns its pipeline constant and the measurements behind it:
// internal/mixerapi (the Dot's mixer daemon) and internal/opensl (the
// Show's AudioFlinger).
//
// No cgo, no build tag: host-testable.
package playclock

// Clock is one render stream's playout estimate. It is owned by the
// goroutine that hands chunks over.
type Clock struct {
	pipelineNs int64 // queue taking a chunk → its sound in capture stamps, lower bound
	chunkNs    int64 // playout duration of one chunk
	byteRate   int64 // bytes per second of the stream

	endNs int64 // estimated playout end of the last chunk; 0 before the first
}

// New returns a clock for chunks of chunkBytes at byteRate bytes per second
// whose queue reaches capture stamps no sooner than pipelineNs after it
// takes a chunk.
func New(chunkBytes int, byteRate int64, pipelineNs int64) Clock {
	return Clock{
		pipelineNs: pipelineNs,
		chunkNs:    int64(chunkBytes) * 1_000_000_000 / byteRate,
		byteRate:   byteRate,
	}
}

// Chunk records a chunk handed off at handoffNs (taken after the queue
// accepted it) behind ahead queued chunks, and returns its estimated
// playout start. Starts never precede the previous chunk's end, so
// completions never decrease.
func (c *Clock) Chunk(handoffNs int64, ahead int) int64 {
	start := handoffNs + int64(max(ahead-1, 0))*c.chunkNs + c.pipelineNs
	if start < c.endNs {
		start = c.endNs // still playing what came before it
	}
	c.endNs = start + c.chunkNs
	return start
}

// Completion returns the estimated playout time of the byte that ends
// bytesIntoChunk bytes into a chunk starting at startNs.
func (c *Clock) Completion(startNs int64, bytesIntoChunk int64) int64 {
	return startNs + bytesIntoChunk*1_000_000_000/c.byteRate
}
