package detector_test

import (
	"errors"
	"math"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
)

const graphSHA = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f"

// result is one scripted Score outcome; the zero value is an unscored window.
type result struct {
	p      float64
	scored bool
	err    error
}

// scriptedScorer returns results in order, repeating the last one.
type scriptedScorer struct {
	mu      sync.Mutex
	results []result
	calls   int
	closed  bool
	started chan struct{}
	release chan struct{}
}

func scored(ps ...float64) []result {
	out := make([]result, len(ps))
	for i, p := range ps {
		out[i] = result{p: p, scored: true}
	}
	return out
}

func (s *scriptedScorer) Score(pcm []int16) (float64, bool, error) {
	if len(pcm) != detector.WindowSamples {
		panic("window length")
	}
	s.mu.Lock()
	i := min(s.calls, len(s.results)-1)
	s.calls++
	r := s.results[i]
	started, release := s.started, s.release
	s.mu.Unlock()
	if started != nil {
		select {
		case started <- struct{}{}:
		default:
		}
	}
	if release != nil {
		<-release
	}
	return r.p, r.scored, r.err
}

func (s *scriptedScorer) Close() error {
	s.mu.Lock()
	s.closed = true
	s.mu.Unlock()
	return nil
}

func (s *scriptedScorer) Calls() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.calls
}

func (s *scriptedScorer) Closed() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.closed
}

type harness struct {
	d      *detector.Detector
	scorer *scriptedScorer

	profile atomic.Value
	first   uint64

	mu        sync.Mutex
	cands     []detector.Candidate
	ends      []detector.CandidateEnd
	stats     []detector.Stats
	soundFrom uint64
	soundTo   uint64
}

func defaults() detector.Thresholds {
	return detector.Thresholds{Idle: 0.90, Playback: 0.65, NearMiss: 0.17}
}

func newHarness(t *testing.T, results []result, th detector.Thresholds) *harness {
	t.Helper()
	h := &harness{scorer: &scriptedScorer{results: results}}
	h.profile.Store(detector.ProfileIdle)
	var mono atomic.Int64
	var ids atomic.Int64
	d, err := detector.New(detector.Callbacks{
		Profile: func() detector.Profile { return h.profile.Load().(detector.Profile) },
		ProducingSound: func(from, to uint64) bool {
			h.mu.Lock()
			h.soundFrom, h.soundTo = from, to
			h.mu.Unlock()
			return true
		},
		OnCandidate:    func(c detector.Candidate) { h.mu.Lock(); h.cands = append(h.cands, c); h.mu.Unlock() },
		OnCandidateEnd: func(e detector.CandidateEnd) { h.mu.Lock(); h.ends = append(h.ends, e); h.mu.Unlock() },
		OnStats:        func(s detector.Stats) { h.mu.Lock(); h.stats = append(h.stats, s); h.mu.Unlock() },
		NowMonoNS:      func() int64 { return mono.Add(int64(time.Millisecond)) },
		NewID:          func() string { return string(rune('A' + ids.Add(1))) },
	})
	if err != nil {
		t.Fatal(err)
	}
	h.d = d
	t.Cleanup(d.Close)
	if err := d.SetModel(detector.Model{Scorer: h.scorer, GraphSHA256: graphSHA, Thresholds: th}); err != nil {
		t.Fatal(err)
	}
	d.SetEpoch(7)
	return h
}

// feed ingests native 1,280-sample capture blocks and waits for each hop to be
// processed, so policy outcomes are deterministic.
func (h *harness) feed(blocks int) {
	pcm := make([]int16, 1280)
	for range blocks {
		for i := range pcm {
			pcm[i] = int16((h.first + uint64(i)) % 20000)
		}
		h.d.OnBlock(h.first, pcm, false)
		h.first += uint64(len(pcm))
		if h.first%detector.HopSamples == 0 {
			h.d.FlushStats()
		}
	}
}

func (h *harness) gap(blocks int) {
	h.d.OnBlock(h.first, make([]int16, 1280*blocks), true)
	h.first += uint64(1280 * blocks)
	h.d.FlushStats()
}

func (h *harness) snapshot() ([]detector.Candidate, []detector.CandidateEnd, detector.Stats) {
	h.d.FlushStats()
	h.mu.Lock()
	defer h.mu.Unlock()
	return append([]detector.Candidate(nil), h.cands...),
		append([]detector.CandidateEnd(nil), h.ends...),
		h.stats[len(h.stats)-1]
}

func TestHopGridFirstWindowAndAcrossGap(t *testing.T) {
	h := newHarness(t, scored(0.01), defaults())
	h.feed(17) // ends at 21,760: no full grid window yet
	if n := h.scorer.Calls(); n != 0 {
		t.Fatalf("scored %d windows before sample 23,040", n)
	}
	h.feed(1) // 23,040
	if n := h.scorer.Calls(); n != 1 {
		t.Fatalf("first window: %d calls, want 1", n)
	}
	// Missing [23,040, 24,320): the next full valid window ends at the first
	// multiple of 2,560 at or after 24,320 + 22,400 = 46,720, i.e. 48,640.
	h.first += 1280
	h.feed(19) // through 48,640
	if n := h.scorer.Calls(); n != 2 {
		t.Fatalf("after gap: %d calls, want 2", n)
	}
	h.feed(2) // 51,200
	if n := h.scorer.Calls(); n != 3 {
		t.Fatalf("next hop: %d calls, want 3", n)
	}
}

func TestSingleScoreAfterResetSuffices(t *testing.T) {
	h := newHarness(t, scored(0.91), defaults())
	h.feed(18)
	cands, _, _ := h.snapshot()
	if len(cands) != 1 {
		t.Fatalf("%d candidates, want 1", len(cands))
	}
	c := cands[0]
	if c.FirstCrossingEnd != 23040 || c.SupportStart != 640 || c.Threshold != 0.90 || c.Profile != detector.ProfileIdle {
		t.Fatalf("candidate %+v", c)
	}
	if c.CaptureEpoch != 7 || c.GraphSHA256 != graphSHA || c.ScorerRevision != 1 {
		t.Fatalf("identity %+v", c)
	}
	if len(c.Hops) != 1 || *c.Hops[0].Raw != 0.91 || *c.Hops[0].Smoothed != 0.91 {
		t.Fatalf("hops %+v", c.Hops)
	}
}

func TestSmoothingIsMeanOfLastThree(t *testing.T) {
	h := newHarness(t, scored(0.3, 0.6, 0.9, 0.3), detector.Thresholds{Idle: 0.99, Playback: 0.99, NearMiss: 0.1})
	h.feed(24)
	_, _, st := h.snapshot()
	if math.Abs(*st.PeakSmoothed-0.6) > 1e-12 {
		t.Fatalf("peak smoothed %v, want mean(0.3,0.6,0.9)=0.6", *st.PeakSmoothed)
	}
}

func TestUnscoredSlotsFiveKeepSixClear(t *testing.T) {
	for _, tc := range []struct {
		unscored int
		opens    bool
	}{{5, true}, {6, false}} {
		seq := scored(0.9)
		for range tc.unscored {
			seq = append(seq, result{})
		}
		seq = append(seq, scored(0.3)...)
		// The first score happens under a profile it cannot cross; the last
		// under idle 0.6, which only mean(0.9, 0.3) reaches.
		h := newHarness(t, seq, detector.Thresholds{Idle: 0.6, Playback: 0.95, NearMiss: 0.1})
		h.profile.Store(detector.ProfilePlayback)
		h.feed(18)
		h.profile.Store(detector.ProfileIdle)
		h.feed(2 * (tc.unscored + 1))
		cands, _, st := h.snapshot()
		if (len(cands) == 1) != tc.opens {
			t.Fatalf("%d unscored: %d candidates, opens=%v", tc.unscored, len(cands), tc.opens)
		}
		if st.HopsScored != 2 {
			t.Fatalf("hops_scored %d, want 2", st.HopsScored)
		}
		if tc.opens {
			c := cands[0]
			// The unscored hops sit between the two scores in the mean.
			if c.SupportStart != 640 || len(c.Hops) != tc.unscored+2 || c.Hops[1].Raw != nil || c.Hops[1].Smoothed != nil {
				t.Fatalf("support %d hops %+v", c.SupportStart, c.Hops)
			}
		}
	}
}

func TestLatchedThresholdAcrossProfileChange(t *testing.T) {
	// Opens at idle 0.8. Under playback 0.2 the extend level would be 0.1 and
	// the candidate would stay open; latched, it is 0.4.
	h := newHarness(t, scored(0.9, 0.3, 0.5, 0.3, 0.3), detector.Thresholds{Idle: 0.8, Playback: 0.2, NearMiss: 0.1})
	h.feed(18)
	h.profile.Store(detector.ProfilePlayback)
	h.feed(4) // 0.6, 0.567: extend
	if _, ends, _ := h.snapshot(); len(ends) != 0 {
		t.Fatalf("closed early: %+v", ends)
	}
	h.feed(2) // 0.367: one below
	if _, ends, _ := h.snapshot(); len(ends) != 0 {
		t.Fatalf("closed after one below: %+v", ends)
	}
	h.feed(2) // 0.367: second below
	cands, ends, _ := h.snapshot()
	if len(cands) != 1 || len(ends) != 1 {
		t.Fatalf("candidates %d ends %d", len(cands), len(ends))
	}
	e := ends[0]
	if e.Reason != detector.ReasonBelow || e.SupportEnd != 23040 || e.PeakSmoothed != 0.9 || e.CandidateID != cands[0].CandidateID {
		t.Fatalf("end %+v", e)
	}
	if cands[0].LeaseID == cands[0].CandidateID {
		t.Fatal("lease ID reuses the candidate ID")
	}
}

func TestSupportEndIsLastRawAtThreshold(t *testing.T) {
	h := newHarness(t, scored(0.95, 0.95, 0.2, 0.1, 0.1, 0.1), defaults())
	h.feed(28)
	_, ends, _ := h.snapshot()
	if len(ends) != 1 || ends[0].SupportEnd != 25600 {
		t.Fatalf("ends %+v, want support_end 25,600", ends)
	}
}

func TestReArmsAfterClose(t *testing.T) {
	h := newHarness(t, scored(0.95, 0.1, 0.1, 0.1, 0.95, 0.95, 0.95), defaults())
	h.feed(18 + 12)
	cands, ends, _ := h.snapshot()
	if len(cands) != 2 || len(ends) != 1 {
		t.Fatalf("candidates %d ends %d, want a second candidate after close", len(cands), len(ends))
	}
}

func TestCloseReasons(t *testing.T) {
	for _, tc := range []struct {
		name   string
		act    func(h *harness)
		reason detector.CloseReason
	}{
		{"gap", func(h *harness) { h.gap(1) }, detector.ReasonGap},
		{"forward jump", func(h *harness) { h.first += 1280; h.feed(1) }, detector.ReasonGap},
		{"reset", func(h *harness) { h.d.Reset() }, detector.ReasonReset},
		{"epoch", func(h *harness) { h.d.SetEpoch(8) }, detector.ReasonReset},
		{"mute", func(h *harness) { h.d.SetMuted(true) }, detector.ReasonMute},
	} {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t, scored(0.95), defaults())
			h.feed(18)
			tc.act(h)
			_, ends, _ := h.snapshot()
			if len(ends) != 1 || ends[0].Reason != tc.reason {
				t.Fatalf("ends %+v, want %s", ends, tc.reason)
			}
		})
	}
}

func TestMuteSuppressesInference(t *testing.T) {
	h := newHarness(t, scored(0.01), defaults())
	h.d.SetMuted(true)
	h.feed(40)
	if n := h.scorer.Calls(); n != 0 {
		t.Fatalf("scored %d windows while muted", n)
	}
	h.d.SetMuted(false)
	h.feed(18) // 51,200 + 23,040: first full window after unmute is at 74,240
	if n := h.scorer.Calls(); n != 1 {
		t.Fatalf("after unmute: %d calls, want 1", n)
	}
}

func TestNewEpochRestartsGrid(t *testing.T) {
	h := newHarness(t, scored(0.95), defaults())
	h.feed(30)
	h.d.SetEpoch(9)
	h.first = 0
	h.feed(18)
	cands, _, _ := h.snapshot()
	last := cands[len(cands)-1]
	if last.CaptureEpoch != 9 || last.FirstCrossingEnd != 23040 {
		t.Fatalf("candidate %+v", last)
	}
}

func TestNearMissEpisodes(t *testing.T) {
	// ep1 peaks at 0.3 and closes after two means below 0.17: counted.
	// ep2 contains a candidate: not counted.
	seq := scored(0.3, 0.01, 0.01, 0.01, 1, 1, 1, 0.01, 0.01, 0.01, 0.01)
	h := newHarness(t, seq, defaults())
	h.feed(18 + 20)
	cands, ends, st := h.snapshot()
	if len(cands) != 1 || len(ends) != 1 {
		t.Fatalf("candidates %d ends %d", len(cands), len(ends))
	}
	if len(st.NearMisses) != 1 || st.NearMisses[0].Peak != 0.3 {
		t.Fatalf("near misses %+v", st.NearMisses)
	}
	if st.HopsScored != 11 || st.CandidatesOpened != 1 || *st.PeakSmoothed != 1 || *st.GraphSHA256 != graphSHA {
		t.Fatalf("stats %+v", st)
	}
	if st.InferMeanMS == nil || st.InferMaxMS == nil || st.WakeUnavailable != nil {
		t.Fatalf("stats %+v", st)
	}
}

func TestStatsBeforeAnyScoreAreNull(t *testing.T) {
	h := newHarness(t, scored(0.01), defaults())
	_, _, st := h.snapshot()
	if st.InferMeanMS != nil || st.InferMaxMS != nil || st.PeakSmoothed != nil || st.NearMisses == nil {
		t.Fatalf("stats %+v", st)
	}
}

func TestInferenceErrorsInvalidateAndMarkUnavailable(t *testing.T) {
	boom := errors.New("boom")
	seq := append(scored(0.95), result{scored: true, err: boom}, result{scored: true, p: math.NaN()}, result{scored: true, err: boom}, result{p: 0.01, scored: true})
	h := newHarness(t, seq, defaults())
	h.feed(18 + 2)
	_, ends, _ := h.snapshot()
	if len(ends) != 1 || ends[0].Reason != detector.ReasonGap {
		t.Fatalf("error window did not close the candidate as invalid: %+v", ends)
	}
	h.feed(2)
	_, _, st := h.snapshot()
	if st.WakeUnavailable != nil {
		t.Fatalf("unavailable after two errors: %+v", st)
	}
	h.feed(2)
	_, _, st = h.snapshot()
	if st.InferenceErrors != 3 || st.WakeUnavailable == nil || *st.WakeUnavailable != detector.UnavailableInferenceErrors {
		t.Fatalf("stats %+v", st)
	}
}

func TestOverrunDropsOldestPendingHop(t *testing.T) {
	h := newHarness(t, scored(0.95, 0.95), defaults())
	h.scorer.mu.Lock()
	h.scorer.started = make(chan struct{}, 1)
	h.scorer.release = make(chan struct{})
	h.scorer.mu.Unlock()
	pcm := make([]int16, 1280)
	for range 18 {
		h.d.OnBlock(h.first, pcm, false)
		h.first += 1280
	}
	select {
	case <-h.scorer.started:
	case <-time.After(5 * time.Second):
		t.Fatal("inference did not start")
	}
	// Six more hops while the first is in inference: four queue, two drop.
	for range 12 {
		for i := range pcm {
			pcm[i] = int16(i + 1)
		}
		h.d.OnBlock(h.first, pcm, false)
		h.first += 1280
	}
	close(h.scorer.release)
	cands, ends, st := h.snapshot()
	if st.HopsDropped != 2 {
		t.Fatalf("hops_dropped %d, want 2", st.HopsDropped)
	}
	if len(ends) == 0 || ends[0].Reason != detector.ReasonOverrun {
		t.Fatalf("ends %+v", ends)
	}
	// The four surviving hops score after the overrun cleared history.
	if n := h.scorer.Calls(); n != 5 {
		t.Fatalf("%d inferences, want 1 + 4 surviving", n)
	}
	if len(cands) != 2 || cands[1].FirstCrossingEnd != 23040+3*2560 {
		t.Fatalf("candidates %+v", cands)
	}
}

func TestProducingSoundRange(t *testing.T) {
	// Fourteen low scores, then 0.95: the mean first reaches 0.9 at the third
	// 0.95, so support_start is the first 0.95's window start, 36,480.
	h := newHarness(t, scored(0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.95), defaults())
	h.feed(18 + 32)
	cands, _, _ := h.snapshot()
	if len(cands) != 1 || !cands[0].ProducingSound {
		t.Fatalf("candidates %+v", cands)
	}
	c := cands[0]
	if c.SupportStart != 36480 {
		t.Fatalf("support_start %d, want 36,480", c.SupportStart)
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.soundFrom != c.SupportStart-32000 || h.soundTo != c.FirstCrossingEnd {
		t.Fatalf("range [%d, %d], want [%d, %d]", h.soundFrom, h.soundTo, c.SupportStart-32000, c.FirstCrossingEnd)
	}
}

func TestModelSwitchAndUnavailable(t *testing.T) {
	h := newHarness(t, scored(0.95), defaults())
	h.feed(18)
	next := &scriptedScorer{results: scored(0.95)}
	if err := h.d.SetModel(detector.Model{Scorer: next, GraphSHA256: graphSHA, Thresholds: defaults()}); err != nil {
		t.Fatal(err)
	}
	if !h.scorer.Closed() {
		t.Fatal("previous scorer not closed after the switch")
	}
	h.feed(18) // the new revision needs a full window after the switch
	cands, ends, _ := h.snapshot()
	if len(ends) == 0 || ends[0].Reason != detector.ReasonReset {
		t.Fatalf("ends %+v", ends)
	}
	if len(cands) != 2 || cands[1].ScorerRevision != 2 || cands[1].FirstCrossingEnd != 46080 {
		t.Fatalf("candidates %+v", cands)
	}

	if err := h.d.SetUnavailable(detector.UnavailableLoadFailed, "bad graph"); err != nil {
		t.Fatal(err)
	}
	h.feed(20)
	_, _, st := h.snapshot()
	if !next.Closed() || st.WakeUnavailable == nil || *st.WakeUnavailable != detector.UnavailableLoadFailed ||
		st.Detail == nil || *st.Detail != "bad graph" || st.GraphSHA256 != nil {
		t.Fatalf("stats %+v", st)
	}
	if n := next.Calls(); n != 1 {
		t.Fatalf("scored %d windows while unavailable", n-1)
	}
	if err := h.d.SetUnavailable(detector.UnavailableInferenceErrors, ""); err == nil {
		t.Fatal("accepted inference_errors as an asset reason")
	}
}

func TestSetModelValidates(t *testing.T) {
	h := newHarness(t, scored(0.01), defaults())
	bad := []detector.Model{
		{Scorer: nil, GraphSHA256: graphSHA, Thresholds: defaults()},
		{Scorer: &scriptedScorer{}, GraphSHA256: "abc", Thresholds: defaults()},
		{Scorer: &scriptedScorer{}, GraphSHA256: graphSHA, Thresholds: detector.Thresholds{Idle: 0.9, Playback: 0, NearMiss: 0.17}},
	}
	for i, m := range bad {
		if err := h.d.SetModel(m); err == nil {
			t.Errorf("model %d accepted", i)
		}
	}
}

func TestOnBlockDoesNotAllocate(t *testing.T) {
	h := newHarness(t, scored(0.01), defaults())
	pcm := make([]int16, 1280)
	allocs := testing.AllocsPerRun(50, func() {
		h.d.OnBlock(h.first, pcm, false)
		h.first += 1280
	})
	if allocs != 0 {
		t.Fatalf("OnBlock allocates %v times per block", allocs)
	}
}

func TestSetThresholdsKeepsLatchedCandidate(t *testing.T) {
	h := newHarness(t, scored(0.95, 0.47), defaults())
	h.feed(18)
	// Means settle at 0.47: below half of 0.99, above half of the latched 0.90.
	if err := h.d.SetThresholds(detector.Thresholds{Idle: 0.99, Playback: 0.65, NearMiss: 0.17}); err != nil {
		t.Fatal(err)
	}
	h.feed(10)
	cands, ends, _ := h.snapshot()
	if len(cands) != 1 || len(ends) != 0 || cands[0].ScorerRevision != 1 {
		t.Fatalf("candidates %+v ends %+v", cands, ends)
	}
}

func TestCheckPolicy(t *testing.T) {
	if err := detector.CheckPolicy(2, 3, 6); err != nil {
		t.Fatal(err)
	}
	for _, p := range [][3]int{{1, 3, 6}, {2, 4, 6}, {2, 3, 5}} {
		if detector.CheckPolicy(p[0], p[1], p[2]) == nil {
			t.Errorf("accepted %v", p)
		}
	}
}
