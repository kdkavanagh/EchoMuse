package alerts

import (
	"math"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/uuid"
)

func minute(n int) int64 { return int64(n) * 60000 }

func TestDismissIsDurableBeforeAckAndNeverReringsAfterReboot(t *testing.T) {
	h := newHarness(t)
	h.trust()
	a := alarm("wake", t0+minute(1))
	h.install("E1", 1, a)
	h.advanceTo(a.dueUTC)
	if h.activeID() != a.occID() {
		t.Fatalf("not ringing: %+v", h.active())
	}
	if _, active := h.fill(4800); !active {
		t.Fatal("ringing alarm produced no audio")
	}

	res := h.e.Act("", a.occID(), ActionDismiss, "button")
	if res.Status != StatusDurable || res.Operation == nil || res.Ended == nil || res.Ended.Reason != "button" {
		t.Fatalf("dismiss %+v", res)
	}
	// Durable before the ack: an independent reader of the store sees it now.
	other, err := openStore(h.root)
	if err != nil {
		t.Fatal(err)
	}
	if op := other.st.ops[res.OpID]; op == nil || op.Action != ActionDismiss || op.OccurrenceID != a.occID() {
		t.Fatalf("dismiss not journaled before ack: %+v", other.st.ops)
	}
	other.close()
	// The ring fades out within 10 ms.
	out, _ := h.fill(960)
	for i := 480; i < len(out); i++ {
		if out[i] != 0 {
			t.Fatalf("sample %d = %d after the 10 ms stop fade", i, out[i])
		}
	}

	h.reboot(time.Minute)
	h.trust()
	h.e.Poll()
	if h.active() != nil || len(h.e.State().Queue) != 0 {
		t.Fatalf("dismissed occurrence re-armed after reboot: %+v", h.e.State())
	}
	if ops := h.e.PendingOperations(); len(ops) != 1 || ops[0].OpID != res.OpID {
		t.Fatalf("operation not re-sent after reboot: %+v", ops)
	}
	if again := h.e.Act(res.OpID, a.occID(), ActionDismiss, "button"); again.Status != StatusDurable {
		t.Fatalf("retried op id: %+v", again)
	}
}

func TestDismissPersistenceFailureStopsInMemoryOnly(t *testing.T) {
	h := newHarness(t)
	h.trust()
	a := alarm("wake", t0+minute(1))
	h.install("E1", 1, a)
	h.advanceTo(a.dueUTC)
	h.e.st.log.Close() // every later write fails

	res := h.e.Act("", a.occID(), ActionDismiss, "voice")
	if res.Status != StatusApplied || res.Error != "persistence_failed" || res.Operation == nil {
		t.Fatalf("dismiss with failing store %+v", res)
	}
	h.clk.advance(time.Second)
	h.e.Poll()
	if h.active() != nil {
		t.Fatal("re-rang after an unpersisted dismiss")
	}
	if ops := h.e.PendingOperations(); len(ops) != 1 || ops[0].OpID != res.OpID {
		t.Fatalf("unpersisted op not queued for upload: %+v", ops)
	}
}

func TestSnoozeChildIdentityAndSupersession(t *testing.T) {
	h := newHarness(t)
	h.trust()
	parentDue := time.Date(2026, 9, 23, 11, 30, 0, 0, time.UTC).UnixMilli()
	p := alarm("wake", parentDue)
	p.id = "0b8f4e0a-3c4b-5e6d-9f10-112233445566"
	p.label = "Wake up"
	h.install("E1", 1, p)
	h.advanceTo(parentDue + 500) // the press is 06:30:00.5 local

	res := h.e.Act("", p.id, ActionSnooze, "voice")
	if res.Status != StatusDurable || res.Operation == nil || res.Operation.Child == nil {
		t.Fatalf("snooze %+v", res)
	}
	child := *res.Operation.Child
	// Values from CPython uuid.uuid5 (see TestUUID5MatchesPython).
	want := ChildRef{
		ScheduleID:   "9cc967de-5fa3-5373-b4f0-d9841f3f79f7",
		OccurrenceID: "164ed469-897e-5b59-b53b-23431a19381b",
		DueUTCMs:     parentDue + 1000 + 540000,
		DueLocal:     "2026-09-23T06:39:01-05:00",
	}
	if child != want {
		t.Fatalf("child\n got %+v\nwant %+v", child, want)
	}
	if res.Operation.Action != ActionSnooze || res.Operation.OccurrenceID != p.id {
		t.Fatalf("operation %+v", res.Operation)
	}
	// Parent and child are one journal transaction.
	other, err := openStore(h.root)
	if err != nil {
		t.Fatal(err)
	}
	if other.st.ops[res.OpID] == nil || other.st.children[want.OccurrenceID] == nil {
		t.Fatal("snooze parent and child not persisted together")
	}
	if c := other.st.children[want.OccurrenceID].Occurrence; c.Label != "Snoozed: Wake up" || c.ScheduleID != want.ScheduleID {
		t.Fatalf("child occurrence %+v", c)
	}
	other.close()
	if q := h.e.State().Queue; len(q) != 1 || q[0] != want.OccurrenceID {
		t.Fatalf("child not armed: %v", q)
	}

	// The controller's copy keys the child in UTC: a different occurrence ID
	// for the same schedule and instant. It supersedes the local child.
	cp := occSpec{sched: want.ScheduleID, dueUTC: want.DueUTCMs, dueLocal: "2026-09-23T11:39:01+00:00",
		label: "Snoozed: Wake up", maxRingMs: 600000, rampMs: 20000, loopGapMs: 2000, snoozeMs: 540000,
		sound: SoundFallback, volume: "null", revision: 2}
	if cp.occID() == want.OccurrenceID {
		t.Fatal("fixture must differ in occurrence ID")
	}
	if ack := h.delta("E1", 2, cp.json()); ack.NeedSnapshot {
		t.Fatalf("delta %+v", ack)
	}
	if q := h.e.State().Queue; len(q) != 1 || q[0] != cp.occID() {
		t.Fatalf("double arming after supersession: %v", q)
	}
	h.advanceTo(want.DueUTCMs)
	if h.activeID() != cp.occID() {
		t.Fatalf("delivered copy not ringing: %+v", h.active())
	}
	if q := h.e.State().Queue; len(q) != 0 {
		t.Fatalf("second ring queued: %v", q)
	}
}

func TestRingingChildAdoptsDeliveredCopy(t *testing.T) {
	h := newHarness(t)
	h.trust()
	p := alarm("wake", t0+minute(1))
	h.install("E1", 1, p)
	h.advanceTo(p.dueUTC)
	res := h.e.Act("", p.occID(), ActionSnooze, "voice")
	child := res.Operation.Child
	h.advanceTo(child.DueUTCMs)
	if h.activeID() != child.OccurrenceID {
		t.Fatalf("child not ringing: %+v", h.active())
	}
	deadline := h.active().DeadlineMonoNs
	v := h.e.voice.Load()

	cp := occSpec{sched: child.ScheduleID, dueUTC: child.DueUTCMs, dueLocal: "2026-09-23T16:00:00+00:00",
		label: "Snoozed: wake", maxRingMs: 600000, rampMs: 20000, loopGapMs: 2000, snoozeMs: 540000,
		sound: SoundFallback, volume: "null", revision: 2}
	h.delta("E1", 2, cp.json())
	h.e.Poll()
	a := h.active()
	if a == nil || a.ID != cp.occID() || a.DeadlineMonoNs != deadline || h.e.voice.Load() != v {
		t.Fatalf("ring restarted or lost on supersession: %+v", a)
	}
	if len(h.rec.ended) != 0 { // the parent's snooze reported its end through ActResult
		t.Fatalf("ring_ended events %+v", h.rec.ended)
	}
}

func TestQueueOrdersAlarmsAndTimerRings(t *testing.T) {
	h := newHarness(t)
	h.trust()
	due := t0 + minute(1)
	a, b := alarm("a", due), alarm("b", due)
	first, second := a, b
	if b.occID() < a.occID() {
		first, second = b, a
	}
	h.install("E1", 1, a, b)
	h.advanceTo(due - 1000)
	if err := h.e.HandleTimerRing(TimerRing{RingID: "t-early", Name: "tea", Sound: SoundFallback, LoopGapMs: 2000, MaxRingMs: 900000}); err != nil {
		t.Fatal(err)
	}
	h.e.Poll()
	if h.activeID() != "t-early" {
		t.Fatalf("earlier timer not first: %+v", h.active())
	}
	h.advanceTo(due + 5000)
	if err := h.e.HandleTimerRing(TimerRing{RingID: "t-late", Name: "pasta", Sound: SoundFallback, LoopGapMs: 2000, MaxRingMs: 900000}); err != nil {
		t.Fatal(err)
	}
	h.e.Poll()
	if q := h.e.State().Queue; len(q) != 3 || q[0] != first.occID() || q[1] != second.occID() || q[2] != "t-late" {
		t.Fatalf("queue %v", q)
	}
	if r := h.e.Act("", "t-early", ActionSnooze, "voice"); r.Status != StatusRejected || r.Error != "not_snoozable" {
		t.Fatalf("timer snooze %+v", r)
	}
	for _, want := range []string{"t-early", first.occID(), second.occID(), "t-late"} {
		h.e.Poll()
		if h.activeID() != want {
			t.Fatalf("active %q, want %q", h.activeID(), want)
		}
		r := h.e.Act("", want, ActionDismiss, "entity")
		if r.Ended == nil || r.Ended.ID != want || r.Ended.Reason != "entity" {
			t.Fatalf("stop %s: %+v", want, r)
		}
		if strings.HasPrefix(want, "t-") != (r.Status == StatusApplied && r.Operation == nil) {
			t.Fatalf("timer stops are not journaled, alarm stops are: %+v", r)
		}
	}
	h.e.Poll()
	if h.active() != nil {
		t.Fatal("queue not drained")
	}
}

// The controller's voice stop and LLM tool calls carry UUIDv5 op_ids
// (uuid5(NAMESPACE_URL, …)); rejecting them left "<wake> stop" unable to stop
// a ringing timer.
func TestActAcceptsTheControllersUUIDv5OpIDs(t *testing.T) {
	h := newHarness(t)
	h.trust()
	if err := h.e.HandleTimerRing(TimerRing{RingID: "t", Name: "tea", Sound: SoundFallback, LoopGapMs: 2000, MaxRingMs: 900000}); err != nil {
		t.Fatal(err)
	}
	h.e.Poll()
	if h.activeID() != "t" {
		t.Fatalf("timer not ringing: %+v", h.active())
	}
	for _, bad := range []string{"not-a-uuid", strings.ToUpper(uuid.NewV4().String()), "6ba7b811-9dad-11d1-00b4-00c04fd430c8"} {
		if r := h.e.Act(bad, "t", ActionDismiss, "voice"); r.Status != StatusRejected || r.Error != "invalid_op_id" {
			t.Fatalf("op_id %q: %+v", bad, r)
		}
	}
	voice := uuid.V5(uuid.NamespaceURL, "turn-1|dismiss").String()
	r := h.e.Act(voice, "t", ActionDismiss, "voice")
	if r.Status != StatusApplied || r.Ended == nil || r.Ended.ID != "t" || r.OpID != voice {
		t.Fatalf("voice stop with a UUIDv5 op_id: %+v", r)
	}
	h.e.Poll()
	if h.active() != nil {
		t.Fatal("timer still ringing")
	}

	a := alarm("wake", h.utcNow()+minute(1))
	h.install("E1", 1, a)
	h.advanceTo(a.dueUTC)
	llm := uuid.V5(uuid.NamespaceURL, "request-1").String()
	if r := h.e.Act(llm, a.occID(), ActionDismiss, "llm"); r.Status != StatusDurable || r.Operation == nil || r.Operation.OpID != llm {
		t.Fatalf("llm dismiss with a UUIDv5 op_id: %+v", r)
	}
}

func TestCatchUpWindowAndMissedExpiry(t *testing.T) {
	h := newHarness(t)
	h.trust()
	now := h.utcNow()
	missed := alarm("missed", now-minute(31))
	late := alarm("late", now-minute(29))
	h.install("E1", 1, missed, late)
	if h.activeID() != late.occID() {
		t.Fatalf("catch-up occurrence not ringing: %+v", h.active())
	}
	if len(h.rec.ops) != 1 || h.rec.ops[0].Action != ActionExpire || *h.rec.ops[0].Reason != reasonMissed ||
		h.rec.ops[0].OccurrenceID != missed.occID() || h.rec.ops[0].Source != SourceExecutor {
		t.Fatalf("missed expiry %+v", h.rec.ops)
	}

	// Queued behind a long ring, an occurrence is rechecked at the head.
	h2 := newHarness(t)
	h2.trust()
	ringing := alarm("ringing", h2.utcNow()+minute(1))
	ringing.maxRingMs = minute(40)
	queued := alarm("queued", h2.utcNow()+minute(2))
	h2.install("E1", 1, ringing, queued)
	h2.advanceTo(ringing.dueUTC)
	h2.advanceTo(queued.dueUTC + minute(31))
	h2.e.Act("", ringing.occID(), ActionDismiss, "button")
	h2.e.Poll()
	if h2.active() != nil || len(h2.rec.ops) != 1 || *h2.rec.ops[0].Reason != reasonMissed {
		t.Fatalf("stale queued occurrence rang: active=%+v ops=%+v", h2.active(), h2.rec.ops)
	}
}

func TestRingDeadlineSurvivesRestartButIsNotRenewed(t *testing.T) {
	h := newHarness(t)
	h.trust()
	a := alarm("wake", t0+minute(1))
	a.maxRingMs = 60000
	h.install("E1", 1, a)
	if arm := h.e.st.st.armed[a.occID()]; arm == nil || arm.BootID != bootA || arm.DueUTCMs != a.dueUTC {
		t.Fatalf("armed monotonic deadline not persisted: %+v", arm)
	}
	h.advanceTo(a.dueUTC)
	started := h.active()

	h.clk.advance(30 * time.Second)
	h.restart()
	h.e.Poll()
	resumed := h.active()
	if resumed == nil || resumed.StartedMonoNs != started.StartedMonoNs || resumed.DeadlineMonoNs != started.DeadlineMonoNs {
		t.Fatalf("restart changed the ring: %+v vs %+v", resumed, started)
	}
	if got, want := h.e.voice.Load().startElapsed, int64(30*sampleRate); got != want {
		t.Fatalf("resumed ramp position %d, want %d", got, want)
	}

	// Across a reboot the deadline is reconstructed from UTC, only once trusted.
	h.reboot(10 * time.Second)
	h.e.Poll()
	if st := h.e.State(); st.Active != nil || st.ClockTrusted {
		t.Fatalf("untrusted clock fired a wall-clock deadline: %+v", st)
	}
	h.trust()
	h.e.Poll()
	if h.activeID() != a.occID() {
		t.Fatal("ringing occurrence not resumed within its deadline")
	}
	h.advanceTo(a.dueUTC + 60000)
	if h.active() != nil {
		t.Fatal("ring outlived its original deadline")
	}
	if len(h.rec.ops) != 1 || *h.rec.ops[0].Reason != reasonTimedOut {
		t.Fatalf("timed_out expiry %+v", h.rec.ops)
	}
	if n := len(h.rec.ended); n != 1 || h.rec.ended[0].Reason != "limit" {
		t.Fatalf("ring_ended %+v", h.rec.ended)
	}

	// A restart after the deadline expires it without ringing.
	h3 := newHarness(t)
	h3.trust()
	b := alarm("b", t0+minute(1))
	b.maxRingMs = 60000
	h3.install("E1", 1, b)
	h3.advanceTo(b.dueUTC)
	h3.clk.advance(90 * time.Second)
	h3.restart()
	h3.e.Poll()
	if h3.active() != nil || len(h3.rec.ops) != 1 || *h3.rec.ops[0].Reason != reasonTimedOut ||
		len(h3.rec.ended) != 1 || h3.rec.ended[0].Reason != endRestart {
		t.Fatalf("expired ring resumed: active=%+v ops=%+v ended=%+v", h3.active(), h3.rec.ops, h3.rec.ended)
	}
}

func TestRampAndLoopGapSampleMath(t *testing.T) {
	h := newHarness(t)
	h.trust()
	pcm := make([]int16, 1000)
	for i := range pcm {
		pcm[i] = 20000
	}
	sha := writeWAV(t, h.root, pcm)
	a := alarm("ramp", t0+minute(1))
	a.sound, a.loopGapMs, a.rampMs = sha, 10, 100 // 480-sample gap, 4,800-sample ramp
	a.volume = "0.75"
	h.install("E1", 1, a)
	h.advanceTo(a.dueUTC)
	if f := h.rec.focus[len(h.rec.focus)-1]; f.Volume == nil || *f.Volume != 0.75 {
		t.Fatalf("desired alert volume %+v", f)
	}
	out, _ := h.fill(4 * 1480)
	for k, got := range out {
		var want int16
		if k%1480 < 1000 {
			g := math.Min(1, float64(k)/4800)
			want = int16(math.Round(20000 * g))
		}
		if got != want {
			t.Fatalf("sample %d = %d, want %d", k, got, want)
		}
	}

	// A timer ring has no ramp.
	h.e.Act("", a.occID(), ActionDismiss, "button")
	h.fill(480)
	if err := h.e.HandleTimerRing(TimerRing{RingID: "t", Name: "x", Sound: sha, LoopGapMs: 10, MaxRingMs: 60000}); err != nil {
		t.Fatal(err)
	}
	h.e.Poll()
	out, _ = h.fill(1480 + 1)
	if out[0] != 20000 || out[999] != 20000 || out[1000] != 0 || out[1479] != 0 || out[1480] != 20000 {
		t.Fatalf("timer loop %d %d %d %d %d", out[0], out[999], out[1000], out[1479], out[1480])
	}
}

func TestFallbackToneCadence(t *testing.T) {
	pcm := FallbackPCM()
	if len(pcm) != 4*9600+3*4800+86400 {
		t.Fatalf("period %d samples", len(pcm))
	}
	burstStart := []int{0, 14400, 28800, 43200}
	for b, start := range burstStart {
		burst := pcm[start : start+9600]
		crossings, peak := 0, 0
		for i, s := range burst {
			if i > 0 && (burst[i-1] < 0) != (s < 0) {
				crossings++
			}
			peak = max(peak, int(math.Abs(float64(s))))
			edge := min(i, 9599-i)
			if edge < 480 && math.Abs(float64(s)) > 16384*float64(edge)/480+1 {
				t.Fatalf("burst %d sample %d exceeds the 10 ms fade", b, i)
			}
		}
		if crossings < 350 || crossings > 354 { // 880 Hz for 200 ms
			t.Fatalf("burst %d: %d zero crossings", b, crossings)
		}
		if peak < 16300 {
			t.Fatalf("burst %d peak %d", b, peak)
		}
		gapEnd := start + 9600 + 4800
		if b == 3 {
			gapEnd = len(pcm)
		}
		for i := start + 9600; i < gapEnd; i++ {
			if pcm[i] != 0 {
				t.Fatalf("gap after burst %d not silent at %d", b, i)
			}
		}
	}

	// A missing asset selects the fallback at once, with its own cadence.
	h := newHarness(t)
	h.trust()
	a := alarm("missing", t0+minute(1))
	a.sound = strings.Repeat("ab", 32)
	a.rampMs = 0
	h.install("E1", 1, a)
	if got := h.e.MissingSounds(); len(got) != 1 || got[0] != a.sound {
		t.Fatalf("missing sounds %v", got)
	}
	h.advanceTo(a.dueUTC)
	if v := h.e.voice.Load(); v == nil || &v.pcm[0] != &pcm[0] || v.gap != 0 {
		t.Fatal("missing asset did not select the fallback")
	}
	// A corrupt asset (hash mismatch) is refused the same way.
	if err := os.MkdirAll(AssetDir(h.root), 0o750); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(h.e.assetPath(a.sound), []byte("RIFF"), 0o640); err != nil {
		t.Fatal(err)
	}
	if got, gap := h.e.soundLocked(a.sound, 2000); &got[0] != &pcm[0] || gap != 0 {
		t.Fatal("corrupt asset accepted")
	}
}

func TestBackgroundSilencesButKeepsIdentityAndDeadline(t *testing.T) {
	h := newHarness(t)
	h.trust()
	if err := h.e.HandleTimerRing(TimerRing{RingID: "t", Name: "pasta", Sound: SoundFallback, LoopGapMs: 0, MaxRingMs: 10000}); err != nil {
		t.Fatal(err)
	}
	h.e.Poll()
	h.fill(4800)
	h.e.SetBackground(true)
	h.e.Poll()
	out, _ := h.fill(1440) // the 30 ms focus ramp
	if out[len(out)-1] != 0 {
		t.Fatal("burst not silenced within 30 ms")
	}
	if _, active := h.fill(48000); active {
		t.Fatal("backgrounded ring reported audible")
	}
	st := h.e.State()
	if st.Active == nil || st.Active.ID != "t" || st.Active.Foreground {
		t.Fatalf("background lost identity: %+v", st.Active)
	}
	if f := h.rec.focus[len(h.rec.focus)-1]; !f.Active || f.Foreground {
		t.Fatalf("focus event %+v", f)
	}
	h.clk.advance(10 * time.Second)
	h.e.Poll()
	if h.active() != nil || len(h.rec.ended) != 1 || h.rec.ended[0].Reason != "limit" {
		t.Fatalf("deadline did not run while backgrounded: %+v %+v", h.active(), h.rec.ended)
	}
	if f := h.rec.focus[len(h.rec.focus)-1]; f.Active {
		t.Fatalf("alert focus not released: %+v", f)
	}

	// Ramp progress also runs while backgrounded.
	h.e.SetBackground(false)
	a := alarm("ramp", t0+minute(1))
	h.install("E1", 1, a)
	h.advanceTo(a.dueUTC)
	h.e.SetBackground(true)
	h.fill(48000)
	if got := h.e.mix.elapsed; got != 48000 {
		t.Fatalf("elapsed %d samples after 1 s in background", got)
	}
	if f := h.rec.focus[len(h.rec.focus)-1]; f.ID != a.occID() || f.Volume != nil {
		t.Fatalf("alarm focus %+v", f)
	}
}

func TestDeliveredTombstoneStopsRing(t *testing.T) {
	h := newHarness(t)
	h.trust()
	a := alarm("wake", t0+minute(1))
	h.install("E1", 1, a)
	h.advanceTo(a.dueUTC)
	h.delta("E1", 2, tombJSON(a.occID(), 2, "deleted"))
	h.e.Poll()
	if h.active() != nil || len(h.rec.ended) != 1 || h.rec.ended[0].Reason != "stopped" {
		t.Fatalf("tombstoned ring continued: %+v %+v", h.active(), h.rec.ended)
	}
	if len(h.rec.ops) != 0 {
		t.Fatal("a controller deletion produced a local operation")
	}
}

func TestTimerRingsAreNotPersisted(t *testing.T) {
	h := newHarness(t)
	if err := h.e.HandleTimerRing(TimerRing{RingID: "t", Name: "pasta", Sound: SoundFallback, LoopGapMs: 2000, MaxRingMs: 900000}); err != nil {
		t.Fatal(err)
	}
	h.e.Poll()
	if h.activeID() != "t" {
		t.Fatal("timer ring did not start without a trusted clock")
	}
	h.restart()
	h.e.Poll()
	if h.active() != nil {
		t.Fatal("timer ring survived a restart")
	}
}

func TestClockTrustEstimator(t *testing.T) {
	clk := &fakeClock{ns: int64(time.Hour)}
	c := newClockEstimator(bootA, clk, nil)
	offset := int64(1_790_000_000_000)
	exchange := func(rtt time.Duration, skewMs int64) bool {
		t.Helper()
		clk.advance(time.Second)
		nonce, ok := c.requestIfDue()
		if !ok {
			t.Fatal("request not due")
		}
		sent := clk.ns
		clk.advance(rtt)
		utc := (sent+clk.ns)/2/int64(time.Millisecond) + offset + skewMs
		changed, err := c.reply(nonce, utc)
		if err != nil {
			t.Fatal(err)
		}
		return changed
	}
	exchange(600*time.Millisecond, 0)
	exchange(600*time.Millisecond, 0)
	if c.trusted {
		t.Fatal("trusted replies with RTT > 500 ms")
	}
	exchange(100*time.Millisecond, 0)
	exchange(100*time.Millisecond, 1500) // disagrees by more than 1 s
	if c.trusted {
		t.Fatal("trusted disagreeing replies")
	}
	if !exchange(500*time.Millisecond, 1400) { // agrees with the previous within 1 s
		t.Fatal("agreeing pair not trusted")
	}
	if got := c.offsetMs; got != offset+1450 {
		t.Fatalf("offset %d, want %d", got-offset, 1450)
	}
	if _, err := c.reply("unknown", 0); err == nil {
		t.Fatal("reply without request accepted")
	}
	// Trusted: the next request waits 10 minutes.
	if _, ok := c.requestIfDue(); ok {
		t.Fatal("request due immediately after trust")
	}
	clk.advance(10 * time.Minute)
	if _, ok := c.requestIfDue(); !ok {
		t.Fatal("refresh not due after 10 min")
	}
	// A saved anchor is reused only within the boot that made it.
	if !newClockEstimator(bootA, clk, c.anchor()).trusted || newClockEstimator(bootB, clk, c.anchor()).trusted {
		t.Fatal("anchor reuse across boots")
	}
}

func TestClockAnchorPersistsWithinBoot(t *testing.T) {
	h := newHarness(t)
	h.trust()
	h.restart()
	if !h.e.ClockInfo().Trusted {
		t.Fatal("anchor not restored within the same boot")
	}
	h.reboot(time.Second)
	info := h.e.ClockInfo()
	if info.Trusted || info.UTCMs != nil || h.e.State().ClockTrusted {
		t.Fatalf("anchor reused after reboot: %+v", info)
	}
}

func TestWakeLockFileProtocol(t *testing.T) {
	h := newHarness(t)
	read := func(p string) string { b, _ := os.ReadFile(p); return string(b) }
	reset := func() {
		_ = os.WriteFile(h.paths.Lock, nil, 0o600)
		_ = os.WriteFile(h.paths.Unlock, nil, 0o600)
	}
	// Startup proves the lock, then releases it because nothing is armed.
	if read(h.paths.Lock) != wakeLockName || read(h.paths.Unlock) != wakeLockName {
		t.Fatalf("startup probe lock=%q unlock=%q", read(h.paths.Lock), read(h.paths.Unlock))
	}
	if st := h.e.State(); st.WakeLock != "released" || !h.e.CacheCapable() || h.e.Hello().Wakeup != "ok" {
		t.Fatalf("idle wakelock %+v", st)
	}
	reset()
	a := alarm("wake", t0+minute(1))
	h.install("E1", 1, a)
	if read(h.paths.Lock) != wakeLockName || h.e.State().WakeLock != "held" {
		t.Fatalf("armed occurrence did not hold the lock: %q %s", read(h.paths.Lock), h.e.State().WakeLock)
	}
	reset()
	h.e.Act("", a.occID(), ActionDismiss, "dashboard")
	h.e.Poll()
	if read(h.paths.Unlock) != wakeLockName || h.e.State().WakeLock != "released" {
		t.Fatal("empty cache did not release the lock")
	}

	// Clean shutdown releases the same name.
	b := alarm("later", t0+minute(5))
	h.delta("E1", 2, b.json())
	h.e.Poll()
	reset()
	_ = h.e.Close()
	if read(h.paths.Unlock) != wakeLockName {
		t.Fatal("clean shutdown did not release the lock")
	}

	// A lock the kernel does not list is unavailable.
	h2 := newHarness(t)
	h2.crash()
	h2.paths.Lock = os.DevNull
	h2.open()
	if h2.e.State().WakeLock != "unavailable" || h2.e.CacheCapable() || h2.e.Hello().Wakeup != "alarm_wakeup_unavailable" {
		t.Fatalf("unlisted lock accepted: %+v", h2.e.State())
	}
}
