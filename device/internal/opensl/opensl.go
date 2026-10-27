//go:build server

// Package opensl is a minimal Go binding to Android's OpenSL ES, the Echo
// Show's audio backend (docs/echo-show-port-plan.md decision 3): capture
// through the VOICE_RECOGNITION preset reaches the HAL's ASP pipeline 0 (AEC,
// v3.3 metadata in bit 0), and playback through AudioFlinger becomes ASP's
// echo reference (docs/echo-show-crown.md §4.1, §4.3). audioserver owns both
// PCMs; the firmware never opens ALSA or writes tinymix on this path.
//
// The library is dlopen'd at RUNTIME (see shim.h), not linked, so the one
// firmware binary also starts on the Dot, where Open is simply never called.
//
// Recorder and Player present a BLOCKING API (ReadStamped/Write) although
// OpenSL ES calls back when buffer queues complete, on a thread it owns rather
// than one Go created. That keeps slmic a read loop and slspeaker a pump, and
// all bridging between the two models lives here.
package opensl

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
	"sync/atomic"
	"time"
	"unsafe"

	"github.com/wilbowes/EchoMuse/internal/audio/playclock"
	"github.com/wilbowes/EchoMuse/internal/monoclock"
)

var (
	// ErrClosed is returned by ReadStamped/Write after Close.
	ErrClosed = errors.New("opensl: closed")
	// ErrStalled is returned by ReadStamped/Write when OpenSL ES completed
	// no buffer for the caller's timeout: the stream is broken and must be
	// re-opened.
	ErrStalled = errors.New("opensl: no buffer completed in time")
)

// defaultLib resolves against /system/lib; EM_OPENSL_LIB overrides it.
const defaultLib = "libOpenSLES.so"

// playPipelineNs is a lower bound on AudioFlinger taking a player buffer →
// its sound in capture stamps (slmic anchors them on AFE_TIMESTAMP), for
// internal/audio/playclock. It must never exceed the true figure: a late
// render stamp makes the mic appear to lead its own reference, outside the
// controller's 0–500 ms lag search.
//
// From the §4.4 probe of docs/echo-show-crown.md (G6G16D12143203LD: a
// 48 kHz player of 4 × 20 ms buffers on the alarm stream; capture
// completions fitted against the sample count), re-read for this constant:
//   - Startup. The four buffers were queued at T0 and PLAYING set; the track
//     took three at once, then nothing for 184–186 ms (the output leaving
//     standby), then about one per 16 ms FastMixer cycle. So the mixer first
//     read sample 0 at about T0 + 168 ms, and sample 0 was heard at
//     T0 + 260 ms (GENERIC and UNPROCESSED capture): about 92 ms from the
//     mixer reading a sample to its sound on the fitted completion clock.
//   - Steady state. A buffer enqueued behind three others (44–60 ms still to
//     play, the head one partly read) and the track's 60–80 ms ring (it took
//     three buffers at once, not four) was heard 221–230 ms later, which
//     leaves 81–126 ms: consistent with the above.
//   - Capture scale. In the two VOICE_RECOGNITION captures with playback
//     (5,450 frames) a frame's AFE_TIMESTAMP sat 0–27 ms (median 15) before
//     the fitted completion of its last sample, so on slmic's stamps those
//     92 ms read as 65–92 ms.
//
// A buffer handed to an empty queue and a drained ring (the writer stalled)
// meets only that minimum, so this sits 25 ms under 65 ms: more than a
// FastMixer cycle of margin on the startup reading. Everywhere else the
// estimate is early by the head buffer, the ring and that margin (roughly
// 85–210 ms, inside the lag search), as the Dot's is by 59–99 ms. Phase 1
// re-measures it through this Player and slmic's stamps with
// device/tools/sl_latency (docs/echo-show-port-plan.md §4 risks).
const playPipelineNs = 40_000_000

// Engine is the process's dlopen'd libOpenSLES.so, its SLEngineItf and the
// one OutputMix every Player routes into.
type Engine struct {
	eng C.em_engine
}

var (
	openMu sync.Mutex
	engine *Engine
)

// Open returns the process's engine, loading libOpenSLES.so (or
// EM_OPENSL_LIB) on first use. A failed open is retried by the next call.
func Open() (*Engine, error) {
	openMu.Lock()
	defer openMu.Unlock()
	if engine != nil {
		return engine, nil
	}
	lib := os.Getenv("EM_OPENSL_LIB")
	if lib == "" {
		lib = defaultLib
	}
	cPath := C.CString(lib)
	defer C.free(unsafe.Pointer(cPath))
	e := &Engine{}
	if err := goErr(C.em_sl_open(cPath, &e.eng)); err != nil {
		return nil, fmt.Errorf("opensl: open %s: %w", lib, err)
	}
	engine = e
	return e, nil
}

// ─── shared callback registry ───────────────────────────────────────────────
//
// Every Recorder and Player registers under an int64 handle so the exported
// callbacks, invoked from a thread OpenSL ES owns, find the right Go object.
// One handle space for both, so a stray callback after Close can never be
// misattributed to an object that reused a small integer.

var (
	regMu     sync.Mutex
	nextID    atomic.Int64
	recorders = map[int64]*Recorder{}
	players   = map[int64]*Player{}
)

//export em_go_recorder_cb
func em_go_recorder_cb(ctx C.longlong) {
	regMu.Lock()
	r := recorders[int64(ctx)]
	regMu.Unlock()
	if r != nil {
		r.onComplete()
	}
}

//export em_go_player_cb
func em_go_player_cb(ctx C.longlong) {
	regMu.Lock()
	p := players[int64(ctx)]
	regMu.Unlock()
	if p != nil {
		p.onComplete()
	}
}

// ─── recorder ────────────────────────────────────────────────────────────────

// Recorder captures mono S16LE PCM through the VOICE_RECOGNITION preset in
// fixed periods, each stamped with CLOCK_MONOTONIC at its completion.
// ReadStamped is for one goroutine; Close may run on another.
type Recorder struct {
	rec      C.em_recorder
	handle   int64
	bufBytes int

	// inflight is the FIFO of slots enqueued with OpenSL ES, in enqueue
	// order: first the order in which slots are primed, then each re-enqueue
	// in onComplete. A simple buffer queue completes in enqueue order, so
	// popping it names the slot each completion filled without OpenSL ES
	// saying so.
	inflight chan int
	frames   chan frame
	free     chan []byte
	last     []byte // returned to free at the next read
	drops    atomic.Uint64

	failed   chan struct{} // closed when a re-enqueue failed: the queue is starving
	failErr  error         // written before failed is closed
	failOnce sync.Once

	timer     *time.Timer // ReadStamped's stall timer
	closeOnce sync.Once
}

// frame is one completed period and its CLOCK_MONOTONIC completion stamp.
type frame struct {
	pcm    []byte
	monoNs int64
}

// NewRecorder opens and starts a VOICE_RECOGNITION recorder at rateHz with
// nbuf OpenSL buffers of periodFrames mono samples each. The reader's queue
// holds nbuf × 4 periods; beyond that, periods drop (counted by Drops).
func (e *Engine) NewRecorder(rateHz, periodFrames, nbuf int) (*Recorder, error) {
	if periodFrames <= 0 {
		return nil, fmt.Errorf("opensl: NewRecorder: periodFrames must be positive, got %d", periodFrames)
	}
	if nbuf < 2 {
		return nil, fmt.Errorf("opensl: NewRecorder: nbuf must be at least 2, got %d", nbuf)
	}
	bufBytes := periodFrames * 2
	queueDepth := nbuf * 4
	r := &Recorder{
		bufBytes: bufBytes,
		inflight: make(chan int, nbuf),
		// The callback thread must never block: a full queue drops the
		// period (counted), and the completion stamps of the periods that
		// follow size the gap.
		frames: make(chan frame, queueDepth),
		free:   make(chan []byte, queueDepth),
		failed: make(chan struct{}),
		timer:  time.NewTimer(time.Hour),
	}
	r.timer.Stop()
	for range queueDepth {
		r.free <- make([]byte, bufBytes)
	}
	// em_recorder_open primes slots 0..nbuf-1 in order and starts
	// recording, so the FIFO must hold them before the first completion.
	for i := range nbuf {
		r.inflight <- i
	}
	r.handle = nextID.Add(1)
	regMu.Lock()
	recorders[r.handle] = r
	regMu.Unlock()

	err := goErr(C.em_recorder_open(&e.eng, C.int(rateHz), C.int(bufBytes), C.int(nbuf),
		C.longlong(r.handle), &r.rec))
	if err != nil {
		regMu.Lock()
		delete(recorders, r.handle)
		regMu.Unlock()
		return nil, fmt.Errorf("opensl: NewRecorder(VOICE_RECOGNITION, %d Hz): %w", rateHz, err)
	}
	return r, nil
}

// ReadStamped blocks for the next completed period, with the CLOCK_MONOTONIC
// ns at which OpenSL ES completed it. pcm is borrowed until the next read.
// It returns ErrStalled when no period completes within timeout, the
// re-enqueue error once the queue can no longer be fed, and ErrClosed once
// Close has run.
func (r *Recorder) ReadStamped(timeout time.Duration) (pcm []byte, monoNs int64, err error) {
	if r.last != nil {
		r.free <- r.last
		r.last = nil
	}
	select {
	case f, ok := <-r.frames:
		return r.take(f, ok)
	default:
	}
	r.timer.Reset(timeout)
	defer r.timer.Stop()
	select {
	case f, ok := <-r.frames:
		return r.take(f, ok)
	case <-r.failed:
		return nil, 0, r.failErr
	case <-r.timer.C:
		return nil, 0, ErrStalled
	}
}

func (r *Recorder) take(f frame, ok bool) ([]byte, int64, error) {
	if !ok {
		return nil, 0, ErrClosed
	}
	r.last = f.pcm
	return f.pcm, f.monoNs, nil
}

// Drops counts periods dropped because the reader fell behind.
func (r *Recorder) Drops() uint64 { return r.drops.Load() }

// Close stops capture and releases the recorder; a blocked ReadStamped
// returns ErrClosed. Safe to call more than once.
func (r *Recorder) Close() {
	r.closeOnce.Do(func() {
		regMu.Lock()
		delete(recorders, r.handle)
		regMu.Unlock()
		C.em_recorder_close(&r.rec) // waits out a callback in progress
		close(r.frames)
	})
}

// onComplete runs on the OpenSL ES callback thread each time a queued buffer
// has been filled. It must not block: stamp the completion, copy the buffer
// out to the reader (or drop it, counted), and re-enqueue the slot at once
// so the recorder always has somewhere to write.
func (r *Recorder) onComplete() {
	now := monoclock.Now()
	var idx int
	select {
	case idx = <-r.inflight:
	default:
		return // nothing enqueued: cannot happen while the FIFO mirrors OpenSL ES
	}
	select {
	case buf := <-r.free:
		ptr := C.em_recorder_bufptr(&r.rec, C.int(idx))
		copy(buf, unsafe.Slice((*byte)(ptr), r.bufBytes))
		select {
		case r.frames <- frame{pcm: buf, monoNs: now}:
		default:
			r.free <- buf
			r.drops.Add(1)
		}
	default:
		r.drops.Add(1)
	}
	if err := goErr(C.em_recorder_enqueue(&r.rec, C.int(idx))); err != nil {
		r.failOnce.Do(func() {
			r.failErr = fmt.Errorf("opensl: recorder: %w", err)
			close(r.failed)
		})
		return
	}
	select {
	case r.inflight <- idx:
	default:
	}
}

// ─── player ──────────────────────────────────────────────────────────────────

// Player renders mono S16LE PCM on STREAM_MUSIC through the engine's
// OutputMix, in buffers of one fixed size. Write blocks while every buffer
// is queued, so the OpenSL queue never holds more than nbuf of them and the
// caller is paced by playback. Each Write's completion fires, in write
// order, when OpenSL ES has consumed the buffer, carrying the buffer's
// estimated playout end on the capture stamp scale (playclock: the handoff,
// the buffers queued ahead of it and playPipelineNs).
type Player struct {
	play     C.em_player
	handle   int64
	bufBytes int

	free  chan int      // slots Write may fill
	done  chan struct{} // closed by Close
	timer *time.Timer   // Write's stall timer

	// mu serialises Write's enqueue and bookkeeping with completions, so a
	// completion can never pop a slot before Write has recorded it. OpenSL
	// ES calls the buffer-queue callback without holding its own lock, and
	// Close destroys the player outside mu, so mu never waits on OpenSL ES.
	mu       sync.Mutex
	inflight []pending // enqueued slots, in enqueue order
	clock    playclock.Clock
	closed   bool

	onCompleteFn atomic.Pointer[func(int64)]
}

type pending struct {
	idx    int
	doneNs int64 // estimated playout end
}

// NewPlayer opens a rateHz mono player with nbuf buffers of bufBytes and
// sets it playing (an empty queue plays nothing and fires nothing).
func (e *Engine) NewPlayer(rateHz, bufBytes, nbuf int) (*Player, error) {
	if bufBytes <= 0 || bufBytes%2 != 0 {
		return nil, fmt.Errorf("opensl: NewPlayer: bufBytes must be a positive whole number of samples, got %d", bufBytes)
	}
	if nbuf < 1 {
		return nil, fmt.Errorf("opensl: NewPlayer: nbuf must be positive, got %d", nbuf)
	}
	p := &Player{
		bufBytes: bufBytes,
		free:     make(chan int, nbuf),
		done:     make(chan struct{}),
		timer:    time.NewTimer(time.Hour),
		inflight: make([]pending, 0, nbuf),
		clock:    playclock.New(bufBytes, int64(rateHz)*2, playPipelineNs),
	}
	p.timer.Stop()
	for i := range nbuf {
		p.free <- i
	}
	p.handle = nextID.Add(1)
	regMu.Lock()
	players[p.handle] = p
	regMu.Unlock()

	err := goErr(C.em_player_open(&e.eng, C.int(rateHz), C.int(bufBytes), C.int(nbuf),
		C.longlong(p.handle), &p.play))
	if err != nil {
		regMu.Lock()
		delete(players, p.handle)
		regMu.Unlock()
		return nil, fmt.Errorf("opensl: NewPlayer(STREAM_MUSIC, %d Hz): %w", rateHz, err)
	}
	return p, nil
}

// SetOnComplete installs the per-buffer completion callback. It must be set
// before the first Write and must return immediately: it runs on the
// OpenSL ES callback thread.
func (p *Player) SetOnComplete(fn func(monoNs int64)) { p.onCompleteFn.Store(&fn) }

// Write copies data, exactly one buffer, into a free slot and enqueues it,
// waiting up to stall for a slot to free. It returns ErrStalled when none
// did (OpenSL ES stopped consuming: the player must be replaced) and
// ErrClosed once Close has run. data may be reused when Write returns.
func (p *Player) Write(data []byte, stall time.Duration) error {
	if len(data) != p.bufBytes {
		return fmt.Errorf("opensl: write of %d bytes, want %d", len(data), p.bufBytes)
	}
	var idx int
	select {
	case <-p.done:
		return ErrClosed
	case idx = <-p.free:
	default:
		p.timer.Reset(stall)
		select {
		case <-p.done:
			p.timer.Stop()
			return ErrClosed
		case idx = <-p.free:
			p.timer.Stop()
		case <-p.timer.C:
			return ErrStalled
		}
	}

	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed {
		return ErrClosed
	}
	C.memcpy(C.em_player_bufptr(&p.play, C.int(idx)), unsafe.Pointer(&data[0]), C.size_t(len(data)))
	ahead := len(p.inflight)
	if err := goErr(C.em_player_enqueue(&p.play, C.int(idx))); err != nil {
		p.free <- idx // not queued: the slot is still free
		return err
	}
	start := p.clock.Chunk(monoclock.Now(), ahead)
	p.inflight = append(p.inflight, pending{idx: idx, doneNs: p.clock.Completion(start, int64(len(data)))})
	return nil
}

// Close stops playback and releases the player, discarding what is queued;
// a blocked Write returns ErrClosed. Safe to call more than once.
func (p *Player) Close() {
	p.mu.Lock()
	if p.closed {
		p.mu.Unlock()
		return
	}
	p.closed = true
	p.mu.Unlock()
	close(p.done)
	regMu.Lock()
	delete(players, p.handle)
	regMu.Unlock()
	C.em_player_close(&p.play) // waits out a callback in progress
}

// onComplete runs on the OpenSL ES callback thread each time a queued buffer
// has been consumed: pop it (FIFO, as for the recorder), free its slot for
// Write, and report its estimated playout end.
func (p *Player) onComplete() {
	p.mu.Lock()
	if len(p.inflight) == 0 {
		p.mu.Unlock()
		return // nothing enqueued: cannot happen while inflight mirrors OpenSL ES
	}
	done := p.inflight[0]
	p.inflight = p.inflight[:copy(p.inflight, p.inflight[1:])]
	p.mu.Unlock()
	p.free <- done.idx // never blocks: free holds every slot not inflight
	if fn := p.onCompleteFn.Load(); fn != nil {
		(*fn)(done.doneNs)
	}
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
