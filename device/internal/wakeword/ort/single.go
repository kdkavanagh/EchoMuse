package ort

/*
#include "shim.h"
*/
import "C"

import "fmt"

// Single is one ONNX session with one float32 input and one float32 output.
//
// The three-model Inferer above exists because openWakeWord's pipeline IS
// three graphs; BC-ResNet is one graph that carries its own frontend, so it
// needs nothing but "run this tensor through this model". Keeping that as a
// separate type rather than generalising Inferer is deliberate — Inferer's
// value is that its three methods are named after the stages and check the
// shapes upstream fixed, and a shape-agnostic Inferer would lose all of it.
type Single struct {
	m *model
	// shape is the input shape, fixed at construction: this session is opened
	// for one model whose window length never changes at runtime.
	shape []C.int64_t
	// n is the expected input length, so a mismatch is caught here with a
	// message naming both numbers rather than inside ORT.
	n int

	xnnpack bool
}

// NewSingle opens a session for a model taking [1, n] float32.
//
// n comes from the sidecar's window, so a sidecar that disagrees with the
// graph is caught on the first inference rather than producing a confidently
// wrong score — ORT would otherwise broadcast or fail with a message naming
// neither file.
func (r *Runtime) NewSingle(path string, n int, o Options) (*Single, error) {
	if o.Threads < 1 {
		return nil, fmt.Errorf("ort: Threads must be at least 1, got %d", o.Threads)
	}
	if n < 1 {
		return nil, fmt.Errorf("ort: input length must be positive, got %d", n)
	}
	m, err := r.load(path, "bcresnet", o)
	if err != nil {
		return nil, err
	}
	return &Single{
		m:       m,
		shape:   []C.int64_t{1, C.int64_t(n)},
		n:       n,
		xnnpack: m.m.xnnpack != 0,
	}, nil
}

// XNNPACKActive reports whether the XNNPACK provider attached. False means the
// CPU provider is in use: same numbers, roughly 1.5x the CPU.
func (s *Single) XNNPACKActive() bool { return s.xnnpack }

// Close releases the session. Safe to call twice.
func (s *Single) Close() error {
	if s.m != nil {
		C.em_model_free(&s.m.m)
		s.m = nil
	}
	return nil
}

// Run scores one window. The returned slice aliases a buffer the next Run
// overwrites; the caller reads it before pushing more audio, exactly as the
// openWakeWord path does.
func (s *Single) Run(window []float32) ([]float32, error) {
	if s.m == nil {
		return nil, ErrClosed
	}
	if len(window) != s.n {
		return nil, fmt.Errorf(
			"ort: model was opened for a %d-sample window but got %d — "+
				"the sidecar and the .onnx disagree", s.n, len(window))
	}
	return s.m.run(window, s.shape)
}
