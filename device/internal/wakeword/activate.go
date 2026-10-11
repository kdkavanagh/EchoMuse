// Package wakeword activates the speech assets named in session.ready on the
// device detector (SPEC §5.1, §16.5): fetch missing files, verify hashes, load
// runtime+graph+sidecar, switch between hops, then delete unreferenced files.
// The last activation that succeeded is recorded beside the assets, so the
// detector can score with it again before any controller names a model.
package wakeword

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
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

// recordName is the last successful activation's record, in the speech
// directory beside the files it names (Store.GC leaves it alone).
const recordName = "active.json"

// record is what Restore loads: the assets and policy of the last
// activation that succeeded.
type record struct {
	Assets     Assets              `json:"assets"`
	Thresholds detector.Thresholds `json:"thresholds"`
	// Open and Shadow keep nil (absent) distinct from empty, as session.ready does.
	Open   []detector.OpenRule `json:"open_rules"`
	Shadow []detector.OpenRule `json:"shadow_rules"`
}

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

type assetFile struct {
	sha, ext string
	dst      *string
}

// files lists want's three files, each with the Paths field it installs to.
func (want Assets) files(p *Paths) [3]assetFile {
	return [3]assetFile{
		{want.RuntimeSHA256, extRuntime, &p.Runtime},
		{want.GraphSHA256, extGraph, &p.Graph},
		{want.SidecarSHA256, extSidecar, &p.Sidecar},
	}
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

// Activate makes the detector score with a under thresholds th and rules.
// While files download the current graph keeps scoring. An asset the
// controller cannot supply reports wake_unavailable=missing_asset; a file
// that fails to load reports load_failed. Either way the non-named graph
// stops. On success the switch is atomic between hops with a new scorer
// revision, the activation is recorded for Restore, and unreferenced speech
// files are deleted. A transport or context error that leaves an asset
// missing also reports missing_asset and is returned. The same assets again
// only update the policy (and the record).
func (a *Activator) Activate(ctx context.Context, tr assets.Transport, want Assets, th detector.Thresholds, rules detector.Rules) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	rec := record{Assets: want, Thresholds: th, Open: rules.Open, Shadow: rules.Shadow}
	if a.active != nil && *a.active == want {
		if err := a.det.SetPolicy(th, rules); err != nil {
			return err
		}
		return a.save(rec)
	}

	var p Paths
	for _, f := range want.files(&p) {
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
	if err := a.det.SetModel(detector.Model{Scorer: scorer, GraphSHA256: want.GraphSHA256, Thresholds: th, Rules: rules}); err != nil {
		scorer.Close()
		a.unavailable(detector.UnavailableLoadFailed, err)
		return err
	}
	w := want
	a.active = &w
	if err := a.save(rec); err != nil {
		return err
	}
	if err := a.store.GC([]string{want.RuntimeSHA256, want.GraphSHA256, want.SidecarSHA256}); err != nil {
		return fmt.Errorf("wakeword: gc: %w", err)
	}
	return nil
}

// Restore loads the recorded activation from installed files alone, so the
// detector scores while no controller has named a model: after a boot, or
// after an activation failed. It does nothing once ctx is done (a
// controller's activation supersedes it) or while a model is active, and
// with no record or an unusable file it changes nothing and fetches
// nothing. It reports whether it loaded the recorded model.
func (a *Activator) Restore(ctx context.Context) (bool, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if ctx.Err() != nil || a.active != nil {
		return false, nil
	}
	b, err := os.ReadFile(a.recordPath())
	if errors.Is(err, os.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("wakeword: %w", err)
	}
	var rec record
	if err := json.Unmarshal(b, &rec); err != nil {
		return false, fmt.Errorf("wakeword: %s: %w", recordName, err)
	}
	var p Paths
	for _, f := range rec.Assets.files(&p) {
		if err := a.store.Verify(f.sha, f.ext); err != nil {
			return false, err
		}
		*f.dst = a.store.Path(f.sha, f.ext)
	}
	scorer, err := a.load(p)
	if err != nil {
		return false, err
	}
	rules := detector.Rules{Open: rec.Open, Shadow: rec.Shadow}
	if err := a.det.SetModel(detector.Model{Scorer: scorer, GraphSHA256: rec.Assets.GraphSHA256, Thresholds: rec.Thresholds, Rules: rules}); err != nil {
		scorer.Close()
		return false, err
	}
	w := rec.Assets
	a.active = &w
	return true, nil
}

func (a *Activator) recordPath() string { return filepath.Join(a.store.Dir(), recordName) }

// save replaces the record atomically: written, synced, renamed, and the
// directory synced.
func (a *Activator) save(rec record) error {
	b, err := json.Marshal(rec)
	if err != nil {
		return fmt.Errorf("wakeword: record: %w", err)
	}
	path := a.recordPath()
	tmp := path + ".tmp"
	f, err := os.Create(tmp)
	if err != nil {
		return fmt.Errorf("wakeword: record: %w", err)
	}
	_, err = f.Write(b)
	if err == nil {
		err = f.Sync()
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err == nil {
		err = os.Rename(tmp, path)
	}
	if err == nil {
		err = syncDir(a.store.Dir())
	}
	if err != nil {
		os.Remove(tmp)
		return fmt.Errorf("wakeword: record: %w", err)
	}
	return nil
}

func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	return d.Sync()
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
