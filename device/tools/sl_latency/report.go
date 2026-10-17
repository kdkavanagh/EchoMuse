//go:build server

package main

import (
	"bufio"
	"encoding/binary"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
	"github.com/wilbowes/EchoMuse/internal/audio/clockfit"
	"github.com/wilbowes/EchoMuse/internal/render"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
	"github.com/wilbowes/EchoMuse/tools/sl_latency/latency"
)

// renderClock keeps the sink's completion stamps, one per write in write
// order, and fits them as the supervisor's render clock does (onAnchor:
// render frame (w+1) × SinkFrames completed at the stamp).
type renderClock struct {
	mu   sync.Mutex
	done []int64
	fit  *clockfit.Fit
}

func newRenderClock(writes int) *renderClock {
	return &renderClock{
		done: make([]int64, 0, writes),
		fit:  clockfit.New(render.SampleRate, latency.WriteNs),
	}
}

// complete runs on the sink's completion path (the OpenSL callback thread,
// or the AAudio writer).
func (r *renderClock) complete(ns int64) {
	r.mu.Lock()
	r.done = append(r.done, ns)
	frame := uint64(len(r.done)) * render.SinkFrames
	if r.fit.Add(frame, ns).IsReset() {
		r.fit.Add(frame, ns)
	}
	r.mu.Unlock()
}

func (r *renderClock) fitAt(frame uint64) (int64, bool) { return r.fit.SampleToNs(frame) }

// wait waits up to d for n completions.
func (r *renderClock) wait(n int, d time.Duration) {
	for deadline := time.Now().Add(d); time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
		r.mu.Lock()
		got := len(r.done)
		r.mu.Unlock()
		if got >= n {
			return
		}
	}
}

func (r *renderClock) completions() []int64 {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]int64(nil), r.done...)
}

// period is one delivered capture period's timing and AFE summary.
type period struct {
	first    int   // index of its first sample in the capture
	stampNs  int64 // Block.MonoNs
	doneNs   int64 // its read completion
	anchored bool  // MonoNs came from its anchor (AFE frame or AAudio fit)
	afe      afe.Period
}

// microphone is the firmware's capture backend under test: OpenSL
// (slmic.OpenSLMicrophone) or AAudio (slmic.AAudioMicrophone).
type microphone interface {
	pkgmic.Microphone
	Completion() (doneNs int64, anchored bool)
}

// capturer reads the microphone as the supervisor's capture loop does.
type capturer struct {
	mic     microphone
	first   chan struct{} // closed at the first period
	capture latency.Capture
	periods []period
}

func (c *capturer) run() {
	for {
		b, err := c.mic.Read()
		if err != nil {
			return
		}
		doneNs, anchored := c.mic.Completion()
		p := period{first: len(c.capture.PCM), stampNs: b.MonoNs, doneNs: doneNs, anchored: anchored}
		if b.AFE != nil {
			p.afe = *b.AFE
		}
		c.capture.Add(b.PCM, b.MonoNs)
		c.periods = append(c.periods, p)
		if len(c.periods) == 1 {
			close(c.first)
		}
	}
}

type burstRec struct {
	write   int
	stalled bool
	fitNs   int64 // the render clock fit's stamp for its first frame when it was written
	fitOK   bool
}

// result is one burst's measurement.
type result struct {
	burstRec
	refNs          int64 // render stamp of its first frame; 0 without a completion
	pa, erle, pcm  latency.Onset
	hasCompletions bool
}

type report struct {
	results []result
}

func analyse(o options, c *capturer, clock *renderClock, bursts []burstRec) report {
	done := clock.completions()
	spec := latency.BurstSpec{Wave: o.wave, FreqHz: o.freq,
		Samples: int(int64(o.burst) * latency.CaptureRate / int64(time.Second))}
	var r report
	for _, b := range bursts {
		res := result{burstRec: b}
		if b.write < len(done) {
			res.hasCompletions = true
			res.refNs = done[b.write] - latency.WriteNs // the burst starts the write
			res.pa = c.capture.FirstRise(res.refNs, func(f latency.Frame) bool { return f.Playback })
			res.erle = c.capture.FirstRise(res.refNs, func(f latency.Frame) bool { return f.ERLE > 0 })
			res.pcm = c.capture.DetectPCM(res.refNs, spec, o.minSNR)
		}
		r.results = append(r.results, res)
	}
	return r
}

func lagMs(o latency.Onset, refNs int64) (float64, bool) {
	if !o.Found {
		return 0, false
	}
	return float64(o.Ns-refNs) / 1e6, true
}

func cell(v float64, ok bool) string {
	if !ok {
		return "       -"
	}
	return fmt.Sprintf("%+8.1f", v)
}

func summary(w io.Writer, name string, xs []float64) {
	if len(xs) == 0 {
		fmt.Fprintf(w, "%-22s n=0\n", name)
		return
	}
	q := latency.Quantiles(xs, 0, 0.5, 1)
	fmt.Fprintf(w, "%-22s n=%d min=%+.1f median=%+.1f max=%+.1f\n", name, len(xs), q[0], q[1], q[2])
}

func (r report) print(w io.Writer, c *capturer, clock *renderClock, drops uint64, written int) {
	t0 := c.capture.SampleNs(0) // run() returns before analysis when no period arrived
	fmt.Fprintf(w, "%5s  %5s  %5s  %9s  %8s %8s  %8s %8s  %s\n",
		"burst", "write", "stall", "ref_ms", "fit-ref", "pa_lag", "erle_lag", "pcm_lag", "pcm_snr")
	var pa, erle, pcm, pcmPA []float64
	negPCM, negPA := 0, 0
	for i, res := range r.results {
		stall := "    -"
		if res.stalled {
			stall = "  yes"
		}
		if !res.hasCompletions {
			fmt.Fprintf(w, "%5d  %5d  %s   (no completion for its write)\n", i+1, res.write, stall)
			continue
		}
		fitErr, fitOK := 0.0, res.fitOK
		if fitOK {
			fitErr = float64(res.fitNs-res.refNs) / 1e6
		}
		pl, pok := lagMs(res.pa, res.refNs)
		el, eok := lagMs(res.erle, res.refNs)
		cl, cok := lagMs(res.pcm, res.refNs)
		snr := "-"
		if res.pcm.SNRdB != 0 { // zero: the capture did not cover the search window
			snr = fmt.Sprintf("%.1f dB", res.pcm.SNRdB)
		}
		fmt.Fprintf(w, "%5d  %5d  %s  %9.1f  %s %s  %s %s  %s\n", i+1, res.write, stall,
			float64(res.refNs-t0)/1e6, cell(fitErr, fitOK), cell(pl, pok), cell(el, eok), cell(cl, cok), snr)
		if pok {
			pa = append(pa, pl)
			if pl < 0 {
				negPA++
			}
		}
		if eok {
			erle = append(erle, el)
		}
		if cok {
			pcm = append(pcm, cl)
			if cl < 0 {
				negPCM++
			}
			if pok {
				pcmPA = append(pcmPA, cl-pl)
			}
		}
	}
	fmt.Fprintln(w, "lag = capture onset stamp − render stamp of the burst's first frame, ms; ref_ms from the first capture sample")
	summary(w, "pa_lag", pa)
	summary(w, "erle_lag", erle)
	summary(w, "pcm_lag", pcm)
	summary(w, "pcm − pa (echo after PA)", pcmPA)
	if negPCM > 0 {
		fmt.Fprintf(w, "WARNING: %d pcm lag(s) negative: the echo is stamped before its reference, so render stamps are late or capture stamps early\n", negPCM)
	}
	if negPA > 0 {
		fmt.Fprintf(w, "note: %d pa lag(s) negative; PA leads the echo in the output, so only pcm_lag must stay ≥ 0\n", negPA)
	}

	var periods, frames, invalid, syncs, gaps, lost, anchored, held int
	var offsets, stampLags []float64
	for _, p := range c.periods {
		periods++
		frames += int(p.afe.Record.Frames)
		invalid += int(p.afe.Invalid)
		syncs += int(p.afe.Syncs)
		gaps += int(p.afe.Gaps)
		lost += int(p.afe.Lost)
		if p.anchored {
			anchored++
			stampLags = append(stampLags, float64(p.doneNs-p.stampNs)/1e6)
			if p.stampNs == p.doneNs {
				held++
			}
		}
		if off, ok := latency.CompletionOffsetMs(p.doneNs, p.afe.Anchor); ok {
			offsets = append(offsets, off)
		}
	}
	fmt.Fprintf(w, "[afe] periods=%d frames=%d invalid=%d syncs=%d gaps=%d lost=%d; stamps anchored=%d at-completion=%d; mic drops=%d\n",
		periods, frames, invalid, syncs, gaps, lost, anchored, periods-anchored, drops)
	if len(offsets) > 0 {
		q := latency.Quantiles(offsets, 0, 0.1, 0.5, 0.9, 1)
		fmt.Fprintf(w, "[afe] completion − AFE_TIMESTAMP, ms: n=%d min=%.1f p10=%.1f median=%.1f p90=%.1f max=%.1f\n",
			len(offsets), q[0], q[1], q[2], q[3], q[4])
	}
	if len(stampLags) > 0 {
		q := latency.Quantiles(stampLags, 0, 0.1, 0.5, 0.9, 1)
		fmt.Fprintf(w, "[stamp] completion − anchored stamp, ms: n=%d min=%.1f p10=%.1f median=%.1f p90=%.1f max=%.1f; held at completion %d\n",
			len(stampLags), q[0], q[1], q[2], q[3], q[4], held)
	}
	fmt.Fprintf(w, "sink: writes=%d completions=%d\n", written, len(clock.completions()))
}

// periodLen is the length of period i.
func (c *capturer) periodLen(i int) int {
	if i+1 < len(c.periods) {
		return c.periods[i+1].first - c.periods[i].first
	}
	return len(c.capture.PCM) - c.periods[i].first
}

// dump writes the capture and the stamps for offline analysis.
func dump(prefix string, c *capturer, clock *renderClock, plan []latency.Step) error {
	if err := os.MkdirAll(filepath.Dir(prefix), 0o755); err != nil {
		return err
	}
	if err := writeFile(prefix+".raw", func(w *bufio.Writer) error {
		return binary.Write(w, binary.LittleEndian, c.capture.PCM)
	}); err != nil {
		return err
	}
	if err := writeFile(prefix+".periods.csv", func(w *bufio.Writer) error {
		fmt.Fprintln(w, "first_sample,samples,stamp_ns,completion_ns,anchored,afe_ts_ms,afe_after,frames,playback,erle_max")
		for i, p := range c.periods {
			a := p.afe.Anchor
			ts := ""
			if a.Valid {
				ts = fmt.Sprint(a.TimestampMs)
			}
			fmt.Fprintf(w, "%d,%d,%d,%d,%t,%s,%d,%d,%d,%d\n", p.first, c.periodLen(i), p.stampNs, p.doneNs, p.anchored,
				ts, a.After, p.afe.Record.Frames, p.afe.Record.Playback, p.afe.Record.ERLEMax)
		}
		return nil
	}); err != nil {
		return err
	}
	done := clock.completions()
	return writeFile(prefix+".writes.csv", func(w *bufio.Writer) error {
		fmt.Fprintln(w, "write,burst,stall_before_ms,completion_ns")
		for i, s := range plan {
			ns := ""
			if i < len(done) {
				ns = fmt.Sprint(done[i])
			}
			fmt.Fprintf(w, "%d,%t,%d,%s\n", i, s.Burst, s.StallBefore.Milliseconds(), ns)
		}
		return nil
	})
}

func writeFile(path string, fill func(*bufio.Writer) error) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	w := bufio.NewWriter(f)
	if err := fill(w); err != nil {
		f.Close()
		return err
	}
	if err := w.Flush(); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}
