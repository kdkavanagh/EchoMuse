package server

import "github.com/wilbowes/EchoMuse/internal/androidvol"

// StreamVolume is Android's STREAM_MUSIC index, 0..androidvol.MaxIndex
// (internal/androidvol.Control).
type StreamVolume interface {
	Index() (int, error)
	SetIndex(index int) error
}

// androidHardware is the Echo Show's Hardware. Volume is Android's
// STREAM_MUSIC index, the authority the volume keys and the kiosk share,
// mapped proportionally to the wire's 0..127. The Show has no codec
// controls the firmware may touch: audioserver owns both PCMs and their
// mixer controls, the speaker amp follows Android's routing, and there is
// no privacy LED or ADC mute the firmware integrates yet (Phase 7), so the
// rest are no-ops and never shell tinymix.
type androidHardware struct{ vol StreamVolume }

// NewAndroidHardware returns the Show's Hardware over vol.
func NewAndroidHardware(vol StreamVolume) Hardware { return androidHardware{vol: vol} }

func (h androidHardware) ReadDAC() (int, error) {
	index, err := h.vol.Index()
	if err != nil {
		return 0, err
	}
	return androidvol.LevelForIndex(index), nil
}

func (h androidHardware) SetDAC(level int) error {
	return h.vol.SetIndex(androidvol.IndexForLevel(clamp(level)))
}

func (androidHardware) SetADCMute(bool) error    { return nil }
func (androidHardware) SetSpeakerAmp(bool) error { return nil }
func (androidHardware) SetMuteLED(bool) error    { return nil }
