package uplink

import (
	"encoding/binary"
	"errors"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/audio/ring"
	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

const (
	micEpoch = 0x1111
	refEpoch = 0x2222
	block    = 1280
	sampleNs = 62500
)

type sentMsg struct {
	typ  proto.MessageType
	gen  uint32
	body proto.UplinkEnded
}

type fakeSink struct {
	full   bool
	frames [][]byte
}

func (s *fakeSink) SendFrame(frame []byte, _ int64) error {
	if s.full {
		return client.ErrBackpressure
	}
	s.frames = append(s.frames, append([]byte(nil), frame...))
	return nil
}

type harness struct {
	t     *testing.T
	now   int64
	mic   *ring.Ring[int16]
	ref   *ring.Ring[int16]
	cells *ring.Ring[ema.Cell]
	sink  *fakeSink
	sent  []sentMsg
	e     *Executor
}

func newHarness(t *testing.T) *harness {
	h := &harness{t: t, mic: ring.NewMic(), ref: ring.NewReference(), cells: ring.NewCells(), sink: &fakeSink{}}
	h.e = New(Rings{Mic: h.mic, Ref: h.ref, Cells: h.cells}, h, func() int64 { return h.now })
	h.e.SetEpochs(micEpoch, refEpoch)
	h.e.Attach(h.sink, h.send)
	return h
}

// CaptureToReference: the test's reference epoch is aligned with capture.
func (h *harness) CaptureToReference(c uint64) (uint64, bool) { return c, true }

func (h *harness) send(typ proto.MessageType, gen uint32, body any) (string, error) {
	h.sent = append(h.sent, sentMsg{typ: typ, gen: gen, body: body.(proto.UplinkEnded)})
	return "id", nil
}

// appendMic appends n capture blocks and the cells they complete. Sample
// values encode their index so tests can check nothing moved.
func (h *harness) appendMic(n int) {
	for range n {
		first := h.mic.End()
		pcm := make([]int16, block)
		for i := range pcm {
			pcm[i] = int16((first+uint64(i))%30000 + 1)
		}
		must(h.t, h.mic.Append(first, pcm, ring.Meta{MonoNs: int64(first) * sampleNs, Flags: ema.FlagEstimated}))
		for k := h.cells.End(); k < h.mic.End()/ema.CellSamples; k++ {
			must(h.t, h.cells.Append(k, []ema.Cell{{E: -1000}}, ring.Meta{MonoNs: int64(k) * 512 * sampleNs}))
		}
	}
}

// appendRef appends n reference samples of value v in 160-sample blocks.
func (h *harness) appendRef(n int, v int16) {
	for range n / 160 {
		first := h.ref.End()
		pcm := make([]int16, 160)
		for i := range pcm {
			pcm[i] = v
		}
		must(h.t, h.ref.Append(first, pcm, ring.Meta{MonoNs: int64(first) * sampleNs, Mask: ema.SourceContent}))
	}
}

func (h *harness) open(id string, gen uint32, streams map[proto.StreamID]string) error {
	return h.e.Open(proto.Envelope{Generation: gen}, proto.UplinkOpen{
		LeaseID: id, Owner: "turn-" + id, Reason: proto.LeaseTurn, Streams: streams, TTLMs: 3000})
}

// frames decodes and drains everything the sink received.
func (h *harness) frames() []ema.Header {
	var out []ema.Header
	for _, f := range h.sink.frames {
		hd, _, err := ema.DecodeFrame(f)
		must(h.t, err)
		out = append(out, hd)
	}
	h.sink.frames = nil
	return out
}

func (h *harness) ended() []sentMsg {
	out := h.sent
	h.sent = nil
	return out
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func byKind(hs []ema.Header, k ema.Kind) []ema.Header {
	var out []ema.Header
	for _, h := range hs {
		if h.Kind == k {
			out = append(out, h)
		}
	}
	return out
}

// span checks the packets of one stream are contiguous, sequenced and within
// the frame limit, and returns the covered header-domain range.
func span(t *testing.T, hs []ema.Header, maxFrames uint32) (first, end uint64) {
	t.Helper()
	if len(hs) == 0 {
		t.Fatal("no packets")
	}
	unit := uint64(1)
	if hs[0].Kind == ema.KindCells {
		unit = ema.CellSamples
	}
	first, end = hs[0].FirstSample, hs[0].FirstSample
	for i, h := range hs {
		if h.FirstSample != end {
			t.Fatalf("packet %d starts at %d, want %d", i, h.FirstSample, end)
		}
		if i > 0 && h.Sequence != hs[i-1].Sequence+1 {
			t.Fatalf("packet %d sequence %d after %d", i, h.Sequence, hs[i-1].Sequence)
		}
		if h.FrameCount > maxFrames {
			t.Fatalf("packet %d carries %d frames", i, h.FrameCount)
		}
		end += uint64(h.FrameCount) * unit
	}
	return first, end
}

func u64(v uint64) proto.NullU64 { return proto.U64(v) }

func TestOpenRoundsStartsToGridsAndClipsToRing(t *testing.T) {
	h := newHarness(t)
	h.appendMic(100) // 128,000 samples: the 6 s mic ring now starts at 32,000
	h.appendRef(160000, 7)
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "30000", "cells": "30000", "reference": "41000"}))
	h.e.step()
	fs := h.frames()

	micFirst, micEnd := span(t, byKind(fs, ema.KindMic), ema.MaxUplinkPCMFrames)
	if micFirst != 32256 || micEnd != 128000 { // 30000 → 29696, clipped to the first 512 boundary ≥ 32000
		t.Fatalf("mic backfill [%d, %d)", micFirst, micEnd)
	}
	cellFirst, cellEnd := span(t, byKind(fs, ema.KindCells), ema.MaxCells)
	if cellFirst != 29696 || cellEnd != 128000 {
		t.Fatalf("cells backfill [%d, %d)", cellFirst, cellEnd)
	}
	refFirst, refEnd := span(t, byKind(fs, ema.KindReference), ema.MaxUplinkPCMFrames)
	if refFirst != 40960 || refEnd != 160000 { // 41000 rounded down to a 2,560 hop
		t.Fatalf("reference backfill [%d, %d)", refFirst, refEnd)
	}

	if _, err := h.e.Close(proto.Envelope{Generation: 1}, proto.UplinkClose{LeaseID: "L", Reason: proto.CloseCommitted}); err != nil {
		t.Fatal(err)
	}
	msgs := h.ended()
	if len(msgs) != 1 || msgs[0].gen != 1 {
		t.Fatalf("ended messages %+v", msgs)
	}
	b := msgs[0].body
	if b.Reason != proto.EndedClosed ||
		b.ClippedStart["mic"] != u64(32256) || b.ClippedStart["cells"].Valid || b.ClippedStart["reference"].Valid ||
		b.LastSample["mic"] != u64(127999) || b.LastSample["cells"] != u64(127999) || b.LastSample["reference"] != u64(159999) {
		t.Fatalf("uplink.ended %+v", b)
	}
}

func TestReferenceStartOutsideClockFitIsClipped(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	h.appendRef(12800, 7)
	// A start the clock fit cannot map starts at the oldest valid hop.
	h.e.clock = unmapped{}
	must(t, h.open("L", 1, map[proto.StreamID]string{"reference": "4000"}))
	h.e.step()
	if got := byKind(h.frames(), ema.KindReference); len(got) == 0 || got[0].FirstSample != 0 {
		t.Fatalf("reference packets %+v", got)
	}
	h.e.Close(proto.Envelope{Generation: 1}, proto.UplinkClose{LeaseID: "L"})
	if b := h.ended()[0].body; b.ClippedStart["reference"] != u64(0) {
		t.Fatalf("clipped_start %+v", b.ClippedStart)
	}
}

type unmapped struct{}

func (unmapped) CaptureToReference(uint64) (uint64, bool) { return 0, false }

func TestCandidateBackfillPrecedesLiveInStreamOrder(t *testing.T) {
	h := newHarness(t)
	h.appendMic(40) // 51,200 samples
	h.appendRef(51200, 7)
	h.e.OpenCandidate("C", 30000)
	h.e.step()
	if fs := h.frames(); len(fs) != 0 {
		t.Fatalf("%d packets before the candidate was accepted", len(fs))
	}

	h.e.AcceptCandidate("C", true)
	h.e.step()
	fs := h.frames()
	// Backfill: every mic packet, then every cell packet, then reference.
	var kinds []ema.Kind
	for _, f := range fs {
		if len(kinds) == 0 || kinds[len(kinds)-1] != f.Kind {
			kinds = append(kinds, f.Kind)
		}
	}
	if len(kinds) != 3 || kinds[0] != ema.KindMic || kinds[1] != ema.KindCells || kinds[2] != ema.KindReference {
		t.Fatalf("stream order %v", kinds)
	}
	mic := byKind(fs, ema.KindMic)
	// support_start − 4,800 = 25,200, rounded down to 25,088.
	if first, end := span(t, mic, ema.MaxUplinkPCMFrames); first != 25088 || end != 51200 {
		t.Fatalf("mic backfill [%d, %d)", first, end)
	}
	for _, m := range mic { // original timestamps, not send time
		if m.MonoNs != m.FirstSample*sampleNs {
			t.Fatalf("mic packet at %d stamped %d", m.FirstSample, m.MonoNs)
		}
	}
	if first, end := span(t, byKind(fs, ema.KindCells), ema.MaxCells); first != 0 || end != 51200 {
		t.Fatalf("cells backfill [%d, %d)", first, end)
	}
	if first, end := span(t, byKind(fs, ema.KindReference), ema.MaxUplinkPCMFrames); first != 0 || end != 51200 {
		t.Fatalf("reference backfill [%d, %d)", first, end)
	}

	// Live audio continues each stream where its backfill ended.
	h.appendMic(1)
	h.appendRef(block, 7)
	h.e.step()
	live := h.frames()
	if len(live) != 3 || live[0].Kind != ema.KindMic || live[0].FirstSample != 51200 ||
		live[1].Kind != ema.KindCells || live[1].FirstSample != 51200 || live[1].FrameCount != 2 ||
		live[2].Kind != ema.KindReference || live[2].FirstSample != 51200 {
		t.Fatalf("live packets %+v", live)
	}
	if live[0].Sequence != mic[len(mic)-1].Sequence+1 {
		t.Fatalf("live sequence %d after %d", live[0].Sequence, mic[len(mic)-1].Sequence)
	}
}

func TestShortLiveReferenceWaitsOneBlock(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	must(t, h.open("L", 1, map[proto.StreamID]string{"reference": "live"}))
	h.e.step()
	h.appendRef(640, 7)
	h.e.step()
	if fs := h.frames(); len(fs) != 0 {
		t.Fatalf("sent %d packets before 1,280 samples or 80 ms", len(fs))
	}
	h.now += int64(80 * time.Millisecond)
	h.e.step()
	if fs := h.frames(); len(fs) != 1 || fs[0].FrameCount != 640 {
		t.Fatalf("flushed %+v", fs)
	}
}

func TestGapAdvancesIndicesWithDiscontinuity(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	h.mic.Missing(2 * block)
	h.appendMic(1)
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "0"}))
	h.e.step()
	fs := h.frames()
	if len(fs) != 2 {
		t.Fatalf("%d packets", len(fs))
	}
	if fs[0].FirstSample != 0 || fs[0].Flags&ema.FlagDiscontinuity != 0 {
		t.Fatalf("first packet %+v", fs[0])
	}
	if fs[1].FirstSample != 2*block || fs[1].FrameCount != block || fs[1].Flags&ema.FlagDiscontinuity == 0 ||
		fs[1].Sequence != fs[0].Sequence+1 {
		t.Fatalf("packet after the gap %+v", fs[1])
	}

	// A live gap behaves the same way.
	h.mic.Missing(5 * block)
	h.appendMic(1)
	h.e.step()
	fs = h.frames()
	if len(fs) != 1 || fs[0].FirstSample != 5*block || fs[0].Flags&ema.FlagDiscontinuity == 0 {
		t.Fatalf("live packet after the gap %+v", fs)
	}
}

func TestReferenceDigitalSilenceHasNoPayload(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	h.appendRef(block, 0)
	h.appendRef(block, 3)
	must(t, h.open("L", 1, map[proto.StreamID]string{"reference": "0"}))
	h.e.step()
	raw := h.sink.frames
	if len(raw) != 2 {
		t.Fatalf("%d packets", len(raw))
	}
	silent, _, err := ema.DecodeFrame(raw[0])
	must(t, err)
	if silent.Flags&ema.FlagDigitalSilence == 0 || silent.PayloadBytes != 0 || len(raw[0]) != ema.HeaderSize ||
		silent.FrameCount != block || silent.SourceMask != uint32(ema.SourceContent) {
		t.Fatalf("silent packet %+v (%d bytes)", silent, len(raw[0]))
	}
	loud, payload, err := ema.DecodeFrame(raw[1])
	must(t, err)
	if loud.Flags&ema.FlagDigitalSilence != 0 || len(payload) != 2*block || int16(binary.LittleEndian.Uint16(payload)) != 3 {
		t.Fatalf("audible packet %+v", loud)
	}
}

func TestCandidateWithoutAckEndsWithTTL(t *testing.T) {
	h := newHarness(t)
	h.appendMic(10)
	h.e.OpenCandidate("C", 10000)
	h.now += int64(CandidateAckWait) - 1
	h.e.step()
	if msgs := h.ended(); len(msgs) != 0 {
		t.Fatalf("ended early: %+v", msgs)
	}
	h.now++
	h.e.step()
	msgs := h.ended()
	if len(msgs) != 1 || msgs[0].gen != 1 || msgs[0].body.Reason != proto.EndedTTL ||
		msgs[0].body.LastSample["mic"].Valid || len(h.frames()) != 0 {
		t.Fatalf("ended %+v", msgs)
	}
	h.e.AcceptCandidate("C", true) // too late: no lease
	h.e.step()
	if len(h.frames()) != 0 {
		t.Fatal("audio for an expired candidate lease")
	}

	h.e.OpenCandidate("D", 10000)
	h.e.AcceptCandidate("D", false)
	if msgs := h.ended(); len(msgs) != 1 || msgs[0].body.Reason != proto.EndedClosed {
		t.Fatalf("rejected candidate %+v", msgs)
	}
}

func TestCandidateConvertsToTurnLease(t *testing.T) {
	h := newHarness(t)
	h.appendMic(10)
	h.e.OpenCandidate("C", 10000)
	renew := func(gen uint32, reason proto.LeaseReason, owner string) (bool, error) {
		return h.e.Renew(proto.Envelope{Generation: gen}, proto.UplinkRenew{LeaseID: "C", TTLMs: 3000, Reason: reason, Owner: owner})
	}
	if _, err := renew(2, proto.LeaseTurn, "T"); !errors.Is(err, ErrInvalid) {
		t.Fatalf("converting an unacknowledged candidate: %v", err)
	}
	h.e.AcceptCandidate("C", true)
	if _, err := renew(1, proto.LeaseTurn, "T"); !errors.Is(err, ErrInvalid) {
		t.Fatalf("conversion without generation + 1: %v", err)
	}
	if converted, err := renew(2, proto.LeaseTurn, "T"); err != nil || !converted {
		t.Fatalf("conversion: %v %v", converted, err)
	}
	if _, err := renew(1, "", ""); !errors.Is(err, ErrStaleGeneration) {
		t.Fatalf("stale renew: %v", err)
	}
	if converted, err := renew(2, "", ""); err != nil || converted {
		t.Fatalf("renew: %v %v", converted, err)
	}
	if _, err := h.e.Close(proto.Envelope{Generation: 1}, proto.UplinkClose{LeaseID: "C"}); !errors.Is(err, ErrStaleGeneration) {
		t.Fatalf("stale close: %v", err)
	}
	h.e.step()
	candidate, err := h.e.Close(proto.Envelope{Generation: 2}, proto.UplinkClose{LeaseID: "C", Reason: proto.CloseCommitted})
	if err != nil || candidate {
		t.Fatalf("close: %v %v", candidate, err)
	}
	msgs := h.ended()
	if len(msgs) != 1 || msgs[0].gen != 2 || msgs[0].body.LastSample["mic"] != u64(12799) {
		t.Fatalf("ended %+v", msgs)
	}
	if _, err := renew(2, "", ""); !errors.Is(err, ErrUnknownLease) {
		t.Fatalf("renew after close: %v", err)
	}

	h.e.OpenCandidate("E", 10000)
	if candidate, err := h.e.Close(proto.Envelope{Generation: 1}, proto.UplinkClose{LeaseID: "E", Reason: proto.CloseRejected}); err != nil || !candidate {
		t.Fatalf("closing a candidate lease: %v %v", candidate, err)
	}
}

func TestLeaseExpiresUnlessRenewed(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "live"}))
	h.now += int64(2500 * time.Millisecond)
	if _, err := h.e.Renew(proto.Envelope{Generation: 1}, proto.UplinkRenew{LeaseID: "L", TTLMs: 3000}); err != nil {
		t.Fatal(err)
	}
	h.now += int64(2999 * time.Millisecond)
	h.e.step()
	if msgs := h.ended(); len(msgs) != 0 {
		t.Fatalf("renewed lease ended: %+v", msgs)
	}
	h.now += int64(time.Millisecond)
	h.e.step()
	if msgs := h.ended(); len(msgs) != 1 || msgs[0].body.Reason != proto.EndedTTL {
		t.Fatalf("ended %+v", msgs)
	}
}

func TestLiveAudioQueuedOverOneSecondEndsLeasesWantingIt(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	must(t, h.open("M", 1, map[proto.StreamID]string{"mic": "live"}))
	must(t, h.open("R", 1, map[proto.StreamID]string{"reference": "live"}))
	h.sink.full = true
	h.appendMic(1)
	h.e.step() // the block is queued at t = 0
	for range 12 {
		h.now += int64(80 * time.Millisecond)
		h.appendMic(1)
		h.e.step()
	}
	h.now = int64(OverrunAge)
	h.e.step()
	if msgs := h.ended(); len(msgs) != 0 {
		t.Fatalf("ended at exactly 1,000 ms: %+v", msgs)
	}
	h.now++
	h.e.step()
	msgs := h.ended()
	if len(msgs) != 1 || msgs[0].body.LeaseID != "M" || msgs[0].body.Reason != proto.EndedOverrun {
		t.Fatalf("ended %+v", msgs)
	}
	if _, err := h.e.Renew(proto.Envelope{Generation: 1}, proto.UplinkRenew{LeaseID: "R", TTLMs: 3000}); err != nil {
		t.Fatalf("reference lease: %v", err)
	}
}

func TestMuteEndsLeasesAndBlocksOpenUntilNewEpoch(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "live", "cells": "live"}))
	h.e.step()
	h.appendMic(1)
	h.e.step()
	h.frames()
	h.e.Mute()
	msgs := h.ended()
	if len(msgs) != 1 || msgs[0].body.Reason != proto.EndedMute ||
		msgs[0].body.LastSample["mic"] != u64(2559) || msgs[0].body.ClippedStart["mic"].Valid {
		t.Fatalf("ended %+v", msgs)
	}
	if err := h.open("N", 1, map[proto.StreamID]string{"mic": "live"}); !errors.Is(err, ErrInvalid) {
		t.Fatalf("open while muted: %v", err)
	}
	h.mic.Reset(0)
	h.cells.Reset(0)
	h.e.SetEpochs(0x3333, refEpoch)
	must(t, h.open("N", 1, map[proto.StreamID]string{"mic": "live"}))
	h.appendMic(1)
	h.e.step()
	if fs := h.frames(); len(fs) != 1 || fs[0].Epoch != 0x3333 || fs[0].Sequence != 0 || fs[0].FirstSample != 0 {
		t.Fatalf("new epoch packets %+v", fs)
	}
}

func TestEpochChanges(t *testing.T) {
	h := newHarness(t)
	h.appendMic(1)
	h.appendRef(block, 5)
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "live", "reference": "live"}))
	h.e.step()
	h.frames()

	// A new render epoch restarts the reference stream; the lease goes on.
	h.ref.Reset(0)
	h.e.SetEpochs(micEpoch, 0x4444)
	h.appendRef(block, 5)
	h.e.step()
	if msgs := h.ended(); len(msgs) != 0 {
		t.Fatalf("reference epoch ended leases: %+v", msgs)
	}
	fs := byKind(h.frames(), ema.KindReference)
	if len(fs) != 1 || fs[0].Epoch != 0x4444 || fs[0].Sequence != 0 || fs[0].FirstSample != 0 {
		t.Fatalf("reference after its epoch change %+v", fs)
	}

	// A new capture epoch ends every lease.
	h.mic.Reset(0)
	h.cells.Reset(0)
	h.e.SetEpochs(0x5555, 0x4444)
	msgs := h.ended()
	if len(msgs) != 1 || msgs[0].body.Reason != proto.EndedEpoch || msgs[0].body.LastSample["reference"] != u64(block-1) {
		t.Fatalf("ended %+v", msgs)
	}
}

func TestSessionLossDiscardsLeasesAndAudio(t *testing.T) {
	h := newHarness(t)
	h.appendMic(3)
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "0"}))
	h.e.Detach()
	sink := &fakeSink{}
	h.e.Attach(sink, h.send)
	h.appendMic(1)
	h.e.step()
	if len(sink.frames) != 0 || len(h.ended()) != 0 {
		t.Fatalf("after session loss: %d packets, messages %+v", len(sink.frames), h.sent)
	}
	if _, err := h.e.Renew(proto.Envelope{Generation: 1}, proto.UplinkRenew{LeaseID: "L"}); !errors.Is(err, ErrUnknownLease) {
		t.Fatalf("lease survived session loss: %v", err)
	}
}

func TestPacketingDoesNotAllocate(t *testing.T) {
	h := newHarness(t)
	h.appendMic(60)
	h.appendRef(76800, 3)
	h.sink.full = true
	must(t, h.open("L", 1, map[proto.StreamID]string{"mic": "0", "cells": "0", "reference": "0"}))
	allocs := testing.AllocsPerRun(20, func() {
		if _, _, ok := h.e.packLocked(h.now); !ok {
			t.Fatal("no packet")
		}
		h.e.commitLocked()
	})
	if allocs != 0 {
		t.Fatalf("%.1f allocations per packet", allocs)
	}
}
