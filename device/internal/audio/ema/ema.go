// Package ema encodes and validates EMA1 audio frames (WIRE §3, SPEC §16.1):
// a fixed 64-byte little-endian header followed by PCM16 samples, cell
// records or AFE records. Encode and Decode work in caller buffers and never
// allocate.
package ema

import (
	"crypto/rand"
	"encoding/binary"
	"errors"
)

// HeaderSize is the fixed EMA1 header length in bytes.
const HeaderSize = 64

// Magic is the ASCII tag at offset 0.
var Magic = [4]byte{'E', 'M', 'A', '1'}

// Kind names what a frame carries (§16.1 offset 4).
type Kind uint8

const (
	KindMic       Kind = 1 // canonical capture PCM
	KindReference Kind = 2 // 16 kHz decimated final mix
	KindRender    Kind = 3 // downlink render-source PCM
	KindCells     Kind = 4 // cell records v1
	KindAFE       Kind = 5 // AFE records v1 (afe_metadata_v1 sessions only)
)

// Format is the payload encoding (§16.1 offset 7).
type Format uint8

const (
	FormatPCM16  Format = 1 // signed PCM16 little-endian, kinds 1–3
	FormatCellV1 Format = 2 // 4-byte cell records, kind 4
	FormatAFEV1  Format = 3 // 14-byte AFE records, kind 5
)

// Header flag bits (§16.1 offset 5).
const (
	FlagDiscontinuity  uint8 = 1 << 0
	FlagMuted          uint8 = 1 << 1
	FlagUnderrun       uint8 = 1 << 2
	FlagEstimated      uint8 = 1 << 3
	FlagDigitalSilence uint8 = 1 << 4 // kind 2 only; payload omitted
	flagsKnown               = FlagDiscontinuity | FlagMuted | FlagUnderrun | FlagEstimated | FlagDigitalSilence
)

// Active-source mask bits (§16.1 offset 56 and cell byte 3).
const (
	SourceContent uint8 = 1
	SourceAlert   uint8 = 2
	SourceDialog  uint8 = 4
	SourceEarcon  uint8 = 8
	sourcesKnown        = SourceContent | SourceAlert | SourceDialog | SourceEarcon
)

// UncertaintyUnknown is the timing-uncertainty value meaning "unknown".
const UncertaintyUnknown uint32 = 0xffffffff

// Sample rates and frame-count bounds per kind (§16.1).
const (
	RateCapture = 16000 // mic, reference, cells, afe
	RateRender  = 48000 // render sources

	MaxUplinkPCMFrames = 1280 // kinds 1 and 2
	MaxRenderFrames    = 3840 // kind 3
	MaxCells           = 320  // kind 4
	MaxAFERecords      = 125  // kind 5

	CellSamples    = 512  // mic samples per cell (§16.6)
	CellRecordSize = 4    // bytes per cell record
	AFESamples     = 1280 // mic samples per AFE record: one capture period
	pcmSampleSize  = 2
)

// Validation errors. Decode and Validate return exactly one of these.
var (
	ErrShort         = errors.New("ema: buffer shorter than header")
	ErrMagic         = errors.New("ema: bad magic")
	ErrKind          = errors.New("ema: unknown kind")
	ErrFlags         = errors.New("ema: invalid flags")
	ErrChannels      = errors.New("ema: channels must be 1")
	ErrFormat        = errors.New("ema: format does not match kind")
	ErrEpoch         = errors.New("ema: epoch must be nonzero")
	ErrRate          = errors.New("ema: sample rate does not match kind")
	ErrFrameCount    = errors.New("ema: frame count out of range")
	ErrGeneration    = errors.New("ema: generation must be zero for this kind")
	ErrSourceMask    = errors.New("ema: invalid active-source mask")
	ErrCellAlignment = errors.New("ema: cell first sample not a multiple of 512")
	ErrAFEAlignment  = errors.New("ema: afe first sample not a multiple of 1280")
	ErrPayloadLength = errors.New("ema: payload length mismatch")
)

// Header is one decoded EMA1 header. Channels is always 1 and Format is
// implied by Kind; both are checked on decode and written on encode.
type Header struct {
	Kind          Kind
	Flags         uint8
	Channels      uint8
	Format        Format
	Epoch         uint64
	Sequence      uint64
	FirstSample   uint64 // kind 4: start sample of the first cell; kind 5: of the first record
	MonoNs        uint64 // first-sample CLOCK_MONOTONIC ns; 0 for downlink
	UncertaintyUs uint32
	SampleRate    uint32
	FrameCount    uint32 // samples, or cells for kind 4, or records for kind 5
	Generation    uint32 // kind 3 only
	SourceMask    uint32 // kind 2 only
	PayloadBytes  uint32
}

// FormatFor returns the only valid format for kind.
func FormatFor(k Kind) Format {
	switch k {
	case KindCells:
		return FormatCellV1
	case KindAFE:
		return FormatAFEV1
	default:
		return FormatPCM16
	}
}

// RateFor returns the only valid sample rate for kind.
func RateFor(k Kind) uint32 {
	if k == KindRender {
		return RateRender
	}
	return RateCapture
}

// MaxFrames returns the frame-count upper bound for kind.
func MaxFrames(k Kind) uint32 {
	switch k {
	case KindRender:
		return MaxRenderFrames
	case KindCells:
		return MaxCells
	case KindAFE:
		return MaxAFERecords
	default:
		return MaxUplinkPCMFrames
	}
}

// PayloadLen is the exact payload length for a frame of kind with flags and
// frames: frames×2 for PCM, 0 with digital silence, frames×4 for cells,
// frames×14 for AFE records.
func PayloadLen(k Kind, flags uint8, frames uint32) uint32 {
	switch {
	case k == KindCells:
		return frames * CellRecordSize
	case k == KindAFE:
		return frames * AFERecordSize
	case flags&FlagDigitalSilence != 0:
		return 0
	default:
		return frames * pcmSampleSize
	}
}

// NewHeader returns a header for kind with Channels, Format, SampleRate,
// FrameCount and PayloadBytes filled in consistently.
func NewHeader(k Kind, flags uint8, epoch, seq, first uint64, frames uint32) Header {
	return Header{
		Kind:         k,
		Flags:        flags,
		Channels:     1,
		Format:       FormatFor(k),
		Epoch:        epoch,
		Sequence:     seq,
		FirstSample:  first,
		SampleRate:   RateFor(k),
		FrameCount:   frames,
		PayloadBytes: PayloadLen(k, flags, frames),
	}
}

// Validate checks every WIRE §3 rule that a single header can violate.
func (h *Header) Validate() error {
	if h.Kind < KindMic || h.Kind > KindAFE {
		return ErrKind
	}
	if h.Flags&^flagsKnown != 0 || (h.Flags&FlagDigitalSilence != 0 && h.Kind != KindReference) {
		return ErrFlags
	}
	if h.Channels != 1 {
		return ErrChannels
	}
	if h.Format != FormatFor(h.Kind) {
		return ErrFormat
	}
	if h.Epoch == 0 {
		return ErrEpoch
	}
	if h.SampleRate != RateFor(h.Kind) {
		return ErrRate
	}
	if h.FrameCount < 1 || h.FrameCount > MaxFrames(h.Kind) {
		return ErrFrameCount
	}
	if h.Kind != KindRender && h.Generation != 0 {
		return ErrGeneration
	}
	if (h.Kind != KindReference && h.SourceMask != 0) || h.SourceMask&^uint32(sourcesKnown) != 0 {
		return ErrSourceMask
	}
	if h.Kind == KindCells && h.FirstSample%CellSamples != 0 {
		return ErrCellAlignment
	}
	if h.Kind == KindAFE && h.FirstSample%AFESamples != 0 {
		return ErrAFEAlignment
	}
	if h.PayloadBytes != PayloadLen(h.Kind, h.Flags, h.FrameCount) {
		return ErrPayloadLength
	}
	return nil
}

// Encode validates h and writes it into dst[:HeaderSize].
func (h *Header) Encode(dst []byte) error {
	if len(dst) < HeaderSize {
		return ErrShort
	}
	if err := h.Validate(); err != nil {
		return err
	}
	le := binary.LittleEndian
	copy(dst[0:4], Magic[:])
	dst[4] = byte(h.Kind)
	dst[5] = h.Flags
	dst[6] = h.Channels
	dst[7] = byte(h.Format)
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
	return nil
}

// Decode parses and validates the header at the start of src.
func Decode(src []byte) (Header, error) {
	if len(src) < HeaderSize {
		return Header{}, ErrShort
	}
	if [4]byte(src[0:4]) != Magic {
		return Header{}, ErrMagic
	}
	le := binary.LittleEndian
	h := Header{
		Kind:          Kind(src[4]),
		Flags:         src[5],
		Channels:      src[6],
		Format:        Format(src[7]),
		Epoch:         le.Uint64(src[8:]),
		Sequence:      le.Uint64(src[16:]),
		FirstSample:   le.Uint64(src[24:]),
		MonoNs:        le.Uint64(src[32:]),
		UncertaintyUs: le.Uint32(src[40:]),
		SampleRate:    le.Uint32(src[44:]),
		FrameCount:    le.Uint32(src[48:]),
		Generation:    le.Uint32(src[52:]),
		SourceMask:    le.Uint32(src[56:]),
		PayloadBytes:  le.Uint32(src[60:]),
	}
	if err := h.Validate(); err != nil {
		return Header{}, err
	}
	return h, nil
}

// DecodeFrame decodes a whole message and returns its payload, which must be
// exactly PayloadBytes long. The payload aliases msg.
func DecodeFrame(msg []byte) (Header, []byte, error) {
	h, err := Decode(msg)
	if err != nil {
		return Header{}, nil, err
	}
	if uint64(len(msg)-HeaderSize) != uint64(h.PayloadBytes) {
		return Header{}, nil, ErrPayloadLength
	}
	return h, msg[HeaderSize:], nil
}

// PutPCM writes samples as PCM16LE into dst, which must hold len(pcm)*2 bytes.
func PutPCM(dst []byte, pcm []int16) {
	dst = dst[:len(pcm)*pcmSampleSize]
	for i, s := range pcm {
		binary.LittleEndian.PutUint16(dst[i*pcmSampleSize:], uint16(s))
	}
}

// PCM reads PCM16LE samples from src into dst, which must hold len(src)/2.
func PCM(dst []int16, src []byte) {
	dst = dst[:len(src)/pcmSampleSize]
	for i := range dst {
		dst[i] = int16(binary.LittleEndian.Uint16(src[i*pcmSampleSize:]))
	}
}

// NewEpoch draws a random nonzero stream epoch (§16.1). It panics if the
// system random source fails: epochs must never repeat.
func NewEpoch() uint64 {
	var b [8]byte
	for {
		if _, err := rand.Read(b[:]); err != nil {
			panic("ema: crypto/rand: " + err.Error())
		}
		if v := binary.LittleEndian.Uint64(b[:]); v != 0 {
			return v
		}
	}
}
