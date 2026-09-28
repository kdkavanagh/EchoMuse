package wakeword_test

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"sync"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
)

func digest(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

type transport struct {
	mu      sync.Mutex
	files   map[string][]byte
	fetches int
}

func (t *transport) Fetch(_ context.Context, sha string, off int64, w io.Writer) (int64, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.fetches++
	b, ok := t.files[sha]
	if !ok {
		return 0, assets.ErrNotFound
	}
	_, err := w.Write(b[off:])
	return int64(len(b)), err
}

type scorer struct {
	mu     sync.Mutex
	closed bool
}

func (s *scorer) Score([]int16) (float64, bool, error) { return 0.01, true, nil }
func (s *scorer) Close() error                         { s.mu.Lock(); s.closed = true; s.mu.Unlock(); return nil }

type fixture struct {
	det   *detector.Detector
	store *assets.Store
	tr    *transport
	want  wakeword.Assets
	loads int
	err   error

	mu    sync.Mutex
	stats []detector.Stats
}

func newFixture(t *testing.T) *fixture {
	t.Helper()
	f := &fixture{tr: &transport{files: map[string][]byte{}}}
	det, err := detector.New(detector.Callbacks{
		Profile:        func() detector.Profile { return detector.ProfileIdle },
		ProducingSound: func(uint64, uint64) bool { return false },
		OnCandidate:    func(detector.Candidate) {},
		OnCandidateEnd: func(detector.CandidateEnd) {},
		OnStats:        func(s detector.Stats) { f.mu.Lock(); f.stats = append(f.stats, s); f.mu.Unlock() },
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(det.Close)
	f.det = det
	if f.store, err = assets.Open(t.TempDir()); err != nil {
		t.Fatal(err)
	}
	for i, body := range [][]byte{[]byte("runtime"), []byte("graph"), []byte("sidecar")} {
		sha := digest(body)
		f.tr.files[sha] = body
		switch i {
		case 0:
			f.want.RuntimeSHA256 = sha
		case 1:
			f.want.GraphSHA256 = sha
		case 2:
			f.want.SidecarSHA256 = sha
		}
	}
	return f
}

func (f *fixture) activator() *wakeword.Activator {
	return wakeword.NewActivator(f.det, f.store, func(p wakeword.Paths) (detector.Scorer, error) {
		f.loads++
		if f.err != nil {
			return nil, f.err
		}
		for _, path := range []string{p.Runtime, p.Graph, p.Sidecar} {
			if _, err := os.Stat(path); err != nil {
				return nil, err
			}
		}
		return &scorer{}, nil
	})
}

func (f *fixture) lastStats() detector.Stats {
	f.det.FlushStats()
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.stats[len(f.stats)-1]
}

var th = detector.Thresholds{Idle: 0.9, Playback: 0.65, NearMiss: 0.17}

func TestActivateInstallsSwitchesAndCollects(t *testing.T) {
	f := newFixture(t)
	stale := []byte("old graph")
	if err := os.WriteFile(f.store.Path(digest(stale), "onnx"), stale, 0o644); err != nil {
		t.Fatal(err)
	}
	a := f.activator()
	if err := a.Activate(context.Background(), f.tr, f.want, th); err != nil {
		t.Fatal(err)
	}
	st := f.lastStats()
	if st.GraphSHA256 == nil || *st.GraphSHA256 != f.want.GraphSHA256 || st.WakeUnavailable != nil {
		t.Fatalf("stats %+v", st)
	}
	if f.store.Has(digest(stale)) {
		t.Error("unreferenced speech asset kept after a successful switch")
	}
	// The same assets again: nothing is fetched or reloaded.
	if err := a.Activate(context.Background(), f.tr, f.want, detector.Thresholds{Idle: 0.8, Playback: 0.6, NearMiss: 0.2}); err != nil {
		t.Fatal(err)
	}
	if f.tr.fetches != 3 || f.loads != 1 {
		t.Fatalf("fetches %d loads %d", f.tr.fetches, f.loads)
	}
}

func TestActivateReportsMissingAsset(t *testing.T) {
	f := newFixture(t)
	delete(f.tr.files, f.want.GraphSHA256)
	err := f.activator().Activate(context.Background(), f.tr, f.want, th)
	if !errors.Is(err, assets.ErrNotFound) {
		t.Fatalf("error %v", err)
	}
	st := f.lastStats()
	if st.WakeUnavailable == nil || *st.WakeUnavailable != detector.UnavailableMissingAsset || st.Detail == nil {
		t.Fatalf("stats %+v", st)
	}
	if f.loads != 0 {
		t.Fatal("loaded with a missing asset")
	}
}

func TestActivateReportsLoadFailedAndKeepsFiles(t *testing.T) {
	f := newFixture(t)
	f.err = errors.New("graph emits 2 outputs, sidecar names 3 labels")
	if err := f.activator().Activate(context.Background(), f.tr, f.want, th); err == nil {
		t.Fatal("no error")
	}
	st := f.lastStats()
	if st.WakeUnavailable == nil || *st.WakeUnavailable != detector.UnavailableLoadFailed || *st.Detail != f.err.Error() || st.GraphSHA256 != nil {
		t.Fatalf("stats %+v", st)
	}
}

func TestActivateRefetchesCorruptInstalledFile(t *testing.T) {
	f := newFixture(t)
	if err := os.WriteFile(f.store.Path(f.want.SidecarSHA256, "json"), []byte("corrupt"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := f.activator().Activate(context.Background(), f.tr, f.want, th); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(f.store.Path(f.want.SidecarSHA256, "json"))
	if err != nil || string(got) != "sidecar" {
		t.Fatalf("sidecar %q, %v", got, err)
	}
}
