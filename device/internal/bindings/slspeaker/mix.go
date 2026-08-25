package slspeaker

import "math"

// Ducking and mixing for the two playback streams.
//
// No build tag and no OpenSL import, deliberately: this is the arithmetic the
// write path runs on every period, and it is the part worth testing on the
// host. slspeaker.go is `//go:build server` and cannot be compiled or tested
// anywhere but the device.
//
// WHY THE DEVICE MIXES AT ALL. Barge-in used to pause the music, answer, and
// resume. Every assistant people compare us to ducks instead, and pausing
// carried real bugs with it: a Music Assistant flow stream cannot be seeked,
// so a 28-second turn cost 28 seconds of the song, and the media_player
// entity had to report PLAYING while internally paused (#62) or Home
// Assistant would decline the user's own pause.
//
// It has to happen HERE rather than in the controller because of `LEAD_S`:
// the music feed runs 4 seconds ahead of realtime, so when a wake word fires
// the next 4 seconds of music are already in this device's buffer. Audio
// that has left the controller cannot be ducked by the controller. Doing it
// on the device means the music keeps its full ~5.5s of link-stall
// protection AND ducking is instant, because the gain is applied to audio we
// are already holding.

// Q15 fixed point: 32768 == unity. Integer maths on a 32-bit A53 with no
// FPU pressure on the audio path, and exact at unity — a float multiply
// would leave a stream that is nominally not ducked very slightly altered.
const unityGain int32 = 1 << 15

// duckRampPeriods — periods taken to traverse the FULL gain range. A period
// is ~42.7ms, so 4 gives a ~170ms ramp for a duck from unity to silence, and
// proportionally less for a shallower one. Fast enough that the duck lands
// with the wake word, slow enough to be a fade rather than a step: stepping
// the gain at a period boundary is an audible click, landing on exactly the
// transition the user is listening to.
//
// A CONSTANT SLEW, not a proportional one. `(target-gain)/n` per period is
// an exponential approach: 4 is then a time constant, not a duration, and
// the gain crawls the last few percent for over a second — measured at 31
// periods (1.3s) to settle, against the 170ms this comment used to claim.
//
// A period count rather than a wall-clock value, because the period size is
// whatever the wire sends.
const duckRampPeriods = 4

// rampStep is the most the gain may move in one period.
const rampStep = unityGain / duckRampPeriods

// Mixer combines the voice and music streams for one output period.
//
// Single-consumer by contract: only the pump goroutine (slspeaker.go) touches
// it, so nothing here is synchronised. The gain TARGET is set from elsewhere
// and is the one field that needs atomicity — it lives in Speaker, not here.
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

// mixCue adds a device-local notification sound to the period about to be
// written, returning the buffer to write.
//
// A cue is neither ducked nor a stream: it is not audio from the controller,
// so it has no prime gate, no discard-until-EOS and no StreamStats, and it
// does not fade the music under it — a 170ms dip around a chime is a worse
// artefact than the chime. It IS mixed before the point the HAL takes its
// far-end reference, because the room hears it and the wake listener must
// not.
//
// Returning cueMono itself when there is nothing else playing is safe
// because it is the pump loop's own scratch buffer and Player.Next refills
// it in place; opensl.Player.Write copies before returning.
func mixCue(out, cueMono []byte) []byte {
	if len(cueMono) == 0 {
		return out
	}
	if out == nil {
		return cueMono
	}
	mixInto(out, cueMono)
	return out
}

// mixInto sums music into voice with saturation. It walks raw 2-byte samples
// regardless of channel layout.
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
