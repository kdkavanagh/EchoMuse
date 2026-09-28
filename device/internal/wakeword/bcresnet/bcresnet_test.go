package bcresnet_test

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet/bcresnettest"
)

const deployedSidecarSHA = "25da0c652c562bf0a45a8f34bb46e38788bfc689e31401f33950831a0f2af51f"

func deployedSidecarBytes(t *testing.T) []byte {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join(bcresnettest.Dir(), "bcresnet_audio.json"))
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(raw)
	if got := hex.EncodeToString(sum[:]); got != deployedSidecarSHA {
		t.Fatalf("testdata sidecar sha256 %s is not the deployed sidecar", got)
	}
	return raw
}

func fixture(t *testing.T) *bcresnettest.Fixture {
	t.Helper()
	f, err := bcresnettest.Load()
	if err != nil {
		t.Fatalf("fixture: %v (regenerate with testdata/gen_fixture.py)", err)
	}
	if f.SidecarSHA256 != deployedSidecarSHA {
		t.Fatalf("fixture was generated from sidecar %s", f.SidecarSHA256)
	}
	return f
}

func TestDeployedSidecarParses(t *testing.T) {
	sc, err := bcresnet.ParseSidecar(deployedSidecarBytes(t))
	if err != nil {
		t.Fatal(err)
	}
	if sc.WakeIndex != 1 || sc.Labels[sc.WakeIndex] != "ohphelia" || sc.NormPeak != 0.8 {
		t.Fatalf("unexpected deployed sidecar %+v", sc)
	}
	if err := sc.CheckGraphIO(bcresnet.WindowSamples, 3); err != nil {
		t.Fatalf("deployed IO rejected: %v", err)
	}
}

func TestSidecarRejections(t *testing.T) {
	base := deployedSidecarBytes(t)
	cases := map[string]func(m map[string]any){
		"type":            func(m map[string]any) { m["type"] = "oww" },
		"missing type":    func(m map[string]any) { delete(m, "type") },
		"48 kHz":          func(m map[string]any) { m["sampleRate"] = 48000 },
		"window":          func(m map[string]any) { m["window"] = 22401 },
		"no labels":       func(m map[string]any) { m["labels"] = []string{} },
		"wakeIndex high":  func(m map[string]any) { m["wakeIndex"] = 3 },
		"wakeIndex neg":   func(m map[string]any) { m["wakeIndex"] = -1 },
		"normPeak zero":   func(m map[string]any) { m["normPeak"] = 0 },
		"normPeak over 1": func(m map[string]any) { m["normPeak"] = 1.5 },
	}
	for name, mutate := range cases {
		t.Run(name, func(t *testing.T) {
			var m map[string]any
			if err := json.Unmarshal(base, &m); err != nil {
				t.Fatal(err)
			}
			mutate(m)
			raw, _ := json.Marshal(m)
			if _, err := bcresnet.ParseSidecar(raw); err == nil {
				t.Fatalf("accepted %s", raw)
			}
		})
	}
	if _, err := bcresnet.ParseSidecar([]byte("{")); err == nil {
		t.Fatal("accepted invalid JSON")
	}
}

func TestGraphIOMustMatchSidecar(t *testing.T) {
	sc, err := bcresnet.ParseSidecar(deployedSidecarBytes(t))
	if err != nil {
		t.Fatal(err)
	}
	if err := sc.CheckGraphIO(bcresnet.WindowSamples, 2); err == nil {
		t.Error("accepted 2 outputs for 3 labels")
	}
	if err := sc.CheckGraphIO(16000, 3); err == nil {
		t.Error("accepted a 16,000-sample input")
	}
	if err := sc.CheckGraphIO(-1, 3); err == nil {
		t.Error("accepted a dynamic input length")
	}
}

// TestPrepareMatchesDeployedFixture checks SPEC §5.1 preparation against the
// Python reference on the fixture windows, including one below the RMS floor.
func TestPrepareMatchesDeployedFixture(t *testing.T) {
	f := fixture(t)
	dst := make([]float32, bcresnet.WindowSamples)
	for _, w := range f.Windows {
		t.Run(w.Name, func(t *testing.T) {
			got := bcresnet.Prepare(dst, w.PCM(), 0.8)
			if got != w.Scored {
				t.Fatalf("scored=%v, reference %v (rms %g)", got, w.Scored, w.RMS)
			}
			if !got {
				return
			}
			for k, want := range w.Probe {
				if v := dst[k*f.ProbeStride]; v != float32(want) {
					t.Fatalf("sample %d = %v, reference %v", k*f.ProbeStride, v, want)
				}
			}
			var sum, sumSq float64
			for _, v := range dst {
				sum += float64(v)
				sumSq += float64(v) * float64(v)
			}
			if math.Abs(sum-w.Sum) > 1e-6*math.Max(1, math.Abs(w.Sum)) || math.Abs(sumSq-w.SumSq) > 1e-6*w.SumSq {
				t.Fatalf("sum %v / %v, reference %v / %v", sum, sumSq, w.Sum, w.SumSq)
			}
		})
	}
}

func TestPrepareRMSFloorBoundary(t *testing.T) {
	dst := make([]float32, bcresnet.WindowSamples)
	pcm := make([]int16, bcresnet.WindowSamples)
	fill := func(mag int16) {
		for i := range pcm {
			pcm[i] = mag
			if i%2 == 1 {
				pcm[i] = -mag
			}
		}
	}
	fill(3) // RMS 9.2e-5
	if bcresnet.Prepare(dst, pcm, 0.8) {
		t.Error("scored a window with RMS below 1e-4")
	}
	fill(4) // RMS 1.2e-4
	if !bcresnet.Prepare(dst, pcm, 0.8) {
		t.Fatal("did not score a window with RMS above 1e-4")
	}
	if math.Abs(float64(dst[0])-0.8) > 1e-6 || math.Abs(float64(dst[1])+0.8) > 1e-6 {
		t.Errorf("not scaled to peak 0.8: %v %v", dst[0], dst[1])
	}
}

type fakeInferer struct {
	logits []float32
	err    error
	calls  int
}

func (f *fakeInferer) Run([]float32) ([]float32, error) { f.calls++; return f.logits, f.err }
func (f *fakeInferer) Close() error                     { return nil }

func deployedSidecar(t *testing.T) bcresnet.Sidecar {
	sc, err := bcresnet.ParseSidecar(deployedSidecarBytes(t))
	if err != nil {
		t.Fatal(err)
	}
	return sc
}

func TestScorerProbabilityFromDeployedLogits(t *testing.T) {
	f := fixture(t)
	for _, w := range f.Windows {
		inf := &fakeInferer{}
		for _, l := range w.Logits {
			inf.logits = append(inf.logits, float32(l))
		}
		s, err := bcresnet.NewScorer(inf, deployedSidecar(t))
		if err != nil {
			t.Fatal(err)
		}
		p, scored, err := s.Score(w.PCM())
		if err != nil {
			t.Fatalf("%s: %v", w.Name, err)
		}
		if scored != w.Scored {
			t.Fatalf("%s: scored=%v, reference %v", w.Name, scored, w.Scored)
		}
		if !scored {
			if inf.calls != 0 {
				t.Errorf("%s: graph ran on a window below the RMS floor", w.Name)
			}
			continue
		}
		if math.Abs(p-w.Prob) > 1e-7 {
			t.Errorf("%s: prob %v, reference %v", w.Name, p, w.Prob)
		}
	}
}

func TestScorerErrors(t *testing.T) {
	pcm := make([]int16, bcresnet.WindowSamples)
	for i := range pcm {
		pcm[i] = int16(1000 * (i%3 - 1))
	}
	cases := map[string][]float32{
		"nan":         {0, float32(math.NaN()), 0},
		"inf":         {float32(math.Inf(1)), 0, 0},
		"logit count": {0, 1},
	}
	for name, logits := range cases {
		s, err := bcresnet.NewScorer(&fakeInferer{logits: logits}, deployedSidecar(t))
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err := s.Score(pcm); err == nil {
			t.Errorf("%s: no error", name)
		}
	}
	s, _ := bcresnet.NewScorer(&fakeInferer{logits: []float32{0, 0, 0}}, deployedSidecar(t))
	if _, _, err := s.Score(pcm[:100]); err == nil || !strings.Contains(err.Error(), "window") {
		t.Errorf("short window: %v", err)
	}
}

func TestWakeProbabilityIsShiftInvariant(t *testing.T) {
	a := bcresnet.WakeProbability([]float32{0.5, 2, -1}, 1)
	b := bcresnet.WakeProbability([]float32{1000.5, 1002, 999}, 1)
	if math.Abs(a-b) > 1e-12 || math.IsNaN(b) {
		t.Fatalf("softmax not max-subtracted: %v vs %v", a, b)
	}
}
