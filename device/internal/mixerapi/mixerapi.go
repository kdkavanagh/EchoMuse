//go:build server

// Package mixerapi is a minimal Go binding to Amazon's libmixerAPI.so, the
// client library of the `mixer` daemon that owns the audio HAL on Fire OS 6
// (docs/fireos6-port.md §2.1): firmware ⇄ libmixerAPI ⇄ mixer ⇄ HAL (ASP).
//
// The library is dlopen'd at RUNTIME, not linked: it is a vendor library
// with no NDK stub to link against, and a missing or broken copy then fails
// Open with an error rather than the binary at load.
//
// Both Recorder and Player present a BLOCKING ReadStamped/Write shape, so
// slmic/slspeaker stay a read loop and a pump loop rather than making every
// caller callback-shaped. libmixerAPI has no completion callback this
// package uses (see "C prototypes" below for why); instead every
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
//	void    *MixerOpenPlay(unsigned rate, unsigned ch, unsigned bits, const char *type); // NULL = "Music"
//	void    *MixerGetBufPlay(void *h, int *status, unsigned *size);
//	int      MixerReleaseBufPlay(void *h, unsigned bytes);
//	unsigned MixerGetUnderflowMs(void *h); // since the last call; 0xFFFFFFFF on a null handle or error
//	int      MixerClose(void *h);
//	unsigned MixerGetBufSize(void *h); // 0 on a null handle or error
//	unsigned mixer::DataTrans::GetNumBlkReady(void *h); // C++ member, h as `this`
//
// MixerOpenPlay is MixerOpenPlayAdv(rate, ch, bits, 0, type): mode 0
// (PLAYBACK_MODE_MUSIC) and a stream type name, which is also the mixer's
// volume class. The type sizes the client-side block queue (constants in
// MixerOpenPlayAdv): "Voip" 4 × 50 ms, "LineIn" 8 × 8 ms, "WHA" 64 × 16 ms,
// anything else, NULL included, 16 × 50 ms. Player opens the default
// "Music": "Voip" is a volume class of its own and the mixer special-cases
// "LineIn" (it drops a stream's initial frames); the queue depth is bounded
// by Player instead (below).
//
// Every handle is the library's mixer::DataTrans*: MixerGetBufSize,
// MixerGetUnderflowMs and MixerGetNumBytes forward it unchanged as `this`
// to DataTrans::GetUserBlkSize, GetResetUnderflowMs and AddCount. That is
// what makes the exported, non-virtual DataTrans::GetNumBlkReady callable:
// a leaf that reads the shared queue header, the count of released blocks
// the mixer has not yet taken. Reading the underflow counter resets it.
//
// Status codes returned through *status (confirmed by behaviour): 0 ok, 110
// ETIMEDOUT (no data for roughly 1.5s — micAsr's own internal poll
// timeout), 112 EHOSTDOWN (the mixer tore the stream down — e.g. another
// ASR client, such as PuffinApp, took the mixer's single ASR mode back).
// Any other status means the handle is wedged (see "Wall-clock steps").
//
// MixerGetBufPlayTimed exists and was exercised, but its fourth argument's
// true shape could not be pinned: disassembly shows it reads a 64-bit value
// off the caller's stack, unlike MixerGetBufRecTimed's plain uint64_t* ts in
// a register, and every call made with a `uint64_t *ts` there read back
// ts=0 while behaving identically (buffer pointer, size, pacing) to plain
// MixerGetBufPlay. Rather than ship an unconfirmed ABI, the player uses
// plain MixerGetBufPlay and takes its timing from the queue depth instead
// (below), so the "Timed" variant's only possible advantage here, a
// timestamp, was never observed to work anyway.
//
// MixerSetupAsyncCallbacks/MixerStopAsyncCallbacks and
// MixerPlaybackEffectControl were probed but are not called. Effect control
// returned "invalid effect"/"invalid effect op" for every (effect, op) in
// 1..4 tried against an open Music stream (see "Gain staging" below), so
// Phase 1 found no per-stream gain knob to pin down. Async callbacks are
// unneeded: Player paces itself on the queue depth (below).
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
// period from it). Whether ts marks a block's first or last sample could not be told
// apart (16 ms blocks); playclock.go's constants were measured against ts
// read as the end, the conservative reading for them. micRaw, by contrast,
// returns ts=0 and is not used.
//
// # Backpressure and render timing
//
// A fresh "Music" stream accepts exactly 16 chunks (800 ms) without
// blocking; after that every Get/Release round-trip blocks for about one
// chunk, paced by the DAC. A writer that lets GetBufPlay pace it therefore
// keeps 800 ms queued, and everything it plays (the wake chime included)
// is heard ~0.96 s after it was written. Player instead hands a chunk
// over only while fewer than playQueueBlocks blocks wait in the mixer's
// queue (GetNumBlkReady, polled), and Write holds back once a native
// chunk is waiting in Player, so at most one chunk plus one write sits
// in front of the mixer's own queue. Each write's completion is stamped
// at its estimated playout time on the capture ts scale, from the queue
// depth read at handoff (playclock.go).
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
// # Wall-clock steps
//
// A micAsr stream open while the wall clock steps forward can wedge. On a
// Fire OS 6574.1 Dot, the boot NTP sync stepped the clock 3m23s forward
// 11 s after the stream opened: the next read returned ETIMEDOUT at once,
// and every read after it status 14, with logcat
// "Mixer_DataTrans:InCapture-GetReadBuff:check bababeef state 0:write
// access ts <wall-clock ms>" and "MixerReleaseBufRec:Release Failed". The
// handle never carried data again, while the same stream opened by a
// restarted process did at once (2026-10-11). The same Dot's first boot,
// a 2010 → 2026 step, scored 3 wake hops in its whole session; a Fire OS
// 6.5.6.9 Dot ran through a 15h46m step unharmed. So a Recorder opens its
// stream only once NewRecorder's start is closed (the server closes it
// after the first sync, internal/timesync), and re-opens on any status but
// 0 and 110.
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
// # Gain staging (SPEC/plan "single volume authority")
//
// tinymix control 61 (PCM Playback Volume) was read before, during and
// after every playback spike and never changed value: the mixer does not
// rewrite it, so it stays the sole per-device volume control. The mixer's
// main volume (persist.mixer.init.main.volume, 70) is deliberately left
// alone. It is also the AFE's speaker volume index, which picks the output
// EQ and limiter, and at 70 the output matches Fire OS 5: a −12 dBFS tone
// sweep through each platform's render sink, read back at the codec input
// from the DL1 write-back (tinycap -d 9), came out within 0.04 dB of Fire
// OS 5's from 250 Hz to 4 kHz and 1 dB lower at 125 Hz. Main volume 100
// was 13–18 dB louder from 500 Hz up (2026-10-07).
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

// maxOutstandingWrites bounds Player's outstanding writes at render.Sink's
// documented "at most 8". Write's one-chunk bound on the accumulator is
// the tighter limit for render.SinkFrames writes (≤ 3 outstanding).
const maxOutstandingWrites = 4

// playQueueBlocks is how many released blocks Player lets wait in the
// mixer's queue: 150 ms of "Music" blocks, against the 800 ms the mixer
// itself accepts. A burst written to the render sink reached micAsr capture
// stamps 321–385 ms later at 3 blocks, 271–331 ms at 2 and 957–986 ms with
// the full queue; through Fire OS 5's OpenSL sink, 356–371 ms (2026-10-07).
// 3 matches Fire OS 5 and leaves a late pump at least two queued blocks, one
// of them possibly playing, before the mixer runs dry.
const playQueueBlocks = 3

// playPollInterval paces Player's queue-depth polling while the queue is
// full: well under one 50 ms block, so a freed block is refilled promptly.
const playPollInterval = 10 * time.Millisecond

// reopenAfterTimeouts is how many consecutive ETIMEDOUTs a Recorder waits
// before treating the stream as gone and re-opening it.
const reopenAfterTimeouts = 3

// reopenRetryInterval paces Recorder's open retries on persistent failure
// (e.g. the mixer itself is mid-restart), and its re-opens of a wedged
// handle, so a fresh stream that fails at once cannot spin.
const reopenRetryInterval = 500 * time.Millisecond

// Lib is a dlopen'd libmixerAPI.so instance, shared by every Recorder and
// Player opened against the same path: one dlopen per process is simplest
// and nothing here needs two.
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

// NewRecorder re-frames streamName's native blocks (e.g. "micAsr") into
// fixed periodFrames-sample mono periods. The stream is opened once start
// is closed (see the package doc's "Wall-clock steps"); until then, and
// while an open fails and is retried, ReadStamped just waits.
func (l *Lib) NewRecorder(streamName string, periodFrames int, start <-chan struct{}) (*Recorder, error) {
	if err := checkGID(); err != nil {
		return nil, err
	}
	if periodFrames <= 0 {
		return nil, fmt.Errorf("mixerapi: NewRecorder: periodFrames must be positive, got %d", periodFrames)
	}
	r := &Recorder{
		lib:      l,
		name:     streamName,
		frames:   make(chan frame, recQueueDepth),
		stopped:  make(chan struct{}),
		pumpDone: make(chan struct{}),
	}
	go r.pump(periodFrames, start)
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

func (r *Recorder) pump(periodFrames int, start <-chan struct{}) {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	defer close(r.pumpDone)

	select {
	case <-start:
	case <-r.stopped:
		return
	}
	if !r.openStream() {
		return
	}
	if sz := uint32(C.em_buf_size(&r.lib.lib, r.handle)); sz > 0 {
		log.Printf("mixerapi: %s: MixerGetBufSize=%d (informational — periods re-frame whatever size reads actually return)", r.name, sz)
	}

	// The pool covers every period that can be alive at once: recQueueDepth
	// queued, one held by the reader until its next ReadStamped, one filling.
	rf := newReframer(periodFrames*2, recQueueDepth+2, recByteRate)
	timeouts, reopens := 0, 0
	loggedTimeout, loggedHostDown, loggedWedged := false, false, false

	emit := func(period []byte, monoNs int64) bool {
		select {
		case r.frames <- frame{pcm: period, monoNs: monoNs}:
			return true
		default:
			r.drops.Add(1)
			return false
		}
	}
	reopen := func() {
		if r.openStream() {
			rf.reset()
			reopens++
		}
		timeouts = 0
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
			if reopens > 0 {
				log.Printf("mixerapi: %s: carrying data again after %d re-open(s)", r.name, reopens)
			}
			timeouts, reopens = 0, 0
			loggedTimeout, loggedHostDown, loggedWedged = false, false, false
		case 110: // ETIMEDOUT
			timeouts++
			if !loggedTimeout {
				log.Printf("mixerapi: %s: ETIMEDOUT (no data for ~1.5s)", r.name)
				loggedTimeout = true
			}
			if timeouts >= reopenAfterTimeouts {
				reopen()
			}
		case 112: // EHOSTDOWN
			if !loggedHostDown {
				log.Printf("mixerapi: %s: EHOSTDOWN (stream torn down), reopening", r.name)
				loggedHostDown = true
			}
			reopen()
		default: // a wedged handle: see the package doc's "Wall-clock steps"
			if !loggedWedged {
				log.Printf("mixerapi: %s: MixerGetBufRecTimed status=%d, reopening", r.name, int(status))
				loggedWedged = true
			}
			if r.wait(reopenRetryInterval) {
				reopen()
			}
		}
	}
}

// openStream closes the current handle, if any, and retries MixerOpenRec
// until it succeeds or Close() is called. Reports whether it got a fresh
// handle (false means Close() interrupted the retry).
func (r *Recorder) openStream() bool {
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
		log.Printf("mixerapi: %s: open failed: %v (retrying)", r.name, err)
		if !r.wait(reopenRetryInterval) {
			return false
		}
	}
}

// wait sleeps d, or until Close() is called; reports whether it slept the
// full d.
func (r *Recorder) wait(d time.Duration) bool {
	select {
	case <-r.stopped:
		return false
	case <-time.After(d):
		return true
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

// Player renders mono S16LE PCM through one mixer "Music" stream. Write
// blocks while a native chunk is already waiting to be handed over, and
// the pump hands one over only while fewer than playQueueBlocks blocks wait
// in the mixer's queue, which paces the caller at playback rate (see the
// package doc's "Backpressure"). Completions carry the estimated playout
// time of each write's last byte (playclock.go).
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

	clock       playClock // pump goroutine only
	underflowMs uint64    // pump goroutine only: total since open

	onComplete atomic.Pointer[func(int64)]
}

type pendingWrite struct {
	end int64 // cumulative byte offset (relative to written) at which this write's data ends
}

// NewPlayer opens a rate/ch/bits stream of the default "Music" type (see
// the package doc's "C prototypes"). maxWriteBytes bounds the largest single
// Write this player accepts.
func (l *Lib) NewPlayer(rateHz, ch, bits int, maxWriteBytes int) (*Player, error) {
	if err := checkGID(); err != nil {
		return nil, err
	}
	h := C.em_open_play(&l.lib, C.uint(rateHz), C.uint(ch), C.uint(bits))
	if h == nil {
		return nil, fmt.Errorf("mixerapi: MixerOpenPlay(%d,%d,%d,Music): returned null", rateHz, ch, bits)
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
		acc:        make([]byte, 0, chunkBytes+maxWriteBytes),
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

// Write copies data into the pending accumulator, first blocking while a
// whole native chunk is already waiting for the pump, which hands the
// actual mixer round-trips over at the queue's pace. data is copied; the
// caller's slice may be reused immediately. Returns ErrClosed once Close
// has run, or a round-trip's error once the pump has observed one.
func (p *Player) Write(data []byte) error {
	if len(data) > p.maxWrite {
		return fmt.Errorf("mixerapi: write of %d bytes exceeds the %d-byte limit", len(data), p.maxWrite)
	}
	p.mu.Lock()
	for !p.closed && p.err == nil && (len(p.acc) >= p.chunkBytes || len(p.pending) >= maxOutstandingWrites) {
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

		if !p.awaitQueueRoom() {
			continue // closed while waiting: the loop tears down
		}
		ahead, handoffNs, err := p.writeChunk(chunk)

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

		startNs := p.clock.chunk(handoffNs, ahead)
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

// awaitQueueRoom sleeps until fewer than playQueueBlocks released blocks
// wait in the mixer's queue (package doc, "Backpressure"). It reports false
// if Close ran meanwhile.
func (p *Player) awaitQueueRoom() bool {
	for uint(C.em_blk_ready(&p.lib.lib, p.handle)) >= playQueueBlocks {
		time.Sleep(playPollInterval)
		p.mu.Lock()
		closed := p.closed
		p.mu.Unlock()
		if closed {
			return false
		}
	}
	return true
}

// writeChunk performs one native Get/fill/Release round-trip. It reports
// how many released blocks were still queued ahead of this one and the
// CLOCK_MONOTONIC time the release returned, which playClock turns into a
// playout estimate.
func (p *Player) writeChunk(chunk []byte) (ahead int, handoffNs int64, err error) {
	var status C.int
	var size C.uint
	buf := C.em_get_buf_play(&p.lib.lib, p.handle, &status, &size)
	if buf == nil || status != 0 {
		return 0, 0, fmt.Errorf("mixerapi: MixerGetBufPlay: status=%d", int(status))
	}
	n := int(size)
	if n > len(chunk) {
		n = len(chunk)
	}
	if n > 0 {
		C.memcpy(buf, unsafe.Pointer(&chunk[0]), C.size_t(n))
	}
	ahead = int(C.em_blk_ready(&p.lib.lib, p.handle))
	if rc := C.em_release_buf_play(&p.lib.lib, p.handle, C.uint(n)); rc != 0 {
		return 0, 0, fmt.Errorf("mixerapi: MixerReleaseBufPlay: rc=%d", int(rc))
	}
	handoffNs = monoclock.Now()
	// The counter resets on every read (DataTrans::GetResetUnderflowMs), so
	// any nonzero reading is a new underrun: the pump fell behind playback.
	if ms := uint32(C.em_underflow_ms(&p.lib.lib, p.handle)); ms != 0xFFFFFFFF && ms > 0 {
		p.underflowMs += uint64(ms)
		log.Printf("mixerapi: play underflow %dms (total %dms)", ms, p.underflowMs)
	}
	return ahead, handoffNs, nil
}

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
