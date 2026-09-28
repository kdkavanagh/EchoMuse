package alerts

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"hash/crc32"
	"io"
	"os"
	"path/filepath"
	"sort"
)

// DefaultRoot is the device alert store directory (SPEC §16.5).
const DefaultRoot = "/data/local/etc/echomuse/alerts"

const (
	snapshotName     = "snapshot.json"
	journalName      = "operations.log"
	snapshotTempName = "snapshot.json.tmp"
	corruptSuffix    = ".corrupt"

	// A journal record is: uint32 LE payload length, JSON payload, uint32 LE
	// CRC32 (IEEE) over the length and payload.
	recordHeader   = 4
	recordTrailer  = 4
	maxRecordBytes = 8 << 20
	// compactAt is the journal size that triggers snapshot compaction.
	compactAt = 256 << 10

	snapshotVersion = 1
)

// diskSnapshot is snapshot.json: the state through journal transaction Through.
type diskSnapshot struct {
	Version     int            `json:"version"`
	Through     uint64         `json:"through"`
	Epoch       *string        `json:"delivery_epoch"`
	Acked       uint64         `json:"acked_sequence"`
	OpSeq       uint64         `json:"op_seq"`
	Corrupt     bool           `json:"corrupt"`
	Occurrences []*occurrence  `json:"occurrences"`
	Operations  []*localOp     `json:"operations"`
	Children    []*localChild  `json:"children"`
	Armed       []*armedRecord `json:"armed"`
	Rings       []*ringRecord  `json:"rings"`
	Clock       *clockAnchor   `json:"clock"`
}

// store is an atomic snapshot plus a length-prefixed, CRC32-protected journal
// of whole transactions (SPEC §10.6 step 3, §16.5).
type store struct {
	root    string
	log     *os.File
	logSize int64
	st      *state
	txn     uint64 // last applied transaction number
	broken  error  // a failed write could not be rolled back; refuse commits
	// crashHook, when set by tests, is called at named compaction points and
	// aborts compaction when it returns an error.
	crashHook func(point string) error
}

// openStore loads the snapshot and replays complete journal records. Only an
// incomplete final record (or a final record torn by a partial sector write)
// is dropped. A damaged snapshot or a bad record before the tail is copied
// aside under the ".corrupt" suffix, and the recovered state is persisted with
// the corrupt mark, which only a controller snapshot clears.
func openStore(root string) (*store, error) {
	if root == "" {
		root = DefaultRoot
	}
	if err := os.MkdirAll(root, 0o750); err != nil {
		return nil, err
	}
	if err := os.Remove(filepath.Join(root, snapshotTempName)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	s := &store{root: root, st: newState()}
	snapRaw, snapErr := s.loadSnapshot()
	f, err := os.OpenFile(filepath.Join(root, journalName), os.O_CREATE|os.O_RDWR, 0o640)
	if err != nil {
		return nil, err
	}
	s.log = f
	data, err := io.ReadAll(f)
	if err != nil {
		f.Close()
		return nil, err
	}
	validEnd, journalCorrupt := s.replay(data)
	if snapErr == nil && !journalCorrupt {
		if validEnd != len(data) {
			if err := f.Truncate(int64(validEnd)); err != nil {
				f.Close()
				return nil, err
			}
			if err := f.Sync(); err != nil {
				f.Close()
				return nil, err
			}
		}
		s.logSize = int64(validEnd)
		return s, nil
	}
	// Quarantine: keep the damaged bytes for diagnosis, then persist what was
	// recovered so later operations are durable again.
	if snapErr != nil {
		if err := writeFileSync(filepath.Join(root, snapshotName+corruptSuffix), snapRaw); err != nil {
			f.Close()
			return nil, err
		}
	}
	if journalCorrupt {
		if err := writeFileSync(filepath.Join(root, journalName+corruptSuffix), data); err != nil {
			f.Close()
			return nil, err
		}
	}
	s.st.corrupt = true
	s.logSize = int64(len(data))
	if err := s.compact(); err != nil {
		f.Close()
		return nil, err
	}
	return s, nil
}

// loadSnapshot returns the raw bytes and an error when the snapshot exists but
// is unusable.
func (s *store) loadSnapshot() ([]byte, error) {
	b, err := os.ReadFile(filepath.Join(s.root, snapshotName))
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var d diskSnapshot
	if err := json.Unmarshal(b, &d); err != nil {
		return b, fmt.Errorf("alerts: snapshot: %w", err)
	}
	if d.Version != snapshotVersion {
		return b, fmt.Errorf("alerts: snapshot version %d", d.Version)
	}
	st := newState()
	st.epoch, st.acked, st.opSeq, st.corrupt, st.clock = d.Epoch, d.Acked, d.OpSeq, d.Corrupt, d.Clock
	for _, o := range d.Occurrences {
		st.occ[o.OccurrenceID] = o
	}
	for _, op := range d.Operations {
		st.ops[op.OpID] = op
	}
	for _, c := range d.Children {
		st.children[c.Occurrence.OccurrenceID] = c
	}
	for _, a := range d.Armed {
		st.armed[a.OccurrenceID] = a
	}
	for _, r := range d.Rings {
		st.rings[r.OccurrenceID] = r
	}
	s.st, s.txn = st, d.Through
	return b, nil
}

// replay applies complete records after s.txn. It returns the end of the last
// good record and whether a bad record precedes the tail.
func (s *store) replay(data []byte) (validEnd int, corrupt bool) {
	off := 0
	for off < len(data) {
		rest := data[off:]
		if len(rest) < recordHeader {
			return off, false // incomplete final length
		}
		n := int(binary.LittleEndian.Uint32(rest))
		if n == 0 {
			if allZero(rest) {
				return off, false // preallocated/zeroed tail
			}
			return off, true
		}
		end := recordHeader + n + recordTrailer
		if n > maxRecordBytes || end > len(rest) {
			if end > len(rest) {
				return off, false // incomplete final record
			}
			return off, true
		}
		want := binary.LittleEndian.Uint32(rest[recordHeader+n:])
		if crc32.ChecksumIEEE(rest[:recordHeader+n]) != want {
			return off, end != len(rest) // a torn final record is the tail
		}
		var t txn
		if err := json.Unmarshal(rest[recordHeader:recordHeader+n], &t); err != nil {
			return off, true
		}
		if t.N > s.txn {
			if t.N != s.txn+1 {
				return off, true
			}
			s.st.apply(&t)
			s.txn = t.N
		}
		off += end
	}
	return off, false
}

func allZero(b []byte) bool {
	for _, c := range b {
		if c != 0 {
			return false
		}
	}
	return true
}

// commit makes t durable as one journal record and then applies it. On error
// the state is unchanged.
func (s *store) commit(t *txn) error {
	if t.empty() {
		return nil
	}
	if s.broken != nil {
		return s.broken
	}
	t.N = s.txn + 1
	payload, err := json.Marshal(t)
	if err != nil {
		return err
	}
	if len(payload) > maxRecordBytes {
		return fmt.Errorf("alerts: journal record of %d bytes exceeds limit", len(payload))
	}
	rec := make([]byte, recordHeader, recordHeader+len(payload)+recordTrailer)
	binary.LittleEndian.PutUint32(rec, uint32(len(payload)))
	rec = append(rec, payload...)
	rec = binary.LittleEndian.AppendUint32(rec, crc32.ChecksumIEEE(rec))
	if _, err := s.log.WriteAt(rec, s.logSize); err != nil {
		s.rollback()
		return err
	}
	if err := s.log.Sync(); err != nil {
		s.rollback()
		return err
	}
	s.logSize += int64(len(rec))
	s.st.apply(t)
	s.txn = t.N
	if t.Reset || s.logSize >= compactAt {
		// The record is already durable; a failed compaction leaves the
		// journal intact and is retried on the next commit.
		_ = s.compact()
	}
	return nil
}

// rollback removes a partially written record so later records never follow
// garbage.
func (s *store) rollback() {
	if err := s.log.Truncate(s.logSize); err != nil {
		s.broken = fmt.Errorf("alerts: journal rollback failed: %w", err)
		return
	}
	if err := s.log.Sync(); err != nil {
		s.broken = fmt.Errorf("alerts: journal rollback sync failed: %w", err)
	}
}

// compact writes the state to a temporary file, syncs it, renames it over the
// snapshot, syncs the directory, then drops the covered journal. The previous
// snapshot stays in place until the rename (SPEC §16.5).
func (s *store) compact() error {
	b, err := json.Marshal(s.snapshotDoc())
	if err != nil {
		return err
	}
	tmp := filepath.Join(s.root, snapshotTempName)
	if err := writeFileSync(tmp, b); err != nil {
		return err
	}
	if err := s.crash("before_rename"); err != nil {
		return err
	}
	if err := os.Rename(tmp, filepath.Join(s.root, snapshotName)); err != nil {
		return err
	}
	if err := s.crash("after_rename"); err != nil {
		return err
	}
	if err := syncDir(s.root); err != nil {
		return err
	}
	if err := s.log.Truncate(0); err != nil {
		return err
	}
	if err := s.log.Sync(); err != nil {
		return err
	}
	s.logSize = 0
	return nil
}

func (s *store) crash(point string) error {
	if s.crashHook == nil {
		return nil
	}
	return s.crashHook(point)
}

func (s *store) snapshotDoc() *diskSnapshot {
	d := &diskSnapshot{
		Version: snapshotVersion, Through: s.txn, Epoch: s.st.epoch, Acked: s.st.acked,
		OpSeq: s.st.opSeq, Corrupt: s.st.corrupt, Clock: s.st.clock,
		Occurrences: []*occurrence{}, Operations: s.st.opsInOrder(),
		Children: []*localChild{}, Armed: []*armedRecord{}, Rings: []*ringRecord{},
	}
	for _, o := range s.st.occ {
		d.Occurrences = append(d.Occurrences, o)
	}
	sort.Slice(d.Occurrences, func(i, j int) bool { return d.Occurrences[i].OccurrenceID < d.Occurrences[j].OccurrenceID })
	for _, c := range s.st.children {
		d.Children = append(d.Children, c)
	}
	sort.Slice(d.Children, func(i, j int) bool {
		return d.Children[i].Occurrence.OccurrenceID < d.Children[j].Occurrence.OccurrenceID
	})
	for _, a := range s.st.armed {
		d.Armed = append(d.Armed, a)
	}
	sort.Slice(d.Armed, func(i, j int) bool { return d.Armed[i].OccurrenceID < d.Armed[j].OccurrenceID })
	for _, r := range s.st.rings {
		d.Rings = append(d.Rings, r)
	}
	sort.Slice(d.Rings, func(i, j int) bool { return d.Rings[i].OccurrenceID < d.Rings[j].OccurrenceID })
	return d
}

func (s *store) close() error {
	return s.log.Close()
}

func writeFileSync(path string, b []byte) error {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o640)
	if err != nil {
		return err
	}
	if _, err = f.Write(b); err == nil {
		err = f.Sync()
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	return err
}

func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	err = d.Sync()
	if cerr := d.Close(); err == nil {
		err = cerr
	}
	return err
}
