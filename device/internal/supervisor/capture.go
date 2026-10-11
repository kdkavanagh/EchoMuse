package supervisor

import (
	"log"

	"github.com/wilbowes/EchoMuse/internal/audio/capture"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/audio/ring"
	"github.com/wilbowes/EchoMuse/internal/proto"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

// captureBlock is the per-block device flow of §8.1: index the callback,
// append canonical PCM to the mic ring, complete cells into the cell ring,
// store the period's AFE record, feed the detector, and wake the uplink
// sender. A missing range is recorded as missing everywhere; nothing invents
// silence. While privacy-muted the block is discarded (§3.2 invariant 10);
// only the AFE decoder's health is still counted. Allocation-free.
func (s *Supervisor) captureBlock(in pkgmic.Block) {
	s.capMu.Lock()
	defer s.capMu.Unlock()
	if in.AFE != nil {
		s.accountAFE(in.AFE)
	}
	if s.muted {
		return
	}
	b := s.timeline.Add(in.PCM, in.MonoNs)
	if b.NewEpoch {
		s.startCaptureEpochLocked(b.Epoch, proto.StreamReason(b.Reason))
	}
	s.cellMeta = ring.Meta{UncertaintyUs: b.UncertaintyUs, Flags: ema.FlagEstimated}
	if b.HasMissing {
		s.micRing.Missing(b.Missing.To)
		s.afeRing.Missing(b.Missing.To / ema.AFESamples)
		if err := s.acc.Missing(b.Missing.From, b.Missing.To, b.Missing.MonoNs); err != nil {
			log.Printf("[capture] cells missing: %v", err)
		}
	}
	meta := ring.Meta{MonoNs: b.MonoNs, UncertaintyUs: b.UncertaintyUs, Flags: b.Flags}
	if err := s.micRing.Append(b.First, b.PCM, meta); err != nil {
		log.Printf("[capture] mic ring: %v", err)
	}
	if in.AFE != nil {
		s.afeRec[0] = in.AFE.Record
		if err := s.afeRing.Append(b.First/ema.AFESamples, s.afeRec[:], meta); err != nil {
			log.Printf("[capture] afe ring: %v", err)
		}
	}
	if err := s.acc.Push(b.First, b.PCM, b.MonoNs, false); err != nil {
		log.Printf("[capture] cells: %v", err)
	}
	s.det.OnBlock(b.First, b.PCM, false)
	s.up.Notify()
}

// appendCells is the accumulator sink (capture goroutine, capMu held).
func (s *Supervisor) appendCells(firstCell uint64, c []ema.Cell, monoNs int64) {
	m := s.cellMeta
	m.MonoNs = monoNs
	if err := s.cellRing.Append(firstCell, c, m); err != nil {
		log.Printf("[capture] cell ring: %v", err)
	}
}

// startCaptureEpochLocked ends the previous capture streams, resets the
// rings, then moves uplink and detector to the new epoch before its first
// sample is appended. An epoch already announced by an unmute only needs
// its first block. capMu held.
func (s *Supervisor) startCaptureEpochLocked(epoch uint64, reason proto.StreamReason) {
	s.announceMu.Lock()
	defer s.announceMu.Unlock()
	if epoch == s.micEpoch {
		return
	}
	if s.micEpoch != 0 {
		s.endCaptureStreamsLocked(reason)
	}
	s.resetCaptureLocked()
	s.micEpoch, s.micReason = epoch, reason
	s.up.SetEpochs(epoch, s.refEpoch)
	s.det.SetEpoch(epoch)
	s.openCaptureStreamsLocked()
}

func (s *Supervisor) resetCaptureLocked() {
	s.micRing.Reset(0)
	s.cellRing.Reset(0)
	s.afeRing.Reset(0)
	s.acc.Reset(0)
}

// openCaptureStreamsLocked announces the mic and cell streams of the
// current capture epoch, and the afe stream in an afe_metadata_v1 session;
// cells and afe use the mic epoch (§16.1). announceMu held.
func (s *Supervisor) openCaptureStreamsLocked() {
	s.logSend(proto.TypeStreamOpen, 0, proto.StreamOpen{StreamID: proto.StreamMic, Epoch: s.micEpoch,
		Kind: uint8(ema.KindMic), SampleRate: ema.RateCapture, Format: uint8(ema.FormatPCM16), Reason: s.micReason})
	s.logSend(proto.TypeStreamOpen, 0, proto.StreamOpen{StreamID: proto.StreamCells, Epoch: s.micEpoch,
		Kind: uint8(ema.KindCells), SampleRate: ema.RateCapture, Format: uint8(ema.FormatCellV1), Reason: s.micReason})
	if s.afe.on.Load() {
		s.logSend(proto.TypeStreamOpen, 0, proto.StreamOpen{StreamID: proto.StreamAFE, Epoch: s.micEpoch,
			Kind: uint8(ema.KindAFE), SampleRate: ema.RateCapture, Format: uint8(ema.FormatAFEV1), Reason: s.micReason})
	}
}

// endCaptureStreamsLocked reports the final capture sample of the mic, cell
// and afe streams (cells and afe in capture samples). announceMu held.
func (s *Supervisor) endCaptureStreamsLocked(reason proto.StreamReason) {
	s.logSend(proto.TypeStreamEnd, 0, proto.StreamEnd{StreamID: proto.StreamMic, Epoch: s.micEpoch,
		FinalSample: s.micRing.End(), Reason: reason})
	s.logSend(proto.TypeStreamEnd, 0, proto.StreamEnd{StreamID: proto.StreamCells, Epoch: s.micEpoch,
		FinalSample: s.cellRing.End() * ema.CellSamples, Reason: reason})
	if s.afe.on.Load() {
		s.logSend(proto.TypeStreamEnd, 0, proto.StreamEnd{StreamID: proto.StreamAFE, Epoch: s.micEpoch,
			FinalSample: s.afeRing.End() * ema.AFESamples, Reason: reason})
	}
}

// setPrivacy applies a physical mute transition (§3.2 invariant 10, §18.2
// mute.go): muting ends the capture epoch, every uplink lease and the
// open candidate, and erases the mic, cell and AFE rings; unmuting starts a
// new epoch at once so privacy.changed can name it. Alerts are unaffected.
func (s *Supervisor) setPrivacy(muted bool) {
	seq := s.physicalSeq.Add(1)
	s.capMu.Lock()
	s.announceMu.Lock()
	s.muted = muted
	if muted {
		s.logSend(proto.TypePrivacyChanged, 0, proto.PrivacyChanged{Muted: true, PhysicalSeq: seq})
		s.det.SetMuted(true)
		s.up.Mute()
		if s.micEpoch != 0 {
			s.endCaptureStreamsLocked(proto.StreamPrivacy)
		}
		s.resetCaptureLocked()
		s.micEpoch, s.micReason = 0, ""
		s.up.SetEpochs(0, s.refEpoch)
	} else {
		epoch := s.timeline.RestartNow(capture.ReasonPrivacy)
		s.logSend(proto.TypePrivacyChanged, 0, proto.PrivacyChanged{Muted: false, CaptureEpoch: proto.U64(epoch), PhysicalSeq: seq})
		s.resetCaptureLocked()
		s.micEpoch, s.micReason = epoch, proto.StreamPrivacy
		s.up.SetEpochs(epoch, s.refEpoch)
		s.det.SetEpoch(epoch)
		s.det.SetMuted(false)
		s.openCaptureStreamsLocked()
	}
	s.announceMu.Unlock()
	s.capMu.Unlock()
	if muted {
		s.releaseAllCandidates()
	}
}

// captureAt maps a monotonic instant to the current capture epoch and
// sample; both are null while muted or before the first block.
func (s *Supervisor) captureAt(ns int64) (epoch, sample proto.NullU64) {
	s.announceMu.Lock()
	e := s.micEpoch
	s.announceMu.Unlock()
	if e == 0 {
		return epoch, sample
	}
	epoch = proto.U64(e)
	if v, ok := s.timeline.Fit().NsToSample(ns); ok && v >= 0 {
		sample = proto.U64(uint64(v))
	}
	return epoch, sample
}
