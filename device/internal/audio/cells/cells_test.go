package cells

import (
	"math"
	"slices"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/audio/clockfit"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

type maskFn func(uint64, uint64) uint8

func (f maskFn) MaskFor(a, b uint64) uint8 { return f(a, b) }

type emitted struct {
	first uint64
	cells []ema.Cell
	ns    int64
}

func collector() (*[]emitted, Sink) {
	var out []emitted
	return &out, func(first uint64, cells []ema.Cell, ns int64) {
		out = append(out, emitted{first, slices.Clone(cells), ns})
	}
}

func TestEnergySilenceAndSine(t *testing.T) {
	if got := Energy(make([]int16, cellSamples)); got != ema.CellEMin {
		t.Fatalf("silence E = %d, want %d", got, ema.CellEMin)
	}
	sine := make([]int16, cellSamples)
	for i := range sine {
		sine[i] = int16(math.Round(32767 * math.Sin(2*math.Pi*float64(i)/32)))
	}
	if got := Energy(sine); got < -303 || got > -301 {
		t.Fatalf("full-scale sine E = %.2f dB, want about -3.01", float64(got)/100)
	}
}

func TestRemainderCarryAcrossCaptureBlocks(t *testing.T) {
	got, sink := collector()
	var intervals [][2]uint64
	masks := maskFn(func(a, b uint64) uint8 {
		intervals = append(intervals, [2]uint64{a, b})
		return 5
	})
	a := NewAccumulator(masks, sink)
	if err := a.Push(0, make([]int16, 1280), 1_000_000, false); err != nil {
		t.Fatal(err)
	}
	full := make([]int16, 1280)
	for i := range full {
		full[i] = 32767
	}
	if err := a.Push(1280, full, 81_000_000, true); err != nil {
		t.Fatal(err)
	}
	var cells []ema.Cell
	for _, e := range *got {
		cells = append(cells, e.cells...)
	}
	if len(cells) != 5 {
		t.Fatalf("emitted %d cells, want 5", len(cells))
	}
	if cells[0].E != -12000 || cells[1].E != -12000 {
		t.Fatalf("first cells %+v", cells[:2])
	}
	// Cell 2 is half silence from block 1 and half full-scale from block 2.
	if cells[2].E < -303 || cells[2].E > -301 || cells[2].Flags != ema.CellMuted {
		t.Fatalf("remainder cell %+v", cells[2])
	}
	if cells[3].E != 0 || cells[4].E != 0 || cells[3].Flags != ema.CellMuted || cells[4].Flags != ema.CellMuted {
		t.Fatalf("full cells %+v", cells[3:])
	}
	for i, c := range cells {
		if c.Mask != 5 || intervals[i] != [2]uint64{uint64(i * 512), uint64((i + 1) * 512)} {
			t.Fatalf("cell %d mask/interval %+v %v", i, c, intervals[i])
		}
	}
	if (*got)[1].first != 2 || (*got)[1].ns != 65_000_000 {
		t.Fatalf("second batch %+v", (*got)[1])
	}
}

func TestGapCells(t *testing.T) {
	got, sink := collector()
	a := NewAccumulator(maskFn(func(_, _ uint64) uint8 { return ema.SourceAlert }), sink)
	if err := a.Push(0, make([]int16, 100), 0, false); err != nil {
		t.Fatal(err)
	}
	if err := a.Missing(100, 600, 100*sampleNs); err != nil {
		t.Fatal(err)
	}
	if err := a.Push(600, make([]int16, 424), 600*sampleNs, true); err != nil {
		t.Fatal(err)
	}
	var records []ema.Cell
	for _, e := range *got {
		records = append(records, e.cells...)
	}
	if len(records) != 2 {
		t.Fatalf("%d records", len(records))
	}
	if records[0] != (ema.Cell{E: -12000, Flags: ema.CellGap, Mask: ema.SourceAlert}) {
		t.Fatalf("cell 0 %+v", records[0])
	}
	if records[1] != (ema.Cell{E: -12000, Flags: ema.CellGap | ema.CellMuted, Mask: ema.SourceAlert}) {
		t.Fatalf("cell 1 %+v", records[1])
	}
	if err := a.Push(999, nil, 0, false); err != ErrNotContiguous {
		t.Fatalf("non-contiguous: %v", err)
	}
}

func TestMaskHistoryMapsThroughClockFits(t *testing.T) {
	const epochNs int64 = 7_000_000_000
	capture := clockfit.New(16000, 80_000_000)
	render := clockfit.New(48000, 42_666_667)
	capture.Add(0, epochNs)
	render.Add(0, epochNs)
	h := NewMaskHistory(capture, render)
	h.Record(0, 480, ema.SourceContent) // 0..10 ms
	h.Record(480, 480, ema.SourceAlert) // 10..20 ms
	h.Record(960, 480, ema.SourceDialog)
	if got := h.MaskFor(0, 160); got != ema.SourceContent {
		t.Fatalf("first 10 ms mask %d", got)
	}
	if got := h.MaskFor(80, 400); got != ema.SourceContent|ema.SourceAlert|ema.SourceDialog {
		t.Fatalf("5..25 ms mask %d", got)
	}
	// Adjacent equal masks coalesce without changing answers.
	h.Record(1440, 480, ema.SourceDialog)
	if got := h.MaskFor(400, 560); got != ema.SourceDialog {
		t.Fatalf("25..35 ms mask %d", got)
	}
	h.Reset()
	if got := h.MaskFor(0, 512); got != 0 {
		t.Fatalf("mask after reset %d", got)
	}
}

func TestMaskHistoryNeedsClockAnchors(t *testing.T) {
	h := NewMaskHistory(clockfit.New(16000, 80_000_000), clockfit.New(48000, 42_666_667))
	h.Record(0, 480, 1)
	if got := h.MaskFor(0, 512); got != 0 {
		t.Fatalf("unanchored mask %d", got)
	}
}
