package detector

import (
	"errors"
	"fmt"
	"math"
	"runtime"
	"sync"
	"time"

	"github.com/wilbowes/EchoMuse/internal/monoclock"
	"github.com/wilbowes/EchoMuse/internal/uuid"
)

type itemKind uint8

const (
	itemHop     itemKind = iota // a complete valid window to score
	itemGap                     // a missing range: the windows spanning it are invalid
	itemOverrun                 // pending hops were dropped (count in dropped)
)

// windowSlot is one pooled 22,400-sample window copy. There are QueueDepth+1
// slots: QueueDepth pending and one in inference.
type windowSlot struct {
	pcm [WindowSamples]int16
}

type queueItem struct {
	kind    itemKind
	end     uint64 // capture-epoch index one past the window's last sample
	epoch   uint64
	profile Profile
	slot    *windowSlot
	dropped uint64
}

type controlKind uint8

const (
	controlReset controlKind = iota
	controlMute
	controlModel
	controlUnavailable
	controlThresholds
	controlFlush
)

type control struct {
	kind       controlKind
	model      Model
	thresholds Thresholds
	reason     UnavailableReason
	detail     string
	ack        chan struct{}
}

// Detector ingests capture blocks, owns the window history and the bounded
// pending-hop queue (SPEC §5.2), and runs every inference and policy
// transition on one goroutine locked to its OS thread (SPEC §3.1 `wake`).
type Detector struct {
	cb Callbacks

	// Capture-side state, guarded by ingestMu.
	ingestMu  sync.Mutex
	epoch     uint64
	haveEpoch bool
	expected  uint64 // next sample index the history expects
	validFrom uint64 // first sample of the current contiguous valid history
	nextHop   uint64 // end sample of the next hop-grid window
	muted     bool
	enabled   bool // a model is loaded
	ring      [WindowSamples]int16

	// Queue shared with the wake goroutine, guarded by queueMu.
	queueMu  sync.Mutex
	queue    [QueueDepth]queueItem
	queueLen int
	slots    [QueueDepth + 1]windowSlot
	slotUsed [QueueDepth + 1]bool
	overruns uint64 // dropped hops not yet reported to the wake goroutine
	controls []control
	wake     chan struct{}

	quit      chan struct{}
	done      chan struct{}
	closeOnce sync.Once
}

// New starts the wake goroutine. Profile, ProducingSound, OnCandidate,
// OnCandidateEnd and OnStats are required; NowMonoNS defaults to
// CLOCK_MONOTONIC and NewID to random UUIDv4s. Nothing is scored until
// SetModel and SetEpoch.
func New(cb Callbacks) (*Detector, error) {
	switch {
	case cb.Profile == nil:
		return nil, errors.New("detector: Profile callback is required")
	case cb.ProducingSound == nil:
		return nil, errors.New("detector: ProducingSound callback is required")
	case cb.OnCandidate == nil, cb.OnCandidateEnd == nil:
		return nil, errors.New("detector: candidate callbacks are required")
	case cb.OnStats == nil:
		return nil, errors.New("detector: OnStats callback is required")
	}
	if cb.NowMonoNS == nil {
		cb.NowMonoNS = monoclock.Now
	}
	if cb.NewID == nil {
		cb.NewID = func() string { return uuid.NewV4().String() }
	}
	d := &Detector{
		cb:       cb,
		controls: make([]control, 0, 8),
		wake:     make(chan struct{}, 1),
		quit:     make(chan struct{}),
		done:     make(chan struct{}),
	}
	go d.run()
	return d, nil
}

// SetModel switches to a fully loaded graph+sidecar scorer between hops and
// starts a new scorer revision (SPEC §5.1, §16.5). The previous scorer keeps
// scoring until this call and is closed by the wake goroutine after the
// switch. History resets. It returns once the switch has been applied.
func (d *Detector) SetModel(m Model) error {
	if err := m.validate(); err != nil {
		return err
	}
	d.ingestMu.Lock()
	d.enabled = true
	d.resetIngestLocked()
	d.ingestMu.Unlock()
	ack := make(chan struct{})
	d.enqueueControl(control{kind: controlModel, model: m, ack: ack})
	select {
	case <-ack:
	case <-d.done:
	}
	return nil
}

// SetThresholds replaces the profile thresholds between hops without a reset
// or a new scorer revision. An open candidate keeps its latched threshold.
func (d *Detector) SetThresholds(th Thresholds) error {
	if err := th.validate(); err != nil {
		return err
	}
	d.enqueueControl(control{kind: controlThresholds, thresholds: th})
	return nil
}

// SetUnavailable stops scoring, closes the current scorer between hops, and
// reports wake_unavailable with reason missing_asset or load_failed and a
// human-readable detail. A later SetModel clears it.
func (d *Detector) SetUnavailable(reason UnavailableReason, detail string) error {
	if reason != UnavailableMissingAsset && reason != UnavailableLoadFailed {
		return fmt.Errorf("detector: %q is not an asset/load unavailability reason", reason)
	}
	d.ingestMu.Lock()
	d.enabled = false
	d.resetIngestLocked()
	d.ingestMu.Unlock()
	d.enqueueControl(control{kind: controlUnavailable, reason: reason, detail: detail})
	return nil
}

// SetEpoch starts a capture epoch at sample 0; its first window ends at
// FirstWindowEnd. A changed epoch resets the detector; the current epoch is a
// no-op. Blocks before the first SetEpoch are ignored.
func (d *Detector) SetEpoch(epoch uint64) {
	d.ingestMu.Lock()
	if d.haveEpoch && d.epoch == epoch {
		d.ingestMu.Unlock()
		return
	}
	d.epoch, d.haveEpoch = epoch, true
	d.expected, d.validFrom, d.nextHop = 0, 0, FirstWindowEnd
	d.clearQueue()
	d.ingestMu.Unlock()
	d.enqueueControl(control{kind: controlReset})
}

// Reset records a capture discontinuity inside the current epoch. The hop grid
// stays on capture-epoch multiples of HopSamples; the next score needs a full
// valid window after the current sample.
func (d *Detector) Reset() {
	d.ingestMu.Lock()
	d.resetIngestLocked()
	d.ingestMu.Unlock()
	d.enqueueControl(control{kind: controlReset})
}

// SetMuted applies privacy mute (SPEC §3.2 invariant 10). Muting closes an
// open candidate with reason mute and suppresses inference; samples arriving
// while muted are not history.
func (d *Detector) SetMuted(muted bool) {
	d.ingestMu.Lock()
	if d.muted == muted {
		d.ingestMu.Unlock()
		return
	}
	d.muted = muted
	d.resetIngestLocked()
	d.ingestMu.Unlock()
	if muted {
		d.enqueueControl(control{kind: controlMute})
	} else {
		d.enqueueControl(control{kind: controlReset})
	}
}

// OnBlock ingests one capture block. first is the capture-epoch index of
// pcm[0]; pcm is borrowed for the call only. gap=true marks
// [first, first+len(pcm)) missing and pcm's values are ignored; a forward jump
// from the previous block's end is missing too, and a backward one is a
// discontinuity. Runs on the capture goroutine: it copies at most one window
// per hop into a pooled slot, never allocates, and never waits for inference.
func (d *Detector) OnBlock(first uint64, pcm []int16, gap bool) {
	if len(pcm) == 0 {
		return
	}
	d.ingestMu.Lock()
	defer d.ingestMu.Unlock()
	if !d.haveEpoch {
		return
	}
	if first < d.expected {
		d.expected = first
		d.resetIngestLocked()
		d.enqueueControl(control{kind: controlReset})
	}
	if first > d.expected {
		d.missingLocked(first)
	}
	end := first + uint64(len(pcm))
	if gap || d.muted {
		d.missingLocked(end)
		return
	}
	pos := first
	for _, v := range pcm {
		d.ring[pos%WindowSamples] = v
		pos++
		if pos == d.nextHop {
			d.hopLocked(pos)
			d.nextHop += HopSamples
		}
	}
	d.expected = end
}

// missingLocked marks [expected, to) missing. Every window overlapping it is
// invalid; one gap item invalidates the history at this point in hop order.
func (d *Detector) missingLocked(to uint64) {
	for d.nextHop <= to {
		d.nextHop += HopSamples
	}
	d.expected, d.validFrom = to, to
	if d.enabled && !d.muted {
		d.enqueueGap()
	}
}

// hopLocked queues the window ending at end if it lies wholly inside valid
// history. A window reaching before validFrom is invalid; the reset or gap
// that set validFrom has already invalidated the scorer's history.
func (d *Detector) hopLocked(end uint64) {
	if !d.enabled || end-WindowSamples < d.validFrom {
		return
	}
	profile := d.cb.Profile()
	d.queueMu.Lock()
	if d.queueLen == QueueDepth {
		d.dropOldestHopLocked()
	}
	slot := d.allocSlotLocked()
	start := end - WindowSamples
	i := int(start % WindowSamples)
	n := copy(slot.pcm[:], d.ring[i:])
	copy(slot.pcm[n:], d.ring[:i])
	d.queue[d.queueLen] = queueItem{kind: itemHop, end: end, epoch: d.epoch, profile: profile, slot: slot}
	d.queueLen++
	d.queueMu.Unlock()
	d.signal()
}

func (d *Detector) resetIngestLocked() {
	d.validFrom = d.expected
	if d.nextHop < FirstWindowEnd {
		d.nextHop = FirstWindowEnd
	}
	for d.nextHop <= d.expected {
		d.nextHop += HopSamples
	}
	d.clearQueue()
}

func (d *Detector) enqueueGap() {
	d.queueMu.Lock()
	if d.queueLen > 0 && d.queue[d.queueLen-1].kind == itemGap {
		d.queueMu.Unlock()
		return
	}
	if d.queueLen == QueueDepth {
		d.dropOldestHopLocked()
	}
	d.queue[d.queueLen] = queueItem{kind: itemGap, epoch: d.epoch}
	d.queueLen++
	d.queueMu.Unlock()
	d.signal()
}

// dropOldestHopLocked drops the oldest pending hop (SPEC §5.2): the wake
// goroutine will treat it as an invalid window and count a wake_overrun.
// Consecutive gaps are coalesced, so a full queue always holds a hop.
func (d *Detector) dropOldestHopLocked() {
	for i := 0; i < d.queueLen; i++ {
		if d.queue[i].kind != itemHop {
			continue
		}
		d.freeSlotLocked(d.queue[i].slot)
		copy(d.queue[i:], d.queue[i+1:d.queueLen])
		d.queueLen--
		d.queue[d.queueLen] = queueItem{}
		d.overruns++
		return
	}
}

func (d *Detector) allocSlotLocked() *windowSlot {
	for i := range d.slots {
		if !d.slotUsed[i] {
			d.slotUsed[i] = true
			return &d.slots[i]
		}
	}
	panic("detector: window slot pool exhausted")
}

func (d *Detector) freeSlotLocked(slot *windowSlot) {
	if slot == nil {
		return
	}
	for i := range d.slots {
		if slot == &d.slots[i] {
			d.slotUsed[i] = false
			return
		}
	}
}

// clearQueue discards pending hops; a reset control follows. Unreported
// overruns stay counted.
func (d *Detector) clearQueue() {
	d.queueMu.Lock()
	for i := 0; i < d.queueLen; i++ {
		d.freeSlotLocked(d.queue[i].slot)
		d.queue[i] = queueItem{}
	}
	d.queueLen = 0
	d.queueMu.Unlock()
}

func (d *Detector) enqueueControl(c control) {
	d.queueMu.Lock()
	d.controls = append(d.controls, c)
	d.queueMu.Unlock()
	d.signal()
}

func (d *Detector) signal() {
	select {
	case d.wake <- struct{}{}:
	default:
	}
}

// nextControl pops the oldest pending control. Controls precede queued hops:
// every control that invalidates history has already cleared the queue.
func (d *Detector) nextControl() (control, bool) {
	d.queueMu.Lock()
	defer d.queueMu.Unlock()
	if len(d.controls) == 0 {
		return control{}, false
	}
	c := d.controls[0]
	copy(d.controls, d.controls[1:])
	d.controls[len(d.controls)-1] = control{}
	d.controls = d.controls[:len(d.controls)-1]
	return c, true
}

// nextItem pops dropped-hop overruns first, then queued items in hop order.
func (d *Detector) nextItem() (queueItem, bool) {
	d.queueMu.Lock()
	defer d.queueMu.Unlock()
	if d.overruns != 0 {
		it := queueItem{kind: itemOverrun, dropped: d.overruns}
		d.overruns = 0
		return it, true
	}
	if d.queueLen == 0 {
		return queueItem{}, false
	}
	it := d.queue[0]
	copy(d.queue[:], d.queue[1:d.queueLen])
	d.queueLen--
	d.queue[d.queueLen] = queueItem{}
	return it, true
}

func (d *Detector) release(it queueItem) {
	if it.slot == nil {
		return
	}
	d.queueMu.Lock()
	d.freeSlotLocked(it.slot)
	d.queueMu.Unlock()
}

func (d *Detector) run() {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	defer close(d.done)
	c := newCore(d)
	defer c.close()
	ticker := time.NewTicker(StatsPeriod)
	defer ticker.Stop()
	for {
		select {
		case <-d.quit:
			return
		case <-ticker.C:
			c.emitStats(true)
		case <-d.wake:
			d.drain(c)
		}
	}
}

// drain processes all available work. A flush control first processes the
// items queued before it, so FlushStats observes every block ingested before
// the call.
func (d *Detector) drain(c *core) {
	for {
		select {
		case <-d.quit:
			return
		default:
		}
		if ctl, ok := d.nextControl(); ok {
			if ctl.kind == controlFlush {
				d.drainItems(c)
			}
			c.control(ctl)
			continue
		}
		it, ok := d.nextItem()
		if !ok {
			return
		}
		c.item(it)
		d.release(it)
	}
}

func (d *Detector) drainItems(c *core) {
	for {
		it, ok := d.nextItem()
		if !ok {
			return
		}
		c.item(it)
		d.release(it)
	}
}

// FlushStats scores every hop ingested before the call, applies earlier
// controls, and emits wake.stats for the window so far without starting a new
// 30 s window. It blocks until done.
func (d *Detector) FlushStats() {
	ack := make(chan struct{})
	d.enqueueControl(control{kind: controlFlush, ack: ack})
	select {
	case <-ack:
	case <-d.done:
	}
}

// Close stops the wake goroutine and closes the active scorer.
func (d *Detector) Close() {
	d.closeOnce.Do(func() { close(d.quit) })
	<-d.done
}

func finite(v float64) bool { return !math.IsNaN(v) && !math.IsInf(v, 0) }
