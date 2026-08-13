//go:build server

// afe_probe is the phase-0 spike for docs/native-afe-migration.md — a
// standalone tool that answers, cheaply and on real hardware, everything the
// rest of that plan assumes, before any interface or server code changes:
//
//  1. Does OpenSL ES work at all on this FireOS build?
//  2. Does VOICE_RECOGNITION actually instantiate ASP pipeline 0? MIC is
//     configured with an empty algorithm list, so it is the CONTROL: any
//     difference between the two recordings IS the AFE, not the codec path.
//  3. ERLE — the number the whole exercise is for. Plays a known tone and
//     compares captured levels between MIC (no AEC) and VOICE_RECOGNITION
//     (AEC'd) recordings of the SAME tone.
//  4. Rough capture latency/jitter, against the mic pipeline's hard 160ms
//     deadline (see CLAUDE.md's device audio pipeline section).
//
// It does not link into the server, does not change any interface, and
// touches nothing but the microphone/speaker for the duration of a run.
//
// Kill criteria (docs/native-afe-migration.md): if ERLE at VOICE_RECOGNITION
// is not clearly better than the ~7-9dB the current speexdsp path achieves,
// stop there — the whole justification for this migration is that Amazon's
// front end is better, and if it measures otherwise on this hardware, the
// rest of the plan is not worth the disruption.
//
// Build with device/tools/build_tools.sh (needs the echomuse-compiler image;
// like oww_probe this is a module tool, not standalone — it imports
// internal/opensl, and Go's internal-package rule only permits that from
// inside github.com/wilbowes/EchoMuse). Deploy the binary and run:
//
//	adb shell su -c 'stop echomuse'
//	adb push build/afe_probe /data/local/tmp/afe_probe
//	adb shell "su -c '/data/local/tmp/afe_probe -seconds 8 -out /sdcard'"
//	adb pull /sdcard/afe_probe .
//
// Channel count (open question #4 in the plan — does the AFE ever want to
// emit more than one channel?) is NOT explored here: this tool, like
// slmic/slspeaker, always requests mono. If a preset needs to be tried at
// stereo to answer that question, it needs a code change to
// internal/opensl's fixed numChannels=1, not a flag here — cross-check with
// `adb shell dumpsys audio` during a run instead.
package main

import (
	"encoding/binary"
	"flag"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/wilbowes/EchoMuse/internal/opensl"
)

const (
	micSampleRateHz = 16000
	micPeriodFrames = 1280 // 80ms @ 16kHz — matches the real wire framing
	micBuffers      = 4

	toneSampleRateHz = 48000
	tonePeriodFrames = 2048 // matches slspeaker's nominal period
	toneBuffers      = 4
)

func main() {
	var (
		lib       = flag.String("lib", "libOpenSLES.so", "path to libOpenSLES.so (a bare soname resolves against /system/lib on the device)")
		out       = flag.String("out", "/sdcard", "output directory for WAVs and the report")
		seconds   = flag.Int("seconds", 8, "recording duration per preset")
		presetsFl = flag.String("presets", "mic,voice_recognition,voice_communication", "comma-separated: mic, voice_recognition, voice_communication")
		playTone  = flag.Bool("tone", true, "play a known tone through OpenSL ES during each recording (needed for the ERLE comparison)")
		toneHz    = flag.Float64("tone-hz", 1000, "tone frequency")
		toneAmp   = flag.Float64("tone-amp", 0.25, "tone amplitude, 0..1 of full scale")
	)
	flag.Parse()

	if err := os.MkdirAll(*out, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "afe_probe: creating %s: %v\n", *out, err)
		os.Exit(1)
	}

	presets, err := parsePresets(*presetsFl)
	if err != nil {
		fmt.Fprintf(os.Stderr, "afe_probe: %v\n", err)
		os.Exit(2)
	}

	fmt.Printf("afe_probe: opening %s\n", *lib)
	eng, err := opensl.Open(*lib)
	if err != nil {
		fmt.Fprintf(os.Stderr, "afe_probe: FAIL — OpenSL ES did not open: %v\n", err)
		fmt.Fprintln(os.Stderr, "This answers question 1 on its own: OpenSL ES does not work on this build. Stop here.")
		os.Exit(1)
	}
	fmt.Println("OpenSL ES engine opened OK — question 1 answered: yes.")

	results := map[string]result{}
	var order []string
	for _, p := range presets {
		fmt.Printf("\n=== recording %s for %ds (tone=%v) ===\n", p, *seconds, *playTone)
		r, err := recordOne(eng, p, *seconds, *out, *playTone, *toneHz, *toneAmp)
		if err != nil {
			fmt.Fprintf(os.Stderr, "  FAIL: %v\n", err)
			continue
		}
		fmt.Printf("  wrote %s\n", r.wavPath)
		fmt.Printf("  frames=%d startupLatency=%s maxGap=%s rms=%.6f (%.1f dBFS)\n",
			r.frames, r.startupLatency.Round(time.Millisecond), r.maxGap.Round(time.Millisecond),
			r.rms, dbfs(r.rms))
		results[p.String()] = r
		order = append(order, p.String())
	}

	fmt.Println("\n=== summary ===")
	for _, name := range order {
		r := results[name]
		fmt.Printf("  %-20s rms=%.1fdBFS startupLatency=%s maxGap=%s\n",
			name, dbfs(r.rms), r.startupLatency.Round(time.Millisecond), r.maxGap.Round(time.Millisecond))
	}

	if *playTone {
		reportERLE(results)
	} else {
		fmt.Println("\n(tone playback was off — no ERLE comparison; question 3 unanswered this run)")
	}
}

// reportERLE compares captured level between the MIC control and each AEC
// pipeline while the same known tone played, per docs/native-afe-
// migration.md point 3. MIC has an empty algorithm list (passthrough), so it
// stands in for "what the tone sounds like at this mic with no cancellation
// at all" — the baseline any AEC is judged against.
func reportERLE(results map[string]result) {
	mic, ok := results[opensl.PresetMic.String()]
	if !ok {
		fmt.Println("\n(no MIC control recording — cannot compute ERLE)")
		return
	}
	fmt.Println("\n=== ERLE (vs MIC control, no cancellation) ===")
	any := false
	for _, p := range []opensl.Preset{opensl.PresetVoiceRecognition, opensl.PresetVoiceCommunication} {
		r, ok := results[p.String()]
		if !ok {
			continue
		}
		any = true
		erle := dbfs(mic.rms) - dbfs(r.rms)
		fmt.Printf("  %-20s ERLE ~= %.1f dB\n", p, erle)
		if p == opensl.PresetVoiceRecognition {
			switch {
			case erle > 9:
				fmt.Printf("    -> clearly better than the ~7-9dB speexdsp ceiling. Kill criteria NOT met — proceed.\n")
			case erle >= 7:
				fmt.Printf("    -> in the same range as the current speexdsp ceiling (~7-9dB), not clearly better.\n")
				fmt.Printf("       Re-read the kill criteria in docs/native-afe-migration.md before proceeding.\n")
			default:
				fmt.Printf("    -> WORSE than or not clearly above the current ~7-9dB speexdsp ceiling.\n")
				fmt.Printf("       Kill criteria MET — per the plan, stop here.\n")
			}
		}
	}
	if !any {
		fmt.Println("  (neither VOICE_RECOGNITION nor VOICE_COMMUNICATION was recorded)")
	}
	fmt.Println("\nThis is a single-tone, single-room, single-take estimate — a real ERLE")
	fmt.Println("number wants several takes, a real speaker/mic placement, and ideally")
	fmt.Println("real speech rather than a pure tone. Treat this as a go/no-go signal,")
	fmt.Println("not a number to publish.")
}

// ─── recording ────────────────────────────────────────────────────────────

type result struct {
	wavPath        string
	frames         int
	startupLatency time.Duration
	maxGap         time.Duration
	rms            float64 // 0..1 of int16 full scale, over the analysis window
}

// recordOne opens a recorder at preset, optionally plays a known tone for the
// same duration, and writes a WAV. The RMS reported is measured over the
// LAST 70% of the capture, skipping the opening ~30% — the recorder's own
// startup transient and (when playing) the tone's ramp-in would otherwise
// bias the level measurement.
func recordOne(eng *opensl.Engine, preset opensl.Preset, seconds int, outDir string,
	playTone bool, toneHz, toneAmp float64) (result, error) {
	rec, err := eng.NewRecorder(preset, micSampleRateHz, micPeriodFrames, micBuffers)
	if err != nil {
		return result{}, fmt.Errorf("open recorder: %w", err)
	}
	defer rec.Close()

	var tonePlayer *opensl.Player
	toneDone := make(chan struct{})
	if playTone {
		tonePlayer, err = eng.NewPlayer(toneSampleRateHz, tonePeriodFrames*2, toneBuffers)
		if err != nil {
			return result{}, fmt.Errorf("open tone player: %w", err)
		}
		defer tonePlayer.Close()
		go func() {
			defer close(toneDone)
			playSineFor(tonePlayer, toneHz, toneAmp, time.Duration(seconds+1)*time.Second)
		}()
		// Give the tone a moment's head start so the recording doesn't open
		// on silence while the player is still spinning up.
		time.Sleep(150 * time.Millisecond)
	}

	start := time.Now()
	if err := rec.Start(); err != nil {
		return result{}, fmt.Errorf("start recorder: %w", err)
	}
	defer rec.Stop()

	var (
		samples        []int16
		startupLatency time.Duration
		lastRead       time.Time
		maxGap         time.Duration
		gotFirst       bool
	)
	deadline := time.Now().Add(time.Duration(seconds) * time.Second)
	for time.Now().Before(deadline) {
		buf, err := rec.Read()
		if err != nil {
			return result{}, fmt.Errorf("read: %w", err)
		}
		now := time.Now()
		if !gotFirst {
			startupLatency = now.Sub(start)
			gotFirst = true
		} else if gap := now.Sub(lastRead); gap > maxGap {
			maxGap = gap
		}
		lastRead = now
		samples = append(samples, bytesToInt16(buf)...)
	}

	if playTone {
		<-toneDone
	}
	if rec.Drops() > 0 {
		fmt.Printf("  note: %d capture periods dropped (Read() fell behind)\n", rec.Drops())
	}

	wavPath := filepath.Join(outDir, "afe_probe_"+preset.String()+".wav")
	if err := writeWAV(wavPath, samples, micSampleRateHz); err != nil {
		return result{}, fmt.Errorf("write wav: %w", err)
	}

	// Analysis window: skip the first 30% (recorder startup + tone ramp-in).
	skip := len(samples) * 3 / 10
	window := samples[skip:]

	return result{
		wavPath:        wavPath,
		frames:         len(samples),
		startupLatency: startupLatency,
		maxGap:         maxGap,
		rms:            rmsInt16(window),
	}, nil
}

// playSineFor writes a continuous sine tone to a Player for dur, in
// tonePeriodFrames-sample chunks. Amplitude is applied directly in float
// before quantising, so toneAmp=1.0 is exactly full scale.
func playSineFor(p *opensl.Player, hz, amp float64, dur time.Duration) {
	periodSamples := tonePeriodFrames
	buf := make([]byte, periodSamples*2)
	n := 0
	deadline := time.Now().Add(dur)
	for time.Now().Before(deadline) {
		for i := 0; i < periodSamples; i++ {
			t := float64(n) / float64(toneSampleRateHz)
			v := amp * math.Sin(2*math.Pi*hz*t)
			s := int16(v * math.MaxInt16)
			binary.LittleEndian.PutUint16(buf[i*2:], uint16(s))
			n++
		}
		if err := p.Write(buf); err != nil {
			return // player closed underneath us — nothing left to do
		}
	}
}

// ─── helpers ──────────────────────────────────────────────────────────────

func parsePresets(s string) ([]opensl.Preset, error) {
	var out []opensl.Preset
	for _, tok := range strings.Split(s, ",") {
		switch strings.ToLower(strings.TrimSpace(tok)) {
		case "mic":
			out = append(out, opensl.PresetMic)
		case "voice_recognition", "vr":
			out = append(out, opensl.PresetVoiceRecognition)
		case "voice_communication", "vc":
			out = append(out, opensl.PresetVoiceCommunication)
		case "":
			// tolerate a trailing comma
		default:
			return nil, fmt.Errorf("unknown preset %q (want mic, voice_recognition, voice_communication)", tok)
		}
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("no presets given")
	}
	return out, nil
}

func bytesToInt16(b []byte) []int16 {
	out := make([]int16, len(b)/2)
	for i := range out {
		out[i] = int16(binary.LittleEndian.Uint16(b[i*2:]))
	}
	return out
}

func rmsInt16(s []int16) float64 {
	if len(s) == 0 {
		return 0
	}
	var sum float64
	for _, v := range s {
		f := float64(v) / 32768.0
		sum += f * f
	}
	return math.Sqrt(sum / float64(len(s)))
}

// dbfs converts a 0..1 RMS fraction to dBFS. A silent (zero) signal reports
// a large negative floor rather than -Inf, so it still sorts/prints sanely.
func dbfs(rms float64) float64 {
	if rms <= 0 {
		return -120
	}
	return 20 * math.Log10(rms)
}

func writeWAV(path string, samples []int16, rateHz int) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()

	dataSize := uint32(len(samples) * 2)
	write := func(v any) { binary.Write(f, binary.LittleEndian, v) }

	f.WriteString("RIFF")
	write(uint32(36) + dataSize)
	f.WriteString("WAVE")
	f.WriteString("fmt ")
	write(uint32(16))
	write(uint16(1))          // PCM
	write(uint16(1))          // mono
	write(uint32(rateHz))     // sample rate
	write(uint32(rateHz * 2)) // byte rate
	write(uint16(2))          // block align
	write(uint16(16))         // bits per sample
	f.WriteString("data")
	write(dataSize)
	for _, s := range samples {
		write(s)
	}
	return nil
}
