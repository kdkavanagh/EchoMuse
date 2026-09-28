// Package bcresnettest loads the deployed-graph fixture written by
// bcresnet/testdata/gen_fixture.py and rebuilds its input windows.
package bcresnettest

import (
	"encoding/json"
	"os"
	"path/filepath"
	"runtime"

	"github.com/wilbowes/EchoMuse/internal/wakeword/bcresnet"
)

// Fixture is deployed_fixture.json.
type Fixture struct {
	GraphSHA256   string   `json:"graph_sha256"`
	SidecarSHA256 string   `json:"sidecar_sha256"`
	ProbeStride   int      `json:"probe_stride"`
	Windows       []Window `json:"windows"`
}

// Window is one deterministic input and what the deployed graph made of it.
// Preparation fields are present only when Scored.
type Window struct {
	Name        string    `json:"name"`
	Seed        int64     `json:"seed"`
	Modulus     int64     `json:"modulus"`
	Offset      int64     `json:"offset"`
	LoudFrom    int       `json:"loud_from"`
	LoudModulus int64     `json:"loud_modulus"`
	LoudOffset  int64     `json:"loud_offset"`
	RMS         float64   `json:"rms"`
	Scored      bool      `json:"scored"`
	Peak        float64   `json:"peak"`
	Probe       []float64 `json:"probe"`
	Sum         float64   `json:"sum"`
	SumSq       float64   `json:"sum_sq"`
	Logits      []float64 `json:"logits"`
	Prob        float64   `json:"prob"`
}

// Dir is the bcresnet testdata directory.
func Dir() string {
	_, file, _, _ := runtime.Caller(0)
	return filepath.Join(filepath.Dir(file), "..", "testdata")
}

// Load reads the fixture from Dir.
func Load() (*Fixture, error) {
	raw, err := os.ReadFile(filepath.Join(Dir(), "deployed_fixture.json"))
	if err != nil {
		return nil, err
	}
	var f Fixture
	if err := json.Unmarshal(raw, &f); err != nil {
		return nil, err
	}
	return &f, nil
}

// PCM rebuilds the window's int16 samples with the generator's 31-bit LCG.
func (w Window) PCM() []int16 {
	out := make([]int16, bcresnet.WindowSamples)
	x := w.Seed
	for i := range out {
		x = (1103515245*x + 12345) & 0x7FFFFFFF
		if i < w.LoudFrom {
			out[i] = int16((x>>8)%w.Modulus - w.Offset)
		} else {
			out[i] = int16((x>>8)%w.LoudModulus - w.LoudOffset)
		}
	}
	return out
}
