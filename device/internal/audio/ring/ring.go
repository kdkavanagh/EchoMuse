// Package ring holds the device's fixed-capacity audio histories (SPEC §4.2):
// samples of one epoch addressed by absolute uint64 index, with per-block
// timing metadata and explicit missing ranges. A missing range is recorded,
// never filled. Storage is preallocated; appends and reads do not allocate.
package ring

import (
	"errors"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

// Capacities (§4.2, §16.1).
const (
	MicSamples       = 96000                             // 6 s at 16 kHz
	ReferenceSamples = 128000                            // 8 s at 16 kHz
	CellRecords      = 500                               // 16 s of 32 ms cells
	AFERecords       = ReferenceSamples / ema.AFESamples // 8 s of 80 ms AFE records, the reference's span

	sampleNs = 1_000_000_000 / ema.RateCapture // 62,500 ns per 16 kHz sample
	cellNs   = sampleNs * ema.CellSamples      // 32 ms per cell
	afeNs    = sampleNs * ema.AFESamples       // 80 ms per AFE record
	micBlock = 1280                            // capture callback (§4.1)
	refBlock = 160                             // one 480-sample mixer block decimated by 3
	cellBlk  = 2                               // cells completed per capture block, at least
)

var (
	// ErrNotContiguous: an append did not start at End. Missing audio must
	// be recorded with Missing first.
	ErrNotContiguous = errors.New("ring: append does not start at end")
	// ErrTooLarge: a single append exceeds the ring capacity.
	ErrTooLarge = errors.New("ring: block larger than capacity")
	// ErrNotRetained: the requested range is not (or no longer) held as
	// valid samples.
	ErrNotRetained = errors.New("ring: range not retained")
)

// Meta is the timing of a stored block. MonoNs anchors the block's first
// index; indices after it are MonoNs + offset×UnitNs.
type Meta struct {
	MonoNs        int64
	UncertaintyUs uint32
	Flags         uint8 // ema header flag bits
	Mask          uint8 // active-source mask (reference ring)
}

// Segment is one ordered piece of a read range: part of a stored block, or a
// missing range. A gap segment carries zero Meta.
type Segment struct {
	First, End uint64
	Gap        bool
	Meta
}

type record struct {
	first, end uint64
	gap        bool
	meta       Meta
}

// Ring stores the latest Capacity indices of one epoch. It is safe for one
// writer and concurrent readers.
type Ring[T any] struct {
	mu     sync.Mutex
	data   []T
	recs   []record // circular, oldest at head
	head   int
	count  int
	unitNs int64
	start  uint64 // oldest retained index
	end    uint64 // next index to append
}

// New returns a ring of capacity samples and at most maxBlocks stored blocks
// and gaps. unitNs is the duration of one index.
func New[T any](capacity, maxBlocks int, unitNs int64) *Ring[T] {
	return &Ring[T]{data: make([]T, capacity), recs: make([]record, maxBlocks), unitNs: unitNs}
}

// NewMic is the 6 s capture ring.
func NewMic() *Ring[int16] {
	return New[int16](MicSamples, 2*MicSamples/micBlock+2, sampleNs)
}

// NewReference is the 8 s final-mix reference ring.
func NewReference() *Ring[int16] {
	return New[int16](ReferenceSamples, 2*ReferenceSamples/refBlock+2, sampleNs)
}

// NewCells is the 16 s cell-record ring, indexed by cell number (cell k
// starts at capture sample 512k).
func NewCells() *Ring[ema.Cell] {
	return New[ema.Cell](CellRecords, 2*CellRecords/cellBlk+2, cellNs)
}

// NewAFE is the 8 s AFE-record ring, indexed by capture period (record k
// covers capture samples [1280k, 1280k+1280)); one record per append.
func NewAFE() *Ring[ema.AFERecord] {
	return New[ema.AFERecord](AFERecords, 2*AFERecords+2, afeNs)
}

// Capacity is the number of indices the ring retains.
func (r *Ring[T]) Capacity() int { return len(r.data) }

// Reset empties the ring for a new epoch whose first index is first.
func (r *Ring[T]) Reset(first uint64) {
	r.mu.Lock()
	r.head, r.count = 0, 0
	r.start, r.end = first, first
	r.mu.Unlock()
}

// Start is the oldest retained index, valid or missing.
func (r *Ring[T]) Start() uint64 {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.start
}

// End is the next index to be appended.
func (r *Ring[T]) End() uint64 {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.end
}

// OldestValid is the first retained index holding a sample; ok is false when
// the ring holds none.
func (r *Ring[T]) OldestValid() (idx uint64, ok bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	for i := 0; i < r.count; i++ {
		rec := r.rec(i)
		if !rec.gap {
			return max(rec.first, r.start), true
		}
	}
	return 0, false
}

// Append stores samples starting at first, which must equal End.
func (r *Ring[T]) Append(first uint64, samples []T, m Meta) error {
	if len(samples) > len(r.data) {
		return ErrTooLarge
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	if first != r.end {
		return ErrNotContiguous
	}
	if len(samples) == 0 {
		return nil
	}
	n := uint64(len(samples))
	c := uint64(len(r.data))
	pos := first % c
	k := copy(r.data[pos:], samples)
	copy(r.data, samples[k:])
	r.push(record{first: first, end: first + n, meta: m})
	return nil
}

// Missing records [End, to) as a missing range.
func (r *Ring[T]) Missing(to uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if to <= r.end {
		return
	}
	if r.count > 0 {
		if last := r.rec(r.count - 1); last.gap && last.end == r.end {
			last.end = to
			r.end = to
			r.trim()
			return
		}
	}
	r.push(record{first: r.end, end: to, gap: true})
}

func (r *Ring[T]) rec(i int) *record {
	return &r.recs[(r.head+i)%len(r.recs)]
}

func (r *Ring[T]) push(rec record) {
	if r.count == len(r.recs) {
		old := r.rec(0)
		r.start = max(r.start, old.end)
		r.head = (r.head + 1) % len(r.recs)
		r.count--
	}
	*r.rec(r.count) = rec
	r.count++
	r.end = rec.end
	r.trim()
}

// trim advances start to cover at most Capacity indices and drops records
// that end before it.
func (r *Ring[T]) trim() {
	if c := uint64(len(r.data)); r.end-r.start > c {
		r.start = r.end - c
	}
	for r.count > 0 && r.rec(0).end <= r.start {
		r.head = (r.head + 1) % len(r.recs)
		r.count--
	}
}

// Read appends to dst the ordered segments covering [from, to), clipped to
// [Start, End), and returns the clipped start. Segments of one stored block
// never merge with another block, so each keeps its own Meta; a segment
// starting inside a block has its MonoNs advanced to its first index.
func (r *Ring[T]) Read(from, to uint64, dst []Segment) (clippedFrom uint64, segs []Segment) {
	r.mu.Lock()
	defer r.mu.Unlock()
	from = max(from, r.start)
	to = min(to, r.end)
	segs = dst
	if from >= to {
		return from, segs
	}
	for i := r.find(from); i < r.count; i++ {
		rec := r.rec(i)
		if rec.first >= to {
			break
		}
		s := Segment{First: max(rec.first, from), End: min(rec.end, to), Gap: rec.gap}
		if !rec.gap {
			s.Meta = rec.meta
			s.MonoNs += int64(s.First-rec.first) * r.unitNs
		}
		segs = append(segs, s)
	}
	return from, segs
}

// find returns the position of the first record whose end is after idx.
func (r *Ring[T]) find(idx uint64) int {
	lo, hi := 0, r.count
	for lo < hi {
		mid := (lo + hi) / 2
		if r.rec(mid).end <= idx {
			lo = mid + 1
		} else {
			hi = mid
		}
	}
	return lo
}

// Copy fills dst with the samples [first, first+len(dst)), which must all be
// retained and valid.
func (r *Ring[T]) Copy(dst []T, first uint64) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	last := first + uint64(len(dst))
	if first < r.start || last > r.end {
		return ErrNotRetained
	}
	for i := r.find(first); i < r.count; i++ {
		rec := r.rec(i)
		if rec.first >= last {
			break
		}
		if rec.gap {
			return ErrNotRetained
		}
	}
	c := uint64(len(r.data))
	pos := first % c
	k := copy(dst, r.data[pos:])
	copy(dst[k:], r.data)
	return nil
}
