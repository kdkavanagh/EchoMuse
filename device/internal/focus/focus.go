// Package focus owns the device's speaker focus (SPEC §6.2, §16.2, §16.6):
// dialog leases, the provisional duck, and whether the active alert
// occurrence is foreground. It is pure logic with an injected clock; its
// decisions reach the mixer and alert executor through callbacks.
package focus

import (
	"errors"
	"sync"
	"time"

	"github.com/wilbowes/EchoMuse/internal/render"
)

const (
	DefaultTTL    = 3 * time.Second // dialog and candidate lease TTL (§4.4, §7)
	DefaultDuckDB = -18.0           // duckDb default (§6.2)

	// AlertYield bounds how long dialog output delays a newly due alert (§6.2).
	AlertYield = 2 * time.Second
	// AlertBackgroundCap is the occurrence-level background budget (§6.2).
	AlertBackgroundCap = 15 * time.Second

	ProvisionalWindow    = 5 * time.Second // §16.6
	ProvisionalPerWindow = 2               // §16.6
)

// Kind is a focus.acquire class.
type Kind string

const (
	DialogInput  Kind = "dialog_input"
	DialogOutput Kind = "dialog_output"
)

var (
	ErrKind    = errors.New("focus: invalid focus class")
	ErrStale   = errors.New("focus: stale generation")
	ErrUnknown = errors.New("focus: unknown lease")
)

// Lease is a live dialog focus lease.
type Lease struct {
	ID         string
	Owner      string
	Generation uint32
	Kind       Kind
	Expires    time.Time
}

// Output is the current focus decision. Mix goes to render.Mixer.SetPolicy,
// AlertBackground to the alert executor.
type Output struct {
	Mix             render.Policy
	AlertBackground bool
}

// Callbacks run outside the Manager lock, in order, and may call back in.
type Callbacks struct {
	Apply          func(Output)
	CancelPlayback func(playbackID string, generation uint32, ramp render.Ramp)
	LeaseExpired   func(Lease)
	// AlertPreempted reports that the occurrence's background budget ran
	// out: the alert takes the foreground and the uncommitted utterance ends
	// as alert_preempted.
	AlertPreempted func(occurrenceID string)
}

// Config wires a Manager. DuckDB is the configured duckDb.
type Config struct {
	Now       func() time.Time
	DuckDB    float64
	Callbacks Callbacks
}

type lease struct {
	Lease
	duckDB float64
	// converted marks an accepted candidate's provisional duck held as turn
	// focus until the turn's own lease supersedes it (§16.6).
	converted bool
}

type provisional struct {
	duckDB  float64
	expires time.Time
}

// Manager is safe for concurrent use.
type Manager struct {
	mu     sync.Mutex
	now    func() time.Time
	cb     Callbacks
	duckDB float64

	leases    map[string]*lease
	prov      map[string]*provisional
	duckTimes [ProvisionalPerWindow]time.Time

	alertID     string
	alertBG     bool
	bgSince     time.Time
	budget      time.Duration // background time not yet reset by a completed burst
	budgetSpent bool
	yieldAt     time.Time // due-during-dialog deadline; zero when none
	yielded     bool      // dialog output yielded; output alone no longer backgrounds

	dialogID      string
	dialogOwner   string
	dialogGen     uint32
	dialogPlaying bool

	ramp    render.Ramp
	out     Output
	pending []func()
}

func New(cfg Config) *Manager {
	now := cfg.Now
	if now == nil {
		now = time.Now
	}
	return &Manager{
		now:    now,
		cb:     cfg.Callbacks,
		duckDB: cfg.DuckDB,
		leases: make(map[string]*lease),
		prov:   make(map[string]*provisional),
	}
}

// update runs fn under the lock, then applies expiries and deadlines,
// recomputes the policy and runs the queued callbacks.
func (m *Manager) update(fn func(now time.Time) error) error {
	m.mu.Lock()
	now := m.now()
	m.expireLocked(now)
	err := fn(now)
	m.deadlinesLocked(now)
	m.recomputeLocked(now)
	acts := m.pending
	m.pending = nil
	m.mu.Unlock()
	for _, a := range acts {
		a()
	}
	return err
}

// SetDuckDB sets the depth used by subsequent leases and provisional ducks.
func (m *Manager) SetDuckDB(db float64) {
	m.mu.Lock()
	m.duckDB = db
	m.mu.Unlock()
}

// Output returns the current decision.
func (m *Manager) Output() Output {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.out
}

// Acquire creates or refreshes a dialog lease (focus.acquire).
func (m *Manager) Acquire(id, owner string, generation uint32, kind Kind, ttl time.Duration) error {
	if kind != DialogInput && kind != DialogOutput {
		return ErrKind
	}
	return m.update(func(now time.Time) error {
		if l, ok := m.leases[id]; ok && generation < l.Generation {
			return ErrStale
		}
		for k, l := range m.leases {
			if l.converted && l.Owner == owner {
				delete(m.leases, k)
			}
		}
		if kind == DialogInput {
			m.yielded = false
			m.cancelDialogLocked()
		}
		m.leases[id] = &lease{
			Lease:  Lease{ID: id, Owner: owner, Generation: generation, Kind: kind, Expires: now.Add(ttlOr(ttl))},
			duckDB: m.duckDB,
		}
		return nil
	})
}

// Renew extends a lease (focus.renew); generation may only grow.
func (m *Manager) Renew(id string, generation uint32, ttl time.Duration) error {
	return m.update(func(now time.Time) error {
		l, ok := m.leases[id]
		if !ok {
			return ErrUnknown
		}
		if generation < l.Generation {
			return ErrStale
		}
		l.Generation = generation
		l.Expires = now.Add(ttlOr(ttl))
		return nil
	})
}

// Release ends a lease (focus.release). Releasing an unknown lease is a no-op.
func (m *Manager) Release(id string, generation uint32) error {
	return m.update(func(time.Time) error {
		l, ok := m.leases[id]
		if !ok {
			return nil
		}
		if generation < l.Generation {
			return ErrStale
		}
		delete(m.leases, id)
		return nil
	})
}

// EndSession ends every lease and provisional duck (session loss, §16.1),
// cancelling the dialog output they owned.
func (m *Manager) EndSession() {
	m.update(func(time.Time) error {
		clear(m.leases)
		clear(m.prov)
		m.cancelDialogLocked()
		return nil
	})
}

// CandidateOpen applies the provisional duck for a wake candidate (§16.6).
// It reports whether a duck was applied: only when the device was producing
// sound, once per candidate and at most ProvisionalPerWindow per window.
func (m *Manager) CandidateOpen(candidateID string, producingSound bool, ttl time.Duration) bool {
	applied := false
	m.update(func(now time.Time) error {
		if !producingSound {
			return nil
		}
		if _, ok := m.prov[candidateID]; ok {
			applied = true
			return nil
		}
		recent := 0
		for _, t := range m.duckTimes {
			if !t.IsZero() && now.Sub(t) < ProvisionalWindow {
				recent++
			}
		}
		if recent >= ProvisionalPerWindow {
			return nil
		}
		oldest := 0
		for i, t := range m.duckTimes {
			if t.Before(m.duckTimes[oldest]) {
				oldest = i
			}
		}
		m.duckTimes[oldest] = now
		m.prov[candidateID] = &provisional{duckDB: m.duckDB, expires: now.Add(ttlOr(ttl))}
		m.yielded = false
		applied = true
		return nil
	})
	return applied
}

// CandidateRenew extends a provisional duck with its candidate lease.
func (m *Manager) CandidateRenew(candidateID string, ttl time.Duration) {
	m.update(func(now time.Time) error {
		if p, ok := m.prov[candidateID]; ok {
			p.expires = now.Add(ttlOr(ttl))
		}
		return nil
	})
}

// CandidateRelease restores the previous policy (uplink.close rejected or
// arbitration_lost, or the candidate lease ended).
func (m *Manager) CandidateRelease(candidateID string) {
	m.update(func(time.Time) error {
		delete(m.prov, candidateID)
		return nil
	})
}

// CandidateAccept converts the provisional duck into the turn's input focus
// without an audible restore. The turn's own focus.acquire supersedes it; if
// none arrives it expires after DefaultTTL.
func (m *Manager) CandidateAccept(candidateID, owner string, generation uint32) {
	m.update(func(now time.Time) error {
		p, ok := m.prov[candidateID]
		if !ok {
			return nil
		}
		delete(m.prov, candidateID)
		m.cancelDialogLocked()
		for _, l := range m.leases {
			if l.Owner == owner && !l.converted {
				return nil
			}
		}
		m.leases[candidateID] = &lease{
			Lease:     Lease{ID: candidateID, Owner: owner, Generation: generation, Kind: DialogInput, Expires: now.Add(DefaultTTL)},
			duckDB:    p.duckDB,
			converted: true,
		}
		return nil
	})
}

// DialogOutputStarted records the current dialog_output playback. Output that
// starts while an alert is foreground is preempted at once (§16.7).
func (m *Manager) DialogOutputStarted(playbackID, owner string, generation uint32) {
	m.update(func(time.Time) error {
		if m.alertID != "" && !m.alertBG {
			m.queue(func() { m.cancelPlayback(playbackID, generation, render.RampNormal) })
			return nil
		}
		m.dialogID, m.dialogOwner = playbackID, owner
		m.dialogGen, m.dialogPlaying = generation, true
		return nil
	})
}

// DialogOutputFinished records that the dialog_output playback finished.
func (m *Manager) DialogOutputFinished(playbackID string, generation uint32) {
	m.update(func(time.Time) error {
		if m.dialogPlaying && m.dialogID == playbackID && m.dialogGen == generation {
			m.dialogPlaying = false
			if !m.yieldAt.IsZero() {
				m.yieldAt = time.Time{}
				m.yielded = true
			}
		}
		return nil
	})
}

// AlertActive reports the occurrence at the head of the alert queue. A new
// occurrence gets a fresh background budget; one that becomes due during
// dialog output waits at most AlertYield.
func (m *Manager) AlertActive(occurrenceID string) {
	m.update(func(now time.Time) error {
		if occurrenceID == m.alertID {
			return nil
		}
		m.resetAlertLocked()
		m.alertID = occurrenceID
		if m.dialogPlaying {
			m.yieldAt = now.Add(AlertYield)
		}
		return nil
	})
}

// AlertBurstCompleted reports a completed foreground burst, which resets the
// occurrence's background budget.
func (m *Manager) AlertBurstCompleted(occurrenceID string) {
	m.update(func(now time.Time) error {
		if occurrenceID != m.alertID {
			return nil
		}
		m.budget, m.budgetSpent = 0, false
		m.bgSince = now
		return nil
	})
}

// AlertEnded reports that the occurrence stopped ringing (dismissed, snoozed,
// expired). physical selects the 10 ms stop ramp for the resulting change.
func (m *Manager) AlertEnded(occurrenceID string, physical bool) {
	m.update(func(time.Time) error {
		if occurrenceID != m.alertID {
			return nil
		}
		m.resetAlertLocked()
		if physical {
			m.ramp = render.RampPhysicalStop
		}
		return nil
	})
}

// Tick applies expiries and deadlines; call it at NextDeadline.
func (m *Manager) Tick() { m.update(func(time.Time) error { return nil }) }

// NextDeadline is the earliest time Tick has work to do.
func (m *Manager) NextDeadline() (time.Time, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	var next time.Time
	consider := func(t time.Time) {
		if !t.IsZero() && (next.IsZero() || t.Before(next)) {
			next = t
		}
	}
	for _, l := range m.leases {
		consider(l.Expires)
	}
	for _, p := range m.prov {
		consider(p.expires)
	}
	consider(m.yieldAt)
	if m.alertBG && !m.budgetSpent {
		consider(m.bgSince.Add(AlertBackgroundCap - m.budget))
	}
	return next, !next.IsZero()
}

func (m *Manager) resetAlertLocked() {
	m.alertID = ""
	m.alertBG = false
	m.budget, m.budgetSpent = 0, false
	m.yieldAt, m.yielded = time.Time{}, false
}

func (m *Manager) queue(fn func()) { m.pending = append(m.pending, fn) }

func (m *Manager) cancelPlayback(id string, generation uint32, ramp render.Ramp) {
	if m.cb.CancelPlayback != nil {
		m.cb.CancelPlayback(id, generation, ramp)
	}
}

// cancelDialogLocked cancels the current dialog output with the 30 ms fade.
func (m *Manager) cancelDialogLocked() {
	if !m.dialogPlaying {
		return
	}
	id, gen := m.dialogID, m.dialogGen
	m.dialogPlaying = false
	m.queue(func() { m.cancelPlayback(id, gen, render.RampNormal) })
}

func (m *Manager) expireLocked(now time.Time) {
	for id, l := range m.leases {
		if now.Before(l.Expires) {
			continue
		}
		delete(m.leases, id)
		expired := l.Lease
		if m.cb.LeaseExpired != nil {
			m.queue(func() { m.cb.LeaseExpired(expired) })
		}
		if m.dialogPlaying && m.dialogOwner == l.Owner && m.dialogGen <= l.Generation && !m.ownerLiveLocked(l.Owner) {
			m.cancelDialogLocked()
		}
	}
	for id, p := range m.prov {
		if !now.Before(p.expires) {
			delete(m.prov, id)
		}
	}
}

func (m *Manager) ownerLiveLocked(owner string) bool {
	for _, l := range m.leases {
		if l.Owner == owner {
			return true
		}
	}
	return false
}

func (m *Manager) deadlinesLocked(now time.Time) {
	if !m.yieldAt.IsZero() && !now.Before(m.yieldAt) {
		m.yieldAt = time.Time{}
		m.yielded = true
		m.cancelDialogLocked()
	}
	if m.alertBG && !m.budgetSpent && m.budget+now.Sub(m.bgSince) >= AlertBackgroundCap {
		m.budgetSpent = true
		m.cancelDialogLocked()
		if m.cb.AlertPreempted != nil {
			id := m.alertID
			m.queue(func() { m.cb.AlertPreempted(id) })
		}
	}
}

// recomputeLocked derives the policy from surviving leases and alert state
// (§16.2): the most attenuating duck wins; foreground order is physical stop,
// an alert whose budget expired, dialog, an ordinary due alert, content.
func (m *Manager) recomputeLocked(now time.Time) {
	var contentDB, dialogDB float64
	input, output := false, m.dialogPlaying
	for _, l := range m.leases {
		contentDB = min(contentDB, l.duckDB)
		if l.converted {
			dialogDB = min(dialogDB, l.duckDB)
		}
		switch l.Kind {
		case DialogInput:
			input = true
		case DialogOutput:
			output = true
		}
	}
	for _, p := range m.prov {
		contentDB = min(contentDB, p.duckDB)
		dialogDB = min(dialogDB, p.duckDB)
		input = true
	}
	alert := m.alertID != ""
	bg := alert && !m.budgetSpent && (!m.yieldAt.IsZero() || input || (output && !m.yielded))
	if bg != m.alertBG {
		if bg {
			m.bgSince = now
		} else {
			m.budget += now.Sub(m.bgSince)
		}
		m.alertBG = bg
	}
	out := Output{
		Mix: render.Policy{
			ContentDuckDB: contentDB,
			DialogDuckDB:  dialogDB,
			// Content stays paused while an occurrence is pending, foreground
			// or backgrounded, until the alert queue is empty (§6.2).
			PauseContent: alert,
		},
		AlertBackground: bg,
	}
	ramp := m.ramp
	m.ramp = render.RampNormal
	cur := m.out
	cur.Mix.Ramp = render.RampNormal
	if out == cur {
		return
	}
	out.Mix.Ramp = ramp
	m.out = out
	if m.cb.Apply != nil {
		m.queue(func() { m.cb.Apply(out) })
	}
}

func ttlOr(ttl time.Duration) time.Duration {
	if ttl <= 0 {
		return DefaultTTL
	}
	return ttl
}
