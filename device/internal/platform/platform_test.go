package platform

import (
	"testing"

	"github.com/wilbowes/EchoMuse/internal/proto"
)

func fakeProbe(files []string, product string) probe {
	return probe{
		exists: func(path string) bool {
			for _, f := range files {
				if f == path {
					return true
				}
			}
			return false
		},
		getprop: func(key string) string {
			if key == productKey {
				return product
			}
			return ""
		},
	}
}

// libmixerAPI.so is the Dot whatever else is present; the Show needs both
// the crown product and OpenSL; anything else refuses to start.
func TestDetect(t *testing.T) {
	for _, tc := range []struct {
		name    string
		files   []string
		product string
		want    Kind
	}{
		{"dot", []string{mixerLib, openSLLib}, "biscuit_puffin", KindDot},
		{"dot without product prop", []string{mixerLib}, "", KindDot},
		{"show", []string{openSLLib}, "crown", KindCrown},
		{"crown without OpenSL", nil, "crown", 0},
		{"other OpenSL image", []string{openSLLib}, "biscuit", 0},
		{"nothing", nil, "", 0},
	} {
		got, err := detect(fakeProbe(tc.files, tc.product))
		if got != tc.want || (err == nil) != (tc.want != 0) {
			t.Errorf("%s: detect = %v, %v; want %v", tc.name, got, err, tc.want)
		}
	}
}

func TestKindPlatformAndScratch(t *testing.T) {
	if KindDot.Platform() != proto.PlatformFireOS6 || KindCrown.Platform() != proto.PlatformFireOS6Crown {
		t.Errorf("platforms %q %q", KindDot.Platform(), KindCrown.Platform())
	}
	if KindDot.ScratchDir() != "/tmp" || KindCrown.ScratchDir() != "/data/local/tmp/echomuse" {
		t.Errorf("scratch %q %q", KindDot.ScratchDir(), KindCrown.ScratchDir())
	}
}
