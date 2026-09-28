package capture

import (
	"testing"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

func testTimeline() *Timeline {
	t := NewTimeline()
	var epoch uint64 = 100
	t.newEpoch = func() uint64 {
		epoch++
		return epoch
	}
	return t
}

func period() []int16 { return make([]int16, PeriodSamples) }

func TestIndexesCallbacksAndBackdatesAnchor(t *testing.T) {
	timeline := testTimeline()
	b := timeline.Add(period(), PeriodNs)
	if !b.NewEpoch || b.Reason != ReasonStart || b.Epoch != 101 || b.First != 0 || b.MonoNs != 0 {
		t.Fatalf("first block %+v", b)
	}
	if b.Flags != ema.FlagEstimated || b.UncertaintyUs != 80_000 {
		t.Fatalf("timing %+v", b)
	}
	b = timeline.Add(period(), 2*PeriodNs)
	if b.NewEpoch || b.First != PeriodSamples || b.MonoNs != PeriodNs || b.HasMissing {
		t.Fatalf("second block %+v", b)
	}
}

func TestGapSizingUsesElapsedWholePeriods(t *testing.T) {
	timeline := testTimeline()
	timeline.Add(period(), PeriodNs)
	timeline.Add(period(), 2*PeriodNs)
	// Three periods since the previous completion means two periods lost.
	b := timeline.Add(period(), 5*PeriodNs)
	if b.NewEpoch || !b.HasMissing || b.First != 4*PeriodSamples {
		t.Fatalf("gap block %+v", b)
	}
	want := Range{From: 2 * PeriodSamples, To: 4 * PeriodSamples, MonoNs: 2 * PeriodNs}
	if b.Missing != want || b.Flags != ema.FlagEstimated|ema.FlagDiscontinuity {
		t.Fatalf("missing %+v, flags %d", b.Missing, b.Flags)
	}
}

func TestGapThresholdAndRounding(t *testing.T) {
	timeline := testTimeline()
	timeline.Add(period(), PeriodNs)
	// Exactly 1.5 periods is not a gap ("longer than" half-period late).
	if b := timeline.Add(period(), PeriodNs+3*PeriodNs/2); b.HasMissing {
		t.Fatalf("gap at threshold %+v", b)
	}

	timeline = testTimeline()
	timeline.Add(period(), PeriodNs)
	// 1.75 periods rounds to one lost callback.
	b := timeline.Add(period(), PeriodNs+7*PeriodNs/4)
	if !b.HasMissing || b.Missing.To-b.Missing.From != PeriodSamples {
		t.Fatalf("rounded gap %+v", b)
	}
}

func TestRestartAndBackwardsTimeStartNewEpoch(t *testing.T) {
	timeline := testTimeline()
	first := timeline.Add(period(), PeriodNs)
	timeline.Restart(ReasonPrivacy)
	privacy := timeline.Add(period(), 2*PeriodNs)
	if !privacy.NewEpoch || privacy.Reason != ReasonPrivacy || privacy.Epoch == first.Epoch || privacy.First != 0 {
		t.Fatalf("privacy block %+v", privacy)
	}
	back := timeline.Add(period(), PeriodNs)
	if !back.NewEpoch || back.Reason != ReasonClockReset || back.Epoch == privacy.Epoch || back.First != 0 {
		t.Fatalf("backwards block %+v", back)
	}
}

func TestClockDriftStartsNewEpoch(t *testing.T) {
	timeline := testTimeline()
	var previous uint64
	for i := 1; i < 100; i++ {
		done := int64(float64(i*PeriodNs) * 1.002) // 2,000 ppm slow
		b := timeline.Add(period(), done)
		if i == 1 {
			previous = b.Epoch
			continue
		}
		if b.NewEpoch {
			if b.Reason != ReasonClockReset || b.Epoch == previous || b.First != 0 {
				t.Fatalf("reset block %+v", b)
			}
			return
		}
	}
	t.Fatal("clock drift never reset the epoch")
}
