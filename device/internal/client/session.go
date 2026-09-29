package client

import (
	"context"
	"encoding/json"
	"errors"
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/monoclock"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/uuid"
)

var (
	// ErrClosed: the session is lost; nothing more is sent on it.
	ErrClosed = errors.New("client: session closed")
	// ErrBackpressure: a bounded send queue is full; the caller retries or
	// drops according to its own rules.
	ErrBackpressure = errors.New("client: send queue full")
	// ErrControlTooLarge: the envelope exceeds the 256 KiB control frame cap.
	ErrControlTooLarge = errors.New("client: control frame exceeds 256 KiB")
)

// controlQueueDepth bounds queued control envelopes. Control has its own
// socket and writer, so it never waits behind audio (§4.4).
const controlQueueDepth = 256

type controlWrite struct {
	data []byte
	done chan error // non-nil: the caller waits for the write
}

// Session is one established v1 session, valid from Handler.Ready until
// Handler.Lost. Its methods are safe for concurrent use.
type Session struct {
	link *Link
	id   string

	ctx    context.Context
	cancel context.CancelFunc
	ctrl   *websocket.Conn
	audio  *websocket.Conn
	sink   *socketAudioSink
	assets *assetTransport

	controlQ chan controlWrite
	lostWhy  chan LostReason // first failure reason; buffered 1
	closed   atomic.Bool
	degraded atomic.Bool
	lastRx   atomic.Int64 // CLOCK_MONOTONIC ns of the last control message
	wg       sync.WaitGroup
}

func newSession(parent context.Context, l *Link, id string, ctrl, audio *websocket.Conn, at *assetTransport) *Session {
	ctx, cancel := context.WithCancel(parent)
	s := &Session{
		link:     l,
		id:       id,
		ctx:      ctx,
		cancel:   cancel,
		ctrl:     ctrl,
		audio:    audio,
		assets:   at,
		controlQ: make(chan controlWrite, controlQueueDepth),
		lostWhy:  make(chan LostReason, 1),
	}
	s.sink = newSocketAudioSink(audio, l.t.write, s.fail)
	s.lastRx.Store(monoclock.Now())
	return s
}

// ID is the controller-assigned session ID.
func (s *Session) ID() string { return s.id }

// Audio is the uplink frame sink on the audio socket.
func (s *Session) Audio() AudioSink { return s.sink }

// Assets fetches files over the assets socket, one request in flight.
func (s *Session) Assets() assets.Transport { return s.assets }

// Degraded reports 2 s without any control message (§16.1).
func (s *Session) Degraded() bool { return s.degraded.Load() }

// Send queues one control envelope without blocking and returns its fresh
// UUIDv4 message ID (the command ID).
func (s *Session) Send(typ proto.MessageType, generation uint32, body any) (string, error) {
	if s.closed.Load() {
		return "", ErrClosed
	}
	id := uuid.NewV4().String()
	data, err := marshalEnvelope(s.link.cfg.DeviceID, &s.id, typ, id, generation, body)
	if err != nil {
		return "", err
	}
	if len(data) > proto.MaxControlBytes {
		return "", ErrControlTooLarge
	}
	select {
	case s.controlQ <- controlWrite{data: data}:
		return id, nil
	default:
		return "", ErrBackpressure
	}
}

// Ack sends command.ack for a C→D message (WIRE §4.1).
func (s *Session) Ack(messageID string, status proto.AckStatus, errCode *proto.AckCode) error {
	_, err := s.Send(proto.TypeCommandAck, 0, proto.CommandAck{MessageID: messageID, Status: status, Error: errCode})
	return err
}

// run starts the session's goroutines, calls ready, reads control until the
// session ends (or the link's context does), and returns why it ended.
func (s *Session) run(ready func()) LostReason {
	stop := context.AfterFunc(s.ctx, func() { s.fail(LostClosed) })
	defer stop()
	s.wg.Add(5)
	go s.writeControl()
	go s.heartbeats()
	go s.liveness()
	go s.readAudio()
	go func() {
		defer s.wg.Done()
		s.sink.run(s.ctx)
	}()
	ready()
	reason := s.readControl()
	s.close()
	s.wg.Wait()
	return reason
}

func (s *Session) readControl() LostReason {
	for {
		kind, raw, err := s.ctrl.ReadMessage()
		if err != nil {
			select {
			case reason := <-s.lostWhy:
				return reason
			default:
				return LostClosed
			}
		}
		s.lastRx.Store(monoclock.Now())
		s.degraded.Store(false)
		if kind != websocket.TextMessage {
			s.protocolError(proto.ErrMalformedMessage, "control frames are text")
			return LostProtocol
		}
		env, err := decodeEnvelope(raw, s.link.cfg.DeviceID)
		if err == nil && (env.SessionID == nil || *env.SessionID != s.id) {
			err = errors.New("envelope for another session")
		}
		if err != nil {
			s.protocolError(proto.ErrMalformedMessage, err.Error())
			return LostProtocol
		}
		switch env.Type {
		case proto.TypeHeartbeat:
		case proto.TypeProtocolError:
			var pe proto.ProtocolError
			_ = json.Unmarshal(env.Body, &pe)
			log.Printf("[client] controller protocol.error %s: %s", pe.Code, pe.Detail)
		case proto.TypePing:
			s.pong(env.Body)
		case proto.TypeShellOpen:
			var b struct {
				Pty bool `json:"pty"`
			}
			_ = json.Unmarshal(env.Body, &b)
			s.link.shell.open(b.Pty)
		case proto.TypeShellClose:
			s.link.shell.close()
		default:
			s.h().Control(env)
		}
	}
}

func (s *Session) h() Handler { return s.link.h }

// pingPong is the retained ping body and its pong echo. ID is echoed
// verbatim, whatever JSON the controller chose; an absent id stays absent.
type pingPong struct {
	ID json.RawMessage `json:"id,omitempty"`
}

// pong echoes a ping's id so the controller can pair it with its send; RTT is
// measured on the controller's clock alone.
func (s *Session) pong(body json.RawMessage) {
	var ping pingPong
	_ = json.Unmarshal(body, &ping)
	_, _ = s.Send(proto.TypePong, 0, pingPong{ID: ping.ID})
}

func (s *Session) writeControl() {
	defer s.wg.Done()
	for {
		select {
		case <-s.ctx.Done():
			return
		case w := <-s.controlQ:
			s.ctrl.SetWriteDeadline(time.Now().Add(s.link.t.write))
			err := s.ctrl.WriteMessage(websocket.TextMessage, w.data)
			if w.done != nil {
				w.done <- err
			}
			if err != nil {
				s.fail(LostClosed)
				return
			}
		}
	}
}

func (s *Session) heartbeats() {
	defer s.wg.Done()
	t := time.NewTicker(s.link.t.heartbeat)
	defer t.Stop()
	for {
		select {
		case <-s.ctx.Done():
			return
		case <-t.C:
			if _, err := s.Send(proto.TypeHeartbeat, 0, proto.Heartbeat{MonoNs: monoclock.Now()}); err != nil {
				s.fail(LostClosed)
				return
			}
		}
	}
}

// liveness counts any received control message as liveness: 2 s of silence
// degrades the link, 3 s loses the session (§16.1).
func (s *Session) liveness() {
	defer s.wg.Done()
	t := time.NewTicker(max(s.link.t.heartbeat/10, time.Millisecond))
	defer t.Stop()
	for {
		select {
		case <-s.ctx.Done():
			return
		case <-t.C:
			silence := time.Duration(monoclock.Now() - s.lastRx.Load())
			s.degraded.Store(silence >= s.link.t.degraded)
			if silence >= s.link.t.lost {
				s.fail(LostTimeout)
				return
			}
		}
	}
}

// readAudio accepts only valid kind-3 downlink frames (WIRE §3); anything
// else is a protocol error that ends the session.
func (s *Session) readAudio() {
	defer s.wg.Done()
	for {
		kind, msg, err := s.audio.ReadMessage()
		if err != nil {
			s.fail(LostClosed)
			return
		}
		if kind != websocket.BinaryMessage {
			s.protocolError(proto.ErrMalformedFrame, "audio frames are binary")
			s.fail(LostProtocol)
			return
		}
		h, pcm, err := ema.DecodeFrame(msg)
		if err == nil && h.Kind != ema.KindRender {
			err = errors.New("downlink frames are kind 3")
		}
		if err != nil {
			s.protocolError(proto.ErrMalformedFrame, err.Error())
			s.fail(LostProtocol)
			return
		}
		s.h().RenderAudio(h, pcm)
	}
}

// protocolError sends protocol.error and waits until it is written, so it
// precedes the close that follows.
func (s *Session) protocolError(code proto.ErrorCode, detail string) {
	data, err := marshalEnvelope(s.link.cfg.DeviceID, &s.id, proto.TypeProtocolError, uuid.NewV4().String(), 0,
		proto.ProtocolError{Code: code, Detail: detail})
	if err != nil {
		return
	}
	done := make(chan error, 1)
	select {
	case s.controlQ <- controlWrite{data: data, done: done}:
	case <-s.ctx.Done():
		return
	}
	select {
	case <-done:
	case <-s.ctx.Done():
	}
}

// fail records the first loss reason and closes the control socket, which
// ends readControl.
func (s *Session) fail(reason LostReason) {
	select {
	case s.lostWhy <- reason:
	default:
	}
	s.ctrl.Close()
}

func (s *Session) close() {
	s.closed.Store(true)
	s.cancel()
	s.ctrl.Close()
	s.audio.Close()
	s.assets.close()
}
