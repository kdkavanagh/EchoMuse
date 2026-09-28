package supervisor

import (
	"errors"

	"github.com/wilbowes/EchoMuse/internal/focus"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/uplink"
)

// command.ack rejection codes (WIRE §4.1).
const (
	codeInvalid         = "invalid"
	codeInvalidClass    = "invalid_class"
	codeUnknownAsset    = "unknown_asset"
	codeUnknownLease    = "unknown_lease"
	codeUnknownPlayback = "unknown_playback"
	codeStaleGeneration = "stale_generation"
	codeMuted           = "muted"
)

func renderCode(err error) string {
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
func endCode(err error) string {
	if errors.Is(err, render.ErrStale) {
		return codeUnknownPlayback
	}
	return codeInvalid
}

func focusCode(err error) string {
	switch {
	case errors.Is(err, focus.ErrStale):
		return codeStaleGeneration
	case errors.Is(err, focus.ErrUnknown):
		return codeUnknownLease
	default:
		return codeInvalid
	}
}

func uplinkCode(err error) string {
	switch {
	case errors.Is(err, uplink.ErrUnknownLease):
		return codeUnknownLease
	case errors.Is(err, uplink.ErrStaleGeneration):
		return codeStaleGeneration
	default:
		return codeInvalid
	}
}

// isSHA256 reports whether s is a lowercase hex SHA-256.
func isSHA256(s string) bool {
	if len(s) != 64 {
		return false
	}
	for _, c := range s {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}
