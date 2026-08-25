//go:build server

// Package slspeaker renders playback through Android's audio HAL via OpenSL
// ES, which is what reaches the far-end tap Amazon's ASP front end takes at
// the HAL's playback side. Writing PCM straight to the codec never passes
// through AudioFlinger, so the AEC on slmic's capture path would have nothing
// to cancel against (docs/native-afe-migration.md, "the one rule that
// matters": capture and playback must BOTH go through the framework).
//
// It is the only playback backend. The two-plane design (voice ducks music
// instead of pausing it), the duck ramp, the prime gate and the per-stream
// delivery stats live in mix.go and stream.go, which carry no OpenSL
// dependency and are host-testable.
package slspeaker

import (
	"fmt"
	"log"
	"math"
	"os"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/internal/cue"
	"github.com/wilbowes/EchoMuse/internal/opensl"
	pkgspeaker "github.com/wilbowes/EchoMuse/pkg/speaker"
)

const (
	defaultLib   = "libOpenSLES.so"
	sampleRateHz = 48000 // matches the wire — nothing resamples

	// periodFrames is nominal, used only to pace the pump loop while idle
	// (see pumpLoop) — unlike ALSA there is no hardware ring size this must
	// match exactly. 2048 mono samples ≈ 42.7ms is the period length
	// duckRampPeriods (mix.go) was tuned against.
	periodFrames = 2048

	// maxPeriodBytes bounds a single Write, with generous headroom over the
	// wire's actual period size (4096 bytes of mono S16).
	maxPeriodBytes = 8192

	// hwBuffers is the OpenSL ES-facing double/triple buffering only, not
	// the lead ring below. A touch deeper than the recorder's: a stall here
	// is audible, where a dropped capture period is just a missed analysis
	// window.
	hwBuffers = 4

	// audioChanDepth / primePeriods carry over the figures the ALSA-ring
	// backend used, as a starting point rather than an independently derived
	// match: 128 periods ≈ 5.46s of lead against measured 1.8–2.6s link
	// stalls, held before start until ~1s is queued. Redoing the sizing
	// analysis for THIS backend (link-stall depth vs. hardware buffer
	// headroom) is filed in docs/native-afe-migration.md's "Deferred /
	// filed" section — these are for Phase 3 measurement, not verified
	// figures.
	audioChanDepth = 128
	primePeriods   = 24
)

// Speaker implements pkg/speaker.FullSpeaker over an OpenSL ES AudioPlayer.
type Speaker struct {
	eng  *opensl.Engine
	play *opensl.Player

	stopCh chan struct{}
	deadCh chan struct{}

	voice *audioStream
	music *audioStream

	duckTarget atomic.Int32
	mixer      Mixer

	// cue is the third plane — short device-local sounds (the wake
	// confirmation chime), mixed at the same write point with none of
	// audioStream's stream machinery — no prime gate, no discard-until-EOS,
	// no StreamStats (see internal/cue). cueBuf is pump-loop-local scratch,
	// and is mono: this backend writes the wire format straight out.
	cue    cue.Player
	cueBuf []byte

	// levelTap fires at the Write point — VOICE only, pre-mix, so the meter
	// ring visualises the response and not the music bed under it. There is
	// no echo tap beside it: the HAL takes its own far-end reference
	// internally, which is the entire reason capture and playback both have
	// to go through the framework (docs/native-afe-migration.md, "the one
	// rule that matters").
	levelTap func(rms float64)

	statsMu sync.Mutex
	statsCb func(pkgspeaker.StreamStats)
}

// OnStreamStats registers a per-stream stats callback.
func (s *Speaker) OnStreamStats(cb func(pkgspeaker.StreamStats)) {
	s.statsMu.Lock()
	s.statsCb = cb
	s.statsMu.Unlock()
}

// NewSpeaker opens the OpenSL ES engine (shared with slmic's Recorder if one
// is also open — opensl.Open caches by library path) and an AudioPlayer.
//
// It does NOT run `stop media`: this backend needs mediaserver to keep owning
// the PCM, or the HAL's ASP front end never sees this playback as a far-end
// reference at all (docs/native-afe-migration.md, "the one rule that
// matters").
func NewSpeaker(levelTap func(rms float64)) (*Speaker, error) {
	lib := os.Getenv("EM_OPENSL_LIB")
	if lib == "" {
		lib = defaultLib
	}
	eng, err := opensl.Open(lib)
	if err != nil {
		return nil, fmt.Errorf("slspeaker: %w", err)
	}
	play, err := eng.NewPlayer(sampleRateHz, maxPeriodBytes, hwBuffers)
	if err != nil {
		return nil, fmt.Errorf("slspeaker: %w", err)
	}

	s := &Speaker{
		eng:      eng,
		play:     play,
		stopCh:   make(chan struct{}),
		deadCh:   make(chan struct{}),
		levelTap: levelTap,
	}
	s.voice = newAudioStream(audioChanDepth, s.deadCh)
	s.music = newAudioStream(audioChanDepth, s.deadCh)
	s.cueBuf = make([]byte, periodFrames*2) // mono S16
	s.duckTarget.Store(unityGain)
	s.mixer.SetGainImmediate(unityGain)

	go s.pumpLoop()

	// Enable the internal speaker amp. It is the one piece of hardware setup
	// that is NOT about owning the ALSA PCM: Ext_Speaker_Amp_Switch is a
	// physical mixer control, defaults off, and is turned
	// off by accdet on a headphone insert; Init was historically the only thing
	// that ever turned it on (issue #80). Dropping it here made the native-AFE
	// path deliver every period with underruns=0, minDepth healthy and the
	// controller reporting a clean turn, into a dead amp — audible as nothing
	// at all, with no failure visible anywhere in the logs. Confirmed on
	// hardware 2026-08-12: the control read Off with no jack inserted, and
	// setting it On restored audio with no other change.
	//
	// There is no "amp on onto a clocked, silent DAC" wait before it. That
	// ordering keeps the amp's turn-on transient inaudible, but it relies on
	// a continuously silence-filled session, which this path deliberately
	// does not have — an idle OpenSL ES player queues nothing at all (see
	// NewPlayer). Whether that costs an audible pop is unverified on
	// hardware; a pop is strictly better than silence, and Phase 0/3 owns
	// settling it with measurement rather than a guess, the same reasoning
	// Close already documents.
	s.EnableSpeakerAmp()

	log.Printf("[slspeaker] playing via OpenSL ES (%dHz mono) — mediaserver keeps the PCM, no stop media on this path", sampleRateHz)
	return s, nil
}

// Init satisfies pkg/speaker.Speaker. NewSpeaker already does all the work
// (opening the OpenSL ES objects starts playback state immediately; see
// opensl.Engine.NewPlayer), so this is a no-op.
func (s *Speaker) Init() error { return nil }

// pumpLoop drains whichever plane(s) have audio, mixes them, and writes to
// the OpenSL ES player.
//
// Pacing is the one subtlety. A blocking ALSA write paces such a loop even
// when idle, because the session is continuously silence-filled; OpenSL ES's
// buffer queue has no such requirement — an AudioPlayer with an empty queue
// is simply idle, not underrunning (see NewPlayer's doc), so there is nothing
// to write and nothing that blocks. This loop therefore sleeps one nominal
// period when idle, and lets Write's own blocking (on a free hardware buffer
// slot) provide the pacing once something is actually playing.
//
// Whether ASP's own internal AEC wants a continuous (silence-inclusive)
// far-end reference is unverified on hardware — it is one of the questions
// docs/native-afe-migration.md's Phase 0 spike exists to answer.
func (s *Speaker) pumpLoop() {
	defer close(s.deadCh)
	idlePeriod := time.Duration(periodFrames) * time.Second / time.Duration(sampleRateHz)

	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		var voice, music []byte
		if s.voice.ready(primePeriods) {
			voice = s.voice.take()
		} else if s.voice.playing {
			s.report(s.voice.drained(), "voice")
		}
		if s.music.ready(primePeriods) {
			music = s.music.take()
		} else if s.music.playing {
			s.report(s.music.drained(), "music")
		}

		var level float64
		if voice != nil && s.levelTap != nil {
			level = periodRMS(voice)
		}

		out := s.mixer.Mix(voice, music, s.duckTarget.Load())
		// Before the idle sleep, or a cue starting while nothing else plays
		// would be the one thing this loop never notices. The level tap
		// stays voice-only either way — a cue is the device speaking for
		// itself, not a response to visualise.
		if s.cue.Next(s.cueBuf) {
			out = mixCue(out, s.cueBuf)
		}
		if out == nil {
			time.Sleep(idlePeriod)
			continue
		}

		if s.levelTap != nil {
			s.levelTap(level)
		}
		if err := s.play.Write(out); err != nil {
			log.Printf("[slspeaker] pumpLoop: write error: %v", err)
			return
		}
	}
}

func (s *Speaker) report(st *pkgspeaker.StreamStats, plane string) {
	if st == nil {
		log.Printf("[slspeaker] UNDERRUN: %s channel drained mid-stream — injecting silence", plane)
		return
	}
	log.Printf("[slspeaker] %s stream complete — returning to silence "+
		"(periods=%d underruns=%d minDepth=%d primeWait=%dms recvSpan=%dms maxGap=%dms)",
		plane, st.Periods, st.Underruns, st.MinDepth,
		st.PrimeWaitMs, st.RecvSpanMs, st.MaxGapMs)
	if plane != "voice" || st.Periods == 0 {
		return
	}
	s.statsMu.Lock()
	cb := s.statsCb
	s.statsMu.Unlock()
	if cb != nil {
		go cb(*st)
	}
}

// PumpPeriod queues one period of VOICE audio. The wire is already mono at
// this backend's native rate and OpenSL ES takes the mono channel directly —
// duplicating L=R was only ever an I2S/codec-path requirement of writing the
// PCM ourselves.
func (s *Speaker) PumpPeriod(data []byte) error {
	_, err := s.voice.pump(data, len(data))
	return err
}

// PlayCue starts a device-local notification sound (the wake confirmation
// chime). Returns immediately — pumpLoop mixes it in over the following
// periods.
func (s *Speaker) PlayCue(pcm []byte) { s.cue.Play(pcm) }

// PumpMusic queues one period of MUSIC audio (0x04) — identical handling to
// the voice plane, kept separate so a voice turn can duck it rather than
// stopping it.
func (s *Speaker) PumpMusic(data []byte) error {
	_, err := s.music.pump(data, len(data))
	return err
}

// SetDuck sets the gain applied to music while it plays under voice, in dB
// of attenuation. Ramped, not applied at once — see mix.go's applyGain.
func (s *Speaker) SetDuck(db float64) { s.duckTarget.Store(DuckGain(db)) }

// IsStreaming reports whether a VOICE stream is mid-flight. Music
// deliberately does not count: it is a quiet continuous bed, not a response
// being talked over, and reporting it would drop the device's wake threshold
// to the barge-in bar for the length of a song.
func (s *Speaker) IsStreaming() bool { return s.voice.isActive() }

// IsPlayingMusic reports whether a music stream is mid-flight.
func (s *Speaker) IsPlayingMusic() bool { return s.music.isActive() }

// EndStream marks the in-flight voice stream complete (0x03).
func (s *Speaker) EndStream() { s.voice.endStream() }

// EndMusicStream marks the in-flight music stream complete (0x05).
func (s *Speaker) EndMusicStream() { s.music.endStream() }

// Flush cuts a playing VOICE stream (barge-in): drains the Go-side channel
// and arms discard-until-EOS. It does NOT clear anything already handed to
// the OpenSL ES hardware queue — that queue holds already-MIXED periods
// (voice and music summed, or a bare music period when voice was nil that
// round), so clearing it indiscriminately could cut real, still-wanted music
// audio that happened to be queued unmixed. The residual tail is hwBuffers
// deep, a few tens of milliseconds.
func (s *Speaker) Flush() { s.voice.flush() }

// FlushMusic stops music the same way, for the user genuinely stopping or
// pausing. A voice turn ducks instead and must never send this: flushing
// discards the buffered audio that makes ducking instant, and on a
// non-seekable stream it cannot be recovered.
func (s *Speaker) FlushMusic() { s.music.flush() }

// EnableSpeakerAmp re-enables the internal speaker amp after a headphone plug
// is removed. accdet mutes this physical mixer control on insert regardless
// of who owns the PCM, and nothing else ever turns it back on (issue #80).
func (s *Speaker) EnableSpeakerAmp() {
	if out, err := exec.Command("tinymix", "-D", "0", "5", "On").CombinedOutput(); err != nil {
		log.Printf("[slspeaker] could not re-enable speaker amp: %v — %s", err, strings.TrimSpace(string(out)))
		return
	}
	log.Println("[slspeaker] speaker amp re-enabled")
}

// Close stops the pump loop and releases the player.
//
// Deliberately does NOT drive the amp/mute tinymix controls to mask a close
// transient. mediaserver owns the PCM here, and Android's own audio HAL is
// already observed to react to hardware events on this same amp/mux (see
// CLAUDE.md's jack section) — fighting that from here risks a worse click
// than doing nothing. Neither has been verified on hardware; Phase 0/3 owns
// settling it with measurement.
func (s *Speaker) Close() {
	close(s.stopCh)
	s.play.Clear() // drop anything queued but not yet played
	s.play.Close()
	log.Println("[slspeaker] closed")
}

// periodRMS computes the RMS level of a mono S16LE period, normalized to
// 0..1 of int16 full-scale. Every sample: a mono period is a quarter the size
// of the stereo one this used to subsample, so the equivalent cost is already
// paid.
func periodRMS(period []byte) float64 {
	if len(period) < 2 {
		return 0
	}
	var sum uint64
	n := 0
	for i := 0; i+1 < len(period); i += 2 {
		v := int64(int16(uint16(period[i]) | uint16(period[i+1])<<8))
		sum += uint64(v * v)
		n++
	}
	if n == 0 {
		return 0
	}
	return math.Sqrt(float64(sum)/float64(n)) / 32768.0
}

var _ pkgspeaker.FullSpeaker = (*Speaker)(nil)
