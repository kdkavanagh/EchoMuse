// Package cue plays short, device-local notification sounds — currently the
// wake-word confirmation chime.
//
// WHY THIS IS NOT AUDIO FROM THE CONTROLLER. A wake confirmation is only
// worth anything if it lands promptly: it is the device saying "I heard you"
// before the person has finished the sentence, and it competes with the ring
// lighting up rather than with the spoken reply. Streaming ~90KB of PCM for
// it would put a WiFi link with measured 1.1–2.6s RTT excursions between the
// wake and the feedback, on the plane that also carries the response, and it
// would spend that bandwidth again on every wake. The clip is fixed, it is
// tiny, and the device already has a mixer — so it ships inside the firmware
// and is played from memory. Only the *decision* to play it crosses the wire
// (a `wake_sound` control message, one small JSON object).
//
// That is deliberately the opposite call to em_sounds.py's timer ring, and
// for a reason that does not apply here: Home Assistant owns the timer
// countdown, so a ring cannot happen without the controller anyway and
// on-device storage would buy no autonomy. The wake word is detected either
// on the controller OR on the device (owwOnDevice), so the sound is the one
// part of a wake that never needs anything but the Echo.
//
// A cue is NOT a stream, and that distinction is what keeps it out of
// internal/bindings/speaker's audioStream machinery: there is no prime gate
// (the audio is already here in full), no discard-until-EOS (nothing is in
// flight to discard), no underrun (a drained cue has simply finished) and no
// StreamStats. Feeding it through the voice plane instead would have emitted
// a spurious playback_stats report, which the controller attaches to
// device.last_turn_id — corrupting the delivery instrumentation of whichever
// turn happened to be nearest — and would have made IsStreaming() true, which
// drops the on-device wake scorer to its barge-in threshold. Hence a third,
// deliberately dumb plane, mixed at the same write point as the other two.
package cue

import (
	_ "embed"
	"sync"
)

// wakeWordTriggered is the wake confirmation chime: 48kHz mono S16_LE, the
// device's native wire/output rate, so nothing resamples it and playing it
// costs a memcpy.
//
// Source: sounds/wake_word_triggered.flac from esphome/home-assistant-voice-pe
// (ESPHome, MIT — the ESPHome License puts everything but the C++/runtime
// sources under MIT). The .flac is kept beside this file as provenance and
// as the thing to re-decode from; see gen.sh for the exact ffmpeg command.
// Decoded at build authoring time rather than on the device because a FLAC
// decoder is a dependency this firmware has no other use for, and 89KB of
// raw PCM on a 10MB binary is not worth one.
//
// Peaks at −3.1dBFS, i.e. loud, as authored. Left at its original level:
// this plays over a barge-in's still-draining TTS at worst, and the mixer
// saturates rather than wraps.
//
//go:embed wake_word_triggered.pcm
var wakeWordTriggered []byte

// WakeWordTriggered returns the wake confirmation chime as 48kHz mono S16_LE.
//
// The slice aliases read-only embedded data — Player never hands it to a
// caller and never writes through it (see Next, which copies).
func WakeWordTriggered() []byte { return wakeWordTriggered }

// Player hands out one buffer of a cue at a time, for a speaker backend's
// pump loop to mix into the period it is about to write.
//
// Play is called from the control-plane goroutine and Next from the audio
// pump goroutine, so the two are guarded — a mutex rather than atomics
// because a cue swap is a compound change (buffer and position together),
// and the lock is taken ~23 times a second on the audio path at a cost of
// nothing measurable.
type Player struct {
	mu  sync.Mutex
	pcm []byte
	pos int
}

// Play starts (or restarts) a cue. Restarting is the intended behaviour for a
// second trigger: a wake confirmation that refused to interrupt its own tail
// would be silent for exactly the double-wake it should acknowledge.
//
// Empty PCM is a no-op rather than an error — a caller with nothing to play
// should be indistinguishable from one that did not call.
func (p *Player) Play(pcm []byte) {
	if len(pcm) == 0 {
		return
	}
	p.mu.Lock()
	p.pcm, p.pos = pcm, 0
	p.mu.Unlock()
}

// Stop abandons whatever is playing.
func (p *Player) Stop() {
	p.mu.Lock()
	p.pcm, p.pos = nil, 0
	p.mu.Unlock()
}

// Playing reports whether a cue still has audio to give.
func (p *Player) Playing() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.pcm != nil
}

// Next fills buf with the next slice of the playing cue and reports whether
// it wrote anything at all. False means nothing is playing and buf is
// untouched — the caller mixes nothing.
//
// The FINAL buffer is zero-padded rather than returned short, so a caller can
// always treat a true result as "here is a whole period". A short period
// handed to ALSA or to OpenSL ES is a different length of audio than the
// pacing loop expects, which is a way to make a clean one-second chime end
// with a click for no benefit.
func (p *Player) Next(buf []byte) bool {
	if len(buf) == 0 {
		return false
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.pcm == nil {
		return false
	}
	n := copy(buf, p.pcm[p.pos:])
	p.pos += n
	for i := n; i < len(buf); i++ {
		buf[i] = 0
	}
	if p.pos >= len(p.pcm) {
		p.pcm, p.pos = nil, 0
	}
	return true
}
