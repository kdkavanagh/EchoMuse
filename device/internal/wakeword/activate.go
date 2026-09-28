// Package wakeword activates the speech assets named in session.ready on the
// device detector (SPEC §5.1, §16.5): fetch missing files, verify hashes, load
// runtime+graph+sidecar, switch between hops, then delete unreferenced files.
package wakeword

import (
	"context"
	"errors"
	"fmt"
	"os"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
	"github.com/wilbowes/EchoMuse/internal/wakeword/ort"
)

// SpeechDir is where speech assets live (SPEC §16.5).
const SpeechDir = "/data/local/share/echomuse/speech"

// Asset file extensions (SPEC §16.5).
const (
	extRuntime = "so"
	extGraph   = "onnx"
	extSidecar = "json"
)

// Assets is session.ready `assets`.
type Assets struct {
	RuntimeSHA256 string `json:"runtime_sha256"`
	GraphSHA256   string `json:"graph_sha256"`
	SidecarSHA256 string `json:"sidecar_sha256"`
}

// Paths are the installed files of one Assets set.
type Paths struct {
	Runtime, Graph, Sidecar string
}

// LoadFunc builds a scorer from installed, hash-verified files.
type LoadFunc func(p Paths) (detector.Scorer, error)

// LoadORT loads the runtime with dlopen and the graph with the SPEC §16.5
// session options, and rejects a graph whose IO disagrees with its sidecar.
func LoadORT(p Paths) (detector.Scorer, error) {
	sc, err := bcresnet.LoadSidecar(p.Sidecar)
	if err != nil {
		return nil, err
	}
	rt, err := ort.Open(p.Runtime)
	if err != nil {
		return nil, err
	}
	sess, err := rt.NewSession(p.Graph)
	if err != nil {
		return nil, err
	}
	if err := sc.CheckGraphIO(sess.InputLen(), sess.OutputCount()); err != nil {
		sess.Close()
		return nil, err
	}
	s, err := bcresnet.NewScorer(sess, sc)
	if err != nil {
		sess.Close()
		return nil, err
	}
	return s, nil
}

// Activator keeps the detector on the speech assets the controller named.
// Its methods are serialized.
type Activator struct {
	det   *detector.Detector
	store *assets.Store
	load  LoadFunc

	mu     sync.Mutex
	active *Assets // assets the detector is scoring with; nil when none
}

// NewActivator returns an Activator for det over store, loading with load
// (LoadORT on the device).
func NewActivator(det *detector.Detector, store *assets.Store, load LoadFunc) *Activator {
	return &Activator{det: det, store: store, load: load}
}

// Activate makes the detector score with a. While files download the current
// graph keeps scoring. An asset the controller cannot supply reports
// wake_unavailable=missing_asset; a file that fails to load reports
// load_failed. Either way the non-named graph stops. On success the switch is
// atomic between hops with a new scorer revision, and unreferenced speech
// files are deleted. A transport or context error that leaves an asset
// missing also reports missing_asset and is returned.
func (a *Activator) Activate(ctx context.Context, tr assets.Transport, want Assets, th detector.Thresholds) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.active != nil && *a.active == want {
		return a.det.SetThresholds(th)
	}

	var p Paths
	for _, f := range []struct {
		sha, ext string
		dst      *string
	}{
		{want.RuntimeSHA256, extRuntime, &p.Runtime},
		{want.GraphSHA256, extGraph, &p.Graph},
		{want.SidecarSHA256, extSidecar, &p.Sidecar},
	} {
		path, err := a.install(ctx, tr, f.sha, f.ext)
		if err != nil {
			a.unavailable(detector.UnavailableMissingAsset, err)
			return err
		}
		*f.dst = path
	}

	scorer, err := a.load(p)
	if err != nil {
		a.unavailable(detector.UnavailableLoadFailed, err)
		return err
	}
	if err := a.det.SetModel(detector.Model{Scorer: scorer, GraphSHA256: want.GraphSHA256, Thresholds: th}); err != nil {
		scorer.Close()
		a.unavailable(detector.UnavailableLoadFailed, err)
		return err
	}
	w := want
	a.active = &w
	if err := a.store.GC([]string{want.RuntimeSHA256, want.GraphSHA256, want.SidecarSHA256}); err != nil {
		return fmt.Errorf("wakeword: gc: %w", err)
	}
	return nil
}

// install re-verifies an installed file (a corrupt one is removed) and
// fetches it if absent.
func (a *Activator) install(ctx context.Context, tr assets.Transport, sha, ext string) (string, error) {
	if _, err := os.Stat(a.store.Path(sha, ext)); err == nil {
		if err := a.store.Verify(sha, ext); err != nil && !errors.Is(err, assets.ErrHashMismatch) {
			return "", err
		}
	}
	return a.store.Ensure(ctx, tr, sha, ext)
}

// Unavailable stops scoring and reports wake_unavailable (missing_asset or
// load_failed); the next Activate loads its assets afresh.
func (a *Activator) Unavailable(reason detector.UnavailableReason, err error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.unavailable(reason, err)
}

func (a *Activator) unavailable(reason detector.UnavailableReason, err error) {
	a.active = nil
	_ = a.det.SetUnavailable(reason, err.Error())
}
