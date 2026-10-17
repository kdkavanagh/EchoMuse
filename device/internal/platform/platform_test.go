package platform

import (
	"testing"

	"github.com/wilbowes/EchoMuse/internal/proto"
)

func fakeProbe(files []string, props map[string]string) probe {
	return probe{
		exists: func(path string) bool {
			for _, f := range files {
				if f == path {
					return true
				}
			}
			return false
		},
		getprop: func(key string) string { return props[key] },
	}
}

// The properties the images were measured with (docs/echo-show-crown.md §1,
// §8.5; docs/fireos6-port.md).
var (
	dotProps     = map[string]string{productKey: "biscuit_puffin", sdkKey: "25"}
	fireOSCrown  = map[string]string{productKey: "crown", sdkKey: "25"}
	lineageCrown = map[string]string{productKey: "crown", sdkKey: "30", lineageKey: "18.1-20261017-UNOFFICIAL-crown"}
)

// libmixerAPI.so is the Dot whatever else is present; a crown with
// ro.lineage.version is the LineageOS Show; the Fire OS Show needs crown,
// API 25 and OpenSL; anything else refuses to start.
func TestDetect(t *testing.T) {
	both := []string{openSLLib}
	for _, tc := range []struct {
		name  string
		files []string
		props map[string]string
		want  Kind
	}{
		{"dot", []string{mixerLib, openSLLib}, dotProps, KindDot},
		{"dot without props", []string{mixerLib}, nil, KindDot},
		{"fire os show", both, fireOSCrown, KindCrown},
		{"lineage show", both, lineageCrown, KindCrownLineage},
		{"lineage show without OpenSL", nil, lineageCrown, KindCrownLineage},
		// A LineageOS image must never reach the Fire OS path, whose
		// volume check would call setStreamVolume (code 10 on API 30).
		{"crown at API 30 without the lineage prop", both, map[string]string{productKey: "crown", sdkKey: "30"}, 0},
		{"fire os crown without OpenSL", nil, fireOSCrown, 0},
		{"crown without an SDK prop", both, map[string]string{productKey: "crown"}, 0},
		{"lineage on another product", both, map[string]string{productKey: "biscuit", sdkKey: "30", lineageKey: "18.1"}, 0},
		{"other OpenSL image", both, map[string]string{productKey: "biscuit", sdkKey: "25"}, 0},
		{"nothing", nil, nil, 0},
	} {
		got, err := detect(fakeProbe(tc.files, tc.props))
		if got != tc.want || (err == nil) != (tc.want != 0) {
			t.Errorf("%s: detect = %v, %v; want %v", tc.name, got, err, tc.want)
		}
	}
}

func TestKindPlatformAndScratch(t *testing.T) {
	for k, want := range map[Kind]proto.Platform{
		KindDot: proto.PlatformFireOS6, KindCrown: proto.PlatformFireOS6Crown, KindCrownLineage: proto.PlatformLineage18Crown,
	} {
		if k.Platform() != want {
			t.Errorf("%v.Platform() = %q, want %q", k, k.Platform(), want)
		}
	}
	if KindDot.ScratchDir() != "/tmp" || KindCrown.ScratchDir() != "/data/local/tmp/echomuse" ||
		KindCrownLineage.ScratchDir() != "/data/local/tmp/echomuse" {
		t.Errorf("scratch %q %q %q", KindDot.ScratchDir(), KindCrown.ScratchDir(), KindCrownLineage.ScratchDir())
	}
	if KindDot.Show() || !KindCrown.Show() || !KindCrownLineage.Show() {
		t.Error("Show() wrong")
	}
}

// Fire OS names its build in ro.build.version.name; LineageOS leaves it
// empty and is named from ro.lineage.version.
func TestOSVersion(t *testing.T) {
	for _, tc := range []struct {
		props map[string]string
		want  string
	}{
		{map[string]string{"ro.build.version.name": "Fire OS 6574.1 (NS65741/8146)"}, "Fire OS 6574.1 (NS65741/8146)"},
		{lineageCrown, "LineageOS 18.1-20261017-UNOFFICIAL-crown"},
		{nil, ""},
	} {
		if got := osVersionFrom(func(k string) string { return tc.props[k] }); got != tc.want {
			t.Errorf("osVersionFrom(%v) = %q, want %q", tc.props, got, tc.want)
		}
	}
}
