// Package bcresnet implements the deployed BC-ResNet artifact contract of
// SPEC §5.1: the sidecar, the per-window preparation, and the wake probability.
// It holds no streaming state; windowing, hops, and smoothing belong to
// wakeword/detector.
package bcresnet

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
)

// Artifact constants (SPEC §5.1).
const (
	// SampleRate is the only sample rate the graph accepts.
	SampleRate = 16000
	// WindowSamples is the graph input length: 1.4 s at 16 kHz.
	WindowSamples = 22400
	// RMSFloor: a window whose RMS (of int16/32768) is below this is not scored.
	RMSFloor = 1e-4
	// PeakGuard: a window whose peak is at or below this is not rescaled.
	PeakGuard = 1e-4
	// sidecarType is the sidecar's `type` value.
	sidecarType = "bcresnet"
)

// Sidecar is the JSON file shipped beside the graph. Field names are the
// exporter's.
type Sidecar struct {
	Type        string   `json:"type"`
	SampleRate  int      `json:"sampleRate"`
	Window      int      `json:"window"`
	Labels      []string `json:"labels"`
	WakeIndex   int      `json:"wakeIndex"`
	NMels       int      `json:"nMels"`
	ClipSeconds float64  `json:"clipSeconds"`
	NormPeak    float64  `json:"normPeak"`
}

// ParseSidecar decodes and validates a sidecar.
func ParseSidecar(raw []byte) (Sidecar, error) {
	var s Sidecar
	if err := json.Unmarshal(raw, &s); err != nil {
		return Sidecar{}, fmt.Errorf("bcresnet: sidecar is not valid JSON: %w", err)
	}
	if err := s.Validate(); err != nil {
		return Sidecar{}, err
	}
	return s, nil
}

// LoadSidecar reads and validates the sidecar at path.
func LoadSidecar(path string) (Sidecar, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return Sidecar{}, fmt.Errorf("bcresnet: %w", err)
	}
	s, err := ParseSidecar(raw)
	if err != nil {
		return Sidecar{}, fmt.Errorf("%w (%s)", err, path)
	}
	return s, nil
}

// Validate enforces the SPEC §5.1 load rules that the sidecar alone decides:
// 16 kHz, a 22,400-sample window, a wake index inside the labels, and a
// normalization peak in (0, 1].
func (s Sidecar) Validate() error {
	switch {
	case s.Type != sidecarType:
		return fmt.Errorf("bcresnet: sidecar type %q, want %q", s.Type, sidecarType)
	case s.SampleRate != SampleRate:
		return fmt.Errorf("bcresnet: sidecar sampleRate %d, want %d", s.SampleRate, SampleRate)
	case s.Window != WindowSamples:
		return fmt.Errorf("bcresnet: sidecar window %d, want %d", s.Window, WindowSamples)
	case len(s.Labels) == 0:
		return errors.New("bcresnet: sidecar has no labels")
	case s.WakeIndex < 0 || s.WakeIndex >= len(s.Labels):
		return fmt.Errorf("bcresnet: sidecar wakeIndex %d outside %d labels", s.WakeIndex, len(s.Labels))
	case !(s.NormPeak > 0 && s.NormPeak <= 1):
		return fmt.Errorf("bcresnet: sidecar normPeak %v outside (0, 1]", s.NormPeak)
	}
	return nil
}

// CheckGraphIO rejects a graph whose input length or output count disagrees
// with the sidecar (SPEC §5.1). A negative dimension is dynamic and rejected:
// the deployed graph fixes both.
func (s Sidecar) CheckGraphIO(inputLen, outputCount int) error {
	if inputLen != WindowSamples {
		return fmt.Errorf("bcresnet: graph input length %d, want %d", inputLen, WindowSamples)
	}
	if outputCount != len(s.Labels) {
		return fmt.Errorf("bcresnet: graph emits %d outputs, sidecar names %d labels", outputCount, len(s.Labels))
	}
	return nil
}

// Prepare writes the SPEC §5.1 model input for one int16 window into dst
// (both WindowSamples long): x = pcm/32768; if RMS(x) < RMSFloor it reports
// false and dst is not a model input; otherwise, if peak(x) > PeakGuard, x is
// scaled to peak normPeak.
func Prepare(dst []float32, pcm []int16, normPeak float64) bool {
	var sumSq float64
	var peak float32
	for i, v := range pcm {
		x := float32(v) / 32768.0
		dst[i] = x
		sumSq += float64(x) * float64(x)
		if x < 0 {
			x = -x
		}
		if x > peak {
			peak = x
		}
	}
	if math.Sqrt(sumSq/float64(len(pcm))) < RMSFloor {
		return false
	}
	normalizePeak(dst, peak, normPeak)
	return true
}

// normalizePeak scales x to peak normPeak unless its peak is within the guard.
// The gain is computed in float64 and applied in float32, as the controller's
// numpy scorer does.
func normalizePeak(x []float32, peak float32, normPeak float64) {
	if peak <= PeakGuard {
		return
	}
	g := float32(normPeak / float64(peak))
	for i := range x {
		x[i] *= g
	}
}

// WakeProbability is softmax(logits − max(logits))[wakeIndex]. It returns NaN
// when a logit is non-finite; callers treat that as an inference error.
func WakeProbability(logits []float32, wakeIndex int) float64 {
	m := math.Inf(-1)
	for _, v := range logits {
		f := float64(v)
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return math.NaN()
		}
		m = math.Max(m, f)
	}
	var sum, wake float64
	for i, v := range logits {
		e := math.Exp(float64(v) - m)
		sum += e
		if i == wakeIndex {
			wake = e
		}
	}
	return wake / sum
}

// Inferer runs the graph on one prepared window and returns its logits. The
// result may alias a buffer the next Run overwrites.
type Inferer interface {
	Run(window []float32) ([]float32, error)
	Close() error
}

// Scorer turns an int16 window into a wake probability with one graph and its
// sidecar. It is owned by one goroutine; Score does not allocate.
type Scorer struct {
	inf     Inferer
	sidecar Sidecar
	scratch []float32
}

// NewScorer pairs a loaded graph with its validated sidecar. The Scorer owns inf.
func NewScorer(inf Inferer, sc Sidecar) (*Scorer, error) {
	if err := sc.Validate(); err != nil {
		return nil, err
	}
	return &Scorer{inf: inf, sidecar: sc, scratch: make([]float32, WindowSamples)}, nil
}

// Sidecar returns the scorer's sidecar.
func (s *Scorer) Sidecar() Sidecar { return s.sidecar }

// Score prepares pcm (WindowSamples long) and runs the graph. scored is false,
// with a nil error, for a window below the RMS floor: the graph did not run.
// A graph error, a logit count that disagrees with the labels, or a
// non-finite probability is an error.
func (s *Scorer) Score(pcm []int16) (prob float64, scored bool, err error) {
	if len(pcm) != WindowSamples {
		return 0, false, fmt.Errorf("bcresnet: window of %d samples, want %d", len(pcm), WindowSamples)
	}
	if !Prepare(s.scratch, pcm, s.sidecar.NormPeak) {
		return 0, false, nil
	}
	logits, err := s.inf.Run(s.scratch)
	if err != nil {
		return 0, true, fmt.Errorf("bcresnet: run: %w", err)
	}
	if len(logits) != len(s.sidecar.Labels) {
		return 0, true, fmt.Errorf("bcresnet: %d logits for %d labels", len(logits), len(s.sidecar.Labels))
	}
	p := WakeProbability(logits, s.sidecar.WakeIndex)
	if math.IsNaN(p) || math.IsInf(p, 0) {
		return 0, true, errors.New("bcresnet: non-finite output")
	}
	return p, true, nil
}

// Close releases the graph.
func (s *Scorer) Close() error { return s.inf.Close() }
