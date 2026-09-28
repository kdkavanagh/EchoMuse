// Package refdsp decimates the device's 48 kHz final mix into the 16 kHz
// reference stream (SPEC §16.1 "Reference stream"): a 127-tap
// Hamming-windowed sinc FIR, 7,200 Hz cutoff, factor 3. Reference sample k is
// y[k] = Σ h[n]·x[3k+63−n], aligned with render sample 3k and emitted once
// render sample 3k+63 exists. Samples k < 21 reach before the render epoch
// and are reported missing. No normalization is applied.
package refdsp

import (
	"errors"
	"math"
)

// §16.1 filter constants.
const (
	Taps     = 127
	Factor   = 3
	Delay    = (Taps - 1) / 2 // 63: centre tap, render samples of lookahead
	CutoffHz = 7200.0
	RenderHz = 48000.0
	// FirstValid is the first reference sample whose sum lies inside the
	// render epoch: 3k+63−126 ≥ 0.
	FirstValid = (Taps - 1 - Delay + Factor - 1) / Factor // 21
)

// Coeffs is h[n], n = 0..126, generated once and normalized to sum 1.
var Coeffs = coefficients()

func coefficients() [Taps]float64 {
	var h [Taps]float64
	fc := 2 * CutoffHz / RenderHz
	var sum float64
	for n := range h {
		x := fc * float64(n-Delay)
		sinc := 1.0
		if x != 0 {
			sinc = math.Sin(math.Pi*x) / (math.Pi * x)
		}
		h[n] = fc * sinc * (0.54 - 0.46*math.Cos(2*math.Pi*float64(n)/float64(Taps-1)))
		sum += h[n]
	}
	for n := range h {
		h[n] /= sum
	}
	return h
}

var (
	// ErrNotContiguous: a block did not start at the next render sample of
	// the epoch; the caller restarts with Reset.
	ErrNotContiguous = errors.New("refdsp: block does not continue the render epoch")
	// ErrShortDst: dst cannot hold MaxOutput(len(pcm)) samples.
	ErrShortDst = errors.New("refdsp: output buffer too short")
)

// MaxOutput is the most reference samples one Push of n render samples emits.
func MaxOutput(n int) int { return (n + Factor - 1) / Factor }

// Output describes one Push. Samples [MissingFrom, MissingTo) of the
// reference epoch are missing (only ever within k < 21); Samples holds
// reference samples First.. and aliases the caller's dst. Mask is the union of
// the source masks of the render blocks the output samples are aligned with.
type Output struct {
	MissingFrom, MissingTo uint64
	First                  uint64
	Samples                []int16
	Mask                   uint8
}

// Decimator carries FIR history across render blocks of one render epoch.
// Owned by the mixer-tap goroutine; Push does not allocate.
type Decimator struct {
	line     [2 * Taps]float64 // doubled delay line: line[pos+1:pos+1+Taps] is the window
	masks    [2 * Taps]uint8   // active-source mask parallel to line
	pos      int
	next     uint64 // next render sample expected
	resolved uint64 // reference samples [0, resolved) are emitted or missing
}

// Reset starts a new render (and so reference) epoch at render sample 0.
func (d *Decimator) Reset() { *d = Decimator{} }

// Push feeds render samples [first, first+len(pcm)) carrying mask and writes
// every reference sample they complete into dst.
func (d *Decimator) Push(first uint64, pcm []int16, mask uint8, dst []int16) (Output, error) {
	if first != d.next {
		return Output{}, ErrNotContiguous
	}
	if len(dst) < MaxOutput(len(pcm)) {
		return Output{}, ErrShortDst
	}
	out := Output{First: d.resolved, Samples: dst[:0], MissingFrom: d.resolved, MissingTo: d.resolved}
	for _, s := range pcm {
		d.pos = (d.pos + 1) % Taps
		v := float64(s)
		d.line[d.pos], d.line[d.pos+Taps] = v, v
		d.masks[d.pos], d.masks[d.pos+Taps] = mask, mask
		idx := d.next
		d.next++
		if idx < Delay || (idx-Delay)%Factor != 0 {
			continue
		}
		k := (idx - Delay) / Factor
		d.resolved = k + 1
		if k < FirstValid {
			out.MissingTo = k + 1
			out.First = k + 1
			continue
		}
		out.Samples = append(out.Samples, d.filter())
		out.Mask |= d.windowMask()
	}
	return out, nil
}

func (d *Decimator) windowMask() uint8 {
	var mask uint8
	for _, m := range d.masks[d.pos+1 : d.pos+1+Taps] {
		mask |= m
	}
	return mask
}

// filter evaluates y = Σ h[n]·x[newest−n] over the current window, whose
// oldest sample is line[pos+1] and newest line[pos+Taps].
func (d *Decimator) filter() int16 {
	w := d.line[d.pos+1 : d.pos+1+Taps]
	var acc float64
	for n, h := range Coeffs {
		acc += h * w[Taps-1-n]
	}
	return clamp16(acc)
}

func clamp16(v float64) int16 {
	v = math.Round(v)
	switch {
	case v > math.MaxInt16:
		return math.MaxInt16
	case v < math.MinInt16:
		return math.MinInt16
	default:
		return int16(v)
	}
}

// AllZero reports whether every sample is zero: a reference packet that is
// digital silence (§16.1 flag bit 4).
func AllZero(pcm []int16) bool {
	for _, s := range pcm {
		if s != 0 {
			return false
		}
	}
	return true
}
