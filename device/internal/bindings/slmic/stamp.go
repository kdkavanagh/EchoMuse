package slmic

import "github.com/wilbowes/EchoMuse/internal/audio/afe"

// periodClock stamps OpenSL capture periods (docs/echo-show-port-plan.md
// decision 4). OpenSL ES gives a capture buffer no timestamp, only the
// moment its completion callback ran, which trails the audio by a jittery
// 7–41 ms (median 18; docs/echo-show-crown.md §4.4). The AFE does better:
// each v3.3 frame carries AFE_TIMESTAMP, CLOCK_MONOTONIC milliseconds modulo
// 65,536, which sat 0–27 ms (median 15) before the fitted completion of its
// last sample in the same captures. So a period whose decoder accepted a
// frame is stamped from that frame: its AFE_TIMESTAMP, unwrapped to the
// largest value at or before the read completion that is congruent to it
// modulo 65,536 ms, plus the samples of the period after its final bit at
// 16 kHz. Like every capture stamp, that marks the period's last sample
// (pkg/mic.Block.MonoNs).
//
// A period with no accepted frame (the decoder hunting, a stream without
// metadata) is stamped at its read completion, as is one whose unwrapped
// AFE_TIMESTAMP lies more than maxAnchorLagMs before it, which only a
// different clock or a misread could produce.
//
// Stamps are then held to two rules: never later than the read completion
// (the audio cannot postdate its own delivery), and never earlier than the
// previous stamp, since capture.Timeline reads a backwards step as a clock
// reset and starts a new epoch. Completions come from CLOCK_MONOTONIC in
// callback order, so the two never conflict.
//
// No cgo, no build tag: host-testable.
type periodClock struct {
	last int64 // the previous stamp; 0 before the first
}

// stampKind says where a stamp came from.
type stampKind uint8

const (
	stampAnchored    stampKind = iota // the period's AFE frame
	stampUnlocked                     // no frame in the period: read completion
	stampImplausible                  // anchor too far before completion: read completion
)

const (
	sampleNs = 1_000_000_000 / 16000 // one 16 kHz sample

	// afeTimestampMod is AFE_TIMESTAMP's modulus, in ms.
	afeTimestampMod = 1 << 16

	// maxAnchorLagMs bounds how long before its read completion a frame's
	// AFE_TIMESTAMP may lie. Measured lags are 7–41 ms; a stalled callback
	// thread delivers late, and the recorder queues at most a few periods
	// (opensl.NewRecorder), so half a second is generous for real delivery
	// while a clock or misread error lands outside it 99 % of the time.
	maxAnchorLagMs = 500
)

// stamp returns the capture stamp of a period whose read completed at
// doneNs (CLOCK_MONOTONIC), given its decoded AFE anchor.
func (c *periodClock) stamp(doneNs int64, a afe.Anchor) (int64, stampKind) {
	ns, kind := doneNs, stampUnlocked
	if a.Valid {
		doneMs := doneNs / 1_000_000
		lag := (doneMs - int64(a.TimestampMs)) % afeTimestampMod
		if lag < 0 {
			lag += afeTimestampMod
		}
		if lag <= maxAnchorLagMs {
			ns = (doneMs-lag)*1_000_000 + int64(a.After)*sampleNs
			kind = stampAnchored
		} else {
			kind = stampImplausible
		}
	}
	ns = max(min(ns, doneNs), c.last)
	c.last = ns
	return ns, kind
}
