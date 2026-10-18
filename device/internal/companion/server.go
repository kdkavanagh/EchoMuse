package companion

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"maps"
	"net"
	"net/netip"
	"strings"
	"sync"
	"time"

	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
)

const (
	// maxLine bounds one link line including its newline (plan §11.3,
	// decision 6): a view carries up to 16 KiB of answer, escaped.
	maxLine = 128 << 10
	// frameInterval paces frames to at most 25/s, latest wins.
	frameInterval = 40 * time.Millisecond
	helloTimeout  = 5 * time.Second
	writeTimeout  = 2 * time.Second
	// listenRetry paces listening again after the port could not be bound.
	listenRetry = 5 * time.Second
	// viewDone is the turn.view phase that ends a view.
	viewDone = "done"
)

// Role is what a link connection is (plan §11.2 decision 6): the app, from
// the app's uid, or the Kiosk Satellite plugin, from Kiosk Satellite's.
type Role string

const (
	RoleApp    Role = "app"
	RolePlugin Role = "plugin"
)

// ServerConfig wires a Server. Empty Addr, Companion, Plugin, DataDir,
// KioskDataDir and ProcNet select the device's.
type ServerConfig struct {
	Addr      string
	DeviceID  string // status device_id
	Firmware  string // hello firmware
	Companion string // app hello companion: the embedded app version
	Plugin    string // plugin hello plugin: the embedded plugin version
	Health    *Health
	// TurnViewLinger is config turnViewLinger at start, sent to the plugin
	// after its hello.
	TurnViewLinger float64
	// Press receives honoured touches as action-button edges.
	Press func(pkgbuttons.ButtonClickEvent)
	// Linked, Mismatch and Overlay reach the app's Installer: an app linked
	// or unlinked, an app speaking another link version (reinstall), an app
	// reporting its overlay op denied (grant again).
	Linked   func(bool)
	Mismatch func()
	Overlay  func()
	// PluginLinked and PluginMismatch reach the KioskInstaller: the plugin
	// linked or unlinked, the plugin speaking another link or plugin
	// version (install by hash).
	PluginLinked   func(bool)
	PluginMismatch func()
	// DataDir's owner is the only uid role app is accepted from,
	// KioskDataDir's the only uid role plugin is; a connection's row is
	// looked up in ProcNet.
	DataDir      string
	KioskDataDir string
	ProcNet      []string
}

// Server is the firmware end of the local link (plan §3, §11.3): it accepts
// the app's and the plugin's loopback connections, answers their hellos and
// replays the current state. The app then gets frames, link status, touch
// zones and screen requests, and its presses in an active zone become
// action-button edges; the plugin gets its config, frames and turn views.
// One connection per role; a newly accepted one replaces the old one of its
// role only.
type Server struct {
	cfg  ServerConfig
	sink *Sink

	mu     sync.Mutex
	zone   Zone
	status Status
	linger float64
	view   *view
	newest map[Role]*conn // the latest accepted connection per role
	linked map[Role]*conn // the connection past hello per role, which state goes to
}

type conn struct {
	nc   net.Conn
	role Role
	wake chan struct{} // writer wake-up, buffered 1
	done chan struct{} // closed when the connection ends

	// Guarded by Server.mu.
	queue [][]byte // lines to write, in order
	frame bool     // a newer frame than the last sent

	pressed bool // a press was honoured and awaits its release (read goroutine)
}

// view is the last turn.view, as its link line.
type view struct {
	fields map[string]json.RawMessage // the body, plus t
	phase  string
	line   []byte
}

// NewServer returns a server whose ring frames arrive through Sink. Until a
// link transition is reported the status is down, "starting".
func NewServer(cfg ServerConfig) *Server {
	if cfg.Addr == "" {
		cfg.Addr = Addr
	}
	if cfg.Companion == "" {
		cfg.Companion = Version()
	}
	if cfg.Plugin == "" {
		cfg.Plugin = PluginVersion()
	}
	if cfg.DataDir == "" {
		cfg.DataDir = DataDir
	}
	if cfg.KioskDataDir == "" {
		cfg.KioskDataDir = KioskDataDir
	}
	if cfg.ProcNet == nil {
		cfg.ProcNet = ProcNetFiles
	}
	s := &Server{
		cfg: cfg, sink: &Sink{}, zone: ZoneNone, status: Status{Link: LinkDown, Reason: "starting"},
		linger: cfg.TurnViewLinger, newest: map[Role]*conn{}, linked: map[Role]*conn{},
	}
	s.sink.changed = s.frameChanged
	return s
}

// Sink is the led.Controller the ring paints into.
func (s *Server) Sink() *Sink { return s.sink }

// SetZone sets the touch zone; a change is sent to the app with the
// matching screen hold.
func (s *Server) SetZone(z Zone) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if z == s.zone {
		return
	}
	s.zone = z
	s.sendLocked(RoleApp, line(touchMsg{T: "touch", Zone: z}))
	s.sendLocked(RoleApp, line(screenHoldMsg{T: "screen", Hold: z != ZoneNone}))
}

// ScreenWake asks the app to light the screen (candidate open with config
// screenWake on); dropped while no app is linked.
func (s *Server) ScreenWake() {
	s.mu.Lock()
	s.sendLocked(RoleApp, line(screenWakeMsg{T: "screen", Wake: true}))
	s.mu.Unlock()
}

// SetLink reports a controller link transition to the app.
func (s *Server) SetLink(st Status) {
	s.mu.Lock()
	s.status = st
	s.sendLocked(RoleApp, line(s.statusLocked()))
	s.mu.Unlock()
}

// SetTurnViewLinger takes config turnViewLinger; a change is sent to the
// plugin.
func (s *Server) SetTurnViewLinger(seconds float64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if seconds == s.linger {
		return
	}
	s.linger = seconds
	s.sendLocked(RolePlugin, line(configMsg{T: "config", TurnViewLinger: seconds}))
}

// View relays a turn.view body to the plugin as a view line, unchanged but
// for t, and keeps it for the plugin's next hello (plan §11.2 decision 8).
// A body that is not an object with a string phase, or too long for one
// line, is dropped.
func (s *Server) View(body json.RawMessage) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(body, &fields); err != nil {
		log.Printf("[companion] turn.view dropped: %v", err)
		return
	}
	v, err := newView(fields)
	if err != nil {
		log.Printf("[companion] turn.view dropped: %v", err)
		return
	}
	s.mu.Lock()
	s.view = v
	s.sendLocked(RolePlugin, v.line)
	s.mu.Unlock()
}

// EndView ends a view the controller can no longer finish (its session was
// lost): a last view not done is sent again, and kept, as done, so the
// plugin does not show it forever.
func (s *Server) EndView() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.view == nil || s.view.phase == viewDone {
		return
	}
	fields := maps.Clone(s.view.fields)
	fields["phase"] = json.RawMessage(`"` + viewDone + `"`)
	v, err := newView(fields)
	if err != nil {
		// The same fields made a line before.
		log.Printf("[companion] ending view: %v", err)
		return
	}
	s.view = v
	s.sendLocked(RolePlugin, v.line)
}

// newView builds the view line from a turn.view body's fields, which it
// takes over.
func newView(fields map[string]json.RawMessage) (*view, error) {
	var phase string
	if err := json.Unmarshal(fields["phase"], &phase); err != nil {
		return nil, fmt.Errorf("phase: %w", err)
	}
	fields["t"] = json.RawMessage(`"view"`)
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(fields); err != nil {
		return nil, err
	}
	if b.Len() > maxLine {
		return nil, fmt.Errorf("%d bytes, the link takes %d", b.Len(), maxLine)
	}
	return &view{fields: fields, phase: phase, line: b.Bytes()}, nil
}

// Run listens and serves until ctx ends. A port that cannot be bound is
// retried; nothing here ends the firmware.
func (s *Server) Run(ctx context.Context) {
	for {
		ln, err := net.Listen("tcp", s.cfg.Addr)
		if err != nil {
			s.fail("listen: %v", err)
		} else {
			s.serve(ctx, ln)
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(listenRetry):
		}
	}
}

func (s *Server) serve(ctx context.Context, ln net.Listener) {
	stop := context.AfterFunc(ctx, func() {
		ln.Close()
		s.mu.Lock()
		for _, c := range s.newest {
			c.nc.Close()
		}
		s.mu.Unlock()
	})
	defer stop()
	for {
		nc, err := ln.Accept()
		if err != nil {
			if ctx.Err() != nil {
				return
			}
			var ne net.Error
			if errors.As(err, &ne) && ne.Timeout() {
				continue
			}
			s.fail("accept: %v", err)
			ln.Close()
			return
		}
		go s.handle(nc)
	}
}

func (s *Server) fail(format string, args ...any) {
	msg := fmt.Sprintf(format, args...)
	log.Printf("[companion] %s", msg)
	s.cfg.Health.setError(msg)
}

// handle runs one connection: the uid check, which names the role it may
// take, hello, replay, then reads.
func (s *Server) handle(nc net.Conn) {
	role, err := s.checkPeer(nc)
	if err != nil {
		s.fail("refused connection: %v", err)
		nc.Close()
		return
	}
	c := &conn{nc: nc, role: role, wake: make(chan struct{}, 1), done: make(chan struct{})}
	s.mu.Lock()
	if old := s.newest[role]; old != nil {
		old.nc.Close()
	}
	s.newest[role] = c
	s.mu.Unlock()
	defer s.end(c)

	r := bufio.NewReaderSize(nc, maxLine)
	_ = nc.SetReadDeadline(time.Now().Add(helloTimeout))
	m, err := readMsg(r)
	if err != nil || m.T != "hello" || m.Link == nil {
		log.Printf("[companion] no hello: %v", err)
		return
	}
	_ = nc.SetReadDeadline(time.Time{})
	claimed := m.Role
	if claimed == "" {
		claimed = RoleApp // only link-1 apps omit it
	}
	if claimed != role {
		s.fail("refused connection: %s says role %q, its uid may only be %s", nc.RemoteAddr(), claimed, role)
		return
	}
	var linked bool
	if role == RoleApp {
		linked = s.helloApp(c, m)
	} else {
		linked = s.helloPlugin(c, m)
	}
	if !linked {
		return
	}
	go s.write(c)

	for {
		m, err := readMsg(r)
		if err != nil {
			if !errors.Is(err, net.ErrClosed) {
				log.Printf("[companion] %s link ended: %v", role, err)
			}
			return
		}
		if role == RoleApp {
			s.receive(c, m)
		}
	}
}

// helloApp answers the app's hello and links it, or on another link
// version answers and asks for a reinstall.
func (s *Server) helloApp(c *conn, m inMsg) bool {
	hello := helloMsg{T: "hello", Link: LinkVersion, Firmware: s.cfg.Firmware, Companion: s.cfg.Companion}
	if *m.Link != LinkVersion {
		_ = writeLines(c.nc, [][]byte{line(hello)})
		s.fail("app %q speaks link %d, firmware %d: reinstalling", m.Companion, *m.Link, LinkVersion)
		if s.cfg.Mismatch != nil {
			s.cfg.Mismatch()
		}
		return false
	}
	overlay := m.Overlay == nil || *m.Overlay
	if !s.link(c, line(hello)) {
		return false
	}
	log.Printf("[companion] linked: app %s, overlay %v", m.Companion, overlay)
	s.cfg.Health.linkUp(m.Companion, overlay)
	s.cfg.Health.clearError()
	if s.cfg.Linked != nil {
		s.cfg.Linked(true)
	}
	if !overlay && s.cfg.Overlay != nil {
		s.cfg.Overlay()
	}
	return true
}

// helloPlugin answers the plugin's hello and links it, or on another link
// or plugin version answers and asks the KioskInstaller to run.
func (s *Server) helloPlugin(c *conn, m inMsg) bool {
	hello := pluginHelloMsg{T: "hello", Link: LinkVersion, Firmware: s.cfg.Firmware, Plugin: s.cfg.Plugin}
	if *m.Link != LinkVersion || m.Plugin != s.cfg.Plugin {
		_ = writeLines(c.nc, [][]byte{line(hello)})
		msg := fmt.Sprintf("plugin %q speaks link %d, firmware has %q on link %d: reinstalling", m.Plugin, *m.Link, s.cfg.Plugin, LinkVersion)
		log.Printf("[companion] %s", msg)
		if m.Plugin != "" {
			s.cfg.Health.setPluginVersion(m.Plugin)
		}
		s.cfg.Health.setPluginError(msg)
		if s.cfg.PluginMismatch != nil {
			s.cfg.PluginMismatch()
		}
		return false
	}
	if !s.link(c, line(hello)) {
		return false
	}
	log.Printf("[companion] linked: plugin %s", m.Plugin)
	s.cfg.Health.pluginLinkUp(m.Plugin)
	if s.cfg.PluginLinked != nil {
		s.cfg.PluginLinked(true)
	}
	return true
}

// link makes c its role's linked connection and queues the hello reply and
// the replay, unless a newer connection has replaced c meanwhile. The app
// gets status, touch, frame, then a held screen; the plugin config, frame,
// then the last view.
func (s *Server) link(c *conn, hello []byte) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.newest[c.role] != c {
		return false
	}
	s.linked[c.role] = c
	frame := line(frameMsg{T: "frame", RGB: s.sink.frame()})
	c.queue = append(c.queue, hello)
	switch c.role {
	case RoleApp:
		c.queue = append(c.queue, line(s.statusLocked()), line(touchMsg{T: "touch", Zone: s.zone}), frame)
		if s.zone != ZoneNone {
			c.queue = append(c.queue, line(screenHoldMsg{T: "screen", Hold: true}))
		}
	case RolePlugin:
		c.queue = append(c.queue, line(configMsg{T: "config", TurnViewLinger: s.linger}), frame)
		if s.view != nil {
			c.queue = append(c.queue, s.view.line)
		}
	}
	return true
}

// end closes c and, when it was its role's linked connection, reports it
// unlinked.
func (s *Server) end(c *conn) {
	c.nc.Close()
	close(c.done)
	s.mu.Lock()
	if s.newest[c.role] == c {
		delete(s.newest, c.role)
	}
	wasLinked := s.linked[c.role] == c
	if wasLinked {
		delete(s.linked, c.role)
	}
	s.mu.Unlock()
	if !wasLinked {
		return
	}
	hook := s.cfg.Linked
	if c.role == RoleApp {
		s.cfg.Health.linkDown()
	} else {
		s.cfg.Health.pluginLinkDown()
		hook = s.cfg.PluginLinked
	}
	if hook != nil {
		hook(false)
	}
}

// receive applies one app message past hello. Unknown types are ignored.
func (s *Server) receive(c *conn, m inMsg) {
	switch m.T {
	case "press":
		s.press(c, m)
	case "state":
		if m.Overlay == nil {
			return
		}
		s.cfg.Health.setOverlay(*m.Overlay)
		if !*m.Overlay {
			log.Printf("[companion] overlay op denied: granting again")
			if s.cfg.Overlay != nil {
				s.cfg.Overlay()
			}
		}
	}
}

// press forwards a touch edge as an action-button edge. A press counts only
// in an active zone; its release always follows it, with the app's
// held_ms, even when the zone has ended meanwhile.
func (s *Server) press(c *conn, m inMsg) {
	if m.Down == nil || s.cfg.Press == nil {
		return
	}
	ev := pkgbuttons.ButtonClickEvent{Button: pkgbuttons.Button{Type: pkgbuttons.DotButton}, ClickType: pkgbuttons.DotClick, Down: *m.Down}
	if ev.Down {
		s.mu.Lock()
		active := s.zone != ZoneNone
		s.mu.Unlock()
		if !active || c.pressed {
			return
		}
		c.pressed = true
	} else {
		if !c.pressed {
			return
		}
		c.pressed = false
		if m.HeldMs != nil {
			ev.HeldMs = max(0, *m.HeldMs)
		}
	}
	s.cfg.Press(ev)
}

// write sends c's queued lines in order and frames at most every
// frameInterval, until c ends or a write fails.
func (s *Server) write(c *conn) {
	last := time.Now()
	var timer *time.Timer
	for {
		s.mu.Lock()
		lines := c.queue
		c.queue = nil
		frame := c.frame && time.Since(last) >= frameInterval
		if frame {
			c.frame = false
		}
		pending := c.frame
		s.mu.Unlock()
		if frame {
			lines = append(lines, line(frameMsg{T: "frame", RGB: s.sink.frame()}))
			last = time.Now()
		}
		if len(lines) > 0 {
			if err := writeLines(c.nc, lines); err != nil {
				log.Printf("[companion] %s write: %v", c.role, err)
				c.nc.Close()
				return
			}
			continue
		}
		var tick <-chan time.Time
		if pending {
			wait := frameInterval - time.Since(last)
			if timer == nil {
				timer = time.NewTimer(wait)
			} else {
				timer.Reset(wait)
			}
			tick = timer.C
		}
		select {
		case <-c.done:
			return
		case <-c.wake:
		case <-tick:
		}
	}
}

// frameChanged marks a new frame for every linked connection.
func (s *Server) frameChanged() {
	s.mu.Lock()
	for _, c := range s.linked {
		c.frame = true
		signal(c.wake)
	}
	s.mu.Unlock()
}

// sendLocked queues a line for role's linked connection, if any.
func (s *Server) sendLocked(role Role, l []byte) {
	if c := s.linked[role]; c != nil {
		c.queue = append(c.queue, l)
		signal(c.wake)
	}
}

func (s *Server) statusLocked() statusMsg {
	m := statusMsg{T: "status", Link: s.status.Link, DeviceID: s.cfg.DeviceID}
	if s.status.Reason != "" {
		r := s.status.Reason
		m.Reason = &r
	}
	if s.status.Controller != "" {
		c := s.status.Controller
		m.Controller = &c
	}
	return m
}

// checkPeer returns the role a connection may take from its far end's uid:
// app from the owner of the app's data directory, plugin from the owner of
// Kiosk Satellite's (plan decision 6, §11.2 decision 6). Any other uid is
// refused.
func (s *Server) checkPeer(nc net.Conn) (Role, error) {
	local, err := netip.ParseAddrPort(nc.LocalAddr().String())
	if err != nil {
		return "", err
	}
	remote, err := netip.ParseAddrPort(nc.RemoteAddr().String())
	if err != nil {
		return "", err
	}
	got, err := peerUID(s.cfg.ProcNet, local, remote)
	if err != nil {
		return "", fmt.Errorf("%s: %w", remote, err)
	}
	var owners []string
	for _, o := range []struct {
		role Role
		name string
		dir  string
	}{{RoleApp, "the app", s.cfg.DataDir}, {RolePlugin, "Kiosk Satellite", s.cfg.KioskDataDir}} {
		uid, err := ownerUID(o.dir)
		if err != nil {
			owners = append(owners, fmt.Sprintf("%s's uid: %v", o.name, err))
			continue
		}
		if uid == got {
			return o.role, nil
		}
		owners = append(owners, fmt.Sprintf("%s is uid %d", o.name, uid))
	}
	return "", fmt.Errorf("%s is uid %d; %s", remote, got, strings.Join(owners, ", "))
}

func signal(ch chan struct{}) {
	select {
	case ch <- struct{}{}:
	default:
	}
}

// ── codec ───────────────────────────────────────────────────────────────────

// inMsg is any app or plugin → firmware line; T selects the fields that
// apply.
type inMsg struct {
	T         string `json:"t"`
	Link      *int   `json:"link"`
	Role      Role   `json:"role"`
	Companion string `json:"companion"`
	Plugin    string `json:"plugin"`
	Overlay   *bool  `json:"overlay"`
	Down      *bool  `json:"down"`
	HeldMs    *int64 `json:"held_ms"`
}

type helloMsg struct {
	T         string `json:"t"`
	Link      int    `json:"link"`
	Firmware  string `json:"firmware"`
	Companion string `json:"companion"`
}

type pluginHelloMsg struct {
	T        string `json:"t"`
	Link     int    `json:"link"`
	Firmware string `json:"firmware"`
	Plugin   string `json:"plugin"`
}

type configMsg struct {
	T              string  `json:"t"`
	TurnViewLinger float64 `json:"turn_view_linger"`
}

type statusMsg struct {
	T          string    `json:"t"`
	Link       LinkState `json:"link"`
	Reason     *string   `json:"reason"`
	DeviceID   string    `json:"device_id"`
	Controller *string   `json:"controller"`
}

type touchMsg struct {
	T    string `json:"t"`
	Zone Zone   `json:"zone"`
}

type screenWakeMsg struct {
	T    string `json:"t"`
	Wake bool   `json:"wake"`
}

type screenHoldMsg struct {
	T    string `json:"t"`
	Hold bool   `json:"hold"`
}

type frameMsg struct {
	T   string `json:"t"`
	RGB string `json:"rgb"`
}

var errLineTooLong = fmt.Errorf("line longer than %d bytes", maxLine)

// readMsg reads one line. A line over maxLine or malformed JSON is a
// protocol error, which ends the connection.
func readMsg(r *bufio.Reader) (inMsg, error) {
	var m inMsg
	b, err := r.ReadSlice('\n')
	if errors.Is(err, bufio.ErrBufferFull) {
		return m, errLineTooLong
	}
	if err != nil {
		return m, err
	}
	if err := json.Unmarshal(b, &m); err != nil {
		return m, fmt.Errorf("malformed line: %w", err)
	}
	return m, nil
}

// line is msg as one NDJSON line. Every message type here marshals.
func line(msg any) []byte {
	b, _ := json.Marshal(msg)
	return append(b, '\n')
}

func writeLines(nc net.Conn, lines [][]byte) error {
	_ = nc.SetWriteDeadline(time.Now().Add(writeTimeout))
	for _, l := range lines {
		if _, err := nc.Write(l); err != nil {
			return err
		}
	}
	return nil
}
