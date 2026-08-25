// Package bcresnet scores a wake word with a BC-ResNet model: one graph that
// takes a fixed window of raw audio and emits one logit per class.
//
// It is the second wake-word engine on this device and it is shaped nothing
// like the first. openWakeWord (the parent package) is a three-stage streaming
// pipeline — melspectrogram, embedding, classifier head — where every 80ms
// chunk advances a ring and produces a score. BC-ResNet carries its own log-mel
// frontend inside the graph, takes 1.4s of audio at a time, and is therefore
// run on a HOP: most chunks produce no score at all.
//
// That difference is the reason Push returns an `ok` flag rather than a score
// alone. A chunk that was not judged is not a chunk that scored zero, and
// collapsing the two would feed the crossing test and the max-score statistic
// a stream of invented zeros.
//
// The buffering here is the whole algorithm, so — exactly as in the parent
// package — inference sits behind an interface and this file never imports
// onnxruntime. That is what lets it be tested on the host, where there is no
// ARM runtime and no model.
//
// It is a deliberate port of the controller's em_wake_scorer.BcresnetScorer,
// constant for constant, because the two score the same audio and any
// divergence shows up as a disagreement that looks like a model problem.
package bcresnet

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"strings"
)

const (
	// DefaultHopChunks is how many 80ms chunks pass between scores. Two is
	// 160ms, matching the controller's default: the window is 1.4s and
	// consecutive windows overlap 89%, so scoring every chunk costs 2x the CPU
	// to re-examine audio that has barely changed.
	DefaultHopChunks = 2

	// DefaultSmoothing averages the last N window scores, matching the
	// reference streaming implementation. It removes the single-window spike
	// that an overlapping window produces on a transient — a door click can
	// land inside one window and nowhere near the next two.
	DefaultSmoothing = 3

	// PeakEps is the peak below which a window is NOT rescaled, in the same
	// units as the training-time normalisation (floats in +/-1). The reference
	// implementation owns this constant; a second value here would be a second
	// definition of the model's level invariance.
	PeakEps = 1e-4

	// SilenceRMS is the level below which a window is not scored at all.
	// Distinct from PeakEps and needed BECAUSE of it: a window peaking just
	// above the guard is rescaled by up to ~8000x, so a merely-quiet room
	// becomes full-scale noise fed to a detector. A muted device streams
	// zero-filled frames and is caught here too.
	SilenceRMS = 1e-4
)

// Spec is the sidecar that makes a BC-ResNet .onnx runnable.
//
// The graph alone cannot say which of its logits is the wake word, and the
// answer is not a convention: class order comes from sorting the training
// directories, so the first real model produced labels ["noise", "ohphelia",
// "unknown"] and a wake index of 1. Anything defaulting to 0 would score
// "noise" and present as a detector that simply never fires.
//
// Field names match the JSON the exporter writes and the controller reads;
// they are the same file, pushed to the device beside the model.
type Spec struct {
	Type        string   `json:"type"`
	SampleRate  int      `json:"sampleRate"`
	Window      int      `json:"window"`
	Labels      []string `json:"labels"`
	WakeIndex   int      `json:"wakeIndex"`
	NMels       int      `json:"nMels"`
	ClipSeconds float64  `json:"clipSeconds"`
	NormPeak    float64  `json:"normPeak"`
}

// WakeLabel is the class name the wake index selects, for logging.
func (s Spec) WakeLabel() string {
	if s.WakeIndex < 0 || s.WakeIndex >= len(s.Labels) {
		return ""
	}
	return s.Labels[s.WakeIndex]
}

// Validate rejects a sidecar that cannot describe a runnable model.
//
// Every check here is a failure that would otherwise surface as silence: a
// wake index past the end of the label list scores whatever happens to be at
// that offset in the output tensor, and a window of zero divides by nothing
// forever. Refusing at load time is what makes the log line name the file.
func (s Spec) Validate() error {
	if t := strings.TrimSpace(s.Type); t != "" && t != "bcresnet" {
		return fmt.Errorf("sidecar type is %q, not \"bcresnet\"", t)
	}
	if s.Window <= 0 {
		return fmt.Errorf("window must be positive, got %d", s.Window)
	}
	if s.SampleRate <= 0 {
		return fmt.Errorf("sampleRate must be positive, got %d", s.SampleRate)
	}
	if len(s.Labels) == 0 {
		return fmt.Errorf("labels is empty — nothing names the wake class")
	}
	if s.WakeIndex < 0 || s.WakeIndex >= len(s.Labels) {
		return fmt.Errorf("wakeIndex %d is outside the %d labels %v",
			s.WakeIndex, len(s.Labels), s.Labels)
	}
	if s.NormPeak <= 0 || s.NormPeak > 1 {
		return fmt.Errorf("normPeak must be in (0,1], got %v", s.NormPeak)
	}
	return nil
}

// LoadSpec reads and validates a sidecar.
func LoadSpec(path string) (Spec, error) {
	var s Spec
	raw, err := os.ReadFile(path)
	if err != nil {
		return s, fmt.Errorf("bcresnet: %w", err)
	}
	if err := json.Unmarshal(raw, &s); err != nil {
		return s, fmt.Errorf("bcresnet: %s is not valid JSON: %w", path, err)
	}
	if err := s.Validate(); err != nil {
		return s, fmt.Errorf("bcresnet: %s: %w", path, err)
	}
	return s, nil
}

// Inferer runs the model. `window` is the normalised window as float32 in
// +/-1, length Spec.Window; the result is one logit per label.
//
// Deliberately the narrowest possible surface — the graph is a single input
// and a single output, unlike the parent package's three-method Inferer — so
// the host test can supply a closure.
type Inferer interface {
	Run(window []float32) ([]float32, error)
}

// Detector holds the streaming state for one audio source.
//
// Not safe for concurrent use: like the parent package's Detector it is driven
// from a single goroutine, which for shadow mode is the scorer goroutine.
type Detector struct {
	inf  Inferer
	spec Spec

	hopChunks int
	smoothing int
	minRMS    float64

	buf      []int16 // ring of raw samples, newest at the end
	filled   int
	sinceHop int
	recent   []float32

	// scratch is the normalised window handed to the Inferer. Reused because
	// inference runs 6.25 times a second forever and the steady state must not
	// allocate — same reasoning as ort.model.out.
	scratch []float32
}

// Options tunes the Detector. The zero value means "use the defaults".
type Options struct {
	HopChunks int
	Smoothing int
	MinRMS    float64
}

// New returns a Detector for spec. The spec is assumed validated (LoadSpec
// does it); an invalid one is an error rather than a panic because it arrives
// from a file on the device, not from this codebase.
func New(inf Inferer, spec Spec, o Options) (*Detector, error) {
	if inf == nil {
		return nil, fmt.Errorf("bcresnet: nil Inferer")
	}
	if err := spec.Validate(); err != nil {
		return nil, fmt.Errorf("bcresnet: %w", err)
	}
	if o.HopChunks <= 0 {
		o.HopChunks = DefaultHopChunks
	}
	if o.Smoothing <= 0 {
		o.Smoothing = DefaultSmoothing
	}
	if o.MinRMS <= 0 {
		o.MinRMS = SilenceRMS
	}
	d := &Detector{
		inf:       inf,
		spec:      spec,
		hopChunks: o.HopChunks,
		smoothing: o.Smoothing,
		minRMS:    o.MinRMS,
		buf:       make([]int16, spec.Window),
		scratch:   make([]float32, spec.Window),
		recent:    make([]float32, 0, o.Smoothing),
	}
	d.Reset()
	return d, nil
}

// Spec returns the loaded sidecar.
func (d *Detector) Spec() Spec { return d.spec }

// Ready reports whether a full window has been collected since the last Reset.
func (d *Detector) Ready() bool { return d.filled >= d.spec.Window }

// Reset discards the rolling context: the window and the smoothing history.
//
// The buffer's contents are not zeroed, only disowned — `filled` is what makes
// them unreadable, and a window is never scored until it has been refilled from
// scratch. That also gives the refractory period for free: after a crossing the
// caller resets, and the next score cannot arrive until a whole fresh window
// exists.
func (d *Detector) Reset() {
	d.filled = 0
	// So the first full window scores the moment it exists rather than one hop
	// later. "Score as soon as there is something to score, then every hop" is
	// the intended behaviour.
	d.sinceHop = d.hopChunks - 1
	d.recent = d.recent[:0]
}

// Push adds one chunk of 16kHz mono PCM and reports a smoothed wake
// probability if this chunk produced one.
//
// ok=false means one of: the window is not full yet, this chunk is not a hop
// boundary, or the window is below the silence floor. All three are "not
// judged", which is why they share a return distinct from a score of 0.
func (d *Detector) Push(samples []int16) (score float32, ok bool, err error) {
	// No audio in, no state change. Without this an empty chunk still advances
	// the hop counter, so the next real chunk could score early — harmless in
	// practice (the mic never delivers one) but it makes the hop depend on call
	// count rather than on audio. The controller's scorer has the same guard;
	// the two must not disagree about what a frame is.
	if len(samples) == 0 {
		return 0, false, nil
	}
	d.append(samples)
	if d.filled < d.spec.Window {
		return 0, false, nil
	}
	d.sinceHop++
	if d.sinceHop < d.hopChunks {
		return 0, false, nil
	}
	d.sinceHop = 0
	return d.score()
}

// append slides the window and copies in the new samples.
func (d *Detector) append(samples []int16) {
	n := len(samples)
	w := d.spec.Window
	if n == 0 {
		return
	}
	if n >= w {
		// A chunk larger than the whole window: keep only its tail. Cannot
		// happen with an 80ms mic chunk, but the arithmetic below would be
		// wrong rather than merely wasteful if it did.
		copy(d.buf, samples[n-w:])
		d.filled = w
		return
	}
	copy(d.buf, d.buf[n:])
	copy(d.buf[w-n:], samples)
	if d.filled += n; d.filled > w {
		d.filled = w
	}
}

func (d *Detector) score() (float32, bool, error) {
	// One pass for the sum of squares and the peak: this runs on every hop,
	// forever, over 22400 samples.
	var sumSq float64
	peak := float32(0)
	for i, s := range d.buf {
		v := float32(s) / 32768.0
		d.scratch[i] = v
		sumSq += float64(v) * float64(v)
		if a := float32(math.Abs(float64(v))); a > peak {
			peak = a
		}
	}
	rms := math.Sqrt(sumSq / float64(len(d.buf)))
	if rms < d.minRMS {
		// Not scored, and the smoothing history is deliberately left alone: a
		// silent gap should not dilute the scores either side of it.
		return 0, false, nil
	}
	if peak > PeakEps {
		// The ratio is computed in float64 and narrowed once, matching the
		// controller exactly: it divides a Python float by a Python float and
		// applies the result to a float32 array. Dividing in float32 here
		// instead differs by an ULP, which is invisible in a score and is
		// precisely the kind of drift the parity fixture exists to catch.
		g := float32(d.spec.NormPeak / float64(peak))
		for i := range d.scratch {
			d.scratch[i] *= g
		}
	}

	logits, err := d.inf.Run(d.scratch)
	if err != nil {
		return 0, false, fmt.Errorf("bcresnet: run: %w", err)
	}
	if len(logits) != len(d.spec.Labels) {
		return 0, false, fmt.Errorf(
			"bcresnet: model emits %d logits but the sidecar names %d labels "+
				"%v — mismatched .onnx/.json pair",
			len(logits), len(d.spec.Labels), d.spec.Labels)
	}

	p := softmax(logits)[d.spec.WakeIndex]
	if len(d.recent) == d.smoothing {
		copy(d.recent, d.recent[1:])
		d.recent = d.recent[:d.smoothing-1]
	}
	d.recent = append(d.recent, p)

	var sum float32
	for _, v := range d.recent {
		sum += v
	}
	return sum / float32(len(d.recent)), true, nil
}

// softmax over one row, max-subtracted so a large logit cannot overflow.
func softmax(logits []float32) []float32 {
	max := logits[0]
	for _, v := range logits[1:] {
		if v > max {
			max = v
		}
	}
	out := make([]float32, len(logits))
	var sum float64
	for i, v := range logits {
		e := math.Exp(float64(v - max))
		out[i] = float32(e)
		sum += e
	}
	for i := range out {
		out[i] = float32(float64(out[i]) / sum)
	}
	return out
}
