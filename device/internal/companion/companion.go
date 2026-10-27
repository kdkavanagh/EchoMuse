// Package companion keeps the EchoMuse app on the LineageOS Show
// (docs/echo-show-companion-plan.md): a light bar drawn over the kiosk that
// shows the ring compositor's frames and turns touches in its stop/cancel
// zone into action-button presses, and, where the kiosk is Kiosk Satellite,
// the EchoMuse plugin inside it that shows each turn full screen (plan §11).
// The firmware embeds both (embed_*.go) and owns their life: Installer
// installs the app by version and keeps it running, KioskInstaller installs
// the plugin by hash through Kiosk Satellite's remote API, Server is the
// loopback link both connect to, and Sink is the ring's led.Controller on
// that link. Health is the shared stats.companion record.
package companion

import (
	"strings"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/proto"
)

const (
	// Package is the app's Android package.
	Package = "org.echomuse.companion"
	// LinkVersion is the local link's protocol version (plan §3, §11.3).
	LinkVersion = 2
	// Addr is the loopback address the firmware listens on.
	Addr = "127.0.0.1:8771"
	// DataDir is the app's data directory; its owner is the app's uid.
	DataDir = "/data/data/" + Package

	// KioskPackage is Kiosk Satellite, the kiosk the plugin runs in.
	KioskPackage = "me.jxl.kiosk_satellite"
	// KioskDataDir is its data directory; its owner is the plugin's uid,
	// since a plugin runs in Kiosk Satellite's process (plan decision 6).
	KioskDataDir = "/data/data/" + KioskPackage
	// PluginID is the plugin's id in Kiosk Satellite.
	PluginID = "echomuse"
)

// Version is the embedded app's versionName ("cmp-" + 12 hex), or "" when
// this build embeds no app.
func Version() string { return strings.TrimSpace(version) }

// PluginVersion is the embedded plugin's manifest version, "1.0.0-" and the
// app's 12 hex (plan §11.2 decision 1), or "" when this build embeds no
// app.
func PluginVersion() string {
	v := Version()
	if v == "" {
		return ""
	}
	return "1.0.0-" + strings.TrimPrefix(v, "cmp-")
}

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

// Health is stats.companion, written by the installers and the server.
type Health struct {
	mu      sync.Mutex
	version string // the app's version from its last hello
	linked  bool
	overlay *bool
	err     string
	kiosk   kioskHealth
}

// kioskHealth is stats.companion.kiosk; "" and nil are unknown.
type kioskHealth struct {
	installed     *bool
	version       string
	remoteAdmin   *bool
	pluginVersion string
	pluginLinked  bool
	pluginErr     string
	switchedOn    *bool
}

// Stats returns the stats.companion body.
func (h *Health) Stats() *proto.CompanionStats {
	h.mu.Lock()
	defer h.mu.Unlock()
	linked := h.linked
	st := &proto.CompanionStats{Linked: &linked, Version: str(h.version), Overlay: clone(h.overlay), Error: str(h.err)}
	k := &h.kiosk
	pluginLinked := k.pluginLinked
	st.Kiosk = &proto.KioskStats{
		Installed: clone(k.installed), Version: str(k.version), RemoteAdmin: clone(k.remoteAdmin),
		Plugin: proto.KioskPluginStats{
			Version: str(k.pluginVersion), Linked: &pluginLinked, Error: str(k.pluginErr), SwitchedOn: clone(k.switchedOn),
		},
	}
	return st
}

// str is s, or nil when s is "" (unknown).
func str(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

func clone[T any](p *T) *T {
	if p == nil {
		return nil
	}
	v := *p
	return &v
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

// setKioskPackage records whether Kiosk Satellite is installed and its
// versionName. Without it, whether its remote admin answers is unknown.
func (h *Health) setKioskPackage(version string) {
	h.mu.Lock()
	installed := version != ""
	h.kiosk.installed, h.kiosk.version = &installed, version
	if !installed {
		h.kiosk.remoteAdmin = nil
	}
	h.mu.Unlock()
}

func (h *Health) setRemoteAdmin(up bool) {
	h.mu.Lock()
	h.kiosk.remoteAdmin = &up
	h.mu.Unlock()
}

func (h *Health) setPluginVersion(version string) {
	h.mu.Lock()
	h.kiosk.pluginVersion = version
	h.mu.Unlock()
}

func (h *Health) setPluginError(err string) {
	h.mu.Lock()
	h.kiosk.pluginErr = err
	h.mu.Unlock()
}

// setSwitchedOn records a master switch check: true once this run turned
// it on, and it stays true for the run.
func (h *Health) setSwitchedOn(turnedOn bool) {
	h.mu.Lock()
	if turnedOn || h.kiosk.switchedOn == nil {
		h.kiosk.switchedOn = &turnedOn
	}
	h.mu.Unlock()
}

// pluginLinkUp records a plugin past hello; a working link clears the
// plugin error.
func (h *Health) pluginLinkUp(version string) {
	h.mu.Lock()
	h.kiosk.pluginLinked, h.kiosk.pluginVersion, h.kiosk.pluginErr = true, version, ""
	h.mu.Unlock()
}

func (h *Health) pluginLinkDown() {
	h.mu.Lock()
	h.kiosk.pluginLinked = false
	h.mu.Unlock()
}
