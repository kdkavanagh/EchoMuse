package alerts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	// catchUpMs: an unrung occurrence more than 30 min past due expires as
	// missed instead of ringing (SPEC §10.7).
	catchUpMs = int64(30 * time.Minute / time.Millisecond)
	// pollInterval bounds the scheduling part of due-to-audible latency
	// (SPEC §13.1: p95 <=250 ms).
	pollInterval = 50 * time.Millisecond
	// persistRetryNs spaces retries of operations whose persistence failed.
	persistRetryNs = int64(5 * time.Second)

	defaultBootIDPath = "/proc/sys/kernel/random/boot_id"
)

// Local operation actions, expiry reasons and sources (WIRE alert.local_operation).
const (
	actionDismiss = "dismiss"
	actionSnooze  = "snooze"
	actionExpire  = "expire"

	reasonTimedOut = "timed_out"
	reasonMissed   = "missed"

	sourceExecutor = "executor"
)

// Ring-ended reasons (WIRE alert.ring_ended).
const (
	endStopped = "stopped"
	endButton  = "button"
	endEntity  = "entity"
	endLimit   = "limit"
	endRestart = "restart"
)

// Act result statuses (WIRE command.ack).
const (
	StatusApplied  = "applied"
	StatusDurable  = "durable"
	StatusRejected = "rejected"
)

// EventSink receives executor-originated events. Methods are called from the
// goroutine running Run/Poll, never with the executor's lock held, and must
// not block for long.
type EventSink interface {
	AlertState(AlertState)         // WIRE alert.state: on every change
	RingEnded(RingEnded)           // WIRE alert.ring_ended: ring limit or tombstoned occurrence
	LocalOperation(LocalOperation) // WIRE alert.local_operation: executor expiries
	AlertFocus(AlertFocus)         // alert focus and DAC volume changes
}

// Config configures an Executor. Zero values select the device defaults.
type Config struct {
	Root       string // alert store directory; default DefaultRoot
	BootID     string // this boot's ID; default read from BootIDPath
	BootIDPath string // default /proc/sys/kernel/random/boot_id
	Clock      MonoClock
	WakeLock   WakeLockPaths
	Events     EventSink // required
}

// TimerRing is the WIRE alert.ring body (HA timers; never persisted).
type TimerRing struct {
	RingID    string `json:"ring_id"`
	Name      string `json:"name"`
	Sound     string `json:"sound"`
	LoopGapMs int64  `json:"loop_gap_ms"`
	MaxRingMs int64  `json:"max_ring_ms"`
}

// AlertState is the WIRE alert.state body.
type AlertState struct {
	Active       *ActiveState `json:"active"`
	Queue        []string     `json:"queue"`
	ClockTrusted bool         `json:"clock_trusted"`
	WakeLock     string       `json:"wakelock"`
	Store        string       `json:"store"`
}

// ActiveState is the ringing or backgrounded alert of AlertState.
type ActiveState struct {
	ID             string `json:"id"`
	Kind           string `json:"kind"` // alarm|timer
	Name           string `json:"name"`
	Foreground     bool   `json:"foreground"`
	StartedMonoNs  string `json:"started_mono_ns"`
	DeadlineMonoNs string `json:"deadline_mono_ns"`
}

// RingEnded is the WIRE alert.ring_ended body.
type RingEnded struct {
	ID     string `json:"id"`
	Kind   string `json:"kind"`
	Reason string `json:"reason"`
}

// LocalOperation is the WIRE alert.local_operation body.
type LocalOperation struct {
	OpID         string    `json:"op_id"`
	Action       string    `json:"action"`
	OccurrenceID string    `json:"occurrence_id"`
	ScheduleID   string    `json:"schedule_id"`
	Revision     uint64    `json:"revision"`
	Reason       *string   `json:"reason"`
	Source       string    `json:"source"`
	Child        *ChildRef `json:"child,omitempty"`
}

// ChildRef is the snooze child of a snooze LocalOperation.
type ChildRef struct {
	ScheduleID   string `json:"schedule_id"`
	OccurrenceID string `json:"occurrence_id"`
	DueUTCMs     int64  `json:"due_utc_ms,string"`
	DueLocal     string `json:"due_local"`
}

// OpResult is the WIRE alert.op_result body.
type OpResult struct {
	OpID  string  `json:"op_id"`
	State string  `json:"state"` // applied|rejected
	Error *string `json:"error"`
}

// AlertFocus tells the focus and DAC owner which alert holds alert focus.
// Volume is the occurrence volume for the DAC, nil for the current media
// volume (SPEC §16.5). Active false releases alert focus.
type AlertFocus struct {
	Active     bool
	ID         string
	Kind       string
	Foreground bool
	Volume     *float64
}

// ActResult answers Act. Status is StatusDurable once an alarm operation is
// journaled, StatusApplied for a timer stop or for an alarm stop whose
// persistence failed (Error "persistence_failed": stopped in memory only), or
// StatusRejected with an error code. The caller sends command.ack first, then
// Operation as alert.local_operation, then Ended as alert.ring_ended.
type ActResult struct {
	OpID      string
	Status    string
	Error     string
	Operation *LocalOperation
	Ended     *RingEnded
}

// ClockRequest is the WIRE clock.request body.
type ClockRequest struct {
	Nonce string `json:"nonce"`
}

// ClockReply is the WIRE clock.reply body.
type ClockReply struct {
	Nonce string `json:"nonce"`
	UTCMs string `json:"utc_ms"`
}

// ClockInfo is the WIRE session.hello "clock" object.
type ClockInfo struct {
	Trusted bool    `json:"trusted"`
	MonoNs  string  `json:"mono_ns"`
	UTCMs   *string `json:"utc_ms"`
}

// HelloAlerts is the WIRE session.hello "alerts" object. A corrupt store
// reports a null epoch so the controller sends a snapshot.
type HelloAlerts struct {
	DeliveryEpoch *string `json:"delivery_epoch"`
	AckedSequence uint64  `json:"acked_sequence"`
	Wakeup        string  `json:"wakeup"` // ok|alarm_wakeup_unavailable
}

// Executor owns the durable alert cache, the local ring queue, and the alert
// render source. All methods except Fill are safe for concurrent use.
type Executor struct {
	mu      sync.Mutex
	st      *store
	root    string
	bootID  string
	mono    MonoClock
	clock   *clockEstimator
	wake    *wakeLock
	events  EventSink
	stage   *snapshotStage
	timers  map[string]*timerRing
	active  *activeRing
	sounds  map[string][]int16 // validated assets by SHA-256
	nextSeq uint64
	// volatileOps are stops whose persistence failed: the sound stays stopped
	// in memory, and the operation is uploaded and re-persisted (SPEC §10.7).
	volatileOps []*volatileOp
	lastRetryNs int64
	lockKnown   bool
	lockWanted  bool
	lastState   []byte
	lastFocus   AlertFocus
	wakeCh      chan struct{}
	closed      bool
	voice       atomic.Pointer[voice]
	background  atomic.Bool
	burstID     atomic.Pointer[string]
	burstSeq    atomic.Uint64
	mix         mixerState // owned by the Fill goroutine
}

type timerRing struct {
	TimerRing
	arrivedNs int64
}

type activeRing struct {
	id, kind, name string
	volume         *float64
	startedNs      int64
	deadlineNs     int64
	occ            *occurrence // nil for a timer
}

type volatileOp struct {
	op    *localOp
	child *occurrence // snooze child, nil otherwise
	t     *txn        // the transaction to retry
}

// NewExecutor opens the store, restores the clock anchor of this boot, and
// proves the wakelock by acquiring it (which also reconciles a lock left by a
// predecessor in this boot) before releasing it if nothing is armed.
func NewExecutor(cfg Config) (*Executor, error) {
	if cfg.Events == nil {
		return nil, errors.New("alerts: Config.Events is required")
	}
	bootID := cfg.BootID
	if bootID == "" {
		path := cfg.BootIDPath
		if path == "" {
			path = defaultBootIDPath
		}
		b, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("alerts: boot id: %w", err)
		}
		bootID = strings.TrimSpace(string(b))
	}
	if bootID == "" {
		return nil, errors.New("alerts: empty boot id")
	}
	mono := cfg.Clock
	if mono == nil {
		mono = SystemMonoClock{}
	}
	root := cfg.Root
	if root == "" {
		root = DefaultRoot
	}
	st, err := openStore(root)
	if err != nil {
		return nil, err
	}
	e := &Executor{
		st: st, root: root, bootID: bootID, mono: mono,
		clock:   newClockEstimator(bootID, mono, st.st.clock),
		wake:    newWakeLock(cfg.WakeLock),
		events:  cfg.Events,
		timers:  map[string]*timerRing{},
		sounds:  map[string][]int16{},
		nextSeq: st.st.opSeq,
		wakeCh:  make(chan struct{}, 1),
	}
	for _, o := range e.st.st.occ {
		e.cacheSoundLocked(o.Sound)
	}
	for _, c := range e.st.st.children {
		e.cacheSoundLocked(c.Occurrence.Sound)
	}
	_ = e.wake.acquire()
	e.lockKnown, e.lockWanted = true, true
	e.reconcileWakeLockLocked()
	return e, nil
}

// Run polls until ctx ends. Fill runs separately on the mixer goroutine.
func (e *Executor) Run(ctx context.Context) {
	tick := time.NewTicker(pollInterval)
	defer tick.Stop()
	for {
		e.Poll()
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
		case <-e.wakeCh:
		}
	}
}

func (e *Executor) signal() {
	select {
	case e.wakeCh <- struct{}{}:
	default:
	}
}

type pollEvents struct {
	ops          []LocalOperation
	ended        []RingEnded
	state        AlertState
	stateChanged bool
	focus        AlertFocus
	focusChanged bool
}

// Poll performs every transition due at the current monotonic time.
func (e *Executor) Poll() {
	var ev pollEvents
	e.mu.Lock()
	if e.closed {
		e.mu.Unlock()
		return
	}
	now := e.mono.MonoNowNs()
	e.retryVolatileLocked(now)
	e.checkActiveLocked(now, &ev)
	for e.active == nil {
		c := e.headLocked(now)
		if c == nil {
			break
		}
		if reason := e.expiryLocked(c, now); reason != "" {
			op := e.expireLocked(c.occ, reason)
			ev.ops = append(ev.ops, wireOp(op))
			if c.ring != nil {
				ev.ended = append(ev.ended, RingEnded{ID: c.id, Kind: kindAlarm, Reason: endRestart})
			}
			continue
		}
		e.startLocked(c, now)
	}
	e.reconcileWakeLockLocked()
	ev.state, ev.stateChanged = e.stateChangedLocked()
	ev.focus, ev.focusChanged = e.focusChangedLocked()
	e.mu.Unlock()

	for _, op := range ev.ops {
		e.events.LocalOperation(op)
	}
	for _, r := range ev.ended {
		e.events.RingEnded(r)
	}
	if ev.focusChanged {
		e.events.AlertFocus(ev.focus)
	}
	if ev.stateChanged {
		e.events.AlertState(ev.state)
	}
}

// checkActiveLocked ends the active ring at its limit or when its occurrence
// was tombstoned, and follows a local child to the delivered copy that
// superseded it.
func (e *Executor) checkActiveLocked(now int64, ev *pollEvents) {
	a := e.active
	if a == nil {
		return
	}
	if a.occ == nil {
		if now >= a.deadlineNs {
			delete(e.timers, a.id)
			e.endActiveLocked()
			ev.ended = append(ev.ended, RingEnded{ID: a.id, Kind: kindTimer, Reason: endLimit})
		}
		return
	}
	o := e.currentOccurrenceLocked(a.occ)
	if o == nil || !e.executableLocked(o) {
		e.endActiveLocked()
		ev.ended = append(ev.ended, RingEnded{ID: a.id, Kind: kindAlarm, Reason: endStopped})
		return
	}
	a.id, a.occ, a.name, a.volume = o.OccurrenceID, o, o.Label, o.Volume
	if now >= a.deadlineNs {
		op := e.expireLocked(o, reasonTimedOut)
		ev.ops = append(ev.ops, wireOp(op))
		e.endActiveLocked()
		ev.ended = append(ev.ended, RingEnded{ID: a.id, Kind: kindAlarm, Reason: endLimit})
	}
}

// currentOccurrenceLocked finds o in the cache: by ID, or the delivered copy
// with the same schedule and due time.
func (e *Executor) currentOccurrenceLocked(o *occurrence) *occurrence {
	if c := e.findLocked(o.OccurrenceID); c != nil {
		return c
	}
	for _, d := range e.st.st.occ {
		if d.ScheduleID == o.ScheduleID && d.DueUTCMs == o.DueUTCMs {
			return d
		}
	}
	return nil
}

func (e *Executor) endActiveLocked() {
	e.active = nil
	e.voice.Store(nil)
}

type candidate struct {
	id    string
	dueNs int64
	occ   *occurrence
	ring  *ringRecord
	timer *timerRing
}

// headLocked returns the due item first by due time, then ID (SPEC §10.7).
func (e *Executor) headLocked(now int64) *candidate {
	var head *candidate
	consider := func(c *candidate) {
		if c == nil || c.dueNs > now {
			return
		}
		if head == nil || c.dueNs < head.dueNs || (c.dueNs == head.dueNs && c.id < head.id) {
			head = c
		}
	}
	for _, o := range e.armedLocked() {
		consider(e.alarmCandidateLocked(o))
	}
	for _, t := range e.timers {
		consider(&candidate{id: t.RingID, dueNs: t.arrivedNs, timer: t})
	}
	return head
}

// armOccurrenceLocked persists o's due monotonic deadline for this boot when
// UTC is trusted. Until then it remains cached but cannot fire.
func (e *Executor) armOccurrenceLocked(o *occurrence, t *txn) {
	due, ok := e.clock.monoAtUTC(o.DueUTCMs)
	if !ok {
		return
	}
	if !t.Reset {
		if a := e.st.st.armed[o.OccurrenceID]; a != nil &&
			a.BootID == e.bootID && a.DueUTCMs == o.DueUTCMs && a.DueMonoNs == due {
			return
		}
	}
	t.PutArmed = append(t.PutArmed, &armedRecord{
		OccurrenceID: o.OccurrenceID, BootID: e.bootID,
		DueMonoNs: due, DueUTCMs: o.DueUTCMs,
	})
}

// alarmCandidateLocked places o on the monotonic clock. Without trusted UTC a
// wall-clock deadline does not fire; only a ring already started in this boot
// continues (SPEC §10.4, §16.5).
func (e *Executor) alarmCandidateLocked(o *occurrence) *candidate {
	r := e.st.st.rings[o.OccurrenceID]
	var due int64
	if a := e.st.st.armed[o.OccurrenceID]; a != nil &&
		a.BootID == e.bootID && a.DueUTCMs == o.DueUTCMs {
		due = a.DueMonoNs
	} else if r != nil && r.BootID == e.bootID {
		// A ring already started in this boot retains its monotonic identity
		// even if a later clock refresh cannot be persisted.
		due = r.FirstMonoNs
	} else {
		return nil
	}
	return &candidate{id: o.OccurrenceID, dueNs: due, occ: o, ring: r}
}

// expiryLocked returns the expire reason for an alarm at the head of the
// queue: its original ring deadline passed (restart never grants another full
// ring), or it never rang and is past the catch-up window.
func (e *Executor) expiryLocked(c *candidate, now int64) string {
	if c.occ == nil {
		return ""
	}
	if c.ring != nil {
		if dl, ok := e.ringMonoLocked(c.ring.BootID, c.ring.DeadlineMonoNs, c.ring.DeadlineUTCMs); ok && now >= dl {
			return reasonTimedOut
		}
		return ""
	}
	if utc, ok := e.clock.utcNowMs(); ok && utc-c.occ.DueUTCMs > catchUpMs {
		return reasonMissed
	}
	return ""
}

// ringMonoLocked maps a persisted ring instant to this boot's monotonic
// clock: directly within the same boot, through trusted UTC across boots.
func (e *Executor) ringMonoLocked(bootID string, monoNs, utcMs int64) (int64, bool) {
	if bootID == e.bootID {
		return monoNs, true
	}
	return e.clock.monoAtUTC(utcMs)
}

func (e *Executor) startLocked(c *candidate, now int64) {
	a := &activeRing{id: c.id, startedNs: now}
	v := &voice{id: c.id}
	if t := c.timer; t != nil {
		a.kind, a.name = kindTimer, t.Name
		a.deadlineNs = now + t.MaxRingMs*int64(time.Millisecond)
		v.pcm, v.gap = e.soundLocked(t.Sound, t.LoopGapMs)
	} else {
		o := c.occ
		a.kind, a.name, a.volume, a.occ = kindAlarm, o.Label, o.Volume, o
		if r := c.ring; r != nil {
			a.startedNs, _ = e.ringMonoLocked(r.BootID, r.FirstMonoNs, r.FirstUTCMs)
			a.deadlineNs, _ = e.ringMonoLocked(r.BootID, r.DeadlineMonoNs, r.DeadlineUTCMs)
		} else {
			a.deadlineNs = now + o.MaxRingMs*int64(time.Millisecond)
			utc, _ := e.clock.utcNowMs()
			rec := &ringRecord{
				OccurrenceID: o.OccurrenceID, BootID: e.bootID,
				FirstMonoNs: now, DeadlineMonoNs: a.deadlineNs,
				FirstUTCMs: utc, DeadlineUTCMs: utc + o.MaxRingMs,
			}
			// Ringing never waits on storage; if the record cannot be written
			// a restart treats the occurrence as unrung, bounded by catch-up.
			_ = e.st.commit(&txn{PutRings: []*ringRecord{rec}})
		}
		v.pcm, v.gap = e.soundLocked(o.Sound, o.LoopGapMs)
		v.ramp = o.RampMs * sampleRate / 1000
		v.startElapsed = (now - a.startedNs) * sampleRate / int64(time.Second)
	}
	e.active = a
	e.voice.Store(v)
}

// soundLocked only selects already validated memory or the fallback: no file
// I/O, fetch, or codec work can block a due deadline (SPEC §16.5).
func (e *Executor) soundLocked(sound string, loopGapMs int64) ([]int16, int64) {
	if pcm := e.sounds[sound]; sound != SoundFallback && pcm != nil {
		return pcm, loopGapMs * sampleRate / 1000
	}
	return FallbackPCM(), 0
}

func (e *Executor) cacheSoundLocked(sound string) {
	if sound == SoundFallback || e.sounds[sound] != nil {
		return
	}
	if pcm, err := readAsset(e.assetPath(sound), sound); err == nil {
		e.sounds[sound] = pcm
	}
}

// AssetInstalled validates and admits an alert WAV after the asset fetcher has
// atomically installed it. Invalid files remain unavailable and use fallback.
func (e *Executor) AssetInstalled(sha string) error {
	if !validSHA256(sha) {
		return errors.New("alerts: invalid asset hash")
	}
	pcm, err := readAsset(e.assetPath(sha), sha)
	if err != nil {
		return err
	}
	e.mu.Lock()
	e.sounds[sha] = pcm
	e.mu.Unlock()
	return nil
}

func (e *Executor) assetPath(sha string) string {
	return filepath.Join(e.root, "assets", sha+".wav")
}

// armedLocked lists every occurrence that may still ring: delivered or local
// children not tombstoned by a local operation, plus in-memory snooze children
// whose persistence failed and that no delivered copy has superseded.
func (e *Executor) armedLocked() []*occurrence {
	var out []*occurrence
	for _, o := range e.st.st.occ {
		if e.executableLocked(o) {
			out = append(out, o)
		}
	}
	for _, c := range e.st.st.children {
		if e.executableLocked(c.Occurrence) {
			out = append(out, c.Occurrence)
		}
	}
	for _, v := range e.volatileOps {
		if v.child != nil && e.executableLocked(v.child) && !e.deliveredCopyLocked(v.child) {
			out = append(out, v.child)
		}
	}
	return out
}

func (e *Executor) deliveredCopyLocked(child *occurrence) bool {
	lc := &localChild{Occurrence: child}
	for _, o := range e.st.st.occ {
		if supersedes(o, lc) {
			return true
		}
	}
	return false
}

func (e *Executor) executableLocked(o *occurrence) bool {
	if e.st.st.protects(o) {
		return false
	}
	for _, v := range e.volatileOps {
		if opTargets(v.op, o) {
			return false
		}
	}
	return true
}

// findLocked finds an occurrence by ID among delivered, local, and in-memory children.
func (e *Executor) findLocked(id string) *occurrence {
	if o := e.st.st.occ[id]; o != nil {
		return o
	}
	if c := e.st.st.children[id]; c != nil {
		return c.Occurrence
	}
	for _, v := range e.volatileOps {
		if v.child != nil && v.child.OccurrenceID == id {
			return v.child
		}
	}
	return nil
}

func (e *Executor) newOpLocked(opID string, o *occurrence, action, source string) *localOp {
	e.nextSeq++
	return &localOp{
		Seq: e.nextSeq, OpID: opID, Action: action,
		OccurrenceID: o.OccurrenceID, ScheduleID: o.ScheduleID, Revision: o.Revision,
		DueUTCMs: o.DueUTCMs, Source: source,
	}
}

// persistOpLocked journals op, its target's end, and a snooze child in one
// transaction (SPEC §16.5). On failure the operation is kept in memory.
func (e *Executor) persistOpLocked(op *localOp, target, child *occurrence) error {
	t := &txn{
		PutOps: []*localOp{op}, DelArmed: []string{target.OccurrenceID},
		DelRings: []string{target.OccurrenceID},
	}
	if e.st.st.children[target.OccurrenceID] != nil {
		t.DelChildren = []string{target.OccurrenceID}
	}
	if child != nil {
		t.PutChildren = []*localChild{{Occurrence: child, OpID: op.OpID}}
		e.armOccurrenceLocked(child, t)
	}
	if err := e.st.commit(t); err != nil {
		e.volatileOps = append(e.volatileOps, &volatileOp{op: op, child: child, t: t})
		return err
	}
	return nil
}

func (e *Executor) retryVolatileLocked(now int64) {
	if len(e.volatileOps) == 0 || now-e.lastRetryNs < persistRetryNs {
		return
	}
	e.lastRetryNs = now
	kept := e.volatileOps[:0]
	for _, v := range e.volatileOps {
		if v.op.Result == nil && e.st.commit(v.t) == nil {
			continue
		}
		kept = append(kept, v)
	}
	e.volatileOps = kept
}

func (e *Executor) expireLocked(o *occurrence, reason string) *localOp {
	op := e.newOpLocked(NewUUID4().String(), o, actionExpire, sourceExecutor)
	op.Reason = &reason
	_ = e.persistOpLocked(op, o, nil)
	return op
}

func wireOp(o *localOp) LocalOperation {
	return LocalOperation{
		OpID: o.OpID, Action: o.Action, OccurrenceID: o.OccurrenceID, ScheduleID: o.ScheduleID,
		Revision: o.Revision, Reason: o.Reason, Source: o.Source, Child: o.Child,
	}
}

// armedOrRingingLocked reports whether the kernel wakelock is required.
func (e *Executor) armedOrRingingLocked() bool {
	return e.active != nil || len(e.timers) > 0 || len(e.armedLocked()) > 0
}

// reconcileWakeLockLocked holds the wakelock while anything is armed or
// ringing and releases it when the cache is empty (SPEC §16.5).
func (e *Executor) reconcileWakeLockLocked() {
	want := e.armedOrRingingLocked()
	if e.lockKnown && want == e.lockWanted {
		return
	}
	e.lockKnown, e.lockWanted = true, want
	if want {
		_ = e.wake.acquire()
	} else {
		_ = e.wake.release()
	}
}

func (e *Executor) stateLocked() AlertState {
	s := AlertState{Queue: []string{}, ClockTrusted: e.clock.trusted, WakeLock: e.wake.status(), Store: "ok"}
	if e.st.st.corrupt {
		s.Store = "corrupt"
	}
	if a := e.active; a != nil {
		s.Active = &ActiveState{
			ID: a.id, Kind: a.kind, Name: a.name, Foreground: !e.background.Load(),
			StartedMonoNs:  strconv.FormatInt(a.startedNs, 10),
			DeadlineMonoNs: strconv.FormatInt(a.deadlineNs, 10),
		}
	}
	type queued struct {
		id    string
		dueNs int64
	}
	var q []queued
	for _, o := range e.armedLocked() {
		if e.active != nil && o.OccurrenceID == e.active.id {
			continue
		}
		due := int64(math.MaxInt64)
		if a := e.st.st.armed[o.OccurrenceID]; a != nil && a.BootID == e.bootID && a.DueUTCMs == o.DueUTCMs {
			due = a.DueMonoNs
		}
		q = append(q, queued{o.OccurrenceID, due})
	}
	for id, t := range e.timers {
		if e.active == nil || id != e.active.id {
			q = append(q, queued{id, t.arrivedNs})
		}
	}
	sort.Slice(q, func(i, j int) bool {
		if q[i].dueNs != q[j].dueNs {
			return q[i].dueNs < q[j].dueNs
		}
		return q[i].id < q[j].id
	})
	for _, x := range q {
		s.Queue = append(s.Queue, x.id)
	}
	return s
}

func (e *Executor) stateChangedLocked() (AlertState, bool) {
	s := e.stateLocked()
	b, _ := json.Marshal(s)
	if string(b) == string(e.lastState) {
		return s, false
	}
	e.lastState = b
	return s, true
}

func (e *Executor) focusChangedLocked() (AlertFocus, bool) {
	var f AlertFocus
	if a := e.active; a != nil {
		f = AlertFocus{Active: true, ID: a.id, Kind: a.kind, Foreground: !e.background.Load(), Volume: a.volume}
	}
	last := e.lastFocus
	same := f.Active == last.Active && f.ID == last.ID && f.Kind == last.Kind &&
		f.Foreground == last.Foreground && sameVolume(f.Volume, last.Volume)
	e.lastFocus = f
	return f, !same
}

func sameVolume(a, b *float64) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// State returns the current WIRE alert.state body (sent after session.ready).
func (e *Executor) State() AlertState {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.stateLocked()
}

// HandleTimerRing queues an HA timer ring (WIRE alert.ring). It joins the
// alert queue by arrival time and is never persisted; a repeated ring ID is
// ignored.
func (e *Executor) HandleTimerRing(r TimerRing) error {
	if r.RingID == "" || !validSound(r.Sound) || r.LoopGapMs < 0 || r.MaxRingMs <= 0 {
		return errors.New("alerts: invalid alert.ring")
	}
	e.mu.Lock()
	arrived := e.mono.MonoNowNs()
	e.cacheSoundLocked(r.Sound)
	if e.timers[r.RingID] == nil && (e.active == nil || e.active.id != r.RingID) {
		e.timers[r.RingID] = &timerRing{TimerRing: r, arrivedNs: arrived}
	}
	e.reconcileWakeLockLocked()
	e.mu.Unlock()
	e.signal()
	return nil
}

// Act stops or snoozes exactly the named occurrence or timer ring (WIRE
// alert.act, the physical button with source "button"). An empty opID is
// replaced by a fresh UUIDv4. Any canonical RFC 4122 UUID is a valid opID:
// the controller mints UUIDv5 for voice commands and LLM tool calls (SPEC
// §16.7) and UUIDv4 elsewhere. An alarm dismiss/snooze fades the ring within
// 10 ms, then journals the operation, with a snooze's child in the same
// transaction, before reporting StatusDurable (SPEC §10.7). Snooze applies
// only to a ringing alarm. A timer ring is stopped without persistence.
func (e *Executor) Act(opID, targetID, action, source string) ActResult {
	if opID == "" {
		opID = NewUUID4().String()
	}
	res := ActResult{OpID: opID, Status: StatusRejected}
	u, err := ParseUUID(opID)
	if err != nil || u.String() != opID || u[8]>>6 != 2 {
		res.Error = "invalid_op_id"
		return res
	}
	switch source {
	case "button", "voice", "entity", "llm", "dashboard":
	default:
		res.Error = "invalid_source"
		return res
	}
	if action != actionDismiss && action != actionSnooze {
		res.Error = "invalid_action"
		return res
	}
	e.mu.Lock()
	defer e.signal()
	defer e.mu.Unlock()

	if op := e.st.st.ops[opID]; op != nil {
		res.Status = StatusDurable // idempotent retry of a journaled operation
		if op.Result == nil {
			w := wireOp(op)
			res.Operation = &w
		}
		return res
	}
	for _, v := range e.volatileOps {
		if v.op.OpID == opID {
			res.Status, res.Error = StatusApplied, "persistence_failed"
			if v.op.Result == nil {
				w := wireOp(v.op)
				res.Operation = &w
			}
			return res
		}
	}
	isActive := e.active != nil && e.active.id == targetID
	if t := e.timers[targetID]; t != nil {
		if action == actionSnooze {
			res.Error = "not_snoozable"
			return res
		}
		delete(e.timers, targetID)
		if isActive {
			e.endActiveLocked()
			res.Ended = &RingEnded{ID: targetID, Kind: kindTimer, Reason: ringEndReason(source)}
		}
		res.Status = StatusApplied
		e.reconcileWakeLockLocked()
		return res
	}
	o := e.findLocked(targetID)
	switch {
	case o == nil:
		res.Error = "unknown_target"
		return res
	case !e.executableLocked(o):
		res.Error = "already_handled"
		return res
	case action == actionSnooze && !isActive:
		res.Error = "not_ringing"
		return res
	}
	var child *occurrence
	if action == actionSnooze {
		press, ok := e.clock.utcNowMs()
		if !ok {
			res.Error = "clock_untrusted"
			return res
		}
		var err error
		if child, err = snoozeChild(o, press); err != nil {
			res.Error = "invalid_due_local"
			return res
		}
	}
	if isActive {
		e.endActiveLocked()
		res.Ended = &RingEnded{ID: targetID, Kind: kindAlarm, Reason: ringEndReason(source)}
	}
	op := e.newOpLocked(opID, o, action, source)
	if child != nil {
		op.Child = &ChildRef{ScheduleID: child.ScheduleID, OccurrenceID: child.OccurrenceID,
			DueUTCMs: child.DueUTCMs, DueLocal: child.DueLocal}
	}
	if err := e.persistOpLocked(op, o, child); err != nil {
		res.Status, res.Error = StatusApplied, "persistence_failed"
	} else {
		res.Status = StatusDurable
	}
	e.reconcileWakeLockLocked()
	w := wireOp(op)
	res.Operation = &w
	return res
}

func ringEndReason(source string) string {
	if source == endButton || source == endEntity {
		return source
	}
	return endStopped
}

// PendingOperations returns every operation the controller has not answered,
// in creation order, for upload at each session start before any snapshot
// is installed (SPEC §10.6 step 5).
func (e *Executor) PendingOperations() []LocalOperation {
	e.mu.Lock()
	defer e.mu.Unlock()
	var out []LocalOperation
	for _, op := range e.st.st.opsInOrder() {
		if op.Result == nil {
			out = append(out, wireOp(op))
		}
	}
	for _, v := range e.volatileOps {
		if v.op.Result == nil {
			out = append(out, wireOp(v.op))
		}
	}
	return out
}

// ApplyOpResult records the controller's WIRE alert.op_result. The operation
// stays as a protected tombstone while its target is still delivered; a
// rejected snooze removes its local child.
func (e *Executor) ApplyOpResult(r OpResult) error {
	if r.State != StatusApplied && r.State != StatusRejected {
		return fmt.Errorf("alerts: op_result state %q", r.State)
	}
	e.mu.Lock()
	defer e.signal()
	defer e.mu.Unlock()
	res := r
	for _, v := range e.volatileOps {
		if v.op.OpID == r.OpID {
			v.op.Result = &res
			if r.State == StatusRejected {
				v.child = nil
			}
			e.reconcileWakeLockLocked()
			return nil
		}
	}
	st := e.st.st
	op := st.ops[r.OpID]
	if op == nil {
		return nil // already settled
	}
	upd := *op
	upd.Result = &res
	t := &txn{PutOps: []*localOp{&upd}}
	if op.Child != nil {
		if c := st.children[op.Child.OccurrenceID]; c != nil {
			if r.State == StatusRejected {
				t.DelChildren = []string{c.Occurrence.OccurrenceID}
				t.DelArmed = []string{c.Occurrence.OccurrenceID}
				t.DelRings = []string{c.Occurrence.OccurrenceID}
			} else {
				cc := *c
				cc.Resolved = true
				t.PutChildren = []*localChild{&cc}
			}
		}
	}
	if !targetDelivered(op, st.occ) {
		t.PutOps, t.DelOps = nil, []string{r.OpID}
	}
	err := e.st.commit(t)
	if err == nil {
		e.reconcileWakeLockLocked()
	}
	return err
}

// ClockRequestIfDue returns a WIRE clock.request body when one should be sent:
// every 1 s until UTC is trusted, then every 10 min.
func (e *Executor) ClockRequestIfDue() (ClockRequest, bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	nonce, ok := e.clock.requestIfDue()
	return ClockRequest{Nonce: nonce}, ok
}

// ApplyClockReply consumes a WIRE clock.reply received on the authenticated
// control socket and persists a newly trusted anchor for this boot.
func (e *Executor) ApplyClockReply(r ClockReply) error {
	utc, err := strconv.ParseInt(r.UTCMs, 10, 64)
	if err != nil {
		return fmt.Errorf("alerts: clock.reply utc_ms %q", r.UTCMs)
	}
	e.mu.Lock()
	defer e.signal()
	defer e.mu.Unlock()
	changed, err := e.clock.reply(r.Nonce, utc)
	if err != nil || !changed {
		return err
	}
	t := &txn{Clock: e.clock.anchor()}
	for _, o := range e.st.st.occ {
		e.armOccurrenceLocked(o, t)
	}
	for _, c := range e.st.st.children {
		e.armOccurrenceLocked(c.Occurrence, t)
	}
	return e.st.commit(t)
}

// ClockSessionLost discards unanswered clock requests.
func (e *Executor) ClockSessionLost() {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.clock.sessionLost()
}

// ClockInfo returns the WIRE session.hello "clock" object.
func (e *Executor) ClockInfo() ClockInfo {
	e.mu.Lock()
	defer e.mu.Unlock()
	info := ClockInfo{Trusted: e.clock.trusted, MonoNs: strconv.FormatInt(e.mono.MonoNowNs(), 10)}
	if utc, ok := e.clock.utcNowMs(); ok {
		s := strconv.FormatInt(utc, 10)
		info.UTCMs = &s
	}
	return info
}

// UTCNowMs returns trusted UTC milliseconds.
func (e *Executor) UTCNowMs() (int64, bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.clock.utcNowMs()
}

// Hello returns the WIRE session.hello "alerts" object.
func (e *Executor) Hello() HelloAlerts {
	e.mu.Lock()
	defer e.mu.Unlock()
	h := HelloAlerts{AckedSequence: e.st.st.acked, Wakeup: "ok"}
	if e.st.st.epoch != nil && !e.st.st.corrupt {
		epoch := *e.st.st.epoch
		h.DeliveryEpoch = &epoch
	}
	if e.wake.unavailable {
		h.Wakeup = "alarm_wakeup_unavailable"
	}
	return h
}

// CacheCapable reports whether alert_cache_v1 may be announced: withheld while
// the wakelock is unavailable (SPEC §16.5).
func (e *Executor) CacheCapable() bool {
	e.mu.Lock()
	defer e.mu.Unlock()
	return !e.wake.unavailable
}

// MissingSounds lists the asset hashes armed alarms need that are not
// installed under <root>/assets, for background fetching.
func (e *Executor) MissingSounds() []string {
	e.mu.Lock()
	defer e.mu.Unlock()
	seen := map[string]bool{}
	var out []string
	for _, o := range e.armedLocked() {
		if o.Sound == SoundFallback || seen[o.Sound] || e.sounds[o.Sound] != nil {
			continue
		}
		seen[o.Sound] = true
		if _, err := os.Stat(e.assetPath(o.Sound)); err != nil {
			out = append(out, o.Sound)
		}
	}
	sort.Strings(out)
	return out
}

// AssetDir is where the asset fetcher installs alert sounds as <sha256>.wav.
func (e *Executor) AssetDir() string {
	return filepath.Join(e.root, "assets")
}

// Close silences the source, releases the wakelock (clean shutdown), and
// closes the store.
func (e *Executor) Close() error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.closed {
		return nil
	}
	e.closed = true
	e.endActiveLocked()
	_ = e.wake.release()
	return e.st.close()
}
