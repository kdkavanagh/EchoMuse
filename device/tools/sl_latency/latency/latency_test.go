package latency

import (
	"math"
	"math/bits"
	"math/rand/v2"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
	"github.com/wilbowes/EchoMuse/internal/render"
)

const (
	ms     = int64(1_000_000)
	period = 1280
	t0     = int64(1_000_000) * ms // capture time of the first sample
)

// frame is one AFE frame to synthesise.
type frame struct {
	pa   bool
	erle uint8
}

// frameBits encodes a valid v3.3 frame, most significant bit first
// (afe.parse's layout).
func frameBits(counter uint8, tsMs uint16, f frame) (hi, lo uint64) {
	hi = 0xA563<<48 | 104<<41 | uint64(counter&0x3f)<<35 | uint64(tsMs)<<19 | uint64(f.erle)<<11
	if f.pa {
		lo |= 1 << 60
	}
	lo |= uint64(bits.OnesCount64(hi) + bits.OnesCount64(lo>>8))
	return hi, lo
}

// stream builds a capture of len(frames) 128-sample frames, the frames in
// bit 0 of noise of amplitude amp, and adds it period by period with
// contiguous stamps from t0.
func stream(frames []frame, amp int) (*Capture, []int16) {
	r := rand.New(rand.NewPCG(3, 3))
	pcm := make([]int16, len(frames)*afe.FrameBits)
	for k, f := range frames {
		hi, lo := frameBits(uint8(k), uint16(t0/ms+int64(k)*8), f)
		for i := range afe.FrameBits {
			var b uint64
			if i < 64 {
				b = hi >> (63 - i) & 1
			} else {
				b = lo >> (127 - i) & 1
			}
			v := int16(r.IntN(2*amp+1)-amp) &^ 1
			pcm[k*afe.FrameBits+i] = v | int16(b)
		}
	}
	return add(pcm), pcm
}

func add(pcm []int16) *Capture {
	c := &Capture{}
	for off := 0; off < len(pcm); off += period {
		end := min(off+period, len(pcm))
		c.Add(pcm[off:end], t0+int64(end)*SampleNs)
	}
	return c
}

func quiet(n int) []frame { return make([]frame, n) }

// Each decoded frame is placed at its first sample's capture time: frame k
// starts at sample 128k. The decoder locks on its second frame.
func TestFramesArePlacedOnTheStamps(t *testing.T) {
	fs := quiet(40)
	fs[25] = frame{pa: true, erle: 7}
	c, _ := stream(fs, 200)
	if len(c.Frames) != len(fs)-1 {
		t.Fatalf("%d frames decoded, want %d", len(c.Frames), len(fs)-1)
	}
	for i, f := range c.Frames {
		k := i + 1
		if want := t0 + int64(k*afe.FrameBits)*SampleNs; f.StartNs != want {
			t.Fatalf("frame %d starts %d, want %d", k, f.StartNs, want)
		}
		if f.Playback != fs[k].pa || f.ERLE != fs[k].erle {
			t.Fatalf("frame %d fields %+v", k, f)
		}
	}
}

func TestSampleTimesAndIndexFollowTheStamps(t *testing.T) {
	c := add(make([]int16, 3*period))
	// A dropped period: the next one is stamped two periods on.
	c.Add(make([]int16, period), t0+5*period*SampleNs)
	if got, want := c.SampleNs(period+5), t0+(period+5)*SampleNs; got != want {
		t.Fatalf("SampleNs = %d, want %d", got, want)
	}
	if got, want := c.SampleNs(3*period), t0+4*period*SampleNs; got != want {
		t.Fatalf("after the gap SampleNs = %d, want %d", got, want)
	}
	for _, tc := range []struct {
		ns   int64
		want int
	}{
		{t0 - ms, 0},
		{t0 + 7*SampleNs, 7},
		{t0 + 7*SampleNs + 1, 8},
		{t0 + 3*period*SampleNs + 1, 3 * period}, // inside the gap: the first sample after it
		{t0 + 9*period*SampleNs, 4 * period},
	} {
		if got := c.IndexAt(tc.ns); got != tc.want {
			t.Errorf("IndexAt(%d) = %d, want %d", tc.ns-t0, got, tc.want)
		}
	}
}

// burstAt writes a burst of l samples into pcm from sample at, keeping bit 0.
func burstAt(pcm []int16, at, l int, w Wave, amp float64) {
	r := rand.New(rand.NewPCG(5, 5))
	for i := range l {
		v := amp * math.Sin(2*math.Pi*1000*float64(i)/CaptureRate)
		if w == Noise {
			v = amp * (2*r.Float64() - 1)
		}
		pcm[at+i] = (pcm[at+i] + int16(v)) &^ 1
	}
}

// A residual burst well over the room is found to the sample, for a tone
// and for noise. The matched filter also finds a tone whose peak is under
// the room's RMS (its gain over a 20 ms window is 22 dB), to within a few
// ms since the noise moves the peak of its triangular response; one 40 dB
// under the room is not found.
func TestDetectPCM(t *testing.T) {
	const l = 320 // 20 ms
	for _, tc := range []struct {
		name      string
		wave      Wave
		amp, room float64
		found     bool
		tolNs     int64
	}{
		{"tone", Tone, 400, 20, true, 0},
		{"noise", Noise, 400, 20, true, 0},
		{"tone under the room", Tone, 8, 20, true, 2 * ms},
		{"buried tone", Tone, 4, 400, false, 0},
	} {
		t.Run(tc.name, func(t *testing.T) {
			_, pcm := stream(quiet(16000*4/afe.FrameBits), int(tc.room))
			at := 16000*2 + 1234 // 2.077 s in
			burstAt(pcm, at, l, tc.wave, tc.amp)
			c := add(pcm)
			ref := c.SampleNs(at) - 85*ms // the render stamp 85 ms early
			o := c.DetectPCM(ref, BurstSpec{Wave: tc.wave, FreqHz: 1000, Samples: l}, DefaultMinSNRdB)
			if o.Found != tc.found {
				t.Fatalf("found %v (%.1f dB), want %v", o.Found, o.SNRdB, tc.found)
			}
			if d := o.Ns - c.SampleNs(at); tc.found && max(d, -d) > tc.tolNs {
				t.Fatalf("onset %.3f ms from the burst", float64(d)/float64(ms))
			}
		})
	}
}

// A capture that does not reach past the search window finds nothing.
func TestDetectPCMNeedsTheWindow(t *testing.T) {
	c := add(make([]int16, 16000))
	if o := c.DetectPCM(t0+200*ms, BurstSpec{Wave: Tone, FreqHz: 1000, Samples: 320}, 10); o.Found {
		t.Fatalf("found %+v without a floor", o)
	}
}

// The rise is the first frame of the window after a frame without it; a
// window that opens inside a hangover waits for the next rise.
func TestFirstRise(t *testing.T) {
	fs := quiet(500)
	for k := 100; k < 180; k++ { // a hangover that runs into the window
		fs[k].pa = true
	}
	for k := 260; k < 300; k++ {
		fs[k].pa = true
	}
	fs[262].erle = 9
	c, _ := stream(fs, 20)
	frameNs := func(k int) int64 { return t0 + int64(k*afe.FrameBits)*SampleNs }
	ref := frameNs(150) + SearchBeforeNs - 5*ms // the window opens inside the hangover
	if o := c.FirstRise(ref, func(f Frame) bool { return f.Playback }); !o.Found || o.Ns != frameNs(260) {
		t.Fatalf("PA rise %+v, want frame 260 at %d", o, frameNs(260))
	}
	if o := c.FirstRise(ref, func(f Frame) bool { return f.ERLE > 0 }); !o.Found || o.Ns != frameNs(262) {
		t.Fatalf("ERLE rise %+v, want frame 262", o)
	}
	if o := c.FirstRise(frameNs(400), func(f Frame) bool { return f.Playback }); o.Found {
		t.Fatalf("rise found after the playback: %+v", o)
	}
}

func TestCompletionOffsetUnwrapsTheTimestamp(t *testing.T) {
	wrap := int64(9 << 16) // ms
	for _, tc := range []struct {
		doneNs int64
		a      afe.Anchor
		want   float64
	}{
		{(wrap+5)*ms + 300_000, afe.Anchor{Valid: true, TimestampMs: uint16((wrap - 13) % (1 << 16))}, 18.3},
		{(wrap+30)*ms + 0, afe.Anchor{Valid: true, TimestampMs: 10, After: 16}, 19},
	} {
		got, ok := CompletionOffsetMs(tc.doneNs, tc.a)
		if !ok || math.Abs(got-tc.want) > 1e-9 {
			t.Errorf("offset %v (%v), want %v", got, ok, tc.want)
		}
	}
	if _, ok := CompletionOffsetMs(wrap*ms, afe.Anchor{}); ok {
		t.Error("offset without a frame")
	}
}

func TestQuantiles(t *testing.T) {
	q := Quantiles([]float64{5, 1, 4, 2, 3}, 0, 0.5, 1)
	if q[0] != 1 || q[1] != 3 || q[2] != 5 {
		t.Fatalf("quantiles %v", q)
	}
}

func TestBurstWrite(t *testing.T) {
	b := Burst(Tone, 1000, -6, 960)
	if len(b) != render.SinkFrames {
		t.Fatalf("burst write of %d frames", len(b))
	}
	peak := 0
	for _, v := range b[:960] {
		peak = max(peak, int(v), -int(v))
	}
	if want := int(math.Round(32767 * math.Pow(10, -6.0/20))); peak != want {
		t.Fatalf("peak %d, want %d", peak, want)
	}
	for i, v := range b[960:] {
		if v != 0 {
			t.Fatalf("sample %d after the burst is %d", 960+i, v)
		}
	}
}

func TestPlan(t *testing.T) {
	steps := Plan(time.Second, 2500*time.Millisecond, 3, 2*time.Second, time.Second)
	lead, gap, tail := 24, 59, 24 // ceil(d / 42.67 ms)
	var at []int
	for i, s := range steps {
		if s.Burst {
			at = append(at, i)
		}
		if s.StallBefore != 0 && (!s.Burst || s.StallBefore != 2*time.Second) {
			t.Fatalf("step %d %+v", i, s)
		}
	}
	if want := []int{lead, lead + gap, lead + 2*gap, lead + 3*gap}; len(at) != 4 || at[0] != want[0] || at[3] != want[3] {
		t.Fatalf("bursts at %v, want %v", at, want)
	}
	if !steps[at[3]].Burst || steps[at[3]].StallBefore != 2*time.Second || steps[at[2]].StallBefore != 0 {
		t.Fatal("only the last burst follows the stall")
	}
	if len(steps) != lead+4*gap+tail {
		t.Fatalf("%d steps, want %d", len(steps), lead+4*gap+tail)
	}
}
