package mixerapi

import (
	"bytes"
	"testing"
)

// seq returns n bytes starting at start, so emitted periods can be checked
// for exact byte content, not just length.
func seq(start, n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte(start + i)
	}
	return b
}

// retainAll is an emit callback that always retains (rotates the pool) and
// records every (period, monoNs) it sees, as an independent copy — tests
// comparing periods across calls must not alias the pool's own buffers.
func retainAll(got *[][]byte, stamps *[]int64) func([]byte, int64) bool {
	return func(p []byte, ns int64) bool {
		*got = append(*got, append([]byte(nil), p...))
		*stamps = append(*stamps, ns)
		return true
	}
}

func TestReframerExactMultiple(t *testing.T) {
	// byteRate=1e9 (1 byte/ns) makes the sub-block interpolation arithmetic
	// exact and easy to check by hand.
	f := newReframer(10, 4, 1e9)
	var got [][]byte
	var stamps []int64
	emit := retainAll(&got, &stamps)
	for i, ts := range []int64{100, 200} {
		f.add(seq(i*5, 5), ts, emit)
	}
	if len(got) != 1 {
		t.Fatalf("got %d periods, want 1", len(got))
	}
	if !bytes.Equal(got[0], seq(0, 10)) {
		t.Errorf("period content = %v, want %v", got[0], seq(0, 10))
	}
	// The period completes exactly at the second block's last byte: no
	// bytes of that block remain after it, so its stamp is that block's
	// own end time unchanged.
	if stamps[0] != 200 {
		t.Errorf("stamp = %d, want 200 (block ends exactly at the period boundary)", stamps[0])
	}
}

func TestReframerBlockSizeNotDividingPeriod(t *testing.T) {
	// periodBytes=10, blocks of 3: periods complete at byte offsets
	// 10, 20, 30... which fall mid-block (3 doesn't divide 10), so a
	// block must be able to complete one period and start the next, and
	// each period's stamp must be backdated from its completing block's
	// end by however many of that block's bytes come after it.
	f := newReframer(10, 4, 1e9) // 1 byte/ns: remaining bytes == ns backdated
	var got [][]byte
	var stamps []int64
	emit := retainAll(&got, &stamps)
	total := 0
	for i := range 8 { // 8*3 = 24 bytes -> two complete 10-byte periods, 4 left over
		ts := int64(1000 + 100*i) // spaced out so backdating is unambiguous
		f.add(seq(total, 3), ts, emit)
		total += 3
	}
	if len(got) != 2 {
		t.Fatalf("got %d periods, want 2", len(got))
	}
	if !bytes.Equal(got[0], seq(0, 10)) {
		t.Errorf("period 0 = %v, want %v", got[0], seq(0, 10))
	}
	if !bytes.Equal(got[1], seq(10, 10)) {
		t.Errorf("period 1 = %v, want %v", got[1], seq(10, 10))
	}
	// Period 0 (bytes 0..9) is completed by the 4th block (i=3, bytes
	// 9..11, ts=1300): 1 byte of that 3-byte block (offset 10,11) comes
	// after the period's last byte (offset 9) -> backdated by 2ns.
	if want := int64(1300 - 2); stamps[0] != want {
		t.Errorf("stamp 0 = %d, want %d", stamps[0], want)
	}
	// Period 1 (bytes 10..19) is completed by the 7th block (i=6, bytes
	// 18..20, ts=1600): 1 byte (offset 21 would be next, block covers
	// 18,19,20 - period ends at 19, so 1 byte (offset 20) remains) -> -1ns.
	if want := int64(1600 - 1); stamps[1] != want {
		t.Errorf("stamp 1 = %d, want %d", stamps[1], want)
	}
	if f.filled != 4 {
		t.Errorf("leftover filled = %d, want 4", f.filled)
	}
}

func TestReframerBlockLargerThanPeriod(t *testing.T) {
	// A single block spanning more than one period must emit each period
	// it completes, in order, each independently backdated from the
	// block's one end time by its own remaining-bytes.
	f := newReframer(10, 4, 1e9)
	var got [][]byte
	var stamps []int64
	emit := retainAll(&got, &stamps)
	f.add(seq(0, 25), 1000, emit) // 25 bytes, ts=1000 is the end of byte 24 (last)
	if len(got) != 2 {
		t.Fatalf("got %d periods, want 2", len(got))
	}
	if !bytes.Equal(got[0], seq(0, 10)) || !bytes.Equal(got[1], seq(10, 10)) {
		t.Errorf("period content wrong: %v, %v", got[0], got[1])
	}
	// Period 0 ends at byte 9; 15 bytes (offsets 10..24) remain -> -15ns.
	if want := int64(1000 - 15); stamps[0] != want {
		t.Errorf("stamp 0 = %d, want %d", stamps[0], want)
	}
	// Period 1 ends at byte 19; 5 bytes (offsets 20..24) remain -> -5ns.
	if want := int64(1000 - 5); stamps[1] != want {
		t.Errorf("stamp 1 = %d, want %d", stamps[1], want)
	}
	if f.filled != 5 {
		t.Errorf("leftover filled = %d, want 5 (25 - 2*10)", f.filled)
	}
}

func TestReframerRealByteRate(t *testing.T) {
	// 16kHz mono S16: 32000 bytes/sec. A 640-byte (20 ms) period ending 320
	// bytes before its block's end time is stamped exactly 10 ms earlier.
	f := newReframer(640, 4, 32000)
	var got [][]byte
	var stamps []int64
	emit := retainAll(&got, &stamps)
	f.add(seq(0, 640+320), 5_000_000_000, emit) // one period plus 320 trailing bytes
	if len(got) != 1 {
		t.Fatalf("got %d periods, want 1", len(got))
	}
	if want := int64(5_000_000_000 - 10_000_000); stamps[0] != want {
		t.Errorf("stamp = %d, want %d (10ms earlier)", stamps[0], want)
	}
}

func TestReframerResetDropsPartialPeriod(t *testing.T) {
	// A gap (stream re-open) must discard whatever partial period was
	// in flight, not splice pre-gap bytes onto post-gap bytes.
	f := newReframer(10, 4, 1e9)
	var got [][]byte
	var stamps []int64
	emit := retainAll(&got, &stamps)

	f.add(seq(0, 6), 100, emit) // 6 bytes in, period not yet complete
	if f.filled != 6 {
		t.Fatalf("filled = %d, want 6", f.filled)
	}
	f.reset()
	if f.filled != 0 {
		t.Fatalf("filled after reset = %d, want 0", f.filled)
	}

	f.add(seq(50, 10), 200, emit) // a fresh, unrelated 10 bytes after the gap
	if len(got) != 1 {
		t.Fatalf("got %d periods, want 1", len(got))
	}
	if !bytes.Equal(got[0], seq(50, 10)) {
		t.Errorf("period after reset = %v, want %v (no pre-gap bytes)", got[0], seq(50, 10))
	}
}

func TestReframerDroppedPeriodSlotIsReusedNotRotated(t *testing.T) {
	// When emit declines to retain (its consumer's queue is full), the
	// SAME pool buffer must be reused for the next period, not rotated —
	// otherwise a retained-but-still-unread earlier period could be
	// silently overwritten once the pool wraps around.
	f := newReframer(4, 2, 1e9) // pool of exactly 2 buffers
	var retained [][]byte

	// Period A: retained (rotates pool 0 -> 1).
	f.add(seq(0, 4), 1, func(p []byte, ns int64) bool {
		retained = append(retained, p) // keep the ACTUAL slice, not a copy
		return true
	})
	// Period B: dropped (does not rotate; buffer 1 is reused next, not
	// buffer 0 — if it rotated to buffer 0, period A's still-referenced
	// bytes would be clobbered by period C below).
	f.add(seq(10, 4), 2, func(p []byte, ns int64) bool { return false })
	// Period C: retained. If the dropped period B had wrongly rotated the
	// pool back to buffer 0, this would alias and corrupt period A's slice.
	f.add(seq(20, 4), 3, func(p []byte, ns int64) bool {
		retained = append(retained, p)
		return true
	})

	if len(retained) != 2 {
		t.Fatalf("got %d retained periods, want 2", len(retained))
	}
	if !bytes.Equal(retained[0], seq(0, 4)) {
		t.Errorf("period A was corrupted: got %v, want %v", retained[0], seq(0, 4))
	}
	if !bytes.Equal(retained[1], seq(20, 4)) {
		t.Errorf("period C = %v, want %v", retained[1], seq(20, 4))
	}
}

func TestReframerPoolCyclesIndependentBuffersWithinWindow(t *testing.T) {
	// Within one pool-size's worth of retained periods, every returned
	// slice must be backed by a distinct array (no aliasing) — this is
	// what lets the pump hand a period straight to a channel without an
	// extra per-period allocation.
	const poolSize = 4
	f := newReframer(4, poolSize, 1e9)
	var slices [][]byte
	for i := range poolSize {
		f.add(seq(i*4, 4), int64(i), func(p []byte, ns int64) bool {
			slices = append(slices, p)
			return true
		})
	}
	for i := range slices {
		for j := i + 1; j < len(slices); j++ {
			if &slices[i][0] == &slices[j][0] {
				t.Errorf("slices %d and %d alias the same backing array within one pool cycle", i, j)
			}
		}
	}
}
