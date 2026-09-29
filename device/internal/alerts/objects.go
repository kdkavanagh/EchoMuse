package alerts

import (
	"encoding/json"
	"fmt"
	"strconv"
	"time"
	"unicode/utf8"

	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/uuid"
)

// occurrence is one alarm occurrence of the device cache (SPEC §16.4). JSON
// names are the canonical object's keys; due_utc_ms is a decimal string.
type occurrence struct {
	DueLocal     string   `json:"due_local"`
	DueUTCMs     int64    `json:"due_utc_ms,string"`
	Kind         Kind     `json:"kind"` // alarm|snooze
	Label        string   `json:"label"`
	LoopGapMs    int64    `json:"loop_gap_ms"`
	MaxRingMs    int64    `json:"max_ring_ms"`
	OccurrenceID string   `json:"occurrence_id"`
	RampMs       int64    `json:"ramp_ms"`
	Revision     uint64   `json:"revision"`
	ScheduleID   string   `json:"schedule_id"`
	SnoozeMs     int64    `json:"snooze_ms"`
	Sound        string   `json:"sound"`  // 64-hex SHA-256 or SoundFallback
	Volume       *float64 `json:"volume"` // nil: ring at the current media volume
}

// TombstoneReason says why the controller removed an occurrence (SPEC §16.4).
type TombstoneReason string

const (
	tombDismissed TombstoneReason = "dismissed"
	tombSnoozed   TombstoneReason = "snoozed"
	tombExpired   TombstoneReason = "expired"
	tombDeleted   TombstoneReason = "deleted"
)

// tombstone removes an occurrence from the cache (SPEC §16.4).
type tombstone struct {
	OccurrenceID string
	Revision     uint64
	Reason       TombstoneReason
}

// SoundFallback names the built-in fallback tone (SPEC §16.3, §16.5).
const SoundFallback = "builtin:fallback"

const (
	// SPEC §16.3: snooze child summary prefix and label cap.
	snoozeLabelPrefix = "Snoozed: "
	maxLabelRunes     = 120

	// isoSeconds is ISO-8601 with numeric offset, second precision (SPEC §16.3 occurrence key).
	isoSeconds = "2006-01-02T15:04:05-07:00"
)

// parseObject validates one decoded cache object: a tombstone when it has a
// "tombstone" key, otherwise an occurrence. Unknown keys are ignored.
func parseObject(v any) (*occurrence, *tombstone, error) {
	m, ok := v.(map[string]any)
	if !ok {
		return nil, nil, fmt.Errorf("alerts: cache object is not a JSON object")
	}
	id, err := uuidField(m, "occurrence_id")
	if err != nil {
		return nil, nil, err
	}
	rev, err := intField(m, "revision")
	if err != nil {
		return nil, nil, err
	}
	if _, isTomb := m["tombstone"]; isTomb {
		s, err := stringField(m, "tombstone")
		if err != nil {
			return nil, nil, err
		}
		switch reason := TombstoneReason(s); reason {
		case tombDismissed, tombSnoozed, tombExpired, tombDeleted:
			return nil, &tombstone{OccurrenceID: id, Revision: uint64(rev), Reason: reason}, nil
		default:
			return nil, nil, fmt.Errorf("alerts: unknown tombstone %q", s)
		}
	}
	o := &occurrence{OccurrenceID: id, Revision: uint64(rev)}
	if o.ScheduleID, err = uuidField(m, "schedule_id"); err != nil {
		return nil, nil, err
	}
	if o.DueLocal, err = stringField(m, "due_local"); err != nil {
		return nil, nil, err
	}
	if _, err := time.Parse(time.RFC3339, o.DueLocal); err != nil {
		return nil, nil, fmt.Errorf("alerts: due_local %q: %w", o.DueLocal, err)
	}
	due, err := stringField(m, "due_utc_ms")
	if err != nil {
		return nil, nil, err
	}
	if o.DueUTCMs, err = strconv.ParseInt(due, 10, 64); err != nil || o.DueUTCMs < 0 {
		return nil, nil, fmt.Errorf("alerts: due_utc_ms %q", due)
	}
	kind, err := stringField(m, "kind")
	if err != nil {
		return nil, nil, err
	}
	o.Kind = Kind(kind)
	if o.Kind != KindAlarm && o.Kind != KindSnooze {
		return nil, nil, fmt.Errorf("alerts: kind %q", o.Kind)
	}
	if o.Label, err = stringField(m, "label"); err != nil {
		return nil, nil, err
	}
	if utf8.RuneCountInString(o.Label) > maxLabelRunes {
		return nil, nil, fmt.Errorf("alerts: label exceeds %d characters", maxLabelRunes)
	}
	for _, f := range []struct {
		key string
		dst *int64
	}{{"loop_gap_ms", &o.LoopGapMs}, {"max_ring_ms", &o.MaxRingMs}, {"ramp_ms", &o.RampMs}, {"snooze_ms", &o.SnoozeMs}} {
		if *f.dst, err = intField(m, f.key); err != nil {
			return nil, nil, err
		}
	}
	if o.MaxRingMs == 0 {
		return nil, nil, fmt.Errorf("alerts: max_ring_ms is zero")
	}
	if o.Sound, err = stringField(m, "sound"); err != nil {
		return nil, nil, err
	}
	if !validSound(o.Sound) {
		return nil, nil, fmt.Errorf("alerts: sound %q", o.Sound)
	}
	vol, present := m["volume"]
	if !present {
		return nil, nil, fmt.Errorf("alerts: missing volume")
	}
	if vol != nil {
		n, ok := vol.(json.Number)
		if !ok {
			return nil, nil, fmt.Errorf("alerts: volume is not a number")
		}
		f, err := n.Float64()
		// SPEC §16.5: audio admission requires nonzero alert gain.
		if err != nil || !(f > 0 && f <= 1) {
			return nil, nil, fmt.Errorf("alerts: volume %s outside (0, 1]", n)
		}
		o.Volume = &f
	}
	return o, nil, nil
}

func validSound(s string) bool {
	return s == SoundFallback || assets.IsSHA256(s)
}

func stringField(m map[string]any, key string) (string, error) {
	s, ok := m[key].(string)
	if !ok {
		return "", fmt.Errorf("alerts: %s is not a string", key)
	}
	return s, nil
}

// intField reads a non-negative JSON integer.
func intField(m map[string]any, key string) (int64, error) {
	n, ok := m[key].(json.Number)
	if !ok {
		return 0, fmt.Errorf("alerts: %s is not a number", key)
	}
	v, err := strconv.ParseInt(string(n), 10, 64)
	if err != nil || v < 0 {
		return 0, fmt.Errorf("alerts: %s = %s is not a non-negative integer", key, n)
	}
	return v, nil
}

// uuidField reads a UUID in Python's canonical lowercase form.
func uuidField(m map[string]any, key string) (string, error) {
	s, err := stringField(m, key)
	if err != nil {
		return "", err
	}
	u, err := uuid.Parse(s)
	if err != nil || u.String() != s {
		return "", fmt.Errorf("alerts: %s %q is not a lowercase uuid", key, s)
	}
	return s, nil
}

// SnoozeChildScheduleID is UUIDv5(NAMESPACE_URL, "echomuse-snooze:<parent occurrence_id>") (SPEC §16.3).
func SnoozeChildScheduleID(parentOccurrenceID string) string {
	return uuid.V5(uuid.NamespaceURL, "echomuse-snooze:"+parentOccurrenceID).String()
}

// OccurrenceID is UUIDv5(schedule_id, occurrence key) (SPEC §16.3).
func OccurrenceID(scheduleID, key string) (string, error) {
	ns, err := uuid.Parse(scheduleID)
	if err != nil {
		return "", err
	}
	return uuid.V5(ns, key).String(), nil
}

// snoozeChild derives the snooze child of parent pressed at pressUTCMs (SPEC
// §16.3): due at the press rounded up to the next whole second plus the
// parent's frozen snooze_ms; its key, due_local, uses the parent's UTC offset
// (the device has no timezone database) at second precision.
func snoozeChild(parent *occurrence, pressUTCMs int64) (*occurrence, error) {
	pt, err := time.Parse(time.RFC3339, parent.DueLocal)
	if err != nil {
		return nil, fmt.Errorf("alerts: parent due_local %q: %w", parent.DueLocal, err)
	}
	_, offset := pt.Zone()
	due := ceilSecondMs(pressUTCMs) + parent.SnoozeMs
	dueLocal := time.UnixMilli(due).In(time.FixedZone("", offset)).Format(isoSeconds)
	sched := SnoozeChildScheduleID(parent.OccurrenceID)
	occID, err := OccurrenceID(sched, dueLocal)
	if err != nil {
		return nil, err
	}
	label := []rune(snoozeLabelPrefix + parent.Label)
	if len(label) > maxLabelRunes {
		label = label[:maxLabelRunes]
	}
	return &occurrence{
		DueLocal:     dueLocal,
		DueUTCMs:     due,
		Kind:         KindSnooze,
		Label:        string(label),
		LoopGapMs:    parent.LoopGapMs,
		MaxRingMs:    parent.MaxRingMs,
		OccurrenceID: occID,
		RampMs:       parent.RampMs,
		ScheduleID:   sched,
		SnoozeMs:     parent.SnoozeMs,
		Sound:        parent.Sound,
		Volume:       parent.Volume,
	}, nil
}

func ceilSecondMs(ms int64) int64 {
	if r := ms % 1000; r != 0 {
		return ms + 1000 - r
	}
	return ms
}
