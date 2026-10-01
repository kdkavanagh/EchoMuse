package supervisor

import (
	"context"
	"encoding/json"
	"log"

	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/config"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/server"
	"github.com/wilbowes/EchoMuse/pkg/led"
)

// v1Capabilities are the protocol capabilities this firmware implements
// (SPEC §11.1); alert_cache_v1 is added only while the wakelock works.
var v1Capabilities = []proto.Capability{
	proto.CapAudioTimeline, proto.CapUplinkLeases, proto.CapDeviceWake, proto.CapRenderReference,
	proto.CapRenderProgress, proto.CapFocusLeases, proto.CapTurnProtocol,
}

// retainedCapabilities are the retained hardware capabilities (§18.2).
var retainedCapabilities = []proto.Capability{proto.CapLEDs, proto.CapLEDAnim, proto.CapButtons, proto.CapButtonHold}

// Hello builds a fresh session.hello (WIRE §4.1).
func (s *Supervisor) Hello() proto.SessionHello {
	caps := append([]proto.Capability(nil), v1Capabilities...)
	if s.ex.CacheCapable() {
		caps = append(caps, proto.CapAlertCache)
	}
	caps = append(caps, retainedCapabilities...)
	caps = append(caps, proto.CapAlertPrefetch, proto.CapLocalWakeChime)
	if s.cfg.AmbientReadable() {
		caps = append(caps, proto.CapAmbientLight)
	}
	installed, err := s.cfg.SpeechStore.List()
	if err != nil {
		log.Printf("[session] speech assets: %v", err)
	}
	if installed == nil {
		installed = []string{}
	}
	s.announceMu.Lock()
	epoch := s.micEpoch
	s.announceMu.Unlock()
	privacy := proto.Privacy{Muted: s.cfg.Physical.IsMuted()}
	if epoch != 0 {
		privacy.CaptureEpoch = proto.U64(epoch)
	}
	level, seeded := s.cfg.Physical.VolumeState()
	return proto.SessionHello{
		Capabilities:       caps,
		FirmwareVersion:    s.cfg.FirmwareVersion,
		BootID:             s.cfg.BootID,
		Protocols:          []int{proto.Version},
		IP:                 s.cfg.IP(),
		AmbientLightStatus: s.cfg.AmbientStatus(),
		Privacy:            privacy,
		Clock:              s.ex.ClockInfo(),
		Alerts:             s.ex.Hello(),
		Assets:             installed,
		Volume:             proto.Volume{Level: level, Seeded: seeded},
	}
}

// Ready implements client.Handler.
func (s *Supervisor) Ready(sess *client.Session, r proto.SessionReady) { s.ready(sess, r) }

// ready publishes the session, announces the current stream epochs,
// uploads pending local alert operations before anything else alert-related
// (§10.6 step 5), reports alert.state, and activates the named speech
// assets. capture_permitted=false changes nothing on the device: the
// controller refuses candidates itself.
func (s *Supervisor) ready(sess Session, r proto.SessionReady) {
	ctx, cancel := context.WithCancel(context.Background())
	s.capMu.Lock()
	s.announceMu.Lock()
	s.mu.Lock()
	s.session, s.sessCtx, s.sessCancel = sess, ctx, cancel
	clear(s.unfetchable)
	s.mu.Unlock()
	s.up.Attach(sess.Audio(), s.sendUplink)
	if s.micEpoch != 0 {
		s.openCaptureStreamsLocked()
	}
	if s.refEpoch != 0 {
		s.openReferenceStreamLocked()
	}
	s.announceMu.Unlock()
	s.capMu.Unlock()

	for _, op := range s.ex.PendingOperations() {
		s.logSend(proto.TypeAlertLocalOp, 0, op)
	}
	s.logSend(proto.TypeAlertState, 0, s.ex.State())
	if level, seeded := s.cfg.Physical.VolumeState(); seeded {
		s.logSend(proto.TypeVolumeState, 0, volumeState{Level: level})
	}
	s.cfg.Physical.SetLinkState(server.LinkUp)
	s.activate(ctx, sess, r)
	if s.cfg.Retained.Ready != nil {
		s.cfg.Retained.Ready()
	}
}

// Rejected implements client.Handler; pending approval pulses white.
func (s *Supervisor) Rejected(reason proto.RejectReason) {
	if reason == proto.RejectPendingApproval {
		s.cfg.Physical.SetLinkState(server.LinkPending)
		return
	}
	s.cfg.Physical.SetLinkState(server.LinkDown)
}

// Lost implements client.Handler: every dialog/uplink lease and network
// playback ends; local alerts, privacy and physical stop continue (§16.1).
func (s *Supervisor) Lost(reason client.LostReason) {
	log.Printf("[session] lost: %s", reason)
	s.endSession()
	s.cfg.Physical.SetLinkState(server.LinkDown)
}

func (s *Supervisor) endSession() {
	s.announceMu.Lock()
	s.mu.Lock()
	cancel := s.sessCancel
	s.session, s.sessCtx, s.sessCancel = nil, nil, nil
	clear(s.candidateAck)
	clear(s.candidates)
	clear(s.leases)
	s.mu.Unlock()
	s.announceMu.Unlock()
	if cancel != nil {
		cancel()
	}
	s.up.Detach()
	s.fm.EndSession()
	s.mix.EndSession()
	s.ex.ClockSessionLost()
	s.cfg.Physical.ClearControllerLEDs()
}

// Control implements client.Handler for every C→D message the link does
// not consume. Unknown types are ignored (WIRE preamble).
func (s *Supervisor) Control(env proto.Envelope) {
	switch env.Type {
	case proto.TypeCommandAck:
		var a proto.CommandAck
		if err := json.Unmarshal(env.Body, &a); err == nil {
			s.candidateAcked(a)
		}
	case proto.TypeRenderStart:
		s.renderStart(env)
	case proto.TypeRenderEnd:
		s.renderEnd(env)
	case proto.TypeRenderCancel:
		s.renderCancel(env)
	case proto.TypeFocusAcquire:
		s.focusAcquire(env)
	case proto.TypeFocusRenew:
		s.focusRenew(env)
	case proto.TypeFocusRelease:
		s.focusRelease(env)
	case proto.TypeUplinkOpen:
		s.uplinkOpen(env)
	case proto.TypeUplinkRenew:
		s.uplinkRenew(env)
	case proto.TypeUplinkClose:
		s.uplinkClose(env)
	case proto.TypeAlertSnapshot:
		s.alertSnapshot(env)
	case proto.TypeAlertDelta:
		s.alertDelta(env)
	case proto.TypeAlertAct:
		s.alertAct(env)
	case proto.TypeAlertRing:
		s.alertRing(env)
	case proto.TypeAlertPrefetch:
		s.alertPrefetch(env)
	case proto.TypeAlertOpResult:
		s.alertOpResult(env)
	case proto.TypeClockReply:
		s.clockReply(env)
	default:
		s.retained(env)
	}
}

// Uplink lease commands (WIRE §4.5).

func (s *Supervisor) uplinkOpen(env proto.Envelope) {
	var b proto.UplinkOpen
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	s.capMu.Lock()
	muted := s.muted
	s.capMu.Unlock()
	if muted {
		s.ack(env, proto.AckRejected, codeMuted)
		return
	}
	s.ackErr(env, s.up.Open(env, b), uplinkCode)
}

// uplinkRenew extends a lease. Converting a candidate lease into its turn's
// lease converts the provisional duck into the turn's input focus (§16.6).
func (s *Supervisor) uplinkRenew(env proto.Envelope) {
	var b proto.UplinkRenew
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	converted, err := s.up.Renew(env, b)
	if err != nil {
		s.ack(env, proto.AckRejected, uplinkCode(err))
		return
	}
	s.mu.Lock()
	cand, ok := s.candidates[b.LeaseID]
	if converted {
		delete(s.candidates, b.LeaseID)
	}
	s.mu.Unlock()
	switch {
	case !ok:
	case converted:
		s.fm.CandidateAccept(cand.id, b.Owner, env.Generation)
	default:
		s.fm.CandidateRenew(cand.id, ttl(b.TTLMs))
	}
	s.ack(env, proto.AckAccepted, "")
}

func (s *Supervisor) uplinkClose(env proto.Envelope) {
	var b proto.UplinkClose
	if err := json.Unmarshal(env.Body, &b); err != nil {
		s.ack(env, proto.AckRejected, codeInvalid)
		return
	}
	candidate, err := s.up.Close(env, b)
	if err != nil {
		s.ack(env, proto.AckRejected, uplinkCode(err))
		return
	}
	if candidate {
		s.releaseCandidateLease(b.LeaseID)
	}
	s.ack(env, proto.AckAccepted, "")
}

// retained handles the WIRE §4.8 messages: legacy bodies in the envelope.
func (s *Supervisor) retained(env proto.Envelope) {
	switch env.Type {
	case proto.TypeLEDs:
		var b struct {
			LEDs []led.Led `json:"leds"`
		}
		if s.decode(env, &b) {
			s.cfg.Physical.SetLEDs(b.LEDs)
		}
	case proto.TypeLEDAnim:
		var b struct {
			Anim server.AnimSpec `json:"anim"`
		}
		if s.decode(env, &b) {
			s.cfg.Physical.StartAnim(b.Anim)
		}
	case proto.TypeVolumeSet:
		var b volumeState
		if s.decode(env, &b) {
			s.cfg.Physical.SetVolume(b.Level)
		}
	case proto.TypeConfig:
		var m config.Message
		if !s.decode(env, &m) {
			return
		}
		v := s.cfg.DeviceConfig.Apply(m)
		if m.StartupVolume != nil {
			s.cfg.Physical.SeedVolume(v.StartupVolume)
		}
		s.fm.SetDuckDB(v.DuckDB)
		if s.cfg.Retained.ConfigApplied != nil {
			s.cfg.Retained.ConfigApplied(v)
		}
	case proto.TypeWifiChange:
		var b struct {
			SSID string `json:"ssid"`
			PSK  string `json:"psk"`
		}
		if s.decode(env, &b) && s.cfg.Retained.WifiChange != nil {
			s.cfg.Retained.WifiChange(b.SSID, b.PSK)
		}
	case proto.TypeWifiCommit:
		if s.cfg.Retained.WifiCommit != nil {
			s.cfg.Retained.WifiCommit()
		}
	case proto.TypeWifiScan:
		if s.cfg.Retained.WifiScan != nil {
			s.cfg.Retained.WifiScan()
		}
	}
}

func (s *Supervisor) decode(env proto.Envelope, v any) bool {
	if err := json.Unmarshal(env.Body, v); err != nil {
		log.Printf("[session] %s: %v", env.Type, err)
		return false
	}
	return true
}
