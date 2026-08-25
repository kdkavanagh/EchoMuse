package client

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"log"
	"math"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	"github.com/wilbowes/EchoMuse/internal/config"
	"github.com/wilbowes/EchoMuse/internal/wakeword/shadow"
	"github.com/wilbowes/EchoMuse/pkg/mic"
	"github.com/wilbowes/EchoMuse/pkg/speaker"
)

// ─── Binary frame types ───────────────────────────────────────────────────────

const (
	frameTypeMic     = byte(0x01)
	frameTypeSpeaker = byte(0x02)
	frameTypeEOS     = byte(0x03)
	// Music rides its own frame types so the device can hold it on a second
	// plane and mix it under voice rather than pausing it. An older
	// controller simply never sends these; a newer one only sends them to a
	// device announcing "audio_mix".
	frameTypeMusic    = byte(0x04)
	frameTypeMusicEOS = byte(0x05)
	frameTypeVADEnd  = byte(0x04)
	// frameTypeNoSpeechTimeout signals that the turn ended because no speech
	// was ever detected — distinct from frameTypeVADEnd (speech detected,
	// then ended). Sent when noSpeechTimeout elapses with active==false the
	// entire time. Distinguishing the two lets the controller treat "wake
	// word then silence" (Alexa-equivalent: quietly give up) differently
	// from "spoke, pipeline processed it, HA had nothing to say" — the two
	// cases were previously indistinguishable on the wire, which is also
	// why the controller had no way to short-circuit the former without
	// risking mishandling the latter.
	frameTypeNoSpeechTimeout = byte(0x05)
)

// ─── WebSocket keepalive (data + control) ─────────────────────────────────────
//
// Neither long-lived socket had transport-level liveness before v2.8.4. The
// data client blocked in ReadMessage with no read deadline and sent no pings,
// so a silently dropped TCP connection (WiFi blip / AP roam — no FIN or RST
// ever delivered) left it half-open forever: connect() never returned, Run()'s
// redial loop never started, and the device kept a zombie data channel — deaf
// and mute — while the control socket kept it looking healthy. Observed
// 2026-07-14: Office wedged this way for 7h; the controller's defensive
// mic_start fired every 10s against a stream writing into the dead socket.
// The control client's app-level pong ticker had the write-side half of the
// same bug: on write error it returned without closing the conn, leaving its
// read loop wedged identically.
//
// Pings prove the full round trip (device → controller → device); the pong
// handler refreshes the read deadline, so a dead path errors the read loop
// out within wsPongWait and the redial loop takes over. Write deadlines bound
// every write so a full kernel send buffer can't hold connMu forever.
const (
	wsPingInterval = 20 * time.Second // WS ping cadence
	wsPongWait     = 45 * time.Second // read deadline; > 2× ping interval
	wsWriteWait    = 10 * time.Second // per-write deadline (frames, identify, pings)
)

// ─── VAD constants ────────────────────────────────────────────────────────────

const (
	vadOwwChunkBytes = 1280 * 2 // 2560 bytes = 80ms

	// micPeriodMs is the cadence the mic backend delivers at — one OpenSL ES
	// period, which slmic sizes to match the wire frame exactly. Used only to
	// size the preroll ring; every window that matters is computed from the
	// buffer actually received, so a backend that changed this would still be
	// handled correctly.
	micPeriodMs = 80

	// prerollBudgetMs is how much pre-gate audio is retained while the VAD
	// gate is closed and flushed upstream the moment it opens. The ring is
	// sized in wall-clock terms rather than as a period count, because a fixed
	// count drifts with the delivery cadence (the old prerollPeriods=16 was
	// meant as ~512ms of periods but actually held 2.5s of them). Only applies to lockMic
	// (bounded turn) streams — the always-on wake stream is ungated and
	// sends everything, so OWW always sees a continuous stream. For turns,
	// preroll gives STT the true first phoneme instead of a hard splice at
	// gate-open.
	prerollBudgetMs = 512

	// noSpeechTimeout bounds how long streamMic will wait for speech to
	// ever be detected after a turn starts. If active never becomes true
	// within this window, the turn ends via frameTypeNoSpeechTimeout rather
	// than sitting open indefinitely — mirrors Alexa's behaviour of giving
	// up quickly on a wake word followed by silence, rather than depending
	// on the upstream pipeline's own (much longer, HA VAD-driven) timeout.
	// Only guards the "never spoke" case; once active==true this deadline
	// no longer applies — the existing silenceMax hysteresis owns speech
	// end-of-turn detection from that point on.
	noSpeechTimeout = 5 * time.Second
)

// noSpeechTimeoutForTest overrides noSpeechTimeout when non-zero — set only
// from tests, to avoid needing a real 5s wait per test run. Left at its
// zero value in production; streamMic falls back to the real constant.
var noSpeechTimeoutForTest time.Duration

func effectiveNoSpeechTimeout() time.Duration {
	if noSpeechTimeoutForTest > 0 {
		return noSpeechTimeoutForTest
	}
	return noSpeechTimeout
}

func vadPeriodRMS(mono []byte) float64 {
	n := len(mono) / 2
	if n == 0 {
		return 0
	}
	var sum float64
	for i := 0; i < n; i++ {
		s := int16(binary.LittleEndian.Uint16(mono[i*2:]))
		f := float64(s) / 32768.0
		sum += f * f
	}
	return math.Sqrt(sum / float64(n))
}

// ─── DataClient ───────────────────────────────────────────────────────────────

type DataClient struct {
	deviceID string
	mic      mic.Subscribable
	spk      speaker.Speaker

	readyCh chan string

	micMu     sync.Mutex
	micActive bool
	micStopCh chan struct{}
	// micConn is the connection the active stream writes to — connect()'s
	// exit cleanup only stops the mic if the stream is its own (see the
	// defer in connect for the zombie-stream incident this guards against).
	micConn *websocket.Conn

	conn   *websocket.Conn
	connMu sync.Mutex

	// shadowScorer scores the always-on wake stream on the device without
	// acting on it (internal/wakeword/shadow), nil when off. Guarded because
	// a config push swaps it from the control goroutine while the mic
	// goroutine is pushing frames into it.
	shadowMu     sync.Mutex
	shadowScorer *shadow.Scorer
}

// NewDataClient wires the mic/speaker pipeline. Every period the microphone
// hands over is already one fully processed mono channel — Android's audio HAL
// ran per-mic AEC, beamforming and gain before EchoMuse saw it (see
// docs/native-afe-migration.md) — so this client only frames, gates and sends;
// it does no signal processing of its own.
func NewDataClient(deviceID string, microphone mic.Subscribable, spk speaker.Speaker) *DataClient {
	return &DataClient{
		deviceID: deviceID,
		mic:      microphone,
		spk:      spk,
		readyCh:  make(chan string, 1),
	}
}

// SetShadowScorer installs (or removes, with nil) the on-device wake word
// scorer. Any previous scorer is closed, which releases its ONNX Runtime
// sessions — a config push that changes the wake model rebuilds it, and
// leaking a set of sessions per change would be a slow death on a device with
// 1GB of storage and less RAM.
//
// Returns the scorer it replaced, already closed, purely so callers can log the
// transition.
func (d *DataClient) SetShadowScorer(s *shadow.Scorer) {
	d.shadowMu.Lock()
	old := d.shadowScorer
	d.shadowScorer = s
	d.shadowMu.Unlock()
	if old != nil {
		old.Close()
	}
}

// ShadowScorer returns the active scorer, or nil.
func (d *DataClient) ShadowScorer() *shadow.Scorer {
	d.shadowMu.Lock()
	defer d.shadowMu.Unlock()
	return d.shadowScorer
}

func (d *DataClient) NotifyReady(serverAddr string) {
	select {
	case d.readyCh <- serverAddr:
	default:
		select {
		case <-d.readyCh:
		default:
		}
		d.readyCh <- serverAddr
	}
}

func (d *DataClient) StartMic(lockMic bool) {
	d.micMu.Lock()
	defer d.micMu.Unlock()
	if d.micActive {
		log.Println("[data] StartMic: already active — ignoring")
		return
	}
	d.connMu.Lock()
	conn := d.conn
	d.connMu.Unlock()
	if conn == nil {
		log.Println("[data] StartMic: no connection yet")
		return
	}
	d.micActive = true
	d.micStopCh = make(chan struct{})
	d.micConn = conn
	go d.streamMic(conn, d.micStopCh, lockMic)
	log.Println("[data] Mic streaming started")
}

func (d *DataClient) StopMic() {
	d.micMu.Lock()
	defer d.micMu.Unlock()
	if !d.micActive {
		return
	}
	close(d.micStopCh)
	d.micActive = false
	// beam.Unlock() is deferred inside streamMic — always runs on the mic
	// goroutine, eliminating the data race with Process() and Lock().
	log.Println("[data] Mic streaming stopped")
}

func (d *DataClient) Run(ctx context.Context) error {
	var lastAddr string
	for {
		var addr string
		if lastAddr == "" {
			// No previous address — block until control signals us.
			select {
			case <-ctx.Done():
				return ctx.Err()
			case addr = <-d.readyCh:
			}
		} else {
			// Lost connection — retry same addr after 5s, or use new addr
			// immediately if control signals one (e.g. controller moved).
			select {
			case <-ctx.Done():
				return ctx.Err()
			case addr = <-d.readyCh:
			case <-time.After(5 * time.Second):
				addr = lastAddr
			}
		}
		// Drain any stale addr queued while we were connected.
		select {
		case addr = <-d.readyCh:
		default:
		}
		lastAddr = addr
		log.Printf("[data] Connecting to %s", addr)
		if err := d.connect(ctx, addr); err != nil && err != context.Canceled {
			log.Printf("[data] Connection lost: %v — retrying", err)
		}
	}
}

// connect dials baseURL+"/data" — baseURL ("ws://…" or "wss://…") comes
// from the control client via NotifyReady, so both planes always ride the
// same listener. Credentials are re-read per dial (see tlscreds.go).
func (d *DataClient) connect(ctx context.Context, baseURL string) error {
	creds := loadLinkCreds()
	dialer := creds.dialer()
	conn, _, err := dialer.DialContext(ctx, baseURL+"/data", creds.header())
	if err != nil {
		return err
	}
	defer conn.Close()

	identifyBytes, _ := json.Marshal(map[string]string{
		"type":      "identify",
		"device_id": d.deviceID,
	})
	// Send identify BEFORE publishing conn — same ordering fix as the control
	// client's register message. StartMic can fire independently of controller
	// timing (unmute calls StartMic(false) from the button goroutine); once
	// d.conn is visible, streamMic's sendFrame writes under connMu, and this
	// unlocked write racing it would be a concurrent write on the same
	// gorilla conn (panics).
	conn.SetWriteDeadline(time.Now().Add(wsWriteWait))
	if err := conn.WriteMessage(websocket.TextMessage, identifyBytes); err != nil {
		return err
	}
	log.Printf("[data] Identified as %s", d.deviceID)

	d.connMu.Lock()
	d.conn = conn
	d.connMu.Unlock()

	// Exit cleanup is ownership-guarded (2026-07-16): the control client
	// cancels this connection's context and spawns a replacement data.Run on
	// every control reconnect, so by the time this defer runs a replacement
	// connection may already be live with its own mic stream — an unguarded
	// close(micStopCh)/conn=nil here would kill the *new* stream / unpublish
	// the *new* conn. (Observed as the Office zombie-stream incident: the
	// unguarded half of this bug was the reverse case — see the ctx watcher
	// below.)
	defer func() {
		d.micMu.Lock()
		if d.micActive && d.micConn == conn {
			close(d.micStopCh)
			d.micActive = false
		}
		d.micMu.Unlock()

		d.connMu.Lock()
		if d.conn == conn {
			d.conn = nil
		}
		d.connMu.Unlock()
	}()

	// Keepalive — see the wsPingInterval block comment. Deadline refreshes
	// all happen on this (the read) goroutine: the pong handler runs inside
	// ReadMessage, and the per-message refresh below covers connections busy
	// with speaker traffic.
	conn.SetReadDeadline(time.Now().Add(wsPongWait))
	conn.SetPongHandler(func(string) error {
		conn.SetReadDeadline(time.Now().Add(wsPongWait))
		return nil
	})
	done := make(chan struct{})
	defer close(done)

	// Context watcher — cancellation must tear down an ESTABLISHED
	// connection, not just abort a dial. The control client cancels this
	// context on every control-WS reconnect; before this watcher existed,
	// a control-only drop (data TCP path still healthy) left this
	// connection — and its mic stream — alive as a zombie: the stream held
	// micActive against a socket the controller had already superseded, so
	// every subsequent mic_start was refused with "already active" and the
	// device was deaf to wake words until something sent mic_stop (Office,
	// 2026-07-16, 4.7h). Closing conn errors the read loop out; the exit
	// defer then releases the mic stream for the replacement connection.
	go func() {
		select {
		case <-done:
		case <-ctx.Done():
			log.Println("[data] context cancelled — closing connection")
			conn.Close()
		}
	}()

	go func() {
		ticker := time.NewTicker(wsPingInterval)
		defer ticker.Stop()
		for {
			select {
			case <-done:
				return
			case <-ticker.C:
				// WriteControl is safe concurrently with WriteMessage
				// (gorilla's documented exception), so no connMu here — a
				// mic-frame write wedged on a full send buffer can't block
				// the ping that would detect the dead path.
				if err := conn.WriteControl(websocket.PingMessage, nil, time.Now().Add(wsWriteWait)); err != nil {
					log.Printf("[data] keepalive ping failed: %v — closing connection", err)
					conn.Close() // unblock the read loop now, not at the read deadline
					return
				}
			}
		}
	}()

	for {
		msgType, data, err := conn.ReadMessage()
		if err != nil {
			return err
		}
		conn.SetReadDeadline(time.Now().Add(wsPongWait))
		if msgType != websocket.BinaryMessage || len(data) == 0 {
			continue
		}
		switch data[0] {
		case frameTypeSpeaker:
			if len(data) > 1 && d.spk != nil {
				if err := d.spk.PumpPeriod(data[1:]); err != nil {
					log.Printf("[data] PumpPeriod error: %v", err)
				}
			}
		case frameTypeEOS:
			log.Println("[data] Speaker: end of stream")
			if d.spk != nil {
				d.spk.EndStream()
			}
		case frameTypeMusic:
			if len(data) > 1 && d.spk != nil {
				if err := d.spk.PumpMusic(data[1:]); err != nil {
					log.Printf("[data] PumpMusic error: %v", err)
				}
			}
		case frameTypeMusicEOS:
			log.Println("[data] Music: end of stream")
			if d.spk != nil {
				d.spk.EndMusicStream()
			}
		default:
			log.Printf("[data] Unknown binary frame type: 0x%02x", data[0])
		}
	}
}

// streamMic subscribes to the mic, runs the processing pipeline, and streams
// binary frames to the controller. The always-on wake stream (!lockMic) is
// ungated and AGC-free: every period is sent, continuously. Bounded turn
// streams (lockMic) keep the VAD gate, preroll ring, end-of-speech sentinels,
// and no-speech timer.
func (d *DataClient) streamMic(conn *websocket.Conn, stopCh <-chan struct{}, lockMic bool) {
	if d.mic == nil {
		log.Println("[data] streamMic: no mic")
		return
	}

	// Clear micActive on exit regardless of why we stopped — StopMic, stopCh
	// signal, or ALSA stream death. Without this, a mic death leaves micActive=true
	// and StartMic silently refuses to restart.
	//
	// Ownership check (2026-07-06): only clear micActive if this goroutine is
	// still the current stream. StopMic→StartMic in quick succession (the
	// controller sends that pair after every voice turn) spawns a replacement
	// goroutine while this one is still draining its last few periods;
	// without the check, this defer then stamped micActive=false over the
	// replacement's true, and the NEXT mic_start spawned a second concurrent
	// stream that no StopMic could ever reach (micStopCh no longer points at
	// it). Leaked gated streams are silent while idle but transmit during
	// speech — every utterance reached the controller twice (STT heard
	// "turn on on the on the office…") and their VADEnd sentinels cleared
	// the OWW chunk buffer, progressively killing wake detection until the
	// process restarted. d.micStopCh is compared against our own stopCh as
	// the identity token: they're equal only if no StartMic ran after us.
	defer func() {
		d.micMu.Lock()
		if d.micStopCh == stopCh {
			d.micActive = false
		}
		d.micMu.Unlock()
		log.Println("[data] streamMic: exited")
	}()

	ch := d.mic.Subscribe()
	defer d.mic.Unsubscribe(ch)

	cfg := config.Get()

	speechCount := 0
	silenceCount := 0
	active := false
	everActive := false // true once active has been true at least once this turn
	buf := make([]byte, 0, vadOwwChunkBytes*4)
	// preroll ring — mono periods captured while the gate is closed, oldest
	// first. Flushed into buf at gate open, cleared while active. Slices are
	// retained (not copied): the recorder hands out a fresh allocation per
	// period (opensl.Recorder.onComplete copies out of the hardware slot
	// before re-enqueuing it), so nothing aliases them.
	preroll := make([][]byte, 0, prerollBudgetMs/micPeriodMs+1)
	var seqNum uint16
	var periodCount uint64 // periodic RMS diagnostic

	// On-device shadow scoring. The pointer is re-read as the stream runs, not
	// captured once: a config push builds a new Scorer and restarts nothing, so
	// capturing it here meant enabling on-device scoring did nothing at all
	// until the next StartMic — which on an idle device is the next voice turn,
	// and never if the wake word it is meant to detect is the thing that would
	// cause one. Measured: `[shadow] on-device wake word scoring` in the log,
	// dev_frames=0 in the database, and a scorer costing 0% CPU.
	//
	// Reset only when the pointer CHANGES, which covers both cases that matter:
	// a fresh stream (StopMic/StartMic after every voice turn, whose gap the
	// detector must not splice across) and a newly installed scorer picked up
	// mid-stream.
	shadowScorer := d.ShadowScorer()
	if shadowScorer != nil {
		shadowScorer.Reset()
	}

	sendFrame := func(payload []byte) {
		frame := make([]byte, 3+len(payload))
		frame[0] = frameTypeMic
		binary.BigEndian.PutUint16(frame[1:3], seqNum)
		seqNum++
		copy(frame[3:], payload)
		d.connMu.Lock()
		conn.SetWriteDeadline(time.Now().Add(wsWriteWait))
		err := conn.WriteMessage(websocket.BinaryMessage, frame)
		d.connMu.Unlock()
		if err != nil {
			log.Printf("[data] streamMic: send error: %v", err)
			// Any write error leaves a gorilla conn permanently broken —
			// close it so connect()'s read loop unblocks and Run() redials
			// immediately instead of waiting out the read deadline.
			conn.Close()
		}
	}

	// noSpeechTimer fires if speech is never detected within noSpeechTimeout
	// of turn start. Stopped (and its channel drained) the instant active
	// first becomes true — from that point on, end-of-turn is entirely
	// owned by the existing silenceMax hysteresis below, same as before
	// this change.
	//
	// Only armed when lockMic is true. Per SETUP.md's mic_start semantics:
	// mic_start with no lock_mic is the permanent, always-on ch6/omni
	// wake-word listening stream (started once at connect, meant to run
	// indefinitely) — mic_start with lock_mic:true is a bounded voice turn
	// (post-wake-word or button press, perimeter mic locked for the turn's
	// duration). The no-speech timeout is only meaningful for the latter;
	// arming it unconditionally silently killed the permanent listening
	// stream after 5s of ordinary silence, with nothing to restart it,
	// breaking wake-word detection entirely until a button press happened
	// to re-enter streamMic fresh. Confirmed against real device logs
	// before this fix: "no speech detected within timeout" fired 5s after
	// every idle-listening Mic streaming started, with no corresponding
	// StartMic call to bring it back.
	//
	// When not armed, noSpeechTimerC stays nil — a nil channel blocks
	// forever in a select, which is the idiomatic Go way to permanently
	// disable a select case at zero runtime cost.
	var noSpeechTimer *time.Timer
	var noSpeechTimerC <-chan time.Time
	if lockMic {
		noSpeechTimer = time.NewTimer(effectiveNoSpeechTimeout())
		defer noSpeechTimer.Stop()
		noSpeechTimerC = noSpeechTimer.C
	}

	for {
		select {
		case <-stopCh:
			return

		case <-noSpeechTimerC:
			// Timer firing implies active was never true — if it had been,
			// this case would already be unreachable (timer stopped below).
			// Unreachable entirely when !lockMic, since noSpeechTimerC is
			// nil in that case and a nil channel never becomes ready.
			log.Println("[data] streamMic: no speech detected within timeout — ending turn")
			sendFrame([]byte{frameTypeNoSpeechTimeout})
			return

		case raw, ok := <-ch:
			if !ok {
				return
			}
			// Stop has priority: select picks randomly among ready cases,
			// so without this a closed stopCh racing a ready mic channel
			// keeps this goroutine draining periods alongside its
			// replacement stream.
			select {
			case <-stopCh:
				return
			default:
			}

			snap := cfg.Snapshot()
			threshold := snap.VadThreshold

			// `raw` IS the finished mono period. Android's audio HAL ran
			// per-mic AEC, the fixed and adaptive beamformers, SNR beam
			// selection and its own AGC before this ever reached Go
			// (docs/native-afe-migration.md), so there is nothing left here
			// to filter, steer or amplify — this loop only measures, gates
			// and frames. There is no per-frame beam angle either: the AFE
			// picks a beam internally and reports it only in ASP debug
			// output, which is why the LED direction arc went with the old
			// pipeline.
			mono := raw

			// VAD on the audio as delivered. vadThreshold is an absolute
			// level against that stream — there is no gain stage of ours in
			// front of it to scale for, but note the number is NOT
			// comparable with the pre-AFE fleet's: the HAL's PGA and the
			// AFE's +7.2dB output gain sit where micGainDb used to, so this
			// wants tuning by measurement rather than carrying a value over.
			rms := vadPeriodRMS(mono)
			speech := rms >= threshold

			// Gate windows in units of actual iterations: divide the
			// configured ms by the duration of the buffer that actually
			// arrived rather than by an assumed period length. (The old /32
			// assumed 32ms periods against 160ms batches and silently made
			// both windows 5× longer than configured.)
			batchMs := len(mono) / 32 // S16 mono @16kHz: 32 bytes per ms
			if batchMs < 1 {
				batchMs = 1
			}
			speechNeeded := (snap.VadSpeechMs + batchMs - 1) / batchMs
			if speechNeeded < 1 {
				speechNeeded = 1
			}
			silenceMax := (snap.VadSilenceMs + batchMs - 1) / batchMs
			if silenceMax < 1 {
				silenceMax = 1
			}

			// Periodic RMS diagnostic — every ~10 min. Was every 100 counts
			// (~16s measured on-device) while idle capture levels were being
			// characterised; that job is done (2026-07-07 fleet analysis)
			// and /tmp/server.log is RAM-backed and unrotated.
			if periodCount%7500 == 0 {
				log.Printf("[data] VAD diag: rms=%.5f threshold=%.5f gate=%v active=%v",
					rms, threshold, speech, active)
			}
			periodCount++

			// Ungated wake stream: the always-on (!lockMic) stream sends
			// every period, batched into 80ms chunks — no VAD
			// gate, no preroll, no end-of-speech sentinels. openwakeword
			// is a streaming model whose internal mel-spectrogram buffer
			// assumes continuous audio; feeding it VAD-gated bursts spliced
			// together (even with preroll) measurably depresses scores, and
			// an absolute RMS threshold is wrong in at least one room of
			// every home. Bandwidth is a non-issue: 16kHz mono S16 is
			// 32KB/s, ~12.5 frames/s at this chunk size — 6× smaller than
			// the TTS playback stream. Turn endpointing for wake-triggered
			// turns is owned controller-side (HA STT_VAD_END in esphome
			// mode, plus the controller's own no-speech timeout); the RMS
			// gate below now serves only bounded lockMic turns.
			if !lockMic {
				buf = append(buf, mono...)
				for len(buf) >= vadOwwChunkBytes {
					// Score the SAME bytes on the SAME 80ms boundaries the
					// controller receives, so a device/controller score
					// difference can only be the engine and not the framing.
					// PushBytes never blocks: it drops when the scorer is
					// behind rather than delaying this loop, which reads
					// 160ms ALSA batches out of a 160ms-deep ring.
					if sc := d.ShadowScorer(); sc != shadowScorer {
						// Adopted mid-stream, or dropped. A new Scorer is
						// already reset; resetting here as well is harmless and
						// keeps "the detector never splices across a change"
						// true in one place.
						shadowScorer = sc
						if shadowScorer != nil {
							shadowScorer.Reset()
						}
					}
					if shadowScorer != nil {
						shadowScorer.PushBytes(buf[:vadOwwChunkBytes])
					}
					sendFrame(buf[:vadOwwChunkBytes])
					buf = buf[vadOwwChunkBytes:]
				}
				continue
			}

			if speech {
				silenceCount = 0
				if !active {
					speechCount++
					if speechCount >= speechNeeded {
						active = true
						// Gate open — flush the preroll ring ahead of the
						// current period so the controller receives ~500ms of
						// pre-onset context (and the true start of speech,
						// including periods consumed by the speechNeeded
						// count-up) instead of a hard splice at onset.
						for _, p := range preroll {
							buf = append(buf, p...)
						}
						preroll = preroll[:0]
						if !everActive {
							everActive = true
							// Speech has genuinely started — the no-speech
							// grace period no longer applies (if it was ever
							// armed; nil when !lockMic — see construction
							// above). Stop the timer; drain per
							// time.Timer.Stop's documented pattern in case
							// it raced and already fired.
							if noSpeechTimer != nil {
								if !noSpeechTimer.Stop() {
									select {
									case <-noSpeechTimerC:
									default:
									}
								}
							}
						}
					}
				}
			} else {
				speechCount = 0
				if active {
					silenceCount++
					if silenceCount >= silenceMax {
						active = false
						silenceCount = 0
						if len(buf) > 0 {
							pad := make([]byte, vadOwwChunkBytes-len(buf)%vadOwwChunkBytes)
							buf = append(buf, pad...)
							for len(buf) >= vadOwwChunkBytes {
								sendFrame(buf[:vadOwwChunkBytes])
								buf = buf[vadOwwChunkBytes:]
							}
							buf = buf[:0]
						}
						sendFrame([]byte{frameTypeVADEnd})
					}
				}
			}

			if active {
				buf = append(buf, mono...)
				for len(buf) >= vadOwwChunkBytes {
					sendFrame(buf[:vadOwwChunkBytes])
					buf = buf[vadOwwChunkBytes:]
				}
			} else {
				// Gate closed — keep the most recent batches for the next
				// gate open, capped by duration rather than count.
				prerollMax := prerollBudgetMs / batchMs
				if prerollMax < 1 {
					prerollMax = 1
				}
				for len(preroll) >= prerollMax {
					copy(preroll, preroll[1:])
					preroll = preroll[:len(preroll)-1]
				}
				preroll = append(preroll, mono)
			}
		}
	}
}
