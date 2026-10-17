package slmic

import "github.com/wilbowes/EchoMuse/internal/audio/clockfit"

// fitClock stamps AAudio capture periods (docs/echo-show-lineage-plan.md
// decision 4). AAudio's getTimestamp pairs a frame position with the
// CLOCK_MONOTONIC time the HAL captured it, which stamps sample time, unlike
// AFE_TIMESTAMP (AFE processing time, in bursts). Single pairs scatter
// 15–16 ms peak to peak around a straight line, one 256-frame ALSA period,
// and sit about 5 ms late: read completion minus the stamped capture time
// of the read's last frame was −7.9 ms at least and −4.9 ms at the median
// over five opens, and a negative lag is impossible (docs/echo-show-crown.md
// §8.4 Q6). So every pair goes into a least-squares fit over the last 10 s
// (internal/audio/clockfit), and a period's stamp is the fit's time for its
// last frame less captureBiasNs. Like every capture stamp, that marks the
// period's last sample (pkg/mic.Block.MonoNs).
//
// Before the first pair (a fresh stream runs a read or two before AAudio has
// one), and when the fitted stamp lies more than maxFitLagNs before the read
// completion, which only a stale or broken fit could produce, the period is
// stamped at its read completion.
//
// Stamps are then held to periodClock's two rules: never later than the
// read completion, and never earlier than the previous stamp, since
// capture.Timeline reads a backwards step as a clock reset.
//
// A re-opened stream counts frames from zero again: reset discards the fit,
// so no pair from the old stream is ever mapped onto the new one's frames.
// The previous stamp is kept, so stamps stay monotonic across the re-open.
//
// No cgo, no build tag: host-testable.
type fitClock struct {
	fit  *clockfit.Fit
	last int64 // the previous stamp; 0 before the first

	// The last pair added, so a pair AAudio repeats is added once.
	lastPos, lastNs int64
	havePair        bool
}

const (
	// captureBiasNs is how late AAudio's capture timestamps run on the
	// Show under LineageOS 18.1: the median completion lag of findings
	// §8.4 Q6, −4.5 to −5.0 ms over five opens.
	captureBiasNs = 5_000_000

	// maxFitLagNs bounds how far before its read completion a fitted stamp
	// may lie. A period is read within a few HAL periods of its capture
	// (the measured lag spans −8 to +13 ms); half a second is generous for
	// a reader that stalled, while a stale fit lands outside it.
	maxFitLagNs = 500_000_000
)

func newFitClock() *fitClock {
	return &fitClock{fit: clockfit.New(16000, periodNs)}
}

// reset forgets the fit, for a re-opened stream whose frames count from 0.
func (c *fitClock) reset() {
	c.fit.Reset()
	c.havePair = false
}

// stamp returns the capture stamp of the period whose last frame is
// endFrame−1 (frames counted from the stream's start), read completing at
// doneNs, given the stream's latest timestamp pair (pos captured at tsNs;
// ok false when AAudio had none).
func (c *fitClock) stamp(doneNs, endFrame, pos, tsNs int64, ok bool) (int64, stampKind) {
	if ok && pos >= 0 && (!c.havePair || pos != c.lastPos || tsNs != c.lastNs) {
		if c.fit.Add(uint64(pos), tsNs).IsReset() {
			c.fit.Add(uint64(pos), tsNs) // a fresh epoch starts at this pair
		}
		c.lastPos, c.lastNs, c.havePair = pos, tsNs, true
	}
	ns, kind := doneNs, stampUnlocked
	if fitted, ok := c.fit.SampleToNs(uint64(max(endFrame-1, 0))); ok {
		fitted -= captureBiasNs
		if doneNs-fitted <= maxFitLagNs {
			ns, kind = fitted, stampAnchored
		} else {
			kind = stampImplausible
		}
	}
	ns = max(min(ns, doneNs), c.last)
	c.last = ns
	return ns, kind
}
