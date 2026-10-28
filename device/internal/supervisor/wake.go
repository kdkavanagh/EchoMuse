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

// localChimePrefix names the wake chime's playback on the device; the mixer
// marks it Local, so it is never reported.
const localChimePrefix = "local-wake-chime:"

// offlineNoticePrefix names the offline notice's (local, unreported) playback.
const offlineNoticePrefix = "offline-notice:"

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

// onCandidate opens the candidate's audio subscription and, when the device
// was producing sound, the provisional duck, starts the local wake chime
// when it applies, lights the kiosk screen when screen wake is on, then
// reports the candidate. The controller decides every candidate, including
// while capture is not permitted. With no session there is no one to decide
// it: the screen still lights, and offlineWake answers instead.
func (s *Supervisor) onCandidate(c detector.Candidate) {
	if !s.connected() {
		s.screenWake()
		s.offlineWake(c)
		return
	}
	s.up.OpenCandidate(c.SubscriptionID, c.SupportStart)
	// The provisional duck waits for the ack exactly as long as its subscription.
	s.fm.CandidateOpen(c.CandidateID, c.ProducingSound, uplink.CandidateAckWait)
	body := wakeCandidate{Candidate: c, ActiveAlert: s.activeAlert()}
	// Before the report and outside mu: the mixer delivers its queued
	// finished/progress hooks, which lock mu, on this goroutine.
	body.Chimed = s.localWakeChime(c, body.ActiveAlert != nil)
	s.screenWake()

	s.mu.Lock()
	defer s.mu.Unlock()
	s.candidates[c.SubscriptionID] = candidateState{id: c.CandidateID}
	if s.session == nil {
		return
	}
	// Registered under mu so the controller's ack cannot overtake it.
	id, err := s.session.Send(proto.TypeWakeCandidate, 1, body)
	if err != nil {
		log.Printf("[wake] send candidate: %v", err)
		return
	}
	s.candidateAck[id] = c.SubscriptionID
}

// offlineWake answers a wake heard with no controller with the offline
// notice. Nothing is uploaded, ducked or reported. A wake heard while the
// device produced sound is ignored: only the controller can tell the wake
// word from the device's own output (§6.1), and that includes the notice
// itself. So is one during an alert, which the action button stops.
func (s *Supervisor) offlineWake(c detector.Candidate) {
	if c.ProducingSound {
		log.Printf("[wake] offline: candidate %s heard over the device's own sound; ignored", c.CandidateID)
		return
	}
	s.offlineNotice(c.CandidateID)
}

// offlineNotice plays cue.OfflineNotice ("I can't connect to the smart home
// right now", then the no-input chime) unless an alert is active or the
// notice is already playing. Must not hold mu.
func (s *Supervisor) offlineNotice(id string) {
	if s.activeAlert() != nil {
		log.Printf("[wake] offline: %s during an alert; no notice", id)
		return
	}
	if !s.noticePlaying.CompareAndSwap(false, true) {
		return
	}
	if err := s.mix.StartLocal(offlineNoticePrefix+id, render.Earcon, cue.OfflineNotice(), 0); err != nil {
		s.noticePlaying.Store(false)
		log.Printf("[wake] offline notice: %v", err)
		return
	}
	log.Printf("[wake] offline: notice for %s", id)
}

// restoreWake has the detector score with the last recorded model while no
// controller has named one (wakeword.Activator.Restore).
func (s *Supervisor) restoreWake() {
	s.mu.Lock()
	ctx := s.offCtx
	s.mu.Unlock()
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		restored, err := s.act.Restore(ctx)
		switch {
		case err != nil:
			log.Printf("[wake] restore last model: %v", err)
		case restored:
			log.Printf("[wake] scoring with the last model until a controller names one")
		}
	}()
}

// localWakeChime plays the wake chime the moment an idle candidate opens
// (config wakeSound) instead of after the controller's acceptance. Never
// while disconnected (offlineWake answers those), during a diagnostic
// subscription (wakes are refused), producing sound (the controller verifies
// those wakes) or with an alert active: the controller chimes those on
// acceptance. A rejected candidate keeps its chime. Must not hold mu.
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

// screenWake lights the screen through ScreenOn (screen_wake, config
// screenWake) at every candidate open: the kiosk's REST API, or the
// companion app on a screen_overlay platform. The request is asynchronous
// and its failures never touch the wake path.
func (s *Supervisor) screenWake() {
	if !s.has(proto.CapScreenWake) {
		return
	}
	if v := s.cfg.DeviceConfig.Get(); v.ScreenWake {
		s.cfg.ScreenOn(v.ScreenWakeURL, v.ScreenWakePassword)
	}
}

func (s *Supervisor) onCandidateEnd(e detector.CandidateEnd) {
	s.logSend(proto.TypeWakeCandidateEnd, 1, e)
}

// onNearMiss reports a closed near-miss episode in wake.near_miss
// (near_miss_v1). With no session it is dropped; wake.stats still counts it.
func (s *Supervisor) onNearMiss(e detector.NearMissEvent) {
	s.logSend(proto.TypeWakeNearMiss, 0, e)
}

// onStats reports wake.stats, with the AFE decoder's health in an
// afe_metadata_v1 session; the AFE window follows the detector's.
func (s *Supervisor) onStats(st detector.Stats) {
	body := wakeStats{Stats: st}
	if afe := s.afeStats(st.WindowEnd, st.WindowMS); afe != nil && s.afe.on.Load() {
		body.AFE = afe
	}
	s.logSend(proto.TypeWakeStats, 0, body)
}

// candidateAcked applies the controller's command.ack for a wake.candidate:
// accepted releases the subscription's audio and extends its provisional duck
// to the subscription TTL; rejected ends both.
func (s *Supervisor) candidateAcked(a proto.CommandAck) bool {
	s.mu.Lock()
	subscriptionID, ok := s.candidateAck[a.MessageID]
	delete(s.candidateAck, a.MessageID)
	cand, live := s.candidates[subscriptionID]
	s.mu.Unlock()
	if !ok {
		return false
	}
	accepted := a.Status == proto.AckAccepted
	s.up.AcceptCandidate(subscriptionID, accepted)
	switch {
	case !live:
	case accepted:
		s.fm.CandidateRenew(cand.id, focus.DefaultTTL)
	default:
		s.releaseCandidateSubscription(subscriptionID)
	}
	return true
}

// releaseCandidateSubscription restores the policy the provisional duck
// replaced (§16.6): uplink.close rejected/arbitration_lost, TTL or any other
// end.
func (s *Supervisor) releaseCandidateSubscription(subscriptionID string) {
	s.mu.Lock()
	cand, ok := s.candidates[subscriptionID]
	delete(s.candidates, subscriptionID)
	for id, subID := range s.candidateAck {
		if subID == subscriptionID {
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
	for subscriptionID := range s.candidates {
		ids = append(ids, subscriptionID)
	}
	s.mu.Unlock()
	for _, subscriptionID := range ids {
		s.releaseCandidateSubscription(subscriptionID)
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
// assets socket. A policy this firmware does not implement, including an
// invalid open/shadow rule list, leaves the detector unavailable rather than
// scoring under different semantics.
func (s *Supervisor) activate(ctx context.Context, sess Session, r proto.SessionReady) {
	d := r.Detector
	rules := detector.Rules{Open: detectorRules(d.OpenRules), Shadow: detectorRules(d.ShadowRules)}
	err := detector.CheckPolicy(d.HopBlocks, d.Smoothing, d.ClearAfterUnscored)
	if err == nil {
		err = detector.CheckRules(rules)
	}
	if err != nil {
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
	s.activating.Add(1)
	go func() {
		defer s.wg.Done()
		defer s.activating.Add(-1)
		if err := s.act.Activate(ctx, sess.Assets(), want, th, rules); err != nil {
			log.Printf("[wake] activate: %v", err)
		}
	}()
}

// detectorRules converts a session.ready rule list, keeping nil (absent)
// distinct from empty.
func detectorRules(in []proto.OpenRule) []detector.OpenRule {
	if in == nil {
		return nil
	}
	out := make([]detector.OpenRule, len(in))
	for i, r := range in {
		out[i] = detector.OpenRule{
			Profile: detector.Profile(r.Profile), Windows: r.Windows,
			Combine: detector.RuleCombine(r.Combine), Threshold: r.Threshold,
		}
	}
	return out
}
