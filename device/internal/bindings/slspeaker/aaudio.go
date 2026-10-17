//go:build server

package slspeaker

import (
	"fmt"
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/internal/aaudio"
	"github.com/wilbowes/EchoMuse/internal/monoclock"
	"github.com/wilbowes/EchoMuse/internal/render"
)

const (
	// aaudioQueueWrites is how many render.SinkFrames writes the AAudio
	// buffer is asked to hold: the two the OpenSL sink keeps in front of
	// AudioFlinger. Behind them AudioFlinger's output holds about 210 ms
	// more (the server's consumed count ran 9,968–10,352 frames ahead of
	// the presented position; measured 2026-10-17), so a write's last
	// frame is presented about 285 ms after the write returns.
	aaudioQueueWrites = 2

	// aaudioStall is how long Write waits for AAudio to take a write before
	// the stream is taken for broken (audioserver restarted, the track was
	// invalidated) and the render mixer restarts the sink.
	aaudioStall = 1500 * time.Millisecond

	// aaudioMaxPending bounds the writes awaiting completion. The server
	// consumes them within the buffer (two or three pending); should its
	// consumed count ever stop moving, the oldest is completed anyway, so
	// render.Sink's at most 8 outstanding writes holds.
	aaudioMaxPending = 6

	// aaudioRenderLatencyNs is aaClock's latencyNs: AAudio's presentation
	// time of a frame → its echo on slmic.AAudioMicrophone's capture
	// stamps. Findings §8.4 Q6 measured 86.5–89.3 ms from fitted
	// presentation to the VOICE_RECOGNITION echo; the capture stamps here
	// sit captureBiasNs (5 ms) earlier than that fit, so about 83 ms.
	// device/tools/sl_latency over this sink and microphone (2026-10-17,
	// lineage-18.1 NS6571 libasp, index 9/15) measured, with this constant:
	//   - noise bursts, five opens: pcm lag 39.7–53.6 ms (per-open medians
	//     46.3–48.0), so 100–114 ms in all;
	//   - tone bursts: 23.3 ms on a first burst, before the AEC converged
	//     on the tone (83 ms in all, the findings' figure), 59–70 ms after.
	// It sits 23 ms under the earliest, as opensl.playPipelineNs sits 25 ms
	// under its own. A burst after a 2.5 s writer stall reads +242 ms (the
	// handoff bound alone; early, inside the lag search).
	aaudioRenderLatencyNs = 60_000_000
)

// AAudioSink implements render.Sink over one AAudio output stream on the
// LineageOS Echo Show: 48 kHz mono, USAGE_MEDIA (STREAM_MUSIC, the device's
// volume). Everything played through AudioFlinger is ASP's echo reference,
// so the native AFE cancels it. Each write completes once the server has
// consumed it, carrying its estimated playout end on the capture stamp
// scale (aaClock).
type AAudioSink struct {
	lib        *aaudio.Lib
	onComplete atomic.Pointer[func(int64)]

	mu     sync.Mutex
	stream *aaudio.Stream
	closed bool

	// Write's own state, reset with each stream.
	written int64 // frames written to the current stream
	clock   aaClock
	pending []pendingWrite
	forced  uint64 // completions forced by aaudioMaxPending
}

type pendingWrite struct {
	endFrame int64
	endNs    int64
}

// NewAAudio opens and starts the output stream. Like NewOpenSL it switches
// no amp: audioserver owns the codec.
func NewAAudio() (*AAudioSink, error) {
	lib, err := aaudio.Open()
	if err != nil {
		return nil, fmt.Errorf("slspeaker: %w", err)
	}
	s := &AAudioSink{lib: lib, pending: make([]pendingWrite, 0, aaudioMaxPending+1)}
	if err := s.openStream(); err != nil {
		return nil, err
	}
	log.Printf("slspeaker: AAudio media output: %s", s.stream.Describe())
	return s, nil
}

// openStream opens and starts a stream; s.mu is held or s is unshared.
func (s *AAudioSink) openStream() error {
	st, err := s.lib.OpenOutput(render.SampleRate, 1, aaudio.UsageMedia, aaudioQueueWrites*render.SinkFrames)
	if err != nil {
		return fmt.Errorf("slspeaker: %w", err)
	}
	// Started before the first write: writes to a stream not yet started
	// fail (findings §8.4 Q6), and an empty started stream plays silence.
	if err := st.Start(); err != nil {
		st.Close()
		return fmt.Errorf("slspeaker: %w", err)
	}
	s.stream = st
	s.written = 0
	s.clock = newAAClock(render.SampleRate, aaudioRenderLatencyNs)
	s.pending = s.pending[:0]
	return nil
}

// SetOnComplete registers the per-buffer completion callback.
func (s *AAudioSink) SetOnComplete(fn func(monoNs int64)) { s.onComplete.Store(&fn) }

// Write blocks until AAudio has taken one render.SinkFrames buffer, then
// completes every earlier write the server has consumed.
func (s *AAudioSink) Write(pcm []int16) error {
	if len(pcm) != render.SinkFrames {
		return fmt.Errorf("slspeaker: write of %d frames, want %d", len(pcm), render.SinkFrames)
	}
	s.mu.Lock()
	st, closed := s.stream, s.closed
	s.mu.Unlock()
	if closed {
		return aaudio.ErrClosed
	}
	n, err := st.Write(pcm, aaudioStall)
	if err != nil {
		return fmt.Errorf("slspeaker: %w", err)
	}
	if n < len(pcm) {
		return fmt.Errorf("slspeaker: AAudio took %d of %d frames in %v", n, len(pcm), aaudioStall)
	}
	handoff := monoclock.Now()
	s.written += int64(n)
	consumed := st.FramesRead()
	pos, tsNs, ok := st.Timestamp()
	s.pending = append(s.pending, pendingWrite{endFrame: s.written, endNs: s.clock.end(handoff, s.written, consumed, pos, tsNs, ok)})

	fn := s.onComplete.Load()
	done := 0
	for done < len(s.pending) && (s.pending[done].endFrame <= consumed || len(s.pending)-done > aaudioMaxPending) {
		if s.pending[done].endFrame > consumed {
			if s.forced++; s.forced&(s.forced-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slspeaker: AAudio consumed count stuck at %d with %d writes pending; completing the oldest (%d so far)",
					consumed, len(s.pending)-done, s.forced)
			}
		}
		if fn != nil {
			(*fn)(s.pending[done].endNs)
		}
		done++
	}
	s.pending = s.pending[:copy(s.pending, s.pending[done:])]
	return nil
}

// Restart replaces the stream after a write failure; writes still pending
// are abandoned with it (the mixer starts a new epoch).
func (s *AAudioSink) Restart() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return aaudio.ErrClosed
	}
	s.stream.Close()
	return s.openStream()
}

// Close releases the stream; a blocked Write returns.
func (s *AAudioSink) Close() error {
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return nil
	}
	s.closed = true
	st := s.stream
	s.mu.Unlock()
	st.Close()
	return nil
}

var _ render.Sink = (*AAudioSink)(nil)
