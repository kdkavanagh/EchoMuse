package shadow

import (
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
)

// hopEngine is an Engine that scores only every nth Push, after a warm-up.
// It stands in for BC-ResNet without needing a model: what the scorer has to
// get right is the three-way distinction between "not warm", "warm but between
// hops", and "here is a score".
type hopEngine struct {
	warmAfter int // pushes before Ready
	hop       int
	score     float32

	pushes int
}

func (e *hopEngine) Reset()      { e.pushes = 0 }
func (e *hopEngine) Ready() bool { return e.pushes >= e.warmAfter }

func (e *hopEngine) Push(_ []int16) (float32, bool, error) {
	e.pushes++
	if !e.Ready() {
		return 0, false, nil
	}
	if (e.pushes-e.warmAfter)%e.hop != 0 {
		return 0, false, nil
	}
	return e.score, true, nil
}

// drainAfter pushes n frames and returns the stats.
//
// It waits for the queue to empty between pushes rather than filling it: the
// queue is 8 deep and drops by design when full, so a tight loop of 12 would
// measure the drop path instead of the accounting under test.
func drainAfter(t *testing.T, s *Scorer, pushes int) Stats {
	t.Helper()
	for i := 0; i < pushes; i++ {
		s.Push(make([]int16, 1280))
		deadline := time.Now().Add(2 * time.Second)
		for len(s.ch) > 0 && time.Now().Before(deadline) {
			time.Sleep(time.Millisecond)
		}
	}
	// The last frame is off the channel but may still be in flight.
	time.Sleep(50 * time.Millisecond)
	st := s.Drain()
	if st.Drops != 0 {
		t.Fatalf("%d frames dropped — the test is measuring the queue, not the accounting", st.Drops)
	}
	return st
}

func TestSkippedAndNotReadyAreCountedApart(t *testing.T) {
	// They mean opposite things. A rising NotReady says the stream keeps
	// restarting and the engine never warms up; a high Skipped is just
	// BC-ResNet's duty cycle. One number for both would make a healthy
	// hop-scoring engine indistinguishable from a broken stream.
	eng := &hopEngine{warmAfter: 4, hop: 2, score: 0.1}
	s := NewEngineScorer(eng, 0.9, nil)
	defer s.Close()

	st := drainAfter(t, s, 12)

	if st.Frames != 12 {
		t.Fatalf("Frames=%d, want 12", st.Frames)
	}
	if st.NotReady != 3 {
		t.Fatalf("NotReady=%d, want 3 (the warm-up)", st.NotReady)
	}
	if st.Skipped == 0 {
		t.Fatal("Skipped=0 — hop gaps were counted as warm-up or as scores")
	}
	if st.NotReady+st.Skipped >= st.Frames {
		t.Fatalf("every frame was unscored: NotReady=%d Skipped=%d Frames=%d",
			st.NotReady, st.Skipped, st.Frames)
	}
}

func TestAnUnscoredFrameIsNotAZeroScore(t *testing.T) {
	// MaxScore is the number that says whether a device is nearly detecting or
	// nowhere close. Feeding it an invented 0.0 for every hop gap would be
	// harmless for the max but would make Frames-with-a-score unrecoverable.
	eng := &hopEngine{warmAfter: 2, hop: 3, score: 0.42}
	s := NewEngineScorer(eng, 0.9, nil)
	defer s.Close()

	st := drainAfter(t, s, 12)
	if st.MaxScore != 0.42 {
		t.Fatalf("MaxScore=%v, want 0.42", st.MaxScore)
	}
}

func TestAScoreOnlyCrossesOnAScoredFrame(t *testing.T) {
	// atomic: onCross fires on the SCORER goroutine, so a plain int here is a
	// data race in the test rather than in the product.
	var crossings atomic.Int64
	eng := &hopEngine{warmAfter: 1, hop: 4, score: 0.95}
	s := NewEngineScorer(eng, 0.5, func(score, th float32, at time.Time) {
		crossings.Add(1)
	})
	s.refract = 0 // the refractory is tested elsewhere; here every score counts
	defer s.Close()

	st := drainAfter(t, s, 13)
	n := crossings.Load()
	if n == 0 {
		t.Fatal("no crossings from an engine scoring above threshold")
	}
	if uint64(n) > st.Frames-st.Skipped-st.NotReady {
		t.Fatalf("%d crossings from %d scored frames",
			n, st.Frames-st.Skipped-st.NotReady)
	}
}

// ── the openWakeWord adapter ───────────────────────────────────────────────

func TestTheOwwAdapterScoresOnlyOnceItIsReady(t *testing.T) {
	// The parent Detector keeps Push and Score separate on purpose. The
	// adapter must not turn "not enough embeddings yet" into a score of zero.
	inf := &fakeInferer{score: 0.7}
	e := NewOwwEngine(inf)

	if e.Ready() {
		t.Fatal("Ready() before any audio")
	}
	if _, ok, err := e.Push(make([]int16, 1280)); ok || err != nil {
		t.Fatalf("first chunk: ok=%v err=%v", ok, err)
	}

	// 16 embeddings, one per chunk, is ~1.28s.
	for i := 0; i < 24; i++ {
		e.Push(make([]int16, 1280))
	}
	score, ok, err := e.Push(make([]int16, 1280))
	if err != nil || !ok {
		t.Fatalf("warmed adapter: ok=%v err=%v", ok, err)
	}
	if score != 0.7 {
		t.Fatalf("score=%v, want the inferer's 0.7", score)
	}
}

func TestTheOwwAdapterPropagatesInferenceErrors(t *testing.T) {
	inf := &fakeInferer{score: 0.7, failEmbed: true}
	e := NewOwwEngine(inf)
	if _, ok, err := e.Push(make([]int16, 1280)); ok || err == nil {
		t.Fatalf("ok=%v err=%v, want the embed failure", ok, err)
	}
}

// ── Open picks the engine from the sidecar ─────────────────────────────────
//
// Open cannot be run to completion without ONNX Runtime and a real model, but
// the BRANCH can: each path checks for its own files first and names the one
// it wanted. That is the decision worth pinning — getting it wrong means a
// device silently loads the wrong engine for the model it was given.

func withDir(t *testing.T) string {
	t.Helper()
	d := t.TempDir()
	t.Setenv("EM_OWW_DIR", d)
	return d
}

func TestOpenTakesTheBcresnetPathWhenASidecarIsPresent(t *testing.T) {
	d := withDir(t)
	os.WriteFile(filepath.Join(d, "ophelia.json"), []byte(`{
	  "type":"bcresnet","sampleRate":16000,"window":22400,
	  "labels":["noise","ohphelia","unknown"],"wakeIndex":1,
	  "nMels":40,"clipSeconds":1.4,"normPeak":0.8}`), 0o644)
	// No .onnx, so it must fail — but as BC-ResNet, not by asking for
	// openWakeWord's shared feature models.
	_, err := Open("/data/oww_models/ophelia.onnx", 0.5, nil)
	if err == nil {
		t.Fatal("Open succeeded with no model file")
	}
	if !strings.Contains(err.Error(), "BC-ResNet model not installed") {
		t.Fatalf("error %q — did it take the openWakeWord path?", err)
	}
}

func TestOpenTakesTheOpenWakeWordPathWithNoSidecar(t *testing.T) {
	d := withDir(t)
	os.WriteFile(filepath.Join(d, "ophelia.onnx"), []byte("not a model"), 0o644)
	_, err := Open("/data/oww_models/ophelia.onnx", 0.5, nil)
	if err == nil {
		t.Fatal("Open succeeded with no runtime installed")
	}
	// It should be asking for the shared feature models, which BC-ResNet never
	// opens.
	if !strings.Contains(err.Error(), "melspectrogram") &&
		!strings.Contains(err.Error(), "embedding") {
		t.Fatalf("error %q — did it take the BC-ResNet path?", err)
	}
}

func TestBcresnetNeverDemandsTheSharedFeatureModels(t *testing.T) {
	// Its log-mel frontend is inside the graph. A device carrying only a
	// BC-ResNet model is correctly provisioned, and refusing it for a
	// melspectrogram.onnx it will never open would make the asset planner and
	// the device disagree about what "installed" means.
	d := withDir(t)
	os.WriteFile(filepath.Join(d, "w.json"), []byte(`{
	  "sampleRate":16000,"window":22400,"labels":["a","b"],"wakeIndex":1,
	  "normPeak":0.8}`), 0o644)
	os.WriteFile(filepath.Join(d, "w.onnx"), []byte("not a model"), 0o644)

	_, err := Open("w", 0.5, nil)
	if err == nil {
		t.Fatal("Open succeeded with no runtime")
	}
	for _, forbidden := range []string{"melspectrogram", "embedding model"} {
		if strings.Contains(err.Error(), forbidden) {
			t.Fatalf("BC-ResNet path asked for %s: %v", forbidden, err)
		}
	}
}

func TestABadSidecarIsRefusedByName(t *testing.T) {
	d := withDir(t)
	os.WriteFile(filepath.Join(d, "w.json"), []byte(`{
	  "sampleRate":16000,"window":22400,"labels":["a","b"],"wakeIndex":7,
	  "normPeak":0.8}`), 0o644)
	os.WriteFile(filepath.Join(d, "w.onnx"), []byte("x"), 0o644)

	_, err := Open("w", 0.5, nil)
	if err == nil || !strings.Contains(err.Error(), "wakeIndex") {
		t.Fatalf("error %v, want it to name wakeIndex", err)
	}
}

func TestASidecarAtTheWrongSampleRateIsRefused(t *testing.T) {
	// The mic pipeline delivers 16kHz and cannot resample. A model trained at
	// 8k would score noise forever with nothing saying why.
	d := withDir(t)
	os.WriteFile(filepath.Join(d, "w.json"), []byte(`{
	  "sampleRate":8000,"window":11200,"labels":["a","b"],"wakeIndex":1,
	  "normPeak":0.8}`), 0o644)
	os.WriteFile(filepath.Join(d, "w.onnx"), []byte("x"), 0o644)

	_, err := Open("w", 0.5, nil)
	if err == nil || !strings.Contains(err.Error(), "8000") {
		t.Fatalf("error %v, want it to name the sample rate", err)
	}
}

func TestSidecarPathMatchesTheStemRule(t *testing.T) {
	// The controller decides a model is BC-ResNet by exactly this file
	// existing. If the two ends spell it differently the device loads the
	// wrong engine and nothing anywhere says so.
	if got := SidecarPath("/d", ModelStem("/app/data/oww_models/ophelia.onnx")); got != "/d/ophelia.json" {
		t.Fatalf("SidecarPath = %q", got)
	}
	// A version suffix is not a file extension — the stem rule's old bug.
	if got := SidecarPath("/d", ModelStem("hey_mycroft_v0.1")); got != "/d/hey_mycroft_v0.1.json" {
		t.Fatalf("SidecarPath = %q", got)
	}
}

// Compile-time proof the BC-ResNet detector satisfies the Engine contract via
// its adapter, so a signature change in either is caught here rather than in
// openBcresnet.
var _ Engine = (*bcresnetEngine)(nil)
var _ bcresnet.Inferer = (interface {
	Run([]float32) ([]float32, error)
})(nil)

func TestPushAfterCloseIsANoOpNotAPanic(t *testing.T) {
	// SetShadowScorer swaps the pointer and then Closes the old scorer, while
	// the mic loop pushes to whatever it last read. Any config push that
	// rebuilds the scorer can land between the read and the push, and a send on
	// a closed channel panics — on the mic goroutine, taking the process with
	// it.
	eng := &hopEngine{warmAfter: 1, hop: 1, score: 0.1}
	s := NewEngineScorer(eng, 0.9, nil)
	s.Push(make([]int16, 1280))
	s.Close()

	// Both entry points, and twice, since Close is called on a config change
	// and at shutdown and those can overlap.
	for i := 0; i < 4; i++ {
		s.Push(make([]int16, 1280))
		s.PushBytes(make([]byte, 2560))
	}
	s.Close()
}

func TestPushRacingCloseSurvives(t *testing.T) {
	// The same thing with the timing left to the scheduler. Run under -race
	// this also covers the pointer swap itself.
	eng := &hopEngine{warmAfter: 1, hop: 1, score: 0.1}
	s := NewEngineScorer(eng, 0.9, nil)

	done := make(chan struct{})
	go func() {
		defer close(done)
		for i := 0; i < 500; i++ {
			s.Push(make([]int16, 1280))
		}
	}()
	time.Sleep(2 * time.Millisecond)
	s.Close()
	<-done
}
