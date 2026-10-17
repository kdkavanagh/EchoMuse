package latency

import (
	"math"
	"math/rand/v2"
	"time"

	"github.com/wilbowes/EchoMuse/internal/render"
)

// WriteNs is the playout duration of one render.SinkFrames write.
const WriteNs = int64(render.SinkFrames) * 1_000_000_000 / render.SampleRate

// Burst returns one render.SinkFrames write that starts with frames samples
// of w at levelDBFS peak (FreqHz for a Tone) and is silent after them, so
// the burst's first frame is the write's first frame.
func Burst(w Wave, freqHz, levelDBFS float64, frames int) []int16 {
	out := make([]int16, render.SinkFrames)
	amp := 32767 * math.Pow(10, levelDBFS/20)
	r := rand.New(rand.NewPCG(1, 2))
	for i := range min(frames, len(out)) {
		var v float64
		switch w {
		case Noise:
			v = amp * (2*r.Float64() - 1)
		default:
			v = amp * math.Sin(2*math.Pi*freqHz*float64(i)/render.SampleRate)
		}
		out[i] = int16(math.Round(max(-32768, min(32767, v))))
	}
	return out
}

// Step is one sink write of a run.
type Step struct {
	Burst       bool          // the write is the burst; otherwise silence
	StallBefore time.Duration // the writer pauses this long before it
}

// Plan lays a run out in render.SinkFrames writes:
//   - lead of silence, so capture, the AFE and the render clock fit settle;
//   - n bursts, each starting a write, gap apart;
//   - when stall > 0, one more burst gap after the last, written after the
//     writer has paused for stall so the OpenSL queue ran dry (the case the
//     render pipeline constant is a bound for);
//   - tail of silence, so the last burst's echo is captured.
func Plan(lead, gap time.Duration, n int, stall, tail time.Duration) []Step {
	writes := func(d time.Duration) int {
		return max(1, int((int64(d)+WriteNs-1)/WriteNs))
	}
	g := writes(gap)
	var out []Step
	for range writes(lead) {
		out = append(out, Step{})
	}
	burst := func(stallBefore time.Duration) {
		out = append(out, Step{Burst: true, StallBefore: stallBefore})
		for range g - 1 {
			out = append(out, Step{})
		}
	}
	for range n {
		burst(0)
	}
	if stall > 0 {
		burst(stall)
	}
	for range writes(tail) {
		out = append(out, Step{})
	}
	return out
}
