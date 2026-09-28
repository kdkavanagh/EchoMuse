//go:build server

// Package slmic captures through Android's audio HAL via OpenSL ES at the
// VOICE_RECOGNITION preset, so capture passes through the native AFE (SPEC
// §4.1): 16 kHz mono S16 in 1,280-sample (80 ms) periods, each stamped with
// CLOCK_MONOTONIC when OpenSL ES completed it (§4.3). It has one consumer.
package slmic

import (
	"fmt"
	"os"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/opensl"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	// defaultLib resolves against /system/lib; EM_OPENSL_LIB overrides it.
	defaultLib = "libOpenSLES.so"

	sampleRateHz = 16000 // §4.1
	periodFrames = 1280  // §4.1: 80 ms callbacks

	// hwBuffers is the HAL-facing buffer count only.
	hwBuffers = 4
)

// Microphone is the OpenSL ES capture stream.
type Microphone struct {
	rec *opensl.Recorder
	pcm [periodFrames]int16
}

// Open starts a VOICE_RECOGNITION recorder. It never stops mediaserver or
// the mixer: the HAL must keep owning the PCM for the AFE to run.
func Open() (*Microphone, error) {
	lib := os.Getenv("EM_OPENSL_LIB")
	if lib == "" {
		lib = defaultLib
	}
	eng, err := opensl.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	rec, err := eng.NewRecorder(opensl.PresetVoiceRecognition, sampleRateHz, periodFrames, hwBuffers)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	if err := rec.Start(); err != nil {
		rec.Close()
		return nil, fmt.Errorf("slmic: start: %w", err)
	}
	return &Microphone{rec: rec}, nil
}

// Read blocks for the next completed period. Block.PCM is valid until the
// next Read.
func (m *Microphone) Read() (pkgmic.Block, error) {
	buf, monoNs, err := m.rec.ReadStamped()
	if err != nil {
		return pkgmic.Block{}, fmt.Errorf("slmic: %w", err)
	}
	pcm := m.pcm[:len(buf)/2]
	ema.PCM(pcm, buf)
	return pkgmic.Block{PCM: pcm, MonoNs: monoNs}, nil
}

// Drops counts periods OpenSL ES completed while Read was behind.
func (m *Microphone) Drops() uint64 { return m.rec.Drops() }

// Close stops capture. The opensl.Engine is process-shared and stays open.
func (m *Microphone) Close() {
	_ = m.rec.Stop()
	m.rec.Close()
}

var _ pkgmic.Microphone = (*Microphone)(nil)
