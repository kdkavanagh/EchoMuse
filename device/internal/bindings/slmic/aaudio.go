//go:build server

package slmic

import (
	"errors"
	"fmt"
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/internal/aaudio"
	"github.com/wilbowes/EchoMuse/internal/audio/afe"
	"github.com/wilbowes/EchoMuse/internal/monoclock"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	aaudioRateHz = 16000

	// aaudioChunk bounds one blocking read, so Close never waits on a read
	// longer than this and a stall is noticed on time.
	aaudioChunk = 100 * time.Millisecond

	// aaudioStall is how long a running stream may deliver nothing before
	// it is taken for broken (audioserver restarted, the input was torn
	// down) and re-opened; 19 periods, as for OpenSL.
	aaudioStall = 1500 * time.Millisecond

	// aaudioFirstPeriod is the same bound for a fresh stream, whose first
	// read returns 250–255 ms after start (docs/echo-show-crown.md §8.4).
	aaudioFirstPeriod = 2 * time.Second

	// aaudioRetry paces re-open attempts that fail outright.
	aaudioRetry = 500 * time.Millisecond
)

// errStalled marks a stream that delivered nothing in time.
var errStalled = errors.New("no audio within the stall bound")

// AAudioMicrophone is the LineageOS Echo Show's capture stream: an AAudio
// input with the VOICE_RECOGNITION preset, which audioserver runs through
// ASP's ASR pipeline, so capture passes through the native AFE (SPEC §4.1)
// and, on our ROM's libasp, carries its v3.3 metadata in bit 0
// (docs/echo-show-lineage-plan.md §1). Read decodes that metadata into
// Block.AFE, as the other microphones do; the stamps come from AAudio's own
// capture timestamps (fitClock), the metadata being evidence only.
//
// A stream that fails a read or delivers nothing for aaudioStall
// (aaudioFirstPeriod when fresh) is closed and replaced, inside Read and for
// as long as it takes; both log lines are rate-limited to the 1st, 2nd,
// 4th, 8th … occurrence until audio flows again. The decoder runs on across
// the re-open; the frames lost are sized from the read completions either
// side of the gap and counted in Drops, and the stamps show the gap to
// capture.Timeline.
type AAudioMicrophone struct {
	lib  *aaudio.Lib
	done chan struct{} // closed by Close

	mu     sync.Mutex
	stream *aaudio.Stream // nil while re-opening
	closed bool

	lost  atomic.Uint64 // periods lost to re-opens
	xruns atomic.Uint64 // overruns of the streams already closed
	// curXruns is the current stream's overrun count as the capture
	// goroutine last read it, so Drops never waits on a blocked read.
	curXruns atomic.Uint64

	// The capture goroutine's own state.
	fresh       bool  // the stream has delivered nothing yet
	frames      int64 // frames read from the current stream
	reopens     int   // re-opens since the last delivered period
	openFails   int   // failed opens since the last delivered period
	openFailed  bool  // the last open attempt failed
	lastDone    int64 // read completion of the last period
	anchored    bool  // the last period's stamp came from the fit
	implausible uint64
	pcm         [periodFrames]int16
	dec         afe.Decoder
	meta        afe.Period
	clock       *fitClock
}

// OpenAAudio opens and starts the VOICE_RECOGNITION stream. It never
// touches audioserver or the PCMs: the HAL must own them for the AFE to run.
func OpenAAudio() (*AAudioMicrophone, error) {
	lib, err := aaudio.Open()
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	m := &AAudioMicrophone{lib: lib, done: make(chan struct{}), clock: newFitClock()}
	st, err := m.open()
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	log.Printf("slmic: AAudio VOICE_RECOGNITION input: %s", st.Describe())
	m.install(st)
	return m, nil
}

func (m *AAudioMicrophone) open() (*aaudio.Stream, error) {
	st, err := m.lib.OpenInput(aaudioRateHz, 1, aaudio.InputPresetVoiceRecognition)
	if err != nil {
		return nil, err
	}
	if err := st.Start(); err != nil {
		st.Close()
		return nil, err
	}
	return st, nil
}

// install makes st the current stream; the capture goroutine's frame count
// and fit start over with it.
func (m *AAudioMicrophone) install(st *aaudio.Stream) {
	m.stream = st
	m.curXruns.Store(0)
	m.fresh = true
	m.frames = 0
	m.clock.reset()
}

// Read blocks for the next 1,280-frame period, re-opening the stream as
// often as it takes. Block.PCM and Block.AFE are valid until the next Read.
// It returns an error only once Close has run.
func (m *AAudioMicrophone) Read() (pkgmic.Block, error) {
	for {
		st, err := m.current()
		if err != nil {
			return pkgmic.Block{}, err
		}
		if err := m.fill(st); err != nil {
			if m.isClosed() {
				return pkgmic.Block{}, fmt.Errorf("slmic: %w", aaudio.ErrClosed)
			}
			if m.reopens++; m.reopens&(m.reopens-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slmic: AAudio input: %v; re-opening (%d in a row)", err, m.reopens)
			}
			m.retire(st)
			continue
		}
		doneNs := monoclock.Now()
		m.fresh = false
		m.frames += periodFrames

		m.dec.Decode(m.pcm[:], &m.meta)
		if m.reopens > 0 {
			var lost int64
			if m.lastDone != 0 {
				lost = max((doneNs-m.lastDone+periodNs/2)/periodNs-1, 0)
				m.lost.Add(uint64(lost))
			}
			log.Printf("slmic: AAudio input carrying data again after %d re-open(s), %d period(s) lost", m.reopens, lost)
		}
		m.reopens, m.openFails = 0, 0
		m.lastDone = doneNs

		m.curXruns.Store(uint64(max(st.XRuns(), 0)))
		pos, tsNs, ok := st.Timestamp()
		ns, kind := m.clock.stamp(doneNs, m.frames, pos, tsNs, ok)
		m.anchored = kind == stampAnchored
		if kind == stampImplausible {
			m.implausible++
			if m.implausible&(m.implausible-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slmic: AAudio's fitted capture time lies more than %d ms before read completion; stamped at completion (%d so far)",
					maxFitLagNs/1_000_000, m.implausible)
			}
		}
		return pkgmic.Block{PCM: m.pcm[:], MonoNs: ns, AFE: &m.meta}, nil
	}
}

// fill reads one whole period into m.pcm in aaudioChunk reads, failing on
// a read error or when nothing arrives for the stall bound.
func (m *AAudioMicrophone) fill(st *aaudio.Stream) error {
	limit := aaudioStall
	if m.fresh {
		limit = aaudioFirstPeriod
	}
	got := 0
	last := time.Now()
	for got < periodFrames {
		n, err := st.Read(m.pcm[got:], aaudioChunk)
		if err != nil {
			return err
		}
		if n > 0 {
			got += n
			last = time.Now()
			continue
		}
		if time.Since(last) >= limit {
			return errStalled
		}
	}
	return nil
}

// current returns the open stream, opening a new one first if the last was
// retired. Failed opens are retried every aaudioRetry until one succeeds or
// Close runs.
func (m *AAudioMicrophone) current() (*aaudio.Stream, error) {
	for {
		m.mu.Lock()
		closed, st := m.closed, m.stream
		m.mu.Unlock()
		switch {
		case closed:
			return nil, fmt.Errorf("slmic: %w", aaudio.ErrClosed)
		case st != nil:
			return st, nil
		}
		if m.openFailed {
			select {
			case <-m.done:
				continue
			case <-time.After(aaudioRetry):
			}
		}
		st, err := m.open()
		if m.openFailed = err != nil; m.openFailed {
			if m.openFails++; m.openFails&(m.openFails-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slmic: re-open: %v (retrying every %v; %d failed since audio last flowed)", err, aaudioRetry, m.openFails)
			}
			continue
		}
		m.mu.Lock()
		if m.closed {
			m.mu.Unlock()
			st.Close()
			continue
		}
		m.install(st)
		m.mu.Unlock()
	}
}

// retire closes a stream that failed, keeping its overrun count; the next
// Read opens a replacement.
func (m *AAudioMicrophone) retire(st *aaudio.Stream) {
	xr := st.XRuns()
	st.Close()
	m.mu.Lock()
	if m.stream == st { // Read and Close may both retire it: count it once
		m.stream = nil
		m.curXruns.Store(0)
		m.xruns.Add(uint64(max(xr, 0)))
	}
	m.mu.Unlock()
}

func (m *AAudioMicrophone) isClosed() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.closed
}

// Completion reports, for the period the last Read returned, its read
// completion (CLOCK_MONOTONIC ns) and whether its Block.MonoNs came from
// AAudio's fitted timestamps (false: stamped at the completion).
// Diagnostics (device/tools/sl_latency); call it from the goroutine that
// calls Read.
func (m *AAudioMicrophone) Completion() (doneNs int64, anchored bool) {
	return m.lastDone, m.anchored
}

// Drops counts periods lost to re-opens (sized from the read completions
// either side of the gap) plus AAudio's overruns, each of which lost
// capture the reader did not take in time.
func (m *AAudioMicrophone) Drops() uint64 {
	return m.lost.Load() + m.xruns.Load() + m.curXruns.Load()
}

// Close stops capture; a blocked Read returns.
func (m *AAudioMicrophone) Close() {
	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		return
	}
	m.closed = true
	st := m.stream
	m.mu.Unlock()
	close(m.done)
	if st != nil {
		m.retire(st)
	}
}

var _ pkgmic.Microphone = (*AAudioMicrophone)(nil)
