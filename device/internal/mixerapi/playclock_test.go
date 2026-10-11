package mixerapi

import "testing"

const (
	testChunkBytes = 4800  // 50 ms at 48 kHz mono S16
	testByteRate   = 96000 // 48 kHz × 2 B
	ms             = int64(1_000_000)
)

// Blocks queued ahead of a chunk play before it, but the head one may
// already be playing: each block after it pushes the start back one chunk,
// on top of the pipeline from the mixer to capture.
func TestPlayClockStartsBehindTheQueuedBlocks(t *testing.T) {
	for _, tc := range []struct{ ahead, blocks int64 }{{0, 0}, {1, 0}, {2, 1}, {15, 14}} {
		c := newPlayClock(testChunkBytes, testByteRate)
		h := 1000 * ms
		if got, want := c.chunk(h, int(tc.ahead)), h+tc.blocks*50*ms+playPipelineNs; got != want {
			t.Errorf("ahead %d: start = %d, want %d", tc.ahead, got, want)
		}
	}
}

// While the queue has not run dry the mixer plays chunks back to back: an
// estimate that lands before the previous chunk's end follows on from it.
func TestPlayClockQueuedChunksAreContiguous(t *testing.T) {
	c := newPlayClock(testChunkBytes, testByteRate)
	first := c.chunk(0, 2) // 70–120 ms
	// 30 ms later the mixer has played the head block: one block ahead, so
	// the raw estimate is 30+20 = 50 ms, inside the first chunk.
	if got, want := c.chunk(30*ms, 1), first+50*ms; got != want {
		t.Errorf("second start = %d, want contiguous %d", got, want)
	}
}

// A writer that stalled until the queue drained cannot follow the old audio:
// its chunk starts from its own handoff.
func TestPlayClockReanchorsAfterTheQueueRanDry(t *testing.T) {
	c := newPlayClock(testChunkBytes, testByteRate)
	c.chunk(0, 2) // ends at 120 ms
	h := 2000 * ms
	if got, want := c.chunk(h, 0), h+playPipelineNs; got != want {
		t.Errorf("start after underrun = %d, want %d", got, want)
	}
}

func TestPlayClockCompletionIsTheWritesLastByte(t *testing.T) {
	c := newPlayClock(testChunkBytes, testByteRate)
	start := c.chunk(0, 0)
	// A write ending 2,048 bytes into the chunk completes 2048/96000 s in.
	if got, want := c.completion(start, 2048), start+2048*1_000_000_000/testByteRate; got != want {
		t.Errorf("completion = %d, want %d", got, want)
	}
}
