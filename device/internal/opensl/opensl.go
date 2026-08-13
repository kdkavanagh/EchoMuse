//go:build server

// Package opensl is a minimal Go binding to Android's OpenSL ES, used to
// reach the audio HAL's ASP front end (see docs/native-afe-migration.md) —
// tinyalsa's direct pcm23p/pcm24c writes never pass through AudioFlinger, so
// they never reach it.
//
// The library is dlopen'd at RUNTIME (see shim.h), not linked, so this
// package builds and the firmware boots on a device — or a plain Linux host
// running `go build ./...` without -tags server, which excludes this package
// entirely — where libOpenSLES.so was never resolved. Open just returns an
// error and the caller (slmic/slspeaker) falls back to the tinyalsa backends.
//
// Both Recorder and Player present a BLOCKING API — Read()/Write() — even
// though OpenSL ES itself is callback-driven (a buffer queue completion fires
// on a thread OpenSL ES owns, not one Go created). That asymmetry is
// deliberate: it lets slmic/slspeaker read almost like the tinyalsa backends
// they replace — a read loop, a pump loop — rather than every caller having
// to become callback-shaped. The channel-based bridging between the two
// models lives entirely in this package.
package opensl

/*
#cgo CFLAGS: -O2
#cgo LDFLAGS: -ldl

#include <string.h>
#include "shim.h"
*/
import "C"

import (
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
	"unsafe"
)

// ErrClosed is returned by Read/Write after the Recorder/Player (or the
// Engine that owns it) has been closed.
var ErrClosed = errors.New("opensl: closed")

// Preset selects the Android input_source via SL_ANDROID_KEY_RECORDING_PRESET,
// which is what picks the ASP pipeline the audio HAL runs — see
// docs/native-afe-migration.md's table. The mapping to the real
// SL_ANDROID_RECORDING_PRESET_* value is resolved in C (shim.h's em_preset)
// against the real header, not guessed here.
type Preset int

const (
	// PresetMic is AUDIO_SOURCE_MIC -> ASP pipeline 2, an empty algorithm
	// list (passthrough). Useful as the phase-0 CONTROL: any difference
	// between this and PresetVoiceRecognition is the AFE, not the codec path.
	PresetMic Preset = 0
	// PresetVoiceRecognition is AUDIO_SOURCE_VOICE_RECOGNITION -> ASP
	// pipeline 0 — per-mic AEC, fixed+adaptive beamformer, SNR beam
	// selection. This is the pipeline the whole migration is for.
	PresetVoiceRecognition Preset = 1
	// PresetVoiceCommunication is AUDIO_SOURCE_VOICE_COMMUNICATION -> ASP
	// pipeline 1 (VoIP-tuned AEC/RES/NR/CNG/AGC). Recorded for comparison
	// only — nothing in this migration is expected to use it.
	PresetVoiceCommunication Preset = 2
)

func (p Preset) String() string {
	switch p {
	case PresetMic:
		return "MIC"
	case PresetVoiceRecognition:
		return "VOICE_RECOGNITION"
	case PresetVoiceCommunication:
		return "VOICE_COMMUNICATION"
	default:
		return fmt.Sprintf("Preset(%d)", int(p))
	}
}

// Engine is a dlopen'd libOpenSLES.so instance, its SLEngineItf and a single
// shared OutputMix that every Player routes into.
//
// Opening the SAME path twice returns the same Engine rather than a second
// one — some Android versions only tolerate a single OpenSL ES engine per
// process, and the ort.Runtime precedent (same reasoning, different library)
// makes this the expected shape in this codebase.
type Engine struct {
	eng C.em_engine
}

var (
	openMu  sync.Mutex
	engines = map[string]*Engine{}
)

// Open loads libOpenSLES.so from libPath (an absolute path, or a soname to be
// resolved by the dynamic linker — "libOpenSLES.so" resolves against
// /system/lib on the device) and creates the engine and output mix.
func Open(libPath string) (*Engine, error) {
	openMu.Lock()
	defer openMu.Unlock()

	if e := engines[libPath]; e != nil {
		return e, nil
	}

	cPath := C.CString(libPath)
	defer C.free(unsafe.Pointer(cPath))

	e := &Engine{}
	if err := goErr(C.em_sl_open(cPath, &e.eng)); err != nil {
		return nil, fmt.Errorf("opensl: open %s: %w", libPath, err)
	}
	engines[libPath] = e
	return e, nil
}

// Close releases the engine and every object it owns. Not part of the normal
// server path — the engine lives for the process there — but used by
// afe_probe, which opens and closes across several recordings.
func (e *Engine) Close() {
	openMu.Lock()
	for path, eng := range engines {
		if eng == e {
			delete(engines, path)
		}
	}
	openMu.Unlock()
	C.em_engine_close(&e.eng)
}

// ─── shared callback registry ───────────────────────────────────────────────
//
// Every Recorder and Player registers under a small int64 handle so the
// exported callbacks below — invoked from a thread OpenSL ES owns, which may
// not be one the Go runtime has seen before — can find the right Go object.
// One registry for both rather than two: the handle space is shared, so a
// stray callback after Close can never be misattributed to an unrelated
// object that happened to reuse a small integer.

var (
	regMu     sync.Mutex
	nextID    int64
	recorders = map[int64]*Recorder{}
	players   = map[int64]*Player{}
)

func newHandle() int64 {
	return atomic.AddInt64(&nextID, 1)
}

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

// Recorder captures mono S16LE PCM at a fixed rate through a chosen
// input_source. Read blocks for the next completed period.
type Recorder struct {
	rec      C.em_recorder
	handle   int64
	bufBytes int

	// inflight is the FIFO of buffer slots currently enqueued with OpenSL
	// ES, in the exact order they were enqueued — priming and every
	// re-enqueue in onComplete push in that order, and OpenSL ES's simple
	// buffer queue completes callbacks in enqueue order (the one ordering
	// guarantee this whole design leans on), so popping this FIFO on each
	// completion always names the right slot without OpenSL ES having to
	// say so.
	inflight chan int
	frames   chan []byte
	drops    atomic.Uint64
	closed   atomic.Bool
}

// NewRecorder opens a recorder at rateHz through preset, with nbuf hardware
// buffers of periodFrames mono samples each.
//
// nbuf is the HAL-facing double/triple buffering only — small by design (2-4
// is typical). The application-level lead buffer (matching the tinyalsa
// backends' subscriber channel depth) belongs in the caller, same layering as
// PcmMicrophone's ALSA read loop vs. its per-subscriber channels.
func (e *Engine) NewRecorder(preset Preset, rateHz, periodFrames, nbuf int) (*Recorder, error) {
	if periodFrames <= 0 {
		return nil, fmt.Errorf("opensl: NewRecorder: periodFrames must be positive, got %d", periodFrames)
	}
	if nbuf < 2 {
		return nil, fmt.Errorf("opensl: NewRecorder: nbuf must be at least 2, got %d", nbuf)
	}

	bufBytes := periodFrames * 2 // mono S16LE
	r := &Recorder{
		bufBytes: bufBytes,
		inflight: make(chan int, nbuf),
		// Frame queue depth: a handful of periods of slack for Read()'s
		// caller to fall behind before frames start dropping — the OpenSL
		// ES callback thread must never block, so a full channel drops
		// rather than waits (mirrors PcmMicrophone's subscriber drop).
		frames: make(chan []byte, nbuf*4),
	}
	r.handle = newHandle()
	regMu.Lock()
	recorders[r.handle] = r
	regMu.Unlock()

	err := goErr(C.em_recorder_open(&e.eng, C.int(preset), C.int(rateHz),
		C.int(bufBytes), C.int(nbuf), C.longlong(r.handle), &r.rec))
	if err != nil {
		regMu.Lock()
		delete(recorders, r.handle)
		regMu.Unlock()
		return nil, fmt.Errorf("opensl: NewRecorder(%s, %dHz): %w", preset, rateHz, err)
	}
	if err := goErr(C.em_recorder_prime(&r.rec)); err != nil {
		r.Close()
		return nil, fmt.Errorf("opensl: prime recorder: %w", err)
	}
	for i := 0; i < nbuf; i++ {
		r.inflight <- i // mirrors em_recorder_prime's enqueue order exactly
	}
	return r, nil
}

// FrameSize is the byte length of every period Read returns.
func (r *Recorder) FrameSize() int { return r.bufBytes }

// Start begins recording. Buffers must already be primed (NewRecorder does
// this), or there is nothing for the hardware to fill.
func (r *Recorder) Start() error { return goErr(C.em_recorder_start(&r.rec)) }

// Stop halts recording. The recorder can be Start()ed again.
func (r *Recorder) Stop() error { return goErr(C.em_recorder_stop(&r.rec)) }

// Read blocks for the next completed period and returns a copy the caller
// owns. Returns ErrClosed once Close has run.
func (r *Recorder) Read() ([]byte, error) {
	buf, ok := <-r.frames
	if !ok {
		return nil, ErrClosed
	}
	return buf, nil
}

// Drops counts periods dropped because Read fell behind the audio thread —
// the OpenSL ES analogue of PcmMicrophone's sub_drops counter.
func (r *Recorder) Drops() uint64 { return r.drops.Load() }

// Close stops delivering frames and releases the recorder. Safe to call once;
// a second call is a no-op.
func (r *Recorder) Close() {
	if r.closed.Swap(true) {
		return
	}
	regMu.Lock()
	delete(recorders, r.handle)
	regMu.Unlock()
	C.em_recorder_close(&r.rec)
	close(r.frames)
}

// onComplete runs on the OpenSL ES callback thread every time one queued
// buffer has been filled. It must be fast and must not block: copy the
// finished buffer out, hand it to the reader (or drop it, counted, if the
// reader is behind), and re-enqueue the same slot immediately so the
// recorder is never starved of somewhere to write.
func (r *Recorder) onComplete() {
	var idx int
	select {
	case idx = <-r.inflight:
	default:
		// Nothing was inflight — should not happen (every completion pairs
		// with an earlier enqueue), but a closed/racing recorder must not
		// hang the audio thread waiting on an empty channel.
		return
	}

	ptr := C.em_recorder_bufptr(&r.rec, C.int(idx))
	buf := C.GoBytes(ptr, C.int(r.bufBytes))
	select {
	case r.frames <- buf:
	default:
		r.drops.Add(1)
	}

	// Best-effort: a failure here means the recorder is on its way down: the
	// object was already destroyed under us, or is about to be. There is no
	// caller to report it to from the audio thread, same reasoning as
	// echoTap/levelTap being fire-and-forget in the tinyalsa backend.
	_ = goErr(C.em_recorder_enqueue(&r.rec, C.int(idx)))
	select {
	case r.inflight <- idx:
	default:
	}
}

// ─── player ──────────────────────────────────────────────────────────────────

// Player renders mono S16LE PCM at a fixed rate through the shared output
// mix. Combined with a Recorder from the same Engine, both capture and
// playback route through AudioFlinger and the HAL — the "one rule that
// matters" in docs/native-afe-migration.md: without that, the ASP echo
// canceller has no far-end reference to cancel against.
type Player struct {
	play     C.em_player
	handle   int64
	bufBytes int

	free     chan int // hardware buffer slots available to Write into
	inflight chan int // slots enqueued, awaiting their completion callback (FIFO)
	closed   atomic.Bool
}

// NewPlayer opens a player at rateHz with nbuf hardware buffers, each able to
// hold up to maxBufBytes. Playback starts immediately (see shim.h's
// em_player_open for why); with nothing written yet, the queue is simply
// empty and no callback fires until the first Write.
func (e *Engine) NewPlayer(rateHz, maxBufBytes, nbuf int) (*Player, error) {
	if maxBufBytes <= 0 {
		return nil, fmt.Errorf("opensl: NewPlayer: maxBufBytes must be positive, got %d", maxBufBytes)
	}
	if nbuf < 2 {
		return nil, fmt.Errorf("opensl: NewPlayer: nbuf must be at least 2, got %d", nbuf)
	}

	p := &Player{
		bufBytes: maxBufBytes,
		free:     make(chan int, nbuf),
		inflight: make(chan int, nbuf),
	}
	p.handle = newHandle()
	regMu.Lock()
	players[p.handle] = p
	regMu.Unlock()

	err := goErr(C.em_player_open(&e.eng, C.int(rateHz), C.int(maxBufBytes),
		C.int(nbuf), C.longlong(p.handle), &p.play))
	if err != nil {
		regMu.Lock()
		delete(players, p.handle)
		regMu.Unlock()
		return nil, fmt.Errorf("opensl: NewPlayer(%dHz): %w", rateHz, err)
	}
	for i := 0; i < nbuf; i++ {
		p.free <- i // nothing queued yet — every slot starts free
	}
	return p, nil
}

// MaxFrameBytes is the largest period Write will accept.
func (p *Player) MaxFrameBytes() int { return p.bufBytes }

// Write copies data into the next free hardware buffer and enqueues it,
// blocking until a slot is free — the same backpressure contract
// PumpPeriod/PumpMusic give callers today (rate-limits the caller to playback
// speed). Returns ErrClosed once Close has run.
func (p *Player) Write(data []byte) error {
	if len(data) == 0 {
		return nil
	}
	if len(data) > p.bufBytes {
		return fmt.Errorf("opensl: period of %d bytes exceeds the %d-byte buffer", len(data), p.bufBytes)
	}
	idx, ok := <-p.free
	if !ok {
		return ErrClosed
	}
	ptr := C.em_player_bufptr(&p.play, C.int(idx))
	C.memcpy(ptr, unsafe.Pointer(&data[0]), C.size_t(len(data)))
	if err := goErr(C.em_player_enqueue(&p.play, C.int(idx), C.int(len(data)))); err != nil {
		p.free <- idx // failed to enqueue — return the slot rather than leak it
		return err
	}
	p.inflight <- idx
	return nil
}

// Clear drops everything queued but not yet played (barge-in / user stop).
// Buffers already handed to the HAL and mid-flight are unaffected — the
// OpenSL ES analogue of tinyalsa's Flush leaving up to PeriodCount periods
// still playing.
func (p *Player) Clear() error { return goErr(C.em_player_clear(&p.play)) }

// Close stops accepting writes and releases the player. Safe to call once; a
// second call is a no-op.
func (p *Player) Close() {
	if p.closed.Swap(true) {
		return
	}
	regMu.Lock()
	delete(players, p.handle)
	regMu.Unlock()
	C.em_player_close(&p.play)
	close(p.free)
}

// onComplete runs on the OpenSL ES callback thread every time one enqueued
// buffer has finished playing. It must be fast: pop the slot that completed
// (FIFO — see the Recorder.onComplete comment for why this is safe without
// OpenSL ES naming the slot) and return it to the free pool so a blocked
// Write can proceed.
func (p *Player) onComplete() {
	select {
	case idx := <-p.inflight:
		select {
		case p.free <- idx:
		default:
		}
	default:
		// Spurious callback with nothing inflight — see the matching comment
		// on Recorder.onComplete.
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
