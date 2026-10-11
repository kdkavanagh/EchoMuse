// Package cue holds the firmware's built-in earcon assets as 48 kHz mono
// S16 PCM for the mixer's earcon source (SPEC §11.2, §18.2).
package cue

import (
	_ "embed"
	"encoding/binary"
	"errors"
)

// The render.start local_asset names of the built-in earcons.
const (
	WakeChime = "builtin:wake_chime"
	// NoInputChime ends a wake turn that heard no command (no_input_chime).
	NoInputChime = "builtin:no_input_chime"
)

var ErrNotFound = errors.New("cue: unknown built-in asset")

// wakeChimePCM is sounds/wake_word_triggered.flac from
// esphome/home-assistant-voice-pe (© 2024 Clayton Charles Tapp, CC BY 4.0),
// decoded by gen.sh.
//
//go:embed wake_word_triggered.pcm
var wakeChimePCM []byte

// noInputChimePCM is synthesized by gen.sh.
//
//go:embed no_input_chime.pcm
var noInputChimePCM []byte

var assets = map[string][]int16{
	WakeChime:    decodeS16LE(wakeChimePCM),
	NoInputChime: decodeS16LE(noInputChimePCM),
}

func decodeS16LE(b []byte) []int16 {
	if len(b)%2 != 0 {
		panic("cue: embedded PCM has an odd byte count")
	}
	pcm := make([]int16, len(b)/2)
	for i := range pcm {
		pcm[i] = int16(binary.LittleEndian.Uint16(b[2*i:]))
	}
	return pcm
}

// Asset returns a built-in earcon. The slice is shared and must not be
// modified; render.Mixer only reads local PCM.
func Asset(name string) ([]int16, error) {
	pcm, ok := assets[name]
	if !ok {
		return nil, ErrNotFound
	}
	return pcm, nil
}
