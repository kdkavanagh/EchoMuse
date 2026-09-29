//go:build server

// Package slspeaker is the OpenSL ES render sink (SPEC §4.1): the final mix
// reaches the HAL through AudioFlinger, so the native AFE keeps its far-end
// reference. Mixing, focus and accounting live in internal/render.
package slspeaker

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"
	"unsafe"

	"github.com/wilbowes/EchoMuse/internal/opensl"
	"github.com/wilbowes/EchoMuse/internal/render"
)

const (
	defaultLib = "libOpenSLES.so"
	// hardwareBuffers is the OpenSL queue depth; render.Sink allows at most 8.
	hardwareBuffers = 4
)

// Sink implements render.Sink over one OpenSL ES AudioPlayer.
type Sink struct {
	eng        *opensl.Engine
	onComplete atomic.Pointer[func(int64)]

	mu     sync.Mutex
	player *opensl.Player
	closed bool
}

// New opens the OpenSL engine (shared with slmic's recorder) and a 48 kHz
// mono player, and enables the speaker amp. It does not stop mediaserver:
// the HAL must own the PCM for the AFE to see this playback.
func New() (*Sink, error) {
	lib := os.Getenv("EM_OPENSL_LIB")
	if lib == "" {
		lib = defaultLib
	}
	eng, err := opensl.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("slspeaker: %w", err)
	}
	s := &Sink{eng: eng}
	if err := s.open(); err != nil {
		return nil, err
	}
	EnableSpeakerAmp()
	return s, nil
}

func (s *Sink) open() error {
	p, err := s.eng.NewPlayer(render.SampleRate, render.SinkFrames*2, hardwareBuffers)
	if err != nil {
		return fmt.Errorf("slspeaker: %w", err)
	}
	p.SetOnComplete(s.completed)
	s.player = p
	return nil
}

func (s *Sink) completed(monoNs int64) {
	if fn := s.onComplete.Load(); fn != nil {
		(*fn)(monoNs)
	}
}

// SetOnComplete registers the per-buffer completion callback.
func (s *Sink) SetOnComplete(fn func(monoNs int64)) { s.onComplete.Store(&fn) }

// Write blocks until a hardware buffer is free and enqueues one
// render.SinkFrames buffer. S16 samples are native little-endian on ARM.
func (s *Sink) Write(pcm []int16) error {
	if len(pcm) != render.SinkFrames {
		return fmt.Errorf("slspeaker: write of %d frames, want %d", len(pcm), render.SinkFrames)
	}
	s.mu.Lock()
	p, closed := s.player, s.closed
	s.mu.Unlock()
	if closed {
		return opensl.ErrClosed
	}
	return p.Write(unsafe.Slice((*byte)(unsafe.Pointer(&pcm[0])), len(pcm)*2))
}

// Restart replaces the player after a write failure.
func (s *Sink) Restart() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return opensl.ErrClosed
	}
	s.player.Close()
	return s.open()
}

// Close releases the player; a blocked Write returns opensl.ErrClosed.
func (s *Sink) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.closed {
		s.closed = true
		s.player.Close()
	}
	return nil
}

// EnableSpeakerAmp turns on the internal speaker amp (tinymix control 5),
// which is off by default and switched off by accdet on headphone insert.
func EnableSpeakerAmp() {
	if out, err := exec.Command("tinymix", "-D", "0", "5", "On").CombinedOutput(); err != nil {
		log.Printf("[slspeaker] could not enable speaker amp: %v — %s", err, strings.TrimSpace(string(out)))
	}
}

var _ render.Sink = (*Sink)(nil)
