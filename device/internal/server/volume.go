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

	alertID    string
	alertLevel int
	alert      bool // occurrence is foreground and owns the DAC

	onMedia func(int)
}

func newVolumeController(hw Hardware, r *ring) *volumeController {
	if hw == nil {
		hw = systemHardware{}
	}
	if r == nil {
		r = newRing()
	}
	level, err := hw.ReadDAC()
	if err != nil {
		level = (volumeButtonFloor + volumeMax) / 2
		log.Printf("[volume] read DAC: %v; using %d", err, level)
	}
	return &volumeController{hw: hw, ring: r, media: clamp(level)}
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
	if v.seeded {
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

func (v *volumeController) setMediaLocked(level int) {
	v.media = level
	if !v.alert {
		v.applyLocked(level)
	}
}

func (v *volumeController) step(delta int) {
	v.mu.Lock()
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
