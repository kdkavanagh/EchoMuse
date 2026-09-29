package supervisor

import (
	"errors"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/focus"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/uplink"
)

// command.ack rejection codes (WIRE §4.1).
const (
	codeInvalid         proto.AckCode = "invalid"
	codeInvalidClass    proto.AckCode = "invalid_class"
	codeUnknownAsset    proto.AckCode = "unknown_asset"
	codeUnknownLease    proto.AckCode = "unknown_lease"
	codeUnknownPlayback proto.AckCode = "unknown_playback"
	codeStaleGeneration proto.AckCode = "stale_generation"
	codeMuted           proto.AckCode = "muted"
)

// actAck maps an alert executor Act result onto its command.ack; the
// executor's error codes are alert.act's command.ack codes.
func actAck(res alerts.ActResult) (proto.AckStatus, proto.AckCode) {
	switch res.Status {
	case alerts.StatusApplied:
		return proto.AckApplied, proto.AckCode(res.Error)
	case alerts.StatusDurable:
		return proto.AckDurable, proto.AckCode(res.Error)
	default:
		return proto.AckRejected, proto.AckCode(res.Error)
	}
}

func renderCode(err error) proto.AckCode {
	switch {
	case errors.Is(err, render.ErrStale):
		return codeStaleGeneration
	case errors.Is(err, render.ErrInvalidClass):
		return codeInvalidClass
	default:
		return codeInvalid
	}
}

// endCode maps render.end errors: the mixer reports an unknown or superseded
// playback as ErrStale.
func endCode(err error) proto.AckCode {
	if errors.Is(err, render.ErrStale) {
		return codeUnknownPlayback
	}
	return codeInvalid
}

func focusCode(err error) proto.AckCode {
	switch {
	case errors.Is(err, focus.ErrStale):
		return codeStaleGeneration
	case errors.Is(err, focus.ErrUnknown):
		return codeUnknownLease
	default:
		return codeInvalid
	}
}

func uplinkCode(err error) proto.AckCode {
	switch {
	case errors.Is(err, uplink.ErrUnknownLease):
		return codeUnknownLease
	case errors.Is(err, uplink.ErrStaleGeneration):
		return codeStaleGeneration
	default:
		return codeInvalid
	}
}
