package shadow

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
	"github.com/wilbowes/EchoMuse/internal/wakeword/ort"
)

// DefaultDir is where the ONNX Runtime library and the models live on the
// device. Overridable with EM_OWW_DIR, mainly so the same binary can be
// pointed at a scratch directory while iterating.
//
// The library is NOT part of the firmware: it is 12.3MB, it changes far less
// often than the binary, and embedding it would double both the OTA payload and
// the space taken by the A/B slots. It is installed out of band, and its
// absence is an ordinary, expected condition — see Open.
const DefaultDir = "/data/local/share/echomuse/oww"

// Dir returns the configured model directory.
func Dir() string {
	if d := os.Getenv("EM_OWW_DIR"); d != "" {
		return d
	}
	return DefaultDir
}

// ModelStem turns the controller's owwModel value into the classifier filename
// stem, mirroring em_oww_models.prediction_key EXACTLY.
//
// That value is a bare name for built-in models ("hey_mycroft_v0.1") and a file
// PATH for custom ones. Python's rule is: strip to the filename stem only when
// the value ends in ".onnx", otherwise pass it through untouched.
//
// The untouched case is the whole point, and getting it wrong is not
// hypothetical — an earlier version used filepath.Ext, which sees ".1" as the
// extension of "hey_mycroft_v0.1" and went looking for hey_mycroft_v0.onnx. A
// version suffix is not a file extension.
func ModelStem(owwModel string) string {
	name := strings.TrimSpace(owwModel)
	if !strings.HasSuffix(name, ".onnx") {
		return name
	}
	return strings.TrimSuffix(filepath.Base(name), ".onnx")
}

// SidecarPath is the BC-ResNet sidecar that would sit beside a model stem.
//
// Its EXISTENCE is what marks a model as BC-ResNet, at both ends — never the
// filename, which is user-chosen, and never a config key, which would let the
// two ends disagree about a file only one of them can see. em_oww_models.scan
// and em_wake_scorer.is_bcresnet_model apply exactly this rule.
func SidecarPath(dir, stem string) string {
	return filepath.Join(dir, stem+".json")
}

// Open loads ONNX Runtime and whichever engine the installed model calls for,
// and starts a Scorer.
//
// A missing library or model returns an error and is NOT a failure of the
// device: shadow mode is off until someone installs them, the caller logs it
// once and carries on with controller-side wake word. That is the whole reason
// the runtime is dlopen'd rather than linked.
func Open(owwModel string, threshold float32, onCross func(score, threshold float32, at time.Time)) (*Scorer, error) {
	dir := Dir()
	stem := ModelStem(owwModel)
	if stem == "" {
		return nil, fmt.Errorf("shadow: no wake word model configured")
	}
	if _, err := os.Stat(SidecarPath(dir, stem)); err == nil {
		return openBcresnet(dir, stem, threshold, onCross)
	}
	return openOww(dir, stem, threshold, onCross)
}

// openBcresnet loads a single-graph BC-ResNet detector.
//
// It needs neither of openWakeWord's shared feature models — the log-mel
// frontend is inside the graph — so it must not check for them. A device
// carrying only a BC-ResNet model is correctly provisioned, and demanding
// melspectrogram.onnx here would refuse it for a file it will never open.
func openBcresnet(dir, stem string, threshold float32, onCross func(score, threshold float32, at time.Time)) (*Scorer, error) {
	sidecar := SidecarPath(dir, stem)
	spec, err := bcresnet.LoadSpec(sidecar)
	if err != nil {
		return nil, fmt.Errorf("shadow: %w", err)
	}
	if spec.SampleRate != wakeword.SampleRate {
		return nil, fmt.Errorf(
			"shadow: %s wants %dHz audio but the mic pipeline delivers %d",
			filepath.Base(sidecar), spec.SampleRate, wakeword.SampleRate)
	}

	modelPath := filepath.Join(dir, stem+".onnx")
	if _, err := os.Stat(modelPath); err != nil {
		return nil, fmt.Errorf("shadow: BC-ResNet model not installed at %s", modelPath)
	}

	rt, err := ort.Open(filepath.Join(dir, "libonnxruntime.so"))
	if err != nil {
		return nil, fmt.Errorf("shadow: %w", err)
	}
	sess, err := rt.NewSingle(modelPath, spec.Window, ort.DefaultOptions())
	if err != nil {
		return nil, fmt.Errorf("shadow: %w", err)
	}
	det, err := bcresnet.New(sess, spec, bcresnet.Options{})
	if err != nil {
		sess.Close()
		return nil, fmt.Errorf("shadow: %w", err)
	}

	s := NewEngineScorer(&bcresnetEngine{det: det}, threshold, onCross)
	s.closer = sess
	s.info = fmt.Sprintf(
		"bcresnet via onnxruntime %s, model %s, wake %q@%d, %.2fs window, xnnpack=%v",
		rt.Version(), stem, spec.WakeLabel(), spec.WakeIndex,
		float64(spec.Window)/float64(spec.SampleRate), sess.XNNPACKActive())
	return s, nil
}

// openOww loads openWakeWord's three-model streaming pipeline.
func openOww(dir, stem string, threshold float32, onCross func(score, threshold float32, at time.Time)) (*Scorer, error) {
	models := ort.Models{
		Melspec:    filepath.Join(dir, "melspectrogram.onnx"),
		Embedding:  filepath.Join(dir, "embedding_model.onnx"),
		Classifier: filepath.Join(dir, stem+".onnx"),
	}
	// Checked up front so the log names the missing file. ORT's own error for a
	// bad path mentions neither which of the three it was nor what was expected
	// there, and this is a path people will get wrong while installing by hand.
	for what, p := range map[string]string{
		"melspectrogram model": models.Melspec,
		"embedding model":      models.Embedding,
		"classifier model":     models.Classifier,
	} {
		if _, err := os.Stat(p); err != nil {
			return nil, fmt.Errorf("shadow: %s not installed at %s", what, p)
		}
	}

	lib := filepath.Join(dir, "libonnxruntime.so")
	rt, err := ort.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("shadow: %w", err)
	}
	inf, err := rt.NewInferer(models, ort.DefaultOptions())
	if err != nil {
		return nil, fmt.Errorf("shadow: %w", err)
	}

	s := NewScorer(inf, threshold, onCross)
	s.closer = inf
	s.info = fmt.Sprintf("onnxruntime %s, model %s, xnnpack=%v",
		rt.Version(), stem, inf.XNNPACKActive())
	return s, nil
}
