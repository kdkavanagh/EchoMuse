//go:build server

package slspeaker

import (
	"fmt"
	"os"
	"sync"
	"sync/atomic"
	"unsafe"

	"github.com/wilbowes/EchoMuse/internal/mixerapi"
	"github.com/wilbowes/EchoMuse/internal/render"
)

const (
	// mixerLibDefault resolves against /system/lib; EM_MIXER_LIB overrides it.
	mixerLibDefault = "libmixerAPI.so"

	// playbackModeMusic is libmixerAPI's PLAYBACK_MODE_MUSIC (0), confirmed
	// against the spike's logged `Mixer_MixerAPI:MixerOpenPlayAdv:Music`
	// (docs/fireos6-port.md §2.1).
	playbackModeMusic = 0
)

// MixerSink implements render.Sink over one libmixerAPI MUSIC-mode stream
// (Fire OS 6). See internal/mixerapi's package doc for the native chunk
// size, backpressure and gain-staging findings behind this.
type MixerSink struct {
	lib        *mixerapi.Lib
	onComplete atomic.Pointer[func(int64)]

	mu     sync.Mutex
	player *mixerapi.Player
	closed bool
}

// NewMixer opens the mixer library (shared with slmic's recorder) and a
// 48kHz mono MUSIC stream, and enables the speaker amp. It does not stop
// the mixer: it must keep owning the PCM for the HAL's AFE to see this
// playback (docs/fireos6-port.md §2.1 "the mixer routed it through
// AlgoLoopback ... into ASP").
func NewMixer() (*MixerSink, error) {
	lib := os.Getenv("EM_MIXER_LIB")
	if lib == "" {
		lib = mixerLibDefault
	}
	l, err := mixerapi.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("slspeaker: %w", err)
	}
	s := &MixerSink{lib: l}
	if err := s.openMixer(); err != nil {
		return nil, err
	}
	EnableSpeakerAmp()
	return s, nil
}

func (s *MixerSink) openMixer() error {
	p, err := s.lib.NewPlayer(render.SampleRate, 1, 16, playbackModeMusic, render.SinkFrames*2)
	if err != nil {
		return fmt.Errorf("slspeaker: %w", err)
	}
	p.SetOnComplete(s.completedMixer)
	s.player = p
	return nil
}

func (s *MixerSink) completedMixer(monoNs int64) {
	if fn := s.onComplete.Load(); fn != nil {
		(*fn)(monoNs)
	}
}

// SetOnComplete registers the per-buffer completion callback.
func (s *MixerSink) SetOnComplete(fn func(monoNs int64)) { s.onComplete.Store(&fn) }

// Write blocks until there is room for another outstanding write and
// enqueues one render.SinkFrames buffer. S16 samples are native
// little-endian on ARM.
func (s *MixerSink) Write(pcm []int16) error {
	if len(pcm) != render.SinkFrames {
		return fmt.Errorf("slspeaker: write of %d frames, want %d", len(pcm), render.SinkFrames)
	}
	s.mu.Lock()
	p, closed := s.player, s.closed
	s.mu.Unlock()
	if closed {
		return mixerapi.ErrClosed
	}
	return p.Write(unsafe.Slice((*byte)(unsafe.Pointer(&pcm[0])), len(pcm)*2))
}

// Restart replaces the player after a write failure.
func (s *MixerSink) Restart() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return mixerapi.ErrClosed
	}
	s.player.Close()
	return s.openMixer()
}

// Close releases the player; a blocked Write returns mixerapi.ErrClosed.
func (s *MixerSink) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.closed {
		s.closed = true
		s.player.Close()
	}
	return nil
}

var _ render.Sink = (*MixerSink)(nil)
