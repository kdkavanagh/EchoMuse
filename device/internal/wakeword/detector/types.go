// Package detector owns the continuous device BCResNet scorer described by
// SPEC §5.2. Capture only copies windows into a bounded queue; one goroutine
// locked to its OS thread owns inference and every policy state transition.
package detector

import (
	"errors"
	"fmt"
	"slices"
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
	// ShadowHoldSamples: a shadow episode no live candidate overlapped waits
	// 10 s from its open for a live candidate (retried) before it counts as
	// unmatched.
	ShadowHoldSamples = 10 * 16000
	// ShadowEventsPerWindow caps a shadow rule's events per stats window.
	ShadowEventsPerWindow = 16
	// LeadBuckets is the lead histogram size: −3..+3 hops.
	LeadBuckets = 7
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

// RuleCombine is how an open rule combines its windows.
type RuleCombine string

const (
	// CombineMean fires when the mean of the rule's windows reaches the threshold.
	CombineMean RuleCombine = "mean"
	// CombineAll fires when every one of the rule's windows reaches it.
	CombineAll RuleCombine = "all"
)

const (
	// MaxOpenRules and MaxShadowRules bound session.ready's rule lists.
	MaxOpenRules   = 6
	MaxShadowRules = 8
)

// OpenRule is one candidate-open condition (SPEC §5.2): at a scored hop
// whose profile is Profile, over the last Windows raw scores of the scored
// history (fewer right after a reset, as the smoothing mean is), the mean
// or every score reaches Threshold.
type OpenRule struct {
	Profile   Profile     `json:"profile"`
	Windows   int         `json:"windows"`
	Combine   RuleCombine `json:"combine"`
	Threshold float64     `json:"threshold"`
}

func (r OpenRule) validate() error {
	switch {
	case r.Profile != ProfileIdle && r.Profile != ProfilePlayback:
		return fmt.Errorf("detector: rule profile %q", r.Profile)
	case r.Windows < 1 || r.Windows > SmoothingWindows:
		return fmt.Errorf("detector: rule windows %d outside 1..%d", r.Windows, SmoothingWindows)
	case r.Combine != CombineMean && r.Combine != CombineAll:
		return fmt.Errorf("detector: rule combine %q", r.Combine)
	case !(r.Threshold > 0 && r.Threshold <= 1):
		return fmt.Errorf("detector: rule threshold %v outside (0, 1]", r.Threshold)
	}
	return nil
}

// fires evaluates the rule over the scored history, oldest first. It
// returns the end sample of the oldest window it used.
func (r OpenRule) fires(scores []scoreEntry) (uint64, bool) {
	if len(scores) == 0 {
		return 0, false
	}
	used := scores[max(0, len(scores)-r.Windows):]
	var sum float64
	for _, s := range used {
		if r.Combine == CombineAll && s.raw < r.Threshold {
			return 0, false
		}
		sum += s.raw
	}
	if r.Combine == CombineMean && sum/float64(len(used)) < r.Threshold {
		return 0, false
	}
	return used[0].end, true
}

// Rules are session.ready's open_rules and shadow_rules. A nil Open means
// the field was absent: the two baseline rules derive from the thresholds.
type Rules struct {
	Open   []OpenRule
	Shadow []OpenRule
}

// baseline is the registry rule set: the 3-window mean at each profile's
// threshold.
func baseline(th Thresholds) []OpenRule {
	return []OpenRule{
		{Profile: ProfileIdle, Windows: SmoothingWindows, Combine: CombineMean, Threshold: th.Idle},
		{Profile: ProfilePlayback, Windows: SmoothingWindows, Combine: CombineMean, Threshold: th.Playback},
	}
}

// live returns the live rules in effect under th.
func (r Rules) live(th Thresholds) []OpenRule {
	if r.Open == nil {
		return baseline(th)
	}
	return r.Open
}

// CheckRules rejects a rule set this detector cannot apply: at most
// MaxOpenRules live and MaxShadowRules shadow rules, every field valid, and
// at least one live rule per profile. Absent live rules are always valid.
func CheckRules(r Rules) error {
	if r.Open != nil {
		if len(r.Open) > MaxOpenRules {
			return fmt.Errorf("detector: %d open rules, at most %d", len(r.Open), MaxOpenRules)
		}
		var idle, playback bool
		for _, o := range r.Open {
			if err := o.validate(); err != nil {
				return err
			}
			idle = idle || o.Profile == ProfileIdle
			playback = playback || o.Profile == ProfilePlayback
		}
		if !idle || !playback {
			return errors.New("detector: open rules need at least one rule per profile")
		}
	}
	if len(r.Shadow) > MaxShadowRules {
		return fmt.Errorf("detector: %d shadow rules, at most %d", len(r.Shadow), MaxShadowRules)
	}
	for _, s := range r.Shadow {
		if err := s.validate(); err != nil {
			return err
		}
	}
	return nil
}

func (r Rules) equal(o Rules) bool {
	return (r.Open == nil) == (o.Open == nil) && slices.Equal(r.Open, o.Open) && slices.Equal(r.Shadow, o.Shadow)
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
	Rules       Rules
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
	if err := m.Thresholds.validate(); err != nil {
		return err
	}
	return CheckRules(m.Rules)
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
	// Rule is the live rule that opened the candidate; Threshold is its.
	Rule OpenRule `json:"rule"`
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
	// Shadow has one entry per shadow rule, in shadow_rules order.
	Shadow []ShadowStats `json:"shadow"`
}

// ShadowEventKind is how a shadow episode that no live candidate overlapped
// resolved.
type ShadowEventKind string

const (
	// ShadowUnmatched: no live candidate opened within ShadowHoldSamples of
	// the episode's open, a would-be false wake.
	ShadowUnmatched ShadowEventKind = "unmatched"
	// ShadowRetried: a live candidate opened within that time, likely a real
	// wake the live rules missed and the user repeated.
	ShadowRetried ShadowEventKind = "retried"
)

// ShadowEvent is one resolved non-overlapping shadow episode.
type ShadowEvent struct {
	Kind       ShadowEventKind `json:"kind"`
	OpenSample uint64          `json:"open_sample,string"`
	MonoNS     int64           `json:"mono_ns,string"`
	PeakRaw    float64         `json:"peak_raw"`
	// Raws are the last (up to three) raw scores at the episode's open.
	Raws []float64 `json:"raws"`
}

// ShadowStats is one wake.stats shadow entry: counters for this stats window.
type ShadowStats struct {
	Rule    OpenRule `json:"rule"`
	Hops    uint64   `json:"hops"`
	Opens   uint64   `json:"opens"`
	Matched uint64   `json:"matched"`
	// LeadHist counts matched episodes by (live open − shadow open) in hops,
	// clamped to −3..+3 at index lead+3.
	LeadHist      [LeadBuckets]uint64 `json:"lead_hist"`
	Unmatched     uint64              `json:"unmatched"`
	Retried       uint64              `json:"retried"`
	LiveOnly      uint64              `json:"live_only"`
	Events        []ShadowEvent       `json:"events"`
	EventsDropped uint64              `json:"events_dropped"`
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
