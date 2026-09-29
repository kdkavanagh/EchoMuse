package alerts

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"

	"github.com/wilbowes/EchoMuse/internal/assets"
)

// maxSnapshotPageObjects is the alert.snapshot page limit (SPEC §16.4).
const maxSnapshotPageObjects = 128

// AlertAck is the WIRE alert.ack body. DeliveryEpoch is the device's epoch
// after handling the message, null before any snapshot.
type AlertAck struct {
	DeliveryEpoch  *string `json:"delivery_epoch"`
	AppliedThrough uint64  `json:"applied_through"`
	Durable        bool    `json:"durable"`
	NeedSnapshot   bool    `json:"need_snapshot"`
}

type deltaBody struct {
	DeliveryEpoch string            `json:"delivery_epoch"`
	Sequence      uint64            `json:"sequence"`
	Objects       []json.RawMessage `json:"objects"`
}

type snapshotPage struct {
	DeliveryEpoch string            `json:"delivery_epoch"`
	HighWaterMark uint64            `json:"high_water_mark"`
	PageIndex     int               `json:"page_index"`
	PageCount     int               `json:"page_count"`
	SHA256        string            `json:"sha256"`
	Objects       []json.RawMessage `json:"objects"`
}

// snapshotStage accumulates the pages of one snapshot until it can be verified.
type snapshotStage struct {
	epoch  string
	high   uint64
	count  int
	sha    string
	next   int
	values []any // decoded objects, in page order
}

// ApplyDelta handles one WIRE alert.delta body. The delta is applied only when
// its epoch is the device's and its sequence is the acknowledged sequence plus
// one; otherwise, or if any object is invalid, the state is unchanged and the
// ack asks for a snapshot (SPEC §16.4). A successful ack is sent only after
// the transaction is durable.
func (e *Executor) ApplyDelta(body []byte) AlertAck {
	e.mu.Lock()
	defer e.mu.Unlock()
	st := e.st.st
	var d deltaBody
	if err := json.Unmarshal(body, &d); err != nil || st.corrupt || st.epoch == nil ||
		d.DeliveryEpoch != *st.epoch || d.Sequence != st.acked+1 {
		return e.ackLocked(true, true)
	}
	t := txn{Delivery: &deliveryPos{Epoch: d.DeliveryEpoch, Acked: d.Sequence}}
	delivered := make(map[string]*occurrence, len(st.occ)+len(d.Objects))
	for id, o := range st.occ {
		delivered[id] = o
	}
	for _, raw := range d.Objects {
		v, err := decodeGeneric(raw)
		if err != nil {
			return e.ackLocked(true, true)
		}
		o, tomb, err := parseObject(v)
		if err != nil {
			return e.ackLocked(true, true)
		}
		if tomb != nil {
			// A tombstone also removes a local child with that ID: the
			// controller rejected its snooze or deleted it (SPEC §16.4 merge).
			id := tomb.OccurrenceID
			t.DelOcc = append(t.DelOcc, id)
			t.DelChildren = append(t.DelChildren, id)
			t.DelArmed = append(t.DelArmed, id)
			t.DelRings = append(t.DelRings, id)
			delete(delivered, id)
			continue
		}
		t.PutOcc = append(t.PutOcc, o)
		e.armOccurrenceLocked(o, &t)
		delivered[o.OccurrenceID] = o
		supersedeChildren(st, o, &t)
	}
	dropSettledOps(st, delivered, &t)
	if err := e.st.commit(&t); err != nil {
		return e.ackLocked(true, false)
	}
	for _, o := range t.PutOcc {
		e.cacheSoundLocked(o.Sound)
	}
	e.reconcileWakeLockLocked()
	e.signal()
	return e.ackLocked(false, true)
}

// ApplySnapshotPage stages one WIRE alert.snapshot page. done is false while
// more pages are expected. On the last page the digest over the canonical
// JSON array of all pages' objects is verified and the snapshot is installed
// in one durable transaction, keeping protected local tombstones and pending
// operations (SPEC §16.4). Any inconsistency discards the staged pages and
// acks need_snapshot.
func (e *Executor) ApplySnapshotPage(body []byte) (ack AlertAck, done bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	var p snapshotPage
	if err := json.Unmarshal(body, &p); err != nil || p.PageCount <= 0 || p.PageIndex < 0 ||
		p.PageIndex >= p.PageCount || len(p.Objects) > maxSnapshotPageObjects || !assets.IsSHA256(p.SHA256) {
		return e.rejectSnapshotLocked(), true
	}
	if p.PageIndex == 0 {
		e.stage = &snapshotStage{epoch: p.DeliveryEpoch, high: p.HighWaterMark, count: p.PageCount, sha: p.SHA256}
	}
	sg := e.stage
	if sg == nil || sg.next != p.PageIndex || sg.epoch != p.DeliveryEpoch || sg.high != p.HighWaterMark ||
		sg.count != p.PageCount || sg.sha != p.SHA256 {
		return e.rejectSnapshotLocked(), true
	}
	for _, raw := range p.Objects {
		v, err := decodeGeneric(raw)
		if err != nil {
			return e.rejectSnapshotLocked(), true
		}
		if _, tomb, err := parseObject(v); err != nil || tomb != nil {
			return e.rejectSnapshotLocked(), true // snapshots carry live occurrences only
		}
		sg.values = append(sg.values, v)
	}
	sg.next++
	if sg.next < sg.count {
		return AlertAck{}, false
	}
	e.stage = nil
	canon := []byte{'['}
	for i, v := range sg.values {
		if i > 0 {
			canon = append(canon, ',')
		}
		var err error
		if canon, err = appendCanonical(canon, v); err != nil {
			return e.ackLocked(true, true), true
		}
	}
	canon = append(canon, ']')
	sum := sha256.Sum256(canon)
	if hex.EncodeToString(sum[:]) != sg.sha {
		return e.ackLocked(true, true), true
	}

	st := e.st.st
	t := txn{Reset: true, Delivery: &deliveryPos{Epoch: sg.epoch, Acked: sg.high}}
	delivered := make(map[string]*occurrence, len(sg.values))
	for _, v := range sg.values {
		o, _, _ := parseObject(v)
		t.PutOcc = append(t.PutOcc, o)
		e.armOccurrenceLocked(o, &t)
		delivered[o.OccurrenceID] = o
		supersedeChildren(st, o, &t)
	}
	for id, c := range st.children {
		if c.Resolved && delivered[id] == nil {
			t.DelChildren = append(t.DelChildren, id)
			t.DelArmed = append(t.DelArmed, id)
			t.DelRings = append(t.DelRings, id)
		}
	}
	for id, c := range st.children {
		if !containsString(t.DelChildren, id) {
			e.armOccurrenceLocked(c.Occurrence, &t)
		}
	}
	for id := range st.rings {
		if delivered[id] == nil && st.children[id] == nil {
			t.DelRings = append(t.DelRings, id)
		}
	}
	dropSettledOps(st, delivered, &t)
	if err := e.st.commit(&t); err != nil {
		return e.ackLocked(true, false), true
	}
	for _, o := range t.PutOcc {
		e.cacheSoundLocked(o.Sound)
	}
	e.reconcileWakeLockLocked()
	e.signal()
	return e.ackLocked(false, true), true
}

func containsString(xs []string, want string) bool {
	for _, x := range xs {
		if x == want {
			return true
		}
	}
	return false
}

func (e *Executor) rejectSnapshotLocked() AlertAck {
	e.stage = nil
	return e.ackLocked(true, true)
}

// supersedeChildren removes local snooze children replaced by delivered
// occurrence o, moving a child's ring record so a ringing child keeps its
// first-ring time and deadline under the delivered ID.
func supersedeChildren(st *state, o *occurrence, t *txn) {
	for id, c := range st.children {
		if !supersedes(o, c) {
			continue
		}
		t.DelChildren = append(t.DelChildren, id)
		t.DelArmed = append(t.DelArmed, id)
		if r := st.rings[id]; r != nil && id != o.OccurrenceID {
			moved := *r
			moved.OccurrenceID = o.OccurrenceID
			t.DelRings = append(t.DelRings, id)
			t.PutRings = append(t.PutRings, &moved)
		}
	}
}

// dropSettledOps removes operations the controller has answered whose target
// is no longer delivered (WIRE alert.op_result); unanswered operations are
// never dropped, whatever the delivery epoch.
func dropSettledOps(st *state, delivered map[string]*occurrence, t *txn) {
	for id, op := range st.ops {
		if op.Result != nil && !targetDelivered(op, delivered) {
			t.DelOps = append(t.DelOps, id)
		}
	}
}

func targetDelivered(op *localOp, delivered map[string]*occurrence) bool {
	if delivered[op.OccurrenceID] != nil {
		return true
	}
	for _, o := range delivered {
		if opTargets(op, o) {
			return true
		}
	}
	return false
}

// ackLocked reports the device's durable delivery position.
func (e *Executor) ackLocked(needSnapshot, durable bool) AlertAck {
	st := e.st.st
	a := AlertAck{AppliedThrough: st.acked, Durable: durable, NeedSnapshot: needSnapshot || st.corrupt}
	if st.epoch != nil {
		epoch := *st.epoch
		a.DeliveryEpoch = &epoch
	}
	return a
}

// CanonicalJSON re-encodes one JSON value in the canonical form of SPEC
// §16.3/§16.4, byte-identical to Python's
// json.dumps(v, sort_keys=True, separators=(",", ":"), ensure_ascii=False).
func CanonicalJSON(raw []byte) ([]byte, error) {
	v, err := decodeGeneric(raw)
	if err != nil {
		return nil, err
	}
	return appendCanonical(nil, v)
}
