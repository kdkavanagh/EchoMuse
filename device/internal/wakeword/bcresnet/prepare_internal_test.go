package bcresnet

import (
	"math"
	"testing"
)

// The RMS floor equals the peak guard and RMS never exceeds the peak, so the
// guard cannot trigger through Prepare; it is checked directly.
func TestPeakGuard(t *testing.T) {
	x := []float32{1e-4, -5e-5}
	normalizePeak(x, 1e-4, 0.8)
	if x[0] != 1e-4 || x[1] != -5e-5 {
		t.Fatalf("window at the guard was rescaled: %v", x)
	}
	y := []float32{2e-4, -1e-4}
	normalizePeak(y, 2e-4, 0.8)
	if math.Abs(float64(y[0])-0.8) > 1e-6 || math.Abs(float64(y[1])+0.4) > 1e-6 {
		t.Fatalf("window above the guard: %v, want [0.8 -0.4]", y)
	}
}
