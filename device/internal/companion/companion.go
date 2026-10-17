// Package companion keeps the EchoMuse app on the LineageOS Show
// (docs/echo-show-companion-plan.md): a light bar drawn over the kiosk that
// shows the ring compositor's frames and turns touches in its stop/cancel
// zone into action-button presses. The firmware embeds the app (embed_*.go)
// and owns its life: Installer installs it by version and keeps it running,
// Server is the loopback link the app connects to, Sink is the ring's
// led.Controller on that link. Health is the shared stats.companion record.
package companion

import (
	"strings"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/proto"
)

const (
	// Package is the app's Android package.
	Package = "org.echomuse.companion"
	// LinkVersion is the local link's protocol version (plan §3).
	LinkVersion = 1
	// Addr is the loopback address the firmware listens on.
	Addr = "127.0.0.1:8771"
	// DataDir is the app's data directory; its owner is the app's uid.
	DataDir = "/data/data/" + Package
)

// Version is the embedded app's versionName ("cmp-" + 12 hex), or "" when
// this build embeds no app.
func Version() string { return strings.TrimSpace(version) }

// Zone is where the bar takes touches (plan decision 9).
type Zone string

const (
	ZoneNone   Zone = "none"   // idle: the bar ignores touches
	ZoneCancel Zone = "cancel" // a dialog focus lease is held: a tap cancels the turn
	ZoneStop   Zone = "stop"   // an alert holds foreground focus: a tap stops it
)

// LinkState is the controller link as the app shows it.
type LinkState string

const (
	LinkUp      LinkState = "up"
	LinkDown    LinkState = "down"
	LinkPending LinkState = "pending" // pending approval
)

// Status is one controller link transition. Reason is why the link is not
// up ("" while up): "starting", a client.LostReason or a
// proto.RejectReason. Controller is the host:port last reached, "" before
// any.
type Status struct {
	Link       LinkState
	Reason     string
	Controller string
}

// Health is stats.companion, written by the installer and the server.
type Health struct {
	mu      sync.Mutex
	version string // the app's version from its last hello
	linked  bool
	overlay *bool
	err     string
}

// Stats returns the stats.companion body.
func (h *Health) Stats() *proto.CompanionStats {
	h.mu.Lock()
	defer h.mu.Unlock()
	linked := h.linked
	st := &proto.CompanionStats{Linked: &linked}
	if h.version != "" {
		v := h.version
		st.Version = &v
	}
	if h.overlay != nil {
		o := *h.overlay
		st.Overlay = &o
	}
	if h.err != "" {
		e := h.err
		st.Error = &e
	}
	return st
}

func (h *Health) setError(err string) {
	h.mu.Lock()
	h.err = err
	h.mu.Unlock()
}

func (h *Health) clearError() { h.setError("") }

// linkUp records an app past hello.
func (h *Health) linkUp(version string, overlay bool) {
	h.mu.Lock()
	h.linked, h.version, h.overlay = true, version, &overlay
	h.mu.Unlock()
}

func (h *Health) linkDown() {
	h.mu.Lock()
	h.linked = false
	h.mu.Unlock()
}

func (h *Health) setOverlay(overlay bool) {
	h.mu.Lock()
	h.overlay = &overlay
	h.mu.Unlock()
}
