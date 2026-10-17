//go:build server

// Command sl_latency measures, on the Echo Show, how the firmware's render
// stamps line up with its capture stamps: the lag the controller's echo
// comparison searches (0–500 ms of mic after reference, em_attribution.
// MAX_LAG). It opens audio exactly as the firmware does (slmic.OpenOpenSL,
// slspeaker.NewOpenSL), writes render.SinkFrames buffers of silence with
// short bursts at the start of some of them, and finds each burst in the
// capture.
//
// For each burst, the reference onset is the render stamp of its first
// frame as the render accounting computes it: the completion stamp of the
// write that starts with it, less one write. It is set against three
// capture onsets, each on the firmware's capture stamps:
//   - pa: the AFE frame where PLAYBACK_ACTIVE rose. The AFE computes it on
//     its input timeline, so it leads the echo in the output it rides on
//     (51.8 ms on the Dot, docs/afe-metadata.md §1.3); good to 8 ms.
//   - erle: the frame where ERLE_RAW rose.
//   - pcm: the burst in the AEC residual itself, by matched filter: the lag
//     the controller sees. VOICE_RECOGNITION cancels the echo (tones by
//     37–49 dB, docs/echo-show-crown.md §10.1), so it is found only when the
//     residual stands -min-snr over the room; raise -index, or use -wave
//     noise, which is cancelled less.
//
// A negative pcm lag means the capture shows the echo before its render
// stamp: render stamps are late (opensl.playPipelineNs too large) or
// capture stamps early. The firmware must never do that.
//
// Usage (adb root shell on the Show; stop echomuse first, it holds the
// input and this tool would get no audio):
//
//	sl_latency [-n 10] [-gap 2.5s] [-burst 20ms] [-wave tone|noise]
//	           [-freq 1000] [-level -6] [-lead 3s] [-stall 2.5s] [-tail 2s]
//	           [-min-snr 12] [-index -1] [-o /data/local/tmp/echomuse/lat]
//
// -index sets STREAM_MUSIC for the run and restores it afterwards. -o writes
// <prefix>.raw (the capture, 16 kHz mono S16LE, as delivered, bit 0
// included), <prefix>.periods.csv and <prefix>.writes.csv.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/wilbowes/EchoMuse/internal/androidvol"
	"github.com/wilbowes/EchoMuse/internal/bindings/slmic"
	"github.com/wilbowes/EchoMuse/internal/bindings/slspeaker"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/tools/sl_latency/latency"
)

// firstCaptureWait bounds the wait for the first capture period: the
// microphone replaces a silent fresh recorder every 2 s, so two attempts.
const firstCaptureWait = 5 * time.Second

type options struct {
	n                       int
	gap, burst, lead, stall time.Duration
	tail                    time.Duration
	wave                    latency.Wave
	freq, level, minSNR     float64
	index                   int
	out                     string
}

func parseFlags() options {
	var o options
	var wave string
	flag.IntVar(&o.n, "n", 10, "bursts")
	flag.DurationVar(&o.gap, "gap", 2500*time.Millisecond, "time between burst starts (PLAYBACK_ACTIVE's hangover is up to 1 s)")
	flag.DurationVar(&o.burst, "burst", 20*time.Millisecond, "burst length")
	flag.StringVar(&wave, "wave", "tone", "burst waveform: tone or noise")
	flag.Float64Var(&o.freq, "freq", 1000, "tone frequency, Hz")
	flag.Float64Var(&o.level, "level", -6, "burst peak level, dBFS")
	flag.DurationVar(&o.lead, "lead", 3*time.Second, "silence before the first burst")
	flag.DurationVar(&o.stall, "stall", 2500*time.Millisecond, "writer stall before one extra burst, so the queue runs dry (0: none)")
	flag.DurationVar(&o.tail, "tail", 2*time.Second, "silence after the last burst")
	flag.Float64Var(&o.minSNR, "min-snr", latency.DefaultMinSNRdB, "dB a residual burst's match must stand over the room's median to count")
	flag.IntVar(&o.index, "index", -1, "STREAM_MUSIC index 0..30 for the run, restored afterwards (-1: leave it)")
	flag.StringVar(&o.out, "o", "", "write <prefix>.raw, .periods.csv and .writes.csv")
	flag.Parse()
	switch wave {
	case "tone":
		o.wave = latency.Tone
	case "noise":
		o.wave = latency.Noise
	default:
		log.Fatalf("sl_latency: -wave %q: want tone or noise", wave)
	}
	if o.n < 1 || o.burst <= 0 || time.Duration(latency.WriteNs) < o.burst || o.gap < o.burst {
		log.Fatalf("sl_latency: need -n ≥ 1 and 0 < -burst ≤ %v ≤ -gap", time.Duration(latency.WriteNs))
	}
	return o
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)
	if err := run(parseFlags()); err != nil {
		log.Printf("sl_latency: %v", err)
		os.Exit(1)
	}
}

func run(o options) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	index, restore, err := volume(o.index)
	if err != nil {
		return err
	}
	defer restore()

	mic, err := slmic.OpenOpenSL()
	if err != nil {
		return err
	}
	defer mic.Close()
	sink, err := slspeaker.NewOpenSL()
	if err != nil {
		return err
	}
	defer sink.Close()

	plan := latency.Plan(o.lead, o.gap, o.n, o.stall, o.tail)
	clock := newRenderClock(len(plan))
	sink.SetOnComplete(clock.complete)

	c := &capturer{mic: mic, first: make(chan struct{})}
	capDone := make(chan struct{})
	go func() {
		defer close(capDone)
		c.run()
	}()
	select {
	case <-c.first:
	case <-time.After(firstCaptureWait):
		mic.Close()
		<-capDone
		return errors.New("no capture within 5 s: another recorder holds the input (stop echomuse, and the kiosk's microphone features)")
	case <-ctx.Done():
		mic.Close()
		<-capDone
		return ctx.Err()
	}

	log.Printf("playing %d bursts of %v %s at %.1f dBFS, %v apart (stall %v); %s",
		o.n, o.burst, waveName(o), o.level, o.gap, o.stall, index)
	bursts, written, writeErr := play(ctx, sink, clock, plan, latency.Burst(o.wave, o.freq, o.level,
		int(int64(o.burst)*render.SampleRate/int64(time.Second))))
	clock.wait(written, 3*time.Second)
	mic.Close()
	<-capDone
	_ = sink.Close()

	r := analyse(o, c, clock, bursts)
	r.print(os.Stdout, c, clock, mic.Drops(), written)
	if o.out != "" {
		if err := dump(o.out, c, clock, plan); err != nil {
			log.Printf("sl_latency: -o: %v", err)
		}
	}
	if writeErr != nil {
		return writeErr
	}
	return ctx.Err()
}

func waveName(o options) string {
	if o.wave == latency.Noise {
		return "white noise"
	}
	return fmt.Sprintf("%.0f Hz tone", o.freq)
}

// volume reads STREAM_MUSIC and, for index ≥ 0, sets it until restore.
func volume(index int) (desc string, restore func(), err error) {
	vol := androidvol.New()
	restore = func() {}
	prev, err := vol.Index()
	if index < 0 {
		if err != nil {
			return fmt.Sprintf("STREAM_MUSIC index unknown (%v)", err), restore, nil
		}
		return fmt.Sprintf("STREAM_MUSIC index %d/%d", prev, androidvol.MaxIndex), restore, nil
	}
	if err != nil {
		return "", nil, fmt.Errorf("volume: %w", err)
	}
	if err := vol.Verify(); err != nil {
		return "", nil, fmt.Errorf("volume: %w", err)
	}
	if err := vol.SetIndex(index); err != nil {
		return "", nil, fmt.Errorf("volume: %w", err)
	}
	restore = func() {
		if err := vol.SetIndex(prev); err != nil {
			log.Printf("sl_latency: restoring STREAM_MUSIC index %d: %v", prev, err)
			return
		}
		log.Printf("STREAM_MUSIC index restored to %d", prev)
	}
	return fmt.Sprintf("STREAM_MUSIC index %d/%d for the run (was %d)", index, androidvol.MaxIndex, prev), restore, nil
}

// play writes the plan to the sink and records each burst's write and the
// render clock fit's stamp for its first frame at the moment it was
// written, as the supervisor's reference tap stamps it. It stops at a write
// error, which the firmware would answer with a new render epoch.
func play(ctx context.Context, sink *slspeaker.OpenSLSink, clock *renderClock, plan []latency.Step,
	burst []int16) (bursts []burstRec, written int, err error) {
	silence := make([]int16, render.SinkFrames)
	for w, step := range plan {
		if step.StallBefore > 0 {
			select {
			case <-time.After(step.StallBefore):
			case <-ctx.Done():
			}
		}
		if ctx.Err() != nil {
			return bursts, written, nil
		}
		buf := silence
		if step.Burst {
			fitNs, ok := clock.fitAt(uint64(w) * render.SinkFrames)
			bursts = append(bursts, burstRec{write: w, stalled: step.StallBefore > 0, fitNs: fitNs, fitOK: ok})
			buf = burst
		}
		if err := sink.Write(buf); err != nil {
			return bursts, written, fmt.Errorf("write %d: %w", w, err)
		}
		written++
	}
	return bursts, written, nil
}
