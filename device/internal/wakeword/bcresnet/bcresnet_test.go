package bcresnet

import (
	"errors"
	"math"
	"os"
	"path/filepath"
	"testing"
)

// A small spec with a 4-chunk window, so a test can fill it in four Pushes
// rather than seventeen. The real model's 22400 is not a whole number of
// chunks; nothing here may assume it is.
func testSpec() Spec {
	return Spec{
		Type:        "bcresnet",
		SampleRate:  16000,
		Window:      4 * 1280,
		Labels:      []string{"noise", "ohphelia", "unknown"},
		WakeIndex:   1,
		NMels:       40,
		ClipSeconds: 0.32,
		NormPeak:    0.8,
	}
}

// loud returns a chunk well above the silence floor.
func loud(n int) []int16 {
	s := make([]int16, n)
	for i := range s {
		s[i] = int16(8000 * math.Sin(float64(i)/12))
	}
	return s
}

type fakeInf struct {
	logits []float32
	calls  int
	last   []float32
	err    error
}

func (f *fakeInf) Run(w []float32) ([]float32, error) {
	f.calls++
	f.last = append(f.last[:0], w...)
	if f.err != nil {
		return nil, f.err
	}
	return f.logits, nil
}

func newDet(t *testing.T, inf Inferer, o Options) *Detector {
	t.Helper()
	d, err := New(inf, testSpec(), o)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	return d
}

// ── the hop ────────────────────────────────────────────────────────────────

func TestNoScoreUntilTheWindowIsFull(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{})

	for i := 0; i < 3; i++ {
		if _, ok, err := d.Push(loud(1280)); ok || err != nil {
			t.Fatalf("chunk %d: ok=%v err=%v, want no score", i, ok, err)
		}
	}
	if inf.calls != 0 {
		t.Fatalf("inference ran %d times before the window was full", inf.calls)
	}
	if _, ok, err := d.Push(loud(1280)); !ok || err != nil {
		t.Fatalf("fourth chunk: ok=%v err=%v, want a score", ok, err)
	}
}

func TestScoresOnceTheWindowExistsThenEveryHop(t *testing.T) {
	// "Score as soon as there is something to score, then every hop" — the
	// first full window must not wait a hop, or every turn pays 160ms it
	// need not.
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 2})

	var got []bool
	for i := 0; i < 10; i++ {
		_, ok, err := d.Push(loud(1280))
		if err != nil {
			t.Fatal(err)
		}
		got = append(got, ok)
	}
	want := []bool{false, false, false, true, false, true, false, true, false, true}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("chunk %d: ok=%v want %v (all: %v)", i, got[i], want[i], got)
		}
	}
}

func TestHopOfOneScoresEveryChunk(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	for i := 0; i < 4; i++ {
		if _, ok, _ := d.Push(loud(1280)); !ok {
			t.Fatalf("chunk %d produced no score at hop=1", i)
		}
	}
}

// ── not judged is not zero ─────────────────────────────────────────────────

func TestSilenceIsNotJudgedRatherThanScoredZero(t *testing.T) {
	// A muted device streams zero-filled frames. Scoring them would report a
	// confident 0.0 for audio nobody looked at, and — worse — the peak
	// normalisation would amplify a near-silent window by up to ~8000x.
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{})
	for i := 0; i < 6; i++ {
		if _, ok, err := d.Push(make([]int16, 1280)); ok || err != nil {
			t.Fatalf("silent chunk %d: ok=%v err=%v", i, ok, err)
		}
	}
	if inf.calls != 0 {
		t.Fatalf("ran inference on %d silent windows", inf.calls)
	}
}

func TestASilentGapDoesNotDiluteTheScoresEitherSideOfIt(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1, Smoothing: 3})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	var before float32
	for i := 0; i < 3; i++ {
		before, _, _ = d.Push(loud(1280))
	}
	// A stretch of silence: not scored, and the history must survive it.
	for i := 0; i < 3; i++ {
		d.Push(make([]int16, 1280))
	}
	after, ok, _ := d.Push(loud(1280))
	if !ok {
		t.Fatal("no score after the silent gap")
	}
	if math.Abs(float64(after-before)) > 1e-6 {
		t.Fatalf("score moved across a silent gap: %v -> %v", before, after)
	}
}

// ── the maths ──────────────────────────────────────────────────────────────

func TestSoftmaxPicksTheWakeIndexNotTheFirstLogit(t *testing.T) {
	// The first real model sorts to ["noise","ohphelia","unknown"] with the
	// wake word at index 1. Reading index 0 scores "noise" and presents as a
	// detector that never fires.
	inf := &fakeInf{logits: []float32{5, 0, 0}} // "noise" is the loud one
	d := newDet(t, inf, Options{HopChunks: 1, Smoothing: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	score, ok, _ := d.Push(loud(1280))
	if !ok {
		t.Fatal("no score")
	}
	// e^0 / (e^5 + 2*e^0) — small, because the wake class did NOT win.
	want := float32(1.0 / (math.Exp(5) + 2))
	if math.Abs(float64(score-want)) > 1e-5 {
		t.Fatalf("score %v, want %v — is it reading logit 0?", score, want)
	}
}

func TestSoftmaxIsMaxSubtractedSoALargeLogitCannotOverflow(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 800, 0}}
	d := newDet(t, inf, Options{HopChunks: 1, Smoothing: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	score, ok, _ := d.Push(loud(1280))
	if !ok || math.IsNaN(float64(score)) || math.IsInf(float64(score), 0) {
		t.Fatalf("score=%v ok=%v — exp(800) overflowed", score, ok)
	}
	if score < 0.999 {
		t.Fatalf("score %v, want ~1.0", score)
	}
}

func TestTheWindowIsPeakNormalisedToTheSpecsLevel(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	d.Push(loud(1280))
	if inf.calls == 0 {
		t.Fatal("inference never ran")
	}
	var peak float32
	for _, v := range inf.last {
		if a := float32(math.Abs(float64(v))); a > peak {
			peak = a
		}
	}
	if math.Abs(float64(peak-0.8)) > 1e-5 {
		t.Fatalf("window peak %v, want the spec's normPeak 0.8", peak)
	}
}

func TestSmoothingAveragesTheLastNWindows(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1, Smoothing: 3})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	one, _, _ := d.Push(loud(1280)) // history: [p]
	// Now make the model answer 0 for the wake class twice.
	inf.logits = []float32{5, 0, 0}
	d.Push(loud(1280))
	third, _, _ := d.Push(loud(1280))
	// The mean of one high and two low scores must sit between them.
	if !(third < one) {
		t.Fatalf("smoothed score %v did not fall below the earlier %v", third, one)
	}
	if third <= 0 {
		t.Fatalf("smoothed score %v discarded the earlier high window", third)
	}
}

// ── the ring ───────────────────────────────────────────────────────────────

func TestTheWindowKeepsTheNEWESTAudio(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	// A chunk of a distinctive constant level; it must appear at the END.
	mark := make([]int16, 1280)
	for i := range mark {
		mark[i] = 4000
	}
	d.Push(mark)
	tail := inf.last[len(inf.last)-1280:]
	for i, v := range tail {
		if v <= 0 {
			t.Fatalf("tail sample %d is %v — the newest chunk is not at the end", i, v)
		}
	}
}

func TestAChunkLongerThanTheWindowKeepsOnlyItsTail(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	big := loud(4*1280 + 500)
	if _, _, err := d.Push(big); err != nil {
		t.Fatal(err)
	}
	if !d.Ready() {
		t.Fatal("an oversized chunk did not fill the window")
	}
}

func TestPartialChunksAccumulateIntoAWindow(t *testing.T) {
	// The mic delivers 80ms chunks, but nothing in this package may depend on
	// that: the real window (22400) is 17.5 chunks, so a whole-chunk
	// assumption cannot be made anywhere.
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 8; i++ {
		d.Push(loud(640))
	}
	if !d.Ready() {
		t.Fatal("8 half-chunks did not fill a 4-chunk window")
	}
}

func TestAnEmptyPushIsANoOp(t *testing.T) {
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	if _, ok, err := d.Push(nil); ok || err != nil {
		t.Fatalf("empty push: ok=%v err=%v", ok, err)
	}
}

// ── reset is the refractory ────────────────────────────────────────────────

func TestResetCostsAWholeWindowBeforeTheNextScore(t *testing.T) {
	// This is the refractory period, obtained by mapping Reset faithfully
	// rather than by a second mechanism that could disagree with it.
	inf := &fakeInf{logits: []float32{0, 5, 0}}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	if !d.Ready() {
		t.Fatal("not ready before reset")
	}
	d.Reset()
	if d.Ready() {
		t.Fatal("Ready() true immediately after Reset")
	}
	for i := 0; i < 3; i++ {
		if _, ok, _ := d.Push(loud(1280)); ok {
			t.Fatalf("scored %d chunks after a reset, want a full window first", i+1)
		}
	}
	if _, ok, _ := d.Push(loud(1280)); !ok {
		t.Fatal("no score once the window was refilled")
	}
}

// ── failures name themselves ───────────────────────────────────────────────

func TestALogitCountMismatchIsReportedNotIndexed(t *testing.T) {
	// A model and sidecar that disagree is the likely outcome of installing
	// one and not the other. Reading past the end of the logits would panic
	// on the audio path; a short read would silently score the wrong class.
	inf := &fakeInf{logits: []float32{0, 5}} // two logits, three labels
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	_, ok, err := d.Push(loud(1280))
	if ok || err == nil {
		t.Fatalf("ok=%v err=%v, want a mismatch error", ok, err)
	}
	for _, want := range []string{"2 logits", "3 labels"} {
		if !contains(err.Error(), want) {
			t.Fatalf("error %q does not mention %q", err, want)
		}
	}
}

func TestAnInferenceErrorIsWrappedNotSwallowed(t *testing.T) {
	boom := errors.New("session gone")
	inf := &fakeInf{logits: []float32{0, 5, 0}, err: boom}
	d := newDet(t, inf, Options{HopChunks: 1})
	for i := 0; i < 4; i++ {
		d.Push(loud(1280))
	}
	_, ok, err := d.Push(loud(1280))
	if ok || !errors.Is(err, boom) {
		t.Fatalf("ok=%v err=%v, want the inference error", ok, err)
	}
}

func TestANilInfererIsRefused(t *testing.T) {
	if _, err := New(nil, testSpec(), Options{}); err == nil {
		t.Fatal("New accepted a nil Inferer")
	}
}

// ── the sidecar ────────────────────────────────────────────────────────────

func TestSpecValidateRejectsAWakeIndexPastTheLabels(t *testing.T) {
	s := testSpec()
	s.WakeIndex = 3
	err := s.Validate()
	if err == nil {
		t.Fatal("accepted wakeIndex 3 for 3 labels")
	}
	if !contains(err.Error(), "wakeIndex") {
		t.Fatalf("error %q does not name the field", err)
	}
}

func TestSpecValidateRejectsTheEmptyAndTheImpossible(t *testing.T) {
	for name, mutate := range map[string]func(*Spec){
		"no window":     func(s *Spec) { s.Window = 0 },
		"no rate":       func(s *Spec) { s.SampleRate = 0 },
		"no labels":     func(s *Spec) { s.Labels = nil },
		"negative wake": func(s *Spec) { s.WakeIndex = -1 },
		"no normPeak":   func(s *Spec) { s.NormPeak = 0 },
		"loud normPeak": func(s *Spec) { s.NormPeak = 1.5 },
		"wrong type":    func(s *Spec) { s.Type = "oww" },
	} {
		s := testSpec()
		mutate(&s)
		if err := s.Validate(); err == nil {
			t.Fatalf("%s: Validate accepted it", name)
		}
	}
}

func TestSpecTypeMayBeAbsent(t *testing.T) {
	// The field is a courtesy for a human reading the file; the sidecar's
	// EXISTENCE is what marks a model as BC-ResNet, at both ends.
	s := testSpec()
	s.Type = ""
	if err := s.Validate(); err != nil {
		t.Fatalf("Validate rejected a sidecar with no type: %v", err)
	}
}

func TestLoadSpecReadsTheRealSidecarShape(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "ophelia.json")
	// Byte-for-byte the shape the exporter writes.
	os.WriteFile(p, []byte(`{
  "type": "bcresnet",
  "sampleRate": 16000,
  "window": 22400,
  "labels": ["noise", "ohphelia", "unknown"],
  "wakeIndex": 1,
  "nMels": 40,
  "clipSeconds": 1.4,
  "normPeak": 0.8
}`), 0o644)

	s, err := LoadSpec(p)
	if err != nil {
		t.Fatalf("LoadSpec: %v", err)
	}
	if s.Window != 22400 || s.WakeIndex != 1 || s.WakeLabel() != "ohphelia" {
		t.Fatalf("parsed %+v", s)
	}
	if s.NormPeak != 0.8 {
		t.Fatalf("normPeak %v", s.NormPeak)
	}
}

func TestLoadSpecNamesTheFileItCouldNotRead(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "broken.json")
	os.WriteFile(p, []byte(`{"window": `), 0o644)
	_, err := LoadSpec(p)
	if err == nil {
		t.Fatal("accepted truncated JSON")
	}
	if !contains(err.Error(), "broken.json") {
		t.Fatalf("error %q does not name the file", err)
	}
}

func TestWakeLabelIsEmptyRatherThanPanickingOnABadIndex(t *testing.T) {
	s := testSpec()
	s.WakeIndex = 99
	if got := s.WakeLabel(); got != "" {
		t.Fatalf("WakeLabel() = %q", got)
	}
}

func contains(s, sub string) bool {
	return len(sub) == 0 || (len(s) >= len(sub) && indexOf(s, sub) >= 0)
}

func indexOf(s, sub string) int {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}
