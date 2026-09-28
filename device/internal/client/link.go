// Package client is the device end of the protocol v1 link (WIRE §1–§3,
// §6; SPEC §16.1): controller discovery, the pinned-CA dial, one session's
// control, audio, and assets sockets, heartbeats and liveness, and the
// retained /shell plane.
package client

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"strconv"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/discovery"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

// Session loss reasons passed to Handler.Lost.
const (
	LostClosed   = "closed"       // a socket closed or failed
	LostTimeout  = "session_lost" // 3 s without a control message (§16.1)
	LostProtocol = "protocol"     // a protocol.error ended the session
)

// Handler receives the session lifecycle and every C→D message the link does
// not consume itself (heartbeat, ping, protocol.error, shell_open,
// shell_close). Hello, Rejected, Ready, Control and Lost run on the link's
// control goroutine in order; RenderAudio runs on the audio receive goroutine.
type Handler interface {
	Hello() proto.SessionHello              // fresh snapshot per attempt
	Ready(s *Session, r proto.SessionReady) // session established
	Rejected(reason string)                 // session.rejected; the link retries after 10 s
	Lost(reason string)                     // all sockets closed; s is dead
	Control(env proto.Envelope)             // every other C→D message
	RenderAudio(h ema.Header, pcm []byte)   // validated kind-3 frame; pcm borrowed for the call
}

// Config identifies the endpoint and where its credentials live.
type Config struct {
	DeviceID  string
	CAPath    string // DefaultCAPath when empty
	TokenPath string // DefaultTokenPath when empty
	// Discover finds the controller; nil uses the last server that held a
	// session when it still accepts TCP, else mDNS.
	Discover func(ctx context.Context) (*discovery.ServerInfo, error)
}

// timings are the WIRE §1/§2 durations, overridable in tests.
type timings struct {
	hello     time.Duration // wait for session.ready or session.rejected
	rejected  time.Duration // retry after session.rejected
	reconnect time.Duration // retry after any other failure
	heartbeat time.Duration // heartbeat period
	degraded  time.Duration // silence before the link is degraded
	lost      time.Duration // silence before the session is lost
	write     time.Duration // per-frame write deadline
}

var defaultTimings = timings{
	hello:     10 * time.Second,
	rejected:  10 * time.Second,
	reconnect: time.Second,
	heartbeat: time.Second,
	degraded:  2 * time.Second,
	lost:      3 * time.Second,
	write:     3 * time.Second,
}

// Link owns discovery, reconnection and at most one live Session.
type Link struct {
	cfg  Config
	h    Handler
	t    timings
	last *discovery.ServerInfo // server of the last established session

	shell shellState
}

// New returns a link; Run connects it.
func New(cfg Config, h Handler) *Link {
	if cfg.CAPath == "" {
		cfg.CAPath = DefaultCAPath
	}
	if cfg.TokenPath == "" {
		cfg.TokenPath = DefaultTokenPath
	}
	return &Link{cfg: cfg, h: h, t: defaultTimings}
}

var errRejected = errors.New("client: session rejected")

// Run discovers the controller, holds a session, and reconnects from
// session.hello after every loss until ctx ends.
func (l *Link) Run(ctx context.Context) {
	l.shell.ctx = ctx
	defer l.shell.close()
	for ctx.Err() == nil {
		delay := l.t.reconnect
		server, err := l.find(ctx)
		if err == nil {
			err = l.connect(ctx, server)
			if errors.Is(err, errRejected) {
				delay = l.t.rejected
			}
		}
		if ctx.Err() != nil {
			return
		}
		log.Printf("[client] %v; retrying in %s", err, delay)
		select {
		case <-ctx.Done():
			return
		case <-time.After(delay):
		}
	}
}

// find prefers the last server when it still accepts TCP: after a Wi-Fi
// change the controller may sit on a subnet mDNS does not reach. A cached
// server without a TLS port is refreshed once when a CA is installed.
func (l *Link) find(ctx context.Context) (*discovery.ServerInfo, error) {
	if l.cfg.Discover != nil {
		return l.cfg.Discover(ctx)
	}
	server := l.last
	if server != nil && server.TLSPort == 0 && loadLinkCreds(l.cfg.CAPath, l.cfg.TokenPath).tlsConf != nil {
		if found, err := discovery.FindServerOnce(ctx); err == nil && found != nil {
			server = found
		}
	}
	if server != nil && probeTCP(server.Addr, 3*time.Second) {
		return server, nil
	}
	return discovery.FindServer(ctx)
}

// connect runs one session attempt to completion.
func (l *Link) connect(ctx context.Context, server *discovery.ServerInfo) error {
	creds := loadLinkCreds(l.cfg.CAPath, l.cfg.TokenPath)
	base := "ws://" + server.Addr
	if creds.tlsConf != nil {
		if server.TLSPort > 0 {
			base = "wss://" + net.JoinHostPort(server.Host, strconv.Itoa(server.TLSPort))
		} else {
			log.Printf("[client] CA installed but the controller offers no tls_port; dialling plain ws")
		}
	}
	dialer := creds.dialer()

	ctrl, _, err := dialer.DialContext(ctx, base+proto.PathControl, creds.header())
	if err != nil {
		return fmt.Errorf("client: dial control: %w", err)
	}
	ctrl.SetReadLimit(proto.MaxControlBytes)
	stop := context.AfterFunc(ctx, func() { ctrl.Close() })
	ready, err := l.handshake(ctrl)
	if !stop() || err != nil {
		ctrl.Close()
		if err == nil {
			err = ctx.Err()
		}
		return err
	}

	sessionHeader := creds.header()
	sessionHeader.Set(proto.HeaderSession, ready.SessionID)
	audio, _, err := dialer.DialContext(ctx, base+proto.PathAudio, sessionHeader)
	if err != nil {
		ctrl.Close()
		return fmt.Errorf("client: dial audio: %w", err)
	}
	dialAssets := func(ctx context.Context) (*websocket.Conn, error) {
		conn, _, err := dialer.DialContext(ctx, base+proto.PathAssets, sessionHeader)
		return conn, err
	}
	assetsConn, err := dialAssets(ctx)
	if err != nil {
		ctrl.Close()
		audio.Close()
		return fmt.Errorf("client: dial assets: %w", err)
	}
	l.last = server
	l.shell.mu.Lock()
	l.shell.endpoint = shellEndpoint{base: base, deviceID: l.cfg.DeviceID, dialer: dialer, header: creds.header()}
	l.shell.mu.Unlock()

	s := newSession(ctx, l, ready.SessionID, ctrl, audio, newAssetTransport(assetsConn, dialAssets))
	reason := s.run(func() { l.h.Ready(s, ready) })
	l.h.Lost(reason)
	return fmt.Errorf("client: session %s lost: %s", ready.SessionID, reason)
}

// handshake sends session.hello and waits for session.ready or
// session.rejected (WIRE §1, §4.1).
func (l *Link) handshake(ctrl *websocket.Conn) (proto.SessionReady, error) {
	var ready proto.SessionReady
	hello, err := marshalEnvelope(l.cfg.DeviceID, nil, proto.TypeSessionHello, newUUID(), 0, l.h.Hello())
	if err != nil {
		return ready, err
	}
	deadline := time.Now().Add(l.t.hello)
	ctrl.SetWriteDeadline(deadline)
	if err := ctrl.WriteMessage(websocket.TextMessage, hello); err != nil {
		return ready, fmt.Errorf("client: send session.hello: %w", err)
	}
	ctrl.SetReadDeadline(deadline)
	kind, raw, err := ctrl.ReadMessage()
	if err != nil {
		return ready, fmt.Errorf("client: await session.ready: %w", err)
	}
	ctrl.SetReadDeadline(time.Time{})
	if kind != websocket.TextMessage {
		return ready, errors.New("client: binary frame during handshake")
	}
	env, err := decodeEnvelope(raw, l.cfg.DeviceID)
	if err != nil {
		return ready, err
	}
	switch env.Type {
	case proto.TypeSessionRejected:
		var rej proto.SessionRejected
		if err := json.Unmarshal(env.Body, &rej); err != nil {
			return ready, fmt.Errorf("client: session.rejected body: %w", err)
		}
		l.h.Rejected(rej.Reason)
		return ready, fmt.Errorf("%w: %s", errRejected, rej.Reason)
	case proto.TypeSessionReady:
		if err := json.Unmarshal(env.Body, &ready); err != nil {
			return ready, fmt.Errorf("client: session.ready body: %w", err)
		}
		if ready.Protocol != proto.Version || ready.SessionID == "" ||
			(env.SessionID != nil && *env.SessionID != ready.SessionID) {
			return ready, errors.New("client: session.ready names no valid v1 session")
		}
		return ready, nil
	default:
		return ready, fmt.Errorf("client: expected session.ready, got %q", env.Type)
	}
}

// decodeEnvelope parses a C→D envelope and checks the fields every message
// must carry (WIRE §2).
func decodeEnvelope(raw []byte, deviceID string) (proto.Envelope, error) {
	var env proto.Envelope
	if err := json.Unmarshal(raw, &env); err != nil {
		return env, fmt.Errorf("client: malformed envelope: %w", err)
	}
	switch {
	case env.Protocol != proto.Version:
		return env, fmt.Errorf("client: envelope protocol %d", env.Protocol)
	case env.DeviceID != deviceID:
		return env, fmt.Errorf("client: envelope for device %q", env.DeviceID)
	case env.Type == "" || env.MessageID == "":
		return env, errors.New("client: envelope without type or message_id")
	}
	return env, nil
}

func marshalEnvelope(deviceID string, sessionID *string, typ, messageID string, generation uint32, body any) ([]byte, error) {
	b, err := json.Marshal(body)
	if err != nil {
		return nil, fmt.Errorf("client: marshal %s body: %w", typ, err)
	}
	return json.Marshal(proto.Envelope{
		Protocol:   proto.Version,
		Type:       typ,
		SessionID:  sessionID,
		MessageID:  messageID,
		DeviceID:   deviceID,
		Generation: generation,
		Body:       b,
	})
}

func probeTCP(addr string, timeout time.Duration) bool {
	conn, err := net.DialTimeout("tcp", addr, timeout)
	if err != nil {
		return false
	}
	conn.Close()
	return true
}

// shellState tracks the one /shell session. It outlives control sessions so
// a shell (and a transfer running over it) survives a reconnect.
type shellState struct {
	mu       sync.Mutex
	ctx      context.Context // Link.Run's context
	endpoint shellEndpoint   // of the latest session
	cancel   context.CancelFunc
	gen      uint64
}
