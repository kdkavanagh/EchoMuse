// Package capture assigns capture epochs and sample indices to 80 ms
// microphone callbacks (SPEC §4.2, §4.3): each block's anchor is its
// CLOCK_MONOTONIC completion stamp backdated by the block duration and marked
// estimated; lost callbacks become explicit missing ranges sized from elapsed
// time; a clock-fit reset, privacy transition, or restart starts a new epoch.
package capture

import (
	"crypto/rand"
	"encoding/binary"

	"github.com/wilbowes/EchoMuse/internal/audio/clockfit"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

// Capture constants (§4.1).
const (
	PeriodSamples = 1280
	RateHz        = ema.RateCapture
	SampleNs      = 1_000_000_000 / RateHz
	PeriodNs      = PeriodSamples * SampleNs // 80 ms
)

// Reason says why an epoch started; values are the WIRE stream.open reasons.
type Reason string

const (
	ReasonStart         Reason = "start"
	ReasonDiscontinuity Reason = "discontinuity"
	ReasonPrivacy       Reason = "privacy"
	ReasonClockReset    Reason = "clock_reset"
)

// Range is a missing range [From, To) of capture samples; MonoNs is the
// estimated time of From.
type Range struct {
	From, To uint64
	MonoNs   int64
}

// Block is one indexed capture callback.
type Block struct {
	Epoch    uint64
	NewEpoch bool   // First is sample 0 of a new epoch
	Reason   Reason // why the epoch started; set when NewEpoch
	First    uint64
	PCM      []int16 // borrowed from the caller of Add
	MonoNs   int64   // estimated time of PCM[0]
	// UncertaintyUs is the capture clock-fit uncertainty (§4.3 floor: one
	// capture period, plus the fit's RMS residual).
	UncertaintyUs uint32
	Flags         uint8 // ema.FlagEstimated, plus FlagDiscontinuity after a missing range
	// Missing precedes First when HasMissing: it ends exactly at First.
	Missing    Range
	HasMissing bool
}

// Timeline indexes one microphone's callbacks. Owned by the capture
// goroutine; Fit may be read concurrently.
type Timeline struct {
	fit      *clockfit.Fit
	newEpoch func() uint64

	epoch    uint64
	pending  Reason // non-empty: the next block starts a new epoch
	prepared bool   // epoch was allocated before the first block
	next     uint64 // index of the next sample
	lastDone int64  // completion stamp of the previous block
}

// NewTimeline returns a timeline whose first block starts an epoch with
// reason ReasonStart.
func NewTimeline() *Timeline {
	return &Timeline{
		fit:      clockfit.New(RateHz, PeriodNs),
		newEpoch: randomEpoch,
		pending:  ReasonStart,
	}
}

// Fit is the capture clock fit of the current epoch.
func (t *Timeline) Fit() *clockfit.Fit { return t.fit }

// Epoch is the current capture epoch, zero before the first block.
func (t *Timeline) Epoch() uint64 { return t.epoch }

// Restart makes the next block start a new epoch for reason (recorder
// restart). Use RestartNow when the new epoch must be announced immediately.
func (t *Timeline) Restart(reason Reason) {
	t.pending, t.prepared = reason, false
}

// RestartNow allocates and resets a new epoch immediately; the next Add
// supplies its first clock anchor. Privacy unmute uses this so
// privacy.changed and stream.open can name the epoch before audio resumes.
func (t *Timeline) RestartNow(reason Reason) uint64 {
	t.fit.Reset()
	t.epoch = t.newEpoch()
	t.pending, t.prepared = reason, true
	t.next, t.lastDone = 0, 0
	return t.epoch
}

// Add indexes a callback whose buffer completed at doneNs (CLOCK_MONOTONIC).
func (t *Timeline) Add(pcm []int16, doneNs int64) Block {
	durNs := int64(len(pcm)) * SampleNs
	anchor := doneNs - durNs
	b := Block{PCM: pcm, MonoNs: anchor, Flags: ema.FlagEstimated}

	if t.pending == "" && doneNs < t.lastDone {
		t.pending = ReasonClockReset
	}
	if t.pending == "" {
		b.First = t.next
		if elapsed := doneNs - t.lastDone; 2*elapsed > 3*PeriodNs {
			lost := (elapsed+PeriodNs/2)/PeriodNs - 1
			b.First = t.next + uint64(lost)*PeriodSamples
			b.Missing = Range{From: t.next, To: b.First, MonoNs: t.lastDone}
			b.HasMissing = true
			b.Flags |= ema.FlagDiscontinuity
		}
		if t.fit.Add(b.First, anchor).IsReset() {
			t.pending = ReasonClockReset
			b.First, b.Missing, b.HasMissing = 0, Range{}, false
			b.Flags = ema.FlagEstimated
		}
	}
	if t.pending != "" {
		t.fit.Reset()
		if !t.prepared {
			t.epoch = t.newEpoch()
		}
		b.NewEpoch, b.Reason = true, t.pending
		t.pending, t.prepared = "", false
		t.fit.Add(0, anchor)
	}

	b.Epoch = t.epoch
	unc, _ := t.fit.UncertaintyNs(0)
	b.UncertaintyUs = uint32(min((unc+999)/1000, int64(ema.UncertaintyUnknown-1)))
	t.next = b.First + uint64(len(pcm))
	t.lastDone = doneNs
	return b
}

// randomEpoch draws a random nonzero epoch (§16.1).
func randomEpoch() uint64 {
	var b [8]byte
	for {
		if _, err := rand.Read(b[:]); err != nil {
			panic("capture: crypto/rand: " + err.Error())
		}
		if v := binary.LittleEndian.Uint64(b[:]); v != 0 {
			return v
		}
	}
}
