//go:build server

// Package mixerapi is a minimal Go binding to Amazon's libmixerAPI.so, the
// client library of the `mixer` daemon that owns the audio HAL on Fire OS 6
// (docs/fireos6-port.md §2.1): firmware ⇄ libmixerAPI ⇄ mixer ⇄ HAL (ASP).
// This is the Fire OS 6 counterpart of internal/opensl's firmware ⇄ OpenSL
// ⇄ AudioFlinger ⇄ HAL path on Fire OS 5; cmd/server.go picks between the
// two backends at runtime (OpenSL when libOpenSLES.so resolves, else this
// package).
//
// The library is dlopen'd at RUNTIME, not linked, for the same reason as
// opensl: a Fire OS 5 device, which has no libmixerAPI.so, still starts the
// binary.
//
// Both Recorder and Player present the same BLOCKING ReadStamped/Write
// shape internal/opensl does, for the same reason: slmic/slspeaker stay a
// read loop and a pump loop rather than making every caller callback-shaped.
// Unlike OpenSL ES, libmixerAPI has no completion callback this package
// uses (see "C prototypes" below for why); instead every
// stream owns a goroutine, locked to one OS thread for the stream's whole
// lifetime via runtime.LockOSThread, that makes every blocking library
// call and bridges it to Go's channel-based API — libmixerAPI's thread
// affinity is undemonstrated either way, so one goroutine per handle is the
// conservative default per the port plan.
//
// # C prototypes
//
// Pinned against a disassembly of the device's libmixerAPI.so and confirmed by exercising each
// one from throwaway spikes on device G090LF0965260F1J (docs/
// fireos6-port.md §2.1). [INFERENCE, confirmed by behaviour]
//
//	void    *MixerOpenRec(const char *type);
//	void    *MixerGetBufRecTimed(void *h, int *status, unsigned *size, uint64_t *ts);
//	int      MixerReleaseBufRec(void *h);
//	void    *MixerOpenPlay(unsigned rate, unsigned ch, unsigned bits, uintptr_t mode); // 0 = PLAYBACK_MODE_MUSIC
//	void    *MixerGetBufPlay(void *h, int *status, unsigned *size);
//	int      MixerReleaseBufPlay(void *h, unsigned bytes);
//	unsigned MixerGetUnderflowMs(void *h); // 0xFFFFFFFF on a null handle or error
//	int      MixerClose(void *h);
//	unsigned MixerGetBufSize(void *h); // 0 on a null handle or error
//
// Status codes returned through *status (confirmed by behaviour): 0 ok, 110
// ETIMEDOUT (no data for roughly 1.5s — micAsr's own internal poll
// timeout), 112 EHOSTDOWN (the mixer tore the stream down — e.g. another
// ASR client, such as PuffinApp, took the mixer's single ASR mode back).
//
// MixerGetBufPlayTimed exists and was exercised, but its fourth argument's
// true shape could not be pinned: disassembly shows it reads a 64-bit value
// off the caller's stack, unlike MixerGetBufRecTimed's plain uint64_t* ts in
// a register, and every call made with a `uint64_t *ts` there read back
// ts=0 while behaving identically (buffer pointer, size, pacing) to plain
// MixerGetBufPlay. Rather than ship an unconfirmed ABI, the player uses
// plain MixerGetBufPlay — the backpressure finding below shows it already
// blocks for real pacing, so the "Timed" variant's only possible advantage
// here, a timestamp, was never observed to work anyway.
//
// MixerSetupAsyncCallbacks/MixerStopAsyncCallbacks and
// MixerPlaybackEffectControl were probed but are not called. Effect control
// returned "invalid effect"/"invalid effect op" for every (effect, op) in
// 1..4 tried against an open Music stream (see "Gain staging" below), so
// Phase 1 found no per-stream gain knob to pin down. Async callbacks are
// unneeded once plain GetBufPlay's own blocking is understood (below).
// MixerFlush is deliberately absent: a flushed play stream wedges, and every
// later MixerGetBufPlay waits 3 s (measured), so Player closes instead.
//
// # Capture clock domain
//
// MixerGetBufRecTimed's ts is CLOCK_MONOTONIC-equivalent on this device:
// comparing it against clock_gettime(CLOCK_MONOTONIC) and
// clock_gettime(CLOCK_BOOTTIME) taken immediately after each successful
// read shows ts in the SAME numeric range as both (not a separate epoch),
// with a stable local-minus-ts gap of roughly 72-78ms across consecutive
// reads — i.e. ts tracks the local monotonic clock with a slowly-varying
// processing/IPC latency offset, not a jump or a different epoch. So ts is
// used directly as each native block's end time (reframe.go backdates each
// period from it), unlike opensl's Recorder, which stamps at Go-side read
// completion because OpenSL ES's callback gives no HAL-side timestamp at
// all. Whether ts marks a block's first or last sample could not be told
// apart (16 ms blocks); playclock.go's constants were measured against ts
// read as the end, the conservative reading for them. micRaw, by contrast,
// returns ts=0 and is not used.
//
// # Backpressure and render timing
//
// A fresh play stream accepts exactly 16 chunks (800 ms) without blocking;
// every later Get/Release round-trip then blocks for about one chunk, paced
// by the DAC. Handoff time is therefore ~800 ms ahead of playout in steady
// state, so a completion stamped at handoff would sit outside the
// controller's 0–500 ms echo-lag search. Player instead stamps each write's
// completion at its estimated playout time on the capture ts scale, from a
// measured model of this pacing (playclock.go).
//
// # Native transfer size
//
// The mixer hands back exactly MixerGetBufSize(h) bytes per Get/Release
// round-trip for playback (measured 4800 bytes = 50ms at 48kHz mono, stable
// across every spike run) — but NOT for capture, where MixerGetBufSize
// returned 3200 (apparently some other stream-wide figure) while actual
// per-read blocks were consistently 512 bytes (16ms) for micAsr. So Player
// trusts MixerGetBufSize (confirmed to equal its actual per-transfer size);
// Recorder does not assume any fixed block size at all and re-frames
// whatever size each read actually returns (reframe.go handles any size,
// including ones that do not divide the period evenly). slmic/slspeaker
// re-frame across this native-size boundary: capture accumulates native
// blocks into 1,280-frame (80ms) periods; render accumulates
// render.SinkFrames-sized writes into whatever native chunk size Open
// measured.
//
// # Credentials
//
// A client whose primary group is not aipc (2901) gets a handle back from
// MixerOpenRec/MixerOpenPlay that silently never carries data: the mixer
// can't open the mode-0660 stream file the client created under the wrong
// group ("Mixer_Utils:AddStream:Failed to open ..." in logcat), and every
// subsequent read just times out (status 110) forever. NewRecorder/
// NewPlayer check egid before calling into the library at all, so this
// fails fast with ErrWrongGID instead of a silent, permanent timeout.
//
// # Gain staging (functional only — SPEC/plan "single volume authority")
//
// tinymix control 61 (PCM Playback Volume) was read before, during and
// after every playback spike and never changed value: the mixer does not
// rewrite it, so it stays the sole per-device volume control this package
// needs to respect. Separately, the mixer applies its own fixed scalar on
// top: every Music stream open logs
// `Mixer_AlgoRampGain:SetRampInt:gain 92 ref 100 119dB`, and
// persist.mixer.init.main.volume is 70 — a boot-time-only property read
// once when the `mixer` daemon itself starts, which this port must never
// restart. MixerPlaybackEffectControl, probed as a possible per-stream
// override, returned "invalid effect"/"invalid effect op" for every
// (effect, op) combination in 1..4 tried. No per-stream unity override was
// found, so there is a fixed, firmware-wide mixer-side scalar that control
// 61 cannot reach around. This is reported, not compensated for: the plan
// scopes gain staging functional-only, no loudness measurement.
package mixerapi

/*
#cgo CFLAGS: -O2
#cgo LDFLAGS: -ldl

#include <stdlib.h>
#include <string.h>
#include "shim.h"
*/
import "C"

import (
	"errors"
	"fmt"
	"log"
	"runtime"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"

	"golang.org/x/sys/unix"

	"github.com/wilbowes/EchoMuse/internal/monoclock"
)

// ErrClosed is returned by ReadStamped/Write after Close.
var ErrClosed = errors.New("mixerapi: closed")

// ErrWrongGID is returned by NewRecorder/NewPlayer when the process's
// primary group is not aipc (2901). See the package doc's "Credentials".
var ErrWrongGID = errors.New("mixerapi: not running with egid aipc (2901)")

// requiredGID is the mixer's required client primary group (aipc).
const requiredGID = 2901

func checkGID() error {
	if g := unix.Getegid(); g != requiredGID {
		return fmt.Errorf("%w (egid=%d)", ErrWrongGID, g)
	}
	return nil
}

// maxOutstandingWrites bounds Player's outstanding writes, matching
// render.Sink's documented "at most 8" and slspeaker's existing OpenSL
// hardwareBuffers choice of 4.
const maxOutstandingWrites = 4

// reopenAfterTimeouts is how many consecutive ETIMEDOUTs a Recorder waits
// before treating the stream as gone and re-opening it.
const reopenAfterTimeouts = 3

// reopenRetryInterval paces Recorder's re-open retries on persistent
// failure (e.g. the mixer itself is mid-restart).
const reopenRetryInterval = 500 * time.Millisecond

// Lib is a dlopen'd libmixerAPI.so instance, shared by every Recorder and
// Player opened against the same path — mirroring internal/opensl's Engine:
// one dlopen per process is simplest and nothing here needs two.
type Lib struct {
	lib C.em_lib
}

var (
	libOnce   sync.Once
	libPath   string
	sharedLib *Lib
	libErr    error
)

// Open loads libmixerAPI.so from path (an absolute path, or a soname the
// dynamic linker resolves — "libmixerAPI.so" resolves against /system/lib
// on the device).
func Open(path string) (*Lib, error) {
	libOnce.Do(func() {
		libPath = path
		cPath := C.CString(path)
		defer C.free(unsafe.Pointer(cPath))
		l := &Lib{}
		if msg := C.em_mixer_open(cPath, &l.lib); msg != nil {
			libErr = goErr(msg)
			return
		}
		sharedLib = l
	})
	if libErr != nil {
		return nil, libErr
	}
	if libPath != path {
		return nil, fmt.Errorf("mixerapi: already opened with path %q, got %q", libPath, path)
	}
	return sharedLib, nil
}

// ─── recorder ────────────────────────────────────────────────────────────────

// Recorder captures mono S16LE PCM from one mixer record stream (e.g.
// "micAsr"), re-framed into fixed-size periods. ReadStamped blocks for the
// next completed period.
type Recorder struct {
	lib    *Lib
	name   string
	handle unsafe.Pointer

	frames chan frame
	drops  atomic.Uint64

	stopped  chan struct{}
	pumpDone chan struct{}
}

type frame struct {
	pcm    []byte
	monoNs int64
}

// recQueueDepth is the frame queue's slack: periods ReadStamped's caller
// may fall behind before the pump drops the newest (counted, Drops()).
const recQueueDepth = 16

// recByteRate is micAsr's 16 kHz mono S16. It is fixed rather than read
// back: MixerGetRate can return 0xFFFFFFFF right after the open (a race;
// it reads 16000 by close), so the value at open cannot be trusted.
const recByteRate = 16000 * 2

// NewRecorder opens streamName (e.g. "micAsr") and re-frames its native
// blocks into fixed periodFrames-sample mono periods.
func (l *Lib) NewRecorder(streamName string, periodFrames int) (*Recorder, error) {
	if err := checkGID(); err != nil {
		return nil, err
	}
	if periodFrames <= 0 {
		return nil, fmt.Errorf("mixerapi: NewRecorder: periodFrames must be positive, got %d", periodFrames)
	}
	h, err := l.openRec(streamName)
	if err != nil {
		return nil, err
	}
	r := &Recorder{
		lib:      l,
		name:     streamName,
		handle:   h,
		frames:   make(chan frame, recQueueDepth),
		stopped:  make(chan struct{}),
		pumpDone: make(chan struct{}),
	}
	if sz := uint32(C.em_buf_size(&l.lib, h)); sz > 0 {
		log.Printf("mixerapi: %s: MixerGetBufSize=%d (informational — periods re-frame whatever size reads actually return)", streamName, sz)
	}
	go r.pump(periodFrames)
	return r, nil
}

func (l *Lib) openRec(name string) (unsafe.Pointer, error) {
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))
	h := C.em_open_rec(&l.lib, cName)
	if h == nil {
		return nil, fmt.Errorf("mixerapi: MixerOpenRec(%s): returned null", name)
	}
	return h, nil
}

func (r *Recorder) pump(periodFrames int) {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	defer close(r.pumpDone)

	// The pool covers every period that can be alive at once: recQueueDepth
	// queued, one held by the reader until its next ReadStamped, one filling.
	rf := newReframer(periodFrames*2, recQueueDepth+2, recByteRate)
	timeouts := 0
	loggedTimeout, loggedHostDown := false, false

	emit := func(period []byte, monoNs int64) bool {
		select {
		case r.frames <- frame{pcm: period, monoNs: monoNs}:
			return true
		default:
			r.drops.Add(1)
			return false
		}
	}

	for {
		select {
		case <-r.stopped:
			if r.handle != nil {
				C.em_close(&r.lib.lib, r.handle)
			}
			return
		default:
		}

		h := r.handle
		var status C.int
		var size C.uint
		var ts C.uint64_t
		buf := C.em_get_buf_rec_timed(&r.lib.lib, h, &status, &size, &ts)

		if int(status) == 0 && buf != nil && size > 0 {
			tsNs := int64(ts)
			if tsNs == 0 { // defensive: never seen in practice, never emit a zero stamp
				tsNs = monoclock.Now()
			}
			block := unsafe.Slice((*byte)(buf), int(size))
			rf.add(block, tsNs, emit)
		}
		// Release unconditionally after every Get, whatever the status — the
		// pattern that ran cleanly on hardware — on the handle just used,
		// before any reopen below replaces r.handle.
		C.em_release_buf_rec(&r.lib.lib, h)

		switch int(status) {
		case 0:
			timeouts, loggedTimeout, loggedHostDown = 0, false, false
		case 110: // ETIMEDOUT
			timeouts++
			if !loggedTimeout {
				log.Printf("mixerapi: %s: ETIMEDOUT (no data for ~1.5s)", r.name)
				loggedTimeout = true
			}
			if timeouts >= reopenAfterTimeouts {
				if r.reopen() {
					rf.reset()
				}
				timeouts = 0
			}
		case 112: // EHOSTDOWN
			if !loggedHostDown {
				log.Printf("mixerapi: %s: EHOSTDOWN (stream torn down), reopening", r.name)
				loggedHostDown = true
			}
			if r.reopen() {
				rf.reset()
			}
			timeouts = 0
		default:
			log.Printf("mixerapi: %s: MixerGetBufRecTimed status=%d", r.name, int(status))
		}
	}
}

// reopen closes the current handle and retries MixerOpenRec until it
// succeeds or Close() is called. Reports whether it got a fresh handle
// (false means Close() interrupted the retry).
func (r *Recorder) reopen() bool {
	if r.handle != nil {
		C.em_close(&r.lib.lib, r.handle)
		r.handle = nil
	}
	for {
		select {
		case <-r.stopped:
			return false
		default:
		}
		h, err := r.lib.openRec(r.name)
		if err == nil {
			r.handle = h
			return true
		}
		log.Printf("mixerapi: %s: reopen failed: %v (retrying)", r.name, err)
		select {
		case <-r.stopped:
			return false
		case <-time.After(reopenRetryInterval):
		}
	}
}

// ReadStamped blocks for the next completed period, with the
// CLOCK_MONOTONIC ns the mixer reported for it. pcm is this call's own
// buffer (safe to retain until the next ReadStamped). Returns ErrClosed
// once Close has run.
func (r *Recorder) ReadStamped() (pcm []byte, monoNs int64, err error) {
	f, ok := <-r.frames
	if !ok {
		return nil, 0, ErrClosed
	}
	return f.pcm, f.monoNs, nil
}

// Drops counts periods dropped because ReadStamped fell behind the pump.
func (r *Recorder) Drops() uint64 { return r.drops.Load() }

// Close stops capture and releases the recorder. May take up to ~1.5s if
// the pump is blocked inside a read waiting for data (the ETIMEDOUT
// window): there is no safe way to interrupt that call from outside the
// pump's own goroutine, since this package does not assume concurrent
// calls on the same handle are safe. Safe to call once; a second call is a
// no-op.
func (r *Recorder) Close() {
	select {
	case <-r.stopped:
		return // already closed
	default:
		close(r.stopped)
	}
	<-r.pumpDone
	close(r.frames)
}

// ─── player ──────────────────────────────────────────────────────────────────

// Player renders mono S16LE PCM through one mixer MUSIC-mode stream. Write
// blocks until a slot is free (bounded to maxOutstandingWrites) and paces
// the caller at playback rate once the mixer's own initial buffering
// allowance is used up (see the package doc's "Backpressure"). Completions
// carry the estimated playout time of each write's last byte (playclock.go).
type Player struct {
	lib    *Lib
	handle unsafe.Pointer

	chunkBytes int // native per-transfer size, from MixerGetBufSize at Open
	maxWrite   int // largest accepted Write, informational/validating only

	mu      sync.Mutex
	cond    *sync.Cond
	acc     []byte
	written int64
	release int64
	pending []pendingWrite
	closed  bool
	err     error // sticky: set by the pump on a round-trip failure

	wake chan struct{}

	clock playClock // pump goroutine only

	onComplete atomic.Pointer[func(int64)]

	underflowMs atomic.Uint32
}

type pendingWrite struct {
	end int64 // cumulative byte offset (relative to written) at which this write's data ends
}

// NewPlayer opens a rate/ch/bits MUSIC-mode (mode 0) stream. maxWriteBytes
// bounds the largest single Write this player accepts.
func (l *Lib) NewPlayer(rateHz, ch, bits int, mode uintptr, maxWriteBytes int) (*Player, error) {
	if err := checkGID(); err != nil {
		return nil, err
	}
	h := C.em_open_play(&l.lib, C.uint(rateHz), C.uint(ch), C.uint(bits), C.uintptr_t(mode))
	if h == nil {
		return nil, fmt.Errorf("mixerapi: MixerOpenPlay(%d,%d,%d,%d): returned null", rateHz, ch, bits, mode)
	}
	chunkBytes := int(C.em_buf_size(&l.lib, h))
	if chunkBytes <= 0 {
		C.em_close(&l.lib, h)
		return nil, fmt.Errorf("mixerapi: MixerGetBufSize after MixerOpenPlay: got %d", chunkBytes)
	}
	p := &Player{
		lib:        l,
		handle:     h,
		chunkBytes: chunkBytes,
		maxWrite:   maxWriteBytes,
		acc:        make([]byte, 0, chunkBytes*2),
		wake:       make(chan struct{}, 1),
		clock:      newPlayClock(chunkBytes, int64(rateHz*ch*bits/8)),
	}
	p.cond = sync.NewCond(&p.mu)
	go p.pump()
	return p, nil
}

// SetOnComplete installs the per-buffer completion callback. It must be set
// before the first Write and must return immediately.
func (p *Player) SetOnComplete(fn func(monoNs int64)) { p.onComplete.Store(&fn) }

// Write copies data into the pending accumulator and returns once there is
// room for maxOutstandingWrites more (blocking otherwise), handing the
// actual mixer round-trips to the pump goroutine. data is copied; the
// caller's slice may be reused immediately. Returns ErrClosed once Close
// has run, or a round-trip's error once the pump has observed one.
func (p *Player) Write(data []byte) error {
	if len(data) > p.maxWrite {
		return fmt.Errorf("mixerapi: write of %d bytes exceeds the %d-byte limit", len(data), p.maxWrite)
	}
	p.mu.Lock()
	for !p.closed && p.err == nil && len(p.pending) >= maxOutstandingWrites {
		p.cond.Wait()
	}
	if p.closed {
		p.mu.Unlock()
		return ErrClosed
	}
	if p.err != nil {
		err := p.err
		p.mu.Unlock()
		return err
	}
	p.acc = append(p.acc, data...)
	p.written += int64(len(data))
	p.pending = append(p.pending, pendingWrite{end: p.written})
	p.mu.Unlock()
	select {
	case p.wake <- struct{}{}:
	default:
	}
	return nil
}

func (p *Player) pump() {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	for {
		p.mu.Lock()
		for !p.closed && p.err == nil && len(p.acc) < p.chunkBytes {
			p.mu.Unlock()
			<-p.wake
			p.mu.Lock()
		}
		if p.closed {
			p.mu.Unlock()
			p.teardown()
			return
		}
		if p.err != nil {
			// Nothing more to do until Close(); Write() now rejects new
			// data, so acc cannot grow further.
			p.mu.Unlock()
			<-p.wake // woken again by Close()'s final signal (see Close)
			continue
		}
		chunk := p.acc[:p.chunkBytes]
		p.mu.Unlock()

		blocked, handoffNs, err := p.writeChunk(chunk)

		p.mu.Lock()
		if err != nil {
			p.err = err
			p.cond.Broadcast()
			p.mu.Unlock()
			continue
		}
		chunkStart := p.release
		p.acc = p.acc[:copy(p.acc, p.acc[p.chunkBytes:])]
		p.release += int64(p.chunkBytes)
		var ends [maxOutstandingWrites]int64
		fired := 0
		for fired < len(p.pending) && p.pending[fired].end <= p.release {
			ends[fired] = p.pending[fired].end
			fired++
		}
		p.pending = p.pending[:copy(p.pending, p.pending[fired:])]
		p.cond.Broadcast()
		p.mu.Unlock()

		startNs := p.clock.chunk(handoffNs, blocked)
		if fired > 0 {
			fn := p.onComplete.Load()
			for _, end := range ends[:fired] {
				doneNs := p.clock.completion(startNs, end-chunkStart)
				if fn != nil {
					(*fn)(doneNs)
				}
			}
		}
	}
}

// writeChunk performs one native Get/fill/Release round-trip — the call
// that blocks for real pacing once the mixer's initial allowance is spent
// (package doc, "Backpressure"). It reports whether the round-trip waited
// for the mixer and the CLOCK_MONOTONIC time it returned, which playClock
// turns into a playout estimate.
func (p *Player) writeChunk(chunk []byte) (blocked bool, handoffNs int64, err error) {
	before := monoclock.Now()
	var status C.int
	var size C.uint
	buf := C.em_get_buf_play(&p.lib.lib, p.handle, &status, &size)
	if buf == nil || status != 0 {
		return false, 0, fmt.Errorf("mixerapi: MixerGetBufPlay: status=%d", int(status))
	}
	n := int(size)
	if n > len(chunk) {
		n = len(chunk)
	}
	if n > 0 {
		C.memcpy(buf, unsafe.Pointer(&chunk[0]), C.size_t(n))
	}
	if rc := C.em_release_buf_play(&p.lib.lib, p.handle, C.uint(n)); rc != 0 {
		return false, 0, fmt.Errorf("mixerapi: MixerReleaseBufPlay: rc=%d", int(rc))
	}
	handoffNs = monoclock.Now()
	if ms := uint32(C.em_underflow_ms(&p.lib.lib, p.handle)); ms != 0xFFFFFFFF {
		if old := p.underflowMs.Swap(ms); ms > old {
			log.Printf("mixerapi: play underflow %dms (was %dms)", ms, old)
		}
	}
	return handoffNs-before >= playBlockedNs, handoffNs, nil
}

// UnderflowMs is the last MixerGetUnderflowMs reading: a rise means the
// pump fell behind real playback, a genuine hardware underrun rather than
// this package's own pacing.
func (p *Player) UnderflowMs() uint32 { return p.underflowMs.Load() }

// teardown closes the handle, discarding whatever is still queued. It must
// not call MixerFlush: on this build a flushed stream wedges, and every
// later MixerGetBufPlay waits 3 s (measured). MixerClose with 800 ms still
// queued returns in ~1.4 ms and the next stream opens normally. Runs only
// from the pump goroutine.
func (p *Player) teardown() {
	C.em_close(&p.lib.lib, p.handle)
}

// Close stops accepting writes and releases the player, unblocking any
// Write currently waiting for outstanding room. Safe to call once; a
// second call is a no-op.
func (p *Player) Close() error {
	p.mu.Lock()
	if p.closed {
		p.mu.Unlock()
		return nil
	}
	p.closed = true
	p.mu.Unlock()
	p.cond.Broadcast()
	select {
	case p.wake <- struct{}{}:
	default:
	}
	return nil
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
