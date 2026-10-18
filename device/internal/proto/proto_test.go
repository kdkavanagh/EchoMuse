package proto

import (
	"encoding/json"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/bindings/als"
	"github.com/wilbowes/EchoMuse/internal/bluetooth"
	"github.com/wilbowes/EchoMuse/internal/wifi"
)

// The retained reports were once built as ad-hoc maps; the controller parses
// these exact bytes (WIRE §4.8), including keys present with empty values.
func TestRetainedReportEncodings(t *testing.T) {
	for name, tc := range map[string]struct {
		body any
		want string
	}{
		"wifi_result ok":     {WifiResult{OK: true, SSID: "home"}, `{"error":"","ok":true,"ssid":"home"}`},
		"wifi_result failed": {WifiResult{SSID: "home", Error: "auth"}, `{"error":"auth","ok":false,"ssid":"home"}`},
		"wifi_scan_result": {WifiScanResult{Networks: []wifi.Network{{SSID: "a", Signal: -40}}},
			`{"error":"","networks":[{"ssid":"a","signal":-40}]}`},
		"wifi_scan_result failed": {WifiScanResult{Error: "busy"}, `{"error":"busy","networks":null}`},
		"ble_adverts": {BLEAdverts{Adverts: []bluetooth.Advert{{Addr: "AA:BB:CC:DD:EE:FF", AddrType: 1, Rssi: -60, Data: []byte{2, 1}}}},
			`{"adverts":[{"addr":"AA:BB:CC:DD:EE:FF","addrType":1,"rssi":-60,"data":"AgE="}]}`},
		"ambient_light": {AmbientLight{Lux: 12}, `{"lux":12}`},
		"log":           {Log{Level: LogInfo, Message: "[mem] x"}, `{"level":"info","message":"[mem] x"}`},
	} {
		got, err := json.Marshal(tc.body)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		if string(got) != tc.want {
			t.Errorf("%s:\n got %s\nwant %s", name, got, tc.want)
		}
	}
}

// session.hello's ambient_light_status is null until the supervisor has an
// als.Report, then that report's object.
func TestHelloAmbientLightStatus(t *testing.T) {
	status := func(h SessionHello) string {
		var m map[string]json.RawMessage
		b, err := json.Marshal(h)
		if err != nil {
			t.Fatal(err)
		}
		if err := json.Unmarshal(b, &m); err != nil {
			t.Fatal(err)
		}
		return string(m["ambient_light_status"])
	}
	if got := status(SessionHello{}); got != "null" {
		t.Errorf("absent status %s", got)
	}
	st := als.Status{Code: als.StatusNoChip, Detail: "none", Seen: []string{"x"}}
	if got := status(SessionHello{AmbientLightStatus: &st}); got != `{"code":"no_chip","detail":"none","seen":["x"]}` {
		t.Errorf("status %s", got)
	}
}

// session.hello names the image as the controller parses it: "fireos6" on
// the Dot, "fireos6_crown" on the Fire OS Show, "lineage18_crown" on the
// LineageOS Show, always present on the wire.
func TestHelloPlatform(t *testing.T) {
	for p, want := range map[Platform]string{
		PlatformFireOS6: `"fireos6"`, PlatformFireOS6Crown: `"fireos6_crown"`, PlatformLineage18Crown: `"lineage18_crown"`,
	} {
		var m map[string]json.RawMessage
		b, err := json.Marshal(SessionHello{Platform: p})
		if err != nil {
			t.Fatal(err)
		}
		if err := json.Unmarshal(b, &m); err != nil {
			t.Fatal(err)
		}
		if got := string(m["platform"]); got != want {
			t.Errorf("platform encodes as %s, want %s", got, want)
		}
	}
}

// The optional platform capabilities carry the names the controller gates
// its Wi-Fi, BLE proxy, mute, screen wake, companion, turn view, timer view
// and kiosk plugin controls on.
func TestPlatformCapabilityNames(t *testing.T) {
	for c, want := range map[Capability]string{
		CapWifiControl: "wifi_control", CapBLEProxy: "ble_proxy",
		CapPrivacyMute: "privacy_mute", CapScreenWake: "screen_wake",
		CapScreenOverlay: "screen_overlay", CapTurnView: "turn_view_v1", CapTimerView: "timer_view_v1",
		CapKioskPlugin: "kiosk_plugin",
	} {
		if string(c) != want {
			t.Errorf("capability %q, want %q", c, want)
		}
	}
}

// stats.companion is omitted off a screen_overlay device; on one, every
// unknown field is JSON null, never omitted.
func TestStatsCompanionEncoding(t *testing.T) {
	b, err := json.Marshal(Stats{})
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]json.RawMessage
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatal(err)
	}
	if _, ok := m["companion"]; ok {
		t.Errorf("companion present without health: %s", b)
	}
	linked := false
	b, err = json.Marshal(Stats{Companion: &CompanionStats{Linked: &linked, Kiosk: &KioskStats{Plugin: KioskPluginStats{Linked: &linked}}}})
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatal(err)
	}
	if got := string(m["companion"]); got != `{"version":null,"linked":false,"overlay":null,"error":null,`+
		`"kiosk":{"installed":null,"version":null,"remote_admin":null,`+
		`"plugin":{"version":null,"linked":false,"error":null,"switched_on":null}}}` {
		t.Errorf("companion encodes as %s", got)
	}
}

// hello volume.level is a number, or JSON null (never omitted) when the
// device cannot control volume this run.
func TestHelloVolumeEncoding(t *testing.T) {
	level := 85
	for _, tc := range []struct {
		v    Volume
		want string
	}{
		{Volume{Level: &level, Seeded: true}, `{"level":85,"seeded":true}`},
		{Volume{}, `{"level":null,"seeded":false}`},
	} {
		got, err := json.Marshal(tc.v)
		if err != nil {
			t.Fatal(err)
		}
		if string(got) != tc.want {
			t.Errorf("volume encodes as %s, want %s", got, tc.want)
		}
	}
}
