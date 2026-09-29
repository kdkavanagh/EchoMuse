// Package monoclock reads CLOCK_MONOTONIC in nanoseconds: the clock of every
// device mono_ns field, EMA1 timestamp and audio timing anchor (SPEC §4.3).
package monoclock

import "golang.org/x/sys/unix"

// Now is the current CLOCK_MONOTONIC time in ns. It panics if the clock
// cannot be read, which Linux never reports for CLOCK_MONOTONIC.
func Now() int64 {
	var ts unix.Timespec
	if err := unix.ClockGettime(unix.CLOCK_MONOTONIC, &ts); err != nil {
		panic("monoclock: clock_gettime(CLOCK_MONOTONIC): " + err.Error())
	}
	return ts.Nano()
}
