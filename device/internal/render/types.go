// Package render is the device's single final mixer (SPEC §4.1, §4.3, §6.2):
// every audible source is summed here, per-sample gain ramps apply focus, the
// final mix is tapped for the reference stream, and 2,048-frame buffers are
// handed to the OpenSL sink with completion-timed frame accounting.
package render

import "errors"

// Audio format and buffering (SPEC §4.1, §4.4, §16.1).
const (
	SampleRate  = 48000 // Hz, mono S16
	BlockFrames = 480   // internal mixer block, 10 ms
	SinkFrames  = 2048  // one OpenSL write, 42.7 ms

	NetworkWrites = 128 // network source FIFO depth, in sink writes (~5.5 s)
	PrimeWrites   = 24  // network source prime before start, in sink writes (~1.0 s)

	NormalRampFrames = 1440 // 30 ms linear ramp for duck/restore/cancel (§6.2)
	StopRampFrames   = 480  // 10 ms linear ramp for physical stop (§6.2)

	// DrainGuardNs is the delay after the last buffer completion before a
	// playback is declared drained (§4.3).
	DrainGuardNs = 150_000_000
	// ProgressIntervalNs is the render.progress cadence (§4.3, §16.1).
	ProgressIntervalNs = 80_000_000
	// StarvedLimitFrames ends a network playback as underrun after 2 s of
	// continuous starvation, matching the §7 "2 seconds without output
	// progress" bound.
	StarvedLimitFrames = 2 * SampleRate

	fifoFrames  = NetworkWrites * SinkFrames
	primeFrames = PrimeWrites * SinkFrames
)

// Active-source mask bits of the final mix (SPEC §16.1).
const (
	MaskContent uint8 = 1
	MaskAlert   uint8 = 2
	MaskDialog  uint8 = 4
	MaskEarcon  uint8 = 8
)

// SourceClass names a render.start source class (WIRE §4.2). The cached
// alert executor is not a playback; it is the mixer's PullSource.
type SourceClass string

const (
	Content      SourceClass = "content"
	DialogOutput SourceClass = "dialog_output"
	Earcon       SourceClass = "earcon"
	AlertPreview SourceClass = "alert_preview"
)

// Network reports whether the class is fed by kind-3 packets through a FIFO.
func (c SourceClass) Network() bool { return c == Content || c == DialogOutput }

// FinishReason is the render.finished reason (SPEC §4.3).
type FinishReason string

const (
	Drained   FinishReason = "drained"
	Cancelled FinishReason = "cancelled"
	Failed    FinishReason = "failed"
	Underrun  FinishReason = "underrun"
)

// ProgressEvent is the render.progress event (WIRE §4.2). The device has no
// seek operation: a seek is a new playback.
type ProgressEvent string

const (
	EventProgress ProgressEvent = "progress"
	EventStart    ProgressEvent = "start"
	EventFlush    ProgressEvent = "flush"
	EventPause    ProgressEvent = "pause"
	EventResume   ProgressEvent = "resume"
	EventUnderrun ProgressEvent = "underrun"
	EventGain     ProgressEvent = "gain"
)

// TimingQuality is the render.progress/render.finished timing_quality.
type TimingQuality string

const TimingEstimated TimingQuality = "estimated"

// ReferenceCoverage is the render.progress reference_coverage: how much of
// the playback the reference stream carries.
type ReferenceCoverage string

const CoverageFull ReferenceCoverage = "full"

var (
	ErrInvalidClass  = errors.New("render: invalid source class")
	ErrNoAudio       = errors.New("render: local playback without PCM")
	ErrNoEpoch       = errors.New("render: network playback without epoch")
	ErrStale         = errors.New("render: stale generation")
	ErrUnknownEpoch  = errors.New("render: unknown render epoch")
	ErrDiscontinuous = errors.New("render: packet does not continue the source")
	ErrEnded         = errors.New("render: audio after render.end")
	ErrFIFOFull      = errors.New("render: source FIFO overflow")
	ErrConfig        = errors.New("render: Sink and Now are required")
)

// Sink is the hardware output. Write blocks until a hardware buffer is free
// and enqueues exactly SinkFrames samples. For every successful Write the
// sink later calls the completion callback once, in write order, with the
// buffer's CLOCK_MONOTONIC completion time; the callback never blocks. A sink
// keeps at most 8 writes outstanding.
type Sink interface {
	Write(pcm []int16) error
	SetOnComplete(func(monoNs int64))
	Restart() error
	Close() error
}

// PullSource is a device-local source the mixer pulls each 480-sample 48 kHz block.
// Fill writes len(dst) samples (silence where it has none) and reports whether the
// source is currently audible (its bit in the active-source mask). Called only from
// the mixer goroutine; must not block or allocate.
type PullSource interface {
	Fill(dst []int16) (active bool)
}

// MixBlock is one final-mix block handed to the sink. PCM is borrowed for the
// duration of the tap call. Mask: content=1 alert=2 dialog=4 earcon=8.
type MixBlock struct {
	First uint64 // render-epoch 48 kHz index of PCM[0]
	PCM   []int16
	Mask  uint8
}

// Playback is a render.start. Network classes carry Epoch (kind-3 stream);
// local classes carry their whole PCM, which the mixer only reads.
type Playback struct {
	ID         string
	Generation uint32
	Class      SourceClass
	Epoch      uint64
	GainDB     float64 // user-selected gain; ducking is relative to it
	PCM        []int16
}

// Progress is a render.progress body. Frame counts are this playback's source
// frames handed to the sink and completed by it. MonoNS anchors the render
// epoch's completed frontier. MissingFrom/MissingTo (render-epoch frames,
// half-open) are meaningful only for EventUnderrun; GainDB only for EventGain.
type Progress struct {
	PlaybackID        string
	Generation        uint32
	Event             ProgressEvent
	SubmittedFrames   uint64
	CompletedFrames   uint64
	MonoNS            int64
	UncertaintyUS     uint32
	TimingQuality     TimingQuality
	ReferenceCoverage ReferenceCoverage
	MissingFrom       uint64
	MissingTo         uint64
	GainDB            float64
}

// Finished is a render.finished body. LastCompletedFrame is the exclusive
// completed source-frame frontier.
type Finished struct {
	PlaybackID         string
	Generation         uint32
	LastCompletedFrame uint64
	Reason             FinishReason
	TimingQuality      TimingQuality
}

// Anchor relates the render epoch's completed frontier to CLOCK_MONOTONIC.
type Anchor struct {
	Epoch          uint64
	CompletedFrame uint64 // exclusive render-epoch frame
	MonoNS         int64
}

// Hooks deliver mixer output. Progress and Finished run outside the mixer
// lock, in order, and may call back into the Mixer. Tap, Anchor and Epoch run
// on the mixer goroutine and must not block.
type Hooks struct {
	Progress func(Progress)
	Finished func(Finished)
	Tap      func(MixBlock)
	Anchor   func(Anchor)
	Epoch    func(epoch uint64)
}

// Ramp selects the gain-ramp length of a transition.
type Ramp uint8

const (
	RampNormal Ramp = iota
	RampPhysicalStop
)

// Frames is the ramp length in samples, independent of write size.
func (r Ramp) Frames() int {
	if r == RampPhysicalStop {
		return StopRampFrames
	}
	return NormalRampFrames
}

// Policy is the focus decision the mixer applies (SPEC §6.2, §16.2).
// ContentDuckDB applies to content and alert_preview, DialogDuckDB to
// dialog_output; earcons are never ducked. PauseContent holds content and
// alert_preview without discarding their audio.
type Policy struct {
	ContentDuckDB float64
	DialogDuckDB  float64
	PauseContent  bool
	Ramp          Ramp
}
