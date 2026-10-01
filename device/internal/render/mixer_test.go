package render

import (
	"errors"
	"math"
	"testing"
)

const stepNs = 10_000_000 // one 480-frame block of render time

// fakeSink stands in for OpenSL. With auto set it behaves like a 4-buffer
// hardware queue: a write beyond four outstanding completes the oldest.
type fakeSink struct {
	now        *int64
	onComplete func(int64)
	auto       bool
	pending    int
	writes     int
	fail       error
	restarts   int
}

func (s *fakeSink) SetOnComplete(fn func(int64)) { s.onComplete = fn }
func (s *fakeSink) Restart() error               { s.restarts++; s.fail = nil; s.pending = 0; return nil }
func (s *fakeSink) Close() error                 { return nil }

func (s *fakeSink) Write(pcm []int16) error {
	if s.fail != nil {
		return s.fail
	}
	if len(pcm) != SinkFrames {
		return errors.New("fakeSink: wrong write size")
	}
	if s.auto && s.pending == 4 {
		s.completeOne()
	}
	s.pending++
	s.writes++
	return nil
}

func (s *fakeSink) completeOne() {
	s.pending--
	s.onComplete(*s.now)
}

func (s *fakeSink) completeAll() {
	for s.pending > 0 {
		s.completeOne()
	}
}

type constPull struct {
	value  int16
	active bool
}

func (p *constPull) Fill(dst []int16) bool {
	for i := range dst {
		dst[i] = p.value
	}
	return p.active
}

type rig struct {
	t        *testing.T
	now      int64
	sink     *fakeSink
	m        *Mixer
	progress []Progress
	progAt   []int64
	finished []Finished
	epochs   []uint64
	out      []int16
	firsts   []uint64
	masks    []uint8
}

func newRig(t *testing.T, alert PullSource, auto bool) *rig {
	r := &rig{t: t, now: 1_000_000_000}
	r.sink = &fakeSink{now: &r.now, auto: auto}
	m, err := NewMixer(Config{
		Sink:  r.sink,
		Alert: alert,
		Now:   func() int64 { return r.now },
		Hooks: Hooks{
			Progress: func(p Progress) { r.progress = append(r.progress, p); r.progAt = append(r.progAt, r.now) },
			Finished: func(f Finished) { r.finished = append(r.finished, f) },
			Epoch:    func(e uint64) { r.epochs = append(r.epochs, e) },
			Tap: func(b MixBlock) {
				r.out = append(r.out, b.PCM...)
				r.firsts = append(r.firsts, b.First)
				r.masks = append(r.masks, b.Mask)
			},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	r.m = m
	return r
}

func (r *rig) step(n int) {
	r.t.Helper()
	for range n {
		r.now += stepNs
		if err := r.m.step(); err != nil {
			r.t.Fatalf("step: %v", err)
		}
	}
}

func (r *rig) must(err error) {
	r.t.Helper()
	if err != nil {
		r.t.Fatal(err)
	}
}

func (r *rig) pumpConst(epoch uint64, gen uint32, first uint64, n int, v int16) uint64 {
	r.t.Helper()
	buf := make([]int16, 2048)
	for i := range buf {
		buf[i] = v
	}
	for n > 0 {
		c := min(n, len(buf))
		r.must(r.m.Pump(epoch, gen, first, buf[:c]))
		first += uint64(c)
		n -= c
	}
	return first
}

func (r *rig) events(ev ProgressEvent) []Progress {
	var out []Progress
	for _, p := range r.progress {
		if p.Event == ev {
			out = append(out, p)
		}
	}
	return out
}

func TestNetworkSourceWaitsForPrimeUnlessEnded(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "music", Generation: 1, Class: Content, Epoch: 7}))
	next := r.pumpConst(7, 1, 0, primeFrames-1, 1000)
	r.step(10)
	for i, mask := range r.masks {
		if mask != 0 || r.out[i*BlockFrames] != 0 {
			t.Fatalf("block %d played before the %d-frame prime", i, primeFrames)
		}
	}
	if len(r.events(EventStart)) != 0 {
		t.Fatal("start reported before prime")
	}
	r.pumpConst(7, 1, next, 1, 1000)
	r.step(1)
	if r.masks[10] != MaskContent || r.out[10*BlockFrames] != 1000 {
		t.Fatalf("primed content did not start: mask %d sample %d", r.masks[10], r.out[10*BlockFrames])
	}
	if len(r.events(EventStart)) != 1 {
		t.Fatal("no start event at prime")
	}

	r.must(r.m.Start(Playback{ID: "tts", Generation: 1, Class: DialogOutput, Epoch: 8}))
	r.pumpConst(8, 1, 0, 100, 500)
	r.must(r.m.End("tts", 1, 0))
	r.step(1)
	if r.masks[11]&MaskDialog == 0 {
		t.Fatal("an ended clip shorter than the prime did not play")
	}
}

// render.end travels on the control socket and can overtake the tail of the
// audio on the audio socket: a 1.8 s answer used to play only what had
// arrived (about the 1 s prime) and drop the rest as audio after the end.
func TestRenderEndBeforeTheLastAudioStillPlaysEveryFrameSent(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "tts", Generation: 1, Class: DialogOutput, Epoch: 8}))
	const total = 86_400 // 1.8 s
	arrived := r.pumpConst(8, 1, 0, primeFrames/2, 700)
	r.must(r.m.End("tts", 1, total))
	r.step(20)
	if len(r.events(EventStart)) != 0 {
		t.Fatal("started on a partial clip whose tail is still in flight")
	}
	r.pumpConst(8, 1, arrived, total-int(arrived), 700)
	if err := r.m.Pump(8, 1, total, []int16{1}); !errors.Is(err, ErrEnded) {
		t.Fatalf("audio past the end frame = %v, want ErrEnded", err)
	}
	for i := 0; i < 400 && len(r.finished) == 0; i++ {
		r.step(1)
		r.sink.completeAll()
	}
	if len(r.finished) != 1 || r.finished[0].Reason != Drained {
		t.Fatalf("finished = %+v, want one drained", r.finished)
	}
	played := 0
	for _, v := range r.out {
		if v == 700 {
			played++
		}
	}
	if played != total {
		t.Fatalf("played %d frames, want all %d sent", played, total)
	}
}

func TestFIFOHoldsExactly128Writes(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "music", Generation: 1, Class: Content, Epoch: 7}))
	next := r.pumpConst(7, 1, 0, NetworkWrites*SinkFrames, 1)
	if err := r.m.Pump(7, 1, next, []int16{1}); !errors.Is(err, ErrFIFOFull) {
		t.Fatalf("overflow = %v, want ErrFIFOFull", err)
	}
	if len(r.finished) != 1 || r.finished[0].Reason != Failed {
		t.Fatalf("finished = %+v, want failed", r.finished)
	}
	if err := r.m.Pump(7, 1, next, []int16{1}); !errors.Is(err, ErrStale) {
		t.Fatalf("late packet for a finished playback = %v, want ErrStale", err)
	}
	if err := r.m.Pump(99, 1, 0, []int16{1}); !errors.Is(err, ErrUnknownEpoch) {
		t.Fatalf("unknown epoch = %v", err)
	}
}

func TestDrainedOnlyAfterCompletionPlusGuard(t *testing.T) {
	r := newRig(t, nil, false)
	pcm := make([]int16, 1000)
	r.must(r.m.Start(Playback{ID: "chime", Generation: 1, Class: Earcon, PCM: pcm}))
	r.step(9) // two writes; the chime is entirely in the first
	if len(r.finished) != 0 {
		t.Fatal("drained before any buffer completed")
	}
	r.sink.completeOne()
	done := r.now
	r.step(14)
	if len(r.finished) != 0 {
		t.Fatalf("drained %d ms after completion", (r.now-done)/1e6)
	}
	r.step(1)
	if r.now-done != DrainGuardNs {
		t.Fatalf("test clock drifted: %d", r.now-done)
	}
	if len(r.finished) != 1 || r.finished[0].Reason != Drained || r.finished[0].LastCompletedFrame != 1000 {
		t.Fatalf("finished = %+v", r.finished)
	}
}

func TestCancelReportsCompletedFrontierOnceAndFades(t *testing.T) {
	r := newRig(t, nil, false)
	pcm := make([]int16, 20000)
	for i := range pcm {
		pcm[i] = 8000
	}
	r.must(r.m.Start(Playback{ID: "chime", Generation: 3, Class: Earcon, PCM: pcm}))
	r.step(9) // writes of 2048 each: two submitted
	r.sink.completeOne()
	r.step(1)
	r.m.Cancel("chime", 3, RampNormal)
	r.m.Cancel("chime", 3, RampNormal)
	if len(r.finished) != 1 {
		t.Fatalf("finished %d times, want exactly once", len(r.finished))
	}
	f := r.finished[0]
	if f.Reason != Cancelled || f.LastCompletedFrame != SinkFrames || f.Generation != 3 {
		t.Fatalf("finished = %+v, want cancelled at frame %d", f, SinkFrames)
	}
	if len(r.events(EventFlush)) != 1 {
		t.Fatal("cancel did not report a flush")
	}
	start := len(r.out)
	r.step(4)
	tail := r.out[start:]
	if tail[0] == 0 || tail[0] >= 8000 {
		t.Fatalf("fade did not start from the playing level: %d", tail[0])
	}
	for i := 1; i < NormalRampFrames; i++ {
		if tail[i] > tail[i-1] {
			t.Fatalf("fade rose at sample %d", i)
		}
	}
	for i, v := range tail[NormalRampFrames:] {
		if v != 0 {
			t.Fatalf("audio %d samples after the 30 ms fade", i)
		}
	}
	if r.masks[len(r.masks)-1] != 0 {
		t.Fatal("cancelled earcon still in the mask")
	}
}

func TestGenerationFencing(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "new", Generation: 5, Class: Earcon, PCM: make([]int16, 5000)}))
	if err := r.m.Start(Playback{ID: "old", Generation: 4, Class: Earcon, PCM: make([]int16, 5000)}); !errors.Is(err, ErrStale) {
		t.Fatalf("older generation start = %v", err)
	}
	r.m.Cancel("new", 4, RampNormal)
	if len(r.finished) != 0 {
		t.Fatal("cancel with the wrong generation took effect")
	}

	r.must(r.m.Start(Playback{ID: "tts", Generation: 3, Class: DialogOutput, Epoch: 11}))
	if err := r.m.Pump(11, 2, 0, []int16{1}); !errors.Is(err, ErrStale) {
		t.Fatalf("packet of an older generation = %v", err)
	}
	r.must(r.m.Pump(11, 3, 0, []int16{1, 2}))
	if err := r.m.Pump(11, 3, 5, []int16{1}); !errors.Is(err, ErrDiscontinuous) {
		t.Fatalf("gap = %v", err)
	}
	if err := r.m.End("tts", 2, 0); !errors.Is(err, ErrStale) {
		t.Fatalf("end with older generation = %v", err)
	}
	r.must(r.m.End("tts", 3, 0))
	if err := r.m.Pump(11, 3, 2, []int16{1}); !errors.Is(err, ErrEnded) {
		t.Fatalf("audio after end = %v", err)
	}

	r.m.EndSession()
	if err := r.m.Start(Playback{ID: "tts2", Generation: 1, Class: DialogOutput, Epoch: 12}); err != nil {
		t.Fatalf("a new session's generation 1 was fenced: %v", err)
	}
}

// A device-local earcon plays at the slot's fence without raising it: the
// controller's next earcon at that generation still replaces it, and the
// local playback's events are marked Local.
func TestLocalStartNeitherRaisesNorIsBlockedByTheFence(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "ctl1", Generation: 5, Class: Earcon, PCM: make([]int16, 5000)}))
	r.must(r.m.StartLocal("local", Earcon, make([]int16, 5000), 0))
	r.step(1)
	if len(r.finished) != 1 || r.finished[0].PlaybackID != "ctl1" || r.finished[0].Local {
		t.Fatalf("local start did not replace the controller earcon: %+v", r.finished)
	}
	if got := r.masks[len(r.masks)-1]; got != MaskEarcon {
		t.Fatalf("local earcon not in the mix: mask %d", got)
	}
	if starts := r.events(EventStart); len(starts) != 1 || starts[0].PlaybackID != "local" || !starts[0].Local {
		t.Fatalf("local start events %+v", starts)
	}

	r.must(r.m.Start(Playback{ID: "ctl2", Generation: 5, Class: Earcon, PCM: make([]int16, 5000)}))
	if len(r.finished) != 2 || r.finished[1].PlaybackID != "local" || !r.finished[1].Local {
		t.Fatalf("controller earcon at the fence did not replace the local one: %+v", r.finished)
	}
	if err := r.m.Start(Playback{ID: "old", Generation: 4, Class: Earcon, PCM: make([]int16, 5000)}); !errors.Is(err, ErrStale) {
		t.Fatalf("fence lowered: older generation start = %v", err)
	}
	if err := r.m.StartLocal("net", Content, nil, 0); !errors.Is(err, ErrInvalidClass) {
		t.Fatalf("local network start = %v", err)
	}
}

// startContent plays constant content at unity and returns the tap index of
// its first sample.
func startContent(r *rig, value int16, frames int) int {
	r.must(r.m.Start(Playback{ID: "music", Generation: 1, Class: Content, Epoch: 7}))
	r.pumpConst(7, 1, 0, frames, value)
	r.step(1)
	return len(r.out) - BlockFrames
}

func TestRampLengthsAreExactInSamples(t *testing.T) {
	r := newRig(t, nil, true)
	startContent(r, 10000, 100_000)
	duck := int16(10000 * math.Pow(10, -18.0/20))

	for _, tc := range []struct {
		pol    Policy
		frames int
		target int16
	}{
		{Policy{ContentDuckDB: -18}, NormalRampFrames, duck},
		{Policy{Ramp: RampPhysicalStop}, StopRampFrames, 10000},
	} {
		from := len(r.out)
		r.m.SetPolicy(tc.pol)
		r.step(4)
		got := r.out[from:]
		if got[tc.frames-2] == tc.target {
			t.Fatalf("ramp to %d reached its target before %d samples", tc.target, tc.frames)
		}
		for i := tc.frames - 1; i < len(got); i++ {
			if got[i] != tc.target {
				t.Fatalf("sample %d = %d, want %d once the %d-sample ramp ends", i, got[i], tc.target, tc.frames)
			}
		}
	}
	gains := r.events(EventGain)
	if len(gains) != 2 || gains[0].GainDB != -18 || gains[1].GainDB != 0 {
		t.Fatalf("gain events = %+v", gains)
	}
}

func TestPauseHoldsContentWithoutLosingSamples(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "music", Generation: 1, Class: Content, Epoch: 7}))
	seq := make([]int16, 120_000)
	for i := range seq {
		seq[i] = int16(i%30000 + 1)
	}
	for i := 0; i < len(seq); i += 2048 {
		r.must(r.m.Pump(7, 1, uint64(i), seq[i:min(i+2048, len(seq))]))
	}
	r.step(3)
	last := r.out[len(r.out)-1] // unity: output equals the source
	r.m.SetPolicy(Policy{PauseContent: true})
	r.step(10)
	held := r.masks[len(r.masks)-5:]
	for _, mask := range held {
		if mask != 0 {
			t.Fatal("paused content still audible")
		}
	}
	r.m.SetPolicy(Policy{})
	from := len(r.out)
	r.step(5)
	resumed := r.out[from+NormalRampFrames]
	// Ramp-down and ramp-up each consume exactly one 30 ms ramp of source.
	if want := last + 2*NormalRampFrames + 1; resumed != want {
		t.Fatalf("first unity sample after resume = %d, want %d: samples were lost or repeated", resumed, want)
	}
	if len(r.events(EventPause)) != 1 || len(r.events(EventResume)) != 1 {
		t.Fatal("pause/resume events missing")
	}
}

func TestTapMaskAndContiguousIndices(t *testing.T) {
	alert := &constPull{value: 10, active: true}
	r := newRig(t, alert, true)
	startContent(r, 100, 100_000)
	r.must(r.m.Start(Playback{ID: "tts", Generation: 1, Class: DialogOutput, Epoch: 8}))
	r.pumpConst(8, 1, 0, 5000, 100)
	r.must(r.m.End("tts", 1, 0))
	r.must(r.m.Start(Playback{ID: "chime", Generation: 1, Class: Earcon, PCM: make([]int16, 5000)}))
	r.step(1)
	if got := r.masks[len(r.masks)-1]; got != MaskContent|MaskAlert|MaskDialog|MaskEarcon {
		t.Fatalf("mask = %04b, want all four sources", got)
	}
	if got := r.out[len(r.out)-1]; got != 210 {
		t.Fatalf("sum = %d, want 210", got)
	}
	alert.active = false
	r.m.SetPolicy(Policy{PauseContent: true})
	r.step(5)
	if got := r.masks[len(r.masks)-1]; got != MaskDialog|MaskEarcon {
		t.Fatalf("mask = %04b with content paused and alert silent", got)
	}
	for i, f := range r.firsts {
		if f != uint64(i*BlockFrames) {
			t.Fatalf("block %d First = %d", i, f)
		}
	}
}

func TestSaturatingSum(t *testing.T) {
	r := newRig(t, &constPull{value: 30000, active: true}, true)
	pcm := make([]int16, 480)
	for i := range pcm {
		pcm[i] = 30000
	}
	r.must(r.m.Start(Playback{ID: "chime", Generation: 1, Class: Earcon, PCM: pcm}))
	r.step(1)
	if r.out[0] != 32767 {
		t.Fatalf("sum = %d, want saturation at 32767", r.out[0])
	}
}

func TestProgressEvery80ms(t *testing.T) {
	r := newRig(t, nil, true)
	r.must(r.m.Start(Playback{ID: "chime", Generation: 1, Class: Earcon, PCM: make([]int16, 2*SampleRate)}))
	r.step(100)
	var at []int64
	for i, p := range r.progress {
		if p.Event == EventProgress {
			at = append(at, r.progAt[i])
			if p.SubmittedFrames < p.CompletedFrames || p.TimingQuality != TimingEstimated {
				t.Fatalf("bad progress %+v", p)
			}
		}
	}
	if len(at) < 11 {
		t.Fatalf("%d progress events in 1 s", len(at))
	}
	for i := 1; i < len(at); i++ {
		if at[i]-at[i-1] != ProgressIntervalNs {
			t.Fatalf("progress interval %d ms", (at[i]-at[i-1])/1e6)
		}
	}
	last := r.progress[len(r.progress)-1]
	if last.CompletedFrames == 0 || last.SubmittedFrames <= last.CompletedFrames {
		t.Fatalf("frame accounting %+v", last)
	}
}

func TestUnderrunReportsMissingRangeAndEndsAfterTwoSeconds(t *testing.T) {
	r := newRig(t, nil, true)
	begin := startContent(r, 100, primeFrames)
	r.step(primeFrames/BlockFrames + 5) // the prime plays out, then starves
	starveAt := uint64(begin + primeFrames)
	next := r.pumpConst(7, 1, primeFrames, primeFrames-1, 100)
	r.step(3)
	if len(r.events(EventUnderrun)) != 0 {
		t.Fatal("resumed before re-priming")
	}
	r.pumpConst(7, 1, next, 1, 100)
	r.step(1)
	resume := r.firsts[len(r.firsts)-1]
	u := r.events(EventUnderrun)
	if len(u) != 1 || u[0].MissingFrom != starveAt || u[0].MissingTo != resume {
		t.Fatalf("underrun = %+v, want [%d,%d)", u, starveAt, resume)
	}
	r.step(primeFrames/BlockFrames + StarvedLimitFrames/BlockFrames + 2)
	if len(r.finished) != 1 || r.finished[0].Reason != Underrun {
		t.Fatalf("finished = %+v, want underrun after 2 s of starvation", r.finished)
	}
}

func TestSinkFailureRestartsEpoch(t *testing.T) {
	r := newRig(t, nil, true)
	old := r.m.Epoch()
	r.must(r.m.Start(Playback{ID: "chime", Generation: 1, Class: Earcon, PCM: make([]int16, 50_000)}))
	r.sink.fail = errors.New("hal died")
	var err error
	for err == nil {
		r.now += stepNs
		err = r.m.step()
	}
	r.must(r.m.restart())
	if len(r.finished) != 1 || r.finished[0].Reason != Failed {
		t.Fatalf("finished = %+v", r.finished)
	}
	if len(r.epochs) != 1 || r.epochs[0] == old || r.epochs[0] == 0 || r.m.Epoch() != r.epochs[0] {
		t.Fatalf("epochs = %v (old %d)", r.epochs, old)
	}
	r.firsts = nil
	r.step(1)
	if r.firsts[0] != 0 {
		t.Fatalf("new epoch starts at frame %d", r.firsts[0])
	}
}

func TestMixerStepDoesNotAllocate(t *testing.T) {
	var now int64
	sink := &fakeSink{now: &now, auto: true}
	m, err := NewMixer(Config{
		Sink:  sink,
		Alert: &constPull{value: 1, active: true},
		Now:   func() int64 { return now },
		Hooks: Hooks{
			Progress: func(Progress) {},
			Finished: func(Finished) {},
			Tap:      func(MixBlock) {},
			Anchor:   func(Anchor) {},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := m.Start(Playback{ID: "music", Generation: 1, Class: Content, Epoch: 1}); err != nil {
		t.Fatal(err)
	}
	buf := make([]int16, 2048)
	for i := 0; i < 100; i++ {
		if err := m.Pump(1, 1, uint64(i*2048), buf); err != nil {
			t.Fatal(err)
		}
	}
	if err := m.Start(Playback{ID: "chime", Generation: 1, Class: Earcon, PCM: make([]int16, 100_000)}); err != nil {
		t.Fatal(err)
	}
	m.SetPolicy(Policy{ContentDuckDB: -18})
	allocs := testing.AllocsPerRun(300, func() {
		now += stepNs
		if err := m.step(); err != nil {
			t.Fatal(err)
		}
	})
	if allocs != 0 {
		t.Fatalf("%v allocations per block", allocs)
	}
}
