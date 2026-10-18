package companion

import (
	"context"
	"sync"
	"time"
)

const (
	// linkDownAfter is how long the app or plugin may stay unlinked, after
	// its link dropped or after the last attempt, before its installer runs
	// again (plan decisions 5, §11.2 decision 2).
	linkDownAfter = 30 * time.Second
	// retryMin separates any two attempts; each failure in a row doubles it
	// up to retryMax, so a broken pm or Kiosk Satellite is not hammered
	// every 30 s.
	retryMin = 10 * time.Second
	retryMax = 5 * time.Minute
)

// action is what one installer attempt does; a larger one includes the
// smaller ones' work.
type action uint8

const (
	actNone    action = iota
	actGrant          // re-apply the overlay appop (the app reported it denied)
	actEnsure         // install when the version or hash differs, and so on
	actInstall        // reinstall unconditionally, grant, start (link version mismatch)
)

func (a action) String() string {
	switch a {
	case actGrant:
		return "grant"
	case actEnsure:
		return "ensure"
	case actInstall:
		return "reinstall"
	default:
		return "none"
	}
}

// pacer schedules one installer's attempts (plan decision 5): the first at
// once, then whenever its connection has been unlinked for linkDownAfter
// and on request, never closer than gap() after the last attempt. Attempts
// run one at a time on run's goroutine.
type pacer struct {
	now  func() time.Time
	wake chan struct{}

	mu     sync.Mutex
	want   action    // requested, not yet run
	linked bool      // the connection is linked past hello
	downAt time.Time // when it was last seen unlinked (start, link drop)
	last   time.Time // the last attempt; zero before the first
	fails  int       // attempts failed in a row
}

// init makes the first attempt due at once.
func (p *pacer) init(now func() time.Time) {
	if now == nil {
		now = time.Now
	}
	p.now, p.wake, p.want, p.downAt = now, make(chan struct{}, 1), actEnsure, now()
}

// run calls attempt for each due action until ctx ends.
func (p *pacer) run(ctx context.Context, attempt func(context.Context, action)) {
	for {
		a, wait := p.next(p.now())
		if a != actNone {
			attempt(ctx, a)
			continue
		}
		var timer *time.Timer
		var fire <-chan time.Time
		if wait > 0 {
			timer = time.NewTimer(wait)
			fire = timer.C
		}
		select {
		case <-ctx.Done():
		case <-p.wake:
		case <-fire:
		}
		if timer != nil {
			timer.Stop()
		}
		if ctx.Err() != nil {
			return
		}
	}
}

// SetLinked reports the connection linked past hello, or its link
// dropping.
func (p *pacer) SetLinked(linked bool) {
	p.mu.Lock()
	p.linked = linked
	if !linked {
		p.downAt = p.now()
	}
	p.mu.Unlock()
	p.notify()
}

func (p *pacer) request(a action) {
	p.mu.Lock()
	p.want = max(p.want, a)
	p.mu.Unlock()
	p.notify()
}

func (p *pacer) notify() {
	select {
	case p.wake <- struct{}{}:
	default:
	}
}

// record counts an attempt's outcome toward the gap.
func (p *pacer) record(failed bool) {
	p.mu.Lock()
	if failed {
		p.fails++
	} else {
		p.fails = 0
	}
	p.mu.Unlock()
}

// next is the attempt due at now, or actNone and how long until one may be
// (0: none until a request).
func (p *pacer) next(now time.Time) (action, time.Duration) {
	p.mu.Lock()
	defer p.mu.Unlock()
	a, due := p.want, time.Time{}
	if !p.linked {
		recheck := p.downAt
		if p.last.After(recheck) {
			recheck = p.last
		}
		recheck = recheck.Add(linkDownAfter)
		if !now.Before(recheck) {
			a = max(a, actEnsure)
		} else {
			due = recheck
		}
	}
	if a == actNone {
		if due.IsZero() {
			return actNone, 0
		}
		return actNone, due.Sub(now)
	}
	if !p.last.IsZero() {
		if paced := p.last.Add(p.gap()); now.Before(paced) {
			return actNone, paced.Sub(now)
		}
	}
	p.want, p.last = actNone, now
	return a, 0
}

// gap is the pause after the last attempt: retryMin, doubled per failure
// in a row, at most retryMax.
func (p *pacer) gap() time.Duration {
	return min(retryMin<<min(p.fails, 8), retryMax)
}
