package supervisor

import (
	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/proto"
	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
)

// DotButton handles an action-button edge. Physical events bypass queued
// inference and carry a monotonically increasing physical sequence (§16.1).
// A release while an alert is ringing or backgrounded stops it on the
// device (alarm: journaled dismiss; timer: stopped) with the 10 ms
// physical-stop ramp and reports handled:"alert_stopped"; otherwise the
// controller applies its gesture policy. Both edges carry the capture
// epoch/sample of the press.
func (s *Supervisor) DotButton(ev pkgbuttons.ButtonClickEvent) {
	now := s.now()
	a := s.buttonAction(ev.ClickType, ev.Button.Type, ev.Down, ev.HeldMs, now)
	if ev.Down {
		a.CaptureEpoch, a.CaptureSample = s.captureAt(now)
		s.mu.Lock()
		s.pressEpoch, s.pressSample = a.CaptureEpoch, a.CaptureSample
		s.mu.Unlock()
		s.logSend(proto.TypeButtonAction, 0, a)
		return
	}
	s.mu.Lock()
	a.CaptureEpoch, a.CaptureSample = s.pressEpoch, s.pressSample
	s.pressEpoch, s.pressSample = proto.NullU64{}, proto.NullU64{}
	s.mu.Unlock()
	s.cfg.Physical.CancelVolumeDisplay()

	var result *alerts.ActResult
	if a.OccurrenceID != nil {
		id := *a.OccurrenceID
		res := s.ex.Act("", id, "dismiss", "button")
		if res.Status != alerts.StatusRejected {
			s.fm.AlertEnded(id, true)
			handled := proto.HandledAlertStopped
			a.Handled = &handled
			result = &res
		}
	}
	s.logSend(proto.TypeButtonAction, 0, a)
	if result != nil {
		s.sendActResult(*result)
	}
}

// VolumeButton applies a volume press locally: during a foreground alert it
// adjusts only that occurrence (§16.5), otherwise media volume.
func (s *Supervisor) VolumeButton(up bool) {
	click := pkgbuttons.VolumeDownClick
	if up {
		click = pkgbuttons.VolumeUpClick
		s.cfg.Physical.VolumeStepUp()
	} else {
		s.cfg.Physical.VolumeStepDown()
	}
	now := s.now()
	a := s.buttonAction(click, pkgbuttons.VolumeButton, false, 0, now)
	a.CaptureEpoch, a.CaptureSample = s.captureAt(now)
	s.logSend(proto.TypeButtonAction, 0, a)
}

// MuteButton toggles device-sovereign privacy; the server's mute callback
// runs setPrivacy before the press is reported.
func (s *Supervisor) MuteButton() {
	s.cfg.Physical.MuteToggle()
	now := s.now()
	a := s.buttonAction(pkgbuttons.MuteClick, pkgbuttons.DotButton, false, 0, now)
	a.CaptureEpoch, a.CaptureSample = s.captureAt(now)
	s.logSend(proto.TypeButtonAction, 0, a)
}

func (s *Supervisor) buttonAction(click pkgbuttons.ClickType, button pkgbuttons.ButtonType, down bool, heldMs, now int64) proto.ButtonAction {
	a := proto.ButtonAction{
		ClickType: int(click), Button: string(button), Down: down, HeldMs: heldMs,
		Muted: s.cfg.Physical.IsMuted(), MonoNs: now, PhysicalSeq: s.physicalSeq.Add(1),
	}
	s.mu.Lock()
	if act := s.alertState.Active; act != nil {
		id := act.ID
		a.OccurrenceID = &id
	}
	s.mu.Unlock()
	return a
}
