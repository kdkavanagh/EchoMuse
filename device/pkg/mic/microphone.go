package mic

import (
	"context"
)

type AudioCallback func(audioData []byte)

type Microphone interface {
	Init() error
	Listen(callback AudioCallback, context context.Context) error
}

// Subscribable is implemented by mic backends that support multiple concurrent
// readers via a fan-out model (i.e. PcmMicrophone). The vadStreamHandler uses
// this to tap the permanent ALSA stream without opening a second PCM session.
type Subscribable interface {
	Subscribe() chan []byte
	Unsubscribe(ch chan []byte)
}

// PassthroughReporter is implemented by backends whose fanned-out periods are
// already one fully processed mono channel — the native-AFE backend
// (internal/bindings/slmic), where Android's audio HAL has already run
// per-mic AEC and beamforming before EchoMuse ever sees the stream (see
// docs/native-afe-migration.md).
//
// A backend that does NOT implement this (PcmMicrophone) delivers raw
// interleaved multi-channel capture instead, and callers must run it through
// internal/beamformer before it means anything. Feeding an already-mono
// stream to the beamformer would misinterpret its bytes as interleaved
// channels — silently wrong, not just redundant — so this has to be an
// explicit, backend-declared fact rather than inferred from buffer length.
// A nil check (the backend doesn't implement the interface at all) is
// equivalent to reporting false.
type PassthroughReporter interface {
	Passthrough() bool
}
