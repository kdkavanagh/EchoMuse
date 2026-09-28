// Package proto holds the device protocol v1 control-plane wire types
// (docs/protocol-v1.md). Field names are the WIRE's verbatim. uint64 values
// travel as JSON decimal strings (`,string` tags, or NullU64 when nullable).
//
// Bodies already defined by the package that produces them are not repeated
// here: detector.Candidate/CandidateEnd/Stats (wake.*), alerts.AlertState,
// RingEnded, LocalOperation, OpResult, TimerRing, ClockRequest/ClockReply,
// AlertAck and the snapshot/delta pages.
package proto

import (
	"encoding/json"
	"errors"
	"strconv"
)

// Version is the only protocol revision this firmware speaks.
const Version = 1

// Socket paths (WIRE §1).
const (
	PathControl = "/device/v1/control"
	PathAudio   = "/device/v1/audio"
	PathAssets  = "/device/v1/assets"

	HeaderToken   = "X-EM-Token"
	HeaderSession = "X-EM-Session"

	// MaxControlBytes bounds one control text frame (WIRE §1).
	MaxControlBytes = 256 * 1024
)

// Capabilities (SPEC §11.1) plus the retained hardware capabilities.
const (
	CapAudioTimeline   = "audio_timeline_v1"
	CapUplinkLeases    = "uplink_leases_v1"
	CapDeviceWake      = "device_wake_v1"
	CapRenderReference = "render_reference_v1"
	CapRenderProgress  = "render_progress_v1"
	CapFocusLeases     = "focus_leases_v1"
	CapAlertCache      = "alert_cache_v1"
	CapTurnProtocol    = "turn_protocol_v1"

	CapLEDs         = "leds"
	CapLEDAnim      = "led_anim"
	CapButtons      = "buttons"
	CapButtonHold   = "button_hold"
	CapAmbientLight = "ambient_light"

	// CapAlertPrefetch: the device installs the sounds alert.prefetch names.
	CapAlertPrefetch = "alert_prefetch"
)

// Message types (WIRE §4).
const (
	TypeSessionHello    = "session.hello"
	TypeSessionReady    = "session.ready"
	TypeSessionRejected = "session.rejected"
	TypeHeartbeat       = "heartbeat"
	TypeClockRequest    = "clock.request"
	TypeClockReply      = "clock.reply"
	TypeCommandAck      = "command.ack"
	TypeProtocolError   = "protocol.error"

	TypeStreamOpen = "stream.open"
	TypeStreamEnd  = "stream.end"

	TypeRenderStart    = "render.start"
	TypeRenderEnd      = "render.end"
	TypeRenderCancel   = "render.cancel"
	TypeRenderProgress = "render.progress"
	TypeRenderFinished = "render.finished"

	TypeFocusAcquire = "focus.acquire"
	TypeFocusRenew   = "focus.renew"
	TypeFocusRelease = "focus.release"

	TypeWakeCandidate    = "wake.candidate"
	TypeWakeCandidateEnd = "wake.candidate_end"
	TypeWakeStats        = "wake.stats"

	TypeUplinkOpen  = "uplink.open"
	TypeUplinkRenew = "uplink.renew"
	TypeUplinkClose = "uplink.close"
	TypeUplinkEnded = "uplink.ended"

	TypePrivacyChanged = "privacy.changed"
	TypeButtonAction   = "button.action"

	TypeAlertSnapshot  = "alert.snapshot"
	TypeAlertDelta     = "alert.delta"
	TypeAlertAck       = "alert.ack"
	TypeAlertLocalOp   = "alert.local_operation"
	TypeAlertOpResult  = "alert.op_result"
	TypeAlertAct       = "alert.act"
	TypeAlertRing      = "alert.ring"
	TypeAlertPrefetch  = "alert.prefetch"
	TypeAlertRingEnded = "alert.ring_ended"
	TypeAlertState     = "alert.state"

	// Retained messages (WIRE §4.8): legacy bodies inside the envelope.
	TypeLEDs         = "leds"
	TypeLEDAnim      = "led_anim"
	TypeVolumeSet    = "volume_set"
	TypeConfig       = "config"
	TypeShellOpen    = "shell_open"
	TypeShellClose   = "shell_close"
	TypeWifiChange   = "wifi_change"
	TypeWifiCommit   = "wifi_commit"
	TypeWifiScan     = "wifi_scan"
	TypePing         = "ping"
	TypeVolumeState  = "volume_state"
	TypeAmbientLight = "ambient_light"
	TypeLog          = "log"
	TypeWifiResult   = "wifi_result"
	TypeWifiScanRes  = "wifi_scan_result"
	TypeBLEAdverts   = "ble_adverts"
	TypeStats        = "stats"
	TypePong         = "pong"
)

// Envelope wraps every control message (WIRE §2). SessionID is nil only on
// session.hello.
type Envelope struct {
	Protocol   int             `json:"protocol"`
	Type       string          `json:"type"`
	SessionID  *string         `json:"session_id"`
	MessageID  string          `json:"message_id"`
	DeviceID   string          `json:"device_id"`
	Generation uint32          `json:"generation"`
	Body       json.RawMessage `json:"body"`
}

// NullU64 is a nullable uint64 carried as a decimal string or JSON null.
type NullU64 struct {
	V     uint64
	Valid bool
}

// U64 returns a valid NullU64.
func U64(v uint64) NullU64 { return NullU64{V: v, Valid: true} }

func (n NullU64) MarshalJSON() ([]byte, error) {
	if !n.Valid {
		return []byte("null"), nil
	}
	return json.Marshal(strconv.FormatUint(n.V, 10))
}

func (n *NullU64) UnmarshalJSON(b []byte) error {
	if string(b) == "null" {
		*n = NullU64{}
		return nil
	}
	var s string
	if err := json.Unmarshal(b, &s); err != nil {
		return errors.New("proto: uint64 must be a decimal string")
	}
	v, err := strconv.ParseUint(s, 10, 64)
	if err != nil {
		return errors.New("proto: bad uint64 string")
	}
	*n = NullU64{V: v, Valid: true}
	return nil
}

// ── Session (WIRE §4.1) ─────────────────────────────────────────────────────

type Privacy struct {
	Muted        bool    `json:"muted"`
	CaptureEpoch NullU64 `json:"capture_epoch"`
}

type Volume struct {
	Level  int  `json:"level"`
	Seeded bool `json:"seeded"`
}

// SessionHello's Clock and Alerts come from alerts.Executor (ClockInfo,
// Hello); AmbientLightStatus is the retained als.Report() object.
type SessionHello struct {
	Capabilities       []string        `json:"capabilities"`
	FirmwareVersion    string          `json:"firmware_version"`
	BootID             string          `json:"boot_id"`
	Protocols          []int           `json:"protocols"`
	IP                 string          `json:"ip"`
	AmbientLightStatus json.RawMessage `json:"ambient_light_status"`
	Privacy            Privacy         `json:"privacy"`
	Clock              any             `json:"clock"`
	Alerts             any             `json:"alerts"`
	Assets             []string        `json:"assets"`
	Volume             Volume          `json:"volume"`
}

type SpeechAssets struct {
	RuntimeSHA256 string `json:"runtime_sha256"`
	GraphSHA256   string `json:"graph_sha256"`
	SidecarSHA256 string `json:"sidecar_sha256"`
}

type Thresholds struct {
	Idle     float64 `json:"idle"`
	Playback float64 `json:"playback"`
	NearMiss float64 `json:"near_miss"`
}

type ProvisionalDuck struct {
	DuckDB       float64 `json:"duck_db"`
	MaxPerWindow int     `json:"max_per_window"`
	WindowMs     int64   `json:"window_ms"`
}

type DetectorPolicy struct {
	Thresholds         Thresholds      `json:"thresholds"`
	HopBlocks          int             `json:"hop_blocks"`
	Smoothing          int             `json:"smoothing"`
	ClearAfterUnscored int             `json:"clear_after_unscored"`
	ProvisionalDuck    ProvisionalDuck `json:"provisional_duck"`
}

type SessionReady struct {
	Protocol         int            `json:"protocol"`
	SessionID        string         `json:"session_id"`
	ServerBootID     string         `json:"server_boot_id"`
	CapturePermitted bool           `json:"capture_permitted"`
	Assets           SpeechAssets   `json:"assets"`
	Detector         DetectorPolicy `json:"detector"`
	UTCMs            uint64         `json:"utc_ms,string"`
}

// Session rejection reasons.
const (
	RejectPendingApproval = "pending_approval"
	RejectUnauthorized    = "unauthorized"
	RejectProtocol        = "protocol"
)

type SessionRejected struct {
	Reason string `json:"reason"`
}

type Heartbeat struct {
	MonoNs int64 `json:"mono_ns,string"`
}

// Command acknowledgement statuses.
const (
	AckAccepted = "accepted"
	AckApplied  = "applied"
	AckDurable  = "durable"
	AckRejected = "rejected"
)

type CommandAck struct {
	MessageID string  `json:"message_id"`
	Status    string  `json:"status"`
	Error     *string `json:"error"`
}

type ProtocolError struct {
	Code   string `json:"code"`
	Detail string `json:"detail"`
}

// ── Streams and render (WIRE §4.2) ──────────────────────────────────────────

// Uplink stream IDs; also the uplink lease stream keys.
const (
	StreamMic       = "mic"
	StreamReference = "reference"
	StreamCells     = "cells"
)

// Stream open/end reasons.
const (
	StreamStart         = "start"
	StreamDiscontinuity = "discontinuity"
	StreamPrivacy       = "privacy"
	StreamClockReset    = "clock_reset"
	StreamRenderEpoch   = "render_epoch"
)

type StreamOpen struct {
	StreamID   string `json:"stream_id"`
	Epoch      uint64 `json:"epoch,string"`
	Kind       uint8  `json:"kind"`
	SampleRate uint32 `json:"sample_rate"`
	Format     uint8  `json:"format"`
	Reason     string `json:"reason"`
}

type StreamEnd struct {
	StreamID    string `json:"stream_id"`
	Epoch       uint64 `json:"epoch,string"`
	FinalSample uint64 `json:"final_sample,string"`
	Reason      string `json:"reason"`
}

// RenderStart's Epoch is set only for network sources; LocalAsset only for
// local ones ("builtin:wake_chime", "builtin:fallback" or an alert sha256).
type RenderStart struct {
	PlaybackID   string  `json:"playback_id"`
	SourceClass  string  `json:"source_class"`
	Epoch        NullU64 `json:"epoch"`
	GainDB       float64 `json:"gain_db"`
	Format       uint8   `json:"format"`
	LocalAsset   *string `json:"local_asset"`
	Announcement bool    `json:"announcement"`
}

// RenderEnd ends a network playback. EndFrame is the source frame after the
// last one sent: audio before it may still be in flight on the audio socket.
type RenderEnd struct {
	PlaybackID string  `json:"playback_id"`
	EndFrame   NullU64 `json:"end_frame"`
}

type RenderCancel struct {
	PlaybackID string `json:"playback_id"`
	Reason     string `json:"reason"`
}

// RenderProgress: SeekFrame/MissingFrom/MissingTo/GainDB are present only for
// their event.
type RenderProgress struct {
	PlaybackID        string   `json:"playback_id"`
	Event             string   `json:"event"`
	SubmittedFrames   uint64   `json:"submitted_frames,string"`
	CompletedFrames   uint64   `json:"completed_frames,string"`
	MonoNs            int64    `json:"mono_ns,string"`
	UncertaintyUs     uint32   `json:"uncertainty_us"`
	TimingQuality     string   `json:"timing_quality"`
	ReferenceCoverage string   `json:"reference_coverage"`
	SeekFrame         *NullU64 `json:"seek_frame,omitempty"`
	MissingFrom       *NullU64 `json:"missing_from,omitempty"`
	MissingTo         *NullU64 `json:"missing_to,omitempty"`
	GainDB            *float64 `json:"gain_db,omitempty"`
}

type RenderFinished struct {
	PlaybackID         string `json:"playback_id"`
	LastCompletedFrame uint64 `json:"last_completed_frame,string"`
	Reason             string `json:"reason"`
	TimingQuality      string `json:"timing_quality"`
}

// ── Focus (WIRE §4.3) ───────────────────────────────────────────────────────

type FocusAcquire struct {
	LeaseID string `json:"lease_id"`
	Owner   string `json:"owner"`
	Focus   string `json:"focus"` // dialog_input | dialog_output
	TTLMs   int64  `json:"ttl_ms"`
}

type FocusRenew struct {
	LeaseID string `json:"lease_id"`
	TTLMs   int64  `json:"ttl_ms"`
}

type FocusRelease struct {
	LeaseID string `json:"lease_id"`
}

// ── Wake (WIRE §4.4) ────────────────────────────────────────────────────────

// ActiveAlert is wake.candidate's active_alert (null when nothing is ringing
// or backgrounded). The supervisor adds it beside detector.Candidate's fields.
type ActiveAlert struct {
	ID         string `json:"id"`
	Kind       string `json:"kind"`
	Name       string `json:"name"`
	Foreground bool   `json:"foreground"`
}

// ── Uplink leases (WIRE §4.5) ───────────────────────────────────────────────

// StartLive is the uplink stream start meaning "from now".
const StartLive = "live"

// Uplink reasons and end reasons.
const (
	LeaseCandidate  = "candidate"
	LeaseTurn       = "turn"
	LeaseReply      = "reply"
	LeaseDiagnostic = "diagnostic"

	CloseRejected        = "rejected"
	CloseArbitrationLost = "arbitration_lost"
	CloseCommitted       = "committed"
	CloseClosed          = "closed"

	EndedClosed  = "closed"
	EndedTTL     = "ttl"
	EndedMute    = "mute"
	EndedEpoch   = "epoch"
	EndedOverrun = "overrun"
	EndedSession = "session"
)

// UplinkOpen.Streams maps a stream key to a decimal capture-epoch sample
// index or "live"; an absent key is not wanted.
type UplinkOpen struct {
	LeaseID string            `json:"lease_id"`
	Owner   string            `json:"owner"`
	Reason  string            `json:"reason"`
	Streams map[string]string `json:"streams"`
	TTLMs   int64             `json:"ttl_ms"`
}

// UplinkRenew converts a candidate lease when Reason == "turn" and the
// envelope generation is the lease's + 1.
type UplinkRenew struct {
	LeaseID string `json:"lease_id"`
	TTLMs   int64  `json:"ttl_ms"`
	Reason  string `json:"reason,omitempty"`
	Owner   string `json:"owner,omitempty"`
}

type UplinkClose struct {
	LeaseID string `json:"lease_id"`
	Reason  string `json:"reason"`
}

type UplinkEnded struct {
	LeaseID      string             `json:"lease_id"`
	Reason       string             `json:"reason"`
	LastSample   map[string]NullU64 `json:"last_sample"`
	ClippedStart map[string]NullU64 `json:"clipped_start"`
}

// ── Physical events (WIRE §4.6) ─────────────────────────────────────────────

type PrivacyChanged struct {
	Muted        bool    `json:"muted"`
	CaptureEpoch NullU64 `json:"capture_epoch"`
	PhysicalSeq  uint64  `json:"physical_seq"`
}

// HandledAlertStopped is button.action's handled value when the device
// stopped a ringing or backgrounded alert itself.
const HandledAlertStopped = "alert_stopped"

type ButtonAction struct {
	ClickType     int     `json:"click_type"`
	Button        string  `json:"button"`
	Down          bool    `json:"down"`
	HeldMs        int64   `json:"held_ms"`
	Muted         bool    `json:"muted"`
	MonoNs        int64   `json:"mono_ns,string"`
	CaptureEpoch  NullU64 `json:"capture_epoch"`
	CaptureSample NullU64 `json:"capture_sample"`
	PhysicalSeq   uint64  `json:"physical_seq"`
	OccurrenceID  *string `json:"occurrence_id"`
	Handled       *string `json:"handled"`
}

// ── Alerts (WIRE §4.7) ──────────────────────────────────────────────────────

type AlertAct struct {
	OpID     string `json:"op_id"`
	TargetID string `json:"target_id"`
	Action   string `json:"action"` // dismiss | snooze
	Source   string `json:"source"`
}

// AlertPrefetch names alert sounds (SHA-256) to install before anything
// rings them: alert.ring names the timer sound only when the timer finishes.
type AlertPrefetch struct {
	Sounds []string `json:"sounds"`
}
