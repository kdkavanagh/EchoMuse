package alerts

import "sort"

// state is the durable device alert state. It changes only through txn, so a
// journal replay reproduces it exactly.
type state struct {
	// Delivery position (SPEC §16.4); epoch nil until the first snapshot.
	epoch *string
	acked uint64
	// occ holds the controller-delivered live occurrences by occurrence_id.
	occ map[string]*occurrence
	// ops holds local operations by op_id: pending upload until result is set,
	// then kept as protected tombstones until the target is no longer delivered.
	ops map[string]*localOp
	// opSeq is the highest localOp.Seq ever assigned.
	opSeq uint64
	// children holds local snooze children by occurrence_id until a delivered
	// occurrence supersedes them or the controller resolves them.
	children map[string]*localChild
	// armed stores the due monotonic deadline and boot ID with every armed
	// occurrence (SPEC §16.5). An entry from another boot is not reused.
	armed map[string]*armedRecord
	// rings holds the first-ring time and ring deadline of occurrences that
	// have started ringing (SPEC §10.3, §16.5).
	rings map[string]*ringRecord
	// clock is the trusted UTC anchor of the boot that established it.
	clock *clockAnchor
	// corrupt: a damaged journal or snapshot was quarantined; cleared only by
	// installing a controller snapshot (SPEC §16.5 "requires reconciliation").
	corrupt bool
}

// localOp is a journaled dismiss/snooze/expire (SPEC §10.7, WIRE alert.local_operation).
type localOp struct {
	Seq          uint64        `json:"seq"`
	OpID         string        `json:"op_id"`
	Action       Action        `json:"action"` // dismiss|snooze|expire
	OccurrenceID string        `json:"occurrence_id"`
	ScheduleID   string        `json:"schedule_id"`
	Revision     uint64        `json:"revision"`
	DueUTCMs     int64         `json:"due_utc_ms,string"` // the target's due time
	Reason       *ExpireReason `json:"reason"`            // expire only: timed_out|missed
	Source       Source        `json:"source"`
	Child        *ChildRef     `json:"child,omitempty"` // snooze only
	Result       *OpResult     `json:"result,omitempty"`
}

// localChild is a snooze child created on the device.
type localChild struct {
	Occurrence *occurrence `json:"occurrence"`
	OpID       string      `json:"op_id"`
	// Resolved is set once the creating snooze's op_result reported applied;
	// a later snapshot that omits the child then removes it.
	Resolved bool `json:"resolved"`
}

// armedRecord is the persisted monotonic deadline of one armed occurrence.
type armedRecord struct {
	OccurrenceID string `json:"occurrence_id"`
	BootID       string `json:"boot_id"`
	DueMonoNs    int64  `json:"due_mono_ns,string"`
	DueUTCMs     int64  `json:"due_utc_ms,string"`
}

// ringRecord persists when an occurrence first rang and its ring deadline, in
// the monotonic clock of BootID and in UTC (SPEC §10.3, §16.5).
type ringRecord struct {
	OccurrenceID   string `json:"occurrence_id"`
	BootID         string `json:"boot_id"`
	FirstMonoNs    int64  `json:"first_mono_ns,string"`
	DeadlineMonoNs int64  `json:"deadline_mono_ns,string"`
	FirstUTCMs     int64  `json:"first_utc_ms,string"`
	DeadlineUTCMs  int64  `json:"deadline_utc_ms,string"`
}

// clockAnchor maps BootID's monotonic clock to trusted UTC:
// utc_ms = mono_ns/1e6 + OffsetMs. The armed monotonic deadline of an
// occurrence in that boot is (due_utc_ms - OffsetMs) ms.
type clockAnchor struct {
	BootID   string `json:"boot_id"`
	OffsetMs int64  `json:"offset_ms,string"`
}

// deliveryPos is the delivery epoch and acknowledged sequence.
type deliveryPos struct {
	Epoch string `json:"epoch"`
	Acked uint64 `json:"acked"`
}

// txn is one journal record: one whole transaction (SPEC §16.5). Within each
// collection, deletes apply before puts.
type txn struct {
	N           uint64         `json:"txn"`
	Reset       bool           `json:"reset,omitempty"` // snapshot install: replaces occ, clears corrupt
	Delivery    *deliveryPos   `json:"delivery,omitempty"`
	DelOcc      []string       `json:"del_occ,omitempty"`
	PutOcc      []*occurrence  `json:"put_occ,omitempty"`
	DelOps      []string       `json:"del_ops,omitempty"`
	PutOps      []*localOp     `json:"put_ops,omitempty"`
	DelChildren []string       `json:"del_children,omitempty"`
	PutChildren []*localChild  `json:"put_children,omitempty"`
	DelArmed    []string       `json:"del_armed,omitempty"`
	PutArmed    []*armedRecord `json:"put_armed,omitempty"`
	DelRings    []string       `json:"del_rings,omitempty"`
	PutRings    []*ringRecord  `json:"put_rings,omitempty"`
	Clock       *clockAnchor   `json:"clock,omitempty"`
}

func newState() *state {
	return &state{
		occ:      map[string]*occurrence{},
		ops:      map[string]*localOp{},
		children: map[string]*localChild{},
		armed:    map[string]*armedRecord{},
		rings:    map[string]*ringRecord{},
	}
}

func (t *txn) empty() bool {
	return !t.Reset && t.Delivery == nil && t.Clock == nil &&
		len(t.DelOcc)+len(t.PutOcc)+len(t.DelOps)+len(t.PutOps)+len(t.DelArmed)+len(t.PutArmed)+
			len(t.DelChildren)+len(t.PutChildren)+len(t.DelRings)+len(t.PutRings) == 0
}

func (s *state) apply(t *txn) {
	if t.Reset {
		s.occ = map[string]*occurrence{}
		s.armed = map[string]*armedRecord{}
		s.corrupt = false
	}
	if t.Delivery != nil {
		e := t.Delivery.Epoch
		s.epoch, s.acked = &e, t.Delivery.Acked
	}
	for _, id := range t.DelOcc {
		delete(s.occ, id)
	}
	for _, o := range t.PutOcc {
		s.occ[o.OccurrenceID] = o
	}
	for _, id := range t.DelArmed {
		delete(s.armed, id)
	}
	for _, a := range t.PutArmed {
		s.armed[a.OccurrenceID] = a
	}
	for _, id := range t.DelOps {
		delete(s.ops, id)
	}
	for _, op := range t.PutOps {
		s.ops[op.OpID] = op
		if op.Seq > s.opSeq {
			s.opSeq = op.Seq
		}
	}
	for _, id := range t.DelChildren {
		delete(s.children, id)
	}
	for _, c := range t.PutChildren {
		s.children[c.Occurrence.OccurrenceID] = c
	}
	for _, id := range t.DelRings {
		delete(s.rings, id)
	}
	for _, r := range t.PutRings {
		s.rings[r.OccurrenceID] = r
	}
	if t.Clock != nil {
		c := *t.Clock
		s.clock = &c
	}
}

// opsInOrder returns the operations in creation order.
func (s *state) opsInOrder() []*localOp {
	ops := make([]*localOp, 0, len(s.ops))
	for _, op := range s.ops {
		ops = append(ops, op)
	}
	sort.Slice(ops, func(i, j int) bool { return ops[i].Seq < ops[j].Seq })
	return ops
}

// protects reports whether a local operation tombstones o (SPEC §16.4
// protected local tombstones).
func (s *state) protects(o *occurrence) bool {
	for _, op := range s.ops {
		if opTargets(op, o) {
			return true
		}
	}
	return false
}

// opTargets: the same occurrence_id, or the same schedule and due instant (a
// copy whose occurrence key was formatted differently).
func opTargets(op *localOp, o *occurrence) bool {
	return op.OccurrenceID == o.OccurrenceID ||
		(op.ScheduleID == o.ScheduleID && op.DueUTCMs == o.DueUTCMs)
}

// supersedes reports whether delivered occurrence o replaces local child c
// (SPEC §16.3: the device and controller derive the same child; a differing
// key format must not produce a second ring).
func supersedes(o *occurrence, c *localChild) bool {
	co := c.Occurrence
	return o.OccurrenceID == co.OccurrenceID ||
		(o.ScheduleID == co.ScheduleID && o.DueUTCMs == co.DueUTCMs)
}
