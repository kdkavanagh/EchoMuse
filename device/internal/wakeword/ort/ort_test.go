package ort

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"math"
	"os"
	"path/filepath"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet/bcresnettest"
)

// The deployed-graph tests need an ONNX Runtime library and the deployed graph,
// neither of which is in the repository:
//
//	EM_ORT_LIB=/path/to/libonnxruntime.so EM_BCRESNET_GRAPH=/path/to/bcresnet_audio.onnx go test ./internal/wakeword/ort/
//
// A host library usually lacks XNNPACK, so these open the session without
// requiring it.
func deployedSession(t *testing.T, fx *bcresnettest.Fixture) *Session {
	t.Helper()
	lib, graph := os.Getenv("EM_ORT_LIB"), os.Getenv("EM_BCRESNET_GRAPH")
	if lib == "" || graph == "" {
		t.Skip("set EM_ORT_LIB and EM_BCRESNET_GRAPH to run against the deployed graph")
	}
	raw, err := os.ReadFile(graph)
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(raw)
	if got := hex.EncodeToString(sum[:]); got != fx.GraphSHA256 {
		t.Fatalf("EM_BCRESNET_GRAPH sha256 %s is not the deployed graph %s", got, fx.GraphSHA256)
	}
	rt, err := Open(lib)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("onnxruntime %s", rt.Version())
	s, err := rt.newSession(graph, false)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

func TestDeployedGraphMatchesReference(t *testing.T) {
	fx, err := bcresnettest.Load()
	if err != nil {
		t.Fatal(err)
	}
	sess := deployedSession(t, fx)
	sc, err := bcresnet.LoadSidecar(filepath.Join(bcresnettest.Dir(), "bcresnet_audio.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := sc.CheckGraphIO(sess.InputLen(), sess.OutputCount()); err != nil {
		t.Fatal(err)
	}

	in := make([]float32, bcresnet.WindowSamples)
	for _, w := range fx.Windows {
		if !w.Scored {
			continue
		}
		bcresnet.Prepare(in, w.PCM(), sc.NormPeak)
		logits, err := sess.Run(in)
		if err != nil {
			t.Fatalf("%s: %v", w.Name, err)
		}
		for i, want := range w.Logits {
			if math.Abs(float64(logits[i])-want) > 1e-4 {
				t.Errorf("%s: logit %d = %v, reference %v", w.Name, i, logits[i], want)
			}
		}
		if p := bcresnet.WakeProbability(logits, sc.WakeIndex); math.Abs(p-w.Prob) > 1e-5 {
			t.Errorf("%s: prob %v, reference %v", w.Name, p, w.Prob)
		}
	}
	if allocs := testing.AllocsPerRun(5, func() { sess.Run(in) }); allocs != 0 {
		t.Errorf("Run allocates %v times per call", allocs)
	}

	sess.Close()
	sess.Close()
	if _, err := sess.Run(in); !errors.Is(err, ErrClosed) {
		t.Errorf("Run after Close: %v", err)
	}
}

func TestDeployedGraphRejectsWrongInputLength(t *testing.T) {
	fx, err := bcresnettest.Load()
	if err != nil {
		t.Fatal(err)
	}
	sess := deployedSession(t, fx)
	if _, err := sess.Run(make([]float32, bcresnet.WindowSamples-1)); err == nil {
		t.Fatal("accepted a short input")
	}
}

func TestOpenMissingLibraryFails(t *testing.T) {
	if _, err := Open(filepath.Join(t.TempDir(), "libonnxruntime.so")); err == nil {
		t.Fatal("opened a nonexistent library")
	}
}
