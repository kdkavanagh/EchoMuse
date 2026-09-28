package supervisor

import (
	"context"
	"encoding/json"
	"errors"
	"log"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

// alertAssetExt is the alert sound extension in the alert asset store (§16.5).
const alertAssetExt = "wav"

// AlertState implements alerts.EventSink: alert.state on every change.
func (s *Supervisor) AlertState(st alerts.AlertState) {
	s.mu.Lock()
	s.alertState = st
	s.mu.Unlock()
	s.logSend(proto.TypeAlertState, 0, st)
}

// RingEnded implements alerts.EventSink (telemetry only).
func (s *Supervisor) RingEnded(r alerts.RingEnded) { s.logSend(proto.TypeAlertRingEnded, 0, r) }

// LocalOperation implements alerts.EventSink: executor expiries.
func (s *Supervisor) LocalOperation(op alerts.LocalOperation) {
	s.logSend(proto.TypeAlertLocalOp, 0, op)
}

// AlertFocus implements alerts.EventSink. The occurrence's DAC level, amp
// routing and LED indication follow it (§16.5, §11.2); focus learns which
// occurrence heads the queue, which drives background budgets and
// content pause (§6.2).
func (s *Supervisor) AlertFocus(f alerts.AlertFocus) {
	s.alertForeground.Store(f.Active && f.Foreground)
	s.cfg.Physical.SetAlertAudio(f.Active, f.ID, f.Foreground, f.Volume)
	s.mu.Lock()
	prev := s.alertFocusID
	s.alertFocusID = ""
	if f.Active {
		s.alertFocusID = f.ID
	}
	s.mu.Unlock()
	switch {
	case f.Active:
		s.fm.AlertActive(f.ID)
	case prev != "":
		s.fm.AlertEnded(prev, false)
	}
}

func (s *Supervisor) alertSnapshot(env proto.Envelope) {
	ack, _ := s.ex.ApplySnapshotPage(env.Body)
	s.logSend(proto.TypeAlertAck, 0, ack)
}

func (s *Supervisor) alertDelta(env proto.Envelope) {
	s.logSend(proto.TypeAlertAck, 0, s.ex.ApplyDelta(env.Body))
}

// alertAct applies a controller stop/snooze: command.ack (durable once an
// alarm operation is journaled), then the local operation, then ring end.
func (s *Supervisor) alertAct(env proto.Envelope) {
	var b proto.AlertAct
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	res := s.ex.Act(b.OpID, b.TargetID, b.Action, b.Source)
	s.ack(env, res.Status, res.Error)
	s.sendActResult(res)
}

func (s *Supervisor) sendActResult(res alerts.ActResult) {
	if res.Operation != nil {
		s.logSend(proto.TypeAlertLocalOp, 0, *res.Operation)
	}
	if res.Ended != nil {
		s.logSend(proto.TypeAlertRingEnded, 0, *res.Ended)
	}
}

func (s *Supervisor) alertRing(env proto.Envelope) {
	var r alerts.TimerRing
	if err := json.Unmarshal(env.Body, &r); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	if err := s.ex.HandleTimerRing(r); err != nil {
		log.Printf("[alerts] alert.ring: %v", err)
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	s.ack(env, proto.AckAccepted, "")
}

func (s *Supervisor) alertOpResult(env proto.Envelope) {
	var r alerts.OpResult
	if err := json.Unmarshal(env.Body, &r); err != nil {
		log.Printf("[alerts] op_result: %v", err)
		return
	}
	if err := s.ex.ApplyOpResult(r); err != nil {
		log.Printf("[alerts] op_result %s: %v", r.OpID, err)
	}
}

func (s *Supervisor) clockReply(env proto.Envelope) {
	var r alerts.ClockReply
	if err := json.Unmarshal(env.Body, &r); err != nil {
		log.Printf("[alerts] clock.reply: %v", err)
		return
	}
	if err := s.ex.ApplyClockReply(r); err != nil {
		log.Printf("[alerts] clock.reply: %v", err)
	}
}

// fetchAlertAssets installs missing alert sounds (armed alarms and requested
// previews) over the session's assets socket, one fetch pass at a time.
// Until installed, the executor rings the built-in fallback (§16.5).
func (s *Supervisor) fetchAlertAssets() {
	s.mu.Lock()
	sess, ctx := s.session, s.sessCtx
	s.mu.Unlock()
	if sess == nil || !s.fetching.CompareAndSwap(false, true) {
		return
	}
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		defer s.fetching.Store(false)
		s.fetchAlertAssetsFrom(ctx, sess.Assets())
	}()
}

func (s *Supervisor) fetchAlertAssetsFrom(ctx context.Context, tr assets.Transport) {
	want := s.ex.MissingSounds()
	s.mu.Lock()
	for sha := range s.previewWant {
		want = append(want, sha)
	}
	s.mu.Unlock()
	for _, sha := range want {
		s.mu.Lock()
		skip := s.unfetchable[sha]
		s.mu.Unlock()
		if skip || s.cfg.AlertStore.Has(sha) && s.ex.AssetInstalled(sha) == nil {
			s.fetched(sha)
			continue
		}
		if _, err := s.cfg.AlertStore.Ensure(ctx, tr, sha, alertAssetExt); err != nil {
			if errors.Is(err, assets.ErrNotFound) || errors.Is(err, assets.ErrHashMismatch) {
				s.mu.Lock()
				s.unfetchable[sha] = true
				s.mu.Unlock()
				continue
			}
			if ctx.Err() == nil {
				log.Printf("[alerts] fetch %s: %v", sha, err)
			}
			return
		}
		if err := s.ex.AssetInstalled(sha); err != nil {
			log.Printf("[alerts] asset %s: %v", sha, err)
			s.mu.Lock()
			s.unfetchable[sha] = true
			s.mu.Unlock()
			continue
		}
		s.fetched(sha)
	}
}

func (s *Supervisor) fetched(sha string) {
	s.mu.Lock()
	delete(s.previewWant, sha)
	s.mu.Unlock()
}
