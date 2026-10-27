// Package cue holds the firmware's built-in sounds as 48 kHz mono S16 PCM:
// the earcons controllers name in render.start (SPEC §11.2, §18.2), and the
// offline notice the device plays on its own.
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
	// AnnouncementChime precedes a Home Assistant announcement
	// (announcement_chime).
	AnnouncementChime = "builtin:announcement_chime"
)

var ErrNotFound = errors.New("cue: unknown built-in asset")

// wakeChimePCM is sounds/wake_word_triggered.flac from
// esphome/home-assistant-voice-pe (© 2024 Clayton Charles Tapp, CC BY 4.0),
// decoded by gen.sh.
//
//go:embed wake_word_triggered.pcm
var wakeChimePCM []byte

// noInputChimePCM is ui_error_generic_2.mp3, the Amazon Echo's error
// earcon, decoded by gen.sh.
//
//go:embed ui_error_generic_2.pcm
var noInputChimePCM []byte

// announcementChimePCM is alerts_notification_01.mp3, the Amazon Echo's
// announcement earcon, decoded by gen.sh.
//
//go:embed alerts_notification_01.pcm
var announcementChimePCM []byte

// offlineNoticePCM is Home Assistant's TTS of "I can't connect to the smart
// home right now.", decoded by gen.sh (provenance there).
//
//go:embed offline_notice.pcm
var offlineNoticePCM []byte

var assets = map[string][]int16{
	WakeChime:         decodeS16LE(wakeChimePCM),
	NoInputChime:      decodeS16LE(noInputChimePCM),
	AnnouncementChime: decodeS16LE(announcementChimePCM),
}

var offlineNotice = decodeS16LE(offlineNoticePCM, noInputChimePCM)

// decodeS16LE decodes the concatenation of parts into one slice.
func decodeS16LE(parts ...[]byte) []int16 {
	n := 0
	for _, b := range parts {
		if len(b)%2 != 0 {
			panic("cue: embedded PCM has an odd byte count")
		}
		n += len(b) / 2
	}
	pcm := make([]int16, 0, n)
	for _, b := range parts {
		for i := 0; i < len(b); i += 2 {
			pcm = append(pcm, int16(binary.LittleEndian.Uint16(b[i:])))
		}
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

// OfflineNotice is what the device plays when it hears a wake but has no
// controller: the spoken notice, its own trailing pause, then the no-input
// chime. It is not a render.start asset. The slice is shared and must not be
// modified.
func OfflineNotice() []int16 { return offlineNotice }
