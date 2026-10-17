package server

import (
	"log"
	"math"
	"sync"
	"time"
)

const (
	volumeMin = 0
	// volumeMax is codec unity (tinymix ctl 61 index 127). Above this, positive
	// digital gain clips near-full-scale PCM; never raise this ceiling.
	volumeMax = 127
	// Physical buttons traverse the useful dB-linear band; explicit remote
	// sets may still select 0..127.
	volumeButtonFloor = 47 // −40 dB
	volumeStep        = 8  // 4 dB per press
	volumeDisplay     = 2 * time.Second
)

// volumeController owns codec DAC level. media is persistent user volume;
// an alert foreground temporarily overrides only the DAC (§16.5).
type volumeController struct {
	mu sync.Mutex

	hw   Hardware
	ring *ring

	media  int
	seeded bool
	// disabled is why volume control is off ("" when it works).
	disabled string

	alertID    string
	alertLevel int
	alert      bool // occurrence is foreground and owns the DAC

	onMedia func(int)
}

func newVolumeController(hw Hardware, r *ring, authoritative bool, disabled string) *volumeController {
	if hw == nil {
		hw = systemHardware{}
	}
	if r == nil {
		r = newRing()
	}
	fallback := (volumeButtonFloor + volumeMax) / 2
	if disabled != "" {
		log.Printf("[volume] volume control disabled: %s", disabled)
		return &volumeController{hw: hw, ring: r, media: fallback, disabled: disabled}
	}
	level, err := hw.ReadDAC()
	if err != nil {
		level, authoritative = fallback, false
		log.Printf("[volume] read DAC: %v; using %d", err, level)
	}
	return &volumeController{hw: hw, ring: r, media: clamp(level), seeded: authoritative}
}

func (v *volumeController) setCallback(cb func(int)) {
	v.mu.Lock()
	v.onMedia = cb
	v.mu.Unlock()
}

func (v *volumeController) state() (level int, seeded bool) {
	v.mu.Lock()
	defer v.mu.Unlock()
	return v.media, v.seeded
}

// seed restores startupVolume once. A physical or remote volume change first
// makes that live value authoritative and prevents a later config push from
// overwriting it.
func (v *volumeController) seed(level int) {
	v.mu.Lock()
	if v.seeded || v.disabled != "" {
		v.mu.Unlock()
		return
	}
	v.seeded = true
	v.setMediaLocked(clamp(level))
	cb, out := v.onMedia, v.media
	v.mu.Unlock()
	if cb != nil {
		cb(out)
	}
}

func (v *volumeController) setMedia(level int, show bool) {
	v.mu.Lock()
	if v.disabled != "" {
		v.mu.Unlock()
		log.Printf("[volume] volume_set %d ignored: %s", level, v.disabled)
		return
	}
	v.seeded = true
	v.setMediaLocked(clamp(level))
	cb, out := v.onMedia, v.media
	v.mu.Unlock()
	if show {
		v.ring.showVolume(out)
	}
	if cb != nil {
		cb(out)
	}
}

// external records a change made outside the firmware and already applied
// (internal/androidvol's Watch on the Show), so nothing is written back.
// During a foreground alert it moves only the occurrence's level, as a
// volume button would; otherwise it is media volume, reported.
func (v *volumeController) external(level int) {
	level = clamp(level)
	v.mu.Lock()
	if v.disabled != "" {
		v.mu.Unlock()
		return
	}
	if v.alert {
		v.alertLevel = level
		v.mu.Unlock()
		return
	}
	v.seeded = true
	v.media = level
	cb := v.onMedia
	v.mu.Unlock()
	if cb != nil {
		cb(level)
	}
}

func (v *volumeController) setMediaLocked(level int) {
	v.media = level
	if !v.alert {
		v.applyLocked(level)
	}
}

func (v *volumeController) step(delta int) {
	v.mu.Lock()
	if v.disabled != "" {
		v.mu.Unlock()
		return
	}
	if v.alert {
		v.alertLevel = clampToButtonBand(v.alertLevel + delta)
		v.applyLocked(v.alertLevel)
		level := v.alertLevel
		v.mu.Unlock()
		v.ring.showVolume(level)
		return
	}
	v.seeded = true
	v.setMediaLocked(clampToButtonBand(v.media + delta))
	cb, level := v.onMedia, v.media
	v.mu.Unlock()
	v.ring.showVolume(level)
	if cb != nil {
		cb(level)
	}
}

// setAlert routes the DAC for one active occurrence. Only foreground alerts
// own the DAC; backgrounding immediately restores media volume so dialog can
// play at its selected level. volume is 0..1; nil means current media volume.
func (v *volumeController) setAlert(active bool, id string, foreground bool, volume *float64) {
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.disabled != "" {
		return
	}
	want := active && foreground
	if want {
		if id != v.alertID {
			v.alertID = id
			v.alertLevel = v.media
			if volume != nil {
				v.alertLevel = clamp(int(math.Round(*volume * volumeMax)))
			}
		}
		v.alert = true
		v.applyLocked(v.alertLevel)
		return
	}
	v.alert = false
	v.applyLocked(v.media)
	if !active {
		v.alertID = ""
	}
}

func (v *volumeController) applyLocked(level int) {
	if err := v.hw.SetDAC(level); err != nil {
		log.Printf("[volume] set DAC %d: %v", level, err)
	}
}

func clamp(level int) int { return max(volumeMin, min(volumeMax, level)) }

func clampToButtonBand(level int) int {
	return max(volumeButtonFloor, min(volumeMax, level))
}
