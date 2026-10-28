// Package proto holds the device protocol v1 control-plane wire types
// (docs/protocol-v1.md). Field names are the WIRE's verbatim. uint64 values
// travel as JSON decimal strings (`,string` tags, or NullU64 when nullable).
//
// Bodies already defined by the package that produces them are not repeated
// here: detector.Candidate/CandidateEnd/Stats (wake.*), alerts.AlertState,
// RingEnded, LocalOperation, OpResult, TimerRing, ClockRequest/ClockReply,
// AlertAck and the snapshot/delta pages. Fields drawn from a closed
// vocabulary use the owning package's type (render.SourceClass,
// focus.Kind, alerts.Action, ...) or one of the named string types below;
// they encode exactly as the strings they name.
package proto

import (
	"encoding/json"
	"errors"
	"strconv"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/bindings/als"
	"github.com/wilbowes/EchoMuse/internal/bluetooth"
	"github.com/wilbowes/EchoMuse/internal/focus"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/wifi"
	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
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

// Capability is a session.hello capability name.
type Capability string

// Capabilities (SPEC §11.1) plus the retained hardware capabilities.
const (
	CapAudioTimeline       Capability = "audio_timeline_v1"
	CapUplinkSubscriptions Capability = "uplink_leases_v1"
	CapDeviceWake          Capability = "device_wake_v1"
	CapRenderReference     Capability = "render_reference_v1"
	CapRenderProgress      Capability = "render_progress_v1"
	CapFocusLeases         Capability = "focus_leases_v1"
	CapAlertCache          Capability = "alert_cache_v1"
	CapTurnProtocol        Capability = "turn_protocol_v1"

	CapLEDs         Capability = "leds"
	CapLEDAnim      Capability = "led_anim"
	CapButtons      Capability = "buttons"
	CapButtonHold   Capability = "button_hold"
	CapAmbientLight Capability = "ambient_light"

	// CapAlertPrefetch: the device installs the sounds alert.prefetch names.
	CapAlertPrefetch Capability = "alert_prefetch"
	// CapLocalWakeChime: the device plays the wake chime itself at candidate
	// open when config wakeSound is on, and reports it in wake.candidate's
	// chimed.
	CapLocalWakeChime Capability = "local_wake_chime"
	// CapNoInputChime: render.start accepts the earcon local_asset
	// "builtin:no_input_chime", which ends a wake turn that heard no command.
	CapNoInputChime Capability = "no_input_chime"
	// CapAnnouncementChime: render.start accepts the earcon local_asset
	// "builtin:announcement_chime", which precedes an announcement.
	CapAnnouncementChime Capability = "announcement_chime"
	// CapOpenRules: the device opens candidates on session.ready's
	// detector.open_rules, evaluates its shadow_rules, credits the opening
	// rule in wake.candidate and reports shadow counters in wake.stats.
	CapOpenRules Capability = "open_rules_v1"
	// CapAFEMetadata: the capture carries the native AFE's per-frame
	// metadata (Fire OS 6 micAsr, v3.3) and its decoder is validating
	// frames. On session.ready's afe_metadata the device announces the afe
	// stream, serves it under audio subscriptions and reports wake.stats.afe.
	CapAFEMetadata Capability = "afe_metadata_v1"
	// CapNearMiss: the device reports every near-miss episode (the smoothed
	// score reached session.ready's near_miss floor and no candidate opened)
	// in wake.near_miss and serves uplink subscriptions with reason near_miss.
	CapNearMiss Capability = "near_miss_v1"

	// Optional platform capabilities. A device announces each only where
	// the firmware implements it on the image it runs.
	//
	// CapWifiControl: the device joins Wi-Fi itself and handles wifi_change,
	// wifi_scan and wifi_commit, answering wifi_result / wifi_scan_result
	// (the Dot; Android owns Wi-Fi on the Show).
	CapWifiControl Capability = "wifi_control"
	// CapBLEProxy: the device runs the BLE advertisement scanner, sends
	// ble_adverts and honours config bleProxyEnabled (the Dot).
	CapBLEProxy Capability = "ble_proxy"
	// CapPrivacyMute: the device integrates a hardware privacy mute and
	// reports privacy state (the Dot; deferred on the Show, which reports
	// privacy.muted false).
	CapPrivacyMute Capability = "privacy_mute"
	// CapScreenWake: when a wake candidate opens, the device lights the kiosk
	// screen when config screenWake is on (the Show): through Fully Kiosk's
	// local REST API, or through the companion app on a screen_overlay device.
	CapScreenWake Capability = "screen_wake"
	// CapScreenOverlay: the device keeps the EchoMuse companion app running
	// as a light bar over the kiosk, painting the composited ring there and
	// taking touches in its stop/cancel zone as action-button presses; it
	// lights the screen through the app, not Fully's REST API (the
	// LineageOS Show). Whether the app is up right now is stats.companion.
	CapScreenOverlay Capability = "screen_overlay"
	// CapTurnView: the device accepts turn.view and shows it full screen
	// through its Kiosk Satellite plugin, honouring config turnViewLinger
	// (the LineageOS Show; docs/echo-show-companion-plan.md §11).
	CapTurnView Capability = "turn_view_v1"
	// CapTimerView: the device accepts timer.view and relays it to its
	// Kiosk Satellite plugin, which draws each running timer as a progress
	// ring over the page (the LineageOS Show).
	CapTimerView Capability = "timer_view_v1"
	// CapKioskPlugin: the device installs and enables that plugin through
	// Kiosk Satellite's remote API with config kioskToken and kioskPort,
	// and reports it in stats.companion.kiosk (the LineageOS Show).
	CapKioskPlugin Capability = "kiosk_plugin"
)

// Platform is session.hello's platform: the system image this firmware runs
// on, detected once at start (internal/platform). The audio backend depends
// on both the image and the product, so the value names both; the
// controller admits a session only when it names one of these.
type Platform string

const (
	// PlatformFireOS6 is the Echo Dot Gen 2 on Fire OS 6 (biscuit_puffin):
	// audio through Amazon's mixer daemon (libmixerAPI).
	PlatformFireOS6 Platform = "fireos6"
	// PlatformFireOS6Crown is the Echo Show 8 (crown) on Fire OS 6 with the
	// Android framework: audio through OpenSL ES and AudioFlinger.
	PlatformFireOS6Crown Platform = "fireos6_crown"
	// PlatformLineage18Crown is the same Echo Show 8 on LineageOS 18.1
	// (Android 11): audio through AAudio and AudioFlinger, STREAM_MUSIC
	// 0..15 behind other binder codes.
	PlatformLineage18Crown Platform = "lineage18_crown"
)

// MessageType is an envelope type.
type MessageType string

// Message types (WIRE §4).
const (
	TypeSessionHello    MessageType = "session.hello"
	TypeSessionReady    MessageType = "session.ready"
	TypeSessionRejected MessageType = "session.rejected"
	TypeHeartbeat       MessageType = "heartbeat"
	TypeClockRequest    MessageType = "clock.request"
	TypeClockReply      MessageType = "clock.reply"
	TypeCommandAck      MessageType = "command.ack"
	TypeProtocolError   MessageType = "protocol.error"

	TypeStreamOpen MessageType = "stream.open"
	TypeStreamEnd  MessageType = "stream.end"

	TypeRenderStart    MessageType = "render.start"
	TypeRenderEnd      MessageType = "render.end"
	TypeRenderCancel   MessageType = "render.cancel"
	TypeRenderProgress MessageType = "render.progress"
	TypeRenderFinished MessageType = "render.finished"

	TypeFocusAcquire MessageType = "focus.acquire"
	TypeFocusRenew   MessageType = "focus.renew"
	TypeFocusRelease MessageType = "focus.release"

	TypeWakeCandidate    MessageType = "wake.candidate"
	TypeWakeCandidateEnd MessageType = "wake.candidate_end"
	TypeWakeStats        MessageType = "wake.stats"
	// TypeWakeNearMiss is D→C, generation 0, sent only in a session: one
	// closed near-miss episode (near_miss_v1).
	TypeWakeNearMiss MessageType = "wake.near_miss"

	TypeUplinkOpen  MessageType = "uplink.open"
	TypeUplinkRenew MessageType = "uplink.renew"
	TypeUplinkClose MessageType = "uplink.close"
	TypeUplinkEnded MessageType = "uplink.ended"

	TypePrivacyChanged MessageType = "privacy.changed"
	TypeButtonAction   MessageType = "button.action"

	TypeAlertSnapshot  MessageType = "alert.snapshot"
	TypeAlertDelta     MessageType = "alert.delta"
	TypeAlertAck       MessageType = "alert.ack"
	TypeAlertLocalOp   MessageType = "alert.local_operation"
	TypeAlertOpResult  MessageType = "alert.op_result"
	TypeAlertAct       MessageType = "alert.act"
	TypeAlertRing      MessageType = "alert.ring"
	TypeAlertPrefetch  MessageType = "alert.prefetch"
	TypeAlertRingEnded MessageType = "alert.ring_ended"
	TypeAlertState     MessageType = "alert.state"

	// TypeTurnView is C→D, generation 0, never acked: the whole view of the
	// current turn (turn_view_v1), which the firmware relays unchanged to
	// its Kiosk Satellite plugin.
	TypeTurnView MessageType = "turn.view"

	// TypeTimerView is C→D, generation 0, never acked: the whole list of
	// the speaker's timers (timer_view_v1), which the firmware relays
	// unchanged to its Kiosk Satellite plugin.
	TypeTimerView MessageType = "timer.view"

	// Retained messages (WIRE §4.8): legacy bodies inside the envelope.
	TypeLEDs         MessageType = "leds"
	TypeLEDAnim      MessageType = "led_anim"
	TypeVolumeSet    MessageType = "volume_set"
	TypeConfig       MessageType = "config"
	TypeShellOpen    MessageType = "shell_open"
	TypeShellClose   MessageType = "shell_close"
	TypeWifiChange   MessageType = "wifi_change"
	TypeWifiCommit   MessageType = "wifi_commit"
	TypeWifiScan     MessageType = "wifi_scan"
	TypePing         MessageType = "ping"
	TypeVolumeState  MessageType = "volume_state"
	TypeAmbientLight MessageType = "ambient_light"
	TypeLog          MessageType = "log"
	TypeWifiResult   MessageType = "wifi_result"
	TypeWifiScanRes  MessageType = "wifi_scan_result"
	TypeBLEAdverts   MessageType = "ble_adverts"
	TypeStats        MessageType = "stats"
	TypePong         MessageType = "pong"
)

// Envelope wraps every control message (WIRE §2). SessionID is nil only on
// session.hello.
type Envelope struct {
	Protocol   int             `json:"protocol"`
	Type       MessageType     `json:"type"`
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

// Volume is session.hello's volume. Level is the wire level 0..127; nil
// (JSON null) means the device cannot control volume this run (the Echo
// Show when its Android volume check failed at start). A Dot always sends a
// number.
type Volume struct {
	Level  *int `json:"level"`
	Seeded bool `json:"seeded"`
}

// SessionHello's Clock and Alerts come from alerts.Executor (ClockInfo,
// Hello); AmbientLightStatus is the retained als.Report() object.
type SessionHello struct {
	Capabilities       []Capability       `json:"capabilities"`
	Platform           Platform           `json:"platform"`
	FirmwareVersion    string             `json:"firmware_version"`
	OSVersion          string             `json:"os_version"`
	BootID             string             `json:"boot_id"`
	Protocols          []int              `json:"protocols"`
	IP                 string             `json:"ip"`
	AmbientLightStatus *als.Status        `json:"ambient_light_status"`
	Privacy            Privacy            `json:"privacy"`
	Clock              alerts.ClockInfo   `json:"clock"`
	Alerts             alerts.HelloAlerts `json:"alerts"`
	Assets             []string           `json:"assets"`
	Volume             Volume             `json:"volume"`
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

// OpenRule is one detector.open_rules / shadow_rules entry.
type OpenRule struct {
	Profile   string  `json:"profile"`
	Windows   int     `json:"windows"`
	Combine   string  `json:"combine"`
	Threshold float64 `json:"threshold"`
}

type DetectorPolicy struct {
	Thresholds         Thresholds      `json:"thresholds"`
	HopBlocks          int             `json:"hop_blocks"`
	Smoothing          int             `json:"smoothing"`
	ClearAfterUnscored int             `json:"clear_after_unscored"`
	ProvisionalDuck    ProvisionalDuck `json:"provisional_duck"`
	// OpenRules is nil when absent (an older controller): the baseline rules
	// derive from Thresholds. ShadowRules absent means none.
	OpenRules   []OpenRule `json:"open_rules"`
	ShadowRules []OpenRule `json:"shadow_rules"`
}

type SessionReady struct {
	Protocol         int            `json:"protocol"`
	SessionID        string         `json:"session_id"`
	ServerBootID     string         `json:"server_boot_id"`
	CapturePermitted bool           `json:"capture_permitted"`
	Assets           SpeechAssets   `json:"assets"`
	Detector         DetectorPolicy `json:"detector"`
	UTCMs            uint64         `json:"utc_ms,string"`
	// AFEMetadata opts this session into afe_metadata_v1; a controller
	// sends it only to a device that announced the capability. Absent
	// (older controllers): nothing of it is sent.
	AFEMetadata bool `json:"afe_metadata"`
}

// RejectReason is a session.rejected reason.
type RejectReason string

// Session rejection reasons.
const (
	RejectPendingApproval RejectReason = "pending_approval"
	RejectUnauthorized    RejectReason = "unauthorized"
	RejectProtocol        RejectReason = "protocol"
)

type SessionRejected struct {
	Reason RejectReason `json:"reason"`
}

type Heartbeat struct {
	MonoNs int64 `json:"mono_ns,string"`
}

// AckStatus is a command.ack status.
type AckStatus string

// Command acknowledgement statuses.
const (
	AckAccepted AckStatus = "accepted"
	AckApplied  AckStatus = "applied"
	AckDurable  AckStatus = "durable"
	AckRejected AckStatus = "rejected"
)

// AckCode is a command.ack rejection code; alert.act uses the alert
// executor's codes (alerts.ActError).
type AckCode string

type CommandAck struct {
	MessageID string    `json:"message_id"`
	Status    AckStatus `json:"status"`
	Error     *AckCode  `json:"error"`
}

// ErrorCode is a protocol.error code.
type ErrorCode string

// Protocol error codes the device sends.
const (
	ErrMalformedMessage ErrorCode = "malformed_message"
	ErrMalformedFrame   ErrorCode = "malformed_frame"
)

type ProtocolError struct {
	Code   ErrorCode `json:"code"`
	Detail string    `json:"detail"`
}

// ── Streams and render (WIRE §4.2) ──────────────────────────────────────────

// StreamID names an uplink stream; also the uplink subscription stream keys.
type StreamID string

// Uplink stream IDs.
const (
	StreamMic       StreamID = "mic"
	StreamReference StreamID = "reference"
	StreamCells     StreamID = "cells"
	StreamAFE       StreamID = "afe" // afe_metadata_v1 sessions only
)

// StreamReason gives the reason a stream epoch opened or ended.
type StreamReason string

// Stream open/end reasons.
const (
	StreamStart         StreamReason = "start"
	StreamDiscontinuity StreamReason = "discontinuity"
	StreamPrivacy       StreamReason = "privacy"
	StreamClockReset    StreamReason = "clock_reset"
	StreamRenderEpoch   StreamReason = "render_epoch"
)

type StreamOpen struct {
	StreamID   StreamID     `json:"stream_id"`
	Epoch      uint64       `json:"epoch,string"`
	Kind       uint8        `json:"kind"`
	SampleRate uint32       `json:"sample_rate"`
	Format     uint8        `json:"format"`
	Reason     StreamReason `json:"reason"`
}

type StreamEnd struct {
	StreamID    StreamID     `json:"stream_id"`
	Epoch       uint64       `json:"epoch,string"`
	FinalSample uint64       `json:"final_sample,string"`
	Reason      StreamReason `json:"reason"`
}

// RenderStart's Epoch is set only for network sources; LocalAsset only for
// local ones ("builtin:wake_chime", "builtin:no_input_chime",
// "builtin:announcement_chime", "builtin:fallback" or an alert sha256).
type RenderStart struct {
	PlaybackID   string             `json:"playback_id"`
	SourceClass  render.SourceClass `json:"source_class"`
	Epoch        NullU64            `json:"epoch"`
	GainDB       float64            `json:"gain_db"`
	Format       uint8              `json:"format"`
	LocalAsset   *string            `json:"local_asset"`
	Announcement bool               `json:"announcement"`
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
	PlaybackID        string                   `json:"playback_id"`
	Event             render.ProgressEvent     `json:"event"`
	SubmittedFrames   uint64                   `json:"submitted_frames,string"`
	CompletedFrames   uint64                   `json:"completed_frames,string"`
	MonoNs            int64                    `json:"mono_ns,string"`
	UncertaintyUs     uint32                   `json:"uncertainty_us"`
	TimingQuality     render.TimingQuality     `json:"timing_quality"`
	ReferenceCoverage render.ReferenceCoverage `json:"reference_coverage"`
	SeekFrame         *NullU64                 `json:"seek_frame,omitempty"`
	MissingFrom       *NullU64                 `json:"missing_from,omitempty"`
	MissingTo         *NullU64                 `json:"missing_to,omitempty"`
	GainDB            *float64                 `json:"gain_db,omitempty"`
}

type RenderFinished struct {
	PlaybackID         string               `json:"playback_id"`
	LastCompletedFrame uint64               `json:"last_completed_frame,string"`
	Reason             render.FinishReason  `json:"reason"`
	TimingQuality      render.TimingQuality `json:"timing_quality"`
}

// ── Focus (WIRE §4.3) ───────────────────────────────────────────────────────

type FocusAcquire struct {
	LeaseID string     `json:"lease_id"`
	Owner   string     `json:"owner"`
	Focus   focus.Kind `json:"focus"`
	TTLMs   int64      `json:"ttl_ms"`
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
	ID         string      `json:"id"`
	Kind       alerts.Kind `json:"kind"`
	Name       string      `json:"name"`
	Foreground bool        `json:"foreground"`
}

// AFEStats is wake.stats.afe (afe_metadata_v1 sessions only): the AFE
// metadata decoder's health over the stats window, counted whether or not
// capture is privacy-muted. Expected frames = Periods × 10.
type AFEStats struct {
	Periods    uint64 `json:"periods"`
	Frames     uint64 `json:"frames"`
	Invalid    uint64 `json:"invalid"`
	Syncs      uint64 `json:"syncs"`
	Gaps       uint64 `json:"gaps"`
	LostFrames uint64 `json:"lost_frames"`
}

// ── Uplink subscriptions (WIRE §4.5) ────────────────────────────────────────
//
// The wire calls a subscription a lease, so the JSON key stays lease_id for
// compatibility with every firmware and controller version.

// StartLive is the uplink stream start meaning "from now".
const StartLive = "live"

// SubscriptionReason is an uplink subscription's purpose.
type SubscriptionReason string

// Uplink subscription reasons.
const (
	SubscriptionCandidate  SubscriptionReason = "candidate"
	SubscriptionTurn       SubscriptionReason = "turn"
	SubscriptionReply      SubscriptionReason = "reply"
	SubscriptionDiagnostic SubscriptionReason = "diagnostic"
	// SubscriptionNearMiss uploads a near-miss episode's microphone audio
	// (near_miss_v1). It has no other effect: no duck, no focus, and it is
	// not a diagnostic subscription.
	SubscriptionNearMiss SubscriptionReason = "near_miss"
)

// CloseReason is an uplink.close reason.
type CloseReason string

// Uplink close reasons.
const (
	CloseRejected        CloseReason = "rejected"
	CloseArbitrationLost CloseReason = "arbitration_lost"
	CloseCommitted       CloseReason = "committed"
	CloseClosed          CloseReason = "closed"
)

// EndedReason is an uplink.ended reason.
type EndedReason string

// Uplink end reasons.
const (
	EndedClosed  EndedReason = "closed"
	EndedTTL     EndedReason = "ttl"
	EndedMute    EndedReason = "mute"
	EndedEpoch   EndedReason = "epoch"
	EndedOverrun EndedReason = "overrun"
	EndedSession EndedReason = "session"
)

// UplinkOpen.Streams maps a stream key to a decimal capture-epoch sample
// index or "live"; an absent key is not wanted.
type UplinkOpen struct {
	SubscriptionID string              `json:"lease_id"`
	Owner          string              `json:"owner"`
	Reason         SubscriptionReason  `json:"reason"`
	Streams        map[StreamID]string `json:"streams"`
	TTLMs          int64               `json:"ttl_ms"`
}

// UplinkRenew converts a candidate subscription when Reason == "turn" and
// the envelope generation is the subscription's + 1.
type UplinkRenew struct {
	SubscriptionID string             `json:"lease_id"`
	TTLMs          int64              `json:"ttl_ms"`
	Reason         SubscriptionReason `json:"reason,omitempty"`
	Owner          string             `json:"owner,omitempty"`
}

type UplinkClose struct {
	SubscriptionID string      `json:"lease_id"`
	Reason         CloseReason `json:"reason"`
}

type UplinkEnded struct {
	SubscriptionID string               `json:"lease_id"`
	Reason         EndedReason          `json:"reason"`
	LastSample     map[StreamID]NullU64 `json:"last_sample"`
	ClippedStart   map[StreamID]NullU64 `json:"clipped_start"`
}

// ── Physical events (WIRE §4.6) ─────────────────────────────────────────────

type PrivacyChanged struct {
	Muted        bool    `json:"muted"`
	CaptureEpoch NullU64 `json:"capture_epoch"`
	PhysicalSeq  uint64  `json:"physical_seq"`
}

// Handled is button.action's handled value: what the device did with the
// press itself.
type Handled string

// HandledAlertStopped: the device stopped a ringing or backgrounded alert.
const HandledAlertStopped Handled = "alert_stopped"

type ButtonAction struct {
	ClickType     pkgbuttons.ClickType  `json:"click_type"`
	Button        pkgbuttons.ButtonType `json:"button"`
	Down          bool                  `json:"down"`
	HeldMs        int64                 `json:"held_ms"`
	Muted         bool                  `json:"muted"`
	MonoNs        int64                 `json:"mono_ns,string"`
	CaptureEpoch  NullU64               `json:"capture_epoch"`
	CaptureSample NullU64               `json:"capture_sample"`
	PhysicalSeq   uint64                `json:"physical_seq"`
	OccurrenceID  *string               `json:"occurrence_id"`
	Handled       *Handled              `json:"handled"`
}

// ── Alerts (WIRE §4.7) ──────────────────────────────────────────────────────

type AlertAct struct {
	OpID     string        `json:"op_id"`
	TargetID string        `json:"target_id"`
	Action   alerts.Action `json:"action"` // dismiss | snooze
	Source   alerts.Source `json:"source"`
}

// AlertPrefetch names alert sounds (SHA-256) to install before anything
// rings them: alert.ring names the timer sound only when the timer finishes.
type AlertPrefetch struct {
	Sounds []string `json:"sounds"`
}

// ── Retained device reports (WIRE §4.8) ─────────────────────────────────────

// Stats is the retained stats body. Unavailable measurements use nil rather
// than a numeric sentinel (SPEC §8.1).
type Stats struct {
	CPUPct           float64          `json:"cpuPct"`
	MemUsedMb        int              `json:"memUsedMb"`
	MemTotalMb       int              `json:"memTotalMb"`
	StorageUsedMb    int              `json:"storageUsedMb"`
	StorageTotalMb   int              `json:"storageTotalMb"`
	WifiRssi         *int             `json:"wifiRssi"`
	WifiSsid         string           `json:"wifiSsid"`
	LinkSpeedMbps    int              `json:"linkSpeedMbps,omitempty"`
	WifiFreqMhz      int              `json:"wifiFreqMhz,omitempty"`
	WifiBssid        string           `json:"wifiBssid,omitempty"`
	TxBytes          uint64           `json:"txBytes"`
	RxBytes          uint64           `json:"rxBytes"`
	TxErrors         uint64           `json:"txErrors"`
	TxDropped        uint64           `json:"txDropped"`
	RxCrcErrors      uint64           `json:"rxCrcErrors"`
	Ble              *bluetooth.Stats `json:"ble,omitempty"`
	AmbientLux       *int             `json:"ambientLux"`
	CPUTempC         *float64         `json:"cpuTempC"`
	MaxTempC         *float64         `json:"maxTempC"`
	CoresOnline      int              `json:"coresOnline,omitempty"`
	CoresTotal       int              `json:"coresTotal,omitempty"`
	ThermalCoreLimit int              `json:"thermalCoreLimit,omitempty"`
	// Companion is the screen app's health; present only on a
	// screen_overlay device.
	Companion *CompanionStats `json:"companion,omitempty"`
}

// CompanionStats is stats.companion: each field null when unknown.
type CompanionStats struct {
	// Version is the app's version from its last hello this run.
	Version *string `json:"version"`
	// Linked: an app connection is up past hello.
	Linked *bool `json:"linked"`
	// Overlay is the app's last reported SYSTEM_ALERT_WINDOW op state.
	Overlay *bool `json:"overlay"`
	// Error is the last installer or link error, null when none.
	Error *string `json:"error"`
	// Kiosk is Kiosk Satellite and its EchoMuse plugin (kiosk_plugin).
	Kiosk *KioskStats `json:"kiosk"`
}

// KioskStats is stats.companion.kiosk: each field null when unknown.
type KioskStats struct {
	// Installed: the Kiosk Satellite package is present.
	Installed *bool `json:"installed"`
	// Version is its versionName.
	Version *string `json:"version"`
	// RemoteAdmin: its GET /api/health answered on kioskPort.
	RemoteAdmin *bool            `json:"remote_admin"`
	Plugin      KioskPluginStats `json:"plugin"`
}

// KioskPluginStats is stats.companion.kiosk.plugin.
type KioskPluginStats struct {
	// Version is the plugin version last seen: Kiosk Satellite's
	// listPlugins record, the one this firmware installed, or the
	// plugin's link hello.
	Version *string `json:"version"`
	// Linked: a plugin connection is up past hello.
	Linked *bool `json:"linked"`
	// Error is null, KioskTokenRejected, KioskNoToken or free text (an
	// installPlugin refusal, a link mismatch).
	Error *string `json:"error"`
	// SwitchedOn: this firmware run turned Kiosk Satellite's plugin master
	// switch on; false once checked otherwise.
	SwitchedOn *bool `json:"switched_on"`
}

// The stats.companion.kiosk.plugin.error codes the controller acts on.
const (
	// KioskTokenRejected: Kiosk Satellite answered 401 to kioskToken.
	KioskTokenRejected = "token_rejected"
	// KioskNoToken: Kiosk Satellite is installed with remote administration
	// up, and no kioskToken is configured.
	KioskNoToken = "no_token"
)

// WifiResult is the retained wifi_result body; error is always present, ""
// on success.
type WifiResult struct {
	Error string `json:"error"`
	OK    bool   `json:"ok"`
	SSID  string `json:"ssid"`
}

// WifiScanResult is the retained wifi_scan_result body; networks is null
// when the scan failed.
type WifiScanResult struct {
	Error    string         `json:"error"`
	Networks []wifi.Network `json:"networks"`
}

// BLEAdverts is the retained ble_adverts body.
type BLEAdverts struct {
	Adverts []bluetooth.Advert `json:"adverts"`
}

// AmbientLight is the retained ambient_light body.
type AmbientLight struct {
	Lux int `json:"lux"`
}

// LogLevel is a retained log message's level.
type LogLevel string

const LogInfo LogLevel = "info"

// Log is the retained log body: a device log line for the controller.
type Log struct {
	Level   LogLevel `json:"level"`
	Message string   `json:"message"`
}
