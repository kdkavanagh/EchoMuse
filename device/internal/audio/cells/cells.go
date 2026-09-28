// Package cells computes the device's per-cell loudness records (SPEC §8.1
// per-block flow, §16.6 "Activity and level", §16.1 cell record v1): one
// record per 512-sample cell of canonical capture PCM, carrying the
// remainder of each 80 ms block into the next.
package cells

import (
	"errors"
	"math"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

const (
	cellSamples = ema.CellSamples
	sampleNs    = 1_000_000_000 / ema.RateCapture
	// fullScaleSq converts a sum of int16 squares to a sum of x² with
	// x = int16/32768 (§16.6).
	fullScaleSq = 32768.0 * 32768.0
	// eFloor keeps log10 finite for digital silence (§16.6).
	eFloor = 1e-12
	// batchCells bounds one sink call; a long missing range spans several.
	batchCells = 64
)

// ErrNotContiguous: a block or missing range did not start where the
// previous one ended.
var ErrNotContiguous = errors.New("cells: input does not start at the next sample")

// MaskSource reports the final mix's active-source mask over capture samples
// [cellStart, cellEnd).
type MaskSource interface {
	MaskFor(cellStart, cellEnd uint64) uint8
}

// Sink receives completed cells: cells[i] is cell firstCell+i, and monoNs is
// the estimated CLOCK_MONOTONIC time of cell firstCell's first sample. cells
// is borrowed for the call.
type Sink func(firstCell uint64, cells []ema.Cell, monoNs int64)

// Energy returns the §16.6 loudness E of samples in hundredths of a dB:
// 10·log10(mean(x²)+1e-12), x = int16/32768, rounded and clamped (§16.1).
func Energy(samples []int16) int16 {
	var sq uint64
	for _, s := range samples {
		v := int64(s)
		sq += uint64(v * v)
	}
	return energyOf(sq, len(samples))
}

func energyOf(sumSq uint64, n int) int16 {
	mean := float64(sumSq) / fullScaleSq / float64(n)
	return ema.CellE(10 * math.Log10(mean+eFloor))
}

// Accumulator turns capture blocks and missing ranges of one epoch into cell
// records. It is owned by the capture goroutine.
type Accumulator struct {
	masks MaskSource
	sink  Sink

	next  uint64 // next capture sample expected
	sumSq uint64 // Σ int16² over the samples of the open cell
	gap   bool   // the open cell overlaps a missing range
	muted bool   // the open cell holds a muted sample

	batch      [batchCells]ema.Cell
	n          int
	batchFirst uint64
	batchNs    int64
}

// NewAccumulator returns an accumulator whose epoch starts at sample 0.
func NewAccumulator(masks MaskSource, sink Sink) *Accumulator {
	return &Accumulator{masks: masks, sink: sink}
}

// Reset starts a new epoch at sample first, dropping any open cell. Samples
// of first's cell before first are missing.
func (a *Accumulator) Reset(first uint64) {
	a.next = first
	a.sumSq = 0
	a.gap = first%cellSamples != 0
	a.muted = false
	a.n = 0
}

// Next is the next capture sample the accumulator expects.
func (a *Accumulator) Next() uint64 { return a.next }

// Push adds a capture block whose first sample is first and whose first
// sample was captured at monoNs, emitting every cell it completes.
func (a *Accumulator) Push(first uint64, pcm []int16, monoNs int64, muted bool) error {
	if first != a.next {
		return ErrNotContiguous
	}
	for len(pcm) > 0 {
		take := min(cellSamples-int(a.next%cellSamples), len(pcm))
		for _, s := range pcm[:take] {
			v := int64(s)
			a.sumSq += uint64(v * v)
		}
		a.muted = a.muted || muted
		pcm = pcm[take:]
		a.next += uint64(take)
		if a.next%cellSamples == 0 {
			start := a.next - cellSamples
			a.emit(start, monoNs+(int64(start)-int64(first))*sampleNs)
		}
	}
	a.flush()
	return nil
}

// Missing records the missing range [from, to), whose first sample would have
// been captured at monoNs. Every cell it overlaps is a gap cell.
func (a *Accumulator) Missing(from, to uint64, monoNs int64) error {
	if from != a.next {
		return ErrNotContiguous
	}
	for a.next < to {
		a.gap = true
		step := min(cellSamples-a.next%cellSamples, to-a.next)
		a.next += step
		if a.next%cellSamples == 0 {
			start := a.next - cellSamples
			a.emit(start, monoNs+(int64(start)-int64(from))*sampleNs)
		}
	}
	a.flush()
	return nil
}

// emit closes the cell starting at capture sample start.
func (a *Accumulator) emit(start uint64, startNs int64) {
	var flags uint8
	if a.muted {
		flags |= ema.CellMuted
	}
	mask := a.masks.MaskFor(start, start+cellSamples)
	var c ema.Cell
	if a.gap {
		c = ema.GapCell(flags, mask)
	} else {
		c = ema.Cell{E: energyOf(a.sumSq, cellSamples), Flags: flags, Mask: mask}
	}
	a.sumSq, a.gap, a.muted = 0, false, false

	if a.n == 0 {
		a.batchFirst = start / cellSamples
		a.batchNs = startNs
	}
	a.batch[a.n] = c
	a.n++
	if a.n == batchCells {
		a.flush()
	}
}

func (a *Accumulator) flush() {
	if a.n == 0 {
		return
	}
	a.sink(a.batchFirst, a.batch[:a.n], a.batchNs)
	a.n = 0
}
