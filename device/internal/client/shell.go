package client

import (
	"context"
	"encoding/binary"
	"io"
	"log"
	"net/http"
	"os"
	"os/exec"
	"syscall"

	"github.com/gorilla/websocket"
)

// Shell input frame types (PTY sessions only); pipe sessions carry raw bytes.
const (
	shellFrameStdin  = 0x00 // payload: stdin bytes
	shellFrameResize = 0x01 // payload: cols uint16 BE, rows uint16 BE
)

// shellEndpoint is where /shell/{device_id} is dialled: the latest session's
// base URL, dialer and token header.
type shellEndpoint struct {
	base     string
	deviceID string
	dialer   *websocket.Dialer
	header   http.Header
}

// open replaces any running shell with a new one (shell_open).
func (st *shellState) open(pty bool) {
	st.mu.Lock()
	if st.cancel != nil {
		st.cancel()
	}
	ctx, cancel := context.WithCancel(st.ctx)
	st.gen++
	gen, ep := st.gen, st.endpoint
	st.cancel = cancel
	st.mu.Unlock()

	go func() {
		defer func() {
			st.mu.Lock()
			if st.gen == gen {
				st.cancel = nil
			}
			st.mu.Unlock()
			cancel()
		}()
		runShell(ctx, ep, pty)
	}()
}

// close ends the running shell, if any (shell_close).
func (st *shellState) close() {
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.cancel != nil {
		st.cancel()
		st.cancel = nil
	}
}

// runShell dials the controller's shell endpoint and pipes /system/bin/sh
// through it until the shell exits, the socket drops, or ctx ends. pty=true
// attaches sh to a pseudo-terminal and expects framed input; ?pty=1 tells
// the controller the mode actually established. When PTY allocation fails
// the session falls back to a pipe, so a shell is always available.
func runShell(ctx context.Context, ep shellEndpoint, pty bool) {
	var master, slave *os.File
	if pty {
		var err error
		if master, slave, err = openPty(); err != nil {
			log.Printf("[shell] PTY allocation failed (%v); using a pipe", err)
			pty = false
		}
	}
	url := ep.base + "/shell/" + ep.deviceID
	if pty {
		url += "?pty=1"
	}
	conn, _, err := ep.dialer.DialContext(ctx, url, ep.header)
	if err != nil {
		log.Printf("[shell] dial %s: %v", url, err)
		if pty {
			master.Close()
			slave.Close()
		}
		return
	}
	defer conn.Close()

	cmd := exec.CommandContext(ctx, "/system/bin/sh")
	var output io.Reader
	var input io.WriteCloser
	if pty {
		cmd.Stdin, cmd.Stdout, cmd.Stderr = slave, slave, slave
		cmd.Env = append(os.Environ(), "TERM=xterm-256color")
		cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Setctty: true}
		output, input = master, master
	} else {
		stdin, err := cmd.StdinPipe()
		if err != nil {
			log.Printf("[shell] stdin pipe: %v", err)
			return
		}
		stdout, err := cmd.StdoutPipe()
		if err != nil {
			log.Printf("[shell] stdout pipe: %v", err)
			return
		}
		cmd.Stderr = cmd.Stdout
		output, input = stdout, stdin
	}
	if err := cmd.Start(); err != nil {
		log.Printf("[shell] start: %v", err)
		if pty {
			master.Close()
			slave.Close()
		}
		return
	}
	if pty {
		// The child holds its own slave fd; ours would keep the master
		// from reading EOF after the shell exits.
		slave.Close()
	}

	done := make(chan struct{})
	go func() {
		defer close(done)
		buf := make([]byte, 4096)
		for {
			n, err := output.Read(buf)
			if n > 0 && conn.WriteMessage(websocket.BinaryMessage, buf[:n]) != nil {
				return
			}
			if err != nil {
				return
			}
		}
	}()
	go func() {
		defer input.Close()
		for {
			_, data, err := conn.ReadMessage()
			if err != nil {
				return
			}
			if !pty {
				if _, err := input.Write(data); err != nil {
					return
				}
				continue
			}
			if len(data) == 0 {
				continue
			}
			switch data[0] {
			case shellFrameStdin:
				if _, err := input.Write(data[1:]); err != nil {
					return
				}
			case shellFrameResize:
				if len(data) >= 5 {
					cols := binary.BigEndian.Uint16(data[1:3])
					rows := binary.BigEndian.Uint16(data[3:5])
					if err := setWinsize(master, cols, rows); err != nil {
						log.Printf("[shell] TIOCSWINSZ: %v", err)
					}
				}
			}
		}
	}()

	select {
	case <-done:
	case <-ctx.Done():
	}
	_ = cmd.Process.Kill()
	_ = cmd.Wait()
	if pty {
		master.Close()
	}
	log.Printf("[shell] session closed")
}
