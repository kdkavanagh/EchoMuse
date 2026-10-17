package playclock

import "testing"

const (
	testChunkBytes = 4800  // 50 ms at 48 kHz mono S16
	testByteRate   = 96000 // 48 kHz × 2 B
	testPipelineNs = 20 * ms
	ms             = int64(1_000_000)
)

func newTestClock() Clock { return New(testChunkBytes, testByteRate, testPipelineNs) }

// Chunks queued ahead of a chunk play before it, but the head one may
// already be playing: each chunk after it pushes the start back one chunk,
// on top of the pipeline from the queue to capture.
func TestStartsBehindTheQueuedChunks(t *testing.T) {
	for _, tc := range []struct{ ahead, chunks int64 }{{0, 0}, {1, 0}, {2, 1}, {15, 14}} {
		c := newTestClock()
		h := 1000 * ms
		if got, want := c.Chunk(h, int(tc.ahead)), h+tc.chunks*50*ms+testPipelineNs; got != want {
			t.Errorf("ahead %d: start = %d, want %d", tc.ahead, got, want)
		}
	}
}

// While the queue has not run dry chunks play back to back: an estimate
// that lands before the previous chunk's end follows on from it.
func TestQueuedChunksAreContiguous(t *testing.T) {
	c := newTestClock()
	first := c.Chunk(0, 2) // 70–120 ms
	// 30 ms later the queue has played the head chunk: one chunk ahead, so
	// the raw estimate is 30+20 = 50 ms, inside the first chunk.
	if got, want := c.Chunk(30*ms, 1), first+50*ms; got != want {
		t.Errorf("second start = %d, want contiguous %d", got, want)
	}
}

// A writer that stalled until the queue drained cannot follow the old audio:
// its chunk starts from its own handoff.
func TestReanchorsAfterTheQueueRanDry(t *testing.T) {
	c := newTestClock()
	c.Chunk(0, 2) // ends at 120 ms
	h := 2000 * ms
	if got, want := c.Chunk(h, 0), h+testPipelineNs; got != want {
		t.Errorf("start after underrun = %d, want %d", got, want)
	}
}

func TestCompletionIsTheWritesLastByte(t *testing.T) {
	c := newTestClock()
	start := c.Chunk(0, 0)
	// A write ending 2,048 bytes into the chunk completes 2048/96000 s in.
	if got, want := c.Completion(start, 2048), start+2048*1_000_000_000/testByteRate; got != want {
		t.Errorf("completion = %d, want %d", got, want)
	}
}
