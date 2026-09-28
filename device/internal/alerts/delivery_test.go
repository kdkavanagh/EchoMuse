package alerts

import (
	"fmt"
	"sort"
	"testing"
	"time"
)

func TestDeltaSequencingAndNeedSnapshot(t *testing.T) {
	h := newHarness(t)
	a := alarm("a", t0+time.Hour.Milliseconds())
	b := alarm("b", t0+2*time.Hour.Milliseconds())
	c := alarm("c", t0+3*time.Hour.Milliseconds())

	if ack := h.delta("E1", 1, a.json()); !ack.NeedSnapshot || ack.DeliveryEpoch != nil {
		t.Fatalf("delta before any snapshot: %+v", ack)
	}
	h.install("E1", 5, a)

	ack := h.delta("E1", 6, b.json())
	if ack.NeedSnapshot || !ack.Durable || ack.AppliedThrough != 6 || *ack.DeliveryEpoch != "E1" {
		t.Fatalf("in-order delta: %+v", ack)
	}
	for name, body := range map[string]func() AlertAck{
		"gap":           func() AlertAck { return h.delta("E1", 8, c.json()) },
		"replay":        func() AlertAck { return h.delta("E1", 6, c.json()) },
		"other epoch":   func() AlertAck { return h.delta("E2", 7, c.json()) },
		"invalid":       func() AlertAck { return h.delta("E1", 7, c.json(), `{"occurrence_id":"nope","revision":1}`) },
		"bad tombstone": func() AlertAck { return h.delta("E1", 7, tombJSON(a.occID(), 7, "gone")) },
	} {
		ack := body()
		if !ack.NeedSnapshot || ack.AppliedThrough != 6 {
			t.Fatalf("%s: %+v", name, ack)
		}
	}
	if q := h.e.State().Queue; len(q) != 2 || containsID(q, c.occID()) {
		t.Fatalf("rejected deltas changed the cache: %v", q)
	}

	ack = h.delta("E1", 7, tombJSON(b.occID(), 7, "deleted"), c.json())
	if ack.NeedSnapshot || ack.AppliedThrough != 7 {
		t.Fatalf("tombstone delta: %+v", ack)
	}
	h.restart()
	hello := h.e.Hello()
	if *hello.DeliveryEpoch != "E1" || hello.AckedSequence != 7 {
		t.Fatalf("hello after restart %+v", hello)
	}
	h.trust()
	h.e.Poll()
	q := h.e.State().Queue
	if len(q) != 2 || q[0] != a.occID() || q[1] != c.occID() {
		t.Fatalf("queue after restart %v", q)
	}
}

// python129 reproduces the controller-side fixture whose digest CPython
// computed: 129 occurrences, two per minute, sorted by due then ID.
func python129() []occSpec {
	base := int64(1790422200000)
	vols := []string{"null", "0.75", "1.0"}
	var specs []occSpec
	for i := 0; i < 129; i++ {
		s := alarm(fmt.Sprintf("sched-%d", i), base+int64(i/2)*60000)
		s.label = fmt.Sprintf("Alarm %d ☀", i)
		s.volume = vols[i%3]
		s.revision = i + 1
		specs = append(specs, s)
	}
	sort.Slice(specs, func(i, j int) bool {
		if specs[i].dueUTC != specs[j].dueUTC {
			return specs[i].dueUTC < specs[j].dueUTC
		}
		return specs[i].occID() < specs[j].occID()
	})
	return specs
}

const python129Digest = "1cea678991e668e7877a2baabbbcc384c430c07510510ae7bf42b8f88fb3374e"

// wireOrder writes an occurrence with unsorted keys and whitespace, as a
// non-canonical but equivalent sender would.
func wireOrder(s occSpec) string {
	return fmt.Sprintf(`{ "volume": %s, "sound": %q, "snooze_ms": %d, "schedule_id": %q, "revision": %d,
	 "ramp_ms": %d, "occurrence_id": %q, "max_ring_ms": %d, "loop_gap_ms": %d, "label": %q,
	 "kind": "alarm", "due_utc_ms": "%d", "due_local": %q }`,
		s.volume, s.sound, s.snoozeMs, s.sched, s.revision, s.rampMs, s.occID(), s.maxRingMs,
		s.loopGapMs, s.label, s.dueUTC, s.key())
}

func TestPagedSnapshotDigestAndProtectedTombstones(t *testing.T) {
	h := newHarness(t)
	specs := python129()
	objs := make([]string, len(specs))
	for i, s := range specs {
		objs[i] = wireOrder(s)
	}
	deliver := func(epoch string, high int, objs []string, digest string) AlertAck {
		t.Helper()
		pages := pagesWithDigest(epoch, high, objs, digest)
		var ack AlertAck
		for i, p := range pages {
			var done bool
			ack, done = h.e.ApplySnapshotPage(p)
			if done != (i == len(pages)-1) {
				t.Fatalf("page %d/%d done=%v ack=%+v", i, len(pages), done, ack)
			}
		}
		return ack
	}

	pages := pagesWithDigest("E1", 200, objs, python129Digest)
	if len(pages) != 2 {
		t.Fatalf("%d pages", len(pages))
	}
	ack := deliver("E1", 200, objs, python129Digest)
	if ack.NeedSnapshot || !ack.Durable || ack.AppliedThrough != 200 {
		t.Fatalf("snapshot install %+v", ack)
	}
	h.trust()
	h.e.Poll()
	if q := h.e.State().Queue; len(q) != 129 || q[0] != specs[0].occID() {
		t.Fatalf("queue %d entries, head %v", len(q), q[:1])
	}

	// The device dismisses the first occurrence before the controller hears.
	first := specs[0].occID()
	res := h.e.Act("", first, actionDismiss, "dashboard")
	if res.Status != StatusDurable {
		t.Fatalf("dismiss %+v", res)
	}

	// A digest mismatch and out-of-order pages are refused without change.
	if ack := deliver("E2", 1, objs, "0000000000000000000000000000000000000000000000000000000000000000"); !ack.NeedSnapshot || ack.AppliedThrough != 200 {
		t.Fatalf("bad digest accepted: %+v", ack)
	}
	if ack, done := h.e.ApplySnapshotPage(pages[1]); !done || !ack.NeedSnapshot {
		t.Fatalf("page 1 without page 0 accepted: %+v", ack)
	}

	// A new controller epoch redelivers the occurrence: the local tombstone
	// survives installation and the pending operation is kept.
	if ack := deliver("E2", 1, objs, python129Digest); ack.NeedSnapshot || *ack.DeliveryEpoch != "E2" || ack.AppliedThrough != 1 {
		t.Fatalf("new epoch snapshot %+v", ack)
	}
	h.e.Poll()
	st := h.e.State()
	if len(st.Queue) != 128 || containsID(st.Queue, first) {
		t.Fatalf("protected tombstone lost: %d queued, first queued=%v", len(st.Queue), containsID(st.Queue, first))
	}
	if ops := h.e.PendingOperations(); len(ops) != 1 || ops[0].OpID != res.OpID {
		t.Fatalf("pending ops %+v", ops)
	}

	// Applied upstream but still delivered: still protected; no longer
	// delivered: the operation is dropped.
	if err := h.e.ApplyOpResult(OpResult{OpID: res.OpID, State: "applied"}); err != nil {
		t.Fatal(err)
	}
	if len(h.e.PendingOperations()) != 0 || len(h.e.st.st.ops) != 1 {
		t.Fatalf("answered op: pending=%d stored=%d", len(h.e.PendingOperations()), len(h.e.st.st.ops))
	}
	h.install("E2", 2, specs[1:]...)
	if len(h.e.st.st.ops) != 0 {
		t.Fatal("settled operation kept after its occurrence left the snapshot")
	}
}
