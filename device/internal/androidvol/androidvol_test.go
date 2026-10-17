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

// Replies as `service call audio` printed them on NS65741/8146 (Fire OS)
// and on lineage-18.1 for crown (Android 11, 2026-10-17).
const (
	replyGet12  = "Result: Parcel(00000000 0000000c   '........')\n"
	replyMax30  = "Result: Parcel(00000000 0000001e   '........')\n"
	replyVoid   = "Result: Parcel(00000000    '....')\n"
	replyBadArg = "Result: Parcel(\n  0x00000000: fffffffd 00000012 00610042 00200064 '....B.a.d. .'\n  0x00000010: 00740073 00650072 'Bad stream type 99')\n"

	replyA11Get5  = "Result: Parcel(00000000 00000005   '........')\n"
	replyA11Max15 = "Result: Parcel(00000000 0000000f   '........')\n"
	replyA11Void  = "Result: Parcel(00000000    '....')\n"
	// getStreamVolume(99) on Android 11: IllegalArgumentException with
	// its stack trace, one hex dump line per 16 bytes (truncated here).
	replyA11BadArg = "Result: Parcel(\n" +
		"  0x00000000: fffffffd 00000012 00610042 00200064 '........B.a.d. .'\n" +
		"  0x00000010: 00740073 00650072 006d0061 00740020 's.t.r.e.a.m. .t.'\n" +
		"  0x00000300: 0000000a                            '....            ')\n"
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
	if n, err := parseInt(replyA11Get5); err != nil || n != 5 {
		t.Errorf("Android 11 get: %d, %v", n, err)
	}
	if n, err := parseInt(replyA11Max15); err != nil || n != 15 {
		t.Errorf("Android 11 max: %d, %v", n, err)
	}
	if err := parseVoid(replyA11Void); err != nil {
		t.Errorf("Android 11 set: %v", err)
	}
	if _, err := parseInt(replyA11BadArg); err == nil {
		t.Error("Android 11 exception parsed as an int")
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

// The wire scale and the index map proportionally (contract formula) on
// both builds' ranges, and an index survives the round trip through a
// level, so a reported key change re-applied later as a level lands on the
// same index.
func TestLevelIndexMapping(t *testing.T) {
	for _, tc := range []struct{ level, max, index int }{
		{0, 30, 0}, {127, 30, 30}, {85, 30, 20}, {64, 30, 15}, {2, 30, 0}, {3, 30, 1}, {-5, 30, 0}, {200, 30, 30},
		{0, 15, 0}, {127, 15, 15}, {85, 15, 10}, {64, 15, 8}, {4, 15, 0}, {5, 15, 1}, {200, 15, 15},
	} {
		if got := IndexForLevel(tc.level, tc.max); got != tc.index {
			t.Errorf("IndexForLevel(%d, %d) = %d, want %d", tc.level, tc.max, got, tc.index)
		}
	}
	for _, tc := range []struct{ index, max, level int }{
		{0, 30, 0}, {30, 30, 127}, {12, 30, 51}, {20, 30, 85}, {1, 30, 4}, {31, 30, 127},
		{0, 15, 0}, {15, 15, 127}, {5, 15, 42}, {1, 15, 8}, {16, 15, 127},
	} {
		if got := LevelForIndex(tc.index, tc.max); got != tc.level {
			t.Errorf("LevelForIndex(%d, %d) = %d, want %d", tc.index, tc.max, got, tc.level)
		}
	}
	for _, codes := range []Codes{FireOS, Lineage18} {
		for i := 0; i <= codes.MaxIndex; i++ {
			if got := IndexForLevel(LevelForIndex(i, codes.MaxIndex), codes.MaxIndex); got != i {
				t.Errorf("%s: index %d round trips to %d", codes.Name, i, got)
			}
		}
	}
}

// fakeAudio is AudioService's STREAM_MUSIC as `service` sees it.
type fakeAudio struct {
	mu         sync.Mutex
	codes      Codes // the build; FireOS when zero
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
	if f.codes.Name == "" {
		f.codes = FireOS
	}
	codes := f.codes
	return &Control{
		codes: codes,
		call: func(args ...string) (string, error) {
			f.mu.Lock()
			f.calls = append(f.calls, strings.Join(args, " "))
			switch args[0] {
			case codes.Get:
				index, hook := f.index, f.onGet
				f.mu.Unlock()
				if hook != nil {
					hook()
				}
				return fmt.Sprintf("Result: Parcel(00000000 %08x   '........')\n", index), nil
			case codes.GetMax:
				defer f.mu.Unlock()
				return fmt.Sprintf("Result: Parcel(00000000 %08x   '........')\n", f.max), nil
			case codes.Set:
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
	for _, tc := range []struct {
		codes      Codes
		max        int
		set, clamp string
	}{
		{FireOS, 30, "3 i32 3 i32 20 i32 0 s16 com.android.shell", "3 i32 3 i32 30 i32 0 s16 com.android.shell"},
		{Lineage18, 15, "10 i32 3 i32 12 i32 0 s16 com.android.shell", "10 i32 3 i32 15 i32 0 s16 com.android.shell"},
	} {
		f := &fakeAudio{codes: tc.codes, index: 5, max: tc.max}
		c := f.control()
		want := 20
		if tc.max == 15 {
			want = 12
		}
		if err := c.SetIndex(want); err != nil || f.index != want {
			t.Fatalf("%s set: %v, index %d", tc.codes.Name, err, f.index)
		}
		if err := c.SetIndex(99); err != nil || f.index != tc.max {
			t.Fatalf("%s set clamps: %v, index %d", tc.codes.Name, err, f.index)
		}
		if f.calls[0] != tc.set || f.calls[1] != tc.clamp {
			t.Errorf("%s set calls %q", tc.codes.Name, f.calls)
		}
		f.setErr = true
		if err := c.SetIndex(5); err == nil {
			t.Fatalf("%s: a binder exception must fail the set", tc.codes.Name)
		}
	}
}

// Verify passes on each build's own table without moving the index, and
// fails when the maximum, the set or the read-back disagrees.
func TestVerify(t *testing.T) {
	for _, tc := range []struct {
		codes   Codes
		max     int
		maxCall string
	}{{FireOS, 30, "10 i32 3"}, {Lineage18, 15, "18 i32 3"}} {
		f := &fakeAudio{codes: tc.codes, index: 12, max: tc.max}
		if err := f.control().Verify(); err != nil {
			t.Fatalf("%s: %v", tc.codes.Name, err)
		}
		if f.index != 12 {
			t.Fatalf("%s: verify changed the index to %d", tc.codes.Name, f.index)
		}
		if got := f.calls[0]; got != tc.maxCall {
			t.Errorf("%s: max call %q", tc.codes.Name, got)
		}
	}
	for name, bad := range map[string]*fakeAudio{
		"Fire OS table on a 15-step build":   {index: 12, max: 15},
		"LineageOS table on a 30-step build": {codes: Lineage18, index: 12, max: 30},
		"failing set":                        {index: 12, max: 30, setErr: true},
		"read-back moved":                    {index: 12, max: 30, setOffset: 1},
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
