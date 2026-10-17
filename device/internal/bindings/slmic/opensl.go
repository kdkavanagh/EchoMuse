//go:build server

package slmic

import (
	"fmt"
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/opensl"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	openslRateHz = 16000

	// openslBuffers is the OpenSL buffer count, each one 1,280-sample
	// period: 1,280 is ten 128-sample AFE frames, and the metadata is
	// phase-locked to the stream start (docs/echo-show-crown.md §4.4), so
	// periods stay on the frame grid and each one ends with a frame.
	openslBuffers = 4

	// openslStall is how long a running recorder may complete nothing
	// before it is taken for broken (audioserver restarted, the input was
	// torn down) and re-opened; 19 periods.
	openslStall = 1500 * time.Millisecond

	// openslFirstPeriod is the same bound for a fresh recorder. When the
	// input is free the first buffer completes 197–206 ms after recording
	// starts (docs/echo-show-crown.md §4.4, §10). When another client
	// already records, AOSP 7.1 refuses our startInput, yet OpenSL reports
	// success and simply never completes a buffer, and the recorder never
	// recovers by itself; a recorder cycled every 2 s carried audio 200 ms
	// after the first cycle that followed the holder stopping (§10.2). So a
	// fresh recorder silent this long is replaced, for as long as it takes:
	// at most this plus ~200 ms of capture is lost after the other client
	// lets go.
	openslFirstPeriod = 2 * time.Second

	// openslRetry paces re-open attempts that fail outright.
	openslRetry = 500 * time.Millisecond

	periodNs = periodFrames * sampleNs
)

// OpenSLMicrophone is the Echo Show's capture stream: an OpenSL ES recorder
// with the VOICE_RECOGNITION preset, which audioserver runs through ASP's
// ASR pipeline, so capture passes through the native AFE (SPEC §4.1) and
// carries its v3.3 metadata in bit 0 (docs/echo-show-crown.md §4.3). Read
// decodes that metadata into Block.AFE, as MixerMicrophone does, and stamps
// each period from it (periodClock).
//
// A recorder that reports an error or completes nothing for openslStall
// (openslFirstPeriod when fresh) is destroyed and replaced, inside Read and
// for as long as it takes; both log lines are rate-limited to the 1st, 2nd,
// 4th, 8th … occurrence until audio flows again. The decoder runs on across
// the re-open: a fresh recorder starts on a period boundary and so on the
// frame grid, and its first frame's counter and timestamp report the frames
// lost (as for the Dot's micAsr re-open,
// afe.TestReopenKeepsLockAndReportsTheGap). The stamps show the gap to
// capture.Timeline, which records it as a missing range.
type OpenSLMicrophone struct {
	eng  *opensl.Engine
	done chan struct{} // closed by Close

	mu      sync.Mutex
	rec     *opensl.Recorder // nil while re-opening
	closed  bool
	retired uint64 // queue drops of the recorders already closed

	lost atomic.Uint64 // periods lost to re-opens

	// The capture goroutine's own state.
	fresh       bool // rec has delivered nothing yet
	reopens     int  // re-opens since the last delivered period
	openFails   int  // failed opens since the last delivered period
	openFailed  bool // the last open attempt failed
	lastDone    int64
	anchored    bool // the last period's stamp came from its AFE frame
	implausible uint64
	pcm         [periodFrames]int16
	dec         afe.Decoder
	meta        afe.Period
	clock       periodClock
}

// OpenOpenSL opens and starts the VOICE_RECOGNITION recorder. It never
// touches audioserver or the PCMs: the HAL must own them for the AFE to run.
func OpenOpenSL() (*OpenSLMicrophone, error) {
	eng, err := opensl.Open()
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	rec, err := eng.NewRecorder(openslRateHz, periodFrames, openslBuffers)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	return &OpenSLMicrophone{eng: eng, done: make(chan struct{}), rec: rec, fresh: true}, nil
}

// Read blocks for the next completed period, re-opening the recorder as
// often as it takes. Block.PCM and Block.AFE are valid until the next Read.
// It returns an error only once Close has run.
func (m *OpenSLMicrophone) Read() (pkgmic.Block, error) {
	for {
		rec, err := m.recorder()
		if err != nil {
			return pkgmic.Block{}, err
		}
		timeout := openslStall
		if m.fresh {
			timeout = openslFirstPeriod
		}
		buf, doneNs, err := rec.ReadStamped(timeout)
		if err != nil {
			if m.isClosed() {
				return pkgmic.Block{}, fmt.Errorf("slmic: %w", opensl.ErrClosed)
			}
			if m.reopens++; m.reopens&(m.reopens-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slmic: OpenSL recorder: %v; re-opening (%d in a row)", err, m.reopens)
			}
			m.retire(rec)
			continue
		}
		m.fresh = false

		pcm := m.pcm[:len(buf)/2]
		ema.PCM(pcm, buf)
		m.dec.Decode(pcm, &m.meta)
		if m.reopens > 0 {
			var lost int64
			if m.lastDone != 0 {
				lost = max((doneNs-m.lastDone+periodNs/2)/periodNs-1, 0)
				m.lost.Add(uint64(lost))
			}
			log.Printf("slmic: OpenSL recorder carrying data again after %d re-open(s), %d period(s) lost", m.reopens, lost)
		}
		m.reopens, m.openFails = 0, 0
		m.lastDone = doneNs

		ns, kind := m.clock.stamp(doneNs, m.meta.Anchor)
		m.anchored = kind == stampAnchored
		if kind == stampImplausible {
			m.implausible++
			if m.implausible&(m.implausible-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slmic: AFE_TIMESTAMP %d is not within %d ms before read completion (%d ms mod 65,536); stamped at completion (%d so far)",
					m.meta.Anchor.TimestampMs, maxAnchorLagMs, (doneNs/1_000_000)%afeTimestampMod, m.implausible)
			}
		}
		return pkgmic.Block{PCM: pcm, MonoNs: ns, AFE: &m.meta}, nil
	}
}

// Completion reports, for the period the last Read returned, its read
// completion (CLOCK_MONOTONIC ns of the OpenSL callback) and whether its
// Block.MonoNs came from the period's AFE frame (false: stamped at the
// completion). Diagnostics (device/tools/sl_latency); call it from the
// goroutine that calls Read.
func (m *OpenSLMicrophone) Completion() (doneNs int64, anchored bool) {
	return m.lastDone, m.anchored
}

// recorder returns the open recorder, opening a new one first if the last
// was retired. Failed opens are retried every openslRetry until one
// succeeds or Close runs.
func (m *OpenSLMicrophone) recorder() (*opensl.Recorder, error) {
	for {
		m.mu.Lock()
		closed, rec := m.closed, m.rec
		m.mu.Unlock()
		switch {
		case closed:
			return nil, fmt.Errorf("slmic: %w", opensl.ErrClosed)
		case rec != nil:
			return rec, nil
		}
		if m.openFailed {
			select {
			case <-m.done:
				continue
			case <-time.After(openslRetry):
			}
		}
		rec, err := m.eng.NewRecorder(openslRateHz, periodFrames, openslBuffers)
		if m.openFailed = err != nil; m.openFailed {
			if m.openFails++; m.openFails&(m.openFails-1) == 0 { // 1st, 2nd, 4th, 8th, …
				log.Printf("slmic: re-open: %v (retrying every %v; %d failed since audio last flowed)", err, openslRetry, m.openFails)
			}
			continue
		}
		m.mu.Lock()
		if m.closed {
			m.mu.Unlock()
			rec.Close()
			continue
		}
		m.rec = rec
		m.mu.Unlock()
		m.fresh = true
	}
}

// retire closes a recorder that failed, keeping its drop count; the next
// Read opens a replacement.
func (m *OpenSLMicrophone) retire(rec *opensl.Recorder) {
	rec.Close()
	m.mu.Lock()
	if m.rec == rec { // Read and Close may both retire it: count it once
		m.rec = nil
		m.retired += rec.Drops()
	}
	m.mu.Unlock()
}

func (m *OpenSLMicrophone) isClosed() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.closed
}

// Drops counts periods dropped because Read fell behind, across every
// recorder, plus the periods lost to re-opens (sized from the read
// completions either side of the gap).
func (m *OpenSLMicrophone) Drops() uint64 {
	m.mu.Lock()
	n := m.retired
	if m.rec != nil {
		n += m.rec.Drops()
	}
	m.mu.Unlock()
	return n + m.lost.Load()
}

// Close stops capture; a blocked Read returns. The opensl.Engine is
// process-shared and stays open.
func (m *OpenSLMicrophone) Close() {
	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		return
	}
	m.closed = true
	rec := m.rec
	m.mu.Unlock()
	close(m.done)
	if rec != nil {
		m.retire(rec)
	}
}

var _ pkgmic.Microphone = (*OpenSLMicrophone)(nil)
