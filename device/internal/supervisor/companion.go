package supervisor

import (
	"github.com/wilbowes/EchoMuse/internal/companion"
)

// Companion is the screen app's link on a screen_overlay platform
// (internal/companion, docs/echo-show-companion-plan.md): the supervisor
// keeps its touch zone and controller link status current. Both must
// return at once.
type Companion interface {
	SetZone(companion.Zone)
	SetLink(companion.Status)
}

// updateZone sets the companion's touch zone (plan decision 9): stop while
// an alert holds foreground focus, else cancel while a dialog focus lease
// is held, else none. Call it with no supervisor lock held after any of
// those changes.
func (s *Supervisor) updateZone() {
	if s.cfg.Companion == nil {
		return
	}
	s.zoneMu.Lock()
	defer s.zoneMu.Unlock()
	s.mu.Lock()
	dialog := len(s.leases) > 0
	s.mu.Unlock()
	zone := companion.ZoneNone
	switch {
	case s.alertForeground.Load():
		zone = companion.ZoneStop
	case dialog:
		zone = companion.ZoneCancel
	}
	s.cfg.Companion.SetZone(zone)
}

// companionLink reports a controller link transition to the companion.
// controller "" keeps the last controller reached.
func (s *Supervisor) companionLink(link companion.LinkState, reason, controller string) {
	if s.cfg.Companion == nil {
		return
	}
	s.mu.Lock()
	if controller != "" {
		s.controller = controller
	}
	controller = s.controller
	s.mu.Unlock()
	s.cfg.Companion.SetLink(companion.Status{Link: link, Reason: reason, Controller: controller})
}
