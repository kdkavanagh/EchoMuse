//go:build server

package slmic

import (
	"fmt"
	"os"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/mixerapi"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	// mixerLibDefault resolves against /system/lib; EM_MIXER_LIB overrides it.
	mixerLibDefault = "libmixerAPI.so"

	// mixerStreamName is the mixer record type that selects
	// AUDIO_SOURCE_VOICE_RECOGNITION — the same ASP pipeline the Fire OS 5
	// opensl.PresetVoiceRecognition path selects (docs/fireos6-port.md §2.1).
	mixerStreamName = "micAsr"
)

// MixerMicrophone is the libmixerAPI capture stream (Fire OS 6): see
// internal/mixerapi's package doc for the native block size, clock domain
// and credential findings behind this. periodFrames/sampleRateHz are
// shared with the OpenSL path above (the mixer stream itself is already
// 16kHz mono — there is no rate to request).
//
// micAsr carries the AFE's per-frame metadata in bit 0 of every sample
// (docs/alexa-afe.md). Read decodes it from exactly the periods it delivers,
// so a period dropped before Read shows up as a counter gap, and leaves the
// samples untouched: wake scoring and ASR see what they always saw.
type MixerMicrophone struct {
	rec  *mixerapi.Recorder
	pcm  [periodFrames]int16
	dec  afe.Decoder
	meta afe.Period
}

// OpenMixer starts a micAsr recorder through the mixer daemon.
func OpenMixer() (*MixerMicrophone, error) {
	lib := os.Getenv("EM_MIXER_LIB")
	if lib == "" {
		lib = mixerLibDefault
	}
	l, err := mixerapi.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	rec, err := l.NewRecorder(mixerStreamName, periodFrames)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	return &MixerMicrophone{rec: rec}, nil
}

// Read blocks for the next completed period. Block.PCM and Block.AFE are
// valid until the next Read.
func (m *MixerMicrophone) Read() (pkgmic.Block, error) {
	buf, monoNs, err := m.rec.ReadStamped()
	if err != nil {
		return pkgmic.Block{}, fmt.Errorf("slmic: %w", err)
	}
	pcm := m.pcm[:len(buf)/2]
	ema.PCM(pcm, buf)
	m.dec.Decode(pcm, &m.meta)
	return pkgmic.Block{PCM: pcm, MonoNs: monoNs, AFE: &m.meta}, nil
}

// Drops counts periods dropped because Read fell behind, or lost to a
// stream re-open (EHOSTDOWN/repeated ETIMEDOUT).
func (m *MixerMicrophone) Drops() uint64 { return m.rec.Drops() }

// Close stops capture. The mixerapi.Lib is process-shared and stays open.
func (m *MixerMicrophone) Close() { m.rec.Close() }

var _ pkgmic.Microphone = (*MixerMicrophone)(nil)
