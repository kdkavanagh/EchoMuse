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
// readers via a fan-out model. The vadStreamHandler uses this to tap the
// permanent capture stream without opening a second session.
//
// Every period handed out is one fully processed mono channel: Android's audio
// HAL runs per-mic AEC, beamforming and SNR beam selection before EchoMuse ever
// sees the stream (internal/bindings/slmic, docs/native-afe-migration.md). That
// is the only shape callers ever get — there is no raw multi-channel backend
// any more, and nothing downstream needs to ask which one it was given.
type Subscribable interface {
	Subscribe() chan []byte
	Unsubscribe(ch chan []byte)
}
