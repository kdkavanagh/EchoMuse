package shadow

import (
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
)

// Engine is a wake-word scorer over a stream of 16kHz mono PCM chunks.
//
// It exists because the two engines disagree about something fundamental: how
// often there is a score. openWakeWord advances a ring per 80ms chunk and can
// score every one of them; BC-ResNet examines a 1.4s window and is run on a
// hop, so most chunks produce nothing at all.
//
// Hence the `ok` return. A chunk that was not judged is NOT a chunk that
// scored zero — feeding an invented zero to the crossing test would be
// harmless, but feeding it to MaxScore would quietly make the window summary
// report a detector that is scoring far more often than it is.
type Engine interface {
	// Push adds one chunk. ok reports whether a score came with it.
	Push(samples []int16) (score float32, ok bool, err error)
	// Reset discards the streaming state, for when the mic stream restarts.
	Reset()
	// Ready reports whether enough audio has accumulated to score at all.
	// This is what separates "warming up" from "between hops", which the
	// stats keep apart because only the first is a reason to worry.
	Ready() bool
}

// owwEngine adapts the openWakeWord streaming Detector, whose Push and Score
// are deliberately separate so a caller can push audio it does not intend to
// score. Nothing about that split changes here — this only expresses it in the
// shape the scorer now drives.
type owwEngine struct{ det *wakeword.Detector }

// NewOwwEngine wraps an openWakeWord Inferer as an Engine.
func NewOwwEngine(inf wakeword.Inferer) Engine { return &owwEngine{det: wakeword.New(inf)} }

func (e *owwEngine) Reset()      { e.det.Reset() }
func (e *owwEngine) Ready() bool { return e.det.Ready() }

func (e *owwEngine) Push(samples []int16) (float32, bool, error) {
	if _, err := e.det.Push(samples); err != nil {
		return 0, false, err
	}
	if !e.det.Ready() {
		return 0, false, nil
	}
	score, err := e.det.Score()
	if err != nil {
		return 0, false, err
	}
	return score, true, nil
}

// bcresnetEngine adapts the BC-ResNet Detector, which already has exactly this
// shape — the interface was drawn around it rather than the other way round,
// because it is the engine with something to say about `ok`.
type bcresnetEngine struct{ det *bcresnet.Detector }

func (e *bcresnetEngine) Reset()      { e.det.Reset() }
func (e *bcresnetEngine) Ready() bool { return e.det.Ready() }
func (e *bcresnetEngine) Push(samples []int16) (float32, bool, error) {
	return e.det.Push(samples)
}
