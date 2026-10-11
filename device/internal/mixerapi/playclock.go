package mixerapi

// playClock estimates when audio handed to the mixer is actually heard, on
// the same CLOCK_MONOTONIC scale as Recorder's capture stamps, so render
// completions and capture periods stay comparable (SPEC §4.3; the
// controller's echo comparison searches only 0–500 ms of mic-after-reference
// lag, em_attribution.MAX_LAG).
//
// Player reads, at every handoff, how many released blocks are still queued
// ahead of the new one (DataTrans::GetNumBlkReady). The mixer plays its
// queue in order and contiguously, so a chunk cannot start before
//
//   - handoff + (ahead − 1) × chunk + playPipelineNs: the blocks in front of
//     it must play first, except that the head one may already be playing
//     (it apparently counts as ready until played; see below), and the
//     chunk then takes at least playPipelineNs to reach micAsr capture
//     stamps; nor before
//   - the previous chunk's estimated end, while the queue has not run dry.
//
// Both are lower bounds, so the larger is one too: a stamp may be early by
// up to a block plus pipeline variance (which the lag search absorbs) but is
// never late, which would make the mic appear to lead its own reference.
// Reading the queue on every handoff re-anchors to the DAC, so sample
// accounting's drift (tens of ppm, hundreds of ms an hour) cannot build up.
//
// Measured on device G090LF0965260F1J (docs/fireos6-port.md §6) with a 1 kHz
// burst at the start of a render write: stamped this way, capture lagged the
// reference by +59 to +99 ms behind 1 or 2 queued blocks and +63 to +77 ms
// into an empty queue. Stamped with all `ahead` blocks it lagged by −0.3 to
// +48 ms behind 2, which is why the head block is not counted.
type playClock struct {
	pipelineNs int64 // mixer taking a block → its sound in capture stamps, lower bound
	chunkNs    int64 // playout duration of one chunk
	byteRate   int64 // bytes per second of the stream

	endNs int64 // estimated playout end of the last chunk; 0 before the first
}

const playPipelineNs = 20_000_000

func newPlayClock(chunkBytes int, byteRate int64) playClock {
	return playClock{
		pipelineNs: playPipelineNs,
		chunkNs:    int64(chunkBytes) * 1_000_000_000 / byteRate,
		byteRate:   byteRate,
	}
}

// chunk records a chunk handed off at handoffNs (taken after its Release
// returned) behind ahead released blocks, and returns its estimated playout
// start. Starts never precede the previous chunk's end, so completions
// never decrease.
func (c *playClock) chunk(handoffNs int64, ahead int) int64 {
	start := handoffNs + int64(max(ahead-1, 0))*c.chunkNs + c.pipelineNs
	if start < c.endNs {
		start = c.endNs // still playing what came before it
	}
	c.endNs = start + c.chunkNs
	return start
}

// completion returns the estimated playout time of the byte that ends
// bytesIntoChunk bytes into a chunk starting at startNs.
func (c *playClock) completion(startNs int64, bytesIntoChunk int64) int64 {
	return startNs + bytesIntoChunk*1_000_000_000/c.byteRate
}
