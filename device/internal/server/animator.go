package server

import (
	"log"
	"math"
	"time"

	"github.com/wilbowes/EchoMuse/pkg/led"
)

// AnimSpec describes a device-rendered ring animation: the retained
// `led_anim` body's "anim" object (WIRE §4.8), also used for the device's own
// link and alert indications. Frames are rendered on the device's ticker.
type AnimSpec struct {
	Pattern AnimPattern `json:"pattern"`
	// Colors per pattern: solid — palette 1:1 (one colour fills the ring);
	// spin — [head, trail]; rotate — palette rotated one LED per frame;
	// pulse/meter — palette whose brightness is modulated.
	Colors [][3]uint8 `json:"colors"`
	// PeriodMs: spin/rotate frame interval (0 → 80 ms); pulse cycle
	// (0 → 2,000 ms). Meter ticks every 40 ms.
	PeriodMs int `json:"periodMs"`
	// TTLSec clears the layer if no newer spec replaces it; 0 means none.
	TTLSec int `json:"ttlSec"`

	// Meter response curve; nil selects meterDefaults. Pointers because 0 is
	// a legitimate value for each.
	Attack *float64 `json:"attack"` // envelope rise coefficient per 40 ms tick
	Decay  *float64 `json:"decay"`  // envelope fall coefficient per 40 ms tick
	Floor  *float64 `json:"floor"`  // perceptual brightness at silence
	Gamma  *float64 `json:"gamma"`  // output gamma; >1 expands the dark end
	Ref    *float64 `json:"ref"`    // RMS mapped to full scale
	Curve  *float64 `json:"curve"`  // input exponent; <1 lifts quiet detail
}

// AnimPattern is an AnimSpec pattern.
type AnimPattern string

const (
	PatternOff    AnimPattern = "off"
	PatternSolid  AnimPattern = "solid"
	PatternSpin   AnimPattern = "spin"   // head+trail dot
	PatternRotate AnimPattern = "rotate" // palette rotates around the ring
	PatternPulse  AnimPattern = "pulse"  // sinusoidal throb
	PatternMeter  AnimPattern = "meter"  // brightness follows the final-mix level
)

// meterDefaults: decay 0.30 (τ≈133 ms) tracks syllables; the paint is
// (floor+span·env)^gamma, a perceptual target; ref/curve lift quiet
// consonants without squashing speech (docs/led-ring-states.md).
var meterDefaults = struct {
	attack, decay, floor, gamma, ref, curve float64
}{attack: 0.6, decay: 0.30, floor: 0.06, gamma: 2.2, ref: 0.22, curve: 0.7}

const (
	defaultAnimPeriod  = 80 * time.Millisecond
	defaultPulseCycle  = 2 * time.Second
	throbTick          = 40 * time.Millisecond
	pulseMinBrightness = 0.15
)

// resolveMeter returns spec's meter parameters with defaults filled in and
// clamped so a bad config push cannot produce a dead or strobing ring.
func resolveMeter(spec AnimSpec) (attack, decay, floor, gamma, ref, curve float64) {
	pick := func(p *float64, def, lo, hi float64) float64 {
		if p == nil {
			return def
		}
		return math.Min(hi, math.Max(lo, *p))
	}
	attack = pick(spec.Attack, meterDefaults.attack, 0.05, 1.0)
	decay = pick(spec.Decay, meterDefaults.decay, 0.02, 1.0)
	floor = pick(spec.Floor, meterDefaults.floor, 0.0, 0.6)
	gamma = pick(spec.Gamma, meterDefaults.gamma, 1.0, 3.5)
	ref = pick(spec.Ref, meterDefaults.ref, 0.02, 1.0)
	curve = pick(spec.Curve, meterDefaults.curve, 0.3, 2.0)
	return
}

// frameFunc renders tick n of an animation.
type frameFunc func(n int, elapsed time.Duration) []led.Led

// animate renders spec on layer until the layer is replaced or the TTL
// expires. It returns immediately; frames are painted from a goroutine.
func (r *ring) animate(layer Layer, spec AnimSpec) {
	gen := r.claim(layer)
	var ttl time.Duration
	if spec.TTLSec > 0 {
		ttl = time.Duration(spec.TTLSec) * time.Second
	}
	switch spec.Pattern {
	case PatternOff:
		r.clearGen(layer, gen)
	case PatternSolid:
		r.update(layer, gen, paletteFrame(spec.Colors))
		if ttl > 0 {
			time.AfterFunc(ttl, func() { r.expire(layer, gen) })
		}
	case PatternSpin, PatternRotate:
		period := defaultAnimPeriod
		if spec.PeriodMs > 0 {
			period = time.Duration(spec.PeriodMs) * time.Millisecond
		}
		go r.run(layer, gen, period, ttl, func(n int, _ time.Duration) []led.Led {
			return animFrame(spec, n%numLEDs)
		})
	case PatternPulse:
		cycle := defaultPulseCycle
		if spec.PeriodMs > 0 {
			cycle = time.Duration(spec.PeriodMs) * time.Millisecond
		}
		base := paletteFrame(spec.Colors)
		go r.run(layer, gen, throbTick, ttl, func(_ int, elapsed time.Duration) []led.Led {
			phase := float64(elapsed) / float64(cycle)
			b := pulseMinBrightness + (1-pulseMinBrightness)*(0.5-0.5*math.Cos(2*math.Pi*phase))
			return scaleFrame(base, b)
		})
	case PatternMeter:
		base := paletteFrame(spec.Colors)
		attack, decay, floor, gamma, ref, curve := resolveMeter(spec)
		span := 1.0 - floor
		env := 0.0
		go r.run(layer, gen, throbTick, ttl, func(int, time.Duration) []led.Led {
			level := math.Pow(math.Min(1, r.audioLevel()/ref), curve)
			if level > env {
				env += attack * (level - env)
			} else {
				env += decay * (level - env)
			}
			return scaleFrame(base, math.Pow(floor+span*env, gamma))
		})
	default:
		log.Printf("[ring] unknown animation pattern %q; clearing layer", spec.Pattern)
		r.clearGen(layer, gen)
	}
}

// run paints frames every period until the layer's generation moves on or
// ttl (when nonzero) elapses, which clears the layer.
func (r *ring) run(layer Layer, gen uint64, period, ttl time.Duration, frame frameFunc) {
	start := time.Now()
	ticker := time.NewTicker(period)
	defer ticker.Stop()
	for n := 0; ; n++ {
		elapsed := time.Since(start)
		if ttl > 0 && elapsed >= ttl {
			r.expire(layer, gen)
			return
		}
		if !r.update(layer, gen, frame(n, elapsed)) {
			return
		}
		<-ticker.C
	}
}

func (r *ring) expire(layer Layer, gen uint64) {
	if r.clearGen(layer, gen) {
		log.Printf("[ring] layer %d animation TTL expired; cleared", layer)
	}
}

// scaleFrame returns frame with every channel scaled by b (0..1).
func scaleFrame(frame []led.Led, b float64) []led.Led {
	out := make([]led.Led, len(frame))
	for i, l := range frame {
		out[i] = led.Led{
			ID: l.ID,
			R:  uint8(float64(l.R)*b + 0.5),
			G:  uint8(float64(l.G)*b + 0.5),
			B:  uint8(float64(l.B)*b + 0.5),
		}
	}
	return out
}

// animFrame renders frame pos of a spin/rotate animation.
func animFrame(spec AnimSpec, pos int) []led.Led {
	frame := blackFrame()
	switch spec.Pattern {
	case PatternSpin:
		var head, trail [3]uint8
		if len(spec.Colors) > 0 {
			head = spec.Colors[0]
		}
		if len(spec.Colors) > 1 {
			trail = spec.Colors[1]
		}
		frame[pos%numLEDs] = led.Led{ID: pos % numLEDs, R: head[0], G: head[1], B: head[2]}
		p := (pos + numLEDs - 1) % numLEDs
		frame[p] = led.Led{ID: p, R: trail[0], G: trail[1], B: trail[2]}
	case PatternRotate:
		n := len(spec.Colors)
		if n == 0 {
			return frame
		}
		for i := range frame {
			c := spec.Colors[((i-pos)%n+n)%n]
			frame[i].R, frame[i].G, frame[i].B = c[0], c[1], c[2]
		}
	}
	return frame
}

// paletteFrame maps a colour list onto the ring: one colour fills the whole
// ring, otherwise colours map per LED and the rest stay dark.
func paletteFrame(colors [][3]uint8) []led.Led {
	frame := blackFrame()
	for i := range frame {
		var c [3]uint8
		switch {
		case len(colors) == 1:
			c = colors[0]
		case i < len(colors):
			c = colors[i]
		}
		frame[i].R, frame[i].G, frame[i].B = c[0], c[1], c[2]
	}
	return frame
}

func solidFrame(r, g, b uint8) []led.Led {
	return paletteFrame([][3]uint8{{r, g, b}})
}

func blackFrame() []led.Led {
	frame := make([]led.Led, numLEDs)
	for i := range frame {
		frame[i].ID = i
	}
	return frame
}
