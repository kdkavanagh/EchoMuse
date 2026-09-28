package alerts

import "math"

// Focus gain ramps (SPEC §6.2), in integer level units so ramp lengths are
// exact: full gain is focusFull; a background/foreground transition moves one
// unit per sample (30 ms), a stop moves stopStep units per sample (10 ms).
const (
	focusFull = 30 * sampleRate / 1000
	stopStep  = focusFull / (10 * sampleRate / 1000)
)

// voice is an immutable description of the ring the executor wants audible.
type voice struct {
	id  string
	pcm []int16
	// gap is the silence after each pcm period: loop_gap_ms for assets, zero
	// for the fallback tone, whose period already holds its own gap.
	gap int64
	// ramp is the alarm ramp length in samples (0: none, as for timers).
	ramp int64
	// startElapsed is the ring time already elapsed when this voice starts:
	// nonzero when a restart resumes a ringing occurrence.
	startElapsed int64
}

// mixerState is owned by the goroutine calling Fill.
type mixerState struct {
	cur         *voice
	next        *voice
	switching   bool  // fading cur out at stopStep before starting next
	pos         int64 // position in the pcm+gap loop
	elapsed     int64 // samples since first ring, counting gaps and background
	level       int   // focus gain in units of 1/focusFull
	interrupted bool  // this period was backgrounded or replaced
}

func (m *mixerState) start(v *voice, background bool) {
	m.cur, m.next, m.switching, m.pos = v, nil, false, 0
	m.level = focusFull
	m.interrupted = background
	if background {
		m.level = 0
	}
	if v != nil {
		m.elapsed = v.startElapsed
	}
}

func (m *mixerState) stepFocus(background bool) {
	switch {
	case m.cur == nil:
	case m.switching:
		m.level = max(0, m.level-stopStep)
	case background:
		m.level = max(0, m.level-1)
	default:
		m.level = min(focusFull, m.level+1)
	}
}

// Fill is the render.PullSource of the alert source: it writes len(dst)
// samples of 48 kHz mono PCM16 and reports whether a foreground ring,
// including its loop gaps, occupied any of them. It never blocks or allocates
// and must be called from a single goroutine.
func (e *Executor) Fill(dst []int16) (active bool) {
	m := &e.mix
	background := e.background.Load()
	if want := e.voice.Load(); m.cur == nil {
		if want != nil {
			m.start(want, background)
		}
	} else if want != m.cur {
		m.next, m.switching = want, true
	}
	for i := range dst {
		before := m.level
		m.stepFocus(background)
		if m.switching && m.level == 0 {
			m.start(m.next, background)
		}
		v := m.cur
		if v == nil {
			dst[i] = 0
			continue
		}
		if m.level == 0 {
			// Backgrounded: bursts are silenced; ramp and ring time run on,
			// and the ring resumes at the start of a period.
			dst[i] = 0
			m.pos = 0
			m.interrupted = true
			m.elapsed++
			continue
		}
		if before == 0 && !background {
			m.pos = 0
			m.interrupted = false
		}
		var s int16
		if m.pos < int64(len(v.pcm)) {
			s = v.pcm[m.pos]
		}
		if m.pos++; m.pos == int64(len(v.pcm))+v.gap {
			m.pos = 0
			if !m.interrupted {
				e.burstID.Store(&v.id)
				e.burstSeq.Add(1)
			}
			m.interrupted = false
		}
		g := float64(m.level) / focusFull
		if m.elapsed < v.ramp {
			g *= float64(m.elapsed) / float64(v.ramp)
		}
		m.elapsed++
		dst[i] = int16(math.Round(float64(s) * g))
		active = true
	}
	return active
}

// SetBackground applies alert focus (SPEC §6.2): background silences bursts
// within 30 ms but keeps the occurrence current, its ramp progress, and its
// ring deadline running. It never blocks.
func (e *Executor) SetBackground(background bool) {
	if e.background.Swap(background) != background {
		e.signal()
	}
}

// BurstCompletedSince reports the newest fully foreground loop period after
// seq. A burst interrupted by background focus is not counted (§6.2).
func (e *Executor) BurstCompletedSince(seq uint64) (id string, next uint64, ok bool) {
	next = e.burstSeq.Load()
	if next == seq {
		return "", next, false
	}
	if p := e.burstID.Load(); p != nil {
		return *p, next, true
	}
	return "", next, false
}
