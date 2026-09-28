package alerts

import (
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"math"
	"os"
	"sync"
)

const (
	// sampleRate is the alert source and asset rate (SPEC §16.5): 48 kHz mono PCM16.
	sampleRate  = 48000
	maxPCMBytes = 960000 // 10 s

	// Built-in fallback cadence (SPEC §16.5), in samples at 48 kHz.
	fallbackBursts    = 4
	fallbackBurst     = 9600  // 200 ms
	fallbackInnerGap  = 4800  // 100 ms
	fallbackEdge      = 480   // 10 ms edge fades
	fallbackLoopGap   = 86400 // 1,800 ms after the fourth burst
	fallbackHz        = 880
	fallbackAmplitude = 16384 // -6 dBFS; the ramp and DAC set loudness
)

var (
	fallbackOnce sync.Once
	fallbackPCM  []int16
)

// FallbackPCM returns one period of the built-in fallback tone: four 200 ms
// 880 Hz sine bursts separated by 100 ms of silence, each with 10 ms linear
// edge fades, then the 1,800 ms inter-loop gap (SPEC §16.5). The slice is
// shared and must not be modified.
func FallbackPCM() []int16 {
	fallbackOnce.Do(func() {
		n := fallbackBursts*fallbackBurst + (fallbackBursts-1)*fallbackInnerGap + fallbackLoopGap
		fallbackPCM = make([]int16, n)
		pos := 0
		for b := 0; b < fallbackBursts; b++ {
			for i := 0; i < fallbackBurst; i++ {
				g := 1.0
				if i < fallbackEdge {
					g = float64(i) / fallbackEdge
				}
				if tail := fallbackBurst - 1 - i; tail < fallbackEdge {
					g = math.Min(g, float64(tail)/fallbackEdge)
				}
				phase := 2 * math.Pi * fallbackHz * float64(i) / sampleRate
				fallbackPCM[pos+i] = int16(math.Round(math.Sin(phase) * fallbackAmplitude * g))
			}
			pos += fallbackBurst + fallbackInnerGap
		}
	})
	return fallbackPCM
}

// readAsset loads an alert asset: SHA-256 must equal its name, and the WAV must
// be mono PCM16 at 48 kHz with 1..960,000 data bytes (SPEC §16.5).
func readAsset(path, sha string) ([]int16, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	sum := sha256.Sum256(b)
	if hex.EncodeToString(sum[:]) != sha {
		return nil, fmt.Errorf("alerts: asset %s: hash mismatch", sha)
	}
	if len(b) < 12 || string(b[0:4]) != "RIFF" || string(b[8:12]) != "WAVE" {
		return nil, fmt.Errorf("alerts: asset %s: not RIFF/WAVE", sha)
	}
	var fmtOK bool
	var data []byte
	for off := 12; off+8 <= len(b); {
		id := string(b[off : off+4])
		n := int(binary.LittleEndian.Uint32(b[off+4:]))
		off += 8
		if n > len(b)-off {
			return nil, fmt.Errorf("alerts: asset %s: truncated %q chunk", sha, id)
		}
		switch id {
		case "fmt ":
			if n < 16 {
				return nil, fmt.Errorf("alerts: asset %s: short fmt chunk", sha)
			}
			fmtOK = binary.LittleEndian.Uint16(b[off:]) == 1 && // PCM
				binary.LittleEndian.Uint16(b[off+2:]) == 1 && // mono
				binary.LittleEndian.Uint32(b[off+4:]) == sampleRate &&
				binary.LittleEndian.Uint16(b[off+14:]) == 16
		case "data":
			data = b[off : off+n]
		}
		off += n + n&1
	}
	if !fmtOK {
		return nil, fmt.Errorf("alerts: asset %s: not mono PCM16 at 48 kHz", sha)
	}
	if len(data) == 0 || len(data)%2 != 0 || len(data) > maxPCMBytes {
		return nil, fmt.Errorf("alerts: asset %s: %d data bytes", sha, len(data))
	}
	pcm := make([]int16, len(data)/2)
	for i := range pcm {
		pcm[i] = int16(binary.LittleEndian.Uint16(data[2*i:]))
	}
	return pcm, nil
}

// PreviewPCM returns a validated local alert asset for render.start
// alert_preview and whether it was installed; an absent or invalid asset
// plays the built-in fallback immediately (§16.5). The slice is shared and
// must not be modified.
func (e *Executor) PreviewPCM(sound string) (pcm []int16, installed bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if sound != SoundFallback && validSHA256(sound) {
		e.cacheSoundLocked(sound)
		if pcm := e.sounds[sound]; pcm != nil {
			return pcm, true
		}
	}
	return FallbackPCM(), false
}
