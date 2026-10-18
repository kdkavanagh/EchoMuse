package config

import (
	"encoding/json"
	"testing"
)

// screen_wake's keys arrive flat in the config message; absent keys keep
// their values, screen wake is off by default and an empty URL restores
// Fully Kiosk's local default.
func TestScreenWakeKeys(t *testing.T) {
	d := &Device{v: Values{ScreenWakeURL: DefaultScreenWakeURL}}
	if v := d.Get(); v.ScreenWake || v.ScreenWakeURL != "http://127.0.0.1:2323" || v.ScreenWakePassword != "" {
		t.Fatalf("defaults %+v", v)
	}
	apply := func(body string) Values {
		t.Helper()
		var m Message
		if err := json.Unmarshal([]byte(body), &m); err != nil {
			t.Fatal(err)
		}
		return d.Apply(m)
	}
	v := apply(`{"screenWake":true,"screenWakeUrl":"http://10.0.0.5:2323","screenWakePassword":"p&w"}`)
	if !v.ScreenWake || v.ScreenWakeURL != "http://10.0.0.5:2323" || v.ScreenWakePassword != "p&w" {
		t.Fatalf("applied %+v", v)
	}
	if v = apply(`{"wakeSound":true}`); !v.ScreenWake || v.ScreenWakeURL != "http://10.0.0.5:2323" {
		t.Fatalf("absent keys changed values: %+v", v)
	}
	if v = apply(`{"screenWake":false,"screenWakeUrl":""}`); v.ScreenWake || v.ScreenWakeURL != DefaultScreenWakeURL {
		t.Fatalf("reset %+v", v)
	}
}

// kiosk_plugin's and turn_view_v1's keys: defaults 2324 and 6 s; a token
// can be cleared; a port outside 1–65535 is ignored; the linger is clamped
// to 0–60 s and 0 is kept (hide at once).
func TestKioskKeys(t *testing.T) {
	t.Setenv("KIOSK_TOKEN", "")
	t.Setenv("KIOSK_PORT", "")
	t.Setenv("TURN_VIEW_LINGER", "")
	d := New()
	if v := d.Get(); v.KioskToken != "" || v.KioskPort != 2324 || v.TurnViewLinger != 6 {
		t.Fatalf("defaults %+v", v)
	}
	apply := func(body string) Values {
		t.Helper()
		var m Message
		if err := json.Unmarshal([]byte(body), &m); err != nil {
			t.Fatal(err)
		}
		return d.Apply(m)
	}
	if v := apply(`{"kioskToken":"tok","kioskPort":2400,"turnViewLinger":2.5}`); v.KioskToken != "tok" || v.KioskPort != 2400 || v.TurnViewLinger != 2.5 {
		t.Fatalf("applied %+v", v)
	}
	if v := apply(`{"kioskPort":0,"turnViewLinger":600}`); v.KioskToken != "tok" || v.KioskPort != 2400 || v.TurnViewLinger != 60 {
		t.Fatalf("bad port or long linger: %+v", v)
	}
	if v := apply(`{"kioskPort":70000,"turnViewLinger":0}`); v.KioskPort != 2400 || v.TurnViewLinger != 0 {
		t.Fatalf("port 70000 or linger 0: %+v", v)
	}
	if v := apply(`{"kioskToken":"","turnViewLinger":-1}`); v.KioskToken != "" || v.TurnViewLinger != 0 {
		t.Fatalf("cleared token or negative linger: %+v", v)
	}
}
