package client

import (
	"context"
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

// AudioSink carries uplink EMA1 frames on the audio socket. SendFrame copies
// frame and never blocks: it returns ErrBackpressure while the queue is full
// and ErrClosed once the session is lost. enqueuedNs is when the frame's
// first sample entered the device's send queue (CLOCK_MONOTONIC).
type AudioSink interface {
	SendFrame(frame []byte, enqueuedNs int64) error
}

// audioQueueDepth bounds frames queued on the socket. The uplink executor
// packs live audio into at most 80 ms per packet, so the socket queue holds
// at most 400 ms of any stream's live audio (§4.4); the rest waits in the
// rings, where the executor measures its age.
const audioQueueDepth = 5

type socketAudioSink struct {
	conn  *websocket.Conn
	write time.Duration
	fail  func(reason string)

	mu     sync.Mutex
	closed bool
	q      chan []byte
}

func newSocketAudioSink(conn *websocket.Conn, write time.Duration, fail func(string)) *socketAudioSink {
	return &socketAudioSink{conn: conn, write: write, fail: fail, q: make(chan []byte, audioQueueDepth)}
}

func (s *socketAudioSink) SendFrame(frame []byte, _ int64) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return ErrClosed
	}
	if len(s.q) == cap(s.q) {
		return ErrBackpressure
	}
	s.q <- append([]byte(nil), frame...)
	return nil
}

// run writes queued frames until ctx ends; queued audio is then discarded,
// never carried into another session (WIRE §5).
func (s *socketAudioSink) run(ctx context.Context) {
	defer func() {
		s.mu.Lock()
		s.closed = true
		s.mu.Unlock()
	}()
	for {
		select {
		case <-ctx.Done():
			return
		case f := <-s.q:
			s.conn.SetWriteDeadline(time.Now().Add(s.write))
			if err := s.conn.WriteMessage(websocket.BinaryMessage, f); err != nil {
				s.fail(LostClosed)
				return
			}
		}
	}
}
