package detector

import "time"

const (
	// maxRecords holds every hop slot an opening mean can span: three scored
	// windows with at most five unscored slots between consecutive ones.
	maxRecords = SmoothingWindows + (SmoothingWindows-1)*(ClearAfterUnscored-1)
	// producingSoundLookback: producing_sound covers [support_start − 2 s, open].
	producingSoundLookback = 2 * 16000
	// extendFraction of the latched threshold keeps a candidate open.
	extendFraction = 0.5
	// closeAfterBelow consecutive scored values below the extend level (or the
	// near-miss floor) close a candidate (or a near-miss episode).
	closeAfterBelow = 2
	// errorWindow and errorLimit: three inference errors within 60 s report
	// wake_unavailable=inference_errors (SPEC §8.1).
	errorWindow = 60 * time.Second
	errorLimit  = 3
)

type scoreEntry struct {
	end uint64
	raw float64
}

type hopRecord struct {
	end      uint64
	raw      float64
	smoothed float64
	scored   bool
	profile  Profile
}

type candidateState struct {
	id         string
	threshold  float64
	supportEnd uint64
	peak       float64
	below      int
}

type nearState struct {
	open          bool
	monoNS        int64
	peak          float64
	below         int
	candidateSeen bool
}

type statsState struct {
	startNS      int64
	hopsScored   uint64
	hopsDropped  uint64
	inferErrors  uint64
	inferRuns    uint64
	inferNS      int64
	inferMaxNS   int64
	nearMisses   []NearMiss
	candidates   uint64
	peakSmoothed float64
}

// core is the policy state machine. Only the wake goroutine touches it.
type core struct {
	d *Detector

	model    *Model
	revision uint64

	scores    []scoreEntry // scored windows in the mean, oldest first
	records   []hopRecord  // hop slots since the oldest score in the mean
	unscored  int          // consecutive unscored hop slots
	candidate *candidateState
	near      nearState

	errorTimes  []int64
	unavailable *UnavailableReason
	detail      *string
	stats       statsState
}

func newCore(d *Detector) *core {
	return &core{
		d:          d,
		scores:     make([]scoreEntry, 0, SmoothingWindows),
		records:    make([]hopRecord, 0, maxRecords),
		errorTimes: make([]int64, 0, errorLimit+1),
		stats:      statsState{startNS: d.cb.NowMonoNS()},
	}
}

func (c *core) control(cmd control) {
	switch cmd.kind {
	case controlReset:
		c.invalid(ReasonReset)
	case controlMute:
		c.invalid(ReasonMute)
	case controlModel:
		c.invalid(ReasonReset)
		c.swapModel(&cmd.model)
		c.revision++
		c.setUnavailable(nil, nil)
	case controlUnavailable:
		c.invalid(ReasonReset)
		c.swapModel(nil)
		reason, detail := cmd.reason, cmd.detail
		c.setUnavailable(&reason, &detail)
	case controlThresholds:
		if c.model != nil {
			m := *c.model
			m.Thresholds = cmd.thresholds
			c.model = &m
		}
	case controlFlush:
		c.emitStats(false)
	}
	if cmd.ack != nil {
		close(cmd.ack)
	}
}

func (c *core) swapModel(m *Model) {
	old := c.model
	c.model = m
	c.errorTimes = c.errorTimes[:0]
	if old != nil && (m == nil || old.Scorer != m.Scorer) {
		_ = old.Scorer.Close()
	}
}

func (c *core) item(it queueItem) {
	switch it.kind {
	case itemOverrun:
		c.stats.hopsDropped += it.dropped
		c.invalid(ReasonOverrun)
	case itemGap:
		c.invalid(ReasonGap)
	case itemHop:
		c.score(it)
	}
}

func (c *core) score(it queueItem) {
	if c.model == nil {
		return
	}
	start := time.Now()
	raw, scored, err := c.model.Scorer.Score(it.slot.pcm[:])
	elapsed := time.Since(start)
	now := c.d.cb.NowMonoNS()
	if err != nil || scored && !finite(raw) {
		c.stats.inferErrors++
		c.errorTimes = append(c.errorTimes, now)
		c.updateErrorState(now, true)
		c.invalid(ReasonGap)
		return
	}
	c.updateErrorState(now, true)
	if !scored {
		c.unscored++
		if c.unscored >= ClearAfterUnscored {
			c.scores = c.scores[:0]
			c.records = c.records[:0]
			c.unscored = 0
		} else if len(c.scores) > 0 {
			c.appendRecord(hopRecord{end: it.end, profile: it.profile})
		}
		return
	}

	c.stats.hopsScored++
	c.stats.inferRuns++
	c.stats.inferNS += elapsed.Nanoseconds()
	c.stats.inferMaxNS = max(c.stats.inferMaxNS, elapsed.Nanoseconds())
	c.unscored = 0
	if len(c.scores) == SmoothingWindows {
		copy(c.scores, c.scores[1:])
		c.scores = c.scores[:SmoothingWindows-1]
	}
	c.scores = append(c.scores, scoreEntry{end: it.end, raw: raw})
	var sum float64
	for _, s := range c.scores {
		sum += s.raw
	}
	smoothed := sum / float64(len(c.scores))
	c.stats.peakSmoothed = max(c.stats.peakSmoothed, smoothed)
	c.appendRecord(hopRecord{end: it.end, raw: raw, smoothed: smoothed, scored: true, profile: it.profile})
	c.candidateHop(it, raw, smoothed, now)
	c.nearHop(smoothed, now)
}

// appendRecord keeps the hop slots from the oldest score in the mean onward.
func (c *core) appendRecord(r hopRecord) {
	if len(c.scores) > 0 {
		oldest := c.scores[0].end
		drop := 0
		for drop < len(c.records) && c.records[drop].end < oldest {
			drop++
		}
		if drop > 0 {
			n := copy(c.records, c.records[drop:])
			c.records = c.records[:n]
		}
	}
	if len(c.records) == maxRecords {
		n := copy(c.records, c.records[1:])
		c.records = c.records[:n]
	}
	c.records = append(c.records, r)
}

// candidateHop applies the SPEC §5.2 candidate rules to one scored hop.
func (c *core) candidateHop(it queueItem, raw, smoothed float64, now int64) {
	if cs := c.candidate; cs != nil {
		cs.peak = max(cs.peak, smoothed)
		if raw >= cs.threshold {
			cs.supportEnd = it.end
		}
		if smoothed >= cs.threshold*extendFraction {
			cs.below = 0
			return
		}
		if cs.below++; cs.below == closeAfterBelow {
			c.closeCandidate(ReasonBelow)
		}
		return
	}

	threshold, ok := c.model.Thresholds.forProfile(it.profile)
	if !ok || smoothed < threshold {
		return
	}
	oldestEnd := c.scores[0].end
	supportStart := oldestEnd - WindowSamples
	var supportEnd uint64
	for _, s := range c.scores {
		if s.raw >= threshold {
			supportEnd = s.end
		}
	}
	cs := &candidateState{id: c.d.cb.NewID(), threshold: threshold, supportEnd: supportEnd, peak: smoothed}
	c.candidate = cs
	c.stats.candidates++
	c.near.candidateSeen = true

	var from uint64
	if supportStart > producingSoundLookback {
		from = supportStart - producingSoundLookback
	}
	hops := make([]Hop, len(c.records))
	for i, r := range c.records {
		hops[i] = Hop{EndSample: r.end, Profile: r.profile}
		if r.scored {
			raw, sm := r.raw, r.smoothed
			hops[i].Raw, hops[i].Smoothed = &raw, &sm
		}
	}
	c.d.cb.OnCandidate(Candidate{
		CandidateID:      cs.id,
		LeaseID:          c.d.cb.NewID(),
		CaptureEpoch:     it.epoch,
		GraphSHA256:      c.model.GraphSHA256,
		ScorerRevision:   c.revision,
		Profile:          it.profile,
		Threshold:        threshold,
		ProducingSound:   c.d.cb.ProducingSound(from, it.end),
		FirstCrossingEnd: it.end,
		SupportStart:     supportStart,
		MonoNS:           now,
		Hops:             hops,
	})
}

func (c *core) closeCandidate(reason CloseReason) {
	cs := c.candidate
	if cs == nil {
		return
	}
	c.candidate = nil
	c.d.cb.OnCandidateEnd(CandidateEnd{
		CandidateID:  cs.id,
		SupportEnd:   cs.supportEnd,
		PeakSmoothed: cs.peak,
		Reason:       reason,
	})
}

// nearHop tracks near-miss episodes (SPEC §5.2). candidateHop runs first, so
// an episode opened on the candidate's own hop is already marked.
func (c *core) nearHop(smoothed float64, now int64) {
	floor := c.model.Thresholds.NearMiss
	if !c.near.open {
		if smoothed >= floor {
			c.near = nearState{open: true, monoNS: now, peak: smoothed, candidateSeen: c.candidate != nil}
		}
		return
	}
	c.near.peak = max(c.near.peak, smoothed)
	if smoothed >= floor {
		c.near.below = 0
		return
	}
	if c.near.below++; c.near.below == closeAfterBelow {
		c.endNearMiss()
	}
}

// endNearMiss closes the open episode, counting it if no candidate opened
// during it.
func (c *core) endNearMiss() {
	if c.near.open && !c.near.candidateSeen {
		c.stats.nearMisses = append(c.stats.nearMisses, NearMiss{MonoNS: c.near.monoNS, Peak: c.near.peak})
	}
	c.near = nearState{}
}

// invalid handles an invalid window, reset, or mute: it closes any candidate
// with reason, ends any near-miss episode, and clears the smoothing history.
func (c *core) invalid(reason CloseReason) {
	c.closeCandidate(reason)
	c.endNearMiss()
	c.scores = c.scores[:0]
	c.records = c.records[:0]
	c.unscored = 0
}

// updateErrorState ages inference errors out of the 60 s window and sets or
// clears wake_unavailable=inference_errors, sending stats on a change when
// notify is set.
func (c *core) updateErrorState(now int64, notify bool) {
	cutoff := now - int64(errorWindow)
	drop := 0
	for drop < len(c.errorTimes) && c.errorTimes[drop] <= cutoff {
		drop++
	}
	if drop > 0 {
		n := copy(c.errorTimes, c.errorTimes[drop:])
		c.errorTimes = c.errorTimes[:n]
	}
	errored := c.unavailable != nil && *c.unavailable == UnavailableInferenceErrors
	switch {
	case len(c.errorTimes) >= errorLimit && !errored:
		reason := UnavailableInferenceErrors
		c.unavailable, c.detail = &reason, nil
	case len(c.errorTimes) < errorLimit && errored:
		c.unavailable, c.detail = nil, nil
	default:
		return
	}
	if notify {
		c.emitStats(false)
	}
}

// setUnavailable changes wake_unavailable and sends stats immediately.
func (c *core) setUnavailable(reason *UnavailableReason, detail *string) {
	same := (c.unavailable == nil) == (reason == nil) &&
		(reason == nil || *c.unavailable == *reason) &&
		(c.detail == nil) == (detail == nil) &&
		(detail == nil || *c.detail == *detail)
	if same {
		return
	}
	c.unavailable, c.detail = reason, detail
	c.emitStats(false)
}

// emitStats sends wake.stats for the window since startNS; reset starts the
// next 30 s window.
func (c *core) emitStats(reset bool) {
	now := c.d.cb.NowMonoNS()
	c.updateErrorState(now, false)
	s := Stats{
		WindowMS:         max(0, (now-c.stats.startNS)/int64(time.Millisecond)),
		HopsScored:       c.stats.hopsScored,
		HopsDropped:      c.stats.hopsDropped,
		InferenceErrors:  c.stats.inferErrors,
		NearMisses:       append([]NearMiss{}, c.stats.nearMisses...),
		CandidatesOpened: c.stats.candidates,
	}
	if c.stats.inferRuns > 0 {
		mean := float64(c.stats.inferNS) / float64(c.stats.inferRuns) / float64(time.Millisecond)
		maxMS := float64(c.stats.inferMaxNS) / float64(time.Millisecond)
		s.InferMeanMS, s.InferMaxMS = &mean, &maxMS
	}
	if c.stats.hopsScored > 0 {
		peak := c.stats.peakSmoothed
		s.PeakSmoothed = &peak
	}
	if c.model != nil {
		graph := c.model.GraphSHA256
		s.GraphSHA256 = &graph
	}
	if c.unavailable != nil {
		reason := *c.unavailable
		s.WakeUnavailable = &reason
	}
	if c.detail != nil {
		detail := *c.detail
		s.Detail = &detail
	}
	c.d.cb.OnStats(s)
	if reset {
		c.stats = statsState{startNS: now, nearMisses: c.stats.nearMisses[:0]}
	}
}

func (c *core) close() {
	c.swapModel(nil)
}
