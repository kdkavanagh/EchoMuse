// Package afe decodes the native AFE metadata that the Fire OS 6 mixer's
// micAsr capture carries in bit 0 of every sample, and summarises it per
// capture period as an ema.AFERecord (WIRE §3, afe_metadata_v1).
//
// The layout is Amazon's v3.3 (docs/alexa-afe.md, "AFE metadata in bit 0
// (v3.3)"): one 128-bit frame per 128-sample AFE batch (8 ms), most
// significant bit first, phase-locked to the stream start. A frame is valid
// when its sync byte is 0xA5, its version 0x63 (3.3), its PAYLOAD_SIZE 104
// and its checksum byte equals the popcount of bits 127…8. Every frame of
// eleven captures (two boots, both ASR stream types) passed; a 230 k-sample
// stream without metadata produced four sync-and-checksum matches whose
// versions were not 3.3, so the version and payload size are part of the
// test, and a lock additionally needs a second valid frame 128 bits later
// with FRAME_COUNTER + 1.
//
// FRAME_COUNTER (6 bits) steps by exactly one per frame. AFE_TIMESTAMP is
// milliseconds that average 8 per frame but arrive in bursts (steps of 3–16,
// within ±8 ms of the 8 ms grid), so the decoder uses the counter to count
// missing frames and the timestamp only to resolve the counter's modulo-64
// aliasing.
//
// No cgo and no build tag: host-testable. Decode never allocates.
package afe

import (
	"math/bits"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

// FrameBits is the frame length: one bit per sample of a 128-sample batch.
const FrameBits = 128

const (
	syncVersion = 0xA563 // sync 0xA5, version 3.3 (3 bits major, 5 bits minor)
	payloadSize = 104    // PAYLOAD_SIZE of v3.1–3.3
	counterMask = 0x3f   // FRAME_COUNTER is 6 bits
	frameMs     = 8      // nominal AFE_TIMESTAMP step
)

// Frame holds the v3.3 fields EchoMuse carries.
type Frame struct {
	Counter        uint8  // FRAME_COUNTER, +1 per frame mod 64
	TimestampMs    uint16 // AFE_TIMESTAMP
	ERLE           uint8  // ERLE_RAW
	ComputeRMS     bool   // COMPUTE_RMS: RMS is meaningful
	RMS            uint8  // dB = RMS − 256
	AECDiverged    bool
	MicClipped     bool
	OutputClipped  bool
	PlaybackActive bool
	Volume         uint8 // 7 bits
	DeviceMute     bool
	DTD            uint8 // 5 bits, value DTD/31
	DNNVAD         uint8 // 2 bits, value DNNVAD×0.25
}

// parse validates the 128 bits hi:lo (bit 127, the first received, is hi's
// most significant bit) and decodes the frame.
func parse(hi, lo uint64) (Frame, bool) {
	if hi>>48 != syncVersion || (hi>>41)&0x7f != payloadSize {
		return Frame{}, false
	}
	if uint64(bits.OnesCount64(hi)+bits.OnesCount64(lo>>8)) != lo&0xff {
		return Frame{}, false
	}
	return Frame{
		Counter:        uint8(hi>>35) & counterMask, // 104:99
		TimestampMs:    uint16(hi >> 19),            // 98:83
		ERLE:           uint8(hi >> 11),             // 82:75
		ComputeRMS:     hi>>10&1 != 0,               // 74
		RMS:            uint8(hi >> 2),              // 73:66; 65, 64 are LPM flags
		AECDiverged:    lo>>63 != 0,                 // 63
		MicClipped:     lo>>62&1 != 0,               // 62
		OutputClipped:  lo>>61&1 != 0,               // 61
		PlaybackActive: lo>>60&1 != 0,               // 60
		Volume:         uint8(lo>>53) & 0x7f,        // 59:53
		DeviceMute:     lo>>52&1 != 0,               // 52; 51:44 ARA_VSS
		DTD:            uint8(lo>>39) & 0x1f,        // 43:39; 38:15 UED, proximity, direction
		DNNVAD:         uint8(lo>>13) & 0x3,         // 14:13; 12:8 SER, 7:0 checksum
	}, true
}

// Period is one capture period's summary plus the decoder's health over it.
type Period struct {
	Record ema.AFERecord
	// Invalid counts frames that failed validation where the locked
	// decoder expected one; Syncs lock acquisitions; Gaps discontinuities;
	// Lost the AFE frames those discontinuities skipped (unclamped).
	Invalid, Syncs, Gaps uint8
	Lost                 uint32
}

type state uint8

const (
	searching state = iota // hunting for a candidate frame at every bit
	pending                // a candidate awaits its confirming frame
	locked                 // a frame is due every FrameBits bits
)

// Decoder recovers frames from a stream of capture periods. It is owned by
// the capture goroutine.
type Decoder struct {
	hi, lo uint64
	fill   int // bits in hi:lo, saturating at FrameBits
	st     state

	last  Frame  // the last frame accepted into a summary
	have  bool   // last is set
	since uint64 // bits received after last's final bit

	cand    Frame // the pending candidate
	candAge int   // bits received after cand's final bit

	erleSum, rmsSum, rmsN int
}

// Decode summarises the frames whose final bit lies in pcm, one capture
// period, into p: a frame belongs to the period in which it ends.
func (d *Decoder) Decode(pcm []int16, p *Period) {
	*p = Period{}
	d.erleSum, d.rmsSum, d.rmsN = 0, 0, 0
	for _, s := range pcm {
		d.hi = d.hi<<1 | d.lo>>63
		d.lo = d.lo<<1 | uint64(s)&1
		d.since++
		if d.fill < FrameBits {
			if d.fill++; d.fill < FrameBits {
				continue
			}
		}
		switch d.st {
		case locked:
			if d.since != FrameBits {
				continue
			}
			if f, ok := parse(d.hi, d.lo); ok {
				d.accept(f, p)
			} else {
				p.Invalid++
				d.st = searching
			}
		case pending:
			if d.candAge++; d.candAge != FrameBits {
				continue
			}
			f, ok := parse(d.hi, d.lo)
			switch {
			case ok && f.Counter == (d.cand.Counter+1)&counterMask:
				p.Syncs++
				p.Record.Flags |= ema.AFESync
				d.st = locked
				d.accept(f, p)
			case ok:
				d.cand, d.candAge = f, 0
			default:
				d.st = searching
			}
		case searching:
			if d.hi>>48 != syncVersion {
				continue
			}
			f, ok := parse(d.hi, d.lo)
			if !ok {
				continue
			}
			if d.onGrid(f) {
				// Back on the previous lock's phase with the counter
				// it predicts: continuity confirms the frame.
				d.st = locked
				d.accept(f, p)
				continue
			}
			d.cand, d.candAge, d.st = f, 0, pending
		}
	}
	d.finish(p)
}

// onGrid reports whether f sits a whole number of frames after the last
// accepted frame and carries the counter that predicts.
func (d *Decoder) onGrid(f Frame) bool {
	return d.have && d.since%FrameBits == 0 &&
		f.Counter == (d.last.Counter+uint8(d.since/FrameBits))&counterMask
}

// accept checks f's continuity with the last accepted frame and adds it to
// the period's summary.
func (d *Decoder) accept(f Frame, p *Period) {
	if d.have {
		d.continuity(f, p)
	}
	d.last, d.have, d.since = f, true, 0

	r := &p.Record
	r.Frames++
	if f.PlaybackActive {
		r.Playback++
	}
	if f.ERLE > 0 {
		r.ERLEFrames++
		d.erleSum += int(f.ERLE)
		r.ERLEMax = max(r.ERLEMax, f.ERLE)
	}
	if f.DTD > 0 {
		r.DTDFrames++
		r.DTDMax = max(r.DTDMax, f.DTD)
	}
	if f.ComputeRMS {
		d.rmsN++
		d.rmsSum += int(f.RMS)
		r.RMSMax = max(r.RMSMax, f.RMS)
	}
	if f.DNNVAD > 0 {
		r.VADFrames++
		r.VADMax = max(r.VADMax, f.DNNVAD)
	}
	r.Volume = f.Volume
	if f.OutputClipped {
		r.Flags |= ema.AFEOutputClipped
	}
	if f.MicClipped {
		r.Flags |= ema.AFEMicClipped
	}
	if f.AECDiverged {
		r.Flags |= ema.AFEAECDiverged
	}
	if f.DeviceMute {
		r.Flags |= ema.AFEDeviceMute
	}
}

// continuity compares f with the last accepted frame, d.since bits earlier.
// The AFE produced a number of frames between them that is congruent to the
// counter step modulo 64; of those, the one nearest the timestamp step is
// taken. Anything but exactly one frame per 128 received bits is a gap.
func (d *Decoder) continuity(f Frame, p *Period) {
	slots := (d.since + FrameBits/2) / FrameBits
	step := uint64((f.Counter - d.last.Counter) & counterMask)
	dts := int64(f.TimestampMs - d.last.TimestampMs) // uint16 wrap first
	wraps := max((dts-frameMs*int64(step)+frameMs*32)/(frameMs*64), 0)
	elapsed := step + uint64(wraps)*(counterMask+1)
	if elapsed == slots && d.since%FrameBits == 0 {
		return
	}
	p.Gaps++
	p.Record.Flags |= ema.AFEGap
	if elapsed > slots {
		p.Lost += uint32(min(elapsed-slots, 1<<31))
	}
}

// finish turns the period's sums into the record's rounded means.
func (d *Decoder) finish(p *Period) {
	r := &p.Record
	if r.ERLEFrames > 0 {
		r.ERLEMean = uint8((d.erleSum + int(r.ERLEFrames)/2) / int(r.ERLEFrames))
	}
	if d.rmsN > 0 {
		r.RMSMean = uint8((d.rmsSum + d.rmsN/2) / d.rmsN)
	}
	r.Lost = uint8(min(p.Lost, 255))
}
