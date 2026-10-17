package latency

import (
	"math"
	"slices"
)

// Search bounds around a burst's render stamp, the window the Dot's
// end-to-end probe searched (docs/fireos6-port.md §6): an onset may be
// found up to SearchBeforeNs before the stamp (a late render stamp) and up
// to SearchAfterNs after it. The residual's floor is measured over the
// FloorNs before the window.
const (
	SearchBeforeNs = 300_000_000
	SearchAfterNs  = 1_500_000_000
	FloorNs        = 1_000_000_000
)

// Wave is a burst's waveform.
type Wave uint8

const (
	Tone  Wave = iota // a sine at the burst's frequency
	Noise             // white noise
)

// BurstSpec is what was played, as the capture sees it.
type BurstSpec struct {
	Wave    Wave
	FreqHz  float64 // Tone only
	Samples int     // burst length at CaptureRate
}

// Onset is where a burst was found in the capture.
type Onset struct {
	Found bool
	Ns    int64   // capture time of the onset
	SNRdB float64 // DetectPCM: the match over the floor's median
}

// DefaultMinSNRdB suits a tone: over a noise floor a window's energy at
// one frequency is exponentially distributed, so the largest of the ~90
// independent burst-length windows of a search stands about 8 dB over their
// median by chance; 12 dB leaves it about one false find in 700 searches.
// Broadband energy over a burst-length window varies far less.
const DefaultMinSNRdB = 12

// DetectPCM finds a burst in the residual with a matched filter: the
// burst-length window, within the search bounds of refNs, whose energy at
// the burst's frequency (Tone) or in total (Noise) is largest. Its start is
// the onset, found when it stands minSNRdB over the median window of the
// floor. Bit 0, the AFE metadata, is cleared first.
func (c *Capture) DetectPCM(refNs int64, b BurstSpec, minSNRdB float64) Onset {
	flo := c.IndexAt(refNs - SearchBeforeNs - FloorNs)
	lo := c.IndexAt(refNs - SearchBeforeNs)
	hi := c.IndexAt(refNs + SearchAfterNs)
	l := b.Samples
	if l <= 0 || lo-flo < l || hi-lo < l {
		return Onset{}
	}
	score := c.scores(flo, hi, b)
	floor := median(score[:lo-flo-l+1]) // windows wholly inside the floor
	k := lo - flo
	for j := k + 1; j <= hi-flo-l; j++ {
		if score[j] > score[k] {
			k = j
		}
	}
	snr := math.Inf(1)
	if floor > 0 {
		snr = 10 * math.Log10(score[k]/floor)
	}
	return Onset{Found: snr >= minSNRdB, Ns: c.SampleNs(flo + k), SNRdB: snr}
}

// scores returns, for every window of b.Samples starting at flo+j inside
// [flo, hi), its matched-filter energy.
func (c *Capture) scores(flo, hi int, b BurstSpec) []float64 {
	n := hi - flo
	l := b.Samples
	out := make([]float64, n-l+1)
	switch b.Wave {
	case Noise:
		p := make([]float64, n+1)
		for i := range n {
			x := float64(c.PCM[flo+i] &^ 1)
			p[i+1] = p[i] + x*x
		}
		for j := range out {
			out[j] = p[j+l] - p[j]
		}
	default:
		w := 2 * math.Pi * b.FreqHz / CaptureRate
		pc, ps := make([]float64, n+1), make([]float64, n+1)
		for i := range n {
			x := float64(c.PCM[flo+i] &^ 1)
			s, co := math.Sincos(w * float64(i))
			pc[i+1], ps[i+1] = pc[i]+x*co, ps[i]+x*s
		}
		for j := range out {
			re, im := pc[j+l]-pc[j], ps[j+l]-ps[j]
			out[j] = re*re + im*im
		}
	}
	return out
}

// FirstRise finds, within the search bounds of refNs, the first frame for
// which pred holds right after a frame for which it did not: a rise of
// PLAYBACK_ACTIVE or ERLE_RAW. The onset is that frame's first sample, so
// it is good to one frame (8 ms).
func (c *Capture) FirstRise(refNs int64, pred func(Frame) bool) Onset {
	lo, hi := refNs-SearchBeforeNs, refNs+SearchAfterNs
	prev, have := false, false
	for _, f := range c.Frames {
		if f.StartNs >= hi {
			break
		}
		p := pred(f)
		if f.StartNs >= lo && p && have && !prev {
			return Onset{Found: true, Ns: f.StartNs}
		}
		prev, have = p, true
	}
	return Onset{}
}

func median(xs []float64) float64 {
	s := slices.Clone(xs)
	slices.Sort(s)
	return s[len(s)/2]
}

// Quantiles returns the nearest-rank quantiles qs (0..1) of xs, which must
// not be empty.
func Quantiles(xs []float64, qs ...float64) []float64 {
	s := slices.Clone(xs)
	slices.Sort(s)
	out := make([]float64, len(qs))
	for i, q := range qs {
		out[i] = s[min(len(s)-1, max(0, int(math.Ceil(q*float64(len(s))))-1))]
	}
	return out
}
