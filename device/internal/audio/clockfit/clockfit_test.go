package clockfit

import (
	"math"
	"testing"
)

func addCadence(f *Fit, periods int, ppm float64) Result {
	var result Result
	for i := 0; i <= periods; i++ {
		sample := uint64(i * 1280)
		ns := int64(math.Round(float64(i*80_000_000) * (1 + ppm/1e6)))
		result = f.Add(sample, ns)
		if result.IsReset() {
			return result
		}
	}
	return result
}

func TestNominalMapAndUncertainty(t *testing.T) {
	f := New(16000, 80_000_000)
	if result := f.Add(16_000, 2_000_000_000); result != Accepted {
		t.Fatal(result)
	}
	if ns, ok := f.SampleToNs(24_000); !ok || ns != 2_500_000_000 {
		t.Fatalf("SampleToNs = %d %v", ns, ok)
	}
	if sample, ok := f.NsToSample(1_500_000_000); !ok || sample != 8_000 {
		t.Fatalf("NsToSample = %d %v", sample, ok)
	}
	if u, ok := f.UncertaintyNs(42_000_000); !ok || u != 122_000_000 {
		t.Fatalf("uncertainty %d %v", u, ok)
	}
	if got := f.RateHz(); got != 16000 {
		t.Fatalf("rate %f", got)
	}
}

func TestFitsAllowedDrift(t *testing.T) {
	f := New(16000, 80_000_000)
	if result := addCadence(f, 125, 500); result != Accepted {
		t.Fatalf("result %v", result)
	}
	want := 16000 / 1.0005
	if got := f.RateHz(); math.Abs(got-want) > .01 {
		t.Fatalf("rate %.6f, want %.6f", got, want)
	}
	if ns, ok := f.SampleToNs(80_000); !ok || math.Abs(float64(ns)-5_002_500_000) > 2 {
		t.Fatalf("map %d %v", ns, ok)
	}
}

func TestDriftSignalsEpochReset(t *testing.T) {
	f := New(16000, 80_000_000)
	if result := addCadence(f, 125, 2000); result != ResetDrift {
		t.Fatalf("result %v, want ResetDrift", result)
	}
	if _, ok := f.SampleToNs(0); ok {
		t.Fatal("fit not emptied on reset")
	}
}

func TestBackwardsSignalsEpochReset(t *testing.T) {
	f := New(16000, 80_000_000)
	f.Add(0, 1_000_000_000)
	f.Add(1280, 1_080_000_000)
	if result := f.Add(2560, 1_070_000_000); result != ResetBackwards {
		t.Fatalf("result %v", result)
	}
	if _, ok := f.SampleToNs(0); ok {
		t.Fatal("fit not emptied")
	}
}

func TestRejectsLargeResidualWithoutChangingFit(t *testing.T) {
	f := New(16000, 80_000_000)
	f.Add(0, 1_000_000_000)
	f.Add(1280, 1_080_000_000)
	if result := f.Add(2560, 1_250_000_001); result != Rejected {
		t.Fatalf("result %v", result)
	}
	if ns, ok := f.SampleToNs(2560); !ok || ns != 1_160_000_000 {
		t.Fatalf("fit changed: %d %v", ns, ok)
	}
}

func TestWindowDropsOldAnchors(t *testing.T) {
	f := New(16000, 80_000_000)
	f.Add(0, 0)
	// More than 10 s later the old anchor has aged out before residual
	// checking, so a discontinuous offset is accepted as a fresh fit.
	if result := f.Add(176_000, 20_000_000_000); result != Accepted {
		t.Fatalf("result %v", result)
	}
	if ns, ok := f.SampleToNs(176_000); !ok || ns != 20_000_000_000 {
		t.Fatalf("map %d %v", ns, ok)
	}
}
