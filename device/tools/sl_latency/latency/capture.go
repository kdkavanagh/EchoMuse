// Package latency is the analysis half of device/tools/sl_latency. It keeps
// the capture the firmware's OpenSL microphone delivered, with the stamps the
// firmware gave it, decodes the AFE metadata frame by frame, and finds the
// bursts the tool played: in the AEC residual, and where the AFE's
// PLAYBACK_ACTIVE and ERLE_RAW rose.
//
// No cgo, no build tag: host-testable.
package latency

import (
	"sort"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
)

const (
	// CaptureRate is the microphone's rate (pkg/mic).
	CaptureRate = 16000
	// SampleNs is one capture sample.
	SampleNs = 1_000_000_000 / CaptureRate
)

// Capture is the capture of one run, period by period as Read delivered it.
// Gaps (drops, re-opens) are not filled: PCM is the delivered samples back
// to back, and every lookup goes through the stamps of the periods.
type Capture struct {
	PCM    []int16
	Frames []Frame // the AFE frames decoded from PCM, in order

	blocks []block
	dec    afe.Decoder
	chunk  afe.Period
}

type block struct {
	start int   // index of the period's first sample in PCM
	n     int   // its length
	endNs int64 // the firmware's stamp: the end of its last sample
}

// Frame is one AFE v3.3 frame decoded from the capture.
type Frame struct {
	StartNs  int64 // capture time of the frame's first sample
	Playback bool  // PLAYBACK_ACTIVE
	ERLE     uint8 // ERLE_RAW
}

// Add appends one period: pcm, which it copies, and its firmware stamp, the
// end of its last sample (pkg/mic.Block.MonoNs). Its frames are decoded one
// 128-sample batch at a time, so each batch holds at most one frame's final
// bit and its afe.Anchor places that frame exactly.
func (c *Capture) Add(pcm []int16, stampNs int64) {
	n := len(pcm)
	c.blocks = append(c.blocks, block{start: len(c.PCM), n: n, endNs: stampNs})
	c.PCM = append(c.PCM, pcm...)
	for off := 0; off < n; off += afe.FrameBits {
		end := min(off+afe.FrameBits, n)
		c.dec.Decode(pcm[off:end], &c.chunk)
		a := c.chunk.Anchor
		if !a.Valid {
			continue
		}
		final := end - 1 - a.After // the frame's final bit, within pcm
		c.Frames = append(c.Frames, Frame{
			StartNs:  stampNs - int64(n-final+afe.FrameBits-1)*SampleNs,
			Playback: c.chunk.Record.Playback > 0,
			ERLE:     c.chunk.Record.ERLEMax,
		})
	}
}

// SampleNs is the capture time of PCM[i]: the start of that sample.
func (c *Capture) SampleNs(i int) int64 {
	k := sort.Search(len(c.blocks), func(k int) bool { return c.blocks[k].start+c.blocks[k].n > i })
	if k == len(c.blocks) {
		k--
	}
	b := c.blocks[k]
	return b.endNs - int64(b.start+b.n-i)*SampleNs
}

// IndexAt is the first sample of PCM whose capture time is at or after ns,
// or len(PCM) when there is none.
func (c *Capture) IndexAt(ns int64) int {
	k := sort.Search(len(c.blocks), func(k int) bool { return c.blocks[k].endNs > ns })
	if k == len(c.blocks) {
		return len(c.PCM)
	}
	b := c.blocks[k]
	first := b.endNs - int64(b.n)*SampleNs
	if ns <= first {
		return b.start
	}
	return b.start + int((ns-first+SampleNs-1)/SampleNs)
}

// CompletionOffsetMs is how long after its AFE-anchored end a period's read
// completed: doneNs minus its last frame's AFE_TIMESTAMP, unwrapped to the
// largest value at or before the completion that is congruent to it modulo
// 65,536 ms, plus the samples after that frame. That is slmic's periodClock
// before its clamps. ok is false when the period had no frame.
func CompletionOffsetMs(doneNs int64, a afe.Anchor) (ms float64, ok bool) {
	if !a.Valid {
		return 0, false
	}
	doneMs := doneNs / 1_000_000
	lag := (doneMs - int64(a.TimestampMs)) % (1 << 16)
	if lag < 0 {
		lag += 1 << 16
	}
	endNs := (doneMs-lag)*1_000_000 + int64(a.After)*SampleNs
	return float64(doneNs-endNs) / 1e6, true
}
