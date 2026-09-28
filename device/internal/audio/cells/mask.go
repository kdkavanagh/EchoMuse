package cells

import "sync"

// maskRuns bounds MaskHistory: runs of equal mask over consecutive render
// blocks. Even with the mask changing every 10 ms mixer block this spans
// 2.5 s, well past render queue depth plus a capture period.
const maskRuns = 256

// CaptureClock maps capture-epoch samples to monotonic ns (a capture
// clockfit.Fit).
type CaptureClock interface {
	SampleToNs(sample uint64) (ns int64, ok bool)
}

// RenderClock maps monotonic ns to render-epoch 48 kHz samples (the render
// clockfit.Fit built from buffer-completion anchors).
type RenderClock interface {
	NsToSample(ns int64) (sample int64, ok bool)
}

type maskRun struct {
	first, end uint64 // render-epoch samples
	mask       uint8
}

// MaskHistory records the final mix's active-source mask against render time
// and answers it over capture intervals through the two clock fits (§16.1
// cell byte 3, §4.3). Record runs on the mixer goroutine and MaskFor on the
// capture goroutine; both are short and allocation-free.
type MaskHistory struct {
	capture CaptureClock
	render  RenderClock

	mu    sync.Mutex
	runs  [maskRuns]maskRun // circular, oldest at head
	head  int
	count int
}

// NewMaskHistory maps through capture and render clocks owned by their
// timelines.
func NewMaskHistory(capture CaptureClock, render RenderClock) *MaskHistory {
	return &MaskHistory{capture: capture, render: render}
}

func (h *MaskHistory) at(i int) *maskRun { return &h.runs[(h.head+i)%maskRuns] }

// Reset forgets every run, for a new render epoch.
func (h *MaskHistory) Reset() {
	h.mu.Lock()
	h.head, h.count = 0, 0
	h.mu.Unlock()
}

// Record notes that render samples [first, first+n) of the current render
// epoch carried mask.
func (h *MaskHistory) Record(first uint64, n int, mask uint8) {
	if n <= 0 {
		return
	}
	end := first + uint64(n)
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.count > 0 {
		if last := h.at(h.count - 1); last.end == first && last.mask == mask {
			last.end = end
			return
		}
	}
	if h.count == maskRuns {
		h.head = (h.head + 1) % maskRuns
		h.count--
	}
	*h.at(h.count) = maskRun{first: first, end: end, mask: mask}
	h.count++
}

// MaskFor is the union of the masks rendered during capture samples
// [cellStart, cellEnd). It is zero when nothing recorded overlaps the
// interval or either clock has no anchor yet (nothing has been rendered or
// captured in the current epochs).
func (h *MaskHistory) MaskFor(cellStart, cellEnd uint64) uint8 {
	t0, ok0 := h.capture.SampleToNs(cellStart)
	t1, ok1 := h.capture.SampleToNs(cellEnd)
	if !ok0 || !ok1 {
		return 0
	}
	r0, ok0 := h.render.NsToSample(t0)
	r1, ok1 := h.render.NsToSample(t1)
	if !ok0 || !ok1 || r1 <= 0 {
		return 0
	}
	from := uint64(max(r0, 0))
	to := uint64(r1)
	if to <= from {
		to = from + 1
	}

	var mask uint8
	h.mu.Lock()
	for i := 0; i < h.count; i++ {
		r := h.at(i)
		if r.first < to && from < r.end {
			mask |= r.mask
		}
	}
	h.mu.Unlock()
	return mask
}
