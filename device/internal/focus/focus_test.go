package focus

import (
	"errors"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/render"
)

type cancelCall struct {
	id   string
	gen  uint32
	ramp render.Ramp
}

type harness struct {
	t         *testing.T
	now       time.Time
	m         *Manager
	applied   []Output
	cancels   []cancelCall
	expired   []Lease
	preempted []string
}

func newHarness(t *testing.T) *harness {
	h := &harness{t: t, now: time.Unix(1000, 0)}
	h.m = New(Config{
		Now:    func() time.Time { return h.now },
		DuckDB: DefaultDuckDB,
		Callbacks: Callbacks{
			Apply: func(o Output) { h.applied = append(h.applied, o) },
			CancelPlayback: func(id string, gen uint32, ramp render.Ramp) {
				h.cancels = append(h.cancels, cancelCall{id, gen, ramp})
			},
			LeaseExpired:   func(l Lease) { h.expired = append(h.expired, l) },
			AlertPreempted: func(id string) { h.preempted = append(h.preempted, id) },
		},
	})
	return h
}

func (h *harness) advance(d time.Duration) {
	h.now = h.now.Add(d)
	h.m.Tick()
}

func (h *harness) must(err error) {
	h.t.Helper()
	if err != nil {
		h.t.Fatal(err)
	}
}

func TestDuckTakesMostAttenuatingLeaseAndRestores(t *testing.T) {
	h := newHarness(t)
	h.must(h.m.Acquire("a", "turn1", 1, DialogInput, 0))
	h.must(h.m.Acquire("b", "turn1", 1, DialogOutput, 0))
	if got := h.m.Output().Mix.ContentDuckDB; got != -18 {
		t.Fatalf("two -18 dB leases duck %g dB; ducks must not multiply", got)
	}
	h.m.SetDuckDB(-30)
	h.must(h.m.Acquire("c", "turn2", 2, DialogInput, 0))
	if got := h.m.Output().Mix.ContentDuckDB; got != -30 {
		t.Fatalf("duck = %g, want the most attenuating -30", got)
	}
	if got := h.m.Output().Mix.DialogDuckDB; got != 0 {
		t.Fatalf("a dialog lease ducked dialog output by %g dB", got)
	}
	h.must(h.m.Release("c", 2))
	if got := h.m.Output().Mix.ContentDuckDB; got != -18 {
		t.Fatalf("after releasing the deepest lease duck = %g, want -18", got)
	}
	h.must(h.m.Release("a", 1))
	h.must(h.m.Release("b", 1))
	out := h.m.Output()
	if out.Mix.ContentDuckDB != 0 || out.Mix.Ramp != render.RampNormal {
		t.Fatalf("restore = %+v", out)
	}
}

func TestStaleGenerationsAreRejected(t *testing.T) {
	h := newHarness(t)
	h.must(h.m.Acquire("a", "turn", 5, DialogInput, 0))
	if err := h.m.Renew("a", 4, 0); !errors.Is(err, ErrStale) {
		t.Fatalf("renew with older generation: %v", err)
	}
	if err := h.m.Release("a", 4); !errors.Is(err, ErrStale) {
		t.Fatalf("release with older generation: %v", err)
	}
	if h.m.Output().Mix.ContentDuckDB != -18 {
		t.Fatal("a stale release removed the lease")
	}
	if err := h.m.Renew("missing", 1, 0); !errors.Is(err, ErrUnknown) {
		t.Fatalf("renew of unknown lease: %v", err)
	}
}

func TestLeaseExpiryCancelsOnlyTheExpiredOwnersOutput(t *testing.T) {
	h := newHarness(t)
	h.must(h.m.Acquire("old", "turn7", 7, DialogOutput, time.Second))
	h.m.DialogOutputStarted("p7", "turn7", 7)
	h.advance(999 * time.Millisecond)
	if len(h.cancels) != 0 {
		t.Fatal("cancelled before the TTL elapsed")
	}
	h.advance(time.Millisecond)
	if len(h.expired) != 1 || h.expired[0].ID != "old" {
		t.Fatalf("expired = %+v", h.expired)
	}
	if len(h.cancels) != 1 || h.cancels[0] != (cancelCall{"p7", 7, render.RampNormal}) {
		t.Fatalf("cancels = %+v", h.cancels)
	}
	if h.m.Output().Mix.ContentDuckDB != 0 {
		t.Fatal("expiry left content ducked")
	}

	// A newer owner's output survives an older lease's expiry.
	h.cancels = nil
	h.must(h.m.Acquire("a", "turn8", 8, DialogInput, time.Second))
	h.must(h.m.Acquire("b", "turn9", 9, DialogOutput, time.Minute))
	h.m.DialogOutputStarted("p9", "turn9", 9)
	h.advance(time.Second)
	if len(h.cancels) != 0 {
		t.Fatalf("expiry of generation 8 cancelled generation 9 output: %+v", h.cancels)
	}
}

func TestAlertForegroundPausesContentAndWakeBackgroundsIt(t *testing.T) {
	h := newHarness(t)
	h.m.AlertActive("alarm1")
	out := h.m.Output()
	if out.AlertBackground || !out.Mix.PauseContent {
		t.Fatalf("due alert = %+v, want foreground with content paused", out)
	}
	h.must(h.m.Acquire("in", "turn", 1, DialogInput, 0))
	out = h.m.Output()
	if !out.AlertBackground || !out.Mix.PauseContent || out.Mix.ContentDuckDB != -18 {
		t.Fatalf("wake during alert = %+v", out)
	}
	h.must(h.m.Release("in", 1))
	if h.m.Output().AlertBackground {
		t.Fatal("alert stayed background after the turn ended")
	}
	h.m.AlertEnded("alarm1", false)
	if h.m.Output().Mix.PauseContent {
		t.Fatal("content still paused with no alert")
	}
}

func TestDueAlertWaitsForDialogOutputAtMostTwoSeconds(t *testing.T) {
	h := newHarness(t)
	h.must(h.m.Acquire("out", "turn", 3, DialogOutput, time.Minute))
	h.m.DialogOutputStarted("tts", "turn", 3)
	h.m.AlertActive("alarm")
	out := h.m.Output()
	if !out.AlertBackground || !out.Mix.PauseContent {
		t.Fatalf("alert due during TTS = %+v, want background with content paused", out)
	}
	h.advance(AlertYield - time.Nanosecond)
	if len(h.cancels) != 0 {
		t.Fatal("dialog cancelled before 2 s")
	}
	h.advance(time.Nanosecond)
	if len(h.cancels) != 1 || h.cancels[0] != (cancelCall{"tts", 3, render.RampNormal}) {
		t.Fatalf("cancels = %+v, want the TTS with the 30 ms fade", h.cancels)
	}
	if h.m.Output().AlertBackground {
		t.Fatal("alert still background after the 2 s yield")
	}
}

func TestDueAlertForegroundsWhenDialogDrainsFirst(t *testing.T) {
	h := newHarness(t)
	h.must(h.m.Acquire("out", "turn", 3, DialogOutput, time.Minute))
	h.m.DialogOutputStarted("tts", "turn", 3)
	h.m.AlertActive("alarm")
	h.now = h.now.Add(500 * time.Millisecond)
	h.m.DialogOutputFinished("tts", 3)
	if h.m.Output().AlertBackground {
		t.Fatal("alert did not take the foreground when dialog output drained")
	}
	h.advance(2 * time.Second)
	if len(h.cancels) != 0 {
		t.Fatalf("drained output was cancelled: %+v", h.cancels)
	}
}

func TestDialogOutputStartedUnderForegroundAlertIsPreempted(t *testing.T) {
	h := newHarness(t)
	h.m.AlertActive("timer")
	h.m.DialogOutputStarted("announce", "announcement", 4)
	if len(h.cancels) != 1 || h.cancels[0].id != "announce" {
		t.Fatalf("cancels = %+v", h.cancels)
	}
	if h.m.Output().AlertBackground {
		t.Fatal("an announcement backgrounded a foreground alert")
	}
}

func TestBackgroundCapIsOccurrenceLevel(t *testing.T) {
	h := newHarness(t)
	h.m.AlertActive("alarm")
	h.must(h.m.Acquire("w1", "turn1", 1, DialogInput, time.Minute))
	h.advance(10 * time.Second)
	h.must(h.m.Release("w1", 1))
	h.advance(time.Second) // foreground, but no burst completed
	h.must(h.m.Acquire("w2", "turn2", 2, DialogOutput, time.Minute))
	h.m.DialogOutputStarted("tts", "turn2", 2)
	if !h.m.Output().AlertBackground {
		t.Fatal("second wake did not background the alert")
	}
	if at, ok := h.m.NextDeadline(); !ok || !at.Equal(h.now.Add(5*time.Second)) {
		t.Fatalf("next deadline = %v, want the remaining 5 s budget", at)
	}
	h.advance(5*time.Second - time.Nanosecond)
	if len(h.preempted) != 0 {
		t.Fatal("preempted before 15 s of background")
	}
	h.advance(time.Nanosecond)
	if len(h.preempted) != 1 || h.preempted[0] != "alarm" {
		t.Fatalf("preempted = %v", h.preempted)
	}
	if len(h.cancels) != 1 || h.cancels[0].id != "tts" {
		t.Fatalf("cancels = %+v, want remaining dialog output cancelled", h.cancels)
	}
	if h.m.Output().AlertBackground {
		t.Fatal("alert still background after its budget expired")
	}
	h.must(h.m.Acquire("w3", "turn3", 3, DialogInput, time.Minute))
	if h.m.Output().AlertBackground {
		t.Fatal("a wake backgrounded an alert whose budget expired")
	}
	h.m.AlertBurstCompleted("alarm")
	if !h.m.Output().AlertBackground {
		t.Fatal("after a completed foreground burst a wake must background again")
	}
}

func TestProvisionalDuckRateLimitAndRestore(t *testing.T) {
	h := newHarness(t)
	if h.m.CandidateOpen("idle", false, 0) {
		t.Fatal("ducked a candidate while producing no sound")
	}
	h.m.AlertActive("alarm")
	if !h.m.CandidateOpen("c1", true, time.Second) {
		t.Fatal("first provisional duck refused")
	}
	out := h.m.Output()
	if out.Mix.ContentDuckDB != -18 || out.Mix.DialogDuckDB != -18 || !out.AlertBackground {
		t.Fatalf("provisional duck = %+v", out)
	}
	if !h.m.CandidateOpen("c1", true, time.Second) {
		t.Fatal("repeat open of the same candidate lost its duck")
	}
	if !h.m.CandidateOpen("c2", true, time.Second) {
		t.Fatal("second duck in the window refused")
	}
	if h.m.CandidateOpen("c3", true, time.Second) {
		t.Fatal("third duck within 5 s was applied")
	}
	h.m.CandidateRelease("c1")
	h.m.CandidateRelease("c2")
	out = h.m.Output()
	if out.Mix.ContentDuckDB != 0 || out.Mix.DialogDuckDB != 0 || out.AlertBackground {
		t.Fatalf("release did not restore: %+v", out)
	}
	h.now = h.now.Add(ProvisionalWindow)
	if !h.m.CandidateOpen("c4", true, time.Second) {
		t.Fatal("duck refused after the window passed")
	}
	h.advance(time.Second)
	if out := h.m.Output(); out.Mix.ContentDuckDB != 0 || out.AlertBackground {
		t.Fatalf("expiry did not restore: %+v", out)
	}
}

func TestAcceptConvertsProvisionalDuckIntoTurnFocus(t *testing.T) {
	h := newHarness(t)
	h.m.CandidateOpen("c", true, time.Second)
	h.m.CandidateAccept("c", "turn", 2)
	before := len(h.applied)
	if got := h.m.Output().Mix.ContentDuckDB; got != -18 {
		t.Fatalf("accept restored the duck: %g", got)
	}
	h.must(h.m.Acquire("L", "turn", 2, DialogInput, time.Minute))
	out := h.m.Output()
	if out.Mix.ContentDuckDB != -18 || out.Mix.DialogDuckDB != 0 {
		t.Fatalf("turn focus = %+v, want content ducked and the provisional dialog duck superseded", out)
	}
	for _, o := range h.applied[before:] {
		if o.Mix.ContentDuckDB == 0 {
			t.Fatal("content was restored between accept and the turn's lease")
		}
	}
	h.advance(DefaultTTL)
	if len(h.expired) != 0 {
		t.Fatalf("superseded conversion expired later: %+v", h.expired)
	}
}

func TestPhysicalStopUsesTheShortRamp(t *testing.T) {
	h := newHarness(t)
	h.m.AlertActive("alarm")
	h.m.AlertEnded("alarm", true)
	last := h.applied[len(h.applied)-1]
	if last.Mix.Ramp != render.RampPhysicalStop || last.Mix.PauseContent {
		t.Fatalf("physical stop applied %+v", last)
	}
	h.m.AlertActive("next")
	h.m.AlertEnded("next", false)
	if last := h.applied[len(h.applied)-1]; last.Mix.Ramp != render.RampNormal {
		t.Fatalf("normal stop applied ramp %v", last.Mix.Ramp)
	}
}

func TestEndSessionReleasesEverything(t *testing.T) {
	h := newHarness(t)
	h.must(h.m.Acquire("o", "turn", 1, DialogOutput, time.Minute))
	h.m.DialogOutputStarted("tts", "turn", 1)
	h.m.CandidateOpen("c", true, time.Minute)
	h.m.EndSession()
	if out := h.m.Output(); out.Mix.ContentDuckDB != 0 || out.Mix.DialogDuckDB != 0 {
		t.Fatalf("session end left %+v", out)
	}
	if len(h.cancels) != 1 || h.cancels[0].id != "tts" {
		t.Fatalf("cancels = %+v", h.cancels)
	}
}
