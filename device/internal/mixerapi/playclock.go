package mixerapi

// playPipelineNs is the mixer taking a block → its sound in micAsr capture
// stamps, a lower bound for internal/audio/playclock. Player reads, at every
// handoff, how many released blocks are still queued ahead of the new one
// (DataTrans::GetNumBlkReady); the head one apparently counts as ready until
// played, which is why playclock does not count it.
//
// Measured on device G090LF0965260F1J (docs/fireos6-port.md §6) with a 1 kHz
// burst at the start of a render write: stamped this way, capture lagged the
// reference by +59 to +99 ms behind 1 or 2 queued blocks and +63 to +77 ms
// into an empty queue. Stamped with all `ahead` blocks it lagged by −0.3 to
// +48 ms behind 2, which is why the head block is not counted.
const playPipelineNs = 20_000_000
