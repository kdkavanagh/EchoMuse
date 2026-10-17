package androidvol

import (
	"context"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

// Replies as `service call audio` printed them on NS65741/8146.
const (
	replyGet12  = "Result: Parcel(00000000 0000000c   '........')\n"
	replyMax30  = "Result: Parcel(00000000 0000001e   '........')\n"
	replyVoid   = "Result: Parcel(00000000    '....')\n"
	replyBadArg = "Result: Parcel(\n  0x00000000: fffffffd 00000012 00610042 00200064 '....B.a.d. .'\n  0x00000010: 00740073 00650072 'Bad stream type 99')\n"
)

func TestParseReplies(t *testing.T) {
	if n, err := parseInt(replyGet12); err != nil || n != 12 {
		t.Errorf("get: %d, %v", n, err)
	}
	if n, err := parseInt(replyMax30); err != nil || n != 30 {
		t.Errorf("max: %d, %v", n, err)
	}
	if err := parseVoid(replyVoid); err != nil {
		t.Errorf("set: %v", err)
	}
	for name, bad := range map[string]string{
		"exception":        replyBadArg,
		"single-line exc":  "Result: Parcel(fffffffd    '....')",
		"void as int":      replyVoid,
		"no parcel":        "service: Service audio does not exist",
		"empty":            "",
		"short word":       "Result: Parcel(0000 0000000c '..')",
		"no quote":         "Result: Parcel(00000000 0000000c",
		"non-hex":          "Result: Parcel(00000000 0000000z '........')",
		"extra int words":  "Result: Parcel(00000000 0000000c 00000001 '............')",
		"blank parcel":     "Result: Parcel(   '')",
		"leading garbage":  "x Result: Parcel(00000000 0000000c '........')",
		"exception as int": "Result: Parcel(ffffffff 0000000c '........')",
	} {
		if _, err := parseInt(bad); err == nil {
			t.Errorf("%s: parsed as an int: %q", name, bad)
		}
	}
	if err := parseVoid(replyGet12); err == nil {
		t.Error("an int reply parsed as void")
	}
	for out, want := range map[string]bool{"Service audio: found\n": true, "Service audio: not found\n": false, "": false} {
		if got := isFound(out); got != want {
			t.Errorf("isFound(%q) = %v", out, got)
		}
	}
}

// The wire scale and the index map proportionally (contract formula), and
// an index survives the round trip through a level, so a reported key
// change re-applied later as a level lands on the same index.
func TestLevelIndexMapping(t *testing.T) {
	for _, tc := range []struct{ level, index int }{{0, 0}, {127, 30}, {85, 20}, {64, 15}, {2, 0}, {3, 1}, {-5, 0}, {200, 30}} {
		if got := IndexForLevel(tc.level); got != tc.index {
			t.Errorf("IndexForLevel(%d) = %d, want %d", tc.level, got, tc.index)
		}
	}
	for _, tc := range []struct{ index, level int }{{0, 0}, {30, 127}, {12, 51}, {20, 85}, {1, 4}, {31, 127}} {
		if got := LevelForIndex(tc.index); got != tc.level {
			t.Errorf("LevelForIndex(%d) = %d, want %d", tc.index, got, tc.level)
		}
	}
	for i := 0; i <= MaxIndex; i++ {
		if got := IndexForLevel(LevelForIndex(i)); got != i {
			t.Errorf("index %d round trips to %d", i, got)
		}
	}
}

// fakeAudio is AudioService's STREAM_MUSIC as `service` sees it.
type fakeAudio struct {
	mu         sync.Mutex
	index      int
	max        int
	calls      []string
	setErr     bool
	setOffset  int    // a set lands this far from what was asked
	onGet      func() // runs inside a get after its answer is fixed
	mtime      time.Time
	statErr    error
	unregCheck int // `service check audio` answers "not found" this many times
}

func (f *fakeAudio) control() *Control {
	return &Control{
		call: func(args ...string) (string, error) {
			f.mu.Lock()
			f.calls = append(f.calls, strings.Join(args, " "))
			switch args[0] {
			case codeGet:
				index, hook := f.index, f.onGet
				f.mu.Unlock()
				if hook != nil {
					hook()
				}
				return fmt.Sprintf("Result: Parcel(00000000 %08x   '........')\n", index), nil
			case codeGetMax:
				defer f.mu.Unlock()
				return fmt.Sprintf("Result: Parcel(00000000 %08x   '........')\n", f.max), nil
			case codeSet:
				defer f.mu.Unlock()
				if f.setErr {
					return replyBadArg, nil
				}
				n, _ := strconv.Atoi(args[4])
				f.index = n + f.setOffset
				return replyVoid, nil
			}
			f.mu.Unlock()
			return "", errors.New("unknown code")
		},
		registered: func() bool {
			f.mu.Lock()
			defer f.mu.Unlock()
			if f.unregCheck > 0 {
				f.unregCheck--
				return false
			}
			return true
		},
		modTime: func() (time.Time, error) {
			f.mu.Lock()
			defer f.mu.Unlock()
			return f.mtime, f.statErr
		},
		interval:    time.Millisecond,
		serviceWait: 50 * time.Millisecond,
		known:       -1,
	}
}

func (f *fakeAudio) set(index int) {
	f.mu.Lock()
	f.index = index
	f.mu.Unlock()
}

// persist is AudioService writing the settings file.
func (f *fakeAudio) persist() {
	f.mu.Lock()
	f.mtime = f.mtime.Add(time.Second)
	f.mu.Unlock()
}

// key is a volume-key press: the index moves at once, the file follows.
func (f *fakeAudio) key(index int) {
	f.set(index)
	f.persist()
}

func TestSetIndexCallShape(t *testing.T) {
	f := &fakeAudio{index: 12, max: 30}
	c := f.control()
	if err := c.SetIndex(20); err != nil || f.index != 20 {
		t.Fatalf("set: %v, index %d", err, f.index)
	}
	if got := f.calls[0]; got != "3 i32 3 i32 20 i32 0 s16 com.android.shell" {
		t.Errorf("set call %q", got)
	}
	if err := c.SetIndex(99); err != nil || f.index != 30 {
		t.Fatalf("set clamps: %v, index %d", err, f.index)
	}
	f.setErr = true
	if err := c.SetIndex(5); err == nil {
		t.Fatal("a binder exception must fail the set")
	}
}

// Verify passes on the pinned build without moving the index, and fails
// when the maximum, the set or the read-back disagrees.
func TestVerify(t *testing.T) {
	f := &fakeAudio{index: 12, max: 30}
	if err := f.control().Verify(); err != nil {
		t.Fatalf("pinned build: %v", err)
	}
	if f.index != 12 {
		t.Fatalf("verify changed the index to %d", f.index)
	}
	if got := f.calls[0]; got != "10 i32 3" {
		t.Errorf("max call %q", got)
	}
	for name, bad := range map[string]*fakeAudio{
		"wrong maximum":   {index: 12, max: 15},
		"failing set":     {index: 12, max: 30, setErr: true},
		"read-back moved": {index: 12, max: 30, setOffset: 1},
	} {
		if err := bad.control().Verify(); err == nil {
			t.Errorf("%s accepted", name)
		}
	}
}

// Verify waits for system_server to register AudioService after a cold
// boot, and gives up after the bound without calling it.
func TestVerifyWaitsForAudioService(t *testing.T) {
	f := &fakeAudio{index: 12, max: 30, unregCheck: 5}
	if err := f.control().Verify(); err != nil {
		t.Fatalf("late registration: %v", err)
	}
	f = &fakeAudio{index: 12, max: 30, unregCheck: 1 << 20}
	if err := f.control().Verify(); err == nil || len(f.calls) != 0 {
		t.Fatalf("never registered: %v, calls %v", err, f.calls)
	}
}

// Key presses are reported once each; our own sets (an alert override and
// its restore) are not, though they move the index and the file too.
func TestWatchReportsKeysNotOwnSets(t *testing.T) {
	f := &fakeAudio{index: 12, max: 30, mtime: time.Unix(1000, 0)}
	c := f.control()
	got := make(chan int, 8)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go c.Watch(ctx, func(i int) { got <- i })

	expect := func(want int) {
		t.Helper()
		select {
		case i := <-got:
			if i != want {
				t.Fatalf("reported %d, want %d", i, want)
			}
		case <-time.After(2 * time.Second):
			t.Fatalf("no report of %d", want)
		}
	}
	quiet := func() {
		t.Helper()
		select {
		case i := <-got:
			t.Fatalf("unexpected report %d", i)
		case <-time.After(50 * time.Millisecond):
		}
	}

	quiet() // the first read only records the index
	f.key(13)
	expect(13)
	for _, i := range []int{25, 13} { // alert override, then restore
		if err := c.SetIndex(i); err != nil {
			t.Fatal(err)
		}
		f.persist()
		quiet()
	}
	f.key(14)
	expect(14)
	f.persist() // an unrelated setting persisted
	quiet()
	f.set(15) // a change the file has not followed yet
	quiet()
	f.persist()
	expect(15)
}

// A set landing while a read is in flight discards that read, which may
// predate the set, so the old index is never reported as a key press.
func TestPollDiscardsReadsRacingASet(t *testing.T) {
	f := &fakeAudio{index: 12, max: 30}
	c := f.control()
	if err := c.SetIndex(12); err != nil {
		t.Fatal(err)
	}
	f.onGet = func() {
		f.mu.Lock()
		f.onGet = nil
		f.mu.Unlock()
		if err := c.SetIndex(20); err != nil {
			t.Error(err)
		}
	}
	if _, _, err := c.poll(); !errors.Is(err, errRaced) {
		t.Fatalf("raced read: %v", err)
	}
	if i, changed, err := c.poll(); err != nil || changed || i != 20 {
		t.Fatalf("next read: %d changed=%v err=%v", i, changed, err)
	}
}

// Without the settings file the index is read every tick.
func TestWatchWithoutSettingsFile(t *testing.T) {
	f := &fakeAudio{index: 12, max: 30, statErr: errors.New("no file")}
	c := f.control()
	got := make(chan int, 1)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go c.Watch(ctx, func(i int) { got <- i })
	time.Sleep(20 * time.Millisecond)
	f.set(7)
	select {
	case i := <-got:
		if i != 7 {
			t.Fatalf("reported %d", i)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("no report without the settings file")
	}
}
