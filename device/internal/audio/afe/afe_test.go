package afe

import (
	"bufio"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
)

// The fixtures are bit 0 of real Fire OS 6 captures (G090LF0965260F1J):
// testdata/rw.bits is micAsr frames [160, 1120) of a 14 s capture with a
// played stimulus (idle, playback onset, steady echo, room sound under
// playback); testdata/lm.bits is the beam channel of a micMultiChAsr
// capture after a reboot, frames [900, 940), where OUTPUT_CLIPPED fired.
// Bits are packed most significant first. The .golden files hold one record
// per 1280-sample period, computed from the same bits by an independent
// decoder of the v3.3 table in docs/alexa-afe.md.

const period = 1280

func loadBits(t *testing.T, name string) []uint8 {
	t.Helper()
	raw, err := os.ReadFile("testdata/" + name + ".bits")
	if err != nil {
		t.Fatal(err)
	}
	out := make([]uint8, 0, 8*len(raw))
	for _, b := range raw {
		for i := 7; i >= 0; i-- {
			out = append(out, b>>i&1)
		}
	}
	return out
}

func loadGolden(t *testing.T, name string) []ema.AFERecord {
	t.Helper()
	f, err := os.Open("testdata/" + name + ".golden")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	var out []ema.AFERecord
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if strings.HasPrefix(line, "#") {
			continue
		}
		var v [ema.AFERecordSize]uint8
		fields := strings.Fields(line)
		if len(fields) != len(v) {
			t.Fatalf("golden line %q", line)
		}
		for i, s := range fields {
			n, err := strconv.ParseUint(s, 10, 8)
			if err != nil {
				t.Fatal(err)
			}
			v[i] = uint8(n)
		}
		r, err := ema.ParseAFE(v[:])
		if err != nil {
			t.Fatalf("golden %q: %v", line, err)
		}
		out = append(out, r)
	}
	return out
}

// pcm carries bits in bit 0 of otherwise random samples: only bit 0 may
// matter to the decoder.
func pcm(bits []uint8, seed uint64) []int16 {
	r := rand.New(rand.NewPCG(seed, 1))
	out := make([]int16, len(bits))
	for i, b := range bits {
		out[i] = int16(r.Uint32())&^1 | int16(b)
	}
	return out
}

// decodeAll feeds pcm in period-sized blocks (the last may be short).
func decodeAll(d *Decoder, in []int16) []Period {
	var out []Period
	for len(in) > 0 {
		n := min(period, len(in))
		var p Period
		d.Decode(in[:n], &p)
		out = append(out, p)
		in = in[n:]
	}
	return out
}

func frameAt(t *testing.T, bits []uint8, k int) Frame {
	t.Helper()
	var hi, lo uint64
	for _, b := range bits[k*FrameBits : (k+1)*FrameBits] {
		hi = hi<<1 | lo>>63
		lo = lo<<1 | uint64(b)
	}
	f, ok := parse(hi, lo)
	if !ok {
		t.Fatalf("fixture frame %d does not validate", k)
	}
	return f
}

type totals struct{ frames, invalid, syncs, gaps, playback, erleFrames, dtdFrames, vadFrames int }

func sum(ps []Period) totals {
	var s totals
	for _, p := range ps {
		s.frames += int(p.Record.Frames)
		s.invalid += int(p.Invalid)
		s.syncs += int(p.Syncs)
		s.gaps += int(p.Gaps)
		s.playback += int(p.Record.Playback)
		s.erleFrames += int(p.Record.ERLEFrames)
		s.dtdFrames += int(p.Record.DTDFrames)
		s.vadFrames += int(p.Record.VADFrames)
	}
	return s
}

func TestFixturesMatchTheReferenceDecoder(t *testing.T) {
	for _, name := range []string{"rw", "lm"} {
		t.Run(name, func(t *testing.T) {
			want := loadGolden(t, name)
			got := decodeAll(&Decoder{}, pcm(loadBits(t, name), 7))
			if len(got) != len(want) {
				t.Fatalf("%d periods, want %d", len(got), len(want))
			}
			for i, p := range got {
				if p.Record != want[i] {
					t.Errorf("period %d: %+v, want %+v", i, p.Record, want[i])
				}
				if err := p.Record.Validate(); err != nil {
					t.Errorf("period %d: %v", i, err)
				}
				wantSyncs := uint8(0)
				if i == 0 {
					wantSyncs = 1
				}
				if p.Invalid != 0 || p.Gaps != 0 || p.Lost != 0 || p.Syncs != wantSyncs {
					t.Errorf("period %d health %+v", i, p)
				}
			}
		})
	}
}

// The real fields move: idle, then playback with echo and double talk.
func TestFixtureFieldsMove(t *testing.T) {
	got := decodeAll(&Decoder{}, pcm(loadBits(t, "rw"), 1))
	idle, play := got[3].Record, got[25].Record
	if idle.Playback != 0 || idle.ERLEMax != 0 || idle.Volume != 70 || idle.RMSMax == 0 {
		t.Errorf("idle period %+v", idle)
	}
	if play.Playback != 10 || play.ERLEMax == 0 || play.ERLEFrames != 10 {
		t.Errorf("playback period %+v", play)
	}
	s := sum(got)
	if s.dtdFrames == 0 || s.vadFrames == 0 {
		t.Errorf("no double talk or VAD in the fixture: %+v", s)
	}
	clip := decodeAll(&Decoder{}, pcm(loadBits(t, "lm"), 1))
	if clip[2].Record.Flags&ema.AFEOutputClipped == 0 {
		t.Errorf("OUTPUT_CLIPPED not carried: %+v", clip[2].Record)
	}
}

// A stream joined 77 samples into a frame: the decoder hunts, locks two
// frames later, and every later frame straddles a period boundary.
func TestSyncMidStreamWithFramesSpanningPeriods(t *testing.T) {
	bits := loadBits(t, "rw")
	got := decodeAll(&Decoder{}, pcm(bits[77:], 3))
	n := len(bits) / FrameBits
	var want totals
	for k := 2; k < n; k++ { // frame 0 is cut, frame 1 only confirms the lock
		f := frameAt(t, bits, k)
		want.frames++
		if f.PlaybackActive {
			want.playback++
		}
		if f.ERLE > 0 {
			want.erleFrames++
		}
	}
	want.syncs = 1
	if s := sum(got); s.frames != want.frames || s.playback != want.playback ||
		s.erleFrames != want.erleFrames || s.syncs != 1 || s.gaps != 0 || s.invalid != 0 {
		t.Fatalf("totals %+v, want %+v", s, want)
	}
	// Frame k ends at shifted sample 128k+50: period 0 holds the ends of
	// frames 0–9 of which 2–9 are accepted, then ten per full period.
	if got[0].Record.Frames != 8 || got[0].Record.Flags&ema.AFESync == 0 {
		t.Errorf("first period %+v", got[0])
	}
	for i, p := range got[1 : len(got)-1] {
		if p.Record.Frames != 10 {
			t.Errorf("period %d holds %d frames", i+1, p.Record.Frames)
		}
	}
}

// One corrupted frame costs exactly that frame: the next one is on the old
// grid with the counter it predicts, so the lock resumes without a sync or
// a gap.
func TestCorruptedChecksum(t *testing.T) {
	bits := loadBits(t, "rw")
	const k = 304
	bits[k*FrameBits+50] ^= 1 // inside ERLE_RAW
	got := decodeAll(&Decoder{}, pcm(bits, 4))
	p := got[k/10]
	if p.Invalid != 1 || p.Record.Frames != 9 || p.Syncs != 0 || p.Gaps != 0 {
		t.Fatalf("period %+v", p)
	}
	if s := sum(got); s.frames != len(bits)/FrameBits-2 || s.syncs != 1 {
		t.Fatalf("totals %+v", s)
	}
}

// Periods dropped before the decoder (a full capture queue) keep the frame
// phase; FRAME_COUNTER counts the frames, and AFE_TIMESTAMP resolves a drop
// longer than the counter's 64 frames.
func TestCounterGap(t *testing.T) {
	for _, drop := range []int{3, 7} {
		bits := loadBits(t, "rw")
		const at = 40
		cut := append(append([]uint8(nil), bits[:at*period]...), bits[(at+drop)*period:]...)
		got := decodeAll(&Decoder{}, pcm(cut, 5))
		p := got[at]
		if p.Gaps != 1 || p.Lost != uint32(drop*10) || p.Record.Lost != uint8(drop*10) ||
			p.Record.Flags&ema.AFEGap == 0 || p.Record.Frames != 10 || p.Syncs != 0 {
			t.Errorf("drop %d: period %+v", drop, p)
		}
		if s := sum(got); s.gaps != 1 || s.invalid != 0 {
			t.Errorf("drop %d: totals %+v", drop, s)
		}
	}
}

// A re-opened stream starts on a period boundary and so keeps the frame
// phase: the lock holds, and the counters of the new stream (here a capture
// from another boot) report the discontinuity.
func TestReopenKeepsLockAndReportsTheGap(t *testing.T) {
	rw, lm := loadBits(t, "rw"), loadBits(t, "lm")
	got := decodeAll(&Decoder{}, pcm(append(append([]uint8(nil), rw...), lm...), 6))
	i := len(rw) / period
	p := got[i]
	// Last rw frame: counter 52, 19,153 ms; first lm frame: counter 33,
	// 41,880 ms. 45 mod 64 and ≈2,841 frames by time: 2,861 elapsed, one slot.
	if p.Record.Frames != 10 || p.Syncs != 0 || p.Gaps != 1 || p.Lost != 2860 ||
		p.Record.Lost != 255 || p.Record.Flags&(ema.AFEGap|ema.AFESync) != ema.AFEGap {
		t.Fatalf("reopen period %+v", p)
	}
	want := loadGolden(t, "lm")
	for j, w := range want[1:] {
		if got[i+1+j].Record != w {
			t.Errorf("lm period %d: %+v, want %+v", j+1, got[i+1+j].Record, w)
		}
	}
}

// A stream without metadata (noise, silence) never yields a frame.
func TestNoMetadata(t *testing.T) {
	r := rand.New(rand.NewPCG(9, 9))
	noise := make([]uint8, 16000*60)
	for i := range noise {
		noise[i] = uint8(r.Uint32() & 1)
	}
	for name, in := range map[string][]int16{"noise": pcm(noise, 2), "silence": make([]int16, 16000*5)} {
		if s := sum(decodeAll(&Decoder{}, in)); s.frames != 0 || s.syncs != 0 || s.invalid != 0 {
			t.Errorf("%s: %+v", name, s)
		}
	}
}

func TestDecodeDoesNotAllocate(t *testing.T) {
	in := pcm(loadBits(t, "rw"), 8)
	var d Decoder
	var p Period
	i := 0
	if n := testing.AllocsPerRun(50, func() {
		d.Decode(in[i*period:(i+1)*period], &p)
		i = (i + 1) % (len(in) / period)
	}); n != 0 {
		t.Fatalf("%v allocations per period", n)
	}
}
