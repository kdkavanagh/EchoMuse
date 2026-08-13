package slspeaker

import (
	"errors"
	"sync"
	"sync/atomic"
	"time"

	pkgspeaker "github.com/wilbowes/EchoMuse/pkg/speaker"
)

// errSpeakerDead is returned when the pump loop has exited, so a caller
// blocked on a full buffer unblocks with an error instead of hanging forever.
var errSpeakerDead = errors.New("slspeaker: pump loop has died")

// StreamStats is a type alias for pkg/speaker.StreamStats, the canonical
// definition — see that comment. Aliasing rather than redefining means this
// backend's stats and the tinyalsa backend's are one identical Go type, which
// is what lets cmd/server.go hold either behind pkg/speaker.FullSpeaker.
type StreamStats = pkgspeaker.StreamStats

// audioStream is one buffered playback stream (voice/TTS or music), the same
// state machine as speaker.audioStream — see that file for the full
// reasoning behind the prime gate, the discard-until-EOS contract and the
// minDepth sampling rule. It is duplicated here rather than imported because
// the two backends live in different packages (see the architecture note in
// docs/native-afe-migration.md); nothing about the logic itself differs.
//
// Untagged on purpose — this is the buffering state machine, worth testing on
// the host independent of OpenSL ES.
type audioStream struct {
	ch     chan []byte
	deadCh <-chan struct{} // closed when the pump loop exits

	eosPending atomic.Bool

	mu         sync.Mutex
	active     bool
	discarding bool

	recvFirstNs  atomic.Int64
	recvLastNs   atomic.Int64
	recvMaxGapNs atomic.Int64
	recvBytes    atomic.Uint64

	playing     bool
	periods     uint64
	underruns   uint64
	minDepth    int
	firstPumpNs int64
}

func newAudioStream(depth int, deadCh <-chan struct{}) *audioStream {
	return &audioStream{
		ch:       make(chan []byte, depth),
		deadCh:   deadCh,
		minDepth: -1,
	}
}

// pump queues one period, or reports it was swallowed by a flush. Blocks
// until the pump loop has consumed a slot, or returns an error if that loop
// has died.
func (s *audioStream) pump(period []byte, wireBytes int) (bool, error) {
	s.mu.Lock()
	if s.discarding {
		s.mu.Unlock()
		return false, nil
	}
	newStream := !s.active
	s.active = true
	s.mu.Unlock()

	now := time.Now().UnixNano()
	if newStream {
		s.recvFirstNs.Store(now)
		s.recvMaxGapNs.Store(0)
		s.recvBytes.Store(0)
	} else if last := s.recvLastNs.Load(); last > 0 {
		if gap := now - last; gap > s.recvMaxGapNs.Load() {
			s.recvMaxGapNs.Store(gap)
		}
	}
	s.recvLastNs.Store(now)
	s.recvBytes.Add(uint64(wireBytes))

	select {
	case s.ch <- period:
		return true, nil
	case <-s.deadCh:
		return false, errSpeakerDead
	}
}

// endStream marks the EOS — see speaker.audioStream.endStream for why a
// discarding stream must NOT re-arm eosPending here.
func (s *audioStream) endStream() {
	s.mu.Lock()
	s.active = false
	wasDiscarding := s.discarding
	s.discarding = false
	s.mu.Unlock()
	if wasDiscarding {
		return
	}
	s.eosPending.Store(true)
}

// flush drops everything queued and, if a stream is mid-flight, arms discard
// so the remainder still in flight from the controller is swallowed rather
// than played — see speaker.audioStream.flush.
func (s *audioStream) flush() {
	s.mu.Lock()
	if s.active {
		s.discarding = true
	}
	s.mu.Unlock()
	s.eosPending.Store(true)
	for {
		select {
		case <-s.ch:
		default:
			return
		}
	}
}

func (s *audioStream) isActive() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.active
}

// ready reports whether the pump loop should take a period this round — the
// prime gate. See speaker.audioStream.ready for the full rationale (protects
// the opening seconds of playback against link stalls).
func (s *audioStream) ready(prime int) bool {
	n := len(s.ch)
	if n == 0 {
		return false
	}
	if s.playing {
		return true
	}
	return n >= prime || s.eosPending.Load()
}

// take removes one period, updating consumption-side accounting. Only called
// when ready() said so, and only from the pump goroutine.
func (s *audioStream) take() []byte {
	select {
	case period := <-s.ch:
		s.playing = true
		s.periods++
		if !s.eosPending.Load() {
			if d := len(s.ch); s.minDepth < 0 || d < s.minDepth {
				s.minDepth = d
			}
		}
		if s.firstPumpNs == 0 {
			s.firstPumpNs = time.Now().UnixNano()
		}
		return period
	default:
		return nil
	}
}

// drained is called when the stream was playing and had nothing to give this
// round. Returns the stats to report at a natural EOS/flush, or nil for an
// underrun (mid-stream drain — the audible stutter on a weak link).
func (s *audioStream) drained() *StreamStats {
	s.playing = false
	if !s.eosPending.Swap(false) {
		s.underruns++
		return nil
	}
	st := &StreamStats{
		Periods:   s.periods,
		Underruns: s.underruns,
		MinDepth:  s.minDepth,
		MaxGapMs:  s.recvMaxGapNs.Load() / 1e6,
		BytesRecv: s.recvBytes.Load(),
	}
	firstRecv, lastRecv := s.recvFirstNs.Load(), s.recvLastNs.Load()
	if firstRecv > 0 && lastRecv > firstRecv {
		st.RecvSpanMs = (lastRecv - firstRecv) / 1e6
	}
	if firstRecv > 0 && s.firstPumpNs > firstRecv {
		st.PrimeWaitMs = (s.firstPumpNs - firstRecv) / 1e6
	}
	s.periods, s.underruns = 0, 0
	s.minDepth, s.firstPumpNs = -1, 0
	return st
}
