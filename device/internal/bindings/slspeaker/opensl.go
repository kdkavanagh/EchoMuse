//go:build server

package slspeaker

import (
	"fmt"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"

	"github.com/wilbowes/EchoMuse/internal/opensl"
	"github.com/wilbowes/EchoMuse/internal/render"
)

const (
	// openslBuffers is how many render.SinkFrames writes the OpenSL queue
	// holds (render.Sink allows at most 8). Behind them sits the track's
	// own ring, 60–80 ms on the Show (internal/opensl's playPipelineNs), so
	// two buffers keep about 145–165 ms in front of AudioFlinger's mixer:
	// the slack the Dot keeps with its 3 × 50 ms mixer queue
	// (docs/fireos6-port.md §6), while a write such as the wake chime is
	// heard sooner (an estimated ~250 ms, against the Dot's 321–385 ms).
	openslBuffers = 2

	// openslStall is how long Write waits for OpenSL ES to consume a
	// buffer before the player is taken for broken (audioserver restarted,
	// the track was invalidated) and the render mixer restarts the sink.
	openslStall = 1500 * time.Millisecond
)

// OpenSLSink implements render.Sink over one OpenSL ES player on the Echo
// Show: 48 kHz mono on STREAM_MUSIC, whose index is the device's volume
// authority. Everything played through AudioFlinger is ASP's echo reference,
// so the native AFE cancels it. Completions carry each write's estimated
// playout end on the capture stamp scale (internal/opensl).
type OpenSLSink struct {
	eng        *opensl.Engine
	onComplete atomic.Pointer[func(int64)]

	mu     sync.Mutex
	player *opensl.Player
	closed bool
}

// NewOpenSL opens the OpenSL engine (shared with slmic's recorder) and the
// player. Unlike NewMixer it switches no amp: on the Show audioserver owns
// the codec, and the firmware never writes tinymix.
func NewOpenSL() (*OpenSLSink, error) {
	eng, err := opensl.Open()
	if err != nil {
		return nil, fmt.Errorf("slspeaker: %w", err)
	}
	s := &OpenSLSink{eng: eng}
	if err := s.openPlayer(); err != nil {
		return nil, err
	}
	return s, nil
}

func (s *OpenSLSink) openPlayer() error {
	p, err := s.eng.NewPlayer(render.SampleRate, render.SinkFrames*2, openslBuffers)
	if err != nil {
		return fmt.Errorf("slspeaker: %w", err)
	}
	p.SetOnComplete(s.completed)
	s.player = p
	return nil
}

func (s *OpenSLSink) completed(monoNs int64) {
	if fn := s.onComplete.Load(); fn != nil {
		(*fn)(monoNs)
	}
}

// SetOnComplete registers the per-buffer completion callback.
func (s *OpenSLSink) SetOnComplete(fn func(monoNs int64)) { s.onComplete.Store(&fn) }

// Write blocks until a buffer is free and enqueues one render.SinkFrames
// buffer. S16 samples are native little-endian on ARM.
func (s *OpenSLSink) Write(pcm []int16) error {
	if len(pcm) != render.SinkFrames {
		return fmt.Errorf("slspeaker: write of %d frames, want %d", len(pcm), render.SinkFrames)
	}
	s.mu.Lock()
	p, closed := s.player, s.closed
	s.mu.Unlock()
	if closed {
		return opensl.ErrClosed
	}
	if err := p.Write(unsafe.Slice((*byte)(unsafe.Pointer(&pcm[0])), len(pcm)*2), openslStall); err != nil {
		return fmt.Errorf("slspeaker: %w", err)
	}
	return nil
}

// Restart replaces the player after a write failure.
func (s *OpenSLSink) Restart() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return opensl.ErrClosed
	}
	s.player.Close()
	return s.openPlayer()
}

// Close releases the player; a blocked Write returns opensl.ErrClosed.
func (s *OpenSLSink) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.closed {
		s.closed = true
		s.player.Close()
	}
	return nil
}

var _ render.Sink = (*OpenSLSink)(nil)
