package alerts

import (
	"errors"
	"time"

	"golang.org/x/sys/unix"
)

// SPEC §16.5 and WIRE clock.request: trust UTC after two replies with round
// trip <=500 ms whose offset estimates agree within 1 s; request every 1 s
// until trusted, then every 10 min.
const (
	maxClockRTTNs      = int64(500 * time.Millisecond)
	clockAgreementMs   = int64(1000)
	clockRetryNs       = int64(time.Second)
	clockRefreshNs     = int64(10 * time.Minute)
	maxOutstandingPing = 4
)

// MonoClock supplies CLOCK_MONOTONIC nanoseconds.
type MonoClock interface {
	MonoNowNs() int64
}

// SystemMonoClock reads CLOCK_MONOTONIC.
type SystemMonoClock struct{}

// MonoNowNs implements MonoClock.
func (SystemMonoClock) MonoNowNs() int64 {
	var ts unix.Timespec
	if err := unix.ClockGettime(unix.CLOCK_MONOTONIC, &ts); err != nil {
		panic("alerts: CLOCK_MONOTONIC unavailable: " + err.Error())
	}
	return ts.Nano()
}

var errUnknownNonce = errors.New("alerts: clock.reply for an unknown nonce")

type pendingPing struct {
	nonce  string
	sentNs int64
}

// clockEstimator maps the monotonic clock to UTC from authenticated
// controller clock.reply messages. Not safe for concurrent use; the Executor
// serializes access.
type clockEstimator struct {
	bootID      string
	mono        MonoClock
	trusted     bool
	offsetMs    int64  // utc_ms = mono_ns/1e6 + offsetMs
	candidate   *int64 // first agreeing sample of the current pair
	outstanding []pendingPing
	lastSentNs  int64
	sentAny     bool
}

// newClockEstimator restores a saved anchor only when it belongs to this boot:
// monotonic deadlines are never reused across boots.
func newClockEstimator(bootID string, mono MonoClock, saved *clockAnchor) *clockEstimator {
	c := &clockEstimator{bootID: bootID, mono: mono}
	if saved != nil && saved.BootID == bootID {
		c.trusted, c.offsetMs = true, saved.OffsetMs
	}
	return c
}

// requestIfDue returns a nonce to send in clock.request when one is due.
func (c *clockEstimator) requestIfDue() (string, bool) {
	now := c.mono.MonoNowNs()
	period := clockRetryNs
	if c.trusted {
		period = clockRefreshNs
	}
	if c.sentAny && now-c.lastSentNs < period {
		return "", false
	}
	nonce := NewUUID4().String()
	if len(c.outstanding) == maxOutstandingPing {
		c.outstanding = append(c.outstanding[:0], c.outstanding[1:]...)
	}
	c.outstanding = append(c.outstanding, pendingPing{nonce, now})
	c.lastSentNs, c.sentAny = now, true
	return nonce, true
}

// reply consumes a clock.reply; utcMs is the controller's UTC when it replied.
// It reports whether the trusted offset changed.
func (c *clockEstimator) reply(nonce string, utcMs int64) (bool, error) {
	idx := -1
	for i, p := range c.outstanding {
		if p.nonce == nonce {
			idx = i
			break
		}
	}
	if idx < 0 {
		return false, errUnknownNonce
	}
	sent := c.outstanding[idx].sentNs
	c.outstanding = append(c.outstanding[:idx], c.outstanding[idx+1:]...)
	rtt := c.mono.MonoNowNs() - sent
	if rtt < 0 || rtt > maxClockRTTNs {
		return false, nil
	}
	// The reply was stamped at an unknown point of the round trip; the
	// midpoint bounds the error by rtt/2.
	offset := utcMs - (sent+rtt/2)/int64(time.Millisecond)
	if c.candidate == nil || abs64(offset-*c.candidate) > clockAgreementMs {
		c.candidate = &offset
		return false, nil
	}
	accepted := (*c.candidate + offset) / 2
	c.candidate = nil
	changed := !c.trusted || c.offsetMs != accepted
	c.trusted, c.offsetMs = true, accepted
	return changed, nil
}

// sessionLost forgets unanswered requests and any half-formed pair.
func (c *clockEstimator) sessionLost() {
	c.outstanding = c.outstanding[:0]
	c.candidate = nil
	c.sentAny = false
}

func (c *clockEstimator) utcNowMs() (int64, bool) {
	if !c.trusted {
		return 0, false
	}
	return c.mono.MonoNowNs()/int64(time.Millisecond) + c.offsetMs, true
}

// monoAtUTC converts a UTC instant to this boot's monotonic clock.
func (c *clockEstimator) monoAtUTC(utcMs int64) (int64, bool) {
	if !c.trusted {
		return 0, false
	}
	return (utcMs - c.offsetMs) * int64(time.Millisecond), true
}

func (c *clockEstimator) anchor() *clockAnchor {
	if !c.trusted {
		return nil
	}
	return &clockAnchor{BootID: c.bootID, OffsetMs: c.offsetMs}
}

func abs64(v int64) int64 {
	if v < 0 {
		return -v
	}
	return v
}
