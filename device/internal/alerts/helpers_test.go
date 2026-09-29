package alerts

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/wilbowes/EchoMuse/internal/uuid"
)

type fakeClock struct{ ns int64 }

func (c *fakeClock) MonoNowNs() int64        { return c.ns }
func (c *fakeClock) advance(d time.Duration) { c.ns += int64(d) }

type recorder struct {
	mu     sync.Mutex
	states []AlertState
	ended  []RingEnded
	ops    []LocalOperation
	focus  []AlertFocus
}

func (r *recorder) AlertState(s AlertState) {
	r.mu.Lock()
	r.states = append(r.states, s)
	r.mu.Unlock()
}
func (r *recorder) RingEnded(x RingEnded) { r.mu.Lock(); r.ended = append(r.ended, x); r.mu.Unlock() }
func (r *recorder) LocalOperation(o LocalOperation) {
	r.mu.Lock()
	r.ops = append(r.ops, o)
	r.mu.Unlock()
}
func (r *recorder) AlertFocus(f AlertFocus) { r.mu.Lock(); r.focus = append(r.focus, f); r.mu.Unlock() }

const (
	bootA = "6f1c7b2e-0d51-4c3a-9a55-0b4f6d0e1a01"
	bootB = "6f1c7b2e-0d51-4c3a-9a55-0b4f6d0e1a02"
	// tz is the calendar's UTC offset in the fixtures (-05:00).
	tzOffset = -5 * 3600
)

// harness is an executor over a temporary store with a fake monotonic clock
// whose true UTC is mono/1e6 + offsetMs.
type harness struct {
	t        *testing.T
	root     string
	boot     string
	clk      *fakeClock
	offsetMs int64
	rec      *recorder
	paths    WakeLockPaths
	e        *Executor
}

// t0 is the UTC instant (2026-09-23T11:00:00Z) the harness clock starts at.
var t0 = time.Date(2026, 9, 23, 11, 0, 0, 0, time.UTC).UnixMilli()

func newHarness(t *testing.T) *harness {
	t.Helper()
	dir := t.TempDir()
	paths := WakeLockPaths{Lock: filepath.Join(dir, "wake_lock"), Unlock: filepath.Join(dir, "wake_unlock")}
	for _, p := range []string{paths.Lock, paths.Unlock} {
		if err := os.WriteFile(p, nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	clk := &fakeClock{ns: int64(1000 * time.Second)}
	h := &harness{t: t, root: filepath.Join(dir, "alerts"), boot: bootA, clk: clk,
		offsetMs: t0 - clk.ns/int64(time.Millisecond), paths: paths}
	h.open()
	return h
}

func (h *harness) open() {
	h.t.Helper()
	h.rec = &recorder{}
	e, err := NewExecutor(Config{Root: h.root, BootID: h.boot, Clock: h.clk, WakeLock: h.paths, Events: h.rec})
	if err != nil {
		h.t.Fatal(err)
	}
	h.e = e
	h.t.Cleanup(func() { _ = e.Close() })
}

// crash abandons the executor without a clean shutdown.
func (h *harness) crash() {
	h.e.mu.Lock()
	h.e.closed = true
	_ = h.e.st.close()
	h.e.mu.Unlock()
}

// restart simulates a process crash and restart within the same boot.
func (h *harness) restart() {
	h.crash()
	h.open()
}

// reboot simulates a device reboot: a new boot ID and a monotonic clock
// restarted near zero; true UTC keeps advancing by elapsed.
func (h *harness) reboot(elapsed time.Duration) {
	h.crash()
	utc := h.utcNow() + elapsed.Milliseconds()
	h.boot = bootB
	h.clk.ns = int64(5 * time.Second)
	h.offsetMs = utc - h.clk.ns/int64(time.Millisecond)
	h.open()
}

func (h *harness) utcNow() int64 { return h.clk.ns/int64(time.Millisecond) + h.offsetMs }

// trust runs the WIRE clock.request/clock.reply exchange until UTC is trusted.
func (h *harness) trust() {
	h.t.Helper()
	for i := 0; i < 4 && !h.e.ClockInfo().Trusted; i++ {
		req, ok := h.e.ClockRequestIfDue()
		if !ok {
			h.clk.advance(time.Second)
			req, ok = h.e.ClockRequestIfDue()
			if !ok {
				h.t.Fatal("clock request not due after 1 s")
			}
		}
		sent := h.clk.ns
		h.clk.advance(40 * time.Millisecond)
		mid := (sent + h.clk.ns) / 2
		utc := mid/int64(time.Millisecond) + h.offsetMs
		if err := h.e.ApplyClockReply(ClockReply{Nonce: req.Nonce, UTCMs: fmt.Sprint(utc)}); err != nil {
			h.t.Fatal(err)
		}
	}
	if !h.e.ClockInfo().Trusted {
		h.t.Fatal("clock not trusted")
	}
}

// advanceTo moves the clock so that true UTC equals utcMs, then polls.
func (h *harness) advanceTo(utcMs int64) {
	h.clk.advance(time.Duration(utcMs-h.utcNow()) * time.Millisecond)
	h.e.Poll()
}

func (h *harness) active() *ActiveState { return h.e.State().Active }

func (h *harness) activeID() string {
	if a := h.active(); a != nil {
		return a.ID
	}
	return ""
}

// occSpec describes a delivered occurrence; the JSON is written the way
// Python's json module emits it.
type occSpec struct {
	id        string // default: UUIDv5(schedule, due_local)
	sched     string
	dueUTC    int64
	dueLocal  string // default: dueUTC at -05:00
	label     string
	maxRingMs int64
	rampMs    int64
	loopGapMs int64
	snoozeMs  int64
	sound     string
	volume    string // JSON literal
	revision  int
}

func alarm(name string, dueUTC int64) occSpec {
	return occSpec{sched: uuid.V5(uuid.NamespaceURL, name).String(), dueUTC: dueUTC, label: name,
		maxRingMs: 600000, rampMs: 20000, loopGapMs: 2000, snoozeMs: 540000,
		sound: SoundFallback, volume: "null", revision: 1}
}

func localISO(utcMs int64) string {
	return time.UnixMilli(utcMs).In(time.FixedZone("", tzOffset)).Format(isoSeconds)
}

func (s occSpec) key() string {
	if s.dueLocal != "" {
		return s.dueLocal
	}
	return localISO(s.dueUTC)
}

func (s occSpec) occID() string {
	if s.id != "" {
		return s.id
	}
	id, err := OccurrenceID(s.sched, s.key())
	if err != nil {
		panic(err)
	}
	return id
}

func (s occSpec) json() string {
	return fmt.Sprintf(`{"due_local":%q,"due_utc_ms":"%d","kind":"alarm","label":%q,"loop_gap_ms":%d,`+
		`"max_ring_ms":%d,"occurrence_id":%q,"ramp_ms":%d,"revision":%d,"schedule_id":%q,`+
		`"snooze_ms":%d,"sound":%q,"volume":%s}`,
		s.key(), s.dueUTC, s.label, s.loopGapMs, s.maxRingMs, s.occID(), s.rampMs, s.revision,
		s.sched, s.snoozeMs, s.sound, s.volume)
}

func tombJSON(id string, rev int, reason string) string {
	return fmt.Sprintf(`{"occurrence_id":%q,"revision":%d,"tombstone":%q}`, id, rev, reason)
}

// snapshotPages builds WIRE alert.snapshot pages over objs (raw JSON), sorted
// by the caller, with the digest over their canonical array.
func snapshotPages(t *testing.T, epoch string, high int, objs []string) [][]byte {
	t.Helper()
	canon, err := CanonicalJSON([]byte("[" + strings.Join(objs, ",") + "]"))
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(canon)
	return pagesWithDigest(epoch, high, objs, hex.EncodeToString(sum[:]))
}

func pagesWithDigest(epoch string, high int, objs []string, digest string) [][]byte {
	count := (len(objs) + maxSnapshotPageObjects - 1) / maxSnapshotPageObjects
	if count == 0 {
		count = 1
	}
	var pages [][]byte
	for i := 0; i < count; i++ {
		lo := i * maxSnapshotPageObjects
		hi := min(lo+maxSnapshotPageObjects, len(objs))
		pages = append(pages, []byte(fmt.Sprintf(
			`{"delivery_epoch":%q,"high_water_mark":%d,"page_index":%d,"page_count":%d,"sha256":%q,"objects":[%s]}`,
			epoch, high, i, count, digest, strings.Join(objs[lo:hi], ","))))
	}
	return pages
}

// install delivers a snapshot of specs sorted by due then ID and requires success.
func (h *harness) install(epoch string, high int, specs ...occSpec) {
	h.t.Helper()
	sort.Slice(specs, func(i, j int) bool {
		if specs[i].dueUTC != specs[j].dueUTC {
			return specs[i].dueUTC < specs[j].dueUTC
		}
		return specs[i].occID() < specs[j].occID()
	})
	objs := make([]string, len(specs))
	for i, s := range specs {
		objs[i] = s.json()
	}
	var ack AlertAck
	for _, p := range snapshotPages(h.t, epoch, high, objs) {
		var done bool
		ack, done = h.e.ApplySnapshotPage(p)
		_ = done
	}
	if ack.NeedSnapshot || !ack.Durable || ack.AppliedThrough != uint64(high) {
		h.t.Fatalf("snapshot not installed: %+v", ack)
	}
	h.e.Poll()
}

func (h *harness) delta(epoch string, seq int, objs ...string) AlertAck {
	return h.e.ApplyDelta([]byte(fmt.Sprintf(`{"delivery_epoch":%q,"sequence":%d,"objects":[%s]}`,
		epoch, seq, strings.Join(objs, ","))))
}

// fill renders n samples.
func (h *harness) fill(n int) ([]int16, bool) {
	out := make([]int16, n)
	active := false
	for off := 0; off < n; off += 480 {
		end := min(off+480, n)
		if h.e.Fill(out[off:end]) {
			active = true
		}
	}
	return out, active
}

func containsID(ids []string, id string) bool {
	for _, x := range ids {
		if x == id {
			return true
		}
	}
	return false
}

// writeWAV installs a mono PCM16 48 kHz asset and returns its SHA-256.
func writeWAV(t *testing.T, root string, pcm []int16) string {
	t.Helper()
	data := make([]byte, 0, 44+2*len(pcm))
	le32 := func(v uint32) { data = append(data, byte(v), byte(v>>8), byte(v>>16), byte(v>>24)) }
	le16 := func(v uint16) { data = append(data, byte(v), byte(v>>8)) }
	data = append(data, "RIFF"...)
	le32(uint32(36 + 2*len(pcm)))
	data = append(data, "WAVEfmt "...)
	le32(16)
	le16(1)
	le16(1)
	le32(sampleRate)
	le32(sampleRate * 2)
	le16(2)
	le16(16)
	data = append(data, "data"...)
	le32(uint32(2 * len(pcm)))
	for _, s := range pcm {
		le16(uint16(s))
	}
	sum := sha256.Sum256(data)
	sha := hex.EncodeToString(sum[:])
	dir := filepath.Join(root, "assets")
	if err := os.MkdirAll(dir, 0o750); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, sha+".wav"), data, 0o640); err != nil {
		t.Fatal(err)
	}
	return sha
}
