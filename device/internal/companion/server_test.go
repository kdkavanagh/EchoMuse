package companion

import (
	"bufio"
	"bytes"
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

const (
	appHello    = `{"t":"hello","link":2,"role":"app","companion":"cmp-aaaaaaaaaaaa","overlay":true,"screen_on":true}`
	pluginHello = `{"t":"hello","link":2,"role":"plugin","plugin":"1.0.0-aaaaaaaaaaaa"}`
)

var blackFrame = `{"t":"frame","rgb":"` + strings.Repeat("0", 72) + `"}`

// linkEnv is a Server on an ephemeral loopback port whose app uid is the
// test's own (the owner of a temp DataDir), whose Kiosk Satellite uid is
// another (foreignDir), and whose /proc/net/tcp is a file the test writes
// before each dial.
type linkEnv struct {
	t         *testing.T
	s         *Server
	health    *Health
	addr      netip.AddrPort
	proc      string
	pluginUID int

	mu             sync.Mutex
	presses        []pkgbuttons.ButtonClickEvent
	linked         []bool
	mismatch       int
	overlays       int
	pluginLinked   []bool
	pluginMismatch int
	pressedCh      chan struct{}
}

// foreignDir is a directory owned by a uid other than the test's, which
// stands for Kiosk Satellite's data directory.
func foreignDir(t *testing.T) (string, int) {
	t.Helper()
	if os.Getuid() == 0 {
		dir := t.TempDir()
		if err := os.Chown(dir, 4242, 4242); err != nil {
			t.Fatal(err)
		}
		return dir, 4242
	}
	uid, err := ownerUID("/")
	if err != nil || uid == os.Getuid() {
		t.Skip("no directory owned by another uid")
	}
	return "/", uid
}

func newLinkEnv(t *testing.T) *linkEnv {
	t.Helper()
	kioskDir, pluginUID := foreignDir(t)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	e := &linkEnv{t: t, health: &Health{}, addr: netip.MustParseAddrPort(ln.Addr().String()),
		proc: filepath.Join(t.TempDir(), "tcp"), pluginUID: pluginUID, pressedCh: make(chan struct{}, 16)}
	e.s = NewServer(ServerConfig{
		DeviceID: "G6G16D12143203LD", Firmware: "fw-111111111111", Companion: "cmp-aaaaaaaaaaaa",
		Plugin: "1.0.0-aaaaaaaaaaaa", Health: e.health, TurnViewLinger: 6,
		Press: func(ev pkgbuttons.ButtonClickEvent) {
			e.mu.Lock()
			e.presses = append(e.presses, ev)
			e.mu.Unlock()
			e.pressedCh <- struct{}{}
		},
		Linked:         func(up bool) { e.mu.Lock(); e.linked = append(e.linked, up); e.mu.Unlock() },
		Mismatch:       func() { e.mu.Lock(); e.mismatch++; e.mu.Unlock() },
		Overlay:        func() { e.mu.Lock(); e.overlays++; e.mu.Unlock() },
		PluginLinked:   func(up bool) { e.mu.Lock(); e.pluginLinked = append(e.pluginLinked, up); e.mu.Unlock() },
		PluginMismatch: func() { e.mu.Lock(); e.pluginMismatch++; e.mu.Unlock() },
		DataDir:        t.TempDir(),
		KioskDataDir:   kioskDir,
		ProcNet:        []string{e.proc},
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

// linkPlugin links a plugin and reads its replay: hello, config, frame.
func (e *linkEnv) linkPlugin() *appConn {
	e.t.Helper()
	p := e.dial(e.pluginUID)
	p.send(pluginHello)
	for range 3 {
		p.next()
	}
	e.waitPluginLinked(true)
	return p
}

func (e *linkEnv) waitPluginLinked(want bool) {
	e.t.Helper()
	for deadline := time.Now().Add(3 * time.Second); time.Now().Before(deadline); time.Sleep(5 * time.Millisecond) {
		if *e.health.Stats().Kiosk.Plugin.Linked == want {
			return
		}
	}
	e.t.Fatalf("plugin linked never became %v", want)
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
		`{"t":"hello","link":2,"firmware":"fw-111111111111","companion":"cmp-aaaaaaaaaaaa"}`,
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

// Unknown types are ignored; malformed JSON and lines over 128 KiB end the
// connection, which reports the app unlinked.
func TestCodecErrors(t *testing.T) {
	for name, bad := range map[string]string{
		"malformed": `{"t":"press",`,
		"too long":  `{"t":"x","pad":"` + strings.Repeat("a", 131072) + `"}`,
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

// A line of exactly 128 KiB, newline included, is accepted (plan §11.2
// decision 6).
func TestLineCap(t *testing.T) {
	e := newLinkEnv(t)
	a := e.link()
	head := `{"t":"x","pad":"`
	a.send(head + strings.Repeat("a", 131072-len(head)-3) + `"}`)
	e.s.ScreenWake()
	if got := a.next(); got != `{"t":"screen","wake":true}` {
		t.Fatalf("got %s", got)
	}
}

// A link-1 app (no role): the firmware answers with its own hello, closes
// and asks for a reinstall.
func TestLinkVersionMismatch(t *testing.T) {
	e := newLinkEnv(t)
	a := e.dial(os.Getuid())
	a.send(`{"t":"hello","link":1,"companion":"cmp-bbbbbbbbbbbb","overlay":true,"screen_on":true}`)
	var hello helloMsg
	if err := json.Unmarshal([]byte(a.next()), &hello); err != nil || hello.Link != 2 || hello.Companion != "cmp-aaaaaaaaaaaa" {
		t.Fatalf("hello %+v, %v", hello, err)
	}
	if !a.closed() {
		t.Fatal("mismatched app kept its connection")
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.mismatch != 1 || len(e.linked) != 0 || e.pluginMismatch != 0 {
		t.Fatalf("mismatch %d linked %v plugin mismatch %d", e.mismatch, e.linked, e.pluginMismatch)
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

// A new connection replaces the old one of its role only (plan §11.2
// decision 6): a new plugin leaves the app linked, a new app the plugin.
func TestNewConnectionReplacesOldOfItsRole(t *testing.T) {
	e := newLinkEnv(t)
	app := e.link()
	plugin := e.linkPlugin()

	plugin2 := e.dial(e.pluginUID)
	if !plugin.closed() {
		t.Fatal("old plugin connection kept")
	}
	e.waitPluginLinked(false)
	plugin2.send(pluginHello)
	for range 3 {
		plugin2.next()
	}
	e.s.ScreenWake()
	if got := app.next(); got != `{"t":"screen","wake":true}` {
		t.Fatalf("app after a new plugin got %s", got)
	}

	app2 := e.dial(os.Getuid())
	if !app.closed() {
		t.Fatal("old app connection kept")
	}
	e.waitLinked(false)
	app2.send(appHello)
	for range 4 {
		app2.next()
	}
	e.s.ScreenWake()
	if got := app2.next(); got != `{"t":"screen","wake":true}` {
		t.Fatalf("new app got %s", got)
	}
	e.s.View(json.RawMessage(`{"turn_id":"t1","phase":"listening"}`))
	if got := plugin2.next(); got != `{"phase":"listening","t":"view","turn_id":"t1"}` {
		t.Fatalf("plugin after a new app got %s", got)
	}
	e.waitLinked(true)
	e.waitPluginLinked(true)
	e.mu.Lock()
	defer e.mu.Unlock()
	if !slices.Equal(e.linked, []bool{true, false, true}) || !slices.Equal(e.pluginLinked, []bool{true, false, true}) {
		t.Fatalf("linked hooks app %v plugin %v", e.linked, e.pluginLinked)
	}
}

// Role app only from the app's uid, role plugin only from Kiosk
// Satellite's; a hello without a role is the app's. Every other pairing is
// closed and reported, as is any other uid (TestForeignUIDRefused).
func TestRoleUIDMatrix(t *testing.T) {
	for _, tc := range []struct {
		name   string
		plugin bool // dial from Kiosk Satellite's uid, else the app's
		hello  string
		linked bool
	}{
		{"app from the app's uid", false, appHello, true},
		{"no role from the app's uid", false, `{"t":"hello","link":2,"companion":"cmp-aaaaaaaaaaaa","overlay":true}`, true},
		{"plugin from the app's uid", false, pluginHello, false},
		{"plugin from Kiosk Satellite's uid", true, pluginHello, true},
		{"app from Kiosk Satellite's uid", true, appHello, false},
		{"no role from Kiosk Satellite's uid", true, `{"t":"hello","link":2,"plugin":"1.0.0-aaaaaaaaaaaa"}`, false},
		{"another role from Kiosk Satellite's uid", true, `{"t":"hello","link":2,"role":"admin","plugin":"1.0.0-aaaaaaaaaaaa"}`, false},
	} {
		e := newLinkEnv(t)
		uid := os.Getuid()
		if tc.plugin {
			uid = e.pluginUID
		}
		c := e.dial(uid)
		c.send(tc.hello)
		if !tc.linked {
			if !c.closed() {
				t.Errorf("%s: connection kept", tc.name)
			}
			st := e.health.Stats()
			if *st.Linked || *st.Kiosk.Plugin.Linked || st.Error == nil || !strings.Contains(*st.Error, "refused connection") {
				t.Errorf("%s: health %+v", tc.name, st)
			}
			continue
		}
		var hello map[string]any
		if err := json.Unmarshal([]byte(c.next()), &hello); err != nil {
			t.Fatalf("%s: %v", tc.name, err)
		}
		if _, ok := hello["plugin"]; ok != tc.plugin {
			t.Errorf("%s: hello %v", tc.name, hello)
		}
		if tc.plugin {
			e.waitPluginLinked(true)
		} else {
			e.waitLinked(true)
		}
	}
}

// Plan §11.3: after the plugin's hello the firmware sends hello, config,
// the last frame and the last view, then views, linger changes and frames
// as they happen; nothing meant for the app reaches it, and nothing the
// plugin sends past hello acts.
func TestPluginReplayAndRelay(t *testing.T) {
	e := newLinkEnv(t)
	e.s.SetTurnViewLinger(4)
	_ = e.s.Sink().SetLEDs(led.Led{ID: 0, B: 0xff})
	e.s.View(json.RawMessage(`{"turn_id":"t1","phase":"thinking","heard":"is the <door> locked?","answer":null,"final":false,"follow_up":false}`))
	e.s.SetZone(ZoneStop)

	p := e.dial(e.pluginUID)
	p.send(pluginHello)
	want := []string{
		`{"t":"hello","link":2,"firmware":"fw-111111111111","plugin":"1.0.0-aaaaaaaaaaaa"}`,
		`{"t":"config","turn_view_linger":4}`,
		`{"t":"frame","rgb":"0000ff` + strings.Repeat("000000", 11) + `"}`,
		`{"answer":null,"final":false,"follow_up":false,"heard":"is the <door> locked?","phase":"thinking","t":"view","turn_id":"t1"}`,
	}
	for i, w := range want {
		if got := p.next(); got != w {
			t.Fatalf("line %d\n got %s\nwant %s", i, got, w)
		}
	}
	e.waitPluginLinked(true)
	if st := e.health.Stats().Kiosk.Plugin; *st.Version != "1.0.0-aaaaaaaaaaaa" || st.Error != nil {
		t.Fatalf("plugin health %+v", st)
	}

	p.send(`{"t":"press","down":true}`)
	e.s.SetZone(ZoneCancel)
	e.s.ScreenWake()
	e.s.SetLink(Status{Link: LinkUp})
	e.s.SetTurnViewLinger(4)
	e.s.SetTurnViewLinger(0)
	e.s.View(json.RawMessage(`{"turn_id":"t1","phase":"answering","heard":"is the <door> locked?","answer":"Yes & bolted.","final":true,"follow_up":false,"tools":[]}`))
	_ = e.s.Sink().SetLEDs(led.Led{ID: 0, R: 0xff})
	for _, w := range []string{
		`{"t":"config","turn_view_linger":0}`,
		`{"answer":"Yes & bolted.","final":true,"follow_up":false,"heard":"is the <door> locked?","phase":"answering","t":"view","tools":[],"turn_id":"t1"}`,
		`{"t":"frame","rgb":"ff0000` + strings.Repeat("000000", 11) + `"}`,
	} {
		if got := p.next(); got != w {
			t.Fatalf("got %s\nwant %s", got, w)
		}
	}
	select {
	case <-e.pressedCh:
		t.Fatal("a plugin press reached the action button")
	default:
	}

	// Bodies the plugin could not parse are not relayed.
	e.s.View(json.RawMessage(`{"turn_id":"t2"}`))
	e.s.View(json.RawMessage(`[1]`))
	e.s.View(json.RawMessage(`{"turn_id":"t2","phase":"listening","answer":"` + strings.Repeat("x", 131072) + `"}`))
	e.s.View(json.RawMessage(`{"turn_id":"t3","phase":"listening"}`))
	if got := p.next(); got != `{"phase":"listening","t":"view","turn_id":"t3"}` {
		t.Fatalf("after bad bodies got %s", got)
	}
}

// A lost controller session ends a view not yet done: the plugin gets it
// again as done, and so does a plugin linking later; a done view is not
// sent again.
func TestEndView(t *testing.T) {
	e := newLinkEnv(t)
	e.s.EndView() // no view yet
	p := e.linkPlugin()
	e.s.View(json.RawMessage(`{"turn_id":"t1","phase":"answering","heard":"hi","answer":"Hello","final":false,"follow_up":true}`))
	p.next()
	e.s.EndView()
	done := `{"answer":"Hello","final":false,"follow_up":true,"heard":"hi","phase":"done","t":"view","turn_id":"t1"}`
	if got := p.next(); got != done {
		t.Fatalf("got %s\nwant %s", got, done)
	}
	e.s.EndView()
	e.s.SetTurnViewLinger(2)
	if got := p.next(); got != `{"t":"config","turn_view_linger":2}` {
		t.Fatalf("a done view was ended again: %s", got)
	}

	p2 := e.dial(e.pluginUID)
	p2.send(pluginHello)
	for range 3 {
		p2.next()
	}
	if got := p2.next(); got != done {
		t.Fatalf("replay %s\nwant %s", got, done)
	}
}

// timer_view_v1: each timer.view reaches the plugin as a timers line,
// unchanged but for t, and the last one follows the last view after a new
// plugin's hello; bodies the plugin could not parse are not relayed.
func TestTimersRelayAndReplay(t *testing.T) {
	e := newLinkEnv(t)
	p := e.linkPlugin()
	e.s.Timers(json.RawMessage(`{"timers":[{"id":"a","name":"<pasta>","total_ms":300000,"remaining_ms":123456,"active":true},` +
		`{"id":"b","name":null,"total_ms":60000,"remaining_ms":60000,"active":false}]}`))
	timers := `{"t":"timers","timers":[{"id":"a","name":"<pasta>","total_ms":300000,"remaining_ms":123456,"active":true},` +
		`{"id":"b","name":null,"total_ms":60000,"remaining_ms":60000,"active":false}]}`
	if got := p.next(); got != timers {
		t.Fatalf("got %s\nwant %s", got, timers)
	}

	// Bodies the plugin could not parse are not relayed.
	e.s.Timers(json.RawMessage(`[1]`))
	e.s.Timers(json.RawMessage(`{}`))
	e.s.Timers(json.RawMessage(`{"timers":null}`))
	e.s.Timers(json.RawMessage(`{"timers":{"id":"a"}}`))
	e.s.Timers(json.RawMessage(`{"timers":["` + strings.Repeat("x", 131072) + `"]}`))
	e.s.Timers(json.RawMessage(`{"timers":[]}`))
	if got := p.next(); got != `{"t":"timers","timers":[]}` {
		t.Fatalf("after bad bodies got %s", got)
	}

	e.s.View(json.RawMessage(`{"turn_id":"t1","phase":"listening"}`))
	p.next()
	e.s.Timers(json.RawMessage(`{"timers":[{"id":"a","name":null,"total_ms":1000,"remaining_ms":500,"active":true}]}`))
	p.next()
	p2 := e.dial(e.pluginUID)
	p2.send(pluginHello)
	for range 3 {
		p2.next()
	}
	for _, w := range []string{
		`{"phase":"listening","t":"view","turn_id":"t1"}`,
		`{"t":"timers","timers":[{"id":"a","name":null,"total_ms":1000,"remaining_ms":500,"active":true}]}`,
	} {
		if got := p2.next(); got != w {
			t.Fatalf("replay %s\nwant %s", got, w)
		}
	}
}

// A lost controller session clears the plugin's timers: it gets the empty
// list once, and so does a plugin linking later; nothing is sent before
// the first timers line or when the list is already empty.
func TestEndTimers(t *testing.T) {
	if l, err := timersLine(map[string]json.RawMessage{"timers": json.RawMessage(`[]`)}); err != nil || !bytes.Equal(l, noTimers) {
		t.Fatalf("empty list encodes as %q (%v), want %q", l, err, noTimers)
	}
	e := newLinkEnv(t)
	e.s.EndTimers() // no timers yet
	p := e.linkPlugin()
	e.s.SetTurnViewLinger(2)
	if got := p.next(); got != `{"t":"config","turn_view_linger":2}` {
		t.Fatalf("timers ended before any: %s", got)
	}
	e.s.Timers(json.RawMessage(`{"timers":[{"id":"a","name":"tea","total_ms":180000,"remaining_ms":90000,"active":true}]}`))
	p.next()
	e.s.EndTimers()
	empty := `{"t":"timers","timers":[]}`
	if got := p.next(); got != empty {
		t.Fatalf("got %s\nwant %s", got, empty)
	}
	e.s.EndTimers()
	e.s.SetTurnViewLinger(3)
	if got := p.next(); got != `{"t":"config","turn_view_linger":3}` {
		t.Fatalf("empty timers were ended again: %s", got)
	}

	p2 := e.dial(e.pluginUID)
	p2.send(pluginHello)
	for range 3 {
		p2.next()
	}
	if got := p2.next(); got != empty {
		t.Fatalf("replay %s\nwant %s", got, empty)
	}
}

// A plugin speaking another link or plugin version gets the firmware's
// hello, is closed, and the KioskInstaller is asked to run; health shows
// the version it saw and why.
func TestPluginVersionMismatch(t *testing.T) {
	for _, hello := range []string{
		`{"t":"hello","link":2,"role":"plugin","plugin":"1.0.0-bbbbbbbbbbbb"}`,
		`{"t":"hello","link":3,"role":"plugin","plugin":"1.0.0-aaaaaaaaaaaa"}`,
	} {
		e := newLinkEnv(t)
		p := e.dial(e.pluginUID)
		p.send(hello)
		if got := p.next(); got != `{"t":"hello","link":2,"firmware":"fw-111111111111","plugin":"1.0.0-aaaaaaaaaaaa"}` {
			t.Fatalf("%s: hello %s", hello, got)
		}
		if !p.closed() {
			t.Fatalf("%s: mismatched plugin kept its connection", hello)
		}
		var in inMsg
		_ = json.Unmarshal([]byte(hello), &in)
		st := e.health.Stats().Kiosk.Plugin
		if *st.Linked || st.Version == nil || *st.Version != in.Plugin || st.Error == nil || !strings.Contains(*st.Error, "reinstalling") {
			t.Fatalf("%s: plugin health %+v", hello, st)
		}
		e.mu.Lock()
		if e.pluginMismatch != 1 || len(e.pluginLinked) != 0 || e.mismatch != 0 {
			t.Fatalf("%s: plugin mismatch %d linked %v app mismatch %d", hello, e.pluginMismatch, e.pluginLinked, e.mismatch)
		}
		e.mu.Unlock()
	}
}
