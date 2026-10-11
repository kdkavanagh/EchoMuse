package timesync

import (
	"testing"
	"time"
)

// Captured from the Fire OS 6 Dot (2026-10-07): before and after
// dhcpcd-wlan0 leased.
const (
	routesNone   = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT                                                       \n"
	routesLeased = routesNone +
		"wlan0\t00000000\t0103A8C0\t0003\t0\t0\t307\t00000000\t0\t0\t0                                                                            \n" +
		"wlan0\t0003A8C0\t00000000\t0001\t0\t0\t307\t00FFFFFF\t0\t0\t0                                                                            \n"
)

func TestHasDefaultRoute(t *testing.T) {
	for _, tc := range []struct {
		name  string
		table string
		want  bool
	}{
		{"empty file", "", false},
		{"header only", routesNone, false},
		{"leased", routesLeased, true},
		{"subnet route only", routesNone + "wlan0\t0003A8C0\t00000000\t0001\t0\t0\t307\t00FFFFFF\t0\t0\t0\n", false},
		{"default route down", routesNone + "wlan0\t00000000\t0103A8C0\t0002\t0\t0\t307\t00000000\t0\t0\t0\n", false},
		{"truncated row", routesNone + "wlan0\t00000000\n", false},
	} {
		if got := hasDefaultRoute(tc.table); got != tc.want {
			t.Errorf("%s: hasDefaultRoute = %v, want %v", tc.name, got, tc.want)
		}
	}
}

func TestNextRetryDoublesToCap(t *testing.T) {
	d := retryMin
	var seen []time.Duration
	for range 8 {
		seen = append(seen, d)
		d = nextRetry(d)
	}
	want := []time.Duration{
		10 * time.Second, 20 * time.Second, 40 * time.Second, 80 * time.Second,
		160 * time.Second, 320 * time.Second, retryMax, retryMax,
	}
	for i := range want {
		if seen[i] != want[i] {
			t.Fatalf("retry schedule %v, want %v", seen, want)
		}
	}
}
