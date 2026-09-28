// Package mic is the capture boundary: one fully processed mono channel from
// the native AFE (SPEC §4.1), 16 kHz S16, in 80 ms blocks stamped with the
// CLOCK_MONOTONIC time at which each block completed.
package mic

// Block is one completed capture period.
type Block struct {
	PCM    []int16 // 16 kHz mono; borrowed until the next Read
	MonoNs int64   // CLOCK_MONOTONIC ns when the period completed
}

// Microphone delivers capture blocks to its single consumer.
type Microphone interface {
	// Read blocks for the next completed period. It returns an error once
	// the capture stream has ended.
	Read() (Block, error)
	// Drops counts periods lost because Read fell behind; the completion
	// stamps of the blocks that follow expose the gap.
	Drops() uint64
	Close()
}
