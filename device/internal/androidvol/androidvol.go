// Package androidvol drives Android's STREAM_MUSIC volume index on the Echo
// Show, the volume authority there (docs/echo-show-port-plan.md decision 8):
// the volume keys, the kiosk and EchoMuse all move the same index, 0–30 on
// Fire OS and 0–15 on LineageOS 18.1.
//
// Fire OS 7.1's `media` command has no volume subcommand and `settings` is a
// JVM per call (about 1 s of CPU), so the firmware calls IAudioService over
// binder with `service call audio`. Transaction codes follow each build's
// IAudioService.aidl order, so they differ per image (Codes):
//
//	                                                   Fire OS (AOSP 7.1)  LineageOS 18.1 (Android 11)
//	setStreamVolume(int, int index, int flags, String)  3                   10
//	getStreamVolume(int streamType) int                  8                   16
//	getStreamMaxVolume(int streamType) int               10                  18
//
// Fire OS's were read from boot-framework.oat's IAudioService$Stub on
// NS65741/8146 (docs/echo-show-crown.md §10.5), LineageOS's from its
// transaction table (§8.5); each verified by round trip. On Android 11 code
// 10 is setStreamVolume, so Fire OS's table must never run there
// (internal/platform requires API 25 for Fire OS).
//
// One `service call` costs about 27 ms wall and 18 ms CPU. Pinned codes are
// only right for their build, so Verify checks them at start (the maximum is
// the table's and a get/set/get round trip agrees) and volume control is
// disabled with the reason when it fails. AudioService lives in
// system_server, which registers it well after audioserver runs at boot, so
// Verify first waits a bounded time for the "audio" binder service to exist.
//
// Key-driven changes are noticed without polling binder every second:
// AudioService persists every index change to the system settings
// (volume_music_speaker in /data/system/users/0/settings_system.xml, plain
// XML on both builds) about 0.5 s after it, so the file's modification time
// is a cheap change trigger. The value itself always comes from
// getStreamVolume, which reflects a key press immediately, so changes this
// process made are told apart from key presses exactly.
package androidvol

import (
	"context"
	"errors"
	"fmt"
	"log"
	"math"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Codes are one build's IAudioService transaction codes and STREAM_MUSIC's
// maximum index there.
type Codes struct {
	Name             string // the build, for messages
	Set, Get, GetMax string
	MaxIndex         int
}

var (
	// FireOS is the Fire OS Show's (NS65741/8146, AOSP 7.1).
	FireOS = Codes{Name: "Fire OS 6 (AOSP 7.1)", Set: "3", Get: "8", GetMax: "10", MaxIndex: 30}
	// Lineage18 is LineageOS 18.1's (Android 11), with AOSP's default 15
	// music steps (no ro.config.media_vol_steps on crown).
	Lineage18 = Codes{Name: "LineageOS 18.1 (Android 11)", Set: "10", Get: "16", GetMax: "18", MaxIndex: 15}
)

const (
	// MaxLevel is the wire volume scale's maximum (volume_set, volume_state).
	MaxLevel = 127

	streamMusic = "3"
	// callingPackage is setStreamVolume's AppOps package; measured to work
	// from uid 0 on both builds.
	callingPackage = "com.android.shell"

	settingsFile = "/data/system/users/0/settings_system.xml"
	pollInterval = time.Second
	// serviceWait bounds how long Verify waits for system_server to
	// register AudioService after a cold boot.
	serviceWait = 90 * time.Second
)

// IndexForLevel maps a wire level 0..127 to the STREAM_MUSIC index
// 0..maxIndex.
func IndexForLevel(level, maxIndex int) int {
	level = max(0, min(MaxLevel, level))
	return int(math.Round(float64(level) * float64(maxIndex) / MaxLevel))
}

// LevelForIndex maps a STREAM_MUSIC index 0..maxIndex to the wire level
// 0..127. LevelForIndex then IndexForLevel returns the index unchanged.
func LevelForIndex(index, maxIndex int) int {
	index = max(0, min(maxIndex, index))
	return int(math.Round(float64(index) * MaxLevel / float64(maxIndex)))
}

// parseReply returns the 32-bit words of a `service call` reply parcel:
//
//	Result: Parcel(00000000 0000000c   '........')
//
// The first word is the binder exception code and must be zero. An
// exception reply carries a message and prints over several lines
// ("Result: Parcel(\n  0x00000000: fffffffd 00000012 ..."), so anything but
// one line is a failure too.
func parseReply(out string) ([]uint32, error) {
	s := strings.TrimSpace(out)
	body, ok := strings.CutPrefix(s, "Result: Parcel(")
	if !ok || strings.Contains(s, "\n") {
		return nil, fmt.Errorf("unexpected reply %q", s)
	}
	hex, _, ok := strings.Cut(body, "'")
	if !ok {
		return nil, fmt.Errorf("unexpected reply %q", s)
	}
	fields := strings.Fields(hex)
	if len(fields) == 0 {
		return nil, fmt.Errorf("empty reply %q", s)
	}
	words := make([]uint32, len(fields))
	for i, f := range fields {
		if len(f) != 8 {
			return nil, fmt.Errorf("unexpected reply %q", s)
		}
		w, err := strconv.ParseUint(f, 16, 32)
		if err != nil {
			return nil, fmt.Errorf("unexpected reply %q", s)
		}
		words[i] = uint32(w)
	}
	if words[0] != 0 {
		return nil, fmt.Errorf("binder exception %#x in %q", words[0], s)
	}
	return words, nil
}

// parseInt reads an int-returning call's reply.
func parseInt(out string) (int, error) {
	w, err := parseReply(out)
	if err != nil {
		return 0, err
	}
	if len(w) != 2 {
		return 0, fmt.Errorf("int reply has %d words: %q", len(w), strings.TrimSpace(out))
	}
	return int(int32(w[1])), nil
}

// parseVoid reads a void call's reply.
func parseVoid(out string) error {
	w, err := parseReply(out)
	if err != nil {
		return err
	}
	if len(w) != 1 {
		return fmt.Errorf("void reply has %d words: %q", len(w), strings.TrimSpace(out))
	}
	return nil
}

// Control is STREAM_MUSIC's index. It is safe for concurrent use.
type Control struct {
	codes Codes
	// call runs `service call audio <args...>`; a seam for the tests.
	call func(args ...string) (string, error)
	// registered runs `service check audio`: whether AudioService exists.
	registered func() bool
	// modTime reads the settings file's modification time.
	modTime func() (time.Time, error)
	// interval paces Watch and the wait for AudioService; serviceWait
	// bounds that wait.
	interval    time.Duration
	serviceWait time.Duration

	mu    sync.Mutex // serializes sets; guards known and gen
	known int        // the index last set or observed
	gen   uint64     // bumped by every set
}

// New returns the Show's control for the build whose codes are given.
func New(codes Codes) *Control {
	return &Control{
		codes: codes,
		call: func(args ...string) (string, error) {
			out, err := exec.Command("service", append([]string{"call", "audio"}, args...)...).CombinedOutput()
			if err != nil {
				return "", fmt.Errorf("service call audio %s: %v (%s)", strings.Join(args, " "), err, strings.TrimSpace(string(out)))
			}
			return string(out), nil
		},
		registered: func() bool {
			out, err := exec.Command("service", "check", "audio").Output()
			return err == nil && isFound(string(out))
		},
		modTime: func() (time.Time, error) {
			fi, err := os.Stat(settingsFile)
			if err != nil {
				return time.Time{}, err
			}
			return fi.ModTime(), nil
		},
		interval:    pollInterval,
		serviceWait: serviceWait,
		known:       -1,
	}
}

// isFound reads `service check audio`: "Service audio: found" or
// "Service audio: not found".
func isFound(out string) bool {
	s := strings.TrimSpace(out)
	return strings.HasSuffix(s, ": found")
}

// MaxIndex is STREAM_MUSIC's maximum index on this build.
func (c *Control) MaxIndex() int { return c.codes.MaxIndex }

// Index reads the current STREAM_MUSIC index.
func (c *Control) Index() (int, error) {
	out, err := c.call(c.codes.Get, "i32", streamMusic)
	if err != nil {
		return 0, err
	}
	i, err := parseInt(out)
	if err != nil {
		return 0, fmt.Errorf("getStreamVolume: %w", err)
	}
	if i < 0 || i > c.codes.MaxIndex {
		return 0, fmt.Errorf("getStreamVolume: index %d outside 0..%d", i, c.codes.MaxIndex)
	}
	return i, nil
}

// SetIndex sets the STREAM_MUSIC index without showing Android's volume UI
// (flags 0). The change is never reported by Watch.
func (c *Control) SetIndex(index int) error {
	index = max(0, min(c.codes.MaxIndex, index))
	c.mu.Lock()
	defer c.mu.Unlock()
	out, err := c.call(c.codes.Set, "i32", streamMusic, "i32", strconv.Itoa(index), "i32", "0", "s16", callingPackage)
	if err == nil {
		err = parseVoid(out)
	}
	// Whatever the outcome, a read in flight may have raced this set.
	c.gen++
	if err != nil {
		return fmt.Errorf("setStreamVolume %d: %w", index, err)
	}
	c.known = index
	return nil
}

// Verify checks the pinned codes against this build, once AudioService is
// registered: getStreamMaxVolume must answer the table's maximum, and
// setting the current index again must read back unchanged. It changes
// nothing audible.
func (c *Control) Verify() error {
	if err := c.waitRegistered(); err != nil {
		return err
	}
	out, err := c.call(c.codes.GetMax, "i32", streamMusic)
	if err != nil {
		return err
	}
	maxIndex, err := parseInt(out)
	if err != nil {
		return fmt.Errorf("getStreamMaxVolume: %w", err)
	}
	if maxIndex != c.codes.MaxIndex {
		return fmt.Errorf("getStreamMaxVolume answered %d, want %d: %s's transaction codes do not match this build",
			maxIndex, c.codes.MaxIndex, c.codes.Name)
	}
	before, err := c.Index()
	if err != nil {
		return err
	}
	if err := c.SetIndex(before); err != nil {
		return err
	}
	after, err := c.Index()
	if err != nil {
		return err
	}
	if after != before {
		return fmt.Errorf("round trip read %d after setting %d", after, before)
	}
	return nil
}

// waitRegistered waits up to serviceWait for AudioService to register.
func (c *Control) waitRegistered() error {
	deadline := time.Now().Add(c.serviceWait)
	logged := false
	for !c.registered() {
		if time.Now().After(deadline) {
			return fmt.Errorf("the audio binder service (AudioService) did not register within %s", c.serviceWait)
		}
		if !logged {
			logged = true
			log.Printf("[androidvol] waiting up to %s for system_server to register AudioService", c.serviceWait)
		}
		time.Sleep(c.interval)
	}
	return nil
}

// Watch reports key-driven (or kiosk-driven) index changes to onChange
// until ctx ends; changes made by SetIndex are not reported. onChange runs
// on Watch's goroutine with no lock held. Without the settings file it
// reads the index every second instead.
func (c *Control) Watch(ctx context.Context, onChange func(index int)) {
	var last time.Time
	pending := false // a change was seen that no clean read has covered yet
	warned := false
	lastErr := ""
	t := time.NewTicker(c.interval)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
		m, err := c.modTime()
		switch {
		case err != nil:
			if !warned {
				warned = true
				log.Printf("[androidvol] %v; reading the index every %s instead", err, c.interval)
			}
			pending = true
		case !m.Equal(last):
			last, pending = m, true
		}
		if !pending {
			continue
		}
		index, changed, err := c.poll()
		if err != nil {
			if !errors.Is(err, errRaced) && err.Error() != lastErr {
				lastErr = err.Error()
				log.Printf("[androidvol] %v", err)
			}
			continue // stay pending: the next tick reads again
		}
		pending, lastErr = false, ""
		if changed {
			onChange(index)
		}
	}
}

// errRaced marks a read a concurrent SetIndex overtook.
var errRaced = errors.New("read raced a set")

// poll reads the index and reports whether it differs from the last index
// set or observed; the first read only records it. A read that a SetIndex
// overtook is discarded: it may predate the set.
func (c *Control) poll() (index int, changed bool, err error) {
	c.mu.Lock()
	gen := c.gen
	c.mu.Unlock()
	index, err = c.Index()
	if err != nil {
		return 0, false, err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if gen != c.gen {
		return 0, false, errRaced
	}
	if index == c.known {
		return index, false, nil
	}
	changed = c.known >= 0
	c.known = index
	return index, changed, nil
}
