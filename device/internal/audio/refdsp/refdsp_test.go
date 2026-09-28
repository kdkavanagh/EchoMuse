package refdsp

import (
	"math"
	"math/cmplx"
	"math/rand/v2"
	"slices"
	"testing"
)

func responseDB(freq float64) float64 {
	var z complex128
	for n, h := range Coeffs {
		phase := -2 * math.Pi * freq * float64(n) / RenderHz
		z += complex(h, 0) * cmplx.Exp(complex(0, phase))
	}
	return 20 * math.Log10(cmplx.Abs(z))
}

func TestCoefficientContractAndResponse(t *testing.T) {
	var sum float64
	for _, h := range Coeffs {
		sum += h
	}
	if math.Abs(sum-1) > 1e-14 {
		t.Fatalf("sum %.17g", sum)
	}
	if db := responseDB(1000); math.Abs(db-(-0.0107)) > 0.005 {
		t.Fatalf("1 kHz %.4f dB", db)
	}
	if db := responseDB(7200); math.Abs(db-(-6.032)) > 0.05 {
		t.Fatalf("7.2 kHz %.4f dB", db)
	}
	if db := responseDB(8000); db >= -50 {
		t.Fatalf("8 kHz %.4f dB", db)
	}
	for i := range Coeffs {
		if math.Abs(Coeffs[i]-Coeffs[Taps-1-i]) > 1e-15 {
			t.Fatalf("not symmetric at %d", i)
		}
	}
}

func TestImpulseAlignmentAndWarmupGap(t *testing.T) {
	var d Decimator
	in := make([]int16, 127)
	in[63] = 10_000 // output k=21 is aligned with render sample 3k=63
	out := make([]int16, MaxOutput(len(in)))
	got, err := d.Push(0, in, 3, out)
	if err != nil {
		t.Fatal(err)
	}
	if got.MissingFrom != 0 || got.MissingTo != FirstValid || got.First != FirstValid || len(got.Samples) != 1 {
		t.Fatalf("output %+v", got)
	}
	want := int16(math.Round(10_000 * Coeffs[Delay]))
	if got.Samples[0] != want {
		t.Fatalf("impulse y[21]=%d, want centre tap %d", got.Samples[0], want)
	}
	if got.Mask != 3 {
		t.Fatalf("mask %d", got.Mask)
	}
}

func run(t *testing.T, in []int16, splits []int) (missing uint64, samples []int16) {
	t.Helper()
	var d Decimator
	var first uint64
	for _, n := range splits {
		dst := make([]int16, MaxOutput(n))
		o, err := d.Push(first, in[first:first+uint64(n)], 1, dst)
		if err != nil {
			t.Fatal(err)
		}
		first += uint64(n)
		if o.MissingTo > o.MissingFrom {
			missing = max(missing, o.MissingTo)
		}
		samples = append(samples, o.Samples...)
	}
	return missing, samples
}

func TestHistoryAcrossArbitraryBlockSplits(t *testing.T) {
	rng := rand.New(rand.NewPCG(17, 29))
	in := make([]int16, 4096)
	for i := range in {
		in[i] = int16(rng.IntN(65536) - 32768)
	}
	_, want := run(t, in, []int{len(in)})
	var splits []int
	for remain := len(in); remain > 0; {
		n := min(remain, 1+rng.IntN(173))
		splits = append(splits, n)
		remain -= n
	}
	missing, got := run(t, in, splits)
	if missing != FirstValid {
		t.Fatalf("missing through %d", missing)
	}
	if !slices.Equal(got, want) {
		for i := range min(len(got), len(want)) {
			if got[i] != want[i] {
				t.Fatalf("first difference %d: %d != %d", i, got[i], want[i])
			}
		}
		t.Fatalf("output lengths %d != %d", len(got), len(want))
	}
}

func TestEpochRestartAndContiguity(t *testing.T) {
	var d Decimator
	dst := make([]int16, MaxOutput(100))
	if _, err := d.Push(1, make([]int16, 100), 0, dst); err != ErrNotContiguous {
		t.Fatalf("jump: %v", err)
	}
	if _, err := d.Push(0, make([]int16, 100), 0, dst[:0]); err != ErrShortDst {
		t.Fatalf("short dst: %v", err)
	}
	if _, err := d.Push(0, make([]int16, 100), 0, dst); err != nil {
		t.Fatal(err)
	}
	d.Reset()
	if out, err := d.Push(0, make([]int16, 64), 0, make([]int16, MaxOutput(64))); err != nil || out.MissingTo != 1 {
		t.Fatalf("restart %+v %v", out, err)
	}
}

func TestAllZero(t *testing.T) {
	if !AllZero(nil) || !AllZero(make([]int16, 1280)) || AllZero([]int16{0, 0, -1, 0}) {
		t.Fatal("digital silence detection")
	}
}
