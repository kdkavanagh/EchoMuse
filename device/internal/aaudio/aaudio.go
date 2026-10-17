//go:build server

// Package aaudio is a minimal Go binding to Android's AAudio, the audio
// backend of the Echo Show on LineageOS 18.1 (docs/echo-show-lineage-plan.md
// decision 3): VOICE_RECOGNITION capture reaches the HAL's ASP pipeline 0
// (AEC, v3.3 metadata in bit 0 on our libasp), media playback becomes ASP's
// echo reference, and both directions report getTimestamp on
// CLOCK_MONOTONIC, which OpenSL ES no longer does there (docs/echo-show-
// crown.md §8.4 Q6). audioserver owns both PCMs.
//
// libaaudio.so is dlopen'd at RUNTIME (see shim.h): the firmware targets API
// 22, and the one binary also starts on images without AAudio, where Open is
// never called.
//
// Streams are shared, PCM I16 and PERFORMANCE_MODE_NONE: the low-latency
// path takes FAST capture, which drops timestamps (findings §8.4). Only the
// blocking read/write calls are used, so no AAudio thread calls into Go.
package aaudio

/*
#cgo CFLAGS: -O2
#cgo LDFLAGS: -ldl

#include "shim.h"
*/
import "C"

import (
	"errors"
	"fmt"
	"os"
	"sync"
	"time"
	"unsafe"
)

// ErrClosed is returned by Read and Write after Close.
var ErrClosed = errors.New("aaudio: closed")

// defaultLib resolves against /system/lib; EM_AAUDIO_LIB overrides it.
const defaultLib = "libaaudio.so"

// InputPresetVoiceRecognition is AAUDIO_INPUT_PRESET_VOICE_RECOGNITION: the
// HAL's ASP ASR pipeline (findings §8.4 Q1).
const InputPresetVoiceRecognition = C.AAUDIO_INPUT_PRESET_VOICE_RECOGNITION

// UsageMedia is AAUDIO_USAGE_MEDIA: STREAM_MUSIC, the device's volume.
const UsageMedia = C.AAUDIO_USAGE_MEDIA

// Lib is the process's dlopen'd libaaudio.so.
type Lib struct {
	lib C.em_aa_lib
}

var (
	openMu sync.Mutex
	lib    *Lib
)

// Open returns the process's library, loading libaaudio.so (or
// EM_AAUDIO_LIB) on first use. A failed open is retried by the next call.
func Open() (*Lib, error) {
	openMu.Lock()
	defer openMu.Unlock()
	if lib != nil {
		return lib, nil
	}
	path := os.Getenv("EM_AAUDIO_LIB")
	if path == "" {
		path = defaultLib
	}
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))
	l := &Lib{}
	if err := goErr(C.em_aa_load(cPath, &l.lib)); err != nil {
		return nil, fmt.Errorf("aaudio: open %s: %w", path, err)
	}
	lib = l
	return l, nil
}

// Stream is one open AAudio stream of mono or multichannel S16 frames.
// Read or Write are for one goroutine at a time; Close may run on another.
type Stream struct {
	lib      *Lib
	s        *C.AAudioStream
	input    bool
	channels int

	// ioMu is held across every read or write call into AAudio, which
	// must never overlap AAudioStream_close.
	ioMu sync.Mutex
	mu   sync.Mutex // guards closed
	// closed: Close has run; the stream is stopped, and released by
	// whichever of Close and an in-flight Read/Write finishes last.
	closed bool
}

// OpenInput opens (but does not start) a capture stream with the preset.
func (l *Lib) OpenInput(rateHz, channels int, preset int32) (*Stream, error) {
	return l.open(true, rateHz, channels, preset, 0)
}

// OpenOutput opens (but does not start) a playback stream with the usage,
// asking AAudio to keep at most bufferFrames frames queued (0: AAudio's
// default). BufferFrames reports what it granted.
func (l *Lib) OpenOutput(rateHz, channels int, usage int32, bufferFrames int) (*Stream, error) {
	return l.open(false, rateHz, channels, usage, bufferFrames)
}

func (l *Lib) open(input bool, rateHz, channels int, presetOrUsage int32, bufferFrames int) (*Stream, error) {
	var s *C.AAudioStream
	in := C.int(0)
	if input {
		in = 1
	}
	if err := goErr(C.em_aa_open(&l.lib, in, C.int32_t(rateHz), C.int32_t(channels),
		C.int32_t(presetOrUsage), C.int32_t(bufferFrames), &s)); err != nil {
		dir := "output"
		if input {
			dir = "input"
		}
		return nil, fmt.Errorf("aaudio: open %s (%d Hz, %d ch): %w", dir, rateHz, channels, err)
	}
	return &Stream{lib: l, s: s, input: input, channels: channels}, nil
}

// Start starts the stream.
func (st *Stream) Start() error {
	if !st.lockOpen() {
		return ErrClosed
	}
	err := goErr(C.em_aa_start(&st.lib.lib, st.s))
	st.ioMu.Unlock()
	if err != nil {
		return fmt.Errorf("aaudio: %w", err)
	}
	return nil
}

// Read reads up to len(pcm)/channels frames, blocking until all have
// arrived or timeout passes, and returns how many frames it read.
func (st *Stream) Read(pcm []int16, timeout time.Duration) (int, error) {
	frames := len(pcm) / st.channels
	if frames == 0 {
		return 0, nil
	}
	if !st.lockOpen() {
		return 0, ErrClosed
	}
	defer st.ioMu.Unlock()
	return st.result(C.em_aa_read(&st.lib.lib, st.s, unsafe.Pointer(&pcm[0]), C.int32_t(frames), C.int64_t(timeout)))
}

// Write writes len(pcm)/channels frames, blocking until all are queued or
// timeout passes, and returns how many frames it wrote.
func (st *Stream) Write(pcm []int16, timeout time.Duration) (int, error) {
	frames := len(pcm) / st.channels
	if frames == 0 {
		return 0, nil
	}
	if !st.lockOpen() {
		return 0, ErrClosed
	}
	defer st.ioMu.Unlock()
	return st.result(C.em_aa_write(&st.lib.lib, st.s, unsafe.Pointer(&pcm[0]), C.int32_t(frames), C.int64_t(timeout)))
}

// result turns a read or write's frame count or AAudio error into Go's.
func (st *Stream) result(n C.int32_t) (int, error) {
	if st.isClosed() {
		return 0, ErrClosed
	}
	if n < 0 {
		op := "write"
		if st.input {
			op = "read"
		}
		return 0, fmt.Errorf("aaudio: %s: %d %s", op, int(n), C.GoString(C.em_aa_text(&st.lib.lib, n)))
	}
	return int(n), nil
}

// lockOpen takes ioMu, which keeps Close from releasing the stream, and
// reports true; once Close has run it takes nothing and reports false.
func (st *Stream) lockOpen() bool {
	st.ioMu.Lock()
	if st.isClosed() {
		st.ioMu.Unlock()
		return false
	}
	return true
}

// Timestamp returns the stream's latest (frame position, CLOCK_MONOTONIC
// ns) pair: for input, when that frame was captured; for output, when it
// was (or will be) presented. ok is false when AAudio has none, as before
// the stream runs, and after Close.
func (st *Stream) Timestamp() (pos, ns int64, ok bool) {
	if !st.lockOpen() {
		return 0, 0, false
	}
	var p, t C.int64_t
	r := C.em_aa_timestamp(&st.lib.lib, st.s, &p, &t)
	st.ioMu.Unlock()
	return int64(p), int64(t), r == C.AAUDIO_OK
}

// FramesRead is the frames read so far: by the caller for input, by the
// server (consumed) for output. 0 after Close.
func (st *Stream) FramesRead() int64 {
	if !st.lockOpen() {
		return 0
	}
	defer st.ioMu.Unlock()
	return int64(C.em_aa_frames_read(&st.lib.lib, st.s))
}

// XRuns counts overruns (input) or underruns (output) since open.
func (st *Stream) XRuns() int {
	if !st.lockOpen() {
		return 0
	}
	defer st.ioMu.Unlock()
	return int(C.em_aa_xruns(&st.lib.lib, st.s))
}

// Describe names the stream's negotiated buffer: frames per burst, buffer
// size and capacity, and the performance mode, for the log.
func (st *Stream) Describe() string {
	if !st.lockOpen() {
		return "closed"
	}
	defer st.ioMu.Unlock()
	return fmt.Sprintf("burst %d, buffer %d of %d frames, performance mode %d",
		int(C.em_aa_burst(&st.lib.lib, st.s)), int(C.em_aa_buffer_size(&st.lib.lib, st.s)),
		int(C.em_aa_buffer_capacity(&st.lib.lib, st.s)), int(C.em_aa_perf_mode(&st.lib.lib, st.s)))
}

func (st *Stream) isClosed() bool {
	st.mu.Lock()
	defer st.mu.Unlock()
	return st.closed
}

// Close stops and releases the stream. A Read or Write blocked in AAudio
// returns ErrClosed. Safe to call more than once and from any goroutine.
func (st *Stream) Close() {
	st.mu.Lock()
	if st.closed {
		st.mu.Unlock()
		return
	}
	st.closed = true
	st.mu.Unlock()
	C.em_aa_stop(&st.lib.lib, st.s) // returns a blocked read or write
	st.ioMu.Lock()                  // and waits for it
	C.em_aa_close(&st.lib.lib, st.s)
	st.ioMu.Unlock()
}

// goErr converts the shim's malloc'd char* convention into a Go error,
// freeing the message. NULL means success.
func goErr(msg *C.char) error {
	if msg == nil {
		return nil
	}
	defer C.free(unsafe.Pointer(msg))
	return errors.New(C.GoString(msg))
}
