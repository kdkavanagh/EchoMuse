package ema

import "errors"

// AFE record v1 (kind 5, WIRE §3): one summary of the native AFE's per-frame
// metadata over one 1,280-sample capture period.
const (
	AFERecordSize = 14

	// AFEMaxFrames is the most AFE frames (128 samples each) a capture
	// period can hold.
	AFEMaxFrames = AFESamples / 128

	AFEGap           uint8 = 1 << 0 // the AFE counters show frames missing, or the frame phase moved
	AFESync          uint8 = 1 << 1 // the decoder acquired lock in this period
	AFEOutputClipped uint8 = 1 << 2
	AFEMicClipped    uint8 = 1 << 3
	AFEAECDiverged   uint8 = 1 << 4
	AFEDeviceMute    uint8 = 1 << 5
	afeFlags               = AFEGap | AFESync | AFEOutputClipped | AFEMicClipped | AFEAECDiverged | AFEDeviceMute

	// Field ranges of the AFE's own encoding.
	AFEDTDMax    = 31  // DTD is 5 bits, value raw/31
	AFEVADMax    = 3   // DNN_VAD_PROB is 2 bits, value raw×0.25
	AFEVolumeMax = 127 // VOLUME is 7 bits
)

// AFE record errors.
var (
	ErrAFEShort  = errors.New("ema: afe record shorter than 14 bytes")
	ErrAFERecord = errors.New("ema: afe record field out of range")
	ErrAFEFlags  = errors.New("ema: invalid afe record flags")
)

// AFERecord is one period's summary. Frames 0 means the period holds no
// data; every metric is then 0. Raw values keep the AFE's own scales: RMS
// dB = raw − 256 (0: no frame computed RMS), DTD raw/31, DNN VAD raw×0.25.
type AFERecord struct {
	Frames     uint8 // valid frames that ended in the period
	Flags      uint8
	Playback   uint8 // frames with PLAYBACK_ACTIVE
	ERLEMax    uint8
	ERLEMean   uint8 // mean of the non-zero ERLE_RAW values, rounded
	ERLEFrames uint8 // frames with ERLE_RAW > 0
	DTDMax     uint8
	DTDFrames  uint8 // frames with DTD > 0
	RMSMax     uint8
	RMSMean    uint8
	VADMax     uint8
	VADFrames  uint8 // frames with DNN_VAD_PROB > 0
	Volume     uint8 // VOLUME of the period's last valid frame
	Lost       uint8 // AFE frames missing before this period's frames, clamped
}

// Validate checks the record v1 bounds.
func (r AFERecord) Validate() error {
	if r.Flags&^afeFlags != 0 {
		return ErrAFEFlags
	}
	if r.Frames > AFEMaxFrames || r.Playback > r.Frames || r.ERLEFrames > r.Frames ||
		r.DTDFrames > r.Frames || r.VADFrames > r.Frames ||
		r.DTDMax > AFEDTDMax || r.VADMax > AFEVADMax || r.Volume > AFEVolumeMax {
		return ErrAFERecord
	}
	return nil
}

// PutAFE writes r into dst[:14].
func PutAFE(dst []byte, r AFERecord) {
	_ = dst[AFERecordSize-1]
	dst[0], dst[1], dst[2], dst[3] = r.Frames, r.Flags, r.Playback, r.ERLEMax
	dst[4], dst[5], dst[6], dst[7] = r.ERLEMean, r.ERLEFrames, r.DTDMax, r.DTDFrames
	dst[8], dst[9], dst[10], dst[11] = r.RMSMax, r.RMSMean, r.VADMax, r.VADFrames
	dst[12], dst[13] = r.Volume, r.Lost
}

// ParseAFE decodes and validates the record at the start of src.
func ParseAFE(src []byte) (AFERecord, error) {
	if len(src) < AFERecordSize {
		return AFERecord{}, ErrAFEShort
	}
	r := AFERecord{
		Frames: src[0], Flags: src[1], Playback: src[2], ERLEMax: src[3],
		ERLEMean: src[4], ERLEFrames: src[5], DTDMax: src[6], DTDFrames: src[7],
		RMSMax: src[8], RMSMean: src[9], VADMax: src[10], VADFrames: src[11],
		Volume: src[12], Lost: src[13],
	}
	if err := r.Validate(); err != nil {
		return AFERecord{}, err
	}
	return r, nil
}
