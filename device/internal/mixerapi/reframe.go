package mixerapi

// reframer re-frames libmixerAPI's native capture blocks (arbitrary size —
// measured 512 bytes/16ms for micAsr, but never assumed fixed) into the
// fixed-size periods SPEC §4.1 requires (1,280 frames/80ms for capture).
//
// Each native block is stamped by the caller with ITS OWN end time (see
// mixerapi.go's package doc, "Capture clock domain", for why the mixer's
// raw ts is used as that end time directly). A period that ends mid-block,
// or several periods a single large block completes, each get their OWN
// precise end time: the block's end time backdated by however many of the
// block's bytes come after that period's last byte, at the stream's fixed
// byte rate — not the whole block's single ts, which SPEC §4.3's
// completion-time convention would otherwise misrepresent by up to a
// block's duration.
//
// Buffers are a fixed pool, cycled round-robin: newReframer preallocates
// every period buffer once, so the pump goroutine never allocates in
// steady state. A period only rotates the pool forward when emit retains
// it (e.g. queues it to a reader); a dropped period's slot is reused for
// the next period in place, since nothing holds a reference to dropped
// bytes — overwriting it cannot corrupt anything still reachable.
//
// No cgo, no build tag: host-testable without the ARM toolchain.
type reframer struct {
	bufs     [][]byte
	next     int
	cur      []byte
	filled   int
	byteRate int64 // bytes/second of the capture stream
}

// newReframer returns a reframer whose periods are periodBytes long, with
// a pool of poolSize period buffers — sized by the caller to its queue
// depth plus the in-flight slots (one being filled, one borrowed by the
// consumer until its next read; see mixerapi.go's NewRecorder).
// byteRate is bytes/second (bytesPerSample × sampleRate), used to
// interpolate a sub-block period's own end time.
func newReframer(periodBytes, poolSize int, byteRate int64) *reframer {
	bufs := make([][]byte, poolSize)
	for i := range bufs {
		bufs[i] = make([]byte, periodBytes)
	}
	return &reframer{bufs: bufs, cur: bufs[0], byteRate: byteRate}
}

// reset discards any partially-filled period and rewinds to the first pool
// buffer: used after a stream re-open (EHOSTDOWN, or ETIMEDOUT read as the
// stream being gone), so stale pre-gap bytes are never spliced onto
// post-gap bytes under one timestamp. Safe to rewind to buffer 0
// unconditionally — reset only runs long after any consumer still has a
// reference to what came before (the recorder's read loop has nothing new
// to deliver while the stream is down).
func (f *reframer) reset() {
	f.filled = 0
	f.next = 0
	f.cur = f.bufs[0]
}

// add appends one native block whose own last byte completed at
// blockEndNs, and calls emit once for every period it completes, each with
// that period's own end time (see the package doc above) and a buffer
// slice. emit reports whether it retained the slice; add only advances to
// a fresh pool buffer when it did (see the package doc's "Buffers" above).
func (f *reframer) add(block []byte, blockEndNs int64, emit func(period []byte, monoNs int64) (retained bool)) {
	off := 0
	for off < len(block) {
		n := copy(f.cur[f.filled:], block[off:])
		f.filled += n
		off += n
		if f.filled == len(f.cur) {
			remaining := int64(len(block) - off)
			periodEndNs := blockEndNs - remaining*1_000_000_000/f.byteRate
			if emit(f.cur, periodEndNs) {
				f.next = (f.next + 1) % len(f.bufs)
				f.cur = f.bufs[f.next]
			}
			f.filled = 0
		}
	}
}
