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
