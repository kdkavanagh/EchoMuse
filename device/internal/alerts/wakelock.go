package alerts

import (
	"fmt"
	"os"
	"strings"
)

// wakeLockName is the one kernel wakelock the alert executor owns (SPEC §16.5).
const wakeLockName = "echomuse_alerts"

// WakeLockPaths names the Android kernel wakelock nodes.
type WakeLockPaths struct {
	Lock   string // /sys/power/wake_lock: write a name to hold it; read lists held locks
	Unlock string // /sys/power/wake_unlock: write a name to release it
}

// DefaultWakeLockPaths are the nodes observed on the Dot (SPEC §14 [D1]).
var DefaultWakeLockPaths = WakeLockPaths{Lock: "/sys/power/wake_lock", Unlock: "/sys/power/wake_unlock"}

// Wakelock status values of WIRE alert.state.
const (
	wakeLockHeld        = "held"
	wakeLockReleased    = "released"
	wakeLockUnavailable = "unavailable"
)

// wakeLock drives the kernel wakelock file protocol. The Executor serializes access.
type wakeLock struct {
	paths       WakeLockPaths
	held        bool
	unavailable bool
}

func newWakeLock(paths WakeLockPaths) *wakeLock {
	if paths.Lock == "" && paths.Unlock == "" {
		paths = DefaultWakeLockPaths
	}
	return &wakeLock{paths: paths}
}

// acquire writes the name to wake_lock and verifies the kernel lists it.
// Writing the same name again re-arms the existing lock, so a restarted
// process reconciles ownership instead of accumulating names.
func (w *wakeLock) acquire() error {
	if err := writeNode(w.paths.Lock, wakeLockName); err != nil {
		w.held, w.unavailable = false, true
		return fmt.Errorf("alerts: acquire wakelock: %w", err)
	}
	listed, err := os.ReadFile(w.paths.Lock)
	if err != nil {
		w.held, w.unavailable = false, true
		return fmt.Errorf("alerts: verify wakelock: %w", err)
	}
	for _, name := range strings.Fields(string(listed)) {
		if name == wakeLockName {
			w.held, w.unavailable = true, false
			return nil
		}
	}
	w.held, w.unavailable = false, true
	return fmt.Errorf("alerts: wakelock %q not listed after acquire", wakeLockName)
}

// release writes the name to wake_unlock. It is issued even when this process
// did not acquire the lock, so a lock left by a crashed predecessor in the
// same boot is released.
func (w *wakeLock) release() error {
	if err := writeNode(w.paths.Unlock, wakeLockName); err != nil {
		w.unavailable = true
		return fmt.Errorf("alerts: release wakelock: %w", err)
	}
	w.held = false
	return nil
}

func (w *wakeLock) status() string {
	switch {
	case w.unavailable:
		return wakeLockUnavailable
	case w.held:
		return wakeLockHeld
	default:
		return wakeLockReleased
	}
}

func writeNode(path, value string) error {
	f, err := os.OpenFile(path, os.O_WRONLY, 0)
	if err != nil {
		return err
	}
	_, err = f.WriteString(value)
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	return err
}
