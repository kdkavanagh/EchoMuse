// Package server owns the device's physical controls: on the Dot the
// layered LED ring, ADC privacy mute, codec DAC volume, speaker amp and
// headphone routing; on the Show Android's STREAM_MUSIC volume
// (androidHardware) and nothing else. Audio epochs, mixer, focus, wake and
// alerts belong to internal/supervisor.
package server

import (
	"log"
	"sync"
	"time"

	internalled "github.com/wilbowes/EchoMuse/internal/bindings/led"
	"github.com/wilbowes/EchoMuse/pkg/led"
	"golang.org/x/sys/unix"
)

// defaultStatePath persists mute state across reboots and OTA restarts. It
// lives next to the TLS credentials in /data/local/etc — OTA slot flips only
// touch /data/local/bin, so the file survives them.
//
// Only mute lives here: mute is device-sovereign (the physical button cannot
// be overridden remotely, so the device must restore it itself, controller or
// no controller). Volume is the opposite — the controller's stored
// startupVolume is the source of truth, re-applied via SeedVolume on the
// first config push each run.
const defaultStatePath = "/data/local/etc/echomuse/state.json"

// Config wires the physical server. Nil Hardware selects Dot tinymix/GPIO;
// an empty StatePath selects the persistent device path. The zero value of
// the remaining fields is the Dot.
type Config struct {
	Hardware  Hardware
	StatePath string
	// NoPrivacyMute: the device has no privacy mute the firmware integrates
	// (the Show until Phase 7). Persisted mute is not restored, MuteToggle
	// does nothing and IsMuted stays false.
	NoPrivacyMute bool
	// VolumeAuthoritative: Hardware's level is live device state that
	// outlives this process (Android's STREAM_MUSIC index on the Show).
	// Volume starts seeded from it, so the controller's startupVolume never
	// overrides it and the first session reports it.
	VolumeAuthoritative bool
	// VolumeUnavailable, when set, is why volume control is disabled for
	// this run: no level is ever applied and no change is reported
	// (volume_state), and session.hello's volume level is null, which the
	// dashboard shows as volume unavailable.
	VolumeUnavailable string
}

// Server is safe for concurrent use.
type Server struct {
	hw Hardware

	ring   *ring
	volume *volumeController
	mute   *muteController

	mu             sync.Mutex
	headphones     bool
	alertAmpForced bool
}

// New builds the physical server and restores persisted privacy state. LED
// hardware is attached later by InitLEDs after the native boot sequence.
func New(cfg Config) *Server {
	hw := cfg.Hardware
	if hw == nil {
		hw = systemHardware{}
	}
	path := cfg.StatePath
	if path == "" {
		path = defaultStatePath
	}
	r := newRing()
	s := &Server{hw: hw, ring: r}
	s.volume = newVolumeController(hw, r, cfg.VolumeAuthoritative, cfg.VolumeUnavailable)
	s.mute = newMuteController(hw, r, path, !cfg.NoPrivacyMute)
	return s
}

// InitLEDs waits for the native LED boot sequence, claims the I2C ring, and
// paints the currently sovereign local state.
func (s *Server) InitLEDs() error {
	if uptime, err := getUptime(); err == nil && uptime < 5*time.Second {
		time.Sleep(5*time.Second - uptime)
	}
	c, err := internalled.NewDefaultController()
	if err != nil {
		return err
	}
	s.ring.SetController(c)
	if s.mute.isMuted() {
		_ = s.hw.SetMuteLED(true)
	}
	return nil
}

// SetLEDs applies a retained controller frame. Local privacy/alert/link/volume
// layers may hide it but retain it for hand-back.
func (s *Server) SetLEDs(values []led.Led) { s.ring.setPartial(values) }

// StartAnim starts the retained controller animation.
func (s *Server) StartAnim(spec AnimSpec) { s.ring.animate(LayerController, spec) }

// ClearControllerLEDs ends controller-owned dialog indication. Local layers
// (privacy, alert, disconnected/pending) remain (§11.2).
func (s *Server) ClearControllerLEDs() { s.ring.clear(LayerController) }

// LinkState is the device-local link indication.
type LinkState uint8

const (
	LinkUp      LinkState = iota // session established: no link layer
	LinkDown                     // no session: slow red pulse
	LinkPending                  // pending approval: slow white pulse
)

// SetLinkState paints or clears the device-local link layer.
func (s *Server) SetLinkState(state LinkState) {
	switch state {
	case LinkPending:
		s.ring.animate(LayerLink, AnimSpec{Pattern: PatternPulse, Colors: [][3]uint8{{80, 80, 80}}, PeriodMs: 2800})
	case LinkDown:
		s.ring.animate(LayerLink, AnimSpec{Pattern: PatternPulse, Colors: [][3]uint8{{180, 0, 0}}, PeriodMs: 4000})
	default:
		s.ring.clear(LayerLink)
	}
}

// SetAlertIndication projects the locally executing occurrence. Foreground
// pulses cyan above the controller and link layers, so it survives link loss.
// A background occurrence has yielded to a dialog, so its dim cyan sits under
// the controller layer: the dialog's listening, thinking and speaking ring
// shows over it, as on any other wake.
func (s *Server) SetAlertIndication(active, foreground bool) {
	switch {
	case !active:
		s.ring.clear(LayerAlert)
		s.ring.clear(LayerAlertBackground)
	case foreground:
		s.ring.animate(LayerAlert, AnimSpec{Pattern: PatternPulse, Colors: [][3]uint8{{0, 140, 220}}, PeriodMs: 900})
		s.ring.clear(LayerAlertBackground)
	default:
		// Painted before the foreground pulse goes, so the ring never shows
		// the layers beneath in between.
		s.ring.animate(LayerAlertBackground, AnimSpec{Pattern: PatternSolid, Colors: [][3]uint8{{0, 24, 38}}})
		s.ring.clear(LayerAlert)
	}
}

// SetAudioLevel drives controller meter animations from the final digital mix.
func (s *Server) SetAudioLevel(rms float64) { s.ring.setAudioLevel(rms) }

func (s *Server) VolumeStepUp()                        { s.volume.step(volumeStep) }
func (s *Server) VolumeStepDown()                      { s.volume.step(-volumeStep) }
func (s *Server) SetVolume(level int)                  { s.volume.setMedia(level, false) }
func (s *Server) SeedVolume(level int)                 { s.volume.seed(level) }
func (s *Server) CancelVolumeDisplay()                 { s.ring.cancelVolume() }
func (s *Server) VolumeState() (int, bool)             { return s.volume.state() }
func (s *Server) SetVolumeChangeCallback(cb func(int)) { s.volume.setCallback(cb) }

// ExternalVolumeChange records a volume change made outside the firmware
// and already in effect (the Show's volume keys or kiosk moving Android's
// index): reported like a button press, never written back.
func (s *Server) ExternalVolumeChange(level int) { s.volume.external(level) }

// VolumeUnavailable is why volume control is disabled for this run
// (Config.VolumeUnavailable), or "" when it works; session.hello then
// carries a null level.
func (s *Server) VolumeUnavailable() string { return s.volume.disabled }

func (s *Server) MuteToggle()                         { s.mute.toggle() }
func (s *Server) IsMuted() bool                       { return s.mute.isMuted() }
func (s *Server) SetMuteChangeCallback(cb func(bool)) { s.mute.setCallback(cb) }

// SetAlertAudio applies the occurrence DAC override and alert speaker route.
// A foreground alert forces the internal amp on even with headphones inserted
// and restores the insertion-driven amp state on release (§16.5).
func (s *Server) SetAlertAudio(active bool, id string, foreground bool, volume *float64) {
	s.volume.setAlert(active, id, foreground, volume)
	s.SetAlertIndication(active, foreground)

	s.mu.Lock()
	wantForce := active && foreground && s.headphones
	switch {
	case wantForce && !s.alertAmpForced:
		if err := s.hw.SetSpeakerAmp(true); err != nil {
			log.Printf("[speaker] force amp for alert: %v", err)
		}
		s.alertAmpForced = true
	case !wantForce && s.alertAmpForced:
		if err := s.hw.SetSpeakerAmp(!s.headphones); err != nil {
			log.Printf("[speaker] restore amp after alert: %v", err)
		}
		s.alertAmpForced = false
	}
	s.mu.Unlock()
}

// SetHeadphones records the accessory switch and routes the internal amp
// accordingly: off while a headphone is inserted (the kernel does not
// restore it on removal), except while a foreground alert forces it on.
func (s *Server) SetHeadphones(inserted bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.headphones = inserted
	if s.alertAmpForced {
		return
	}
	if err := s.hw.SetSpeakerAmp(!inserted); err != nil {
		log.Printf("[speaker] amp for headphones=%v: %v", inserted, err)
	}
}

// Close disables the amp after mixer shutdown.
func (s *Server) Close() error { return s.hw.SetSpeakerAmp(false) }

func getUptime() (time.Duration, error) {
	var info unix.Sysinfo_t
	if err := unix.Sysinfo(&info); err != nil {
		return 0, err
	}
	return time.Second * time.Duration(info.Uptime), nil
}
