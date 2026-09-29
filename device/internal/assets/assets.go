// Package assets keeps hash-named files fetched over the assets socket
// (SPEC §16.1, §16.5): speech assets (`<sha256>.{so,onnx,json}`) and alert
// sounds (`<sha256>.wav`), each in its own directory.
//
// A download goes to `<sha256>.part`, resumes by offset after a disconnect,
// is verified against its SHA-256, synced, and renamed into place; the
// directory is synced after the rename. An installed file is never partial.
package assets

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
)

// ErrNotFound: the controller has no asset with the requested hash.
var ErrNotFound = errors.New("asset not found")

// ErrHashMismatch: the downloaded or installed bytes do not hash to their name.
var ErrHashMismatch = errors.New("asset hash mismatch")

// Transport fetches one asset from offset, writing its bytes to w in order,
// and returns the asset's total size. It returns ErrNotFound when the
// controller has no such asset. One call is one request on the assets socket.
type Transport interface {
	Fetch(ctx context.Context, sha256 string, offset int64, w io.Writer) (size int64, err error)
}

const partExt = "part"

// Store is one asset directory. Its methods are safe for concurrent use;
// downloads into one Store are serialized.
type Store struct {
	dir string
	mu  sync.Mutex
}

// Open returns the Store for dir, creating the directory.
func Open(dir string) (*Store, error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, fmt.Errorf("assets: %w", err)
	}
	return &Store{dir: dir}, nil
}

// Path is where the asset sha with extension ext is installed.
func (s *Store) Path(sha, ext string) string {
	return filepath.Join(s.dir, sha+"."+ext)
}

// Has reports whether an asset with this hash is installed, with any extension.
func (s *Store) Has(sha string) bool {
	if validSHA(sha) != nil {
		return false
	}
	names, err := s.names()
	if err != nil {
		return false
	}
	for _, n := range names {
		if h, ext, ok := splitName(n); ok && h == sha && ext != partExt {
			return true
		}
	}
	return false
}

// List returns the sorted hashes of every installed asset.
func (s *Store) List() ([]string, error) {
	names, err := s.names()
	if err != nil {
		return nil, err
	}
	seen := map[string]bool{}
	var out []string
	for _, n := range names {
		if h, ext, ok := splitName(n); ok && ext != partExt && !seen[h] {
			seen[h] = true
			out = append(out, h)
		}
	}
	sort.Strings(out)
	return out, nil
}

// Ensure installs asset sha as `<sha>.<ext>` unless it is already installed,
// and returns its path. A `.part` left by an earlier attempt is resumed; if the
// resumed file does not verify, it is discarded and fetched once more from
// offset 0.
func (s *Store) Ensure(ctx context.Context, tr Transport, sha, ext string) (string, error) {
	if err := validSHA(sha); err != nil {
		return "", err
	}
	if err := validExt(ext); err != nil {
		return "", err
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	final := s.Path(sha, ext)
	if _, err := os.Stat(final); err == nil {
		return final, nil
	}
	part := s.Path(sha, partExt)
	resumed, err := s.fetch(ctx, tr, sha, part)
	if errors.Is(err, ErrHashMismatch) && resumed {
		if err = os.Remove(part); err == nil {
			_, err = s.fetch(ctx, tr, sha, part)
		}
	}
	if err != nil {
		if errors.Is(err, ErrHashMismatch) || errors.Is(err, ErrNotFound) {
			os.Remove(part)
		}
		return "", err
	}
	if err := os.Rename(part, final); err != nil {
		return "", fmt.Errorf("assets: %w", err)
	}
	if err := syncDir(s.dir); err != nil {
		return "", err
	}
	return final, nil
}

// fetch appends the rest of the asset to part and verifies the whole file.
// resumed reports whether it started from a non-zero offset.
func (s *Store) fetch(ctx context.Context, tr Transport, sha, part string) (resumed bool, err error) {
	f, err := os.OpenFile(part, os.O_RDWR|os.O_CREATE, 0o644)
	if err != nil {
		return false, fmt.Errorf("assets: %w", err)
	}
	defer f.Close()

	h := sha256.New()
	offset, err := io.Copy(h, f)
	if err != nil {
		return false, fmt.Errorf("assets: read %s: %w", part, err)
	}
	resumed = offset > 0
	cw := &countWriter{w: io.MultiWriter(f, h)}
	size, err := tr.Fetch(ctx, sha, offset, cw)
	if err != nil {
		return resumed, fmt.Errorf("assets: fetch %s at %d: %w", sha, offset, err)
	}
	if offset+cw.n != size {
		return resumed, fmt.Errorf("assets: %s: %d bytes on disk, asset is %d: %w", sha, offset+cw.n, size, ErrHashMismatch)
	}
	if got := hex.EncodeToString(h.Sum(nil)); got != sha {
		return resumed, fmt.Errorf("assets: %s: content hashes to %s: %w", sha, got, ErrHashMismatch)
	}
	if err := f.Sync(); err != nil {
		return resumed, fmt.Errorf("assets: sync %s: %w", part, err)
	}
	return resumed, nil
}

// Verify re-hashes an installed asset. A file that no longer matches its
// name is removed and reported as ErrHashMismatch.
func (s *Store) Verify(sha, ext string) error {
	if err := validSHA(sha); err != nil {
		return err
	}
	if err := validExt(ext); err != nil {
		return err
	}
	path := s.Path(sha, ext)
	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("assets: %w", err)
	}
	h := sha256.New()
	_, err = io.Copy(h, f)
	f.Close()
	if err != nil {
		return fmt.Errorf("assets: read %s: %w", path, err)
	}
	if got := hex.EncodeToString(h.Sum(nil)); got != sha {
		os.Remove(path)
		return fmt.Errorf("assets: %s hashes to %s: %w", path, got, ErrHashMismatch)
	}
	return nil
}

// GC removes every hash-named file, installed or partial, whose hash is not
// in keep. Files not named `<sha256>.<ext>` are left alone.
func (s *Store) GC(keep []string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	kept := make(map[string]bool, len(keep))
	for _, k := range keep {
		kept[k] = true
	}
	names, err := s.names()
	if err != nil {
		return err
	}
	removed := false
	for _, n := range names {
		if h, _, ok := splitName(n); ok && !kept[h] {
			if err := os.Remove(filepath.Join(s.dir, n)); err != nil && !errors.Is(err, os.ErrNotExist) {
				return fmt.Errorf("assets: %w", err)
			}
			removed = true
		}
	}
	if removed {
		return syncDir(s.dir)
	}
	return nil
}

func (s *Store) names() ([]string, error) {
	ents, err := os.ReadDir(s.dir)
	if err != nil {
		return nil, fmt.Errorf("assets: %w", err)
	}
	out := make([]string, 0, len(ents))
	for _, e := range ents {
		if e.Type().IsRegular() {
			out = append(out, e.Name())
		}
	}
	return out, nil
}

func splitName(name string) (sha, ext string, ok bool) {
	sha, ext, ok = strings.Cut(name, ".")
	if !ok || validSHA(sha) != nil || validExt(ext) != nil {
		return "", "", false
	}
	return sha, ext, true
}

func validSHA(sha string) error {
	if !IsSHA256(sha) {
		return fmt.Errorf("assets: %q is not a lowercase SHA-256 hex digest", sha)
	}
	return nil
}

// IsSHA256 reports whether s is a lowercase hex SHA-256, the name of every
// asset.
func IsSHA256(s string) bool {
	if len(s) != sha256.Size*2 {
		return false
	}
	for i := range len(s) {
		if c := s[i]; !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

func validExt(ext string) error {
	if ext == "" || len(ext) > 8 {
		return fmt.Errorf("assets: bad extension %q", ext)
	}
	for _, c := range ext {
		if !(c >= 'a' && c <= 'z' || c >= '0' && c <= '9') {
			return fmt.Errorf("assets: bad extension %q", ext)
		}
	}
	return nil
}

func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return fmt.Errorf("assets: %w", err)
	}
	defer d.Close()
	if err := d.Sync(); err != nil {
		return fmt.Errorf("assets: sync %s: %w", dir, err)
	}
	return nil
}

type countWriter struct {
	w io.Writer
	n int64
}

func (c *countWriter) Write(p []byte) (int, error) {
	n, err := c.w.Write(p)
	c.n += int64(n)
	return n, err
}
