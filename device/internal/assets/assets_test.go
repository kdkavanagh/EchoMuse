package assets_test

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/assets"
)

func digest(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

type fakeTransport struct {
	data     map[string][]byte
	offsets  []int64
	failOnce int
	bad      bool
	check    func() error
}

func (f *fakeTransport) Fetch(_ context.Context, sha string, offset int64, w io.Writer) (int64, error) {
	f.offsets = append(f.offsets, offset)
	if f.check != nil {
		if err := f.check(); err != nil {
			return 0, err
		}
	}
	b, ok := f.data[sha]
	if !ok {
		return 0, assets.ErrNotFound
	}
	if f.bad {
		b = append([]byte(nil), b...)
		b[0] ^= 0xff
	}
	if offset > int64(len(b)) {
		return int64(len(b)), nil
	}
	remain := b[offset:]
	if f.failOnce > 0 {
		n := f.failOnce
		if n > len(remain) {
			n = len(remain)
		}
		_, _ = w.Write(remain[:n])
		f.failOnce = 0
		return int64(len(b)), errors.New("disconnect")
	}
	_, err := w.Write(remain)
	return int64(len(b)), err
}

func TestEnsureVerifiesAndAtomicallyInstalls(t *testing.T) {
	dir := t.TempDir()
	s, err := assets.Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	body := bytes.Repeat([]byte("echomuse asset\n"), 8192)
	sha := digest(body)
	final := s.Path(sha, "onnx")
	tr := &fakeTransport{data: map[string][]byte{sha: body}}
	tr.check = func() error {
		if _, err := os.Stat(final); !errors.Is(err, os.ErrNotExist) {
			return errors.New("final path visible before fetch completed")
		}
		return nil
	}
	path, err := s.Ensure(context.Background(), tr, sha, "onnx")
	if err != nil {
		t.Fatal(err)
	}
	if path != final {
		t.Fatalf("path %q, want %q", path, final)
	}
	got, err := os.ReadFile(final)
	if err != nil || !bytes.Equal(got, body) {
		t.Fatalf("installed bytes: err=%v equal=%v", err, bytes.Equal(got, body))
	}
	if _, err := os.Stat(s.Path(sha, "part")); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("part remains after install: %v", err)
	}
	if !s.Has(sha) {
		t.Error("Has is false after install")
	}
	if err := s.Verify(sha, "onnx"); err != nil {
		t.Errorf("Verify: %v", err)
	}
	if _, err := s.Ensure(context.Background(), tr, sha, "onnx"); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(tr.offsets, []int64{0}) {
		t.Errorf("installed asset fetched again: offsets %v", tr.offsets)
	}
}

func TestEnsureResumesAfterDisconnect(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	body := bytes.Repeat([]byte{1, 2, 3, 4, 5}, 1000)
	sha := digest(body)
	tr := &fakeTransport{data: map[string][]byte{sha: body}, failOnce: 731}
	if _, err := s.Ensure(context.Background(), tr, sha, "so"); err == nil {
		t.Fatal("first download did not report disconnect")
	}
	if _, err := s.Ensure(context.Background(), tr, sha, "so"); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(tr.offsets, []int64{0, 731}) {
		t.Fatalf("offsets %v, want [0 731]", tr.offsets)
	}
}

func TestEnsureRestartsCorruptPartial(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	body := bytes.Repeat([]byte("correct"), 100)
	sha := digest(body)
	if err := os.WriteFile(s.Path(sha, "part"), []byte("corrupt"), 0o644); err != nil {
		t.Fatal(err)
	}
	tr := &fakeTransport{data: map[string][]byte{sha: body}}
	if _, err := s.Ensure(context.Background(), tr, sha, "json"); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(tr.offsets, []int64{7, 0}) {
		t.Fatalf("offsets %v, want resume then restart", tr.offsets)
	}
}

func TestEnsureRejectsBadControllerBytes(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	body := []byte("correct")
	sha := digest(body)
	tr := &fakeTransport{data: map[string][]byte{sha: body}, bad: true}
	if _, err := s.Ensure(context.Background(), tr, sha, "wav"); !errors.Is(err, assets.ErrHashMismatch) {
		t.Fatalf("error %v, want ErrHashMismatch", err)
	}
	if _, err := os.Stat(s.Path(sha, "wav")); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("bad bytes installed: %v", err)
	}
	if _, err := os.Stat(s.Path(sha, "part")); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("bad part retained: %v", err)
	}
}

func TestEnsureNotFound(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	sha := digest([]byte("absent"))
	_, err := s.Ensure(context.Background(), &fakeTransport{data: map[string][]byte{}}, sha, "onnx")
	if !errors.Is(err, assets.ErrNotFound) {
		t.Fatalf("%v, want ErrNotFound", err)
	}
}

func TestVerifyRemovesCorruption(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	body := []byte("good")
	sha := digest(body)
	path := s.Path(sha, "json")
	if err := os.WriteFile(path, []byte("bad"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := s.Verify(sha, "json"); !errors.Is(err, assets.ErrHashMismatch) {
		t.Fatalf("%v, want ErrHashMismatch", err)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("corrupt installed file remains: %v", err)
	}
}

func TestGCAndList(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	keep := digest([]byte("keep"))
	drop := digest([]byte("drop"))
	for _, p := range []string{s.Path(keep, "onnx"), s.Path(keep, "part"), s.Path(drop, "json"), s.Path(drop, "part")} {
		if err := os.WriteFile(p, nil, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	foreign := filepath.Join(filepath.Dir(s.Path(keep, "onnx")), "README")
	if err := os.WriteFile(foreign, nil, 0o644); err != nil {
		t.Fatal(err)
	}
	want := []string{drop, keep}
	sort.Strings(want)
	list, err := s.List()
	if err != nil || !reflect.DeepEqual(list, want) {
		t.Fatalf("List = %v, %v", list, err)
	}
	if err := s.GC([]string{keep}); err != nil {
		t.Fatal(err)
	}
	for _, p := range []string{s.Path(drop, "json"), s.Path(drop, "part")} {
		if _, err := os.Stat(p); !errors.Is(err, os.ErrNotExist) {
			t.Errorf("%s remains: %v", p, err)
		}
	}
	for _, p := range []string{s.Path(keep, "onnx"), s.Path(keep, "part"), foreign} {
		if _, err := os.Stat(p); err != nil {
			t.Errorf("%s removed: %v", p, err)
		}
	}
}

func TestRejectsUnsafeNames(t *testing.T) {
	s, _ := assets.Open(t.TempDir())
	tr := &fakeTransport{data: map[string][]byte{}}
	for _, tc := range []struct{ sha, ext string }{{"bad", "so"}, {digest(nil), "../so"}, {digest(nil), "SO"}} {
		if _, err := s.Ensure(context.Background(), tr, tc.sha, tc.ext); err == nil {
			t.Errorf("accepted sha=%q ext=%q", tc.sha, tc.ext)
		}
	}
}
