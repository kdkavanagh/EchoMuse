package companion

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/netip"
	"sync"
	"time"

	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
)

const (
	// maxLine bounds one link line including its newline (plan §3).
	maxLine = 4096
	// frameInterval paces frames to at most 25/s, latest wins.
	frameInterval = 40 * time.Millisecond
	helloTimeout  = 5 * time.Second
	writeTimeout  = 2 * time.Second
	// listenRetry paces listening again after the port could not be bound.
	listenRetry = 5 * time.Second
)

// ServerConfig wires a Server. Empty Addr, Companion, DataDir and ProcNet
// select the device's.
type ServerConfig struct {
	Addr      string
	DeviceID  string // status device_id
	Firmware  string // hello firmware
	Companion string // hello companion: the embedded app version
	Health    *Health
	// Press receives honoured touches as action-button edges.
	Press func(pkgbuttons.ButtonClickEvent)
	// Linked, Mismatch and Overlay reach the installer: an app linked or
	// unlinked, an app speaking another link version (reinstall), an app
	// reporting its overlay op denied (grant again).
	Linked   func(bool)
	Mismatch func()
	Overlay  func()
	// DataDir's owner is the only uid a connection is accepted from; its
	// row is looked up in ProcNet.
	DataDir string
	ProcNet []string
}

// Server is the firmware end of the local link (plan §3, decision 6): it
// accepts the app's loopback connection, answers its hello, replays the
// current state and then sends frames, link status, touch zones and screen
// requests, and turns presses in an active zone into action-button edges.
// One connection at a time; a newly accepted one replaces the old.
type Server struct {
	cfg  ServerConfig
	sink *Sink

	mu     sync.Mutex
	zone   Zone
	status Status
	newest *conn // the latest accepted connection
	linked *conn // the connection past hello, which state goes to
}

type conn struct {
	nc   net.Conn
	wake chan struct{} // writer wake-up, buffered 1
	done chan struct{} // closed when the connection ends

	// Guarded by Server.mu.
	queue [][]byte // lines to write, in order
	frame bool     // a newer frame than the last sent

	pressed bool // a press was honoured and awaits its release (read goroutine)
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
	if cfg.DataDir == "" {
		cfg.DataDir = DataDir
	}
	if cfg.ProcNet == nil {
		cfg.ProcNet = ProcNetFiles
	}
	s := &Server{cfg: cfg, sink: &Sink{}, zone: ZoneNone, status: Status{Link: LinkDown, Reason: "starting"}}
	s.sink.changed = s.frameChanged
	return s
}

// Sink is the led.Controller the ring paints into.
func (s *Server) Sink() *Sink { return s.sink }

// SetZone sets the touch zone; a change is sent with the matching screen
// hold.
func (s *Server) SetZone(z Zone) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if z == s.zone {
		return
	}
	s.zone = z
	s.sendLocked(touchMsg{T: "touch", Zone: z})
	s.sendLocked(screenHoldMsg{T: "screen", Hold: z != ZoneNone})
}

// ScreenWake asks the app to light the screen (candidate open with config
// screenWake on); dropped while no app is linked.
func (s *Server) ScreenWake() {
	s.mu.Lock()
	s.sendLocked(screenWakeMsg{T: "screen", Wake: true})
	s.mu.Unlock()
}

// SetLink reports a controller link transition.
func (s *Server) SetLink(st Status) {
	s.mu.Lock()
	s.status = st
	s.sendLocked(s.statusLocked())
	s.mu.Unlock()
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
		if s.newest != nil {
			s.newest.nc.Close()
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

// handle runs one connection: the uid check, hello, replay, then reads.
func (s *Server) handle(nc net.Conn) {
	if err := s.checkPeer(nc); err != nil {
		s.fail("refused connection: %v", err)
		nc.Close()
		return
	}
	c := &conn{nc: nc, wake: make(chan struct{}, 1), done: make(chan struct{})}
	s.mu.Lock()
	if s.newest != nil {
		s.newest.nc.Close()
	}
	s.newest = c
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
	hello := helloMsg{T: "hello", Link: LinkVersion, Firmware: s.cfg.Firmware, Companion: s.cfg.Companion}
	if *m.Link != LinkVersion {
		_ = writeLines(nc, [][]byte{line(hello)})
		s.fail("app %q speaks link %d, firmware %d: reinstalling", m.Companion, *m.Link, LinkVersion)
		if s.cfg.Mismatch != nil {
			s.cfg.Mismatch()
		}
		return
	}
	overlay := m.Overlay == nil || *m.Overlay
	if !s.link(c, hello) {
		return
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
	go s.write(c)

	for {
		m, err := readMsg(r)
		if err != nil {
			if !errors.Is(err, net.ErrClosed) {
				log.Printf("[companion] link ended: %v", err)
			}
			return
		}
		s.receive(c, m)
	}
}

// link makes c the linked connection and queues the hello reply and the
// replay (status, touch, frame, then a held screen), unless a newer
// connection has replaced c meanwhile.
func (s *Server) link(c *conn, hello helloMsg) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.newest != c {
		return false
	}
	s.linked = c
	c.queue = append(c.queue, line(hello), line(s.statusLocked()), line(touchMsg{T: "touch", Zone: s.zone}),
		line(frameMsg{T: "frame", RGB: s.sink.frame()}))
	if s.zone != ZoneNone {
		c.queue = append(c.queue, line(screenHoldMsg{T: "screen", Hold: true}))
	}
	return true
}

// end closes c and, when it was the linked connection, reports the app
// unlinked.
func (s *Server) end(c *conn) {
	c.nc.Close()
	close(c.done)
	s.mu.Lock()
	if s.newest == c {
		s.newest = nil
	}
	wasLinked := s.linked == c
	if wasLinked {
		s.linked = nil
	}
	s.mu.Unlock()
	if wasLinked {
		s.cfg.Health.linkDown()
		if s.cfg.Linked != nil {
			s.cfg.Linked(false)
		}
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
				log.Printf("[companion] write: %v", err)
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

func (s *Server) frameChanged() {
	s.mu.Lock()
	if c := s.linked; c != nil {
		c.frame = true
		signal(c.wake)
	}
	s.mu.Unlock()
}

// sendLocked queues msg for the linked app, if any.
func (s *Server) sendLocked(msg any) {
	if c := s.linked; c != nil {
		c.queue = append(c.queue, line(msg))
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

// checkPeer accepts only the app's uid: the owner of its data directory
// must own the connection's far end (plan decision 6).
func (s *Server) checkPeer(nc net.Conn) error {
	local, err := netip.ParseAddrPort(nc.LocalAddr().String())
	if err != nil {
		return err
	}
	remote, err := netip.ParseAddrPort(nc.RemoteAddr().String())
	if err != nil {
		return err
	}
	want, err := ownerUID(s.cfg.DataDir)
	if err != nil {
		return fmt.Errorf("app uid: %w", err)
	}
	got, err := peerUID(s.cfg.ProcNet, local, remote)
	if err != nil {
		return fmt.Errorf("%s: %w", remote, err)
	}
	if got != want {
		return fmt.Errorf("%s is uid %d, the app is uid %d", remote, got, want)
	}
	return nil
}

func signal(ch chan struct{}) {
	select {
	case ch <- struct{}{}:
	default:
	}
}

// ── codec ───────────────────────────────────────────────────────────────────

// inMsg is any app → firmware line; T selects the fields that apply.
type inMsg struct {
	T         string `json:"t"`
	Link      *int   `json:"link"`
	Companion string `json:"companion"`
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

var errLineTooLong = errors.New("line longer than 4096 bytes")

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
