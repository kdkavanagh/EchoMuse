// Package config holds the controller-tunable device settings carried by the
// retained `config` message (WIRE §4.8): startupVolume, duckDb,
// bleProxyEnabled, wakeSound, for screen_wake devices screenWake,
// screenWakeUrl and screenWakePassword, for kiosk_plugin devices kioskToken
// and kioskPort, and for turn_view_v1 devices turnViewLinger. Wake model,
// runtime, thresholds and hop arrive in session.ready instead (SPEC §16.1,
// §18.2).
package config

import (
	"os"
	"strconv"
	"sync"
)

// Defaults, overridable by environment for bench builds.
const (
	DefaultStartupVolume = 85
	// DefaultDuckDB is duckDb (SPEC §6.2): dialog ducks content by this
	// much, and the provisional duck uses the same depth (§16.6).
	DefaultDuckDB = -18.0
	// DefaultScreenWakeURL is Fully Kiosk's local REST API on the Show.
	DefaultScreenWakeURL = "http://127.0.0.1:2323"
	// DefaultKioskPort is Kiosk Satellite's remote administration port.
	DefaultKioskPort = 2324
	// DefaultTurnViewLinger is how long, in seconds, the plugin's turn view
	// stays up after its turn is done (docs/echo-show-companion-plan.md
	// §11.7); MaxTurnViewLinger bounds it.
	DefaultTurnViewLinger = 6.0
	MaxTurnViewLinger     = 60.0
)

// Message is the `config` body. Absent fields leave the current value.
type Message struct {
	StartupVolume      *int     `json:"startupVolume,omitempty"`
	DuckDB             *float64 `json:"duckDb,omitempty"`
	BLEProxyEnabled    *bool    `json:"bleProxyEnabled,omitempty"`
	WakeSound          *bool    `json:"wakeSound,omitempty"`
	ScreenWake         *bool    `json:"screenWake,omitempty"`
	ScreenWakeURL      *string  `json:"screenWakeUrl,omitempty"`
	ScreenWakePassword *string  `json:"screenWakePassword,omitempty"`
	KioskToken         *string  `json:"kioskToken,omitempty"`
	KioskPort          *int     `json:"kioskPort,omitempty"`
	TurnViewLinger     *float64 `json:"turnViewLinger,omitempty"`
}

// Values is one consistent snapshot of the settings.
type Values struct {
	StartupVolume   int
	DuckDB          float64
	BLEProxyEnabled bool
	// WakeSound plays the wake chime locally the moment an idle wake
	// candidate opens (local_wake_chime).
	WakeSound bool
	// ScreenWake lights the kiosk screen through Fully Kiosk's REST API at
	// ScreenWakeURL when a wake candidate opens (screen_wake). Off by
	// default.
	ScreenWake         bool
	ScreenWakeURL      string
	ScreenWakePassword string
	// KioskToken and KioskPort reach Kiosk Satellite's remote API on
	// 127.0.0.1, where the firmware installs and enables its plugin
	// (kiosk_plugin; plan §11.2 decisions 2, 3). The token never leaves the
	// firmware and is never logged.
	KioskToken string
	KioskPort  int
	// TurnViewLinger is the seconds the plugin's turn view stays up after
	// done, 0 to hide at once (turn_view_v1; plan §11.2 decision 9).
	TurnViewLinger float64
}

// Device is the live configuration. It is safe for concurrent use.
type Device struct {
	mu sync.Mutex
	v  Values
}

// New returns the configuration seeded from the environment
// (STARTUP_VOLUME, DUCK_DB, BLE_PROXY_ENABLED, WAKE_SOUND, SCREEN_WAKE,
// SCREEN_WAKE_URL, SCREEN_WAKE_PASSWORD, KIOSK_TOKEN, KIOSK_PORT,
// TURN_VIEW_LINGER) or the defaults.
func New() *Device {
	return &Device{v: Values{
		StartupVolume:      envInt("STARTUP_VOLUME", DefaultStartupVolume),
		DuckDB:             envFloat("DUCK_DB", DefaultDuckDB),
		BLEProxyEnabled:    envBool("BLE_PROXY_ENABLED", false),
		WakeSound:          envBool("WAKE_SOUND", false),
		ScreenWake:         envBool("SCREEN_WAKE", false),
		ScreenWakeURL:      envString("SCREEN_WAKE_URL", DefaultScreenWakeURL),
		ScreenWakePassword: os.Getenv("SCREEN_WAKE_PASSWORD"),
		KioskToken:         os.Getenv("KIOSK_TOKEN"),
		KioskPort:          envInt("KIOSK_PORT", DefaultKioskPort),
		TurnViewLinger:     envFloat("TURN_VIEW_LINGER", DefaultTurnViewLinger),
	}}
}

// Apply merges a pushed message and returns the result. A startupVolume
// below zero is ignored; duckDb above zero would amplify and is clamped to 0;
// an empty screenWakeUrl restores the default; a kioskPort outside 1–65535
// is ignored; turnViewLinger is clamped to 0–MaxTurnViewLinger.
func (d *Device) Apply(m Message) Values {
	d.mu.Lock()
	defer d.mu.Unlock()
	if m.StartupVolume != nil && *m.StartupVolume >= 0 {
		d.v.StartupVolume = *m.StartupVolume
	}
	if m.DuckDB != nil {
		d.v.DuckDB = min(*m.DuckDB, 0)
	}
	if m.BLEProxyEnabled != nil {
		d.v.BLEProxyEnabled = *m.BLEProxyEnabled
	}
	if m.WakeSound != nil {
		d.v.WakeSound = *m.WakeSound
	}
	if m.ScreenWake != nil {
		d.v.ScreenWake = *m.ScreenWake
	}
	if m.ScreenWakeURL != nil {
		d.v.ScreenWakeURL = *m.ScreenWakeURL
		if d.v.ScreenWakeURL == "" {
			d.v.ScreenWakeURL = DefaultScreenWakeURL
		}
	}
	if m.ScreenWakePassword != nil {
		d.v.ScreenWakePassword = *m.ScreenWakePassword
	}
	if m.KioskToken != nil {
		d.v.KioskToken = *m.KioskToken
	}
	if m.KioskPort != nil && *m.KioskPort > 0 && *m.KioskPort <= 65535 {
		d.v.KioskPort = *m.KioskPort
	}
	if m.TurnViewLinger != nil {
		d.v.TurnViewLinger = min(max(*m.TurnViewLinger, 0), MaxTurnViewLinger)
	}
	return d.v
}

// Get returns the current values.
func (d *Device) Get() Values {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.v
}

func envInt(key string, def int) int {
	if n, err := strconv.Atoi(os.Getenv(key)); err == nil {
		return n
	}
	return def
}

func envFloat(key string, def float64) float64 {
	if f, err := strconv.ParseFloat(os.Getenv(key), 64); err == nil {
		return f
	}
	return def
}

func envBool(key string, def bool) bool {
	if b, err := strconv.ParseBool(os.Getenv(key)); err == nil {
		return b
	}
	return def
}

func envString(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}
