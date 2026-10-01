package supervisor

import (
	"context"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/config"
	"github.com/wilbowes/EchoMuse/internal/cue"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/server"
	"github.com/wilbowes/EchoMuse/internal/uplink"
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

// ── fakes ───────────────────────────────────────────────────────────────────

type sent struct {
	typ  proto.MessageType
	gen  uint32
	id   string
	body map[string]any
}

type ackRec struct {
	id     string
	status proto.AckStatus
	code   *proto.AckCode
}

type fakeSession struct {
	mu     sync.Mutex
	msgs   []sent
	acks   []ackRec
	assets assets.Transport // nil: the controller has no assets
}

func (f *fakeSession) Send(typ proto.MessageType, gen uint32, body any) (string, error) {
	b, err := json.Marshal(body)
	if err != nil {
		return "", err
	}
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		return "", err
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	id := "m" + strconv.Itoa(len(f.msgs))
	f.msgs = append(f.msgs, sent{typ: typ, gen: gen, id: id, body: m})
	return id, nil
}

func (f *fakeSession) Ack(id string, status proto.AckStatus, code *proto.AckCode) error {
	f.mu.Lock()
	f.acks = append(f.acks, ackRec{id, status, code})
	f.mu.Unlock()
	return nil
}

func (f *fakeSession) Audio() client.AudioSink { return nopAudio{} }

func (f *fakeSession) Assets() assets.Transport {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.assets != nil {
		return f.assets
	}
	return missingAssets{}
}

// serve makes the controller hold these assets from now on.
func (f *fakeSession) serve(a assets.Transport) {
	f.mu.Lock()
	f.assets = a
	f.mu.Unlock()
}

func (f *fakeSession) of(typ proto.MessageType) []sent {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []sent
	for _, m := range f.msgs {
		if m.typ == typ {
			out = append(out, m)
		}
	}
	return out
}

func (f *fakeSession) types() []proto.MessageType {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []proto.MessageType
	for _, m := range f.msgs {
		out = append(out, m.typ)
	}
	return out
}

func (f *fakeSession) ackFor(id string) ackRec {
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, a := range f.acks {
		if a.id == id {
			return a
		}
	}
	return ackRec{}
}

type nopAudio struct{}

func (nopAudio) SendFrame([]byte, int64) error { return nil }

type missingAssets struct{}

func (missingAssets) Fetch(context.Context, string, int64, io.Writer) (int64, error) {
	return 0, assets.ErrNotFound
}

// servedAssets is a controller holding exactly these assets by SHA-256.
type servedAssets map[string][]byte

func (s servedAssets) Fetch(_ context.Context, sha string, offset int64, w io.Writer) (int64, error) {
	data, ok := s[sha]
	if !ok {
		return 0, assets.ErrNotFound
	}
	_, err := w.Write(data[offset:])
	return int64(len(data)), err
}

// alertWAV is a valid alert asset (48 kHz mono PCM16) and its SHA-256.
func alertWAV(samples int) ([]byte, string) {
	b := binary.LittleEndian.AppendUint32([]byte("RIFF"), uint32(36+2*samples))
	b = append(b, "WAVEfmt "...)
	b = binary.LittleEndian.AppendUint32(b, 16)
	b = binary.LittleEndian.AppendUint16(b, 1)
	b = binary.LittleEndian.AppendUint16(b, 1)
	b = binary.LittleEndian.AppendUint32(b, 48000)
	b = binary.LittleEndian.AppendUint32(b, 96000)
	b = binary.LittleEndian.AppendUint16(b, 2)
	b = binary.LittleEndian.AppendUint16(b, 16)
	b = append(b, "data"...)
	b = binary.LittleEndian.AppendUint32(b, uint32(2*samples))
	for i := range samples {
		b = binary.LittleEndian.AppendUint16(b, uint16(int16(i%200*100)))
	}
	sum := sha256.Sum256(b)
	return b, hex.EncodeToString(sum[:])
}

type epochCall struct{ mic, ref, micRingEnd uint64 }

type fakeUplink struct {
	rings      uplink.Rings
	mu         sync.Mutex
	epochs     []epochCall
	opened     map[string]uint64
	accepted   map[string]bool
	gens       map[string]uint32
	mutes      int
	diagnostic bool
}

func (f *fakeUplink) Attach(client.AudioSink, uplink.SendFunc) {}
func (f *fakeUplink) Detach()                                  {}
func (f *fakeUplink) Notify()                                  {}
func (f *fakeUplink) Run(context.Context)                      {}
func (f *fakeUplink) DiagnosticLive() bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.diagnostic
}
func (f *fakeUplink) SetEpochs(mic, ref uint64) {
	f.mu.Lock()
	f.epochs = append(f.epochs, epochCall{mic, ref, f.rings.Mic.End()})
	f.mu.Unlock()
}
func (f *fakeUplink) Mute() {
	f.mu.Lock()
	f.mutes++
	clear(f.opened)
	f.mu.Unlock()
}
func (f *fakeUplink) OpenCandidate(id string, supportStart uint64) {
	f.mu.Lock()
	f.opened[id], f.gens[id] = supportStart, 1
	f.mu.Unlock()
}
func (f *fakeUplink) AcceptCandidate(id string, ok bool) {
	f.mu.Lock()
	f.accepted[id] = ok
	f.mu.Unlock()
}
func (f *fakeUplink) Open(proto.Envelope, proto.UplinkOpen) error { return nil }
func (f *fakeUplink) Renew(env proto.Envelope, b proto.UplinkRenew) (bool, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if _, ok := f.opened[b.LeaseID]; !ok {
		return false, uplink.ErrUnknownLease
	}
	if env.Generation == f.gens[b.LeaseID]+1 && b.Reason == proto.LeaseTurn {
		f.gens[b.LeaseID] = env.Generation
		return true, nil
	}
	return false, nil
}
func (f *fakeUplink) Close(env proto.Envelope, b proto.UplinkClose) (bool, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if _, ok := f.opened[b.LeaseID]; !ok {
		return false, uplink.ErrUnknownLease
	}
	delete(f.opened, b.LeaseID)
	return f.gens[b.LeaseID] == 1, nil
}
func (f *fakeUplink) lastEpochs() epochCall {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.epochs[len(f.epochs)-1]
}

type fakeMic struct{ closed chan struct{} }

func (m *fakeMic) Read() (pkgmic.Block, error) { <-m.closed; return pkgmic.Block{}, io.EOF }
func (m *fakeMic) Drops() uint64               { return 0 }
func (m *fakeMic) Close()                      {}

// gateSink lets a test run the real mixer: each Write blocks until the test
// takes it from writes, or done closes.
type gateSink struct{ writes, done chan struct{} }

func (g gateSink) Write([]int16) error {
	select {
	case g.writes <- struct{}{}:
	case <-g.done:
	}
	return nil
}
func (gateSink) SetOnComplete(func(int64)) {}
func (gateSink) Restart() error            { return nil }
func (gateSink) Close() error              { return nil }

type fakeHW struct {
	mu  sync.Mutex
	dac int
}

func (h *fakeHW) ReadDAC() (int, error)    { h.mu.Lock(); defer h.mu.Unlock(); return h.dac, nil }
func (h *fakeHW) SetDAC(l int) error       { h.mu.Lock(); h.dac = l; h.mu.Unlock(); return nil }
func (h *fakeHW) SetADCMute(bool) error    { return nil }
func (h *fakeHW) SetSpeakerAmp(bool) error { return nil }
func (h *fakeHW) SetMuteLED(bool) error    { return nil }
func (h *fakeHW) DAC() int                 { h.mu.Lock(); defer h.mu.Unlock(); return h.dac }

type monoClock struct{ ns int64 }

func (c *monoClock) MonoNowNs() int64 { return c.ns }

// ── harness ─────────────────────────────────────────────────────────────────

const (
	periodNs = int64(80 * time.Millisecond)
	sha1     = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f"
)

type harness struct {
	t    *testing.T
	s    *Supervisor
	hw   *fakeHW
	phys *server.Server
	up   *fakeUplink
	sess *fakeSession
	gate gateSink
	mixg sync.WaitGroup // the mixer goroutine, once mix runs it
	now  atomic.Int64
	pcm  []int16
}

type opts struct {
	noWakeLock bool
	ambient    bool
}

func newHarness(t *testing.T, o opts) *harness {
	t.Helper()
	dir := t.TempDir()
	lock := alerts.WakeLockPaths{Lock: filepath.Join(dir, "wake_lock"), Unlock: filepath.Join(dir, "wake_unlock")}
	if !o.noWakeLock {
		for _, p := range []string{lock.Lock, lock.Unlock} {
			if err := os.WriteFile(p, nil, 0o600); err != nil {
				t.Fatal(err)
			}
		}
	}
	speech, err := assets.Open(filepath.Join(dir, "speech"))
	if err != nil {
		t.Fatal(err)
	}
	sounds, err := assets.Open(filepath.Join(dir, "alerts", "assets"))
	if err != nil {
		t.Fatal(err)
	}
	h := &harness{t: t, hw: &fakeHW{dac: 90}, pcm: make([]int16, 1280),
		gate: gateSink{writes: make(chan struct{}), done: make(chan struct{})}}
	h.now.Store(int64(1000 * time.Second))
	h.phys = server.New(server.Config{Hardware: h.hw, StatePath: filepath.Join(dir, "state.json")})
	s, err := Assemble(Config{
		DeviceID: "dev", FirmwareVersion: "v3.0.0-test", BootID: "boot",
		AmbientReadable: func() bool { return o.ambient },
		Mic:             &fakeMic{closed: make(chan struct{})},
		Physical:        h.phys, DeviceConfig: config.New(),
		SpeechStore: speech, AlertStore: sounds,
		NowMonoNS: h.now.Load,
	}, Deps{
		Sink:      h.gate,
		LoadModel: func(wakeword.Paths) (detector.Scorer, error) { return nil, os.ErrNotExist },
		Alerts:    alerts.Config{Root: filepath.Join(dir, "alerts"), BootID: "boot", WakeLock: lock, Clock: &monoClock{ns: h.now.Load()}},
		NewUplink: func(r uplink.Rings, _ uplink.Clock, _ func() int64) Uplink {
			h.up = &fakeUplink{rings: r, opened: map[string]uint64{}, accepted: map[string]bool{}, gens: map[string]uint32{}}
			return h.up
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	h.s = s
	t.Cleanup(func() {
		close(h.gate.done)
		_ = s.mix.Close()
		h.mixg.Wait()
		s.endSession()
		s.wg.Wait()
		s.det.Close()
		_ = s.ex.Close()
	})
	return h
}

// mix runs the real mixer for two sink writes, so every event of the first
// write's blocks has been delivered, and returns the latest final-mix mask.
// The mixer stays parked in its next write until cleanup.
func (h *harness) mix() uint8 {
	h.t.Helper()
	h.mixg.Add(1)
	go func() {
		defer h.mixg.Done()
		_ = h.s.mix.Run()
	}()
	for range 2 {
		select {
		case <-h.gate.writes:
		case <-time.After(5 * time.Second):
			h.t.Fatal("mixer made no sink write")
		}
	}
	return uint8(h.s.lastMask.Load())
}

// block delivers one 80 ms capture callback completing at doneNs.
func (h *harness) block(doneNs int64) { h.s.captureBlock(pkgmic.Block{PCM: h.pcm, MonoNs: doneNs}) }

// blocks delivers n contiguous callbacks.
func (h *harness) blocks(n int) {
	for range n {
		h.block(h.now.Add(periodNs))
	}
}

func (h *harness) ready() { h.readyWith(nil) }

// readyWith sends session.ready with the default detector policy, changed
// by mod when it is non-nil.
func (h *harness) readyWith(mod func(*proto.DetectorPolicy)) {
	h.sess = &fakeSession{}
	pol := proto.DetectorPolicy{
		Thresholds: proto.Thresholds{Idle: 0.9, Playback: 0.65, NearMiss: 0.17},
		HopBlocks:  2, Smoothing: 3, ClearAfterUnscored: 6,
		ProvisionalDuck: proto.ProvisionalDuck{DuckDB: -18, MaxPerWindow: 2, WindowMs: 5000},
	}
	if mod != nil {
		mod(&pol)
	}
	h.s.ready(h.sess, proto.SessionReady{
		Protocol: 1, SessionID: "s1", CapturePermitted: true,
		Assets:   proto.SpeechAssets{RuntimeSHA256: sha1, GraphSHA256: sha1, SidecarSHA256: sha1},
		Detector: pol,
	})
}

var msgSeq int

func (h *harness) control(typ proto.MessageType, gen uint32, body any) string {
	b, err := json.Marshal(body)
	if err != nil {
		h.t.Fatal(err)
	}
	msgSeq++
	id := "c" + strconv.Itoa(msgSeq)
	h.s.Control(proto.Envelope{Protocol: 1, Type: typ, MessageID: id, Generation: gen, Body: b})
	return id
}

func (h *harness) expectAck(id string, status proto.AckStatus, code proto.AckCode) {
	h.t.Helper()
	a := h.sess.ackFor(id)
	var got proto.AckCode
	if a.code != nil {
		got = *a.code
	}
	if a.status != status || got != code {
		h.t.Fatalf("ack %s = %q/%q, want %q/%q", id, a.status, got, status, code)
	}
}

// ringTimer queues a timer ring and lets the executor start it.
func (h *harness) ringTimer(id string) {
	h.t.Helper()
	if err := h.s.ex.HandleTimerRing(alerts.TimerRing{RingID: id, Name: "pasta", Sound: alerts.SoundFallback, MaxRingMs: 900000}); err != nil {
		h.t.Fatal(err)
	}
	h.s.ex.Poll()
}

func epochOf(m sent) string { s, _ := m.body["epoch"].(string); return s }

// ── tests ───────────────────────────────────────────────────────────────────

// WIRE §4.2: current epochs are announced right after session.ready; a new
// capture epoch ends the old streams, moves the uplink to the new epoch
// before its first sample is appended, and opens the new streams.
func TestEpochSequencingAndStreamOpens(t *testing.T) {
	h := newHarness(t, opts{})
	h.s.onRenderEpoch(42)
	h.blocks(3)
	h.ready()

	opens := h.sess.of(proto.TypeStreamOpen)
	if len(opens) != 3 {
		t.Fatalf("stream.open after ready: %v", h.sess.types())
	}
	mic, cells, ref := opens[0], opens[1], opens[2]
	if mic.body["stream_id"] != "mic" || cells.body["stream_id"] != "cells" || ref.body["stream_id"] != "reference" {
		t.Fatalf("stream ids %v %v %v", mic.body["stream_id"], cells.body["stream_id"], ref.body["stream_id"])
	}
	if epochOf(mic) != epochOf(cells) || mic.body["reason"] != "start" || cells.body["format"] != float64(2) || ref.body["kind"] != float64(2) {
		t.Fatalf("capture opens %v %v", mic.body, cells.body)
	}
	first := epochOf(mic)
	if h.s.micRing.End() != 3*1280 {
		t.Fatalf("mic ring end %d", h.s.micRing.End())
	}

	// Time moves backwards: the capture clock is reset (§4.3).
	h.block(h.now.Load() - periodNs)
	ends := h.sess.of(proto.TypeStreamEnd)
	if len(ends) != 2 || epochOf(ends[0]) != first || ends[0].body["final_sample"] != "3840" || ends[0].body["reason"] != "clock_reset" {
		t.Fatalf("stream.end %v", ends)
	}
	opens = h.sess.of(proto.TypeStreamOpen)
	second := opens[len(opens)-2]
	if second.body["stream_id"] != "mic" || epochOf(second) == first || second.body["reason"] != "clock_reset" {
		t.Fatalf("new mic stream %v", second.body)
	}
	last := h.up.lastEpochs()
	if strconv.FormatUint(last.mic, 10) != epochOf(second) || last.micRingEnd != 0 {
		t.Fatalf("uplink epochs %+v: must switch before the first sample of epoch %s", last, epochOf(second))
	}
	if h.s.micRing.End() != 1280 {
		t.Fatalf("new epoch ring end %d, want 1280", h.s.micRing.End())
	}
}

// §4.4, §16.6: a candidate opens its lease, applies the provisional duck when
// the device was producing sound, and reports wake.candidate (generation 1,
// flat body plus active_alert). The controller's ack releases the lease's
// audio; uplink.close(rejected) restores the previous policy.
func TestCandidateLeaseDuckAndRelease(t *testing.T) {
	h := newHarness(t, opts{})
	h.blocks(2)
	h.ready()
	h.ringTimer("t1")

	h.s.onCandidate(detector.Candidate{CandidateID: "c1", LeaseID: "L1", SupportStart: 20000, ProducingSound: true, Profile: detector.ProfilePlayback, Threshold: 0.65})
	if h.up.opened["L1"] != 20000 {
		t.Fatalf("candidate lease not opened: %v", h.up.opened)
	}
	out := h.s.fm.Output()
	if out.Mix.ContentDuckDB != -18 || out.Mix.DialogDuckDB != -18 || !out.AlertBackground {
		t.Fatalf("provisional duck not applied: %+v", out)
	}
	wc := h.sess.of(proto.TypeWakeCandidate)
	if len(wc) != 1 || wc[0].gen != 1 || wc[0].body["lease_id"] != "L1" || wc[0].body["candidate_id"] != "c1" {
		t.Fatalf("wake.candidate %v", wc)
	}
	alert, _ := wc[0].body["active_alert"].(map[string]any)
	if alert["id"] != "t1" || alert["kind"] != "timer" {
		t.Fatalf("active_alert %v", wc[0].body["active_alert"])
	}

	h.control(proto.TypeCommandAck, 0, proto.CommandAck{MessageID: wc[0].id, Status: proto.AckAccepted})
	if !h.up.accepted["L1"] {
		t.Fatal("controller ack did not release the candidate lease")
	}
	id := h.control(proto.TypeUplinkClose, 1, proto.UplinkClose{LeaseID: "L1", Reason: proto.CloseRejected})
	h.expectAck(id, proto.AckAccepted, "")
	out = h.s.fm.Output()
	if out.Mix.ContentDuckDB != 0 || out.Mix.DialogDuckDB != 0 || out.AlertBackground {
		t.Fatalf("rejection did not restore the policy: %+v", out)
	}
}

// §16.6: converting the candidate lease into the turn's lease keeps the duck
// as the turn's input focus instead of restoring.
func TestCandidateConversionKeepsDuckAsTurnFocus(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	h.s.onCandidate(detector.Candidate{CandidateID: "c1", LeaseID: "L1", SupportStart: 9000, ProducingSound: true})
	id := h.control(proto.TypeUplinkRenew, 2, proto.UplinkRenew{LeaseID: "L1", TTLMs: 3000, Reason: proto.LeaseTurn, Owner: "turn-1"})
	h.expectAck(id, proto.AckAccepted, "")
	if out := h.s.fm.Output(); out.Mix.ContentDuckDB != -18 {
		t.Fatalf("conversion restored the duck: %+v", out)
	}
	h.s.releaseCandidateLease("L1")
	if out := h.s.fm.Output(); out.Mix.ContentDuckDB != -18 {
		t.Fatalf("a converted lease's end released turn focus: %+v", out)
	}
}

// Idle candidates (nothing audible) never duck.
func TestSilentCandidateDoesNotDuck(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	h.s.onCandidate(detector.Candidate{CandidateID: "c1", LeaseID: "L1", ProducingSound: false})
	if out := h.s.fm.Output(); out.Mix.ContentDuckDB != 0 {
		t.Fatalf("idle candidate ducked: %+v", out)
	}
}

// wakeSound pushes the retained config message's wakeSound.
func (h *harness) wakeSound(on bool) {
	h.control(proto.TypeConfig, 0, config.Message{WakeSound: &on})
}

// local_wake_chime: with config wakeSound on, an idle candidate on a
// connected device starts the built-in chime and reports chimed:true; while
// producing sound, with an alert active, during a diagnostic lease, without
// a session or with wakeSound off the chime is left to the controller.
func TestLocalWakeChimeOnlyForIdleConnectedCandidates(t *testing.T) {
	cases := []struct {
		name      string
		setup     func(*harness)
		producing bool
		lost      bool
		chimed    bool
	}{
		{name: "idle", chimed: true},
		{name: "wakeSound off", setup: func(h *harness) { h.wakeSound(false) }},
		{name: "producing sound", producing: true},
		{name: "active alert", setup: func(h *harness) { h.ringTimer("t1") }},
		{name: "diagnostic lease", setup: func(h *harness) { h.up.diagnostic = true }},
		{name: "no session", lost: true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t, opts{})
			h.ready()
			h.wakeSound(true)
			if tc.setup != nil {
				tc.setup(h)
			}
			if tc.lost {
				h.s.Lost(client.LostTimeout)
			}
			h.s.onCandidate(detector.Candidate{CandidateID: "c1", LeaseID: "L1", ProducingSound: tc.producing})
			if earcon := h.mix()&render.MaskEarcon != 0; earcon != tc.chimed {
				t.Fatalf("earcon in the final mix = %v, want %v", earcon, tc.chimed)
			}
			wc := h.sess.of(proto.TypeWakeCandidate)
			switch {
			case tc.lost && len(wc) != 0:
				t.Fatalf("wake.candidate without a session: %v", wc)
			case !tc.lost && (len(wc) != 1 || wc[0].body["chimed"] != tc.chimed):
				t.Fatalf("wake.candidate %v, want chimed:%v", wc, tc.chimed)
			}
			if p := h.sess.of(proto.TypeRenderProgress); len(p) != 0 {
				t.Fatalf("device-local chime reported: %v", p)
			}
		})
	}
}

// The local chime replaces a playing controller earcon (reported cancelled)
// without deadlocking on the mixer's synchronous hooks, is itself never
// reported, and leaves the earcon fence where the controller set it.
func TestLocalWakeChimeIsUnreportedAndUnfenced(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	h.wakeSound(true)
	chime := cue.WakeChime
	start := func(id string) string {
		return h.control(proto.TypeRenderStart, 3, proto.RenderStart{PlaybackID: id, SourceClass: render.Earcon, Format: 1, LocalAsset: &chime})
	}
	h.expectAck(start("e1"), proto.AckAccepted, "")
	h.s.onCandidate(detector.Candidate{CandidateID: "c1", LeaseID: "L1"})
	if wc := h.sess.of(proto.TypeWakeCandidate); len(wc) != 1 || wc[0].body["chimed"] != true {
		t.Fatalf("wake.candidate %v", wc)
	}
	if h.mix()&render.MaskEarcon == 0 {
		t.Fatal("local chime not in the final mix")
	}
	h.expectAck(start("e2"), proto.AckAccepted, "")

	if p := h.sess.of(proto.TypeRenderProgress); len(p) != 0 {
		t.Fatalf("render.progress %v: only the unmixed controller earcons exist", p)
	}
	fin := h.sess.of(proto.TypeRenderFinished)
	if len(fin) != 1 || fin[0].body["playback_id"] != "e1" || fin[0].body["reason"] != string(render.Cancelled) {
		t.Fatalf("render.finished %v, want only e1 cancelled", fin)
	}
}

// WIRE §4.6: a dot release while an alert rings stops it on the device and
// reports handled:"alert_stopped" with the occurrence; both edges carry the
// press's capture position and increasing physical sequence numbers.
func TestDotReleaseStopsRingingAlert(t *testing.T) {
	h := newHarness(t, opts{})
	h.blocks(4)
	h.ready()
	h.ringTimer("t1")

	h.s.DotButton(pkgbuttons.ButtonClickEvent{Button: pkgbuttons.Button{Type: pkgbuttons.DotButton}, ClickType: pkgbuttons.DotClick, Down: true})
	h.now.Add(int64(120 * time.Millisecond))
	h.s.DotButton(pkgbuttons.ButtonClickEvent{Button: pkgbuttons.Button{Type: pkgbuttons.DotButton}, ClickType: pkgbuttons.DotClick, HeldMs: 120})

	acts := h.sess.of(proto.TypeButtonAction)
	if len(acts) != 2 {
		t.Fatalf("button.action %v", acts)
	}
	down, up := acts[0].body, acts[1].body
	if down["handled"] != nil || down["occurrence_id"] != "t1" || down["capture_epoch"] == nil || down["capture_sample"] == nil {
		t.Fatalf("press %v", down)
	}
	if up["handled"] != string(proto.HandledAlertStopped) || up["occurrence_id"] != "t1" {
		t.Fatalf("release %v", up)
	}
	if up["capture_sample"] != down["capture_sample"] || up["capture_epoch"] != down["capture_epoch"] {
		t.Fatalf("release does not carry the press position: %v vs %v", up, down)
	}
	if !(up["physical_seq"].(float64) > down["physical_seq"].(float64)) {
		t.Fatalf("physical_seq not increasing: %v then %v", down["physical_seq"], up["physical_seq"])
	}
	ended := h.sess.of(proto.TypeAlertRingEnded)
	if len(ended) != 1 || ended[0].body["id"] != "t1" || ended[0].body["reason"] != "button" {
		t.Fatalf("alert.ring_ended %v", ended)
	}
	if st := h.s.ex.State(); st.Active != nil {
		t.Fatalf("alert still active: %+v", st.Active)
	}
}

// Without an alert, a release is reported unhandled for the controller's
// gesture policy.
func TestDotReleaseWithoutAlertIsUnhandled(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	h.s.DotButton(pkgbuttons.ButtonClickEvent{Button: pkgbuttons.Button{Type: pkgbuttons.DotButton}, ClickType: pkgbuttons.DotClick})
	acts := h.sess.of(proto.TypeButtonAction)
	if len(acts) != 1 || acts[0].body["handled"] != nil || acts[0].body["occurrence_id"] != nil || acts[0].body["capture_epoch"] != nil {
		t.Fatalf("button.action %v", acts)
	}
}

// §3.2 invariant 10: privacy mute ends every lease and the capture epoch,
// clears the mic and cell rings and drops capture; unmute announces a new
// epoch in privacy.changed before audio resumes.
func TestPrivacyMuteEndsLeasesAndClearsRings(t *testing.T) {
	h := newHarness(t, opts{})
	h.blocks(4)
	h.ready()
	first := epochOf(h.sess.of(proto.TypeStreamOpen)[0])
	h.s.onCandidate(detector.Candidate{CandidateID: "c1", LeaseID: "L1", ProducingSound: true})

	h.phys.MuteToggle()
	if h.up.mutes != 1 {
		t.Fatal("uplink leases not ended")
	}
	if e := h.up.lastEpochs(); e.mic != 0 {
		t.Fatalf("uplink still on capture epoch %d", e.mic)
	}
	if _, ok := h.s.micRing.OldestValid(); ok || h.s.micRing.End() != 0 || h.s.cellRing.End() != 0 {
		t.Fatal("mic/cell rings not cleared")
	}
	if out := h.s.fm.Output(); out.Mix.ContentDuckDB != 0 {
		t.Fatalf("provisional duck survived mute: %+v", out)
	}
	pc := h.sess.of(proto.TypePrivacyChanged)
	if len(pc) != 1 || pc[0].body["muted"] != true || pc[0].body["capture_epoch"] != nil {
		t.Fatalf("privacy.changed %v", pc)
	}
	ends := h.sess.of(proto.TypeStreamEnd)
	if len(ends) != 2 || epochOf(ends[0]) != first || ends[0].body["reason"] != "privacy" {
		t.Fatalf("stream.end %v", ends)
	}
	if hello := h.s.Hello(); !hello.Privacy.Muted || hello.Privacy.CaptureEpoch.Valid {
		t.Fatalf("hello privacy %+v", hello.Privacy)
	}

	h.blocks(2)
	if h.s.micRing.End() != 0 {
		t.Fatal("captured while muted")
	}

	h.phys.MuteToggle()
	pc = h.sess.of(proto.TypePrivacyChanged)
	third := pc[1].body["capture_epoch"]
	if len(pc) != 2 || pc[1].body["muted"] != false || third == nil || third == first {
		t.Fatalf("unmute privacy.changed %v", pc)
	}
	opens := h.sess.of(proto.TypeStreamOpen)
	n := len(opens)
	if epochOf(opens[n-2]) != third || opens[n-2].body["reason"] != "privacy" {
		t.Fatalf("unmute stream.open %v", opens[n-2].body)
	}
	h.blocks(1)
	if len(h.sess.of(proto.TypeStreamOpen)) != n || h.s.micRing.End() != 1280 {
		t.Fatalf("first block after unmute: opens %d ring end %d", len(h.sess.of(proto.TypeStreamOpen)), h.s.micRing.End())
	}
	if _, err := strconv.ParseUint(third.(string), 10, 64); err != nil {
		t.Fatal(err)
	}
}

// uplink.open is refused while muted.
func TestUplinkOpenRefusedWhileMuted(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	h.phys.MuteToggle()
	id := h.control(proto.TypeUplinkOpen, 1, proto.UplinkOpen{LeaseID: "L", Reason: proto.LeaseTurn, Streams: map[proto.StreamID]string{"mic": "live"}, TTLMs: 3000})
	h.expectAck(id, proto.AckRejected, codeMuted)
}

// WIRE §4.2, SPEC §3.2 invariant 3: render commands are fenced by
// generation; cancellation is idempotent and reported with the playback's
// generation in the envelope.
func TestRenderRoutingAndGenerationFencing(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()

	start := func(id string, class render.SourceClass, gen uint32, epoch proto.NullU64, asset *string) string {
		return h.control(proto.TypeRenderStart, gen, proto.RenderStart{PlaybackID: id, SourceClass: class, Epoch: epoch, Format: 1, LocalAsset: asset})
	}
	h.expectAck(start("p5", "content", 5, proto.U64(77), nil), proto.AckAccepted, "")
	h.expectAck(start("p4", "content", 4, proto.U64(78), nil), proto.AckRejected, codeStaleGeneration)
	h.expectAck(start("px", "content", 6, proto.NullU64{}, nil), proto.AckRejected, codeInvalid)
	h.expectAck(start("pq", "mystery", 1, proto.NullU64{}, nil), proto.AckRejected, codeInvalidClass)
	chime, bogus := "builtin:wake_chime", "builtin:nope"
	h.expectAck(start("e1", "earcon", 1, proto.NullU64{}, &chime), proto.AckAccepted, "")
	h.expectAck(start("e2", "earcon", 2, proto.NullU64{}, &bogus), proto.AckRejected, codeUnknownAsset)
	fallback := alerts.SoundFallback
	h.expectAck(start("a1", "alert_preview", 1, proto.NullU64{}, &fallback), proto.AckAccepted, "")

	h.expectAck(h.control(proto.TypeRenderEnd, 4, proto.RenderEnd{PlaybackID: "p5"}), proto.AckRejected, codeUnknownPlayback)

	// A late kind-3 packet of an older generation is dropped.
	pcm := make([]byte, 2*480)
	h.s.RenderAudio(ema.Header{Kind: ema.KindRender, Epoch: 77, Generation: 4, FrameCount: 480}, pcm)

	h.expectAck(h.control(proto.TypeRenderCancel, 5, proto.RenderCancel{PlaybackID: "p5", Reason: "interrupted"}), proto.AckAccepted, "")
	h.expectAck(h.control(proto.TypeRenderCancel, 5, proto.RenderCancel{PlaybackID: "p5", Reason: "interrupted"}), proto.AckAccepted, "")
	var fin []sent
	for _, m := range h.sess.of(proto.TypeRenderFinished) {
		if m.body["playback_id"] == "p5" {
			fin = append(fin, m)
		}
	}
	if len(fin) != 1 || fin[0].gen != 5 || fin[0].body["reason"] != string(render.Cancelled) {
		t.Fatalf("render.finished %v", fin)
	}
}

// §16.5: a foreground occurrence owns the DAC at its own volume and makes the
// wake profile playback; volume buttons then adjust only that occurrence.
func TestAlertFocusRoutesDACAndVolumeButtons(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	vol := 0.5
	h.s.AlertFocus(alerts.AlertFocus{Active: true, ID: "a1", Kind: "alarm", Foreground: true, Volume: &vol})
	if h.hw.DAC() != 64 || h.s.profile() != detector.ProfilePlayback {
		t.Fatalf("DAC %d profile %s", h.hw.DAC(), h.s.profile())
	}
	h.s.VolumeButton(true)
	if h.hw.DAC() != 72 || len(h.sess.of(proto.TypeVolumeState)) != 0 {
		t.Fatalf("occurrence step: DAC %d volume_state %v", h.hw.DAC(), h.sess.of(proto.TypeVolumeState))
	}
	acts := h.sess.of(proto.TypeButtonAction)
	if len(acts) != 1 || acts[0].body["click_type"] != float64(pkgbuttons.VolumeUpClick) || acts[0].body["occurrence_id"] != nil {
		t.Fatalf("volume button.action %v", acts)
	}
	h.s.AlertFocus(alerts.AlertFocus{})
	if h.hw.DAC() != 90 || h.s.profile() != detector.ProfileIdle {
		t.Fatalf("after release: DAC %d profile %s", h.hw.DAC(), h.s.profile())
	}
	h.s.VolumeButton(true)
	if vs := h.sess.of(proto.TypeVolumeState); len(vs) != 1 || vs[0].body["level"] != float64(98) {
		t.Fatalf("media step volume_state %v", vs)
	}
}

// WIRE §4.1 session.hello: the v1 capability set plus retained hardware
// capabilities; alert_cache_v1 only with a working wakelock; ambient_light
// only when readable.
func TestHelloCapabilities(t *testing.T) {
	has := func(caps []proto.Capability, c proto.Capability) bool {
		for _, x := range caps {
			if x == c {
				return true
			}
		}
		return false
	}
	h := newHarness(t, opts{})
	hello := h.s.Hello()
	for _, c := range []proto.Capability{"audio_timeline_v1", "uplink_leases_v1", "device_wake_v1", "render_reference_v1",
		"render_progress_v1", "focus_leases_v1", "alert_cache_v1", "turn_protocol_v1", "leds", "led_anim", "buttons", "button_hold",
		"alert_prefetch"} {
		if !has(hello.Capabilities, c) {
			t.Errorf("missing %s in %v", c, hello.Capabilities)
		}
	}
	if has(hello.Capabilities, "ambient_light") {
		t.Error("ambient_light without a readable sensor")
	}
	if hello.Protocols[0] != 1 || hello.BootID != "boot" || hello.Assets == nil || hello.Privacy.Muted {
		t.Errorf("hello %+v", hello)
	}
	if a := hello.Alerts; a.Wakeup != "ok" {
		t.Errorf("alerts %+v", a)
	}

	h2 := newHarness(t, opts{noWakeLock: true, ambient: true})
	hello = h2.s.Hello()
	if has(hello.Capabilities, "alert_cache_v1") {
		t.Error("alert_cache_v1 announced without a wakelock")
	}
	if a := hello.Alerts; a.Wakeup != "alarm_wakeup_unavailable" {
		t.Errorf("wakeup %q", a.Wakeup)
	}
	if !has(hello.Capabilities, "ambient_light") {
		t.Error("ambient_light missing with a readable sensor")
	}
}

// Session loss clears controller dialog LEDs and every dialog lease; the
// pending local operations and alert state are re-sent after the next ready.
func TestSessionLossEndsLeasesAndReadyResendsAlertState(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	h.expectAck(h.control(proto.TypeFocusAcquire, 3, proto.FocusAcquire{LeaseID: "f1", Owner: "turn", Focus: "dialog_input", TTLMs: 3000}), proto.AckAccepted, "")
	if out := h.s.fm.Output(); out.Mix.ContentDuckDB != -18 {
		t.Fatalf("dialog lease did not duck: %+v", out)
	}
	h.s.Lost(client.LostTimeout)
	if out := h.s.fm.Output(); out.Mix.ContentDuckDB != 0 {
		t.Fatalf("lease survived session loss: %+v", out)
	}
	h.ready()
	if st := h.sess.of(proto.TypeAlertState); len(st) != 1 {
		t.Fatalf("alert.state after ready: %v", h.sess.types())
	}
}

// alert.ring names the timer sound only as the timer finishes, and a missing
// asset rings the fallback (§16.5): alert.prefetch installs it beforehand.
func TestAlertPrefetchInstallsTheTimerSoundBeforeItRings(t *testing.T) {
	h := newHarness(t, opts{})
	h.ready()
	wav, sha := alertWAV(4800)
	h.sess.serve(servedAssets{sha: wav})
	h.expectAck(h.control(proto.TypeAlertPrefetch, 0, proto.AlertPrefetch{Sounds: []string{"builtin:fallback"}}),
		proto.AckRejected, codeInvalid)
	if _, installed := h.s.ex.PreviewPCM(sha); installed {
		t.Fatal("sound installed before the prefetch")
	}
	h.expectAck(h.control(proto.TypeAlertPrefetch, 0, proto.AlertPrefetch{Sounds: []string{sha}}), proto.AckAccepted, "")
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, installed := h.s.ex.PreviewPCM(sha); installed {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("prefetched sound never installed")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// An open/shadow rule list the firmware cannot apply leaves the detector
// unavailable (load_failed) exactly like an unimplemented policy; a valid
// one proceeds to the assets (here missing: missing_asset).
func TestInvalidOpenRulesLeaveTheDetectorUnavailable(t *testing.T) {
	baseline := []proto.OpenRule{
		{Profile: "idle", Windows: 3, Combine: "mean", Threshold: 0.9},
		{Profile: "playback", Windows: 3, Combine: "mean", Threshold: 0.65},
	}
	for _, tc := range []struct {
		name   string
		open   []proto.OpenRule
		shadow []proto.OpenRule
		want   detector.UnavailableReason
	}{
		{"absent", nil, nil, detector.UnavailableMissingAsset},
		{"baseline plus shadow", baseline, []proto.OpenRule{{Profile: "idle", Windows: 2, Combine: "all", Threshold: 0.95}}, detector.UnavailableMissingAsset},
		{"empty open list", []proto.OpenRule{}, nil, detector.UnavailableLoadFailed},
		{"no playback rule", baseline[:1], nil, detector.UnavailableLoadFailed},
		{"four windows", append([]proto.OpenRule{{Profile: "idle", Windows: 4, Combine: "mean", Threshold: 0.9}}, baseline...), nil, detector.UnavailableLoadFailed},
		{"bad shadow combine", baseline, []proto.OpenRule{{Profile: "idle", Windows: 2, Combine: "max", Threshold: 0.9}}, detector.UnavailableLoadFailed},
	} {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t, opts{})
			h.readyWith(func(p *proto.DetectorPolicy) { p.OpenRules, p.ShadowRules = tc.open, tc.shadow })
			deadline := time.Now().Add(5 * time.Second)
			for {
				h.s.det.FlushStats()
				st := h.sess.of(proto.TypeWakeStats)
				if len(st) > 0 {
					if got, _ := st[len(st)-1].body["wake_unavailable"].(string); got == string(tc.want) {
						return
					}
				}
				if time.Now().After(deadline) {
					t.Fatalf("wake.stats %v, want wake_unavailable=%s", st, tc.want)
				}
				time.Sleep(10 * time.Millisecond)
			}
		})
	}
}
