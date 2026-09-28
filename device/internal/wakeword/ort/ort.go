// Package ort runs the BCResNet graph with a dlopen'd ONNX Runtime (SPEC §5.2,
// §16.5). The library and the graph are hash-named speech assets; nothing
// links against ORT, so the firmware starts without it.
//
// A Session is owned by one goroutine: the detector's wake thread.
package ort

/*
#cgo CFLAGS: -I${SRCDIR}/include -O2
#cgo LDFLAGS: -ldl

#include "shim.h"
*/
import "C"

import (
	"errors"
	"fmt"
	"sync"
	"unsafe"

	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
)

// Runtime is a loaded libonnxruntime.so and its environment. Runtimes are
// never unloaded: sessions created from one may outlive a runtime switch until
// the detector swaps them out.
type Runtime struct {
	rt      C.em_runtime
	version string
}

var (
	openMu   sync.Mutex
	runtimes = map[string]*Runtime{}
)

// Open loads the runtime library at libPath. Opening the same path again
// returns the same Runtime, so one hash-named library has one environment.
func Open(libPath string) (*Runtime, error) {
	openMu.Lock()
	defer openMu.Unlock()
	if r := runtimes[libPath]; r != nil {
		return r, nil
	}
	cPath := C.CString(libPath)
	defer C.free(unsafe.Pointer(cPath))
	r := &Runtime{}
	if err := goErr(C.em_ort_open(cPath, &r.rt)); err != nil {
		return nil, fmt.Errorf("ort: open %s: %w", libPath, err)
	}
	r.version = C.GoString(C.em_ort_version(&r.rt))
	runtimes[libPath] = r
	return r, nil
}

// Version is the library's version string, e.g. "1.19.2".
func (r *Runtime) Version() string { return r.version }

// Session is one graph with one float32 [1, n] input and one float32 [1, k]
// output. It implements bcresnet.Inferer.
type Session struct {
	m        C.em_model
	inLen    int
	outCount int
	out      []float32
	outN     C.size_t // Run's output count; a field so cgo need not heap-allocate it
}

var _ bcresnet.Inferer = (*Session)(nil)

// ErrClosed is returned by Run after Close.
var ErrClosed = errors.New("ort: session is closed")

// NewSession loads the graph at path with the SPEC §16.5 session options. It
// fails when the XNNPACK provider cannot attach.
func (r *Runtime) NewSession(path string) (*Session, error) {
	return r.newSession(path, true)
}

func (r *Runtime) newSession(path string, requireXNNPACK bool) (*Session, error) {
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))
	s := &Session{}
	req := C.int(0)
	if requireXNNPACK {
		req = 1
	}
	if err := goErr(C.em_model_load(&r.rt, cPath, req, &s.m)); err != nil {
		return nil, fmt.Errorf("ort: load %s: %w", path, err)
	}
	s.inLen = int(s.m.in_len)
	s.outCount = int(s.m.out_count)
	if s.inLen <= 0 || s.outCount <= 0 {
		C.em_model_free(&s.m)
		return nil, fmt.Errorf("ort: %s has dynamic IO dimensions [*, %d] -> [*, %d]", path, s.inLen, s.outCount)
	}
	s.out = make([]float32, s.outCount)
	return s, nil
}

// InputLen is the graph's fixed input length.
func (s *Session) InputLen() int { return s.inLen }

// OutputCount is the graph's fixed output count.
func (s *Session) OutputCount() int { return s.outCount }

// XNNPACK reports whether the XNNPACK provider is attached.
func (s *Session) XNNPACK() bool { return s.m.xnnpack != 0 }

// Run executes one inference. The result aliases a buffer the next Run
// overwrites. It does not allocate.
func (s *Session) Run(in []float32) ([]float32, error) {
	if s.m.sess == nil {
		return nil, ErrClosed
	}
	if len(in) != s.inLen {
		return nil, fmt.Errorf("ort: input of %d values, graph takes %d", len(in), s.inLen)
	}
	err := goErr(C.em_model_run(&s.m, (*C.float)(unsafe.Pointer(&in[0])), C.int64_t(len(in)),
		(*C.float)(unsafe.Pointer(&s.out[0])), C.size_t(len(s.out)), &s.outN))
	if err != nil {
		return nil, fmt.Errorf("ort: run: %w", err)
	}
	return s.out[:s.outN], nil
}

// Close releases the session. It is safe to call twice.
func (s *Session) Close() error {
	C.em_model_free(&s.m)
	return nil
}

func goErr(msg *C.char) error {
	if msg == nil {
		return nil
	}
	defer C.free(unsafe.Pointer(msg))
	return errors.New(C.GoString(msg))
}
