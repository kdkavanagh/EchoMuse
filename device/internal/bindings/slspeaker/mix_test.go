package slspeaker

import (
	"math"
	"testing"
)

// period builds a mono S16 period where every frame has the same value.
func period(frames int, v int16) []byte {
	b := make([]byte, frames*2)
	for i := 0; i < frames; i++ {
		off := i * 2
		b[off] = byte(uint16(v) & 0xff)
		b[off+1] = byte(uint16(v) >> 8)
	}
	return b
}

func sampleAt(b []byte, frame int) int16 {
	off := frame * 2
	return int16(uint16(b[off]) | uint16(b[off+1])<<8)
}

func TestDuckGainUnityIsExact(t *testing.T) {
	if g := DuckGain(0); g != unityGain {
		t.Fatalf("0dB should be exactly unity, got %d", g)
	}
	m := &Mixer{gain: unityGain}
	in := period(4, 12345)
	m.applyGain(in, unityGain)
	for i := 0; i < 4; i++ {
		if got := sampleAt(in, i); got != 12345 {
			t.Fatalf("unity gain altered the sample: %d", got)
		}
	}
}

func TestDuckGainDecibels(t *testing.T) {
	for _, tc := range []struct{ db, want float64 }{
		{-6, 0.5012}, {-18, 0.1259}, {-60, 0.001},
	} {
		got := float64(DuckGain(tc.db)) / float64(unityGain)
		if math.Abs(got-tc.want) > 0.001 {
			t.Errorf("%gdB: got %.4f want %.4f", tc.db, got, tc.want)
		}
	}
}

func TestVoiceAloneIsNeverAltered(t *testing.T) {
	m := &Mixer{gain: DuckGain(-18)}
	voice := period(4, 8000)
	out := m.Mix(voice, nil, DuckGain(-18))
	for i := 0; i < 4; i++ {
		if got := sampleAt(out, i); got != 8000 {
			t.Fatalf("voice was attenuated: %d", got)
		}
	}
}

func TestMusicAloneIsDucked(t *testing.T) {
	m := &Mixer{gain: DuckGain(-6)}
	music := period(4, 10000)
	out := m.Mix(nil, music, DuckGain(-6))
	got := sampleAt(out, 3)
	if got < 4900 || got > 5100 {
		t.Fatalf("expected ~5000 (-6dB of 10000), got %d", got)
	}
}

func TestNothingPlayingReturnsNil(t *testing.T) {
	m := &Mixer{gain: unityGain}
	if out := m.Mix(nil, nil, unityGain); out != nil {
		t.Fatal("with no audio the caller must write nothing, not a buffer")
	}
}

func TestTheRampSettlesEvenWithNoAudio(t *testing.T) {
	m := &Mixer{gain: unityGain}
	target := DuckGain(-18)
	for i := 0; i < 20; i++ {
		m.Mix(nil, nil, target)
	}
	if m.Gain() != target {
		t.Fatalf("ramp did not settle while idle: %d != %d", m.Gain(), target)
	}
}

func TestTheRampReachesItsTargetExactly(t *testing.T) {
	m := &Mixer{gain: unityGain}
	target := DuckGain(-18)
	for i := 0; i < 200; i++ {
		m.applyGain(period(2, 1000), target)
	}
	if m.Gain() != target {
		t.Fatalf("gain never reached target: %d != %d", m.Gain(), target)
	}
	for i := 0; i < 200; i++ {
		m.applyGain(period(2, 1000), unityGain)
	}
	if m.Gain() != unityGain {
		t.Fatalf("gain never returned to unity: %d", m.Gain())
	}
}

func TestTheRampIsGradualNotAStep(t *testing.T) {
	m := &Mixer{gain: unityGain}
	target := DuckGain(-18)
	first := period(64, 10000)
	m.applyGain(first, target)

	start := sampleAt(first, 0)
	end := sampleAt(first, 63)
	if start <= end {
		t.Fatalf("expected a descending ramp across the period, got %d -> %d", start, end)
	}
	if start < 9000 {
		t.Fatalf("the ramp should START near the previous gain, got %d", start)
	}
	if m.Gain() == target {
		t.Fatal("the whole duck landed in one period — that is the step this avoids")
	}
}

func TestMixSumsBothStreams(t *testing.T) {
	m := &Mixer{gain: unityGain}
	voice := period(4, 1000)
	music := period(4, 2000)
	out := m.Mix(voice, music, unityGain)
	if got := sampleAt(out, 3); got != 3000 {
		t.Fatalf("expected 1000+2000, got %d", got)
	}
}

func TestMixSaturatesRatherThanWrapping(t *testing.T) {
	m := &Mixer{gain: unityGain}
	voice := period(2, 30000)
	music := period(2, 30000)
	out := m.Mix(voice, music, unityGain)
	if got := sampleAt(out, 0); got != math.MaxInt16 {
		t.Fatalf("expected saturation to +32767, got %d", got)
	}

	m2 := &Mixer{gain: unityGain}
	out2 := m2.Mix(period(2, -30000), period(2, -30000), unityGain)
	if got := sampleAt(out2, 0); got != math.MinInt16 {
		t.Fatalf("expected saturation to -32768, got %d", got)
	}
}

func TestMixHandlesMismatchedLengths(t *testing.T) {
	m := &Mixer{gain: unityGain}
	out := m.Mix(period(8, 100), period(3, 100), unityGain)
	if len(out) != 16 {
		t.Fatalf("output should keep the voice period's length, got %d", len(out))
	}
	if got := sampleAt(out, 0); got != 200 {
		t.Fatalf("overlapping region should be summed, got %d", got)
	}
	if got := sampleAt(out, 7); got != 100 {
		t.Fatalf("the tail beyond the shorter buffer should be untouched, got %d", got)
	}
}

// ─── cue plane ────────────────────────────────────────────────────────────────

func TestCueAloneBecomesThePeriod(t *testing.T) {
	// A wake word with nothing else playing — Mix returns nil, and the cue
	// has to become the period rather than be dropped. Mono throughout on
	// this backend: OpenSL ES takes the wire format directly.
	cue := period(4, 5000)
	out := mixCue(nil, cue)
	if len(out) != len(cue) {
		t.Fatalf("cue period changed length: %d vs %d", len(out), len(cue))
	}
	if got := sampleAt(out, 2); got != 5000 {
		t.Fatalf("cue should play at its own level, got %d", got)
	}
}

func TestCueMixesOverWhateverIsPlaying(t *testing.T) {
	out := mixCue(period(4, 1000), period(4, 2000))
	if got := sampleAt(out, 0); got != 3000 {
		t.Fatalf("cue should sum with the playing audio, got %d", got)
	}
}

func TestCueIsNotDucked(t *testing.T) {
	// mixCue runs AFTER Mix, so no duck target reaches the cue.
	m := &Mixer{gain: DuckGain(-18)}
	out := m.Mix(nil, period(4, 8000), DuckGain(-18))
	out = mixCue(out, period(4, 8000))
	music := int32(sampleAt(out, 0)) - 8000
	if music >= 8000 {
		t.Fatalf("music should have been ducked under the cue, got %d", music)
	}
	if music == 0 {
		t.Fatal("the cue replaced the music instead of mixing with it")
	}
}

func TestCueSaturatesRatherThanWraps(t *testing.T) {
	out := mixCue(period(2, 30000), period(2, 30000))
	if got := sampleAt(out, 0); got != math.MaxInt16 {
		t.Fatalf("expected saturation to 32767, got %d", got)
	}
}
