// Package clockfit estimates the affine map between a stream's sample index
// and device CLOCK_MONOTONIC time (SPEC §4.3 "Clock decision"): least squares
// over the latest 10 s of anchors, rejecting anchors whose residual exceeds
// 80 ms, and demanding an epoch reset when the estimated rate leaves nominal
// by more than 1,000 ppm or time moves backwards.
package clockfit

import (
	"math"
	"sync"
)

// SPEC §4.3 constants.
const (
	WindowNs      = 10_000_000_000 // anchors older than this are dropped
	MaxResidualNs = 80_000_000     // anchors further than this from the fit are rejected
	MaxDriftPPM   = 1000           // a larger rate error resets the epoch
)

const (
	// minSlopeSpanNs is the anchor span below which the fit keeps the nominal
	// rate and estimates only the offset. Callback timestamps jitter by
	// milliseconds, so a slope over a shorter span cannot resolve 1,000 ppm.
	minSlopeSpanNs = 5_000_000_000
	// maxAnchors holds 10 s at the densest caller cadence (42.7 ms render
	// writes: 235 anchors).
	maxAnchors = 256
)

// Result is the outcome of one Add.
type Result int

const (
	Accepted       Result = iota
	Rejected              // residual above MaxResidualNs; the fit is unchanged
	ResetBackwards        // time or index moved backwards; the fit is now empty
	ResetDrift            // rate error above MaxDriftPPM; the fit is now empty
)

// IsReset reports whether the caller must start a new stream epoch.
func (r Result) IsReset() bool { return r == ResetBackwards || r == ResetDrift }

type anchor struct {
	sample uint64
	ns     int64
}

// Fit maps sample indices of one stream epoch to monotonic ns and back. It is
// safe for concurrent use.
type Fit struct {
	mu       sync.Mutex
	nominal  float64 // ns per sample at the nominal rate
	periodNs int64   // one callback/write period: the uncertainty floor

	anchors [maxAnchors]anchor // circular, oldest at head
	head    int
	count   int

	// Model: ns(s) = t0 + offset + slope·(s − s0).
	s0     uint64
	t0     int64
	offset float64
	slope  float64
	rmsNs  float64
}

// New returns an empty fit for a stream of nominal rateHz whose anchors are
// taken once per periodNs.
func New(rateHz float64, periodNs int64) *Fit {
	f := &Fit{nominal: 1e9 / rateHz, periodNs: periodNs}
	f.reset()
	return f
}

// Reset discards every anchor, as for a new epoch.
func (f *Fit) Reset() {
	f.mu.Lock()
	f.reset()
	f.mu.Unlock()
}

func (f *Fit) reset() {
	f.head, f.count = 0, 0
	f.offset, f.slope, f.rmsNs = 0, f.nominal, 0
}

func (f *Fit) at(i int) *anchor { return &f.anchors[(f.head+i)%maxAnchors] }

// Add offers the anchor (sample was observed at ns). On a reset result the
// fit is empty; the caller starts a new epoch and re-adds its anchor there.
func (f *Fit) Add(sample uint64, ns int64) Result {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.count > 0 {
		last := f.at(f.count - 1)
		if ns < last.ns || sample < last.sample {
			f.reset()
			return ResetBackwards
		}
	}
	for f.count > 0 && f.at(0).ns < ns-WindowNs {
		f.head = (f.head + 1) % maxAnchors
		f.count--
	}
	if f.count > 0 && math.Abs(float64(ns)-f.predict(sample)) > MaxResidualNs {
		return Rejected
	}
	if f.count == maxAnchors {
		f.head = (f.head + 1) % maxAnchors
		f.count--
	}
	*f.at(f.count) = anchor{sample, ns}
	f.count++
	if f.refit() && math.Abs(f.nominal/f.slope-1)*1e6 > MaxDriftPPM {
		f.reset()
		return ResetDrift
	}
	return Accepted
}

// refit recomputes the model from the retained anchors and reports whether
// the slope was estimated rather than nominal.
func (f *Fit) refit() bool {
	first, last := f.at(0), f.at(f.count-1)
	f.s0, f.t0 = first.sample, first.ns
	n := float64(f.count)
	estimate := f.count >= 2 && last.ns-first.ns >= minSlopeSpanNs

	var mx, my float64
	for i := 0; i < f.count; i++ {
		a := f.at(i)
		mx += float64(a.sample - f.s0)
		my += float64(a.ns - f.t0)
	}
	mx /= n
	my /= n
	f.slope = f.nominal
	if estimate {
		var sxy, sxx float64
		for i := 0; i < f.count; i++ {
			a := f.at(i)
			dx := float64(a.sample-f.s0) - mx
			sxy += dx * (float64(a.ns-f.t0) - my)
			sxx += dx * dx
		}
		if sxx > 0 {
			f.slope = sxy / sxx
		} else {
			estimate = false
		}
	}
	f.offset = my - f.slope*mx

	var ss float64
	for i := 0; i < f.count; i++ {
		a := f.at(i)
		r := float64(a.ns-f.t0) - f.offset - f.slope*float64(a.sample-f.s0)
		ss += r * r
	}
	f.rmsNs = math.Sqrt(ss / n)
	return estimate
}

func (f *Fit) predict(sample uint64) float64 {
	return float64(f.t0) + f.offset + f.slope*float64(int64(sample-f.s0))
}

// SampleToNs maps a sample index to estimated monotonic ns; ok is false
// before the first anchor.
func (f *Fit) SampleToNs(sample uint64) (ns int64, ok bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.count == 0 {
		return 0, false
	}
	return int64(math.Round(f.predict(sample))), true
}

// NsToSample maps monotonic ns to the (possibly negative, i.e. pre-epoch)
// sample index; ok is false before the first anchor.
func (f *Fit) NsToSample(ns int64) (sample int64, ok bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.count == 0 {
		return 0, false
	}
	x := (float64(ns-f.t0) - f.offset) / f.slope
	return int64(f.s0) + int64(math.Round(x)), true
}

// UncertaintyNs is the mapping uncertainty: the §4.3 floor (one period plus
// the caller's outstanding queue time) plus the fit's RMS residual. Unknown
// downstream acoustic delay is not included. ok is false before the first
// anchor.
func (f *Fit) UncertaintyNs(queueNs int64) (ns int64, ok bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.count == 0 {
		return 0, false
	}
	return f.periodNs + queueNs + int64(math.Ceil(f.rmsNs)), true
}

// RateHz is the current rate estimate (nominal until the slope is fitted).
func (f *Fit) RateHz() float64 {
	f.mu.Lock()
	defer f.mu.Unlock()
	return 1e9 / f.slope
}
