package cue

import "testing"

// The embedded asset is the feature. A truncated or wrong-rate file is the
// failure mode that looks like working code: it plays, it is just wrong.
func TestWakeWordTriggeredIsWholeSamplesAtTheWireRate(t *testing.T) {
	pcm := WakeWordTriggered()
	if len(pcm) == 0 {
		t.Fatal("wake_word_triggered.pcm is empty — did go:embed pick up a stub?")
	}
	if len(pcm)%2 != 0 {
		t.Fatalf("odd byte count %d — S16_LE samples cannot be whole", len(pcm))
	}
	// 48kHz mono S16 = 96000 bytes/s. The source clip is ~0.95s; the bound
	// is on the ORDER, not the exact length, so re-authoring the sound does
	// not fail the test while a resample to 16kHz (or a stereo decode) does.
	secs := float64(len(pcm)) / (48000 * 2)
	if secs < 0.3 || secs > 3.0 {
		t.Fatalf("cue is %.2fs at 48kHz mono S16 — that is not a wake chime; "+
			"check gen.sh's -ar/-ac", secs)
	}
}

func TestNextReportsNothingWhenIdle(t *testing.T) {
	var p Player
	buf := make([]byte, 64)
	if p.Next(buf) {
		t.Fatal("an idle player claimed to have written audio")
	}
	if p.Playing() {
		t.Fatal("an idle player reports itself playing")
	}
}

func TestNextDeliversTheWholeCueThenStops(t *testing.T) {
	var p Player
	src := make([]byte, 100)
	for i := range src {
		src[i] = byte(i + 1) // non-zero, so padding is distinguishable
	}
	p.Play(src)

	buf := make([]byte, 30)
	var got []byte
	rounds := 0
	for p.Next(buf) {
		got = append(got, buf...)
		if rounds++; rounds > 10 {
			t.Fatal("player never finished — pos is not advancing")
		}
	}
	if rounds != 4 {
		t.Fatalf("expected 4 buffers for 100 bytes at 30 per call, got %d", rounds)
	}
	// 4 buffers of 30 = 120 bytes: the cue, then 20 bytes of zero padding.
	for i, b := range got[:len(src)] {
		if b != src[i] {
			t.Fatalf("byte %d: got %d, want %d", i, b, src[i])
		}
	}
	for i, b := range got[len(src):] {
		if b != 0 {
			t.Fatalf("tail byte %d is %d — the final buffer must be zero-padded, "+
				"not left holding the previous call's audio", i, b)
		}
	}
	if p.Playing() {
		t.Fatal("player still reports playing after delivering the whole cue")
	}
}

// A second wake must be acknowledged. Restart, not "already playing, ignore".
func TestPlayRestartsAnInFlightCue(t *testing.T) {
	var p Player
	src := []byte{1, 2, 3, 4, 5, 6, 7, 8}
	p.Play(src)

	buf := make([]byte, 4)
	p.Next(buf) // consumes 1..4
	p.Play(src)
	p.Next(buf)
	if buf[0] != 1 {
		t.Fatalf("restart resumed mid-cue (first byte %d) instead of starting over", buf[0])
	}
}

func TestPlayIgnoresAnEmptyCue(t *testing.T) {
	var p Player
	p.Play(nil)
	if p.Playing() {
		t.Fatal("playing nothing must be indistinguishable from not playing")
	}
}
