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

// session.hello names the image as the controller parses it: always
// "fireos6", always present on the wire.
func TestHelloPlatform(t *testing.T) {
	var m map[string]json.RawMessage
	b, err := json.Marshal(SessionHello{Platform: PlatformFireOS6})
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatal(err)
	}
	if got := string(m["platform"]); got != `"fireos6"` {
		t.Errorf("platform encodes as %s, want \"fireos6\"", got)
	}
}
