package slspeaker

import "math"

// Ducking and mixing for the two playback streams, mono variant.
//
// This is deliberately the same algorithm as internal/bindings/speaker/mix.go
// — same ramp, same saturation, same reasoning, ported to mono because the
// wire is duplicated to stereo only for tinyalsa's I2S/codec path (see
// CLAUDE.md's speaker plane note); OpenSL ES takes the mono channel directly,
// so there is nothing to duplicate here. It is a deliberate near-duplicate
// rather than a shared package: the frame width differs (2 bytes here, 4
// there) at exactly the one place — applyGain — where that matters, and
// pulling the rest out for one differing loop was judged not worth new
// cross-package plumbing during this migration. If both backends end up
// shipping, extracting the identical parts (Mixer's dispatch, DuckGain,
// mixInto, the ramp) into a shared internal package is the obvious follow-up.
//
// No build tag and no OpenSL import, deliberately — same as the tinyalsa
// version, this is the arithmetic worth testing on the host.

// unityGain: Q15 fixed point, 32768 == unity. See speaker.unityGain for why
// (integer maths, exact at unity).
const unityGain int32 = 1 << 15

// duckRampPeriods / rampStep: identical reasoning to speaker.duckRampPeriods
// — a CONSTANT SLEW (not proportional), so "N periods" is a duration rather
// than a time constant. Kept as a period count rather than a wall-clock
// value because slspeaker's period size is whatever the wire sends (see
// slspeaker.go), matching the tinyalsa backend's own periods-not-ms framing.
const duckRampPeriods = 4
const rampStep = unityGain / duckRampPeriods

// Mixer combines the voice and music streams for one output period.
//
// Single-consumer by contract: only the pump goroutine (slspeaker.go) touches
// it, so nothing here is synchronised. The gain TARGET is set from elsewhere
// and needs atomicity — it lives in Speaker, not here, same split as
// speaker.PcmSpeaker/Mixer.
type Mixer struct {
	gain int32 // current, Q15, ramps toward the target
}

// DuckGain converts decibels of attenuation to Q15. 0dB returns exactly
// unity rather than a rounded conversion, so "not ducked" is bit-identical
// to the input.
func DuckGain(db float64) int32 {
	if db >= 0 {
		return unityGain
	}
	g := math.Pow(10, db/20.0) * float64(unityGain)
	if g < 0 {
		return 0
	}
	return int32(g + 0.5)
}

// Gain reports the current (ramped) gain, for tests.
func (m *Mixer) Gain() int32 { return m.gain }

// SetGainImmediate jumps the ramp to a value, used at stream start where
// there is no audio to click.
func (m *Mixer) SetGainImmediate(g int32) { m.gain = g }

// Mix produces one output period from whichever streams have audio. Both
// buffers are owned by the caller once dequeued; mixing happens IN PLACE.
// Returns the buffer to write, or nil when there is nothing to play.
func (m *Mixer) Mix(voice, music []byte, target int32) []byte {
	switch {
	case voice == nil && music == nil:
		// Still settle the ramp: a duck requested while nothing plays must
		// not be left half-applied for the next period of audio.
		m.stepGain(target)
		return nil
	case music == nil:
		m.stepGain(target) // voice alone is never ducked
		return voice
	case voice == nil:
		m.applyGain(music, target)
		return music
	default:
		m.applyGain(music, target)
		mixInto(voice, music)
		return voice
	}
}

func (m *Mixer) stepGain(target int32) {
	switch {
	case m.gain < target:
		if m.gain += rampStep; m.gain > target {
			m.gain = target
		}
	case m.gain > target:
		if m.gain -= rampStep; m.gain < target {
			m.gain = target
		}
	}
}

// applyGain scales a mono S16LE period, ramping per SAMPLE across it — a gain
// step at a period boundary is an audible click, landing on exactly the
// transition the user is listening to.
func (m *Mixer) applyGain(buf []byte, target int32) {
	start := m.gain
	m.stepGain(target)
	end := m.gain

	frames := len(buf) / 2 // mono S16
	if frames == 0 {
		return
	}
	for i := 0; i < frames; i++ {
		g := start
		if end != start {
			g = start + (end-start)*int32(i)/int32(frames)
		}
		off := i * 2
		s := int32(int16(uint16(buf[off]) | uint16(buf[off+1])<<8))
		s = (s * g) >> 15
		buf[off] = byte(uint16(s) & 0xff)
		buf[off+1] = byte(uint16(s) >> 8)
	}
}

// mixInto sums music into voice with saturation — identical to
// speaker.mixInto; this loop was never stereo-specific (it walks raw 2-byte
// samples regardless of channel layout), so it carries over unchanged.
//
// Saturating rather than wrapping: an int16 overflow wraps a loud peak to
// full-scale opposite polarity, far worse than the clipping it replaces.
func mixInto(dst, src []byte) {
	n := len(dst)
	if len(src) < n {
		n = len(src)
	}
	for i := 0; i+1 < n; i += 2 {
		a := int32(int16(uint16(dst[i]) | uint16(dst[i+1])<<8))
		b := int32(int16(uint16(src[i]) | uint16(src[i+1])<<8))
		s := a + b
		if s > math.MaxInt16 {
			s = math.MaxInt16
		} else if s < math.MinInt16 {
			s = math.MinInt16
		}
		dst[i] = byte(uint16(s) & 0xff)
		dst[i+1] = byte(uint16(s) >> 8)
	}
}
