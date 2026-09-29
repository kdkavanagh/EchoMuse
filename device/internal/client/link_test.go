package client

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/discovery"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

const (
	testDevice  = "G0TEST"
	testSession = "11111111-2222-4333-8444-555555555555"
	testToken   = "secret-token"
)

var uuidV4 = regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`)

// recorder is a Handler that reports every callback on channels.
type recorder struct {
	hellos   chan time.Time
	ready    chan *Session
	rejected chan proto.RejectReason
	lost     chan LostReason
	control  chan proto.Envelope
	audio    chan ema.Header
}

func newRecorder() *recorder {
	return &recorder{
		hellos:   make(chan time.Time, 16),
		ready:    make(chan *Session, 4),
		rejected: make(chan proto.RejectReason, 4),
		lost:     make(chan LostReason, 4),
		control:  make(chan proto.Envelope, 16),
		audio:    make(chan ema.Header, 16),
	}
}

func (r *recorder) Hello() proto.SessionHello {
	r.hellos <- time.Now()
	return proto.SessionHello{Capabilities: []proto.Capability{proto.CapAudioTimeline}, Protocols: []int{proto.Version}}
}
func (r *recorder) Ready(s *Session, _ proto.SessionReady) { r.ready <- s }
func (r *recorder) Rejected(reason proto.RejectReason)     { r.rejected <- reason }
func (r *recorder) Lost(reason LostReason)                 { r.lost <- reason }
func (r *recorder) Control(env proto.Envelope)             { r.control <- env }
func (r *recorder) RenderAudio(h ema.Header, _ []byte)     { r.audio <- h }

// controller is an in-process fake of the controller's device listener.
type controller struct {
	t        *testing.T
	up       websocket.Upgrader
	onHello  func(c *websocket.Conn) // after session.hello; nil sends session.ready
	onAudio  func(c *websocket.Conn)
	onAssets func(c *websocket.Conn)

	mu      sync.Mutex
	headers map[string]http.Header // per path, the last upgrade's headers
	ctrl    chan *websocket.Conn   // control sockets after session.ready
	hello   chan proto.Envelope
}

func newController(t *testing.T) *controller {
	return &controller{t: t, headers: map[string]http.Header{}, ctrl: make(chan *websocket.Conn, 4), hello: make(chan proto.Envelope, 16)}
}

func (c *controller) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	c.mu.Lock()
	c.headers[r.URL.Path] = r.Header.Clone()
	c.mu.Unlock()
	conn, err := c.up.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	switch r.URL.Path {
	case proto.PathControl:
		_, raw, err := conn.ReadMessage()
		if err != nil {
			return
		}
		var env proto.Envelope
		json.Unmarshal(raw, &env)
		c.hello <- env
		if c.onHello != nil {
			c.onHello(conn)
			return
		}
		sendEnv(c.t, conn, proto.TypeSessionReady, proto.SessionReady{Protocol: 1, SessionID: testSession, UTCMs: 1})
		c.ctrl <- conn
	case proto.PathAudio:
		if c.onAudio != nil {
			c.onAudio(conn)
			return
		}
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				return
			}
		}
	case proto.PathAssets:
		if c.onAssets != nil {
			c.onAssets(conn)
		}
	}
}

func (c *controller) header(path string) http.Header {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.headers[path]
}

func sendEnv(t *testing.T, c *websocket.Conn, typ proto.MessageType, body any) {
	b, _ := json.Marshal(body)
	sid := testSession
	env := proto.Envelope{Protocol: 1, Type: typ, SessionID: &sid, MessageID: "c2d-1", DeviceID: testDevice, Body: b}
	if err := c.WriteJSON(env); err != nil {
		t.Logf("server write %s: %v", typ, err)
	}
}

// readEnv reads control envelopes until one of type typ arrives.
func readEnv(t *testing.T, c *websocket.Conn, typ proto.MessageType) proto.Envelope {
	t.Helper()
	c.SetReadDeadline(time.Now().Add(3 * time.Second))
	for {
		_, raw, err := c.ReadMessage()
		if err != nil {
			t.Fatalf("waiting for %s: %v", typ, err)
		}
		var env proto.Envelope
		if err := json.Unmarshal(raw, &env); err != nil {
			t.Fatal(err)
		}
		if env.Type == typ {
			return env
		}
	}
}

// start runs a Link against the fake controller with test timings.
func start(t *testing.T, c *controller, tune func(*timings)) *recorder {
	t.Helper()
	srv := httptest.NewServer(c)
	t.Cleanup(srv.Close)
	host, portStr, _ := net.SplitHostPort(srv.Listener.Addr().String())
	port, _ := strconv.Atoi(portStr)
	info := &discovery.ServerInfo{Host: host, Port: port, Addr: srv.Listener.Addr().String()}

	dir := t.TempDir()
	tokenPath := filepath.Join(dir, "token")
	if err := os.WriteFile(tokenPath, []byte(testToken+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	rec := newRecorder()
	l := New(Config{
		DeviceID:  testDevice,
		CAPath:    filepath.Join(dir, "ca.pem"),
		TokenPath: tokenPath,
		Discover:  func(context.Context) (*discovery.ServerInfo, error) { return info, nil },
	}, rec)
	l.t.reconnect = 20 * time.Millisecond
	if tune != nil {
		tune(&l.t)
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		l.Run(ctx)
		close(done)
	}()
	t.Cleanup(func() {
		cancel()
		<-done
	})
	return rec
}

func waitFor[T any](t *testing.T, ch <-chan T, what string) T {
	t.Helper()
	select {
	case v := <-ch:
		return v
	case <-time.After(3 * time.Second):
		t.Fatalf("timed out waiting for %s", what)
		panic("unreachable")
	}
}

func TestHandshakeOpensSessionSockets(t *testing.T) {
	c := newController(t)
	rec := start(t, c, nil)
	hello := waitFor(t, c.hello, "session.hello")
	if hello.Type != proto.TypeSessionHello || hello.SessionID != nil || hello.DeviceID != testDevice ||
		hello.Protocol != 1 || !uuidV4.MatchString(hello.MessageID) {
		t.Fatalf("hello envelope %+v", hello)
	}
	s := waitFor(t, rec.ready, "Ready")
	ctrl := waitFor(t, c.ctrl, "control socket")
	if s.ID() != testSession {
		t.Fatalf("session id %q", s.ID())
	}
	for _, p := range []string{proto.PathControl, proto.PathAudio, proto.PathAssets} {
		h := c.header(p)
		if h.Get(proto.HeaderToken) != testToken {
			t.Fatalf("%s upgrade token %q", p, h.Get(proto.HeaderToken))
		}
		if want := testSession; p != proto.PathControl && h.Get(proto.HeaderSession) != want {
			t.Fatalf("%s upgrade session %q", p, h.Get(proto.HeaderSession))
		}
		if h.Get("Sec-Websocket-Extensions") != "" {
			t.Fatalf("%s negotiated extensions %q", p, h.Get("Sec-Websocket-Extensions"))
		}
	}

	id1, err := s.Send(proto.TypeWakeStats, 0, map[string]int{"window_ms": 30000})
	must(t, err)
	id2, _ := s.Send(proto.TypeWakeStats, 0, map[string]int{"window_ms": 30000})
	env := readEnv(t, ctrl, proto.TypeWakeStats)
	if env.MessageID != id1 || id1 == id2 || !uuidV4.MatchString(id1) || env.SessionID == nil || *env.SessionID != testSession {
		t.Fatalf("sent envelope %+v (ids %s %s)", env, id1, id2)
	}
	if _, err := s.Send(proto.TypeLog, 0, map[string]string{"message": string(make([]byte, proto.MaxControlBytes))}); !errors.Is(err, ErrControlTooLarge) {
		t.Fatalf("oversized control frame: %v", err)
	}

	sendEnv(t, ctrl, proto.TypeUplinkOpen, proto.UplinkOpen{LeaseID: "L"})
	if got := waitFor(t, rec.control, "Control"); got.Type != proto.TypeUplinkOpen {
		t.Fatalf("control %+v", got)
	}
	sendEnv(t, ctrl, proto.TypePing, map[string]int{"id": 7})
	if pong := readEnv(t, ctrl, proto.TypePong); string(pong.Body) != `{"id":7}` {
		t.Fatalf("pong %s", pong.Body)
	}
	sendEnv(t, ctrl, proto.TypePing, struct{}{})
	if pong := readEnv(t, ctrl, proto.TypePong); string(pong.Body) != `{}` {
		t.Fatalf("pong without id %s", pong.Body)
	}

	ctrl.Close()
	if reason := waitFor(t, rec.lost, "Lost"); reason != LostClosed {
		t.Fatalf("lost %q", reason)
	}
	if _, err := s.Send(proto.TypeLog, 0, nil); !errors.Is(err, ErrClosed) {
		t.Fatalf("send after loss: %v", err)
	}
	if err := s.Audio().SendFrame([]byte{1}, 0); !errors.Is(err, ErrClosed) {
		t.Fatalf("audio after loss: %v", err)
	}
	// The link reconnects from session.hello.
	waitFor(t, c.hello, "second session.hello")
}

func TestRejectedRetriesAfterDelay(t *testing.T) {
	c := newController(t)
	c.onHello = func(conn *websocket.Conn) {
		sendEnv(t, conn, proto.TypeSessionRejected, proto.SessionRejected{Reason: proto.RejectPendingApproval})
		conn.Close()
	}
	rec := start(t, c, func(tm *timings) { tm.rejected = 300 * time.Millisecond })
	first := waitFor(t, rec.hellos, "hello")
	if reason := waitFor(t, rec.rejected, "Rejected"); reason != proto.RejectPendingApproval {
		t.Fatalf("rejected %q", reason)
	}
	second := waitFor(t, rec.hellos, "retried hello")
	if gap := second.Sub(first); gap < 300*time.Millisecond {
		t.Fatalf("retried after %s", gap)
	}
	select {
	case <-rec.ready:
		t.Fatal("Ready after rejection")
	case <-rec.lost:
		t.Fatal("Lost without a session")
	default:
	}
}

func TestHelloTimeoutRetries(t *testing.T) {
	c := newController(t)
	c.onHello = func(conn *websocket.Conn) {
		time.Sleep(time.Second)
		conn.Close()
	}
	rec := start(t, c, func(tm *timings) { tm.hello = 150 * time.Millisecond })
	first := waitFor(t, rec.hellos, "hello")
	second := waitFor(t, rec.hellos, "retried hello")
	if gap := second.Sub(first); gap < 150*time.Millisecond || gap > 900*time.Millisecond {
		t.Fatalf("retried after %s", gap)
	}
	select {
	case <-rec.ready:
		t.Fatal("Ready without session.ready")
	default:
	}
}

func TestSilenceDegradesThenLosesSession(t *testing.T) {
	c := newController(t)
	rec := start(t, c, func(tm *timings) {
		tm.heartbeat = 20 * time.Millisecond
		tm.degraded = 200 * time.Millisecond
		tm.lost = 300 * time.Millisecond
	})
	s := waitFor(t, rec.ready, "Ready")
	ctrl := waitFor(t, c.ctrl, "control socket")
	readEnv(t, ctrl, proto.TypeHeartbeat) // the device heartbeats on its own

	// Any control message counts as liveness.
	var quiet time.Time
	for range 10 {
		sendEnv(t, ctrl, proto.TypeHeartbeat, proto.Heartbeat{MonoNs: 1})
		quiet = time.Now()
		time.Sleep(50 * time.Millisecond)
	}
	if s.Degraded() {
		t.Fatal("degraded while the controller heartbeats")
	}
	time.Sleep(time.Until(quiet.Add(250 * time.Millisecond)))
	if !s.Degraded() {
		t.Fatal("not degraded after 250 ms of silence")
	}
	if reason := waitFor(t, rec.lost, "Lost"); reason != LostTimeout {
		t.Fatalf("lost %q", reason)
	}
	if since := time.Since(quiet); since < 300*time.Millisecond {
		t.Fatalf("lost after %s of silence", since)
	}
}

func TestInvalidDownlinkAudioIsAProtocolError(t *testing.T) {
	c := newController(t)
	audio := make(chan *websocket.Conn, 1)
	c.onAudio = func(conn *websocket.Conn) {
		audio <- conn
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				return
			}
		}
	}
	rec := start(t, c, nil)
	waitFor(t, rec.ready, "Ready")
	ctrl := waitFor(t, c.ctrl, "control socket")
	a := waitFor(t, audio, "audio socket")

	frame := func(k ema.Kind, frames uint32) []byte {
		h := ema.NewHeader(k, 0, 9, 0, 0, frames)
		if k == ema.KindRender {
			h.Generation = 3
		}
		buf := make([]byte, ema.HeaderSize+int(h.PayloadBytes))
		must(t, h.Encode(buf))
		return buf
	}
	must(t, a.WriteMessage(websocket.BinaryMessage, frame(ema.KindRender, 480)))
	if h := waitFor(t, rec.audio, "RenderAudio"); h.Kind != ema.KindRender || h.Generation != 3 || h.FrameCount != 480 {
		t.Fatalf("render frame %+v", h)
	}

	must(t, a.WriteMessage(websocket.BinaryMessage, frame(ema.KindMic, 1280)))
	pe := readEnv(t, ctrl, proto.TypeProtocolError)
	var body proto.ProtocolError
	must(t, json.Unmarshal(pe.Body, &body))
	if body.Code != "malformed_frame" {
		t.Fatalf("protocol.error %+v", body)
	}
	if reason := waitFor(t, rec.lost, "Lost"); reason != LostProtocol {
		t.Fatalf("lost %q", reason)
	}
	select {
	case h := <-rec.audio:
		t.Fatalf("invalid frame delivered: %+v", h)
	default:
	}
}

// assetServer serves one blob over the assets socket (WIRE §6). dropAfter > 0
// closes the socket after that many bytes of the first reply.
type assetServer struct {
	blob      []byte
	sha       string
	dropAfter int

	mu       sync.Mutex
	requests []assetRequest
}

func (a *assetServer) serve(conn *websocket.Conn) {
	defer conn.Close()
	for {
		var req assetRequest
		if err := conn.ReadJSON(&req); err != nil {
			return
		}
		a.mu.Lock()
		a.requests = append(a.requests, req)
		first := len(a.requests) == 1
		a.mu.Unlock()
		if req.SHA256 != a.sha {
			conn.WriteJSON(map[string]string{"error": "not_found"})
			continue
		}
		rest := a.blob[req.Offset:]
		if first && a.dropAfter > 0 {
			conn.WriteMessage(websocket.BinaryMessage, rest[:a.dropAfter])
			return
		}
		for len(rest) > 0 {
			n := min(len(rest), 7)
			conn.WriteMessage(websocket.BinaryMessage, rest[:n])
			rest = rest[n:]
		}
		conn.WriteJSON(map[string]any{"sha256": a.sha, "size": len(a.blob), "done": true})
	}
}

func TestAssetsResumeAfterDropAndReportNotFound(t *testing.T) {
	blob := []byte("the reference runtime bytes, resumed by offset after a dropped socket")
	sum := sha256.Sum256(blob)
	srv := &assetServer{blob: blob, sha: hex.EncodeToString(sum[:]), dropAfter: 20}
	c := newController(t)
	c.onAssets = srv.serve
	rec := start(t, c, nil)
	s := waitFor(t, rec.ready, "Ready")

	store, err := assets.Open(t.TempDir())
	must(t, err)
	ctx := context.Background()
	if _, err := store.Ensure(ctx, s.Assets(), srv.sha, "so"); err == nil {
		t.Fatal("download survived a dropped socket")
	}
	path, err := store.Ensure(ctx, s.Assets(), srv.sha, "so")
	must(t, err)
	got, err := os.ReadFile(path)
	must(t, err)
	if string(got) != string(blob) {
		t.Fatalf("installed %q", got)
	}
	srv.mu.Lock()
	reqs := append([]assetRequest(nil), srv.requests...)
	srv.mu.Unlock()
	if len(reqs) != 2 || reqs[0].Offset != 0 || reqs[1].Offset != 20 {
		t.Fatalf("requests %+v", reqs)
	}

	missing := hex.EncodeToString(make([]byte, 32))
	if _, err := s.Assets().Fetch(ctx, missing, 0, nil); !errors.Is(err, assets.ErrNotFound) {
		t.Fatalf("missing asset: %v", err)
	}
}

func TestAssetFetchesQueueBehindOneAnother(t *testing.T) {
	blob := []byte("0123456789abcdef")
	sum := sha256.Sum256(blob)
	srv := &assetServer{blob: blob, sha: hex.EncodeToString(sum[:])}
	c := newController(t)
	c.onAssets = srv.serve
	rec := start(t, c, nil)
	s := waitFor(t, rec.ready, "Ready")

	var wg sync.WaitGroup
	errs := make(chan error, 8)
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			var buf sinkWriter
			size, err := s.Assets().Fetch(context.Background(), srv.sha, 4, &buf)
			if err == nil && (size != int64(len(blob)) || string(buf) != string(blob[4:])) {
				err = errors.New("wrong bytes: " + string(buf))
			}
			errs <- err
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		must(t, err)
	}
}

type sinkWriter []byte

func (w *sinkWriter) Write(p []byte) (int, error) {
	*w = append(*w, p...)
	return len(p), nil
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}
