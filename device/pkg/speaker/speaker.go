package speaker

type Speaker interface {
	Init() error
	PumpPeriod(data []byte) error
	// EndStream marks the current audio stream as complete, so the driver
	// can distinguish "channel drained because playback finished" from a
	// mid-stream underrun.
	EndStream()
	// Flush discards queued-but-unplayed audio immediately (barge-in).
	Flush()

	// ── music plane ───────────────────────────────────────────────────────
	// A second, independent stream, mixed with the voice plane at the ALSA
	// write. It exists so a voice turn can DUCK music rather than pausing
	// it: the controller runs its music feed ~4s ahead of realtime, so the
	// next four seconds are already on the device when a wake word fires,
	// and audio that has left the controller cannot be ducked there.
	PumpMusic(data []byte) error
	EndMusicStream()
	// FlushMusic is for the user genuinely stopping or pausing. A voice turn
	// must duck instead — flushing throws away the buffered audio that makes
	// ducking instant, and on a non-seekable stream it cannot be recovered.
	FlushMusic()
	// SetDuck sets music attenuation in dB while voice plays (0 = none).
	SetDuck(db float64)

	Close()
}

// StreamStats is the per-stream delivery report every backend emits once, at
// EOS. Shared here (rather than living only in internal/bindings/speaker)
// because the native-AFE backend (internal/bindings/slspeaker) has to report
// the SAME shape — the schema v7 delivery instrumentation and the dashboard's
// Activity tab are built on it regardless of which backend produced it (see
// docs/native-afe-migration.md's "Instrumentation must survive"). Report
// these properly or report nothing: a backend that fakes zeros for values it
// never measured makes every device on this path look perfect.
type StreamStats struct {
	Periods     uint64 `json:"periods"`
	Underruns   uint64 `json:"underruns"`
	MinDepth    int    `json:"minDepth"`
	PrimeWaitMs int64  `json:"primeWaitMs"`
	RecvSpanMs  int64  `json:"recvSpanMs"`
	MaxGapMs    int64  `json:"maxGapMs"`
	BytesRecv   uint64 `json:"bytesRecv"`
}

// FullSpeaker is the superset cmd/server.go actually wires up, beyond the
// core playback contract every backend must implement: per-stream delivery
// stats, whether a voice stream is mid-flight (drives the on-device shadow
// wake word's barge-in threshold), and re-enabling the speaker amp after a
// headphone plug is removed (internal/bindings/jack). Both the tinyalsa and
// native-AFE backends implement it, so cmd/server.go's backend-selection
// switch can hold one interface-typed variable rather than duplicating every
// call site per backend.
type FullSpeaker interface {
	Speaker
	OnStreamStats(cb func(StreamStats))
	IsStreaming() bool
	EnableSpeakerAmp()

	// PlayCue plays a short device-local notification sound (48kHz mono
	// S16_LE) on a third plane, mixed with voice and music at the same
	// write point — see internal/cue for why a cue is deliberately not a
	// stream and must not travel through the voice plane.
	//
	// On FullSpeaker rather than Speaker because a cue is played from
	// cmd/server.go, which already holds this interface, while
	// internal/server and internal/client take the narrower Speaker: a cue
	// is not part of the playback contract those two depend on.
	PlayCue(pcm []byte)
}
