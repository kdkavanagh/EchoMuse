package render

import (
	"fmt"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

// Playback slots: at most one playback per class (SPEC §16.2).
const (
	slotContent = iota
	slotDialog
	slotEarcon
	slotPreview
	numSlots
)

const (
	maxOutstandingWrites = 8 // Sink contract
	maxBlocksPerWrite    = SinkFrames/BlockFrames + 2
	eventCapacity        = 128 // pending Progress/Finished deliveries
	recentEpochs         = 8   // finished network epochs whose late packets are stale
)

func slotOf(c SourceClass) (int, bool) {
	switch c {
	case Content:
		return slotContent, true
	case DialogOutput:
		return slotDialog, true
	case Earcon:
		return slotEarcon, true
	case AlertPreview:
		return slotPreview, true
	}
	return 0, false
}

func maskOf(c SourceClass) uint8 {
	switch c {
	case Content:
		return MaskContent
	case DialogOutput:
		return MaskDialog
	case Earcon:
		return MaskEarcon
	}
	return MaskAlert // alert_preview
}

// playback is one render.start. Fields are guarded by Mixer.mu.
type playback struct {
	id     string
	gen    uint32
	class  SourceClass
	slot   int
	mask   uint8
	epoch  uint64
	baseDB float64

	fifo       *sampleFIFO // network classes
	pcm        []int16     // local classes, read-only
	pos        int
	lastSample int16 // last mixed source sample, used for a synthetic cancel fade

	next     uint64 // expected first frame of the next packet
	ended    bool   // no more audio follows (render.end and all audio through its end frame; local: always)
	endFrame uint64 // render.end's end frame while audio before it is still in flight; 0: none
	started  bool
	paused   bool
	starving bool
	// missingFrom is the render-epoch frame where the current starvation began.
	missingFrom uint64
	appliedDB   float64
	gain        gainRamp

	consumed  uint64 // source frames mixed
	submitted uint64 // source frames handed to the sink
	completed uint64 // source frames whose buffer completed
	drainAt   int64  // mono ns at which drained is declared; 0 = not yet
	nextTick  int64
}

func (p *playback) available() int {
	if p.fifo != nil {
		return p.fifo.n
	}
	return len(p.pcm) - p.pos
}

func (p *playback) take() int16 {
	p.consumed++
	if p.fifo != nil {
		return p.fifo.pop()
	}
	v := p.pcm[p.pos]
	p.pos++
	return v
}

func (p *playback) drainable() bool {
	return p.started && p.ended && p.available() == 0 && p.completed == p.consumed
}

// fade plays the head of a cancelled playback down to silence so a cancel
// never clicks. It is mixer-goroutine state.
type fade struct {
	buf  [NormalRampFrames]int16
	n    int
	pos  int
	mask uint8
	gain gainRamp
}

func (f *fade) mix(acc *[BlockFrames]int32) bool {
	audible := false
	for i := 0; i < BlockFrames && f.pos < f.n; i++ {
		g := f.gain.next()
		acc[i] += int32(float64(f.buf[f.pos]) * g)
		f.pos++
		if g > 0 {
			audible = true
		}
	}
	return audible
}

// blockRecord remembers which source frames a staged block holds: each slot's
// frames occupy the block prefix [first, first+n).
type blockRecord struct {
	first uint64
	p     [numSlots]*playback
	n     [numSlots]uint16
}

// writeRecord is one outstanding sink write: each playback's submitted
// frontier once that buffer completes.
type writeRecord struct {
	p   [numSlots * maxBlocksPerWrite]*playback
	end [numSlots * maxBlocksPerWrite]uint64
}

type event struct {
	finished bool
	progress Progress
	fin      Finished
}

// Config wires a Mixer. Now must be the Sink's completion clock
// (CLOCK_MONOTONIC ns).
type Config struct {
	Sink  Sink
	Alert PullSource // the alert executor; may be nil
	Now   func() int64
	Hooks Hooks
}

// Mixer is the final mixer. Run owns the mixing goroutine; every other method
// is safe to call from any goroutine.
type Mixer struct {
	sink  Sink
	alert PullSource
	now   func() int64
	hooks Hooks

	mu        sync.Mutex
	epoch     uint64
	mixed     uint64 // render frames mixed in this epoch
	submitted uint64 // render frames handed to the sink
	completed uint64 // render frames completed
	lastDone  int64  // mono ns of the latest completion; 0 = none this epoch
	policy    Policy
	play      [numSlots]*playback
	highGen   [numSlots]uint32
	fifos     [2]sampleFIFO // content, dialog_output
	recent    [recentEpochs]uint64
	recentN   int
	events    [eventCapacity]event
	evHead    int
	evN       int
	writes    [2 * maxOutstandingWrites]writeRecord
	wrHead    int
	wrN       int

	// Mixer-goroutine state.
	acc      [BlockFrames]int32
	block    [BlockFrames]int16
	alertBuf [BlockFrames]int16
	staging  [SinkFrames + BlockFrames]int16
	stageN   int
	blocks   [maxBlocksPerWrite + 2]blockRecord
	blkHead  int
	blkN     int
	fades    [numSlots]fade

	emitMu      sync.Mutex
	completions chan int64
	stop        chan struct{}
	closeOnce   sync.Once
}

// NewMixer allocates every buffer the mixer will use and registers the sink
// completion callback.
func NewMixer(cfg Config) (*Mixer, error) {
	if cfg.Sink == nil || cfg.Now == nil {
		return nil, ErrConfig
	}
	m := &Mixer{
		sink:        cfg.Sink,
		alert:       cfg.Alert,
		now:         cfg.Now,
		hooks:       cfg.Hooks,
		epoch:       ema.NewEpoch(),
		completions: make(chan int64, 2*maxOutstandingWrites),
		stop:        make(chan struct{}),
	}
	m.fifos[slotContent] = newSampleFIFO(fifoFrames)
	m.fifos[slotDialog] = newSampleFIFO(fifoFrames)
	cfg.Sink.SetOnComplete(func(ns int64) {
		select {
		case m.completions <- ns:
		default: // only when Run has exited; the epoch is abandoned
		}
	})
	return m, nil
}

// Epoch is the current render epoch.
func (m *Mixer) Epoch() uint64 {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.epoch
}

// Run mixes and writes until Close. A failed write fails every playback,
// restarts the sink and starts a new render epoch; Run returns only on Close
// or when the sink cannot restart.
func (m *Mixer) Run() error {
	if m.hooks.Epoch != nil {
		m.hooks.Epoch(m.Epoch())
	}
	for {
		select {
		case <-m.stop:
			return nil
		default:
		}
		if err := m.step(); err != nil {
			select {
			case <-m.stop:
				return nil
			default:
			}
			if err := m.restart(); err != nil {
				return fmt.Errorf("render: sink restart: %w", err)
			}
		}
	}
}

// Close stops Run and closes the sink.
func (m *Mixer) Close() error {
	var err error
	m.closeOnce.Do(func() {
		close(m.stop)
		err = m.sink.Close()
	})
	return err
}

// Start begins a playback. A newer generation replaces the class's current
// playback, which finishes cancelled with a 30 ms fade; an older generation
// is rejected with ErrStale; repeating the current start is a no-op.
func (m *Mixer) Start(v Playback) error {
	s, ok := slotOf(v.Class)
	if !ok {
		return ErrInvalidClass
	}
	if v.Class.Network() && v.Epoch == 0 {
		return ErrNoEpoch
	}
	if !v.Class.Network() && len(v.PCM) == 0 {
		return ErrNoAudio
	}
	m.mu.Lock()
	if v.Generation < m.highGen[s] {
		m.mu.Unlock()
		return ErrStale
	}
	if cur := m.play[s]; cur != nil {
		if cur.id == v.ID && cur.gen == v.Generation {
			m.mu.Unlock()
			return nil
		}
		m.endLocked(cur, Cancelled, RampNormal)
	}
	p := &playback{
		id:     v.ID,
		gen:    v.Generation,
		class:  v.Class,
		slot:   s,
		mask:   maskOf(v.Class),
		epoch:  v.Epoch,
		baseDB: v.GainDB,
	}
	if v.Class.Network() {
		p.fifo = &m.fifos[s]
		p.fifo.reset()
	} else {
		p.pcm = v.PCM
		p.ended = true
	}
	db, pause := m.policyFor(p)
	p.appliedDB, p.paused = db, pause
	if pause {
		p.gain.jump(0)
	} else {
		p.gain.jump(dbToGain(db))
	}
	m.highGen[s] = v.Generation
	m.play[s] = p
	m.mu.Unlock()
	m.flush()
	return nil
}

// Pump appends one kind-3 packet to the network playback bound to epoch.
// Packets must continue the source exactly. Overflow fails the playback.
func (m *Mixer) Pump(epoch uint64, generation uint32, first uint64, pcm []int16) error {
	m.mu.Lock()
	err := m.pumpLocked(epoch, generation, first, pcm)
	m.mu.Unlock()
	m.flush()
	return err
}

func (m *Mixer) pumpLocked(epoch uint64, generation uint32, first uint64, pcm []int16) error {
	var p *playback
	for _, c := range m.play[:slotEarcon] {
		if c != nil && c.epoch == epoch {
			p = c
		}
	}
	if p == nil {
		for _, e := range m.recent {
			if e == epoch {
				return ErrStale
			}
		}
		return ErrUnknownEpoch
	}
	switch {
	case generation != p.gen:
		return ErrStale
	case p.ended:
		return ErrEnded
	case first != p.next:
		return ErrDiscontinuous
	}
	if !p.fifo.push(pcm) {
		m.endLocked(p, Failed, RampNormal)
		return ErrFIFOFull
	}
	p.next += uint64(len(pcm))
	if p.endFrame != 0 && p.next >= p.endFrame {
		p.ended, p.endFrame = true, 0
	}
	return nil
}

// End marks that no audio follows endFrame, the source frame after the last
// one sent (render.end). Audio travels on its own socket, so the tail before
// endFrame may still arrive after render.end: the playback keeps accepting it
// and drains only once it has. An endFrame of 0 (not sent) ends at the audio
// already received.
func (m *Mixer) End(id string, generation uint32, endFrame uint64) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	p := m.findLocked(id, generation)
	if p == nil {
		return ErrStale
	}
	if p.fifo != nil && p.next < endFrame {
		p.endFrame = endFrame
		return nil
	}
	p.ended = true
	return nil
}

// Cancel discards the playback's queued audio, fades what is already playing
// over ramp, and reports cancelled at the last completed frontier. Unknown or
// already-finished playbacks are ignored, so cancellation is idempotent.
func (m *Mixer) Cancel(id string, generation uint32, ramp Ramp) {
	m.mu.Lock()
	if p := m.findLocked(id, generation); p != nil {
		if p.started {
			m.progressLocked(p, EventFlush, m.now(), 0, 0)
		}
		m.endLocked(p, Cancelled, ramp)
	}
	m.mu.Unlock()
	m.flush()
}

// EndSession cancels network playbacks, whose audio socket is gone, and
// clears generation fences for the next controller session. Local sources
// continue.
func (m *Mixer) EndSession() {
	m.mu.Lock()
	for _, p := range m.play[:slotEarcon] {
		if p != nil {
			m.endLocked(p, Cancelled, RampNormal)
		}
	}
	m.highGen = [numSlots]uint32{}
	m.mu.Unlock()
	m.flush()
}

// SetPolicy applies a focus decision with per-sample ramps.
func (m *Mixer) SetPolicy(pol Policy) {
	m.mu.Lock()
	m.policy = pol
	now := m.now()
	for _, p := range m.play {
		if p != nil {
			m.applyPolicyLocked(p, pol.Ramp, now)
		}
	}
	m.mu.Unlock()
	m.flush()
}

func (m *Mixer) policyFor(p *playback) (db float64, pause bool) {
	db = p.baseDB
	switch p.class {
	case Content, AlertPreview:
		return db + m.policy.ContentDuckDB, m.policy.PauseContent
	case DialogOutput:
		return db + m.policy.DialogDuckDB, false
	}
	return db, false
}

func (m *Mixer) applyPolicyLocked(p *playback, ramp Ramp, now int64) {
	db, pause := m.policyFor(p)
	if pause != p.paused {
		p.paused = pause
		if p.started {
			ev := EventResume
			if pause {
				ev = EventPause
			}
			m.progressLocked(p, ev, now, 0, 0)
		}
	}
	if db != p.appliedDB {
		p.appliedDB = db
		if p.started && !pause {
			m.progressLocked(p, EventGain, now, 0, 0)
		}
	}
	target := dbToGain(db)
	if pause {
		target = 0
	}
	p.gain.set(target, ramp.Frames())
}

func (m *Mixer) findLocked(id string, generation uint32) *playback {
	for _, p := range m.play {
		if p != nil && p.id == id && p.gen == generation {
			return p
		}
	}
	return nil
}

// endLocked removes p, fading its audible head over ramp, and reports reason.
func (m *Mixer) endLocked(p *playback, reason FinishReason, ramp Ramp) {
	if m.play[p.slot] != p {
		return
	}
	if p.started && !p.gain.silent() {
		f := &m.fades[p.slot]
		frames := ramp.Frames()
		f.n = frames
		for i := range frames {
			f.buf[i] = p.lastSample
		}
		f.pos = 0
		f.mask = p.mask
		f.gain.jump(p.gain.current)
		f.gain.set(0, frames)
	}
	m.finishLocked(p, reason)
}

func (m *Mixer) finishLocked(p *playback, reason FinishReason) {
	m.play[p.slot] = nil
	if p.fifo != nil {
		p.fifo.reset()
		m.recent[m.recentN%recentEpochs] = p.epoch
		m.recentN++
	}
	m.enqueue(event{finished: true, fin: Finished{
		PlaybackID:         p.id,
		Generation:         p.gen,
		LastCompletedFrame: p.completed,
		Reason:             reason,
		TimingQuality:      TimingEstimated,
	}})
}

// progressLocked queues a render.progress. missingFrom/missingTo are used by
// EventUnderrun only.
func (m *Mixer) progressLocked(p *playback, ev ProgressEvent, now int64, missingFrom, missingTo uint64) {
	mono := m.lastDone
	if mono == 0 {
		mono = now
	}
	// Uncertainty: queued render time plus one write of completion granularity.
	outstanding := m.submitted - m.completed + SinkFrames
	m.enqueue(event{progress: Progress{
		PlaybackID:        p.id,
		Generation:        p.gen,
		Event:             ev,
		SubmittedFrames:   p.submitted,
		CompletedFrames:   p.completed,
		MonoNS:            mono,
		UncertaintyUS:     uint32(outstanding * 1_000_000 / SampleRate),
		TimingQuality:     TimingEstimated,
		ReferenceCoverage: CoverageFull,
		MissingFrom:       missingFrom,
		MissingTo:         missingTo,
		GainDB:            p.appliedDB,
	}})
}

// enqueue drops the event only if hooks have stalled for eventCapacity events.
func (m *Mixer) enqueue(ev event) {
	if m.evN == len(m.events) {
		return
	}
	m.events[(m.evHead+m.evN)%len(m.events)] = ev
	m.evN++
}

// flush delivers queued events in order, outside m.mu. A hook that re-enters
// the mixer queues its events for the outer delivery loop.
func (m *Mixer) flush() {
	for {
		if !m.emitMu.TryLock() {
			return
		}
		for {
			m.mu.Lock()
			if m.evN == 0 {
				m.mu.Unlock()
				break
			}
			ev := m.events[m.evHead]
			m.evHead = (m.evHead + 1) % len(m.events)
			m.evN--
			m.mu.Unlock()
			if ev.finished {
				if m.hooks.Finished != nil {
					m.hooks.Finished(ev.fin)
				}
			} else if m.hooks.Progress != nil {
				m.hooks.Progress(ev.progress)
			}
		}
		m.emitMu.Unlock()
		m.mu.Lock()
		pending := m.evN > 0
		m.mu.Unlock()
		if !pending {
			return
		}
	}
}

// step processes completions, mixes one block and writes when a full sink
// buffer is staged. It is the mixer goroutine's only unit of work.
func (m *Mixer) step() error {
	for drained := false; !drained; {
		select {
		case ns := <-m.completions:
			m.complete(ns)
		default:
			drained = true
		}
	}
	m.mixBlock()
	var err error
	if m.stageN >= SinkFrames {
		err = m.writeStaged()
	}
	m.flush()
	return err
}

func (m *Mixer) mixBlock() {
	clear(m.acc[:])
	now := m.now()
	var mask uint8

	m.mu.Lock()
	first := m.mixed
	rec := &m.blocks[(m.blkHead+m.blkN)%len(m.blocks)]
	*rec = blockRecord{first: first}
	m.blkN++
	for s, p := range m.play {
		if p == nil {
			continue
		}
		n, audible := m.mixPlaybackLocked(p, first, now)
		if n > 0 {
			rec.p[s], rec.n[s] = p, uint16(n)
		}
		if audible {
			mask |= p.mask
		}
	}
	for i := range m.fades {
		if m.fades[i].mix(&m.acc) {
			mask |= m.fades[i].mask
		}
	}
	m.mixed += BlockFrames
	for _, p := range m.play {
		switch {
		case p == nil:
		case p.drainAt != 0 && now >= p.drainAt:
			m.finishLocked(p, Drained)
		case p.started && now >= p.nextTick:
			m.progressLocked(p, EventProgress, now, 0, 0)
			p.nextTick += ProgressIntervalNs
			if p.nextTick <= now {
				p.nextTick = now + ProgressIntervalNs
			}
		}
	}
	m.mu.Unlock()

	if m.alert != nil {
		if m.alert.Fill(m.alertBuf[:]) {
			mask |= MaskAlert
		}
		for i, v := range m.alertBuf {
			m.acc[i] += int32(v)
		}
	}
	for i, v := range m.acc {
		m.block[i] = saturate(v)
	}
	copy(m.staging[m.stageN:], m.block[:])
	m.stageN += BlockFrames
	if m.hooks.Tap != nil {
		m.hooks.Tap(MixBlock{First: first, PCM: m.block[:], Mask: mask})
	}
}

// mixPlaybackLocked adds p's next frames into the accumulator from offset 0
// and returns how many source frames it consumed.
func (m *Mixer) mixPlaybackLocked(p *playback, first uint64, now int64) (int, bool) {
	if p.paused && p.gain.silent() {
		return 0, false
	}
	if !p.started {
		if p.fifo != nil && !p.ended && p.fifo.n < primeFrames {
			return 0, false
		}
		p.started = true
		p.nextTick = now + ProgressIntervalNs
		m.progressLocked(p, EventStart, now, 0, 0)
	}
	if p.starving {
		if !p.ended && p.fifo.n < primeFrames {
			if to := first + BlockFrames; to-p.missingFrom >= StarvedLimitFrames {
				m.progressLocked(p, EventUnderrun, now, p.missingFrom, to)
				m.finishLocked(p, Underrun)
			}
			return 0, false
		}
		p.starving = false
		m.progressLocked(p, EventUnderrun, now, p.missingFrom, first)
	}
	n, audible := 0, false
	for ; n < BlockFrames; n++ {
		if p.paused && p.gain.silent() {
			break
		}
		if p.available() == 0 {
			if !p.ended && !p.paused {
				p.starving = true
				p.missingFrom = first + uint64(n)
			}
			break
		}
		g := p.gain.next()
		sample := p.take()
		p.lastSample = sample
		m.acc[n] += int32(float64(sample) * g)
		if g > 0 {
			audible = true
		}
	}
	return n, audible
}

func saturate(v int32) int16 {
	switch {
	case v > 32767:
		return 32767
	case v < -32768:
		return -32768
	}
	return int16(v)
}

// writeStaged hands one SinkFrames buffer to the sink and credits each
// playback's frames in it as submitted.
func (m *Mixer) writeStaged() error {
	if err := m.sink.Write(m.staging[:SinkFrames]); err != nil {
		return err
	}
	m.mu.Lock()
	start := m.submitted
	end := start + SinkFrames
	var rec writeRecord
	entries := 0
	for i := range m.blkN {
		b := &m.blocks[(m.blkHead+i)%len(m.blocks)]
		for s, p := range b.p {
			if p == nil {
				continue
			}
			lo, hi := max(b.first, start), min(b.first+uint64(b.n[s]), end)
			if hi <= lo {
				continue
			}
			k := 0
			for k < entries && rec.p[k] != p {
				k++
			}
			if k == entries {
				rec.p[k] = p
				entries++
			}
			p.submitted += hi - lo
			rec.end[k] = p.submitted
		}
	}
	for m.blkN > 0 && m.blocks[m.blkHead].first+BlockFrames <= end {
		m.blkHead = (m.blkHead + 1) % len(m.blocks)
		m.blkN--
	}
	m.writes[(m.wrHead+m.wrN)%len(m.writes)] = rec
	m.wrN++
	m.submitted = end
	m.mu.Unlock()
	m.stageN = copy(m.staging[:], m.staging[SinkFrames:m.stageN])
	return nil
}

// complete applies one sink buffer completion at mono time ns.
func (m *Mixer) complete(ns int64) {
	m.mu.Lock()
	if m.wrN == 0 {
		m.mu.Unlock()
		return
	}
	rec := &m.writes[m.wrHead]
	m.wrHead = (m.wrHead + 1) % len(m.writes)
	m.wrN--
	for k, p := range rec.p {
		if p != nil && rec.end[k] > p.completed {
			p.completed = rec.end[k]
		}
	}
	m.completed += SinkFrames
	m.lastDone = ns
	for _, p := range m.play {
		if p != nil && p.drainAt == 0 && p.drainable() {
			p.drainAt = ns + DrainGuardNs
		}
	}
	anchor := Anchor{Epoch: m.epoch, CompletedFrame: m.completed, MonoNS: ns}
	m.mu.Unlock()
	if m.hooks.Anchor != nil {
		m.hooks.Anchor(anchor)
	}
}

// restart fails every playback, restarts the sink and begins a new epoch.
func (m *Mixer) restart() error {
	m.mu.Lock()
	for _, p := range m.play {
		if p != nil {
			m.finishLocked(p, Failed)
		}
	}
	m.mu.Unlock()
	m.flush()
	if err := m.sink.Restart(); err != nil {
		return err
	}
	for drained := false; !drained; {
		select {
		case <-m.completions:
		default:
			drained = true
		}
	}
	m.mu.Lock()
	m.epoch = ema.NewEpoch()
	m.mixed, m.submitted, m.completed, m.lastDone = 0, 0, 0, 0
	m.wrHead, m.wrN = 0, 0
	epoch := m.epoch
	m.mu.Unlock()
	m.stageN, m.blkHead, m.blkN = 0, 0, 0
	m.fades = [numSlots]fade{}
	if m.hooks.Epoch != nil {
		m.hooks.Epoch(epoch)
	}
	return nil
}
