//go:build server

// Package slmic captures through Android's audio HAL via OpenSL ES instead of
// writing tinyalsa PCM directly, so the capture reaches Amazon's ASP front
// end (per-mic AEC, fixed+adaptive beamformer, SNR beam selection) instead of
// bypassing it. See docs/native-afe-migration.md.
//
// It implements the same pkg/mic.Microphone (+ Subscribable) contract
// PcmMicrophone does, with one declared difference: Passthrough() reports
// true, because the HAL has already reduced the capture to one processed mono
// channel before this package ever sees it. internal/client's streamMic uses
// that to skip internal/beamformer entirely rather than feed it a buffer
// shaped for raw 9-channel capture (see pkg/mic.PassthroughReporter).
package slmic

import (
	"context"
	"errors"
	"fmt"
	"log"
	"os"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/opensl"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	// defaultLib resolves against /system/lib on the device — the same
	// library the plan's phase-0 spike confirmed present. Overridable for
	// bench/emulator use, same idea as EM_OWW_DIR.
	defaultLib = "libOpenSLES.so"

	// sampleRateHz/periodFrames match the wire format every consumer above
	// this package already expects: mono S16 16kHz in 80ms frames (see
	// CLAUDE.md's device audio pipeline section). Nothing downstream needs
	// to change to accept them.
	sampleRateHz = 16000
	periodFrames = 1280 // 80ms @ 16kHz

	// hwBuffers is the OpenSL ES-facing double/triple buffering only — small
	// by design. It is not the application-level lead buffer; that lives in
	// each subscriber's own channel, same layering as PcmMicrophone's ALSA
	// read loop vs. its per-subscriber fan-out.
	hwBuffers = 4

	// fanoutDepth matches PcmMicrophone.Subscribe's channel depth.
	fanoutDepth = 32
)

// Microphone captures mono S16 16kHz through OpenSL ES at the
// VOICE_RECOGNITION preset and fans it out to subscribers, mirroring
// internal/bindings/mic.PcmMicrophone's shape.
type Microphone struct {
	eng *opensl.Engine
	rec *opensl.Recorder

	mu   sync.Mutex
	subs []chan []byte
}

// NewMicrophone opens the OpenSL ES engine, a recorder at the
// VOICE_RECOGNITION preset, and starts capture — mirroring
// mic.PcmMicrophone.NewMicrophone's contract of returning a fully running
// microphone, so cmd/server.go's backend-selection switch can call either
// constructor identically. Critically, it runs no `stop mixer` / `stop
// media`: unlike the tinyalsa backends, this path needs mediaserver to keep
// owning the PCM, or the HAL's ASP front end is never in the loop at all
// (docs/native-afe-migration.md, "the one rule that matters").
func NewMicrophone() (*Microphone, error) {
	lib := os.Getenv("EM_OPENSL_LIB")
	if lib == "" {
		lib = defaultLib
	}
	eng, err := opensl.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	rec, err := eng.NewRecorder(opensl.PresetVoiceRecognition, sampleRateHz, periodFrames, hwBuffers)
	if err != nil {
		return nil, fmt.Errorf("slmic: %w", err)
	}
	m := &Microphone{eng: eng, rec: rec}
	if err := m.Init(); err != nil {
		rec.Close()
		return nil, fmt.Errorf("slmic: %w", err)
	}
	return m, nil
}

// Init starts capture and the permanent fan-out loop, mirroring
// PcmMicrophone.Init's shape so cmd/server.go's call site does not care which
// backend it holds.
func (m *Microphone) Init() error {
	if err := m.rec.Start(); err != nil {
		return fmt.Errorf("slmic: start: %w", err)
	}
	go m.readLoop()
	log.Printf("[slmic] recording via OpenSL ES at %s preset (%dHz, %dms periods) — "+
		"mediaserver keeps the PCM, no stop mixer/stop media on this path",
		opensl.PresetVoiceRecognition, sampleRateHz, periodFrames*1000/sampleRateHz)
	return nil
}

// readLoop reads completed periods forever and fans each out to every
// current subscriber, exactly like PcmMicrophone.readLoop — down to closing
// every subscriber channel on exit so callers see EOF rather than hang.
func (m *Microphone) readLoop() {
	var drops uint64
	for {
		buf, err := m.rec.Read()
		if err != nil {
			log.Printf("[slmic] recorder stream ended: %v", err)
			break
		}
		m.mu.Lock()
		for _, ch := range m.subs {
			select {
			case ch <- buf:
			default:
				drops++
				if drops == 1 || drops%64 == 0 {
					log.Printf("[slmic] subscriber channel full — period dropped (drops=%d)", drops)
				}
			}
		}
		m.mu.Unlock()
	}

	m.mu.Lock()
	log.Printf("[slmic] recorder stream closed — notifying %d subscribers", len(m.subs))
	for _, ch := range m.subs {
		close(ch)
	}
	m.subs = nil
	m.mu.Unlock()
}

// Subscribe registers a new subscriber and returns its channel.
func (m *Microphone) Subscribe() chan []byte {
	ch := make(chan []byte, fanoutDepth)
	m.mu.Lock()
	m.subs = append(m.subs, ch)
	m.mu.Unlock()
	return ch
}

// Unsubscribe removes a subscriber channel. Safe to call after readLoop has
// already closed it.
func (m *Microphone) Unsubscribe(ch chan []byte) {
	m.mu.Lock()
	defer m.mu.Unlock()
	for i, s := range m.subs {
		if s == ch {
			m.subs = append(m.subs[:i], m.subs[i+1:]...)
			close(ch)
			return
		}
	}
}

// Listen subscribes to the permanent stream and calls callback for each
// period until ctx is cancelled. Satisfies pkgmic.Microphone.
func (m *Microphone) Listen(callback pkgmic.AudioCallback, ctx context.Context) error {
	if callback == nil {
		return errors.New("callback can't be nil")
	}
	ch := m.Subscribe()
	defer m.Unsubscribe(ch)
	for {
		select {
		case <-ctx.Done():
			return nil
		case audio, ok := <-ch:
			if !ok {
				return nil
			}
			callback(audio)
		}
	}
}

// Passthrough reports that every fanned-out period is already one fully
// processed mono channel — see pkg/mic.PassthroughReporter.
func (m *Microphone) Passthrough() bool { return true }

// Close stops capture. The underlying opensl.Engine is process-shared
// (cached by library path) and outlives any one Microphone, so it is
// deliberately not closed here.
func (m *Microphone) Close() {
	_ = m.rec.Stop()
	m.rec.Close()
}

var (
	_ pkgmic.Microphone          = (*Microphone)(nil)
	_ pkgmic.Subscribable        = (*Microphone)(nil)
	_ pkgmic.PassthroughReporter = (*Microphone)(nil)
)
