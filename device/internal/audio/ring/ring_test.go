package ring

import (
	"errors"
	"slices"
	"testing"
)

func ints(from, n int) []int16 {
	v := make([]int16, n)
	for i := range v {
		v[i] = int16(from + i)
	}
	return v
}

func TestWraparoundAndClipping(t *testing.T) {
	r := New[int16](8, 8, 10)
	if err := r.Append(0, ints(0, 6), Meta{MonoNs: 100, Flags: 8}); err != nil {
		t.Fatal(err)
	}
	if err := r.Append(6, ints(6, 6), Meta{MonoNs: 160, Flags: 8}); err != nil {
		t.Fatal(err)
	}
	if r.Start() != 4 || r.End() != 12 {
		t.Fatalf("coverage [%d,%d), want [4,12)", r.Start(), r.End())
	}
	if oldest, ok := r.OldestValid(); !ok || oldest != 4 {
		t.Fatalf("oldest valid %d %v", oldest, ok)
	}
	var segbuf [4]Segment
	clipped, segs := r.Read(0, 20, segbuf[:0])
	if clipped != 4 || len(segs) != 2 || segs[0].First != 4 || segs[0].End != 6 || segs[0].MonoNs != 140 || segs[1].First != 6 || segs[1].End != 12 {
		t.Fatalf("clipped=%d segs=%+v", clipped, segs)
	}
	out := make([]int16, 8)
	if err := r.Copy(out, 4); err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(out, ints(4, 8)) {
		t.Fatalf("wrapped copy %v", out)
	}
	if err := r.Copy(make([]int16, 1), 3); !errors.Is(err, ErrNotRetained) {
		t.Fatalf("old copy: %v", err)
	}
}

func TestExplicitGapSegments(t *testing.T) {
	r := New[int16](16, 8, 100)
	_ = r.Append(0, []int16{1, 2, 3}, Meta{MonoNs: 1_000, Mask: 1})
	r.Missing(7)
	r.Missing(9) // adjacent missing ranges merge
	_ = r.Append(9, []int16{9, 10}, Meta{MonoNs: 3_000, Mask: 2})
	var dst [4]Segment
	_, got := r.Read(2, 10, dst[:0])
	want := []Segment{
		{First: 2, End: 3, Meta: Meta{MonoNs: 1_200, Mask: 1}},
		{First: 3, End: 9, Gap: true},
		{First: 9, End: 10, Meta: Meta{MonoNs: 3_000, Mask: 2}},
	}
	if !slices.Equal(got, want) {
		t.Fatalf("segments\n got %+v\nwant %+v", got, want)
	}
	if err := r.Copy(make([]int16, 2), 2); !errors.Is(err, ErrNotRetained) {
		t.Fatalf("copy through gap: %v", err)
	}
	if oldest, ok := r.OldestValid(); !ok || oldest != 0 {
		t.Fatalf("oldest valid %d %v", oldest, ok)
	}
}

func TestGapCanEvictAllValidSamples(t *testing.T) {
	r := New[int16](6, 6, 1)
	_ = r.Append(0, ints(0, 3), Meta{})
	r.Missing(12)
	if r.Start() != 6 || r.End() != 12 {
		t.Fatalf("coverage [%d,%d)", r.Start(), r.End())
	}
	if _, ok := r.OldestValid(); ok {
		t.Fatal("expected no valid sample")
	}
	var dst [2]Segment
	from, segs := r.Read(0, 20, dst[:0])
	if from != 6 || len(segs) != 1 || !segs[0].Gap || segs[0].First != 6 || segs[0].End != 12 {
		t.Fatalf("from=%d segs=%+v", from, segs)
	}
}

func TestAppendRulesAndReset(t *testing.T) {
	r := New[int16](4, 4, 1)
	if err := r.Append(1, []int16{1}, Meta{}); !errors.Is(err, ErrNotContiguous) {
		t.Fatalf("jump: %v", err)
	}
	if err := r.Append(0, ints(0, 5), Meta{}); !errors.Is(err, ErrTooLarge) {
		t.Fatalf("too large: %v", err)
	}
	r.Reset(100)
	if r.Start() != 100 || r.End() != 100 {
		t.Fatalf("reset coverage [%d,%d)", r.Start(), r.End())
	}
	if err := r.Append(100, []int16{7}, Meta{}); err != nil {
		t.Fatal(err)
	}
	out := make([]int16, 1)
	if err := r.Copy(out, 100); err != nil || out[0] != 7 {
		t.Fatalf("copy %v %v", out, err)
	}
}

func TestRequiredCapacities(t *testing.T) {
	if NewMic().Capacity() != MicSamples || NewReference().Capacity() != ReferenceSamples || NewCells().Capacity() != CellRecords {
		t.Fatal("wrong required ring capacity")
	}
}
