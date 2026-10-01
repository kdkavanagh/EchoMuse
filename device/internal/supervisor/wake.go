package supervisor

import (
	"context"
	"log"

	"github.com/wilbowes/EchoMuse/internal/cue"
	"github.com/wilbowes/EchoMuse/internal/focus"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/uplink"
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
)

// localChimePrefix names the device-local wake chime's playback; the
// mixer marks it Local, so it is never reported.
const localChimePrefix = "local-wake-chime:"

// wakeCandidate is the wake.candidate body: the detector's fields plus the
// supervisor's active_alert and whether the device chimed, marshalled flat
// (WIRE §4.4).
type wakeCandidate struct {
	detector.Candidate
	ActiveAlert *proto.ActiveAlert `json:"active_alert"`
	Chimed      bool               `json:"chimed"`
}

// profile selects the §5.3 threshold profile from the device's own mix:
// playback while content is audible or an alert occurrence is foreground
// (its loop gaps included); idle otherwise, including dialog output.
func (s *Supervisor) profile() detector.Profile {
	if uint8(s.lastMask.Load())&render.MaskContent != 0 || s.alertForeground.Load() {
		return detector.ProfilePlayback
	}
	return detector.ProfileIdle
}

// producingSound reports whether any source was audible in the final mix
// during capture samples [from, to] (§6.1), mapped through the clock fits.
func (s *Supervisor) producingSound(from, to uint64) bool {
	return s.masks.MaskFor(from, to+1) != 0
}

// onCandidate opens the candidate lease and, when the device was producing
// sound, the provisional duck, starts the local wake chime when it applies,
// then reports the candidate. The controller decides every candidate,
// including while capture is not permitted.
func (s *Supervisor) onCandidate(c detector.Candidate) {
	s.up.OpenCandidate(c.LeaseID, c.SupportStart)
	// The provisional duck waits for the ack exactly as long as its lease.
	s.fm.CandidateOpen(c.CandidateID, c.ProducingSound, uplink.CandidateAckWait)
	body := wakeCandidate{Candidate: c, ActiveAlert: s.activeAlert()}
	// Before the report and outside mu: the mixer delivers its queued
	// finished/progress hooks, which lock mu, on this goroutine.
	body.Chimed = s.localWakeChime(c, body.ActiveAlert != nil)

	s.mu.Lock()
	defer s.mu.Unlock()
	s.candidates[c.LeaseID] = candidateState{id: c.CandidateID}
	if s.session == nil {
		return
	}
	// Registered under mu so the controller's ack cannot overtake it.
	id, err := s.session.Send(proto.TypeWakeCandidate, 1, body)
	if err != nil {
		log.Printf("[wake] send candidate: %v", err)
		return
	}
	s.candidateAck[id] = c.LeaseID
}

// localWakeChime plays the wake chime the moment an idle candidate opens
// (config wakeSound) instead of after the controller's acceptance. Never
// while disconnected (a chime followed by nothing), producing sound (the
// controller verifies those wakes and chimes on acceptance), with an alert
// active (the wake stops it) or during a diagnostic lease (wakes are
// refused). A rejected candidate keeps its chime. Must not hold mu.
func (s *Supervisor) localWakeChime(c detector.Candidate, alertActive bool) bool {
	if !s.cfg.DeviceConfig.Get().WakeSound || c.ProducingSound || alertActive ||
		!s.connected() || s.up.DiagnosticLive() {
		return false
	}
	pcm, err := cue.Asset(cue.WakeChime)
	if err == nil {
		err = s.mix.StartLocal(localChimePrefix+c.CandidateID, render.Earcon, pcm, 0)
	}
	if err != nil {
		log.Printf("[wake] local chime: %v", err)
		return false
	}
	return true
}

func (s *Supervisor) onCandidateEnd(e detector.CandidateEnd) {
	s.logSend(proto.TypeWakeCandidateEnd, 1, e)
}

func (s *Supervisor) onStats(st detector.Stats) {
	s.logSend(proto.TypeWakeStats, 0, st)
}

// candidateAcked applies the controller's command.ack for a wake.candidate:
// accepted releases the lease's audio and extends its provisional duck to
// the lease TTL; rejected ends both.
func (s *Supervisor) candidateAcked(a proto.CommandAck) bool {
	s.mu.Lock()
	lease, ok := s.candidateAck[a.MessageID]
	delete(s.candidateAck, a.MessageID)
	cand, live := s.candidates[lease]
	s.mu.Unlock()
	if !ok {
		return false
	}
	accepted := a.Status == proto.AckAccepted
	s.up.AcceptCandidate(lease, accepted)
	switch {
	case !live:
	case accepted:
		s.fm.CandidateRenew(cand.id, focus.DefaultTTL)
	default:
		s.releaseCandidateLease(lease)
	}
	return true
}

// releaseCandidateLease restores the policy the provisional duck replaced
// (§16.6): uplink.close rejected/arbitration_lost, TTL or any other end.
func (s *Supervisor) releaseCandidateLease(leaseID string) {
	s.mu.Lock()
	cand, ok := s.candidates[leaseID]
	delete(s.candidates, leaseID)
	for id, l := range s.candidateAck {
		if l == leaseID {
			delete(s.candidateAck, id)
		}
	}
	s.mu.Unlock()
	if ok {
		s.fm.CandidateRelease(cand.id)
	}
}

func (s *Supervisor) releaseAllCandidates() {
	s.mu.Lock()
	ids := make([]string, 0, len(s.candidates))
	for lease := range s.candidates {
		ids = append(ids, lease)
	}
	s.mu.Unlock()
	for _, lease := range ids {
		s.releaseCandidateLease(lease)
	}
}

// activeAlert is wake.candidate's active_alert: the ringing or
// backgrounded occurrence, or nil.
func (s *Supervisor) activeAlert() *proto.ActiveAlert {
	s.mu.Lock()
	defer s.mu.Unlock()
	a := s.alertState.Active
	if a == nil {
		return nil
	}
	return &proto.ActiveAlert{ID: a.ID, Kind: a.Kind, Name: a.Name, Foreground: a.Foreground}
}

// activate checks the session's detector policy and switches the detector
// to the named speech assets, fetching missing files over the session's
// assets socket. A policy this firmware does not implement leaves the
// detector unavailable rather than scoring under different semantics.
func (s *Supervisor) activate(ctx context.Context, sess Session, r proto.SessionReady) {
	d := r.Detector
	if err := detector.CheckPolicy(d.HopBlocks, d.Smoothing, d.ClearAfterUnscored); err != nil {
		log.Printf("[wake] %v", err)
		s.act.Unavailable(detector.UnavailableLoadFailed, err)
		return
	}
	pd := d.ProvisionalDuck
	if pd.MaxPerWindow != focus.ProvisionalPerWindow || pd.WindowMs != focus.ProvisionalWindow.Milliseconds() {
		log.Printf("[wake] provisional duck window %d per %d ms; firmware implements %d per %d ms",
			pd.MaxPerWindow, pd.WindowMs, focus.ProvisionalPerWindow, focus.ProvisionalWindow.Milliseconds())
	}
	s.fm.SetDuckDB(min(pd.DuckDB, 0))
	th := detector.Thresholds{Idle: d.Thresholds.Idle, Playback: d.Thresholds.Playback, NearMiss: d.Thresholds.NearMiss}
	want := wakeword.Assets{RuntimeSHA256: r.Assets.RuntimeSHA256, GraphSHA256: r.Assets.GraphSHA256, SidecarSHA256: r.Assets.SidecarSHA256}
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		if err := s.act.Activate(ctx, sess.Assets(), want, th); err != nil {
			log.Printf("[wake] activate: %v", err)
		}
	}()
}
