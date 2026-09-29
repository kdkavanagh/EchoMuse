// Package uplink executes the device's uplink leases (SPEC §4.4, WIRE §4.5,
// §5). Each lease names the streams it wants and where each starts; the
// executor maps those starts onto the mic, cell and reference rings, uploads
// backfill and then live audio as EMA1 packets, and ends the lease on close,
// TTL, mute, capture-epoch change, overrun or session loss with an
// uplink.ended report.
//
// The rings are the send queue: audio waits there, uncopied, until the single
// sender goroutine (Run) packs it into a reused frame for the audio sink.
package uplink

import (
	"context"
	"errors"
	"strconv"
	"sync"
	"time"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/audio/refdsp"
	"github.com/wilbowes/EchoMuse/internal/audio/ring"
	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/monoclock"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

// Errors returned for controller commands; the caller maps them to
// command.ack error codes.
var (
	ErrUnknownLease    = errors.New("uplink: unknown lease")
	ErrStaleGeneration = errors.New("uplink: stale generation")
	ErrInvalid         = errors.New("uplink: invalid request")
)

// Lease timing and bounds (§4.4).
const (
	DefaultTTL       = 3 * time.Second // when a command carries no ttl_ms
	CandidateAckWait = time.Second     // candidate lease without command.ack(accepted) ends with ttl
	OverrunAge       = time.Second     // live audio older than this in the queue ends its leases

	// liveFlush sends a short live packet once its first sample has
	// waited one capture block.
	liveFlush = 80 * time.Millisecond
	tick      = 20 * time.Millisecond
)

// Candidate lease stream leads in capture samples (WIRE §4.5).
const (
	candidateMicLead   = 4800   // mic from support_start − 300 ms
	candidateCellsLead = 160000 // cells from mic start − 10 s
	candidateRefLead   = 40000  // reference from mic start − 2.5 s
)

// Stream start grids: mic and cells on cell boundaries, reference on
// reference hops (§4.4).
const (
	micGrid = ema.CellSamples // capture samples
	refGrid = 2560            // reference samples
)

// Live packets carry at most 80 ms, so the sink's five-packet queue holds at
// most 400 ms of any stream's live audio (§4.4). Backfill packets use the
// full frame-count limits.
const (
	liveMaxSamples = 1280
	liveMaxCells   = liveMaxSamples / ema.CellSamples // 2
)

// Rings are the capture-epoch mic and cell rings and the render-epoch
// reference ring, written by the supervisor.
type Rings struct {
	Mic, Ref *ring.Ring[int16]
	Cells    *ring.Ring[ema.Cell]
}

// Clock maps capture samples to reference samples of the current reference
// epoch through the capture and render clock fits.
type Clock interface {
	CaptureToReference(captureSample uint64) (refSample uint64, ok bool)
}

// SendFunc sends one control message (client.Session.Send).
type SendFunc func(typ proto.MessageType, generation uint32, body any) (string, error)

type streamID int

// Streams in upload order (WIRE §5: mic, cells, reference).
const (
	micStream streamID = iota
	cellsStream
	refStream
	numStreams
)

var streamKeys = [numStreams]proto.StreamID{proto.StreamMic, proto.StreamCells, proto.StreamReference}

func streamOf(key proto.StreamID) (streamID, bool) {
	for id, k := range streamKeys {
		if k == key {
			return streamID(id), true
		}
	}
	return 0, false
}

// startReq is a requested start: a capture-epoch sample or live.
type startReq struct {
	live    bool
	capture uint64
}

type lease struct {
	id, owner string
	reason    proto.LeaseReason
	gen       uint32
	accepted  bool  // false only for a candidate awaiting its ack
	deadline  int64 // ack deadline, then TTL expiry (now() clock)

	want    [numStreams]bool
	req     [numStreams]startReq
	start   [numStreams]uint64        // resolved ring index
	clipped [numStreams]proto.NullU64 // header-domain start when clipped
	last    [numStreams]proto.NullU64 // header-domain last sample sent
}

// run is a backfill range [next, end) of ring indices owed to one lease.
type run struct {
	lease     *lease
	next, end uint64
	disc      bool
}

// mark records when the ring end reached end, to age queued live audio.
type mark struct {
	end uint64
	ns  int64
}

const maxMarks = 256

type stream struct {
	epoch  uint64
	seq    uint64 // next packet sequence in epoch
	active bool   // some accepted lease wants the stream
	next   uint64 // live cursor: next ring index to send live
	disc   bool   // the live cursor skipped missing audio
	runs   []run  // backfill, sent in order before live

	observed    uint64 // ring end at the last observation
	marks       [maxMarks]mark
	head, count int
}

type ended struct {
	gen  uint32
	body proto.UplinkEnded
}

// pending is the packet handed to the sink, committed once accepted.
type pending struct {
	id       streamID
	backfill bool
	end      uint64 // ring index after the packet
}

// Executor owns the uplink lease table and the sender. Controller commands
// and the sender may run on different goroutines.
type Executor struct {
	mu    sync.Mutex
	rings Rings
	clock Clock
	now   func() int64

	sink client.AudioSink
	send SendFunc

	micEpoch, refEpoch uint64
	leases             map[string]*lease
	streams            [numStreams]stream
	wake               chan struct{}

	frame   []byte
	pcm     []int16
	cells   []ema.Cell
	segs    []ring.Segment
	pending pending
}

// New returns an executor over the supervisor's rings; now is the
// CLOCK_MONOTONIC source in ns (monoclock.Now when nil).
func New(r Rings, c Clock, now func() int64) *Executor {
	if now == nil {
		now = monoclock.Now
	}
	e := &Executor{
		rings:  r,
		clock:  c,
		now:    now,
		leases: make(map[string]*lease),
		wake:   make(chan struct{}, 1),
		frame:  make([]byte, ema.HeaderSize+ema.MaxUplinkPCMFrames*2),
		pcm:    make([]int16, ema.MaxUplinkPCMFrames),
		cells:  make([]ema.Cell, ema.MaxCells),
		// A read of n indices yields at most n segments.
		segs: make([]ring.Segment, 0, ema.MaxUplinkPCMFrames),
	}
	for i := range e.streams {
		e.streams[i].runs = make([]run, 0, 8)
	}
	return e
}

// Attach binds a new session's audio sink and control sender.
func (e *Executor) Attach(sink client.AudioSink, send SendFunc) {
	e.mu.Lock()
	e.sink, e.send = sink, send
	e.mu.Unlock()
	e.Notify()
}

// Detach ends every lease with reason session and discards queued audio.
// Nothing is reported: the session is gone, and leases do not survive it.
func (e *Executor) Detach() {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.sink, e.send = nil, nil
	for _, l := range e.leases {
		e.endLocked(l, proto.EndedSession)
	}
}

// SetEpochs records the current capture and reference epochs. Call it after
// resetting a ring for a new epoch and before the epoch's first append. A
// capture-epoch change ends every lease (epoch); a reference-epoch change
// restarts the reference stream at the new epoch's live position. mic 0
// means no capture epoch (muted): leases cannot open.
func (e *Executor) SetEpochs(mic, ref uint64) {
	var out []ended
	e.mu.Lock()
	if mic != e.micEpoch {
		for _, l := range e.leases {
			out = append(out, e.endLocked(l, proto.EndedEpoch))
		}
		e.micEpoch = mic
		e.restartLocked(micStream, mic)
		e.restartLocked(cellsStream, mic)
	}
	if ref != e.refEpoch {
		e.refEpoch = ref
		e.restartLocked(refStream, ref)
		s := &e.streams[refStream]
		end := e.ringEnd(refStream)
		if s.active {
			s.next, s.observed = end, end
		}
		for _, l := range e.leases {
			if l.want[refStream] {
				l.start[refStream], l.last[refStream] = end, proto.NullU64{}
			}
		}
	}
	send := e.send
	e.mu.Unlock()
	emit(send, out)
	e.Notify()
}

// Mute ends every lease with reason mute and drops queued audio. Capture has
// no epoch until the next SetEpochs.
func (e *Executor) Mute() {
	var out []ended
	e.mu.Lock()
	for _, l := range e.leases {
		out = append(out, e.endLocked(l, proto.EndedMute))
	}
	e.micEpoch = 0
	e.restartLocked(micStream, 0)
	e.restartLocked(cellsStream, 0)
	send := e.send
	e.mu.Unlock()
	emit(send, out)
}

// OpenCandidate opens the candidate lease announced with wake.candidate
// (generation 1). It uploads nothing until AcceptCandidate(true); without
// that within CandidateAckWait it ends with ttl.
func (e *Executor) OpenCandidate(leaseID string, supportStart uint64) {
	mic := down(sub(supportStart, candidateMicLead), micGrid)
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.send == nil || e.micEpoch == 0 || e.leases[leaseID] != nil {
		return
	}
	l := &lease{id: leaseID, owner: leaseID, reason: proto.LeaseCandidate, gen: 1,
		deadline: e.now() + int64(CandidateAckWait)}
	l.want = [numStreams]bool{true, true, true}
	l.req[micStream] = startReq{capture: mic}
	l.req[cellsStream] = startReq{capture: sub(mic, candidateCellsLead)}
	l.req[refStream] = startReq{capture: sub(mic, candidateRefLead)}
	e.leases[leaseID] = l
}

// AcceptCandidate applies the controller's command.ack for wake.candidate:
// accepted releases the lease's audio; rejected ends it (closed).
func (e *Executor) AcceptCandidate(leaseID string, accepted bool) {
	var out []ended
	e.mu.Lock()
	if l := e.leases[leaseID]; l != nil && !l.accepted {
		if accepted {
			l.accepted = true
			l.deadline = e.now() + int64(DefaultTTL)
			e.activateLocked(l)
		} else {
			out = append(out, e.endLocked(l, proto.EndedClosed))
		}
	}
	send := e.send
	e.mu.Unlock()
	emit(send, out)
	e.Notify()
}

// Open applies uplink.open for a turn, reply or diagnostic lease.
func (e *Executor) Open(env proto.Envelope, b proto.UplinkOpen) error {
	switch b.Reason {
	case proto.LeaseTurn, proto.LeaseReply, proto.LeaseDiagnostic:
	default:
		return ErrInvalid
	}
	if b.LeaseID == "" || env.Generation == 0 || len(b.Streams) == 0 {
		return ErrInvalid
	}
	l := &lease{id: b.LeaseID, owner: b.Owner, reason: b.Reason, gen: env.Generation, accepted: true}
	for key, v := range b.Streams {
		id, ok := streamOf(key)
		if !ok {
			return ErrInvalid
		}
		l.want[id] = true
		if v == proto.StartLive {
			l.req[id] = startReq{live: true}
			continue
		}
		n, err := strconv.ParseUint(v, 10, 64)
		if err != nil {
			return ErrInvalid
		}
		l.req[id] = startReq{capture: n}
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.micEpoch == 0 || e.leases[b.LeaseID] != nil {
		return ErrInvalid
	}
	l.deadline = e.now() + ttl(b.TTLMs)
	e.leases[l.id] = l
	e.activateLocked(l)
	e.Notify()
	return nil
}

// Renew applies uplink.renew. At the lease's generation it extends the TTL;
// at generation + 1 with reason turn it converts an accepted candidate lease
// into the turn's lease in place.
func (e *Executor) Renew(env proto.Envelope, b proto.UplinkRenew) (converted bool, err error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	l := e.leases[b.LeaseID]
	switch {
	case l == nil:
		return false, ErrUnknownLease
	case env.Generation < l.gen:
		return false, ErrStaleGeneration
	case !l.accepted:
		return false, ErrInvalid
	case env.Generation == l.gen:
		if b.Reason != "" || b.Owner != "" {
			return false, ErrInvalid
		}
	case env.Generation == l.gen+1 && l.reason == proto.LeaseCandidate && b.Reason == proto.LeaseTurn && b.Owner != "":
		l.gen, l.reason, l.owner = env.Generation, proto.LeaseTurn, b.Owner
		converted = true
	default:
		return false, ErrInvalid
	}
	l.deadline = e.now() + ttl(b.TTLMs)
	return converted, nil
}

// Close applies uplink.close and reports uplink.ended (closed).
// candidateLease is true when the lease was still a candidate lease, whose
// provisional duck the caller releases.
func (e *Executor) Close(env proto.Envelope, b proto.UplinkClose) (candidateLease bool, err error) {
	e.mu.Lock()
	l := e.leases[b.LeaseID]
	switch {
	case l == nil:
		err = ErrUnknownLease
	case env.Generation < l.gen:
		err = ErrStaleGeneration
	case env.Generation > l.gen:
		err = ErrInvalid
	}
	if err != nil {
		e.mu.Unlock()
		return false, err
	}
	candidateLease = l.reason == proto.LeaseCandidate
	out := []ended{e.endLocked(l, proto.EndedClosed)}
	send := e.send
	e.mu.Unlock()
	emit(send, out)
	return candidateLease, nil
}

// Notify wakes the sender; the supervisor calls it after ring appends.
func (e *Executor) Notify() {
	select {
	case e.wake <- struct{}{}:
	default:
	}
}

// Run is the sender goroutine.
func (e *Executor) Run(ctx context.Context) {
	t := time.NewTicker(tick)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-e.wake:
		case <-t.C:
		}
		e.step()
	}
}

// step expires leases, checks overruns, and sends packets until the sink
// pushes back or nothing is ready.
func (e *Executor) step() {
	e.mu.Lock()
	now := e.now()
	var out []ended
	for _, l := range e.leases {
		if now >= l.deadline {
			out = append(out, e.endLocked(l, proto.EndedTTL))
		}
	}
	for id := range numStreams {
		e.observeLocked(id, now)
		if age, ok := e.liveAgeLocked(id, now); ok && age > int64(OverrunAge) {
			for _, l := range e.leases {
				if l.accepted && l.want[id] {
					out = append(out, e.endLocked(l, proto.EndedOverrun))
				}
			}
		}
	}
	for e.sink != nil {
		frame, enqueued, ok := e.packLocked(now)
		if !ok || e.sink.SendFrame(frame, enqueued) != nil {
			break
		}
		e.commitLocked()
	}
	send := e.send
	e.mu.Unlock()
	emit(send, out)
}

func emit(send SendFunc, out []ended) {
	if send == nil {
		return
	}
	for _, m := range out {
		_, _ = send(proto.TypeUplinkEnded, m.gen, m.body)
	}
}

// endLocked removes l, drops its backfill and returns its uplink.ended.
func (e *Executor) endLocked(l *lease, reason proto.EndedReason) ended {
	delete(e.leases, l.id)
	body := proto.UplinkEnded{
		LeaseID:      l.id,
		Reason:       reason,
		LastSample:   make(map[proto.StreamID]proto.NullU64, numStreams),
		ClippedStart: make(map[proto.StreamID]proto.NullU64, numStreams),
	}
	for id := range numStreams {
		if !l.want[id] {
			continue
		}
		body.LastSample[streamKeys[id]] = l.last[id]
		body.ClippedStart[streamKeys[id]] = l.clipped[id]
		s := &e.streams[id]
		kept := s.runs[:0]
		for _, r := range s.runs {
			if r.lease != l {
				kept = append(kept, r)
			}
		}
		s.runs = kept
		if !e.wantedLocked(id) {
			s.active = false
			s.runs = s.runs[:0]
			s.head, s.count = 0, 0
		}
	}
	return ended{gen: l.gen, body: body}
}

func (e *Executor) wantedLocked(id streamID) bool {
	for _, l := range e.leases {
		if l.accepted && l.want[id] {
			return true
		}
	}
	return false
}

// restartLocked starts a stream's new epoch: sequence 0, nothing queued.
func (e *Executor) restartLocked(id streamID, epoch uint64) {
	s := &e.streams[id]
	s.epoch, s.seq = epoch, 0
	s.runs = s.runs[:0]
	s.disc = false
	s.head, s.count = 0, 0
}

// activateLocked resolves an accepted lease's starts and queues its
// backfill ahead of the stream's live cursor.
func (e *Executor) activateLocked(l *lease) {
	for id := range numStreams {
		if !l.want[id] {
			continue
		}
		start, clipped := e.resolveLocked(id, l.req[id])
		l.start[id], l.clipped[id] = start, clipped
		s := &e.streams[id]
		end := e.ringEnd(id)
		switch {
		case !s.active:
			s.active, s.disc = true, false
			s.runs = s.runs[:0]
			s.head, s.count = 0, 0
			s.observed = end
			s.next = max(start, end)
			if start < end {
				s.runs = append(s.runs, run{lease: l, next: start, end: end})
			}
		case start < s.next:
			s.runs = append(s.runs, run{lease: l, next: start, end: s.next})
		}
	}
}

// resolveLocked maps a requested start to a ring index: capture samples to
// the stream's domain (cells by cell number, reference through the clock
// fits), rounded down to the stream grid and clipped to the oldest valid
// index on that grid. clipped is the header-domain start when a backfill
// start was clipped.
func (e *Executor) resolveLocked(id streamID, req startReq) (start uint64, clipped proto.NullU64) {
	mapped := true
	switch {
	case req.live:
		start = e.ringEnd(id)
	case id == micStream:
		start = req.capture
	case id == cellsStream:
		start = req.capture / ema.CellSamples
	default:
		start, mapped = e.clock.CaptureToReference(req.capture)
	}
	g := grid(id)
	start = down(start, g)
	oldest, ok := e.oldestValid(id)
	if !ok {
		oldest = e.ringEnd(id)
	}
	floor := up(oldest, g)
	if !mapped || start < floor {
		start = floor
		if !req.live {
			clipped = proto.U64(headerIndex(id, floor))
		}
	}
	return start, clipped
}

// observeLocked marks when new live audio entered the ring and forgets
// marks for audio already sent.
func (e *Executor) observeLocked(id streamID, now int64) {
	s := &e.streams[id]
	if !s.active || s.epoch == 0 {
		return
	}
	if end := e.ringEnd(id); end > s.observed {
		s.observed = end
		if s.count == maxMarks {
			// Coalesce into the newest mark: its older time only
			// overstates the age of the samples it now covers.
			s.marks[(s.head+s.count-1)%maxMarks].end = end
		} else {
			s.marks[(s.head+s.count)%maxMarks] = mark{end: end, ns: now}
			s.count++
		}
	}
	for s.count > 0 && s.marks[s.head].end <= s.next {
		s.head = (s.head + 1) % maxMarks
		s.count--
	}
}

// liveAgeLocked is how long the oldest unsent live index has been queued.
func (e *Executor) liveAgeLocked(id streamID, now int64) (int64, bool) {
	s := &e.streams[id]
	if !s.active || s.count == 0 || s.next >= s.observed {
		return 0, false
	}
	return now - s.marks[s.head].ns, true
}

// packLocked builds the next packet: backfill first (mic, cells,
// reference), then live audio. It returns the frame and when its audio
// entered the send queue.
func (e *Executor) packLocked(now int64) (frame []byte, enqueued int64, ok bool) {
	for id := range numStreams {
		s := &e.streams[id]
		if s.epoch == 0 {
			continue
		}
		for len(s.runs) > 0 {
			r := &s.runs[0]
			limit := min(r.end, e.ringEnd(id))
			frame, n, _, built := e.buildLocked(id, &r.next, &r.disc, limit, maxBackfill(id))
			if built {
				e.pending = pending{id: id, backfill: true, end: r.next + n}
				return frame, now, true
			}
			if r.next < r.end {
				break // waiting for the ring to reach the run
			}
			s.runs = s.runs[:copy(s.runs, s.runs[1:])]
		}
	}
	for id := range numStreams {
		s := &e.streams[id]
		if !s.active || s.epoch == 0 || len(s.runs) > 0 {
			continue
		}
		maxN := maxLive(id)
		frame, n, blocked, built := e.buildLocked(id, &s.next, &s.disc, e.ringEnd(id), maxN)
		if !built {
			continue
		}
		age, _ := e.liveAgeLocked(id, now)
		if n == maxN || blocked || age >= int64(liveFlush) {
			e.pending = pending{id: id, end: s.next + n}
			return frame, now - age, true
		}
	}
	return nil, 0, false
}

// commitLocked advances the cursor of the packet the sink accepted and
// records the last sample sent for every lease it serves.
func (e *Executor) commitLocked() {
	p := e.pending
	s := &e.streams[p.id]
	s.seq++
	if p.backfill {
		r := &s.runs[0]
		r.next, r.disc = p.end, false
		if r.next >= r.end {
			s.runs = s.runs[:copy(s.runs, s.runs[1:])]
		}
	} else {
		s.next, s.disc = p.end, false
	}
	last := headerIndex(p.id, p.end) - 1
	for _, l := range e.leases {
		if l.accepted && l.want[p.id] && p.end > l.start[p.id] &&
			(!l.last[p.id].Valid || last > l.last[p.id].V) {
			l.last[p.id] = proto.U64(last)
		}
	}
}

// buildLocked packs audio from *cur, up to limit and maxN frames, into
// e.frame. Missing ranges are skipped, never filled: the cursor moves past
// them and the next packet carries the discontinuity flag. blocked reports a
// packet cut short by a following gap or metadata change, so waiting for
// more audio cannot lengthen it.
func (e *Executor) buildLocked(id streamID, cur *uint64, disc *bool, limit, maxN uint64) (frame []byte, n uint64, blocked, ok bool) {
	for *cur < limit {
		to := min(*cur+maxN, limit)
		from, segs := e.read(id, *cur, to)
		if from > *cur {
			// Overwritten before it was sent: a missing range.
			*cur, *disc = from, true
			continue
		}
		if len(segs) == 0 {
			return nil, 0, false, false
		}
		first := segs[0]
		if first.Gap {
			*cur, *disc = first.End, true
			continue
		}
		base := first.Flags &^ ema.FlagDiscontinuity
		end, unc := first.End, first.UncertaintyUs
		for _, sg := range segs[1:] {
			if sg.Gap || sg.Mask != first.Mask || sg.Flags != base {
				break
			}
			end, unc = sg.End, max(unc, sg.UncertaintyUs)
		}
		n = end - *cur
		frame, ok = e.encodeLocked(id, *cur, n, first.Meta, unc, *disc)
		if !ok {
			// The ring advanced past the range between Read and
			// Copy; the next Read reports it as missing.
			continue
		}
		return frame, n, end < to, true
	}
	return nil, 0, false, false
}

// encodeLocked writes one EMA1 frame for ring indices [first, first+n).
func (e *Executor) encodeLocked(id streamID, first, n uint64, m ring.Meta, unc uint32, disc bool) ([]byte, bool) {
	s := &e.streams[id]
	flags := m.Flags &^ (ema.FlagDiscontinuity | ema.FlagDigitalSilence)
	if disc {
		flags |= ema.FlagDiscontinuity
	}
	payload := e.frame[ema.HeaderSize:]
	var kind ema.Kind
	var size uint64
	switch id {
	case micStream, refStream:
		kind = ema.KindMic
		r := e.rings.Mic
		if id == refStream {
			kind, r = ema.KindReference, e.rings.Ref
		}
		pcm := e.pcm[:n]
		if r.Copy(pcm, first) != nil {
			return nil, false
		}
		if id == refStream && refdsp.AllZero(pcm) {
			flags |= ema.FlagDigitalSilence
		} else {
			ema.PutPCM(payload, pcm)
			size = 2 * n
		}
	case cellsStream:
		kind = ema.KindCells
		cells := e.cells[:n]
		if e.rings.Cells.Copy(cells, first) != nil {
			return nil, false
		}
		for i, c := range cells {
			ema.PutCell(payload[i*ema.CellRecordSize:], c)
		}
		size = n * ema.CellRecordSize
	}
	h := ema.NewHeader(kind, flags, s.epoch, s.seq, headerIndex(id, first), uint32(n))
	h.MonoNs = uint64(max(m.MonoNs, 0))
	h.UncertaintyUs = unc
	if id == refStream {
		h.SourceMask = uint32(m.Mask)
	}
	if h.Encode(e.frame) != nil {
		return nil, false
	}
	return e.frame[:ema.HeaderSize+size], true
}

func (e *Executor) read(id streamID, from, to uint64) (uint64, []ring.Segment) {
	switch id {
	case micStream:
		return e.rings.Mic.Read(from, to, e.segs[:0])
	case cellsStream:
		return e.rings.Cells.Read(from, to, e.segs[:0])
	default:
		return e.rings.Ref.Read(from, to, e.segs[:0])
	}
}

func (e *Executor) ringEnd(id streamID) uint64 {
	switch id {
	case micStream:
		return e.rings.Mic.End()
	case cellsStream:
		return e.rings.Cells.End()
	default:
		return e.rings.Ref.End()
	}
}

func (e *Executor) oldestValid(id streamID) (uint64, bool) {
	switch id {
	case micStream:
		return e.rings.Mic.OldestValid()
	case cellsStream:
		return e.rings.Cells.OldestValid()
	default:
		return e.rings.Ref.OldestValid()
	}
}

// grid is the start grid in ring indices.
func grid(id streamID) uint64 {
	switch id {
	case micStream:
		return micGrid
	case cellsStream:
		return 1 // one cell = 512 capture samples
	default:
		return refGrid
	}
}

func maxBackfill(id streamID) uint64 {
	if id == cellsStream {
		return ema.MaxCells
	}
	return ema.MaxUplinkPCMFrames
}

func maxLive(id streamID) uint64 {
	if id == cellsStream {
		return liveMaxCells
	}
	return liveMaxSamples
}

// headerIndex converts a ring index to the EMA1/JSON sample domain: cells
// are reported by their first capture sample.
func headerIndex(id streamID, idx uint64) uint64 {
	if id == cellsStream {
		return idx * ema.CellSamples
	}
	return idx
}

func ttl(ms int64) int64 {
	if ms <= 0 {
		return int64(DefaultTTL)
	}
	return ms * int64(time.Millisecond)
}

func sub(a, b uint64) uint64 {
	if a < b {
		return 0
	}
	return a - b
}

func down(v, g uint64) uint64 { return v - v%g }

func up(v, g uint64) uint64 { return down(v+g-1, g) }
