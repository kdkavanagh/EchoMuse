package server

import (
	"log"
	"math"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/pkg/led"
)

const numLEDs = 12

// Layer is a composited LED owner. Larger values outrank smaller ones;
// volume and privacy are separate, still higher-priority local overlays.
type Layer uint8

const (
	LayerController Layer = iota // retained leds/led_anim
	LayerLink                    // disconnected/pending local indication
	LayerAlert                   // foreground/background alert (§11.2)
	layerCount
)

type ringLayer struct {
	gen    uint64
	active bool
	frame  [numLEDs]led.Led
}

// ring composites controller and local indications. All lower layers retain
// their latest frame while hidden, so release restores current state.
type ring struct {
	mu sync.Mutex

	leds  led.Controller
	layer [layerCount]ringLayer
	muted bool

	arc      bool
	arcFrame [numLEDs]led.Led
	arcTimer *time.Timer

	level atomic.Uint64 // float64 bits; final-mix RMS in [0,1]
}

func newRing() *ring {
	r := &ring{}
	for l := range r.layer {
		for i := range r.layer[l].frame {
			r.layer[l].frame[i].ID = i
		}
	}
	return r
}

// SetController attaches or replaces the physical ring and paints the
// currently winning layer.
func (r *ring) SetController(c led.Controller) {
	r.mu.Lock()
	r.leds = c
	r.paintLocked()
	r.mu.Unlock()
}

func (r *ring) claim(layer Layer) uint64 {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.layer[layer].gen++
	return r.layer[layer].gen
}

func (r *ring) update(layer Layer, gen uint64, frame []led.Led) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	p := &r.layer[layer]
	if p.gen != gen {
		return false
	}
	clear(p.frame[:])
	for i := range p.frame {
		p.frame[i].ID = i
	}
	for _, v := range frame {
		if v.ID >= 0 && v.ID < numLEDs {
			p.frame[v.ID] = v
		}
	}
	p.active = true
	r.paintLocked()
	return true
}

func (r *ring) clearGen(layer Layer, gen uint64) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	p := &r.layer[layer]
	if p.gen != gen {
		return false
	}
	p.active = false
	r.paintLocked()
	return true
}

func (r *ring) clear(layer Layer) {
	r.mu.Lock()
	p := &r.layer[layer]
	p.gen++
	p.active = false
	r.paintLocked()
	r.mu.Unlock()
}

// setPartial applies one retained `leds` frame to the controller layer.
func (r *ring) setPartial(values []led.Led) {
	r.mu.Lock()
	p := &r.layer[LayerController]
	p.gen++ // cancel a controller animation
	for _, v := range values {
		if v.ID >= 0 && v.ID < numLEDs {
			p.frame[v.ID] = v
		}
	}
	p.active = true
	r.paintLocked()
	r.mu.Unlock()
}

func (r *ring) setMuted(muted bool) {
	r.mu.Lock()
	r.muted = muted
	r.paintLocked()
	r.mu.Unlock()
}

func (r *ring) setAudioLevel(v float64) {
	v = min(1, max(0, v))
	r.level.Store(math.Float64bits(v))
}

func (r *ring) audioLevel() float64 { return math.Float64frombits(r.level.Load()) }

// showVolume temporarily makes the physical volume arc the top local layer.
func (r *ring) showVolume(level int) {
	level = max(volumeMin, min(volumeMax, level))
	span := volumeMax - volumeButtonFloor
	lit := (level - volumeButtonFloor) * numLEDs / span
	if lit < 1 && level > volumeMin {
		lit = 1
	}
	lit = min(lit, numLEDs)
	frame := blackFrame()
	for i := range lit {
		frame[i] = led.Led{ID: i, G: 200, B: 200}
	}

	r.mu.Lock()
	for i := range r.arcFrame {
		r.arcFrame[i] = frame[i]
	}
	r.arc = true
	if r.arcTimer != nil {
		r.arcTimer.Stop()
	}
	r.arcTimer = time.AfterFunc(volumeDisplay, func() {
		r.mu.Lock()
		r.arc = false
		r.arcTimer = nil
		r.paintLocked()
		r.mu.Unlock()
	})
	r.paintLocked()
	r.mu.Unlock()
}

func (r *ring) cancelVolume() {
	r.mu.Lock()
	if r.arcTimer != nil {
		r.arcTimer.Stop()
		r.arcTimer = nil
	}
	r.arc = false
	r.paintLocked()
	r.mu.Unlock()
}

func (r *ring) displayActive() bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.arc
}

func (r *ring) paintLocked() {
	if r.leds == nil {
		return
	}
	var frame []led.Led
	switch {
	case r.arc:
		frame = r.arcFrame[:]
	case r.muted:
		frame = solidFrame(180, 0, 0)
	default:
		for layer := Layer(layerCount - 1); ; layer-- {
			if r.layer[layer].active {
				frame = r.layer[layer].frame[:]
				break
			}
			if layer == 0 {
				break
			}
		}
		if frame == nil {
			frame = blackFrame()
		}
	}
	if err := r.leds.SetLEDs(frame...); err != nil {
		log.Printf("[ring] paint: %v", err)
	}
}
