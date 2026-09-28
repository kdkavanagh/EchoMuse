package ema

import (
	"encoding/binary"
	"errors"
	"math"
)

// Cell record v1 limits and flags (§16.1).
const (
	CellEMin int16 = -12000 // hundredths of a dB; also the value of every gap cell
	CellEMax int16 = 0

	CellGap   uint8 = 1 << 0 // the cell overlaps a missing range
	CellMuted uint8 = 1 << 1
	cellFlags       = CellGap | CellMuted
)

// Cell record errors.
var (
	ErrCellShort = errors.New("ema: cell record shorter than 4 bytes")
	ErrCellE     = errors.New("ema: cell E out of range")
	ErrCellFlags = errors.New("ema: invalid cell flags")
	ErrCellMask  = errors.New("ema: invalid cell source mask")
	ErrCellGapE  = errors.New("ema: gap cell E must be -12000")
)

// Cell is one 512-sample loudness record.
type Cell struct {
	E     int16 // loudness in hundredths of a dB, [-12000, 0]
	Flags uint8 // CellGap, CellMuted
	Mask  uint8 // final-mix active-source mask during the cell
}

// CellE converts a loudness in dB to the wire value: rounded to 0.01 dB and
// clamped to [-12000, 0].
func CellE(db float64) int16 {
	v := math.Round(db * 100)
	switch {
	case v != v || v <= float64(CellEMin):
		return CellEMin
	case v >= float64(CellEMax):
		return CellEMax
	default:
		return int16(v)
	}
}

// GapCell is the record for a cell that overlaps a missing range.
func GapCell(flags, mask uint8) Cell {
	return Cell{E: CellEMin, Flags: flags | CellGap, Mask: mask}
}

// Validate checks the §16.1 cell-record rules.
func (c Cell) Validate() error {
	if c.E < CellEMin || c.E > CellEMax {
		return ErrCellE
	}
	if c.Flags&^cellFlags != 0 {
		return ErrCellFlags
	}
	if c.Mask&^sourcesKnown != 0 {
		return ErrCellMask
	}
	if c.Flags&CellGap != 0 && c.E != CellEMin {
		return ErrCellGapE
	}
	return nil
}

// Encode writes c into dst[:4]. E is clamped to the wire range and forced to
// CellEMin for a gap; reserved flag or source-mask bits are rejected.
func (c Cell) Encode(dst []byte) error {
	if len(dst) < CellRecordSize {
		return ErrCellShort
	}
	if c.Flags&^cellFlags != 0 {
		return ErrCellFlags
	}
	if c.Mask&^sourcesKnown != 0 {
		return ErrCellMask
	}
	if c.Flags&CellGap != 0 {
		c.E = CellEMin
	}
	PutCell(dst, c)
	return nil
}

// PutCell writes c into dst[:4], clamping E to the wire range.
func PutCell(dst []byte, c Cell) {
	e := c.E
	if e < CellEMin {
		e = CellEMin
	} else if e > CellEMax {
		e = CellEMax
	}
	binary.LittleEndian.PutUint16(dst[0:2], uint16(e))
	dst[2] = c.Flags
	dst[3] = c.Mask
}

// ParseCell decodes and validates the record at the start of src.
func ParseCell(src []byte) (Cell, error) {
	if len(src) < CellRecordSize {
		return Cell{}, ErrCellShort
	}
	c := Cell{E: int16(binary.LittleEndian.Uint16(src[0:2])), Flags: src[2], Mask: src[3]}
	if err := c.Validate(); err != nil {
		return Cell{}, err
	}
	return c, nil
}

// DecodeCell is the cell-record v1 decoder.
func DecodeCell(src []byte) (Cell, error) { return ParseCell(src) }
