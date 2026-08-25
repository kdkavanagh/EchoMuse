// Package config provides a shared, concurrency-safe device configuration
// that can be updated at runtime when the controller pushes a config message.
//
// Both the control client (OWW threshold) and the data client (VAD params)
// read from this struct so changes take effect immediately without a restart.
package config

import (
	"log"
	"os"
	"strconv"
	"strings"
	"sync"
)

// Device holds all runtime-tunable parameters for this device.
// Zero values are replaced by defaults on first access via Get().
type Device struct {
	mu sync.RWMutex

	// Microphone / VAD
	VadThreshold float64
	VadSpeechMs  int
	VadSilenceMs int

	// Speaker
	StartupVolume int

	// Wake word
	OwwThreshold float64
	OwwModel     string
	// BargeInEnabled / BargeInThreshold mirror the controller's barge-in
	// settings. The device needs them for on-device scoring: while the speaker
	// is streaming, the controller lowers its wake bar to BargeInThreshold
	// (echo at the mic is ~25dB louder than the person, so speech-over-TTS
	// scores are depressed). A device scoring against the normal threshold
	// during playback is not answering the same question, which made every
	// barge-in look like an on-device miss.
	BargeInEnabled   bool
	BargeInThreshold float64
	// DuckDb is how far MUSIC is attenuated while a voice turn plays over
	// it, in dB (negative = quieter). Config rather than a constant because
	// it is a taste parameter that needs iterating in a real room, the same
	// reasoning as the LED meter response curve — not something to discover
	// via a firmware OTA per attempt.
	DuckDb float64
	// OwwOnDevice selects on-device wake word scoring: "off", "shadow" or
	// "on".
	//
	// Shadow scores the wake stream locally and reports what it would have
	// detected, without acting on it, so device and controller can be
	// compared on the same audio. "on" additionally lets the device TRIGGER
	// the turn: the crossing is sent as an oww_wake message and the
	// controller starts the turn on the device's word rather than its own.
	//
	// The controller keeps scoring in "on" mode — its detections no longer
	// trigger, but they still record whether it agreed, so the comparison
	// that justified shipping this keeps running with the roles inverted.
	// It is also what keeps barge-in working unchanged, since that is
	// scored controller-side over the turn's own audio.
	OwwOnDevice string

	// Mic gain, beamforming, echo cancellation and AGC are NOT config here:
	// Android's audio HAL owns all four (per-mic AEC, fixed + adaptive
	// beamformer, SNR beam selection, its own AGC and output gain), selected
	// by capturing at AUDIO_SOURCE_VOICE_RECOGNITION. Its tuning lives in
	// /system/etc/AFE.cfg, on a read-only partition, so there is nothing here
	// for the controller to set — see docs/native-afe-migration.md.

	// BLE proxy (passive scan over /dev/stpbt, internal/bluetooth) —
	// pointer typed so false is expressible over the wire. Default off.
	BleProxyEnabled *bool

	initialised bool
}

var global = &Device{}

// Get returns the global device config, initialised from environment
// variables on first call.
func Get() *Device {
	global.mu.Lock()
	defer global.mu.Unlock()
	if !global.initialised {
		global.loadDefaults()
		global.initialised = true
	}
	return global
}

// loadDefaults populates from environment variables, falling back to
// hard-coded defaults. Must be called with mu held.
func (d *Device) loadDefaults() {
	d.VadThreshold = envFloat("VAD_THRESHOLD", 0.004)
	d.VadSpeechMs = envInt("VAD_SPEECH_MS", 80)
	d.VadSilenceMs = envInt("VAD_SILENCE_MS", 600)
	d.StartupVolume = envInt("STARTUP_VOLUME", 85)
	d.OwwThreshold = envFloat("OWW_THRESHOLD", 0.5)
	d.OwwModel = envStr("OWW_MODEL", "hey_jarvis_v0.1")
	d.OwwOnDevice = normaliseOnDevice(envStr("OWW_ON_DEVICE", OnDeviceOff))
	d.BargeInThreshold = envFloat("BARGE_IN_THRESHOLD", 0.05)
	d.DuckDb = envFloat("DUCK_DB", -18)
	bleProxyEnabled := envBool("BLE_PROXY_ENABLED", false)
	d.BleProxyEnabled = &bleProxyEnabled
}

// Apply updates the config from a controller-pushed config message.
// Only non-zero / non-empty values from the message are applied so that
// a partial config push doesn't zero out unmentioned fields.
func (d *Device) Apply(msg ConfigMessage) {
	d.mu.Lock()
	defer d.mu.Unlock()

	if !d.initialised {
		d.loadDefaults()
		d.initialised = true
	}

	if msg.VadThreshold > 0 {
		d.VadThreshold = msg.VadThreshold
	}
	if msg.VadSpeechMs > 0 {
		d.VadSpeechMs = msg.VadSpeechMs
	}
	if msg.VadSilenceMs > 0 {
		d.VadSilenceMs = msg.VadSilenceMs
	}
	if msg.OwwThreshold > 0 {
		d.OwwThreshold = msg.OwwThreshold
	}
	if msg.OwwModel != "" {
		d.OwwModel = msg.OwwModel
	}
	if msg.OwwOnDevice != "" {
		d.OwwOnDevice = normaliseOnDevice(msg.OwwOnDevice)
	}
	if msg.BargeInEnabled != nil {
		d.BargeInEnabled = *msg.BargeInEnabled
	}
	if msg.BargeInThreshold > 0 {
		d.BargeInThreshold = msg.BargeInThreshold
	}
	// Negative-going, so the usual "non-zero means set" rule is inverted:
	// a duck of 0dB is a legitimate setting ("do not duck at all") and must
	// be distinguishable from an absent field, hence the pointer.
	if msg.DuckDb != nil {
		d.DuckDb = *msg.DuckDb
	}
	if msg.StartupVolume > 0 {
		d.StartupVolume = msg.StartupVolume
	}
	if msg.BleProxyEnabled != nil {
		d.BleProxyEnabled = msg.BleProxyEnabled
	}
}

// Snapshot returns a consistent copy of all config values.
func (d *Device) Snapshot() ConfigMessage {
	d.mu.RLock()
	defer d.mu.RUnlock()
	// C4 fix (2026-07-05 review): copy every pointed-to value to a local,
	// never &d.Field — the caller dereferences it after RUnlock, racing with
	// Apply() writing the same field on a config push.
	bargeInEnabled := d.BargeInEnabled
	bleProxyEnabled := false
	if d.BleProxyEnabled != nil {
		bleProxyEnabled = *d.BleProxyEnabled
	}
	return ConfigMessage{
		VadThreshold:       d.VadThreshold,
		VadSpeechMs:        d.VadSpeechMs,
		VadSilenceMs:       d.VadSilenceMs,
		OwwThreshold:       d.OwwThreshold,
		OwwModel:           d.OwwModel,
		OwwOnDevice:        d.OwwOnDevice,
		BargeInEnabled:     &bargeInEnabled,
		BargeInThreshold:   d.BargeInThreshold,
		StartupVolume:      d.StartupVolume,
		BleProxyEnabled:    &bleProxyEnabled,
	}
}

// ConfigMessage mirrors the JSON shape of the config control message
// sent by the controller. JSON tags must match em_controller.py exactly.
type ConfigMessage struct {
	Type               string   `json:"type,omitempty"`
	StartupVolume      int      `json:"startupVolume,omitempty"`
	VadThreshold       float64  `json:"vadThreshold,omitempty"`
	VadSpeechMs        int      `json:"vadSpeechMs,omitempty"`
	VadSilenceMs       int      `json:"vadSilenceMs,omitempty"`
	OwwThreshold       float64  `json:"owwThreshold,omitempty"`
	OwwModel           string   `json:"owwModel,omitempty"`
	OwwOnDevice        string   `json:"owwOnDevice,omitempty"`
	BargeInEnabled     *bool    `json:"bargeInEnabled,omitempty"`
	BargeInThreshold   float64  `json:"bargeInThreshold,omitempty"`
	DuckDb             *float64 `json:"duckDb,omitempty"`
	BleProxyEnabled    *bool    `json:"bleProxyEnabled,omitempty"`
}

// On-device wake word modes.
const (
	OnDeviceOff    = "off"
	OnDeviceShadow = "shadow"
	OnDeviceOn     = "on"
)

// normaliseOnDevice maps a pushed value onto a known mode. Anything
// unrecognised becomes "off": a device receiving a mode it cannot honour must
// not guess, because the two plausible guesses are "score but do nothing" and
// "start triggering turns", and one of those is a live behaviour change on a
// device that cannot deliver it.
//
// That rule is why firmware predating "on" is safe to leave in the field: it
// normalises the value away and keeps scoring in shadow. The controller does
// not rely on that — it gates the setting on the oww_trigger capability — but
// the device must not depend on the controller being careful.
func normaliseOnDevice(v string) string {
	switch strings.ToLower(strings.TrimSpace(v)) {
	case OnDeviceShadow:
		return OnDeviceShadow
	case OnDeviceOn:
		return OnDeviceOn
	case "", OnDeviceOff:
		return OnDeviceOff
	default:
		log.Printf("[config] unknown owwOnDevice %q — treating as %q", v, OnDeviceOff)
		return OnDeviceOff
	}
}

// ─── env helpers ──────────────────────────────────────────────────────────────

func envInt(key string, def int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}

func envFloat(key string, def float64) float64 {
	if v := os.Getenv(key); v != "" {
		if f, err := strconv.ParseFloat(v, 64); err == nil {
			return f
		}
	}
	return def
}

func envBool(key string, def bool) bool {
	if v := os.Getenv(key); v != "" {
		return v == "1" || v == "true" || v == "True"
	}
	return def
}

func envStr(key string, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}
