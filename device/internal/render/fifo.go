package render

import "math"

// sampleFIFO is a fixed-capacity ring of mono samples, preallocated once per
// network class and reused by successive playbacks.
type sampleFIFO struct {
	buf  []int16
	head int
	n    int
}

func newSampleFIFO(capacity int) sampleFIFO { return sampleFIFO{buf: make([]int16, capacity)} }

func (q *sampleFIFO) reset() { q.head, q.n = 0, 0 }

func (q *sampleFIFO) free() int { return len(q.buf) - q.n }

// push appends src whole, or nothing when it does not fit.
func (q *sampleFIFO) push(src []int16) bool {
	if len(src) > q.free() {
		return false
	}
	tail := (q.head + q.n) % len(q.buf)
	c := copy(q.buf[tail:], src)
	copy(q.buf, src[c:])
	q.n += len(src)
	return true
}

func (q *sampleFIFO) pop() int16 {
	v := q.buf[q.head]
	q.head++
	if q.head == len(q.buf) {
		q.head = 0
	}
	q.n--
	return v
}

// gainRamp moves linearly to its target over a fixed number of samples,
// whatever the distance, so ramp duration is exact and independent of block
// and write size (SPEC §4.1).
type gainRamp struct {
	current float64
	target  float64
	step    float64
	left    int
}

func (g *gainRamp) jump(v float64) { g.current, g.target, g.step, g.left = v, v, 0, 0 }

func (g *gainRamp) set(target float64, frames int) {
	if target == g.target {
		return
	}
	g.target = target
	g.left = frames
	g.step = (target - g.current) / float64(frames)
}

// next returns the gain for the next sample and advances the ramp.
func (g *gainRamp) next() float64 {
	if g.left == 0 {
		return g.current
	}
	g.current += g.step
	g.left--
	if g.left == 0 {
		g.current = g.target
	}
	return g.current
}

// silent reports that the gain is settled at zero.
func (g *gainRamp) silent() bool { return g.left == 0 && g.current == 0 }

func dbToGain(db float64) float64 {
	if math.IsInf(db, -1) {
		return 0
	}
	return math.Pow(10, db/20)
}
