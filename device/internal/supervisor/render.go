package supervisor

import (
	"encoding/json"
	"errors"
	"log"
	"math"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/audio/refdsp"
	"github.com/wilbowes/EchoMuse/internal/audio/ring"
	"github.com/wilbowes/EchoMuse/internal/cue"
	"github.com/wilbowes/EchoMuse/internal/focus"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/render"
)

// onTap receives every final-mix block before OpenSL output (§4.3): it
// records the active-source mask for cells and producing_sound, decimates
// into the 16 kHz reference ring (§16.1 FIR; all-zero output marked digital
// silence), and drives the LED meter level. Mixer goroutine; allocation-free.
func (s *Supervisor) onTap(b render.MixBlock) {
	s.masks.Record(b.First, len(b.PCM), b.Mask)
	s.lastMask.Store(uint32(b.Mask))
	s.cfg.Physical.SetAudioLevel(rms(b.PCM))

	out, err := s.dec.Push(b.First, b.PCM, b.Mask, s.refBuf[:])
	if err != nil {
		log.Printf("[render] reference decimator: %v", err)
		return
	}
	if out.MissingTo > out.MissingFrom {
		s.refRing.Missing(out.MissingTo)
	}
	if len(out.Samples) == 0 {
		return
	}
	meta := ring.Meta{Flags: ema.FlagEstimated, Mask: out.Mask, UncertaintyUs: ema.UncertaintyUnknown}
	if ns, ok := s.renderFit.SampleToNs(out.First * refdsp.Factor); ok {
		meta.MonoNs = ns
		queued := b.First + uint64(len(b.PCM)) - min(s.completed.Load(), b.First+uint64(len(b.PCM)))
		if unc, ok := s.renderFit.UncertaintyNs(int64(queued) * 1_000_000_000 / render.SampleRate); ok {
			meta.UncertaintyUs = uint32(min((unc+999)/1000, int64(ema.UncertaintyUnknown-1)))
		}
	} else {
		meta.MonoNs = s.now()
	}
	if refdsp.AllZero(out.Samples) {
		meta.Flags |= ema.FlagDigitalSilence
	}
	if err := s.refRing.Append(out.First, out.Samples, meta); err != nil {
		log.Printf("[render] reference ring: %v", err)
	}
	s.up.Notify()
}

// onAnchor fits the render clock from buffer completions (§4.3). A fit
// that demands a reset is restarted from this anchor. Mixer goroutine.
func (s *Supervisor) onAnchor(a render.Anchor) {
	if a.Epoch != s.renderEpoch {
		return
	}
	s.completed.Store(a.CompletedFrame)
	if s.renderFit.Add(a.CompletedFrame, a.MonoNS).IsReset() {
		log.Printf("[render] render clock fit reset at frame %d", a.CompletedFrame)
		s.renderFit.Add(a.CompletedFrame, a.MonoNS)
	}
}

// onRenderEpoch starts a new reference epoch with each render epoch
// (§16.1) and announces it. Mixer goroutine.
func (s *Supervisor) onRenderEpoch(epoch uint64) {
	s.dec.Reset()
	s.masks.Reset()
	s.renderFit.Reset()
	s.completed.Store(0)
	s.renderEpoch = epoch

	s.announceMu.Lock()
	defer s.announceMu.Unlock()
	reason := proto.StreamStart
	if s.refEpoch != 0 {
		reason = proto.StreamRenderEpoch
		s.logSend(proto.TypeStreamEnd, 0, proto.StreamEnd{StreamID: proto.StreamReference, Epoch: s.refEpoch,
			FinalSample: s.refRing.End(), Reason: reason})
	}
	s.refRing.Reset(0)
	s.refEpoch, s.refReason = newEpoch(), reason
	s.up.SetEpochs(s.micEpoch, s.refEpoch)
	s.openReferenceStreamLocked()
}

func (s *Supervisor) openReferenceStreamLocked() {
	s.logSend(proto.TypeStreamOpen, 0, proto.StreamOpen{StreamID: proto.StreamReference, Epoch: s.refEpoch,
		Kind: uint8(ema.KindReference), SampleRate: ema.RateCapture, Format: uint8(ema.FormatPCM16), Reason: s.refReason})
}

// onProgress forwards render.progress (generation in the envelope) and
// tells focus when dialog output becomes audible.
func (s *Supervisor) onProgress(p render.Progress) {
	if p.Event == render.EventStart {
		s.mu.Lock()
		pb, ok := s.playbacks[p.PlaybackID]
		s.mu.Unlock()
		if ok && pb.class == render.DialogOutput && pb.gen == p.Generation {
			s.fm.DialogOutputStarted(p.PlaybackID, pb.owner, p.Generation)
		}
	}
	body := proto.RenderProgress{
		PlaybackID: p.PlaybackID, Event: string(p.Event),
		SubmittedFrames: p.SubmittedFrames, CompletedFrames: p.CompletedFrames,
		MonoNs: p.MonoNS, UncertaintyUs: p.UncertaintyUS,
		TimingQuality: p.TimingQuality, ReferenceCoverage: p.ReferenceCoverage,
	}
	switch p.Event {
	case render.EventUnderrun:
		from, to := proto.U64(p.MissingFrom), proto.U64(p.MissingTo)
		body.MissingFrom, body.MissingTo = &from, &to
	case render.EventGain:
		g := p.GainDB
		body.GainDB = &g
	}
	s.logSend(proto.TypeRenderProgress, p.Generation, body)
}

// onFinished forwards render.finished and releases dialog-output focus.
func (s *Supervisor) onFinished(f render.Finished) {
	s.mu.Lock()
	pb, ok := s.playbacks[f.PlaybackID]
	if ok && pb.gen == f.Generation {
		delete(s.playbacks, f.PlaybackID)
	}
	s.mu.Unlock()
	if ok && pb.class == render.DialogOutput && pb.gen == f.Generation {
		s.fm.DialogOutputFinished(f.PlaybackID, f.Generation)
	}
	s.logSend(proto.TypeRenderFinished, f.Generation, proto.RenderFinished{
		PlaybackID: f.PlaybackID, LastCompletedFrame: f.LastCompletedFrame,
		Reason: string(f.Reason), TimingQuality: f.TimingQuality,
	})
}

// renderStart handles render.start (WIRE §4.2). Network classes bind a
// kind-3 epoch; local classes play a built-in earcon or an alert preview
// from the validated alert asset store, falling back to the built-in tone.
func (s *Supervisor) renderStart(env proto.Envelope) {
	var b proto.RenderStart
	if err := json.Unmarshal(env.Body, &b); err != nil || b.PlaybackID == "" || b.Format != uint8(ema.FormatPCM16) {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	class := render.SourceClass(b.SourceClass)
	v := render.Playback{ID: b.PlaybackID, Generation: env.Generation, Class: class, GainDB: b.GainDB}
	asset := ""
	if b.LocalAsset != nil {
		asset = *b.LocalAsset
	}
	switch {
	case class.Network():
		if !b.Epoch.Valid {
			s.ack(env, proto.AckRejected, codeInvalid)
			return
		}
		v.Epoch = b.Epoch.V
	case class == render.Earcon:
		pcm, err := cue.Asset(asset)
		if err != nil {
			s.ack(env, proto.AckRejected, codeUnknownAsset)
			return
		}
		v.PCM = pcm
	case class == render.AlertPreview:
		if asset == "" {
			s.ack(env, proto.AckRejected, codeUnknownAsset)
			return
		}
		pcm, installed := s.ex.PreviewPCM(asset)
		if !installed && isSHA256(asset) {
			s.mu.Lock()
			s.previewWant[asset] = true
			s.mu.Unlock()
		}
		v.PCM = pcm
	default:
		s.ack(env, proto.AckRejected, codeInvalidClass)
		return
	}

	s.mu.Lock()
	prev, hadPrev := s.playbacks[b.PlaybackID]
	pb := playbackState{class: class, gen: env.Generation}
	if class == render.DialogOutput {
		pb.owner = s.dialogOwner
	}
	s.playbacks[b.PlaybackID] = pb
	s.mu.Unlock()

	if err := s.mix.Start(v); err != nil {
		s.mu.Lock()
		if hadPrev {
			s.playbacks[b.PlaybackID] = prev
		} else {
			delete(s.playbacks, b.PlaybackID)
		}
		s.mu.Unlock()
		s.ack(env, proto.AckRejected, renderCode(err))
		return
	}
	s.ack(env, proto.AckAccepted, "")
}

func (s *Supervisor) renderEnd(env proto.Envelope) {
	var b proto.RenderEnd
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	s.ackErr(env, s.mix.End(b.PlaybackID, env.Generation), endCode)
}

// renderCancel is idempotent: an unknown or finished playback is accepted.
func (s *Supervisor) renderCancel(env proto.Envelope) {
	var b proto.RenderCancel
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	s.mix.Cancel(b.PlaybackID, env.Generation, render.RampNormal)
	s.ack(env, proto.AckAccepted, "")
}

// RenderAudio pumps a validated kind-3 packet into its network playback.
// A packet for an old generation, unknown epoch or ended playback is late
// and dropped (§3.2 invariant 3). Audio receive goroutine; allocation-free.
func (s *Supervisor) RenderAudio(h ema.Header, pcm []byte) {
	n := len(pcm) / 2
	if n > len(s.renderPCM) {
		return
	}
	ema.PCM(s.renderPCM[:n], pcm)
	err := s.mix.Pump(h.Epoch, h.Generation, h.FirstSample, s.renderPCM[:n])
	switch {
	case err == nil, errors.Is(err, render.ErrStale), errors.Is(err, render.ErrUnknownEpoch), errors.Is(err, render.ErrEnded):
	default:
		log.Printf("[render] pump epoch %d gen %d: %v", h.Epoch, h.Generation, err)
	}
}

// Focus commands (WIRE §4.3).

func (s *Supervisor) focusAcquire(env proto.Envelope) {
	var b proto.FocusAcquire
	if err := json.Unmarshal(env.Body, &b); err != nil || b.LeaseID == "" {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	kind := focus.Kind(b.Focus)
	if err := s.fm.Acquire(b.LeaseID, b.Owner, env.Generation, kind, ttl(b.TTLMs)); err != nil {
		s.ack(env, proto.AckRejected, focusCode(err))
		return
	}
	s.mu.Lock()
	s.leases[b.LeaseID] = struct{}{}
	if kind == focus.DialogOutput {
		s.dialogOwner = b.Owner
	}
	s.mu.Unlock()
	s.ack(env, proto.AckAccepted, "")
}

func (s *Supervisor) focusRenew(env proto.Envelope) {
	var b proto.FocusRenew
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	s.ackErr(env, s.fm.Renew(b.LeaseID, env.Generation, ttl(b.TTLMs)), focusCode)
}

func (s *Supervisor) focusRelease(env proto.Envelope) {
	var b proto.FocusRelease
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	err := s.fm.Release(b.LeaseID, env.Generation)
	if err == nil {
		s.mu.Lock()
		delete(s.leases, b.LeaseID)
		s.mu.Unlock()
	}
	s.ackErr(env, err, focusCode)
}

// applyFocus hands a focus decision to the mixer and alert executor.
func (s *Supervisor) applyFocus(out focus.Output) {
	s.mix.SetPolicy(out.Mix)
	s.ex.SetBackground(out.AlertBackground)
}

// focusLeaseExpired clears controller-driven dialog LEDs once the last
// dialog lease has lapsed (§11.2: remote dialog indicators have leases).
func (s *Supervisor) focusLeaseExpired(l focus.Lease) {
	s.mu.Lock()
	delete(s.leases, l.ID)
	empty := len(s.leases) == 0
	s.mu.Unlock()
	if empty {
		s.cfg.Physical.ClearControllerLEDs()
	}
}

// rms is the block's root-mean-square level in [0, 1].
func rms(pcm []int16) float64 {
	if len(pcm) == 0 {
		return 0
	}
	var sum float64
	for _, v := range pcm {
		x := float64(v) / 32768
		sum += x * x
	}
	return math.Sqrt(sum / float64(len(pcm)))
}
