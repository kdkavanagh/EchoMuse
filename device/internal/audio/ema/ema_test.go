package ema

import (
	"encoding/binary"
	"errors"
	"testing"
)

func validHeaders() []Header {
	mic := NewHeader(KindMic, FlagEstimated|FlagDiscontinuity, 0xdeadbeef, 7, 1280*9, 1280)
	mic.MonoNs = 123_456_789
	mic.UncertaintyUs = 80_000
	ref := NewHeader(KindReference, FlagDigitalSilence, 42, 0, 2560, 1280)
	ref.SourceMask = uint32(SourceContent | SourceEarcon)
	refPCM := NewHeader(KindReference, 0, 42, 1, 3840, 160)
	refPCM.SourceMask = uint32(SourceAlert)
	render := NewHeader(KindRender, FlagUnderrun, 99, 3, 0, 3840)
	render.Generation = 17
	render.UncertaintyUs = UncertaintyUnknown
	cells := NewHeader(KindCells, 0, 0xdeadbeef, 2, 512*40, 320)
	return []Header{mic, ref, refPCM, render, cells}
}

func TestHeaderRoundTrip(t *testing.T) {
	for _, h := range validHeaders() {
		msg := make([]byte, HeaderSize+int(h.PayloadBytes))
		if err := h.Encode(msg); err != nil {
			t.Fatalf("encode kind %d: %v", h.Kind, err)
		}
		got, payload, err := DecodeFrame(msg)
		if err != nil {
			t.Fatalf("decode kind %d: %v", h.Kind, err)
		}
		if got != h {
			t.Fatalf("round trip kind %d:\n got %+v\nwant %+v", h.Kind, got, h)
		}
		if len(payload) != int(h.PayloadBytes) {
			t.Fatalf("payload %d bytes, want %d", len(payload), h.PayloadBytes)
		}
	}
}

func TestHeaderLayout(t *testing.T) {
	h := validHeaders()[0]
	buf := make([]byte, HeaderSize)
	if err := h.Encode(buf); err != nil {
		t.Fatal(err)
	}
	le := binary.LittleEndian
	if string(buf[0:4]) != "EMA1" || buf[4] != 1 || buf[5] != FlagEstimated|FlagDiscontinuity || buf[6] != 1 || buf[7] != 1 {
		t.Fatalf("leading bytes %v", buf[:8])
	}
	if le.Uint64(buf[8:]) != 0xdeadbeef || le.Uint64(buf[16:]) != 7 || le.Uint64(buf[24:]) != 11520 ||
		le.Uint64(buf[32:]) != 123_456_789 || le.Uint32(buf[40:]) != 80_000 || le.Uint32(buf[44:]) != 16000 ||
		le.Uint32(buf[48:]) != 1280 || le.Uint32(buf[60:]) != 2560 {
		t.Fatalf("field offsets wrong: % x", buf)
	}
}

func TestHeaderValidation(t *testing.T) {
	base := func(k Kind) Header {
		for _, h := range validHeaders() {
			if h.Kind == k {
				return h
			}
		}
		panic("no kind")
	}
	cases := []struct {
		name string
		mut  func() Header
		want error
	}{
		{"kind zero", func() Header { h := base(KindMic); h.Kind = 0; return h }, ErrKind},
		{"kind five", func() Header { h := base(KindMic); h.Kind = 5; return h }, ErrKind},
		{"reserved flag", func() Header { h := base(KindMic); h.Flags |= 1 << 5; return h }, ErrFlags},
		{"silence on mic", func() Header {
			h := base(KindMic)
			h.Flags |= FlagDigitalSilence
			h.PayloadBytes = 0
			return h
		}, ErrFlags},
		{"stereo", func() Header { h := base(KindMic); h.Channels = 2; return h }, ErrChannels},
		{"cells as pcm", func() Header { h := base(KindCells); h.Format = FormatPCM16; return h }, ErrFormat},
		{"mic as cells", func() Header { h := base(KindMic); h.Format = FormatCellV1; return h }, ErrFormat},
		{"zero epoch", func() Header { h := base(KindMic); h.Epoch = 0; return h }, ErrEpoch},
		{"mic at 48k", func() Header { h := base(KindMic); h.SampleRate = RateRender; return h }, ErrRate},
		{"render at 16k", func() Header { h := base(KindRender); h.SampleRate = RateCapture; return h }, ErrRate},
		{"zero frames", func() Header { h := base(KindMic); h.FrameCount = 0; h.PayloadBytes = 0; return h }, ErrFrameCount},
		{"mic 1281", func() Header { h := base(KindMic); h.FrameCount = 1281; h.PayloadBytes = 2562; return h }, ErrFrameCount},
		{"render 3841", func() Header { h := base(KindRender); h.FrameCount = 3841; h.PayloadBytes = 7682; return h }, ErrFrameCount},
		{"cells 321", func() Header { h := base(KindCells); h.FrameCount = 321; h.PayloadBytes = 1284; return h }, ErrFrameCount},
		{"generation on mic", func() Header { h := base(KindMic); h.Generation = 1; return h }, ErrGeneration},
		{"mask on mic", func() Header { h := base(KindMic); h.SourceMask = 1; return h }, ErrSourceMask},
		{"unknown source", func() Header { h := base(KindReference); h.SourceMask = 16; return h }, ErrSourceMask},
		{"unaligned cells", func() Header { h := base(KindCells); h.FirstSample = 513; return h }, ErrCellAlignment},
		{"pcm payload", func() Header { h := base(KindMic); h.PayloadBytes = 1280; return h }, ErrPayloadLength},
		{"silence with payload", func() Header { h := base(KindReference); h.PayloadBytes = 2560; return h }, ErrPayloadLength},
		{"cell payload", func() Header { h := base(KindCells); h.PayloadBytes = 640; return h }, ErrPayloadLength},
	}
	for _, c := range cases {
		h := c.mut()
		buf := make([]byte, HeaderSize)
		if err := h.Encode(buf); !errors.Is(err, c.want) {
			t.Errorf("%s: encode err %v, want %v", c.name, err, c.want)
		}
		// Decode must reject the same bytes: bypass Encode's check.
		writeRaw(buf, h)
		if _, err := Decode(buf); !errors.Is(err, c.want) {
			t.Errorf("%s: decode err %v, want %v", c.name, err, c.want)
		}
	}
}

func writeRaw(dst []byte, h Header) {
	le := binary.LittleEndian
	copy(dst, Magic[:])
	dst[4], dst[5], dst[6], dst[7] = byte(h.Kind), h.Flags, h.Channels, byte(h.Format)
	le.PutUint64(dst[8:], h.Epoch)
	le.PutUint64(dst[16:], h.Sequence)
	le.PutUint64(dst[24:], h.FirstSample)
	le.PutUint64(dst[32:], h.MonoNs)
	le.PutUint32(dst[40:], h.UncertaintyUs)
	le.PutUint32(dst[44:], h.SampleRate)
	le.PutUint32(dst[48:], h.FrameCount)
	le.PutUint32(dst[52:], h.Generation)
	le.PutUint32(dst[56:], h.SourceMask)
	le.PutUint32(dst[60:], h.PayloadBytes)
}

func TestDecodeFraming(t *testing.T) {
	h := validHeaders()[0]
	msg := make([]byte, HeaderSize+int(h.PayloadBytes))
	if err := h.Encode(msg); err != nil {
		t.Fatal(err)
	}
	if _, err := Decode(msg[:HeaderSize-1]); !errors.Is(err, ErrShort) {
		t.Errorf("short: %v", err)
	}
	if _, _, err := DecodeFrame(msg[:len(msg)-2]); !errors.Is(err, ErrPayloadLength) {
		t.Errorf("truncated payload: %v", err)
	}
	if _, _, err := DecodeFrame(append(msg, 0, 0)); !errors.Is(err, ErrPayloadLength) {
		t.Errorf("trailing bytes: %v", err)
	}
	bad := append([]byte(nil), msg...)
	bad[3] = '2'
	if _, err := Decode(bad); !errors.Is(err, ErrMagic) {
		t.Errorf("magic: %v", err)
	}
	if err := h.Encode(make([]byte, HeaderSize-1)); !errors.Is(err, ErrShort) {
		t.Errorf("encode short: %v", err)
	}
}

func TestPCMRoundTrip(t *testing.T) {
	in := []int16{0, 1, -1, 32767, -32768, 1234}
	buf := make([]byte, 2*len(in))
	PutPCM(buf, in)
	if buf[6] != 0xff || buf[7] != 0x7f {
		t.Fatalf("not little-endian: % x", buf)
	}
	out := make([]int16, len(in))
	PCM(out, buf)
	for i := range in {
		if in[i] != out[i] {
			t.Fatalf("sample %d: %d != %d", i, out[i], in[i])
		}
	}
}

func TestCellRoundTrip(t *testing.T) {
	for _, c := range []Cell{
		{E: -4321, Flags: CellMuted, Mask: SourceContent | SourceDialog},
		{E: 0},
		GapCell(0, SourceAlert),
	} {
		var b [CellRecordSize]byte
		if err := c.Encode(b[:]); err != nil {
			t.Fatal(err)
		}
		got, err := DecodeCell(b[:])
		if err != nil || got != c {
			t.Fatalf("cell %+v: got %+v, %v", c, got, err)
		}
	}
	var b [CellRecordSize]byte
	if err := (Cell{E: -20000}).Encode(b[:]); err != nil {
		t.Fatal(err)
	}
	if got, _ := DecodeCell(b[:]); got.E != CellEMin {
		t.Fatalf("encode clamp: %d", got.E)
	}
	if err := (Cell{}).Encode(b[:3]); !errors.Is(err, ErrCellShort) {
		t.Fatalf("encode short: %v", err)
	}
}

func TestCellValidation(t *testing.T) {
	cases := []struct {
		raw  [4]byte
		want error
	}{
		{[4]byte{0x01, 0x00, 0, 0}, ErrCellE},              // +0.01 dB
		{[4]byte{0x1f, 0xd1, 0, 0}, ErrCellE},              // -12001
		{[4]byte{0x00, 0x00, 4, 0}, ErrCellFlags},          // reserved flag
		{[4]byte{0x00, 0x00, 0, 16}, ErrCellMask},          // unknown source
		{[4]byte{0x00, 0x00, CellGap, 0}, ErrCellGapE},     // gap with E=0
		{[4]byte{0x20, 0xd1, CellGap | CellMuted, 1}, nil}, // -12000 gap
	}
	for _, c := range cases {
		if _, err := ParseCell(c.raw[:]); !errors.Is(err, c.want) {
			t.Errorf("% x: %v, want %v", c.raw, err, c.want)
		}
	}
	if _, err := ParseCell([]byte{0, 0, 0}); !errors.Is(err, ErrCellShort) {
		t.Errorf("short: %v", err)
	}
}

func TestCellE(t *testing.T) {
	cases := map[float64]int16{
		-3.0103: -301, -3.0149: -301, -3.015: -302, 0.4: 0, -120: -12000, -150: -12000,
	}
	for db, want := range cases {
		if got := CellE(db); got != want {
			t.Errorf("CellE(%v) = %d, want %d", db, got, want)
		}
	}
}
