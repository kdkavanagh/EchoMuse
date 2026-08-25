package bcresnet

import (
	"encoding/json"
	"math"
	"os"
	"testing"
)

// The two implementations score the same audio on the same device: the
// controller's em_wake_scorer.BcresnetScorer and this package. A divergence
// between them does not present as a bug in either — it presents as shadow
// mode reporting that the device disagrees with the controller, which reads as
// a model problem and sends someone looking in the wrong place.
//
// So the controller's scorer generates a fixture and this test reproduces it,
// the same discipline stream_fixture.bin applies to the openWakeWord pipeline.
// It needs no ONNX Runtime and no model: the graph is the same file at both
// ends, and what has to agree is the buffering, the normalisation, the softmax
// and the smoothing.
//
// Regenerate with testdata/gen_fixture.py.

type parityFixture struct {
	Window       int         `json:"window"`
	ChunkSamples int         `json:"chunkSamples"`
	HopChunks    int         `json:"hopChunks"`
	Smoothing    int         `json:"smoothing"`
	NormPeak     float64     `json:"normPeak"`
	WakeIndex    int         `json:"wakeIndex"`
	Labels       []string    `json:"labels"`
	LcgSeed      int64       `json:"lcgSeed"`
	Chunks       int         `json:"chunks"`
	Scores       []*float64  `json:"scores"`
	Windows      [][]float64 `json:"windows"`
}

// lcgAudio reproduces gen_fixture.py's generator exactly. An explicit LCG
// rather than a shipped waveform, so the fixture stays small and the input is
// provably the same on both sides.
func lcgAudio(n int, seed int64) []int16 {
	out := make([]int16, n)
	x := seed
	for i := range out {
		x = (1103515245*x + 12345) & 0x7FFFFFFF
		out[i] = int16((x>>8)%20001 - 10000)
	}
	return out
}

// replayInfer returns the same logits the fixture's recorder returned: they
// vary per call so the smoothing is exercised rather than being a constant.
type replayInfer struct {
	calls   int
	windows [][]float32
	nLabels int
}

func (r *replayInfer) Run(w []float32) ([]float32, error) {
	cp := make([]float32, len(w))
	copy(cp, w)
	r.windows = append(r.windows, cp)
	i := r.calls
	r.calls++
	return []float32{0.5, float32(1.0 + 0.5*float64(i)), -0.25}, nil
}

func TestMatchesTheControllersScorer(t *testing.T) {
	raw, err := os.ReadFile("testdata/parity_fixture.json")
	if err != nil {
		t.Fatalf("fixture: %v (regenerate with testdata/gen_fixture.py)", err)
	}
	var f parityFixture
	if err := json.Unmarshal(raw, &f); err != nil {
		t.Fatal(err)
	}

	spec := Spec{
		Type:       "bcresnet",
		SampleRate: 16000,
		Window:     f.Window,
		Labels:     f.Labels,
		WakeIndex:  f.WakeIndex,
		NormPeak:   f.NormPeak,
	}
	inf := &replayInfer{nLabels: len(f.Labels)}
	d, err := New(inf, spec, Options{HopChunks: f.HopChunks, Smoothing: f.Smoothing})
	if err != nil {
		t.Fatal(err)
	}

	audio := lcgAudio(f.Chunks*f.ChunkSamples, f.LcgSeed)
	for i := 0; i < f.Chunks; i++ {
		chunk := audio[i*f.ChunkSamples : (i+1)*f.ChunkSamples]
		score, ok, err := d.Push(chunk)
		if err != nil {
			t.Fatalf("chunk %d: %v", i, err)
		}

		want := f.Scores[i]
		if want == nil {
			if ok {
				// A hop that has drifted by one chunk still scores, and still
				// disagrees with the controller on every frame forever.
				t.Fatalf("chunk %d scored %v; the controller produced nothing", i, score)
			}
			continue
		}
		if !ok {
			t.Fatalf("chunk %d produced no score; the controller produced %v", i, *want)
		}
		// float32 against Python's float32, through a float64 JSON round trip.
		if diff := math.Abs(float64(score) - *want); diff > 1e-6 {
			t.Fatalf("chunk %d: score %v, controller %v (diff %g)", i, score, *want, diff)
		}
	}

	// The windows the model was actually shown. A buffering bug that keeps the
	// wrong 1.4s scores plausibly and is invisible in the final number.
	if len(inf.windows) != len(f.Windows) {
		t.Fatalf("scored %d windows, controller scored %d", len(inf.windows), len(f.Windows))
	}
	for w := range f.Windows {
		if len(inf.windows[w]) != len(f.Windows[w]) {
			t.Fatalf("window %d: %d samples, controller %d",
				w, len(inf.windows[w]), len(f.Windows[w]))
		}
		var worst float64
		for i := range f.Windows[w] {
			if d := math.Abs(float64(inf.windows[w][i]) - f.Windows[w][i]); d > worst {
				worst = d
			}
		}
		// Absolute, because the window is normalised to a known peak (0.8) so
		// its scale is fixed — the same reasoning as the openWakeWord fixture
		// scaling tolerance to the tensor rather than to each element, which
		// is meaningless for values straddling zero.
		if worst > 1e-6 {
			t.Fatalf("window %d diverges from the controller by %g", w, worst)
		}
	}
}

func TestTheFixtureWindowIsNotAWholeNumberOfChunks(t *testing.T) {
	// The real window is 22400 = 17.5 chunks. A fixture with an integral
	// window would pass against an implementation that assumed otherwise —
	// which is exactly the assumption that had to be removed from the
	// controller's own validation.
	raw, err := os.ReadFile("testdata/parity_fixture.json")
	if err != nil {
		t.Skip("no fixture")
	}
	var f parityFixture
	json.Unmarshal(raw, &f)
	if f.Window%f.ChunkSamples == 0 {
		t.Fatalf("fixture window %d is %d whole chunks — it cannot catch a "+
			"whole-chunk assumption", f.Window, f.Window/f.ChunkSamples)
	}
}
