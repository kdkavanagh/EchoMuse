package cue

import (
	"errors"
	"testing"
)

// A resample or stereo decode still "plays", just wrong; bound the duration.
func TestWakeChimeIs48kHzMono(t *testing.T) {
	pcm, err := Asset(WakeChime)
	if err != nil {
		t.Fatal(err)
	}
	if secs := float64(len(pcm)) / 48000; secs < 0.3 || secs > 3 {
		t.Fatalf("wake chime is %.2f s at 48 kHz mono; check gen.sh", secs)
	}
	peak := 0
	for _, v := range pcm {
		peak = max(peak, int(v), -int(v))
	}
	if peak < 1000 {
		t.Fatalf("wake chime peak %d: decoded as silence", peak)
	}
}

func TestUnknownAsset(t *testing.T) {
	if _, err := Asset("builtin:fallback"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("err = %v", err)
	}
}
