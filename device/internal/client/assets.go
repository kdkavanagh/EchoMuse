package client

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	"github.com/wilbowes/EchoMuse/internal/assets"
)

// assetIdle bounds the wait for the next chunk or the closing reply.
const assetIdle = 30 * time.Second

// assetTransport is assets.Transport over /device/v1/assets (WIRE §6). Fetch
// calls queue behind one another so exactly one request is in flight. A
// failed socket is dropped and redialled on the next Fetch; the store resumes
// its .part file by offset.
type assetTransport struct {
	turn   chan struct{} // holds one token: the right to use the socket
	closed chan struct{}
	once   sync.Once
	redial func(context.Context) (*websocket.Conn, error)

	mu   sync.Mutex
	conn *websocket.Conn // nil after a failure until redialled
}

var _ assets.Transport = (*assetTransport)(nil)

type assetRequest struct {
	SHA256 string `json:"sha256"`
	Offset int64  `json:"offset"`
}

type assetReply struct {
	SHA256 string `json:"sha256"`
	Size   int64  `json:"size"`
	Done   bool   `json:"done"`
	Error  string `json:"error"`
}

func newAssetTransport(conn *websocket.Conn, redial func(context.Context) (*websocket.Conn, error)) *assetTransport {
	t := &assetTransport{turn: make(chan struct{}, 1), closed: make(chan struct{}), redial: redial, conn: conn}
	t.turn <- struct{}{}
	return t
}

// Fetch requests sha from offset, writes each binary chunk to w, and returns
// the asset size from the closing reply, or assets.ErrNotFound.
func (t *assetTransport) Fetch(ctx context.Context, sha string, offset int64, w io.Writer) (int64, error) {
	select {
	case <-ctx.Done():
		return 0, ctx.Err()
	case <-t.closed:
		return 0, ErrClosed
	case <-t.turn:
	}
	defer func() { t.turn <- struct{}{} }()

	conn, err := t.connection(ctx)
	if err != nil {
		return 0, err
	}
	size, err := t.exchange(ctx, conn, sha, offset, w)
	if err != nil && !errors.Is(err, assets.ErrNotFound) {
		t.drop(conn)
		if ctx.Err() != nil {
			return 0, ctx.Err()
		}
	}
	return size, err
}

func (t *assetTransport) exchange(ctx context.Context, conn *websocket.Conn, sha string, offset int64, w io.Writer) (int64, error) {
	// A cancelled fetch cannot be abandoned mid-reply on a shared socket:
	// cancellation closes it and the next Fetch redials.
	stop := context.AfterFunc(ctx, func() { conn.Close() })
	defer stop()

	conn.SetWriteDeadline(time.Now().Add(assetIdle))
	if err := conn.WriteJSON(assetRequest{SHA256: sha, Offset: offset}); err != nil {
		return 0, fmt.Errorf("client: asset request: %w", err)
	}
	for {
		conn.SetReadDeadline(time.Now().Add(assetIdle))
		kind, data, err := conn.ReadMessage()
		if err != nil {
			return 0, fmt.Errorf("client: asset reply: %w", err)
		}
		if kind == websocket.BinaryMessage {
			if _, err := w.Write(data); err != nil {
				return 0, err
			}
			continue
		}
		var r assetReply
		if err := json.Unmarshal(data, &r); err != nil {
			return 0, fmt.Errorf("client: asset reply: %w", err)
		}
		switch {
		case r.Error == "not_found":
			return 0, assets.ErrNotFound
		case r.Error != "":
			return 0, fmt.Errorf("client: asset %s: %s", sha, r.Error)
		case !r.Done || r.SHA256 != sha:
			return 0, fmt.Errorf("client: unexpected asset reply %s", data)
		}
		return r.Size, nil
	}
}

// connection returns the socket, redialling after a failure. Only the
// holder of the turn token calls it, so dials never race; the dial runs
// outside mu so close never waits for it.
func (t *assetTransport) connection(ctx context.Context) (*websocket.Conn, error) {
	t.mu.Lock()
	conn := t.conn
	t.mu.Unlock()
	select {
	case <-t.closed:
		return nil, ErrClosed
	default:
	}
	if conn != nil {
		return conn, nil
	}
	conn, err := t.redial(ctx)
	if err != nil {
		return nil, fmt.Errorf("client: dial assets: %w", err)
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	select {
	case <-t.closed:
		conn.Close()
		return nil, ErrClosed
	default:
	}
	t.conn = conn
	return conn, nil
}

func (t *assetTransport) drop(conn *websocket.Conn) {
	t.mu.Lock()
	defer t.mu.Unlock()
	conn.Close()
	if t.conn == conn {
		t.conn = nil
	}
}

func (t *assetTransport) close() {
	t.once.Do(func() {
		close(t.closed)
		t.mu.Lock()
		defer t.mu.Unlock()
		if t.conn != nil {
			t.conn.Close()
			t.conn = nil
		}
	})
}
