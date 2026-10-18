package supervisor

import (
	"encoding/json"

	"github.com/wilbowes/EchoMuse/internal/companion"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

// Companion is the screen app's link on a screen_overlay platform
// (internal/companion, docs/echo-show-companion-plan.md): the supervisor
// keeps its touch zone and controller link status current and, on a
// turn_view_v1 platform, hands it each turn.view for the Kiosk Satellite
// plugin (§11). All must return at once.
type Companion interface {
	SetZone(companion.Zone)
	SetLink(companion.Status)
	// View relays a turn.view body to the plugin; EndView sends the last
	// view again as done when the controller can no longer finish it.
	View(body json.RawMessage)
	EndView()
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

// turnView relays turn.view to the plugin, unchanged (plan §11.2 decision
// 8). The controller sends it only to turn_view_v1 devices; elsewhere it is
// ignored, as any unknown type.
func (s *Supervisor) turnView(env proto.Envelope) {
	if s.has(proto.CapTurnView) {
		s.cfg.Companion.View(env.Body)
	}
}

// endTurnView ends a view left unfinished by a lost session, so the plugin
// does not show it forever.
func (s *Supervisor) endTurnView() {
	if s.has(proto.CapTurnView) {
		s.cfg.Companion.EndView()
	}
}
