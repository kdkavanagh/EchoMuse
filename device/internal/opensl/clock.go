//go:build server

package opensl

import "golang.org/x/sys/unix"

// MonoNow is the current CLOCK_MONOTONIC time in ns, the device's audio
// timing clock (SPEC §4.3).
func MonoNow() int64 {
	var ts unix.Timespec
	if err := unix.ClockGettime(unix.CLOCK_MONOTONIC, &ts); err != nil {
		panic("opensl: clock_gettime(CLOCK_MONOTONIC): " + err.Error())
	}
	return ts.Nano()
}
