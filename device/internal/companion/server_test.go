package companion

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
	"github.com/wilbowes/EchoMuse/pkg/led"
)

const appHello = `{"t":"hello","link":1,"companion":"cmp-aaaaaaaaaaaa","overlay":true,"screen_on":true}`

// linkEnv is a Server on an ephemeral loopback port whose app uid is the
// test's own (the owner of a temp DataDir) and whose /proc/net/tcp is a
// file the test writes before each dial.
type linkEnv struct {
	t      *testing.T
	s      *Server
	health *Health
	addr   netip.AddrPort
	proc   string

	mu        sync.Mutex
	presses   []pkgbuttons.ButtonClickEvent
	linked    []bool
	mismatch  int
	overlays  int
	pressedCh chan struct{}
}

func newLinkEnv(t *testing.T) *linkEnv {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	e := &linkEnv{t: t, health: &Health{}, addr: netip.MustParseAddrPort(ln.Addr().String()),
		proc: filepath.Join(t.TempDir(), "tcp"), pressedCh: make(chan struct{}, 16)}
	e.s = NewServer(ServerConfig{
		DeviceID: "G6G16D12143203LD", Firmware: "fw-111111111111", Companion: "cmp-aaaaaaaaaaaa", Health: e.health,
		Press: func(ev pkgbuttons.ButtonClickEvent) {
			e.mu.Lock()
			e.presses = append(e.presses, ev)
			e.mu.Unlock()
			e.pressedCh <- struct{}{}
		},
		Linked:   func(up bool) { e.mu.Lock(); e.linked = append(e.linked, up); e.mu.Unlock() },
		Mismatch: func() { e.mu.Lock(); e.mismatch++; e.mu.Unlock() },
		Overlay:  func() { e.mu.Lock(); e.overlays++; e.mu.Unlock() },
		DataDir:  t.TempDir(),
		ProcNet:  []string{e.proc},
	})
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { e.s.serve(ctx, ln); close(done) }()
	t.Cleanup(func() { cancel(); <-done })
	return e
}

// appConn is the app's end of the link.
type appConn struct {
	t  *testing.T
	nc net.Conn
	r  *bufio.Reader
}

// dial connects from a fixed local port whose socket row, written first,
// carries uid.
func (e *linkEnv) dial(uid int) *appConn {
	e.t.Helper()
	free, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		e.t.Fatal(err)
	}
	local := netip.MustParseAddrPort(free.Addr().String())
	free.Close()
	// The app's dual-stack socket, as Android's Java sockets may be.
	mapped := func(ap netip.AddrPort) netip.AddrPort {
		return netip.AddrPortFrom(netip.AddrFrom16(ap.Addr().As16()), ap.Port())
	}
	rows := procHeader + procRow(e.addr, local, "01", 0) + procRow(mapped(local), mapped(e.addr), "01", uid)
	if err := os.WriteFile(e.proc, []byte(rows), 0o644); err != nil {
		e.t.Fatal(err)
	}
	d := net.Dialer{LocalAddr: net.TCPAddrFromAddrPort(local)}
	nc, err := d.Dial("tcp", e.addr.String())
	if err != nil {
		e.t.Fatal(err)
	}
	e.t.Cleanup(func() { nc.Close() })
	return &appConn{t: e.t, nc: nc, r: bufio.NewReader(nc)}
}

func (a *appConn) send(line string) {
	a.t.Helper()
	if _, err := a.nc.Write([]byte(line + "\n")); err != nil {
		a.t.Fatal(err)
	}
}

// next reads one line, failing after 3 s.
func (a *appConn) next() string {
	a.t.Helper()
	_ = a.nc.SetReadDeadline(time.Now().Add(3 * time.Second))
	line, err := a.r.ReadString('\n')
	if err != nil {
		a.t.Fatalf("read: %v (got %q)", err, line)
	}
	return strings.TrimSuffix(line, "\n")
}

// closed reports whether the firmware closed the connection within 3 s.
func (a *appConn) closed() bool {
	_ = a.nc.SetReadDeadline(time.Now().Add(3 * time.Second))
	for {
		if _, err := a.r.ReadString('\n'); err != nil {
			return !isTimeout(err)
		}
	}
}

func isTimeout(err error) bool {
	ne, ok := err.(net.Error)
	return ok && ne.Timeout()
}

func (e *linkEnv) link() *appConn {
	e.t.Helper()
	a := e.dial(os.Getuid())
	a.send(appHello)
	for range 4 { // hello, status, touch, frame
		a.next()
	}
	e.waitLinked(true)
	return a
}

func (e *linkEnv) waitLinked(want bool) {
	e.t.Helper()
	for deadline := time.Now().Add(3 * time.Second); time.Now().Before(deadline); time.Sleep(5 * time.Millisecond) {
		if *e.health.Stats().Linked == want {
			return
		}
	}
	e.t.Fatalf("linked never became %v", want)
}

// Plan §3: the firmware answers hello and replays status, touch, the last
// frame and, while held, the screen hold, in that order.
func TestReplayAfterHello(t *testing.T) {
	e := newLinkEnv(t)
	e.s.SetLink(Status{Link: LinkPending, Reason: "pending_approval", Controller: "10.0.0.2:8770"})
	e.s.SetZone(ZoneStop)
	_ = e.s.Sink().SetLEDs(led.Led{ID: 0, R: 0xff}, led.Led{ID: 11, G: 0x80, B: 0x0a}, led.Led{ID: 12, R: 1})

	a := e.dial(os.Getuid())
	a.send(appHello)
	want := []string{
		`{"t":"hello","link":1,"firmware":"fw-111111111111","companion":"cmp-aaaaaaaaaaaa"}`,
		`{"t":"status","link":"pending","reason":"pending_approval","device_id":"G6G16D12143203LD","controller":"10.0.0.2:8770"}`,
		`{"t":"touch","zone":"stop"}`,
		`{"t":"frame","rgb":"ff0000` + strings.Repeat("000000", 10) + `00800a"}`,
		`{"t":"screen","hold":true}`,
	}
	for i, w := range want {
		if got := a.next(); got != w {
			t.Fatalf("line %d\n got %s\nwant %s", i, got, w)
		}
	}
	e.waitLinked(true)
	st := e.health.Stats()
	if *st.Version != "cmp-aaaaaaaaaaaa" || !*st.Overlay || st.Error != nil {
		t.Fatalf("health %+v", st)
	}
}

// Before any paint or link transition the replay is an all-black frame and
// the starting status, with no screen hold.
func TestReplayDefaults(t *testing.T) {
	e := newLinkEnv(t)
	a := e.dial(os.Getuid())
	a.send(appHello)
	a.next()
	for _, w := range []string{
		`{"t":"status","link":"down","reason":"starting","device_id":"G6G16D12143203LD","controller":null}`,
		`{"t":"touch","zone":"none"}`,
		`{"t":"frame","rgb":"` + strings.Repeat("0", 72) + `"}`,
	} {
		if got := a.next(); got != w {
			t.Fatalf("got %s\nwant %s", got, w)
		}
	}
	e.s.ScreenWake()
	if got := a.next(); got != `{"t":"screen","wake":true}` {
		t.Fatalf("after replay: %s", got)
	}
}

// Changes after hello go out as they happen: zone with its screen hold
// (repeats suppressed), screen wake, link status.
func TestStateMessages(t *testing.T) {
	e := newLinkEnv(t)
	a := e.link()
	e.s.SetZone(ZoneCancel)
	e.s.SetZone(ZoneCancel)
	e.s.SetZone(ZoneStop)
	e.s.SetZone(ZoneNone)
	e.s.ScreenWake()
	e.s.SetLink(Status{Link: LinkUp, Controller: "10.0.0.2:8767"})
	for _, w := range []string{
		`{"t":"touch","zone":"cancel"}`, `{"t":"screen","hold":true}`,
		`{"t":"touch","zone":"stop"}`, `{"t":"screen","hold":true}`,
		`{"t":"touch","zone":"none"}`, `{"t":"screen","hold":false}`,
		`{"t":"screen","wake":true}`,
		`{"t":"status","link":"up","reason":null,"device_id":"G6G16D12143203LD","controller":"10.0.0.2:8767"}`,
	} {
		if got := a.next(); got != w {
			t.Fatalf("got %s\nwant %s", got, w)
		}
	}
}

// Frames are coalesced to at most 25/s, latest wins.
func TestFramesCoalesced(t *testing.T) {
	e := newLinkEnv(t)
	a := e.link()
	start := time.Now()
	for i := range 100 {
		_ = e.s.Sink().SetLEDs(led.Led{ID: 0, R: uint8(i)})
		time.Sleep(time.Millisecond)
	}
	elapsed := time.Since(start)
	last := `{"t":"frame","rgb":"630000` + strings.Repeat("000000", 11) + `"}`
	var frames int
	for {
		got := a.next()
		frames++
		if got == last {
			break
		}
	}
	if limit := int(elapsed/frameInterval) + 2; frames > limit {
		t.Fatalf("%d frames in %s, want at most %d", frames, elapsed, limit)
	}
}

// A connection from any uid but the app's is closed unanswered.
func TestForeignUIDRefused(t *testing.T) {
	e := newLinkEnv(t)
	a := e.dial(os.Getuid() + 1)
	if !a.closed() {
		t.Fatal("foreign uid kept its connection")
	}
	st := e.health.Stats()
	if *st.Linked || st.Error == nil || !strings.Contains(*st.Error, "refused connection") {
		t.Fatalf("health %+v", st)
	}
}

// Touch is the action button only in an active zone; the release of an
// honoured press always follows, with the app's held_ms.
func TestPressInActiveZoneOnly(t *testing.T) {
	e := newLinkEnv(t)
	a := e.link()
	e.s.SetZone(ZoneStop)
	a.send(`{"t":"press","down":true}`)
	<-e.pressedCh
	e.s.SetZone(ZoneNone) // the tap stopped the alert before its release
	a.send(`{"t":"press","down":false,"held_ms":812}`)
	<-e.pressedCh
	a.send(`{"t":"press","down":true}`)
	a.send(`{"t":"press","down":false,"held_ms":90}`)
	select {
	case <-e.pressedCh:
		t.Fatal("a press outside an active zone reached the action button")
	case <-time.After(100 * time.Millisecond):
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	dot := pkgbuttons.Button{Type: pkgbuttons.DotButton}
	want := []pkgbuttons.ButtonClickEvent{
		{Button: dot, ClickType: pkgbuttons.DotClick, Down: true},
		{Button: dot, ClickType: pkgbuttons.DotClick, Down: false, HeldMs: 812},
	}
	if !slices.Equal(e.presses, want) {
		t.Fatalf("presses %+v, want %+v", e.presses, want)
	}
}

// Unknown types are ignored; malformed JSON and over-long lines end the
// connection, which reports the app unlinked.
func TestCodecErrors(t *testing.T) {
	for name, bad := range map[string]string{
		"malformed": `{"t":"press",`,
		"too long":  `{"t":"x","pad":"` + strings.Repeat("a", maxLine) + `"}`,
	} {
		e := newLinkEnv(t)
		a := e.link()
		a.send(`{"t":"captions","text":"later"}`)
		e.s.ScreenWake()
		if got := a.next(); got != `{"t":"screen","wake":true}` {
			t.Fatalf("%s: unknown type broke the link: %s", name, got)
		}
		a.send(bad)
		if !a.closed() {
			t.Fatalf("%s: connection survived", name)
		}
		e.waitLinked(false)
		e.mu.Lock()
		if !slices.Equal(e.linked, []bool{true, false}) {
			t.Errorf("%s: linked hooks %v", name, e.linked)
		}
		e.mu.Unlock()
	}
}

// A line of exactly maxLine bytes, newline included, is accepted.
func TestLineCap(t *testing.T) {
	e := newLinkEnv(t)
	a := e.link()
	head := `{"t":"x","pad":"`
	a.send(head + strings.Repeat("a", maxLine-len(head)-3) + `"}`)
	e.s.ScreenWake()
	if got := a.next(); got != `{"t":"screen","wake":true}` {
		t.Fatalf("got %s", got)
	}
}

// Another link version: the firmware answers with its own hello, closes
// and asks for a reinstall.
func TestLinkVersionMismatch(t *testing.T) {
	e := newLinkEnv(t)
	a := e.dial(os.Getuid())
	a.send(`{"t":"hello","link":2,"companion":"cmp-bbbbbbbbbbbb","overlay":true,"screen_on":true}`)
	var hello helloMsg
	if err := json.Unmarshal([]byte(a.next()), &hello); err != nil || hello.Link != 1 {
		t.Fatalf("hello %+v, %v", hello, err)
	}
	if !a.closed() {
		t.Fatal("mismatched app kept its connection")
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.mismatch != 1 || len(e.linked) != 0 {
		t.Fatalf("mismatch %d linked %v", e.mismatch, e.linked)
	}
}

// overlay:false, in hello or later, asks for the appop again and shows in
// health.
func TestOverlayDenied(t *testing.T) {
	e := newLinkEnv(t)
	a := e.link()
	a.send(`{"t":"state","overlay":false,"screen_on":true}`)
	for deadline := time.Now().Add(3 * time.Second); *e.health.Stats().Overlay; time.Sleep(5 * time.Millisecond) {
		if time.Now().After(deadline) {
			t.Fatal("overlay state not recorded")
		}
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.overlays != 1 {
		t.Fatalf("grant requests %d", e.overlays)
	}
}

// A new connection replaces the old one, which is closed.
func TestNewConnectionReplacesOld(t *testing.T) {
	e := newLinkEnv(t)
	old := e.link()
	b := e.dial(os.Getuid())
	if !old.closed() {
		t.Fatal("old connection kept")
	}
	b.send(appHello)
	for range 4 {
		b.next()
	}
	e.s.ScreenWake()
	if got := b.next(); got != `{"t":"screen","wake":true}` {
		t.Fatalf("new connection got %s", got)
	}
	e.waitLinked(true)
}
