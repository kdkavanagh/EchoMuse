package alerts

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func ringTxn(id string) *txn {
	return &txn{PutRings: []*ringRecord{{OccurrenceID: id, BootID: bootA, FirstMonoNs: 1, DeadlineMonoNs: 2}}}
}

func openTestStore(t *testing.T, root string) *store {
	t.Helper()
	s, err := openStore(root)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = s.close() })
	return s
}

func ringIDs(s *store) map[string]bool {
	out := map[string]bool{}
	for id := range s.st.rings {
		out[id] = true
	}
	return out
}

func commitAll(t *testing.T, s *store, ids ...string) {
	t.Helper()
	for _, id := range ids {
		if err := s.commit(ringTxn(id)); err != nil {
			t.Fatal(err)
		}
	}
}

func TestJournalReplayDropsOnlyTornTail(t *testing.T) {
	for name, tail := range map[string]func(rec []byte) []byte{
		"partial length":  func(rec []byte) []byte { return rec[:3] },
		"partial payload": func(rec []byte) []byte { return rec[:len(rec)-6] },
		"torn final record": func(rec []byte) []byte {
			torn := bytes.Clone(rec)
			torn[len(torn)-5] ^= 0xff // payload damaged, length intact
			return torn
		},
		"zeroed tail": func(rec []byte) []byte { return make([]byte, 12) },
	} {
		t.Run(name, func(t *testing.T) {
			root := t.TempDir()
			s := openTestStore(t, root)
			commitAll(t, s, "a", "b", "c")
			good, _ := os.ReadFile(filepath.Join(root, journalName))
			// A fourth record, captured and removed, supplies realistic tail bytes.
			commitAll(t, s, "d")
			all, _ := os.ReadFile(filepath.Join(root, journalName))
			s.close()
			rec := all[len(good):]
			if err := os.WriteFile(filepath.Join(root, journalName), append(bytes.Clone(good), tail(rec)...), 0o640); err != nil {
				t.Fatal(err)
			}

			s2 := openTestStore(t, root)
			if s2.st.corrupt {
				t.Fatal("torn tail marked corrupt")
			}
			if got := ringIDs(s2); len(got) != 3 || !got["a"] || !got["b"] || !got["c"] {
				t.Fatalf("replayed %v", got)
			}
			if fi, _ := os.Stat(filepath.Join(root, journalName)); fi.Size() != int64(len(good)) {
				t.Fatalf("tail not truncated: %d != %d", fi.Size(), len(good))
			}
			commitAll(t, s2, "e")
			s2.close()
			if got := ringIDs(openTestStore(t, root)); !got["e"] || got["d"] {
				t.Fatalf("after append %v", got)
			}
		})
	}
}

func TestJournalMidFileChecksumFailureIsCorruptNotTruncated(t *testing.T) {
	root := t.TempDir()
	s := openTestStore(t, root)
	commitAll(t, s, "a")
	first, _ := os.ReadFile(filepath.Join(root, journalName))
	commitAll(t, s, "b", "c")
	s.close()
	path := filepath.Join(root, journalName)
	data, _ := os.ReadFile(path)
	damaged := bytes.Clone(data)
	damaged[len(first)+recordHeader+2] ^= 0x20 // inside record "b"
	if err := os.WriteFile(path, damaged, 0o640); err != nil {
		t.Fatal(err)
	}

	s2 := openTestStore(t, root)
	if !s2.st.corrupt {
		t.Fatal("mid-file checksum failure not reported")
	}
	if got := ringIDs(s2); len(got) != 1 || !got["a"] {
		t.Fatalf("recovered %v, want only the records before the damage", got)
	}
	kept, err := os.ReadFile(path + corruptSuffix)
	if err != nil || !bytes.Equal(kept, damaged) {
		t.Fatalf("damaged journal not preserved: %v", err)
	}
	// Later operations are durable, and the mark survives restarts until a
	// controller snapshot is installed.
	commitAll(t, s2, "x")
	s2.close()
	s3 := openTestStore(t, root)
	if !s3.st.corrupt || !ringIDs(s3)["x"] {
		t.Fatalf("corrupt=%v rings=%v", s3.st.corrupt, ringIDs(s3))
	}
	if err := s3.commit(&txn{Reset: true, Delivery: &deliveryPos{Epoch: "E", Acked: 1}}); err != nil {
		t.Fatal(err)
	}
	s3.close()
	if openTestStore(t, root).st.corrupt {
		t.Fatal("snapshot install did not clear corruption")
	}
}

func TestCompactionCrashPoints(t *testing.T) {
	for _, point := range []string{"before_rename", "after_rename"} {
		t.Run(point, func(t *testing.T) {
			root := t.TempDir()
			s := openTestStore(t, root)
			commitAll(t, s, "a")
			if err := s.compact(); err != nil { // the previous snapshot
				t.Fatal(err)
			}
			commitAll(t, s, "b")
			crashed := errors.New("crash")
			s.crashHook = func(p string) error {
				if p == point {
					return crashed
				}
				return nil
			}
			commitAll(t, s, "c") // durable in the journal
			if err := s.compact(); !errors.Is(err, crashed) {
				t.Fatalf("compaction did not stop at %s: %v", point, err)
			}
			s.close()

			s2 := openTestStore(t, root)
			if s2.st.corrupt {
				t.Fatal("crash during compaction reported corrupt")
			}
			if got := ringIDs(s2); len(got) != 3 || !got["a"] || !got["b"] || !got["c"] {
				t.Fatalf("after crash %v", got)
			}
			if _, err := os.Stat(filepath.Join(root, snapshotTempName)); !os.IsNotExist(err) {
				t.Fatalf("temporary snapshot left behind: %v", err)
			}
			commitAll(t, s2, "d")
			s2.close()
			if got := ringIDs(openTestStore(t, root)); len(got) != 4 || !got["d"] {
				t.Fatalf("after recovery commit %v", got)
			}
		})
	}
}

func TestCompactionReplacesJournal(t *testing.T) {
	root := t.TempDir()
	s := openTestStore(t, root)
	commitAll(t, s, "a", "b")
	if err := s.compact(); err != nil {
		t.Fatal(err)
	}
	if fi, _ := os.Stat(filepath.Join(root, journalName)); fi.Size() != 0 {
		t.Fatalf("journal not truncated: %d", fi.Size())
	}
	commitAll(t, s, "c")
	s.close()
	s2 := openTestStore(t, root)
	if got := ringIDs(s2); len(got) != 3 || s2.txn != 3 {
		t.Fatalf("rings %v txn %d", got, s2.txn)
	}
}
