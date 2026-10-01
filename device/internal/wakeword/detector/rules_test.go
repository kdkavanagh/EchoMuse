package detector_test

import (
	"slices"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
)

func rule(p detector.Profile, windows int, combine detector.RuleCombine, th float64) detector.OpenRule {
	return detector.OpenRule{Profile: p, Windows: windows, Combine: combine, Threshold: th}
}

var (
	baseIdle     = rule(detector.ProfileIdle, 3, detector.CombineMean, 0.90)
	basePlayback = rule(detector.ProfilePlayback, 3, detector.CombineMean, 0.65)
)

// hopEnd is the end sample of the i-th hop after a reset at sample 0.
func hopEnd(i int) uint64 { return detector.FirstWindowEnd + uint64(i)*detector.HopSamples }

// hops feeds from a fresh harness through hop n-1.
func (h *harness) hops(n int) { h.feed(18 + 2*(n-1)) }

func shadowStats(t *testing.T, h *harness) []detector.ShadowStats {
	t.Helper()
	_, _, st := h.snapshot()
	return st.Shadow
}

func TestAbsentOpenRulesCreditTheBaseline(t *testing.T) {
	h := newHarness(t, scored(0.91), defaults())
	h.hops(1)
	cands, _, st := h.snapshot()
	if len(cands) != 1 || cands[0].Rule != baseIdle {
		t.Fatalf("candidates %+v", cands)
	}
	if st.Shadow == nil || len(st.Shadow) != 0 {
		t.Fatalf("shadow %#v, want an empty list", st.Shadow)
	}
}

// On [0.4, 0.93, 1.0, 1.0…] the 3-window mean first reaches 0.90 at the
// fourth hop; a 2-window rule at 0.90 opens one hop earlier, on the same
// oldest window, so support_start is unchanged.
func TestTwoWindowRuleOpensOneHopEarlier(t *testing.T) {
	seq := scored(0.4, 0.93, 1.0)
	base := newHarness(t, seq, defaults())
	base.hops(4)
	bc, _, _ := base.snapshot()
	if len(bc) != 1 || bc[0].FirstCrossingEnd != hopEnd(3) || bc[0].Rule != baseIdle {
		t.Fatalf("baseline candidates %+v", bc)
	}
	for _, combine := range []detector.RuleCombine{detector.CombineMean, detector.CombineAll} {
		r := rule(detector.ProfileIdle, 2, combine, 0.90)
		h := newRulesHarness(t, seq, defaults(), detector.Rules{Open: []detector.OpenRule{baseIdle, basePlayback, r}})
		h.hops(3)
		cands, _, _ := h.snapshot()
		if len(cands) != 1 {
			t.Fatalf("%s: %d candidates, want 1 at the third hop", combine, len(cands))
		}
		c := cands[0]
		if c.FirstCrossingEnd != hopEnd(2) || c.SupportStart != bc[0].SupportStart || c.Rule != r || c.Threshold != 0.90 {
			t.Fatalf("%s: candidate %+v, baseline support_start %d", combine, c, bc[0].SupportStart)
		}
	}
}

func TestFirstFiringRuleInListOrderIsCredited(t *testing.T) {
	mean := rule(detector.ProfileIdle, 2, detector.CombineMean, 0.92)
	all := rule(detector.ProfileIdle, 2, detector.CombineAll, 0.90)
	for _, order := range [][]detector.OpenRule{{all, mean}, {mean, all}} {
		h := newRulesHarness(t, scored(0.4, 0.93, 1.0), defaults(),
			detector.Rules{Open: append(slices.Clone(order), baseIdle, basePlayback)})
		h.hops(3)
		cands, _, _ := h.snapshot()
		if len(cands) != 1 || cands[0].Rule != order[0] || cands[0].Threshold != order[0].Threshold {
			t.Fatalf("order %v: candidates %+v", order, cands)
		}
	}
}

func TestShadowEpisodeMatchedWithLead(t *testing.T) {
	h := newRulesHarness(t, scored(0.4, 0.93, 1.0), defaults(),
		detector.Rules{Shadow: []detector.OpenRule{rule(detector.ProfileIdle, 2, detector.CombineMean, 0.90)}})
	h.hops(4)
	cands, _, st := h.snapshot()
	if len(cands) != 1 || cands[0].Rule != baseIdle {
		t.Fatalf("shadow rules changed the live candidate: %+v", cands)
	}
	s := st.Shadow[0]
	if s.Hops != 4 || s.Opens != 1 || s.Matched != 1 || s.LeadHist != [7]uint64{0, 0, 0, 0, 1, 0, 0} ||
		s.Unmatched != 0 || s.Retried != 0 || s.LiveOnly != 0 || len(s.Events) != 0 {
		t.Fatalf("shadow %+v, want one match one hop early", s)
	}
}

// A 1-window rule fires on a lone 0.96 the live 3-window mean never reaches.
// Its episode is held 10 s from its open and then counts as unmatched.
func TestShadowEpisodeUnmatchedAfterTenSeconds(t *testing.T) {
	r := rule(detector.ProfileIdle, 1, detector.CombineMean, 0.95)
	h := newRulesHarness(t, scored(0.1, 0.96, 0.1), defaults(), detector.Rules{Shadow: []detector.OpenRule{r}})
	// The hold ends at the first hop at least 160,000 samples after hop 1.
	last := 1
	for hopEnd(last) < hopEnd(1)+detector.ShadowHoldSamples {
		last++
	}
	h.hops(last)
	if s := shadowStats(t, h)[0]; s.Opens != 1 || s.Unmatched != 0 || len(s.Events) != 0 {
		t.Fatalf("resolved before the hold ended: %+v", s)
	}
	h.feed(2)
	cands, _, st := h.snapshot()
	s := st.Shadow[0]
	if len(cands) != 0 || s.Matched != 0 || s.Unmatched != 1 || s.Retried != 0 || len(s.Events) != 1 {
		t.Fatalf("candidates %d shadow %+v", len(cands), s)
	}
	ev := s.Events[0]
	if ev.Kind != detector.ShadowUnmatched || ev.OpenSample != hopEnd(1) || ev.PeakRaw != 0.96 || !slices.Equal(ev.Raws, []float64{0.1, 0.96}) {
		t.Fatalf("event %+v", ev)
	}
}

// A live candidate within 10 s of an unoverlapped episode's open resolves it
// as retried, not unmatched.
func TestShadowEpisodeRetriedByLaterLiveCandidate(t *testing.T) {
	r := rule(detector.ProfileIdle, 1, detector.CombineMean, 0.95)
	h := newRulesHarness(t, scored(0.1, 0.96, 0.1, 0.1, 0.1, 0.95), defaults(), detector.Rules{Shadow: []detector.OpenRule{r}})
	h.hops(8) // the live mean reaches 0.95 at hop 7; the rule reopens at hop 5
	cands, _, st := h.snapshot()
	s := st.Shadow[0]
	if len(cands) != 1 || cands[0].FirstCrossingEnd != hopEnd(7) {
		t.Fatalf("candidates %+v", cands)
	}
	if s.Retried != 1 || s.Unmatched != 0 || len(s.Events) != 1 || s.Events[0].Kind != detector.ShadowRetried ||
		s.Events[0].OpenSample != hopEnd(1) || s.Opens != 2 || s.Matched != 1 || s.LeadHist[5] != 1 {
		t.Fatalf("shadow %+v", s)
	}
	h.feed(2 * 70)
	if s := shadowStats(t, h)[0]; s.Unmatched != 0 || s.Retried != 1 {
		t.Fatalf("retried episode resolved again: %+v", s)
	}
}

// A live candidate no shadow episode overlapped counts live_only for the
// shadow rules of its profile only; hops count per profile.
func TestShadowLiveOnlyAndPerProfileHops(t *testing.T) {
	idle := rule(detector.ProfileIdle, 2, detector.CombineAll, 0.99)
	playback := rule(detector.ProfilePlayback, 1, detector.CombineMean, 0.30)
	h := newRulesHarness(t, scored(0.95, 0.1), defaults(), detector.Rules{Shadow: []detector.OpenRule{idle, playback}})
	h.hops(4) // opens at hop 0, closes after two means below 0.45
	cands, ends, st := h.snapshot()
	if len(cands) != 1 || len(ends) != 1 {
		t.Fatalf("candidates %d ends %d", len(cands), len(ends))
	}
	if st.Shadow[0].LiveOnly != 1 || st.Shadow[0].Hops != 4 || st.Shadow[1].LiveOnly != 0 || st.Shadow[1].Hops != 0 {
		t.Fatalf("shadow %+v", st.Shadow)
	}
	h.profile.Store(detector.ProfilePlayback)
	h.feed(4)
	if s := shadowStats(t, h); s[0].Hops != 4 || s[1].Hops != 2 || s[1].Opens != 0 {
		t.Fatalf("shadow %+v", s)
	}
}

func TestShadowEpisodeAbandonedOnReset(t *testing.T) {
	r := rule(detector.ProfileIdle, 1, detector.CombineMean, 0.95)
	h := newRulesHarness(t, scored(0.1, 0.96, 0.1), defaults(), detector.Rules{Shadow: []detector.OpenRule{r}})
	h.hops(2) // open at hop 1
	h.d.Reset()
	h.feed(18 + 2*80)
	s := shadowStats(t, h)[0]
	if s.Opens != 1 || s.Unmatched != 0 || s.Retried != 0 || s.Matched != 0 || len(s.Events) != 0 {
		t.Fatalf("shadow %+v, want the reset episode abandoned", s)
	}
}

func TestShadowEventsCappedPerWindow(t *testing.T) {
	seq := scored(0.1)
	for range 20 {
		seq = append(seq, scored(0.96, 0.1, 0.1, 0.1)...)
	}
	r := rule(detector.ProfileIdle, 1, detector.CombineMean, 0.95)
	h := newRulesHarness(t, seq, defaults(), detector.Rules{Shadow: []detector.OpenRule{r}})
	h.hops(1 + 20*4 + 70)
	cands, _, st := h.snapshot()
	s := st.Shadow[0]
	if len(cands) != 0 || s.Opens != 20 || s.Unmatched != 20 ||
		len(s.Events) != detector.ShadowEventsPerWindow || s.EventsDropped != 20-detector.ShadowEventsPerWindow {
		t.Fatalf("shadow opens %d unmatched %d events %d dropped %d", s.Opens, s.Unmatched, len(s.Events), s.EventsDropped)
	}
}

// A new policy waits for the open candidate to close, then replaces the
// shadow rules.
func TestPolicyChangeDeferredWhileCandidateOpen(t *testing.T) {
	h := newHarness(t, scored(0.95, 0.95, 0.95, 0.1), defaults())
	h.hops(1)
	r := rule(detector.ProfileIdle, 2, detector.CombineMean, 0.95)
	if err := h.d.SetPolicy(defaults(), detector.Rules{Shadow: []detector.OpenRule{r}}); err != nil {
		t.Fatal(err)
	}
	h.feed(2)
	if s := shadowStats(t, h); len(s) != 0 {
		t.Fatalf("policy applied with a candidate open: %+v", s)
	}
	h.feed(2 * 6)
	_, ends, st := h.snapshot()
	if len(ends) != 1 || len(st.Shadow) != 1 || st.Shadow[0].Rule != r {
		t.Fatalf("ends %+v shadow %+v", ends, st.Shadow)
	}
}

func TestCheckRules(t *testing.T) {
	valid := []detector.Rules{
		{},
		{Open: []detector.OpenRule{baseIdle, basePlayback}},
		{Open: []detector.OpenRule{baseIdle, basePlayback, rule(detector.ProfileIdle, 1, detector.CombineAll, 0.99)},
			Shadow: []detector.OpenRule{rule(detector.ProfilePlayback, 2, detector.CombineMean, 0.30)}},
	}
	for i, r := range valid {
		if err := detector.CheckRules(r); err != nil {
			t.Errorf("valid %d: %v", i, err)
		}
	}
	seven := []detector.OpenRule{baseIdle, basePlayback, baseIdle, basePlayback, baseIdle, basePlayback, baseIdle}
	nine := make([]detector.OpenRule, 9)
	for i := range nine {
		nine[i] = baseIdle
	}
	invalid := []detector.Rules{
		{Open: []detector.OpenRule{}},
		{Open: []detector.OpenRule{baseIdle}},
		{Open: seven},
		{Shadow: nine},
		{Open: []detector.OpenRule{baseIdle, basePlayback, rule(detector.ProfileIdle, 0, detector.CombineMean, 0.9)}},
		{Open: []detector.OpenRule{baseIdle, basePlayback, rule("speech", 2, detector.CombineMean, 0.9)}},
		{Shadow: []detector.OpenRule{rule(detector.ProfileIdle, 2, "max", 0.9)}},
		{Shadow: []detector.OpenRule{rule(detector.ProfileIdle, 2, detector.CombineMean, 1.2)}},
	}
	for i, r := range invalid {
		if detector.CheckRules(r) == nil {
			t.Errorf("invalid %d accepted: %+v", i, r)
		}
	}
	h := newHarness(t, scored(0.01), defaults())
	if err := h.d.SetModel(detector.Model{Scorer: &scriptedScorer{}, GraphSHA256: graphSHA, Thresholds: defaults(), Rules: invalid[0]}); err == nil {
		t.Error("SetModel accepted an empty open rule list")
	}
	if err := h.d.SetPolicy(defaults(), invalid[1]); err == nil {
		t.Error("SetPolicy accepted a rule list without a playback rule")
	}
}
