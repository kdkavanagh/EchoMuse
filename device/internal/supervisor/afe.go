package supervisor

import (
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/wilbowes/EchoMuse/internal/audio/afe"
	"github.com/wilbowes/EchoMuse/internal/audio/ema"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/wakeword/detector"
)

// afe_metadata_v1 (WIRE §4.1, §3 kind 5). A hello announces the capability
// only when the capture backend decodes native AFE metadata and a frame
// validated recently. Right after start no period has been decoded yet, so
// a hello waits briefly for the first one rather than racing it: the
// capability is fixed for the session it opens.
const (
	afeValidFor  = 2 * time.Second
	afeHelloWait = time.Second
)

// afeWindow accumulates one wake.stats window. Health counts every decoded
// period; the field extents, which only the device log reports, cover
// unmuted periods only.
type afeWindow struct {
	periods, frames, invalid, syncs, gaps, lost uint64

	playback, erleFrames, dtdFrames, vadFrames uint64
	erleMax, dtdMax, vadMax, volume            uint8
	rmsLo, rmsHi                               uint8 // raw: dB = raw − 256; 0 none
}

type afeState struct {
	mu      sync.Mutex
	win     afeWindow
	validNs int64         // when a period last held a valid frame; 0 never
	started bool          // first is closed
	first   chan struct{} // closed at the first decoded period

	hello atomic.Bool // the latest hello announced afe_metadata_v1
	on    atomic.Bool // this session opted in (session.ready afe_metadata)
}

// accountAFE adds one decoded period to the window. capMu held; it does not
// allocate.
func (s *Supervisor) accountAFE(p *afe.Period) {
	a := &s.afe
	r := &p.Record
	a.mu.Lock()
	defer a.mu.Unlock()
	w := &a.win
	w.periods++
	w.frames += uint64(r.Frames)
	w.invalid += uint64(p.Invalid)
	w.syncs += uint64(p.Syncs)
	w.gaps += uint64(p.Gaps)
	w.lost += uint64(p.Lost)
	if r.Frames > 0 {
		a.validNs = s.now()
		if !s.muted {
			w.playback += uint64(r.Playback)
			w.erleFrames += uint64(r.ERLEFrames)
			w.dtdFrames += uint64(r.DTDFrames)
			w.vadFrames += uint64(r.VADFrames)
			w.erleMax = max(w.erleMax, r.ERLEMax)
			w.dtdMax = max(w.dtdMax, r.DTDMax)
			w.vadMax = max(w.vadMax, r.VADMax)
			w.volume = r.Volume
			if r.RMSMax > 0 {
				if w.rmsLo == 0 || r.RMSMean < w.rmsLo {
					w.rmsLo = r.RMSMean
				}
				w.rmsHi = max(w.rmsHi, r.RMSMax)
			}
		}
	}
	if !a.started {
		a.started = true
		close(a.first)
	}
}

// afeCapable decides the hello's afe_metadata_v1.
func (s *Supervisor) afeCapable() bool {
	if !s.cfg.AFEMetadata {
		return false
	}
	t := time.NewTimer(afeHelloWait)
	select {
	case <-s.afe.first:
	case <-t.C:
	}
	t.Stop()
	s.afe.mu.Lock()
	valid := s.afe.validNs
	s.afe.mu.Unlock()
	return valid != 0 && s.now()-valid <= int64(afeValidFor)
}

// wakeStats is the wake.stats body: the detector's fields, plus the AFE
// decoder's health in afe_metadata_v1 sessions, marshalled flat.
type wakeStats struct {
	detector.Stats
	AFE *proto.AFEStats `json:"afe,omitempty"`
}

// afeStats returns the window's health counts and, at the end of a stats
// window, logs the window and starts the next one. Without an AFE backend
// it returns nil.
func (s *Supervisor) afeStats(windowEnd bool, windowMs int64) *proto.AFEStats {
	if !s.cfg.AFEMetadata {
		return nil
	}
	a := &s.afe
	a.mu.Lock()
	w := a.win
	if windowEnd {
		a.win = afeWindow{}
	}
	a.mu.Unlock()
	if windowEnd {
		log.Printf("[afe] %ds: %d/%d frames valid, %d invalid, %d syncs, %d gaps, %d lost; "+
			"playback %d, erle max %d (%d frames), dtd max %d/31 (%d), vad max %d/4 (%d), rms %d..%d dB, volume %d",
			windowMs/1000, w.frames, ema.AFEMaxFrames*w.periods, w.invalid, w.syncs, w.gaps, w.lost,
			w.playback, w.erleMax, w.erleFrames, w.dtdMax, w.dtdFrames, w.vadMax, w.vadFrames,
			db(w.rmsLo), db(w.rmsHi), w.volume)
	}
	return &proto.AFEStats{Periods: w.periods, Frames: w.frames, Invalid: w.invalid,
		Syncs: w.syncs, Gaps: w.gaps, LostFrames: w.lost}
}

// db converts an AFE RMS byte to dB; 0 (none) stays 0.
func db(raw uint8) int {
	if raw == 0 {
		return 0
	}
	return int(raw) - 256
}
