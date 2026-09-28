// Package supervisor is the single owner of device audio state (SPEC §3.1):
// capture epochs and rings, the final mixer and its reference tap, focus
// application and the provisional duck, the wake detector's callbacks,
// physical privacy and buttons, and the cached alert executor. Controller
// messages request named operations; they never drive hardware directly.
package supervisor

import (
	"context"
	"crypto/rand"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/audio/capture"
	"github.com/wilbowes/EchoMuse/internal/audio/cells"
	"github.com/wilbowes/EchoMuse/internal/audio/clockfit"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/audio/refdsp"
	"github.com/wilbowes/EchoMuse/internal/audio/ring"
	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/config"
	"github.com/wilbowes/EchoMuse/internal/focus"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/server"
	"github.com/wilbowes/EchoMuse/internal/uplink"
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	// tickInterval paces focus deadlines, burst-completion reports and
	// clock requests; focus leases are 3 s, so 50 ms is ample resolution.
	tickInterval = 50 * time.Millisecond
	// assetInterval paces background fetches of missing alert sounds. A
	// sound is never fetched at its deadline (§16.5).
	assetInterval = 2 * time.Second
	// candidateAckWindow is how long a candidate lease (and its provisional
	// duck) waits for the controller's command.ack (§4.4).
	candidateAckWindow = time.Second
)

// Uplink is the lease executor (internal/uplink).
type Uplink interface {
	Attach(client.AudioSink, uplink.SendFunc)
	Detach()
	SetEpochs(mic, reference uint64)
	Mute()
	OpenCandidate(leaseID string, supportStart uint64)
	AcceptCandidate(leaseID string, accepted bool)
	Open(proto.Envelope, proto.UplinkOpen) error
	Renew(proto.Envelope, proto.UplinkRenew) (converted bool, err error)
	Close(proto.Envelope, proto.UplinkClose) (candidateLease bool, err error)
	Notify()
	Run(context.Context)
}

// Session is the part of a client.Session the supervisor uses.
type Session interface {
	Send(typ string, generation uint32, body any) (messageID string, err error)
	Ack(messageID, status string, errCode *string) error
	Audio() client.AudioSink
	Assets() assets.Transport
}

// RetainedHooks carry retained non-audio commands (WIRE §4.8) to their
// owners. Each must return quickly.
type RetainedHooks struct {
	ConfigApplied func(config.Values)
	WifiChange    func(ssid, psk string)
	WifiCommit    func()
	WifiScan      func()
	// Ready runs after each session.ready, for connect-time retained
	// reports (stats, a pending wifi_result).
	Ready func()
}

// Config is the supervisor's fixed environment.
type Config struct {
	DeviceID        string
	FirmwareVersion string
	BootID          string
	IP              func() string
	// AmbientStatus is the retained als.Report() object; AmbientReadable
	// gates the ambient_light capability.
	AmbientStatus   func() json.RawMessage
	AmbientReadable func() bool

	Mic          pkgmic.Microphone
	Physical     *server.Server
	DeviceConfig *config.Device
	SpeechStore  *assets.Store // /data/local/share/echomuse/speech
	AlertStore   *assets.Store // <alerts root>/assets
	NowMonoNS    func() int64  // CLOCK_MONOTONIC ns; nil selects client.MonoNow
	Retained     RetainedHooks
}

// Deps are the replaceable collaborators Assemble wires.
type Deps struct {
	Sink      render.Sink
	LoadModel wakeword.LoadFunc
	Alerts    alerts.Config // Events is set to the supervisor
	// NewUplink builds the lease executor; nil selects uplink.New.
	NewUplink func(uplink.Rings, uplink.Clock, func() int64) Uplink
}

type candidateState struct{ id string }

type playbackState struct {
	class render.SourceClass
	gen   uint32
	owner string
}

// Supervisor implements client.Handler and alerts.EventSink.
//
// Lock order: capMu → announceMu → mu. Components are never called with mu
// held; announceMu serializes epoch announcements with session publication
// so every stream.open follows session.ready exactly once per session.
type Supervisor struct {
	cfg Config
	now func() int64

	mix  *render.Mixer
	fm   *focus.Manager
	det  *detector.Detector
	ex   *alerts.Executor
	up   Uplink
	act  *wakeword.Activator
	wg   sync.WaitGroup // session-scoped background work
	stop atomic.Bool

	micRing   *ring.Ring[int16]
	refRing   *ring.Ring[int16]
	cellRing  *ring.Ring[ema.Cell]
	masks     *cells.MaskHistory
	renderFit *clockfit.Fit

	// Capture pipeline, guarded by capMu.
	capMu    sync.Mutex
	timeline *capture.Timeline
	acc      *cells.Accumulator
	cellMeta ring.Meta
	muted    bool

	// Mixer-goroutine state (tap, anchor and epoch hooks).
	dec         refdsp.Decimator
	refBuf      [render.BlockFrames / refdsp.Factor]int16
	renderEpoch uint64
	completed   atomic.Uint64

	// Render-audio receive goroutine state.
	renderPCM [ema.MaxRenderFrames]int16

	// Epoch announcements, guarded by announceMu.
	announceMu sync.Mutex
	micEpoch   uint64
	micReason  string
	refEpoch   uint64
	refReason  string

	// Session state, guarded by mu.
	mu           sync.Mutex
	session      Session
	sessCtx      context.Context
	sessCancel   context.CancelFunc
	candidates   map[string]candidateState // candidate lease id → candidate
	candidateAck map[string]string         // wake.candidate message id → lease id
	playbacks    map[string]playbackState
	dialogOwner  string              // owner of the newest dialog_output focus lease
	leases       map[string]struct{} // live focus leases
	alertState   alerts.AlertState
	alertFocusID string
	unfetchable  map[string]bool // alert sounds the controller lacks, this session
	soundWant    map[string]bool // alert sounds no armed alarm needs: previews, prefetched timer sounds
	pressEpoch   proto.NullU64
	pressSample  proto.NullU64

	lastMask        atomic.Uint32 // final-mix mask of the latest tap
	alertForeground atomic.Bool
	fetching        atomic.Bool
	physicalSeq     atomic.Uint64
	burstSeq        uint64 // tick goroutine
}

// Assemble builds the runtime: alert executor, mixer, focus manager,
// detector, activator and uplink, all calling back into one supervisor.
func Assemble(cfg Config, d Deps) (*Supervisor, error) {
	if cfg.DeviceID == "" || cfg.Mic == nil || cfg.Physical == nil || cfg.DeviceConfig == nil ||
		cfg.SpeechStore == nil || cfg.AlertStore == nil || d.Sink == nil || d.LoadModel == nil {
		return nil, errors.New("supervisor: incomplete configuration")
	}
	if cfg.NowMonoNS == nil {
		cfg.NowMonoNS = client.MonoNow
	}
	if cfg.IP == nil {
		cfg.IP = func() string { return "" }
	}
	if cfg.AmbientStatus == nil {
		cfg.AmbientStatus = func() json.RawMessage { return json.RawMessage("null") }
	}
	if cfg.AmbientReadable == nil {
		cfg.AmbientReadable = func() bool { return false }
	}
	s := &Supervisor{
		cfg: cfg, now: cfg.NowMonoNS,
		micRing: ring.NewMic(), refRing: ring.NewReference(), cellRing: ring.NewCells(),
		timeline:   capture.NewTimeline(),
		renderFit:  clockfit.New(render.SampleRate, int64(render.SinkFrames)*int64(time.Second)/render.SampleRate),
		candidates: map[string]candidateState{}, candidateAck: map[string]string{},
		playbacks: map[string]playbackState{}, leases: map[string]struct{}{},
		unfetchable: map[string]bool{}, soundWant: map[string]bool{},
		muted: cfg.Physical.IsMuted(),
	}
	s.masks = cells.NewMaskHistory(s.timeline.Fit(), s.renderFit)
	s.acc = cells.NewAccumulator(s.masks, s.appendCells)

	ac := d.Alerts
	ac.Events = s
	ex, err := alerts.NewExecutor(ac)
	if err != nil {
		return nil, err
	}
	s.ex = ex
	s.alertState = ex.State()

	mix, err := render.NewMixer(render.Config{
		Sink: d.Sink, Alert: ex, Now: cfg.NowMonoNS,
		Hooks: render.Hooks{Progress: s.onProgress, Finished: s.onFinished, Tap: s.onTap, Anchor: s.onAnchor, Epoch: s.onRenderEpoch},
	})
	if err != nil {
		_ = ex.Close()
		return nil, err
	}
	s.mix = mix
	s.fm = focus.New(focus.Config{
		DuckDB: cfg.DeviceConfig.Get().DuckDB,
		Callbacks: focus.Callbacks{
			Apply:          s.applyFocus,
			CancelPlayback: func(id string, gen uint32, r render.Ramp) { s.mix.Cancel(id, gen, r) },
			LeaseExpired:   s.focusLeaseExpired,
		},
	})
	det, err := detector.New(detector.Callbacks{
		Profile: s.profile, ProducingSound: s.producingSound,
		OnCandidate: s.onCandidate, OnCandidateEnd: s.onCandidateEnd, OnStats: s.onStats,
		NowMonoNS: cfg.NowMonoNS,
	})
	if err != nil {
		_ = ex.Close()
		return nil, err
	}
	s.det = det
	s.act = wakeword.NewActivator(det, cfg.SpeechStore, d.LoadModel)
	rings := uplink.Rings{Mic: s.micRing, Ref: s.refRing, Cells: s.cellRing}
	if d.NewUplink != nil {
		s.up = d.NewUplink(rings, s, cfg.NowMonoNS)
	} else {
		s.up = uplink.New(rings, s, cfg.NowMonoNS)
	}
	s.up.SetEpochs(0, 0)
	if s.muted {
		det.SetMuted(true)
	}
	cfg.Physical.SetMuteChangeCallback(s.setPrivacy)
	cfg.Physical.SetVolumeChangeCallback(func(level int) {
		s.logSend(proto.TypeVolumeState, 0, volumeState{Level: level})
	})
	return s, nil
}

// Run owns the mixer, capture, uplink, alert and deadline goroutines until
// ctx ends or capture/render fails. Shutdown closes the alert executor,
// which releases the kernel wakelock (§16.5).
func (s *Supervisor) Run(ctx context.Context) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	go s.up.Run(ctx)
	go s.ex.Run(ctx)
	go s.tickLoop(ctx)
	mixErr := make(chan error, 1)
	capErr := make(chan error, 1)
	go func() { mixErr <- s.mix.Run() }()
	go func() { capErr <- s.captureLoop(ctx) }()

	var err error
	select {
	case <-ctx.Done():
	case err = <-mixErr:
	case err = <-capErr:
	}
	cancel()
	s.stop.Store(true)
	s.endSession()
	s.cfg.Mic.Close()
	_ = s.mix.Close()
	s.wg.Wait()
	s.det.Close()
	if cerr := s.ex.Close(); err == nil {
		err = cerr
	}
	return err
}

func (s *Supervisor) captureLoop(ctx context.Context) error {
	for ctx.Err() == nil {
		b, err := s.cfg.Mic.Read()
		if err != nil {
			if ctx.Err() != nil || s.stop.Load() {
				return nil
			}
			return fmt.Errorf("supervisor: capture: %w", err)
		}
		s.captureBlock(b)
	}
	return nil
}

func (s *Supervisor) tickLoop(ctx context.Context) {
	tick := time.NewTicker(tickInterval)
	defer tick.Stop()
	fetch := time.NewTicker(assetInterval)
	defer fetch.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
			s.tick()
		case <-fetch.C:
			s.fetchAlertAssets()
		}
	}
}

func (s *Supervisor) tick() {
	s.fm.Tick()
	if id, seq, ok := s.ex.BurstCompletedSince(s.burstSeq); ok {
		s.burstSeq = seq
		s.fm.AlertBurstCompleted(id)
	}
	if s.connected() {
		if req, ok := s.ex.ClockRequestIfDue(); ok {
			s.logSend(proto.TypeClockRequest, 0, req)
		}
	}
}

// CaptureToReference implements uplink.Clock: capture sample → monotonic
// time → render sample → reference sample (render/3), through the latest
// clock fits of the current epochs.
func (s *Supervisor) CaptureToReference(sample uint64) (uint64, bool) {
	ns, ok := s.timeline.Fit().SampleToNs(sample)
	if !ok {
		return 0, false
	}
	r, ok := s.renderFit.NsToSample(ns)
	if !ok {
		return 0, false
	}
	if r < 0 {
		return 0, true
	}
	return uint64(r) / refdsp.Factor, true
}

// SendRetained sends a retained device message (WIRE §4.8) on the current
// session; it is dropped while disconnected.
func (s *Supervisor) SendRetained(typ string, body any) { s.logSend(typ, 0, body) }

// Connected reports whether a session is established.
func (s *Supervisor) Connected() bool { return s.connected() }

func (s *Supervisor) connected() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.session != nil
}

func (s *Supervisor) send(typ string, gen uint32, body any) (string, error) {
	s.mu.Lock()
	sess := s.session
	s.mu.Unlock()
	if sess == nil {
		return "", client.ErrClosed
	}
	return sess.Send(typ, gen, body)
}

func (s *Supervisor) logSend(typ string, gen uint32, body any) {
	if _, err := s.send(typ, gen, body); err != nil && !errors.Is(err, client.ErrClosed) {
		log.Printf("[supervisor] send %s: %v", typ, err)
	}
}

// sendUplink is the uplink executor's send: an ended candidate lease also
// ends its provisional duck (§16.6).
func (s *Supervisor) sendUplink(typ string, gen uint32, body any) (string, error) {
	if ended, ok := body.(proto.UplinkEnded); ok && typ == proto.TypeUplinkEnded {
		s.releaseCandidateLease(ended.LeaseID)
	}
	return s.send(typ, gen, body)
}

func (s *Supervisor) ack(env proto.Envelope, status, code string) {
	s.mu.Lock()
	sess := s.session
	s.mu.Unlock()
	if sess == nil {
		return
	}
	var c *string
	if code != "" {
		c = &code
	}
	if err := sess.Ack(env.MessageID, status, c); err != nil && !errors.Is(err, client.ErrClosed) {
		log.Printf("[supervisor] ack %s: %v", env.Type, err)
	}
}

// ackErr acks accepted, or rejected with code(err).
func (s *Supervisor) ackErr(env proto.Envelope, err error, code func(error) string) {
	if err != nil {
		s.ack(env, proto.AckRejected, code(err))
		return
	}
	s.ack(env, proto.AckAccepted, "")
}

type volumeState struct {
	Level int `json:"level"`
}

// newEpoch draws a random nonzero stream epoch (§16.1).
func newEpoch() uint64 {
	var b [8]byte
	for {
		if _, err := rand.Read(b[:]); err != nil {
			panic("supervisor: crypto/rand: " + err.Error())
		}
		if v := binary.LittleEndian.Uint64(b[:]); v != 0 {
			return v
		}
	}
}

func ttl(ms int64) time.Duration { return time.Duration(ms) * time.Millisecond }
