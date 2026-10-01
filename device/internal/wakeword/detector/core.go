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
	// maxShadowHeld bounds a shadow rule's closed, unoverlapped episodes
	// awaiting ShadowHoldSamples: an episode spans at least three scored hops
	// (its open and two below), so consecutive opens are three hops apart.
	maxShadowHeld = ShadowHoldSamples/(closeAfterBelow+1)/HopSamples + 1
	// leadClamp bounds a matched episode's lead in hops.
	leadClamp = (LeadBuckets - 1) / 2
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
	profile    Profile
	openSample uint64 // first_crossing_end
	overlap    uint32 // bit i: shadow rule i had an episode open with it
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

// shadowEpisode is one shadow rule's would-be candidate.
type shadowEpisode struct {
	epoch      uint64
	openSample uint64
	monoNS     int64
	peak       float64
	raws       [SmoothingWindows]float64
	nRaws      int
}

type shadowEvent struct {
	kind ShadowEventKind
	ep   shadowEpisode
}

// shadowCounts are one shadow rule's counters for the stats window.
type shadowCounts struct {
	hops, opens, matched         uint64
	lead                         [LeadBuckets]uint64
	unmatched, retried, liveOnly uint64
	events                       [ShadowEventsPerWindow]shadowEvent
	nEvents                      int
	eventsDropped                uint64
}

// shadowRule is one shadow rule's state, sized when the policy is applied so
// that evaluating it never allocates.
type shadowRule struct {
	rule    OpenRule
	open    bool
	ep      shadowEpisode
	below   int
	matched bool
	// held are closed episodes no live candidate overlapped, oldest first,
	// waiting ShadowHoldSamples from their open for a live candidate.
	held   [maxShadowHeld]shadowEpisode
	nHeld  int
	counts shadowCounts
}

// pendingPolicy is a policy deferred while a live candidate is open.
type pendingPolicy struct {
	thresholds Thresholds
	rules      Rules
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

	live    []OpenRule   // live open rules, in evaluation order
	shadow  []shadowRule // in shadow_rules order
	pending *pendingPolicy

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
		c.pending = nil
		c.invalid(ReasonReset)
		c.swapModel(&cmd.model)
		c.revision++
		c.usePolicy()
		c.setUnavailable(nil, nil)
	case controlUnavailable:
		c.pending = nil
		c.invalid(ReasonReset)
		c.swapModel(nil)
		reason, detail := cmd.reason, cmd.detail
		c.setUnavailable(&reason, &detail)
	case controlPolicy:
		p := &pendingPolicy{thresholds: cmd.thresholds, rules: cmd.rules}
		if c.candidate != nil {
			c.pending = p
		} else {
			c.applyPolicy(p)
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

// applyPolicy switches to new thresholds and rules between candidates. The
// same policy again is a no-op; a different one restarts every shadow
// episode, keeping the window counters of rules that are still configured.
func (c *core) applyPolicy(p *pendingPolicy) {
	c.pending = nil
	if c.model == nil || c.model.Thresholds == p.thresholds && c.model.Rules.equal(p.rules) {
		return
	}
	m := *c.model
	m.Thresholds, m.Rules = p.thresholds, p.rules
	c.model = &m
	c.usePolicy()
}

// usePolicy derives the live rules and sizes the shadow rule state from the
// current model's policy.
func (c *core) usePolicy() {
	c.live = c.model.Rules.live(c.model.Thresholds)
	old := c.shadow
	c.shadow = make([]shadowRule, len(c.model.Rules.Shadow))
	for i, r := range c.model.Rules.Shadow {
		c.shadow[i].rule = r
		for j := range old {
			if old[j].rule == r {
				c.shadow[i].counts = old[j].counts
				break
			}
		}
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
	c.expireHeld(it)
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
	live := c.candidateHop(it, raw, smoothed, now)
	c.shadowHop(it, raw, smoothed, now, live)
	if live != nil && live.below == closeAfterBelow {
		c.closeCandidate(ReasonBelow)
	}
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

// candidateHop applies the SPEC §5.2 candidate rules to one scored hop: an
// open candidate extends (closing is left to the caller after the shadow
// rules have seen the hop); otherwise the live rules for the hop's profile
// are tried in order and the first that fires opens one. It returns the
// candidate open during this hop, or nil.
func (c *core) candidateHop(it queueItem, raw, smoothed float64, now int64) *candidateState {
	if cs := c.candidate; cs != nil {
		cs.peak = max(cs.peak, smoothed)
		if raw >= cs.threshold {
			cs.supportEnd = it.end
		}
		if smoothed >= cs.threshold*extendFraction {
			cs.below = 0
		} else {
			cs.below++
		}
		return cs
	}
	for _, r := range c.live {
		if r.Profile != it.profile {
			continue
		}
		if oldestEnd, ok := r.fires(c.scores); ok {
			c.openCandidate(it, r, oldestEnd, smoothed, now)
			return c.candidate
		}
	}
	return nil
}

// openCandidate opens a candidate credited to rule, whose oldest window ends
// at oldestEnd, and resolves held shadow episodes as retried.
func (c *core) openCandidate(it queueItem, rule OpenRule, oldestEnd uint64, smoothed float64, now int64) {
	threshold := rule.Threshold
	supportStart := oldestEnd - WindowSamples
	var supportEnd uint64
	for _, s := range c.scores {
		if s.raw >= threshold {
			supportEnd = s.end
		}
	}
	cs := &candidateState{
		id: c.d.cb.NewID(), threshold: threshold, supportEnd: supportEnd, peak: smoothed,
		profile: it.profile, openSample: it.end,
	}
	c.candidate = cs
	c.stats.candidates++
	c.near.candidateSeen = true
	c.retryHeld()

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
		Rule:             rule,
	})
}

// closeCandidate closes the open candidate, counts live_only for the shadow
// rules of its profile that never overlapped it, and applies a policy that
// was deferred while it was open.
func (c *core) closeCandidate(reason CloseReason) {
	cs := c.candidate
	if cs == nil {
		return
	}
	c.candidate = nil
	for i := range c.shadow {
		if s := &c.shadow[i]; s.rule.Profile == cs.profile && cs.overlap&(1<<i) == 0 {
			s.counts.liveOnly++
		}
	}
	c.d.cb.OnCandidateEnd(CandidateEnd{
		CandidateID:  cs.id,
		SupportEnd:   cs.supportEnd,
		PeakSmoothed: cs.peak,
		Reason:       reason,
	})
	if c.pending != nil {
		c.applyPolicy(c.pending)
	}
}

// shadowHop evaluates every shadow rule at one scored hop, after the live
// rules; live is the candidate open during this hop, or nil. An episode
// opens like a live candidate under its own rule and extends or closes like
// one at half its threshold. One open at the same hop as a live candidate is
// matched; a closed unmatched episode is held for a retry.
func (c *core) shadowHop(it queueItem, raw, smoothed float64, now int64, live *candidateState) {
	for i := range c.shadow {
		s := &c.shadow[i]
		if s.rule.Profile == it.profile {
			s.counts.hops++
		}
		closing := false
		switch {
		case s.open:
			s.ep.peak = max(s.ep.peak, raw)
			if smoothed >= s.rule.Threshold*extendFraction {
				s.below = 0
			} else if s.below++; s.below == closeAfterBelow {
				closing = true
			}
		case s.rule.Profile == it.profile:
			if _, ok := s.rule.fires(c.scores); !ok {
				continue
			}
			s.open, s.below, s.matched = true, 0, false
			s.ep = shadowEpisode{epoch: it.epoch, openSample: it.end, monoNS: now, peak: raw}
			for _, sc := range c.scores {
				s.ep.raws[s.ep.nRaws] = sc.raw
				s.ep.nRaws++
			}
			s.counts.opens++
		default:
			continue
		}
		if live != nil {
			if !s.matched {
				s.matched = true
				s.counts.matched++
				lead := (int64(live.openSample) - int64(s.ep.openSample)) / HopSamples
				s.counts.lead[min(max(lead, -leadClamp), leadClamp)+leadClamp]++
			}
			live.overlap |= 1 << i
		}
		if closing {
			s.open = false
			if !s.matched {
				c.hold(s)
			}
		}
	}
}

// hold queues a closed unmatched episode for retry resolution. A full queue
// (impossible at the minimum episode length) resolves its oldest first.
func (c *core) hold(s *shadowRule) {
	if s.nHeld == len(s.held) {
		c.resolveOldest(s, ShadowUnmatched)
	}
	s.held[s.nHeld] = s.ep
	s.nHeld++
}

// expireHeld resolves held episodes whose hold has run out as unmatched: at
// least ShadowHoldSamples after their open, or from another capture epoch.
func (c *core) expireHeld(it queueItem) {
	for i := range c.shadow {
		s := &c.shadow[i]
		for s.nHeld > 0 && (s.held[0].epoch != it.epoch || it.end-s.held[0].openSample >= ShadowHoldSamples) {
			c.resolveOldest(s, ShadowUnmatched)
		}
	}
}

// retryHeld resolves every held episode as retried when a live candidate
// opens: expireHeld has already resolved those it came too late for.
func (c *core) retryHeld() {
	for i := range c.shadow {
		s := &c.shadow[i]
		for s.nHeld > 0 {
			c.resolveOldest(s, ShadowRetried)
		}
	}
}

func (c *core) resolveOldest(s *shadowRule, kind ShadowEventKind) {
	ep := s.held[0]
	copy(s.held[:s.nHeld], s.held[1:s.nHeld])
	s.nHeld--
	if kind == ShadowRetried {
		s.counts.retried++
	} else {
		s.counts.unmatched++
	}
	if s.counts.nEvents == len(s.counts.events) {
		s.counts.eventsDropped++
		return
	}
	s.counts.events[s.counts.nEvents] = shadowEvent{kind: kind, ep: ep}
	s.counts.nEvents++
}

// abandonShadow ends every open shadow episode without counting it: the
// window history it was judged on is gone.
func (c *core) abandonShadow() {
	for i := range c.shadow {
		c.shadow[i].open = false
	}
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
// with reason, abandons open shadow episodes, ends any near-miss episode, and
// clears the smoothing history.
func (c *core) invalid(reason CloseReason) {
	c.closeCandidate(reason)
	c.abandonShadow()
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
		Shadow:           make([]ShadowStats, len(c.shadow)),
	}
	for i := range c.shadow {
		sr := &c.shadow[i]
		n := &sr.counts
		out := &s.Shadow[i]
		*out = ShadowStats{
			Rule: sr.rule, Hops: n.hops, Opens: n.opens, Matched: n.matched, LeadHist: n.lead,
			Unmatched: n.unmatched, Retried: n.retried, LiveOnly: n.liveOnly,
			Events: make([]ShadowEvent, n.nEvents), EventsDropped: n.eventsDropped,
		}
		for j, ev := range n.events[:n.nEvents] {
			out.Events[j] = ShadowEvent{
				Kind: ev.kind, OpenSample: ev.ep.openSample, MonoNS: ev.ep.monoNS, PeakRaw: ev.ep.peak,
				Raws: append([]float64(nil), ev.ep.raws[:ev.ep.nRaws]...),
			}
		}
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
		for i := range c.shadow {
			c.shadow[i].counts = shadowCounts{}
		}
	}
}

func (c *core) close() {
	c.swapModel(nil)
}
