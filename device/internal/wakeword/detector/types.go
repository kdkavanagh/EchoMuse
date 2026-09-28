// Package detector owns the continuous device BCResNet scorer described by
// SPEC §5.2. Capture only copies windows into a bounded queue; one goroutine
// locked to its OS thread owns inference and every policy state transition.
package detector

import (
	"errors"
	"fmt"
	"time"
)

const (
	// WindowSamples is the 1.4 s BCResNet window at 16 kHz.
	WindowSamples = 22400
	// BlockSamples is one 80 ms capture callback (SPEC §4.1).
	BlockSamples = 1280
	// HopSamples is two 80 ms capture blocks.
	HopSamples = 2560
	// FirstWindowEnd is the first hop-grid point with a full window.
	FirstWindowEnd = 23040
	// QueueDepth bounds pending inference to 640 ms.
	QueueDepth = 4
	// SmoothingWindows is the number of scored probabilities in the mean.
	SmoothingWindows = 3
	// ClearAfterUnscored is 960 ms of unscored hop slots.
	ClearAfterUnscored = 6
	// StatsPeriod is the wake.stats reporting interval.
	StatsPeriod = 30 * time.Second
)

// CheckPolicy rejects a session.ready detector policy this detector does not
// implement: a hop of 2 blocks, smoothing of 3, and history cleared after 6
// unscored hops (SPEC §5.2, §16.1).
func CheckPolicy(hopBlocks, smoothing, clearAfterUnscored int) error {
	if hopBlocks*BlockSamples != HopSamples || smoothing != SmoothingWindows || clearAfterUnscored != ClearAfterUnscored {
		return fmt.Errorf("detector: policy hop_blocks=%d smoothing=%d clear_after_unscored=%d, implemented %d/%d/%d",
			hopBlocks, smoothing, clearAfterUnscored, HopSamples/BlockSamples, SmoothingWindows, ClearAfterUnscored)
	}
	return nil
}

// Profile is the threshold profile sampled at each hop.
type Profile string

const (
	ProfileIdle     Profile = "idle"
	ProfilePlayback Profile = "playback"
)

// Thresholds comes from session.ready.
type Thresholds struct {
	Idle     float64 `json:"idle"`
	Playback float64 `json:"playback"`
	NearMiss float64 `json:"near_miss"`
}

func (t Thresholds) validate() error {
	for name, v := range map[string]float64{"idle": t.Idle, "playback": t.Playback, "near_miss": t.NearMiss} {
		if !(v > 0 && v <= 1) {
			return fmt.Errorf("detector: %s threshold %v outside (0, 1]", name, v)
		}
	}
	return nil
}

func (t Thresholds) forProfile(p Profile) (float64, bool) {
	switch p {
	case ProfileIdle:
		return t.Idle, true
	case ProfilePlayback:
		return t.Playback, true
	default:
		return 0, false
	}
}

// Scorer scores one complete int16 PCM window. scored=false means its RMS was
// below the model floor and smoothing must be left unchanged. Implementations
// are used only by the wake goroutine.
type Scorer interface {
	Score(pcm []int16) (prob float64, scored bool, err error)
	Close() error
}

// Model is a graph+sidecar scorer that has already loaded successfully. The
// detector keeps its previous Model until SetModel receives this complete one.
type Model struct {
	Scorer      Scorer
	GraphSHA256 string
	Thresholds  Thresholds
}

func (m Model) validate() error {
	if m.Scorer == nil {
		return errors.New("detector: nil scorer")
	}
	if len(m.GraphSHA256) != 64 {
		return errors.New("detector: graph SHA-256 must be 64 hexadecimal characters")
	}
	for _, c := range m.GraphSHA256 {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return errors.New("detector: graph SHA-256 must be lowercase hexadecimal")
		}
	}
	return m.Thresholds.validate()
}

// Hop is one wake.candidate hops record. Raw and Smoothed are nil for an
// unscored window. Uint64 sample indices marshal as decimal strings.
type Hop struct {
	EndSample uint64   `json:"end_sample,string"`
	Raw       *float64 `json:"raw"`
	Smoothed  *float64 `json:"smoothed"`
	Profile   Profile  `json:"profile"`
}

// Candidate is the detector's part of the WIRE wake.candidate body. The
// transport adds `active_alert`, which the detector does not know.
type Candidate struct {
	CandidateID      string  `json:"candidate_id"`
	LeaseID          string  `json:"lease_id"`
	CaptureEpoch     uint64  `json:"capture_epoch,string"`
	GraphSHA256      string  `json:"graph_sha256"`
	ScorerRevision   uint64  `json:"scorer_revision"`
	Profile          Profile `json:"profile"`
	Threshold        float64 `json:"threshold"`
	ProducingSound   bool    `json:"producing_sound"`
	FirstCrossingEnd uint64  `json:"first_crossing_end,string"`
	SupportStart     uint64  `json:"support_start,string"`
	MonoNS           int64   `json:"mono_ns,string"`
	Hops             []Hop   `json:"hops"`
}

// CandidateEnd is the WIRE wake.candidate_end body.
type CandidateEnd struct {
	CandidateID  string      `json:"candidate_id"`
	SupportEnd   uint64      `json:"support_end,string"`
	PeakSmoothed float64     `json:"peak_smoothed"`
	Reason       CloseReason `json:"reason"`
}

// CloseReason is the WIRE wake.candidate_end reason.
type CloseReason string

const (
	ReasonBelow   CloseReason = "below"
	ReasonGap     CloseReason = "gap"
	ReasonReset   CloseReason = "reset"
	ReasonMute    CloseReason = "mute"
	ReasonOverrun CloseReason = "overrun"
)

// NearMiss is one near-miss episode reported in wake.stats.
type NearMiss struct {
	MonoNS int64   `json:"mono_ns,string"`
	Peak   float64 `json:"peak"`
}

// UnavailableReason is WIRE wake_unavailable.
type UnavailableReason string

const (
	UnavailableMissingAsset    UnavailableReason = "missing_asset"
	UnavailableLoadFailed      UnavailableReason = "load_failed"
	UnavailableInferenceErrors UnavailableReason = "inference_errors"
)

// Stats is the WIRE wake.stats body. Inference times are null when nothing
// was inferred in the window and peak_smoothed when nothing was scored.
type Stats struct {
	WindowMS         int64              `json:"window_ms"`
	HopsScored       uint64             `json:"hops_scored"`
	HopsDropped      uint64             `json:"hops_dropped"`
	InferenceErrors  uint64             `json:"inference_errors"`
	InferMeanMS      *float64           `json:"infer_mean_ms"`
	InferMaxMS       *float64           `json:"infer_max_ms"`
	NearMisses       []NearMiss         `json:"near_misses"`
	CandidatesOpened uint64             `json:"candidates_opened"`
	PeakSmoothed     *float64           `json:"peak_smoothed"`
	GraphSHA256      *string            `json:"graph_sha256"`
	WakeUnavailable  *UnavailableReason `json:"wake_unavailable"`
	Detail           *string            `json:"detail"`
}

// Callbacks connect the detector to the device. Profile is called on the
// capture goroutine at each hop and must be cheap and non-blocking; the
// others run on the wake goroutine and must not block it.
type Callbacks struct {
	// Profile selects the threshold profile from the mixer state (SPEC §5.3).
	Profile func() Profile
	// ProducingSound reports whether the final mix was non-silent anywhere in
	// the capture-sample range [from, to].
	ProducingSound func(from, to uint64) bool
	OnCandidate    func(Candidate)
	OnCandidateEnd func(CandidateEnd)
	// OnStats receives wake.stats every StatsPeriod and whenever
	// wake_unavailable changes.
	OnStats func(Stats)
	// NowMonoNS is the device monotonic clock in ns.
	NowMonoNS func() int64
	// NewID returns a fresh UUID for candidate and lease IDs.
	NewID func() string
}
