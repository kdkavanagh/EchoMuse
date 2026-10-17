package als

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

// The threshold policy decides what reaches Home Assistant promptly and what
// waits up to 30s for the stats tick. Too sensitive and a flickering lamp
// floods the control plane; too coarse and "someone turned a light on" is the
// thing it misses.
func TestSignificant(t *testing.T) {
	cases := []struct {
		name          string
		baseline, now int
		want          bool
	}{
		// Measured noise on a still room: 309/311/313/308/312 on consecutive
		// reads, about ±1.5%. None of it may report.
		{"still room drift up", 309, 313, false},
		{"still room drift down", 313, 308, false},

		// The case this exists for.
		{"lamp switched on", 40, 300, true},
		{"lamp switched off", 300, 40, true},

		// Hand over the sensor, measured 309 -> 0.
		{"covered", 309, 0, true},
		{"uncovered", 0, 308, true},

		// Near darkness must not produce infinite ratios: a 2 lux wobble in a
		// dark room is not a room lighting up.
		{"tiny change near zero", 0, 2, false},
		{"tiny change near zero, down", 3, 0, false},

		// Big RELATIVE change but small absolute — still noise-ish.
		{"5 to 9 lux", 5, 9, false},

		// Daylight: 50 lux is invisible against 20000 and must not report,
		// which an absolute threshold would get wrong.
		{"daylight jitter", 20000, 20050, false},
		{"cloud clears", 8000, 20000, true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := Significant(c.baseline, c.now); got != c.want {
				t.Fatalf("Significant(%d, %d) = %v, want %v",
					c.baseline, c.now, got, c.want)
			}
		})
	}
}

// Symmetry matters: a light going off should be as reportable as one coming
// on. Comparing against the baseline rather than the new value is what makes
// that true, and it is easy to get backwards.
func TestSignificantIsSymmetricEnough(t *testing.T) {
	if !Significant(300, 40) || !Significant(40, 300) {
		t.Fatal("a lamp must be reportable in both directions")
	}
}

// ── Status reporting (issue #90) ─────────────────────────────────────────────
//
// Two users' Dots reported no ambient light sensor and there was no way to
// tell whether the chip was absent or the driver had not bound, because the
// answer was only ever written to a log file on the device. These pin the
// three verdicts against a fake i2c bus.

// fakeBus builds an i2c tree and points the package at it. Returns the root.
// Also resets the package's cached state, which otherwise leaks between tests
// and makes the second one assert against the first one's scan.
func fakeBus(t *testing.T, devices map[string]bool) {
	t.Helper()
	root := t.TempDir()
	for name, withAttr := range devices {
		dir := filepath.Join(root, name)
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, "name"), []byte(name+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		if withAttr {
			if err := os.WriteFile(filepath.Join(dir, "als_lux"), []byte("42\n"), 0o644); err != nil {
				t.Fatal(err)
			}
		}
	}

	mu.Lock()
	path, lastScan, reported = "", time.Time{}, false
	status = Status{Code: StatusUnknown}
	pollAlsPs, pollFailed = false, false
	mu.Unlock()

	oldI2C, oldIIO, oldAlsPs, oldPoll := i2cGlob, iioGlob, alsPsPath, alsPsPollPath
	i2cGlob = filepath.Join(root, "*", "name")
	// No IIO or als_ps fixture in this helper — point both at empty roots
	// so a stray real sensor on the host (or a leftover test fixture) can
	// never make the i2c-only fixtures match through a fallback.
	iioGlob = filepath.Join(t.TempDir(), "*", "name")
	alsPsPath = filepath.Join(t.TempDir(), "als_ps", "lux")
	alsPsPollPath = filepath.Join(filepath.Dir(alsPsPath), "enable_als_polling")
	t.Cleanup(func() {
		i2cGlob, iioGlob, alsPsPath, alsPsPollPath = oldI2C, oldIIO, oldAlsPs, oldPoll
		mu.Lock()
		path, lastScan, reported = "", time.Time{}, false
		status = Status{Code: StatusUnknown}
		pollAlsPs, pollFailed = false, false
		mu.Unlock()
	})
}

func TestStatusOK(t *testing.T) {
	fakeBus(t, map[string]bool{"tsl2540": true, "tsl2584tsv": false})
	got := Report()
	if got.Code != StatusOK {
		t.Fatalf("code = %q, want %q (detail %q)", got.Code, StatusOK, got.Detail)
	}
	if got.Path == "" {
		t.Fatal("a resolved sensor must report where it was found")
	}
	if !Present() {
		t.Fatal("Present() must be true when the sensor resolves")
	}
}

// Seen must list the WHOLE bus even when the sensor is found early.
//
// The first version returned as soon as it matched, so a working device
// reported only the names sorting before tsl2540 — on real hardware that
// dropped is31fl3236, tlv320aic32x4 and bq24297. Comparing a healthy bus
// against a broken one is what this field is for, so a truncated list from
// the healthy side defeats the purpose. The fixtures missed it because none
// had a device sorting after the match; `zz_after` is here to guarantee one
// always does.
func TestSeenListsWholeBusWhenSensorFound(t *testing.T) {
	fakeBus(t, map[string]bool{
		"aa_before": false,
		"tsl2540":   true,
		"zz_after":  false,
	})
	got := Report()
	if got.Code != StatusOK {
		t.Fatalf("code = %q, want %q", got.Code, StatusOK)
	}
	if len(got.Seen) != 3 {
		t.Fatalf("Seen = %v, want all three bus devices", got.Seen)
	}
	var sawAfter bool
	for _, s := range got.Seen {
		if s == "zz_after" {
			sawAfter = true
		}
	}
	if !sawAfter {
		t.Fatalf("Seen = %v, missing the device that sorts after the sensor", got.Seen)
	}
}

// The hypothesis for #90: these units carry the second ALS, which is present
// on working devices too but has no usable driver, and not the tsl2540.
func TestStatusNoChip(t *testing.T) {
	fakeBus(t, map[string]bool{"tsl2584tsv": false, "tlv320aic3101": false})
	got := Report()
	if got.Code != StatusNoChip {
		t.Fatalf("code = %q, want %q", got.Code, StatusNoChip)
	}
	// Seen is what makes the answer verifiable rather than merely asserted,
	// and identifies an unfamiliar revision the first time one appears.
	if len(got.Seen) != 2 {
		t.Fatalf("Seen = %v, want both bus devices", got.Seen)
	}
	if Present() {
		t.Fatal("Present() must be false with no tsl2540")
	}
}

// Distinct from no_chip because the fixes are opposite: this one is ours.
func TestStatusNoAttribute(t *testing.T) {
	fakeBus(t, map[string]bool{"tsl2540": false})
	got := Report()
	if got.Code != StatusNoAttribute {
		t.Fatalf("code = %q, want %q", got.Code, StatusNoAttribute)
	}
	if got.Detail == "" {
		t.Fatal("a failure must carry a detail a human can read in a bundle")
	}
}

// The absence LOG is deliberately once-only; the status must not be, or a
// device whose bus changed would keep reporting its first answer forever.
func TestStatusRefreshesAcrossScans(t *testing.T) {
	fakeBus(t, map[string]bool{"tsl2584tsv": false})
	if got := Report(); got.Code != StatusNoChip {
		t.Fatalf("first scan: code = %q, want %q", got.Code, StatusNoChip)
	}
	mu.Lock()
	lastScan = time.Time{} // allow an immediate re-scan
	mu.Unlock()
	if got := Report(); got.Code != StatusNoChip {
		t.Fatalf("second scan: code = %q, want %q", got.Code, StatusNoChip)
	}
}

// ── IIO fallback (the fitted tsl2584tsv is bound over IIO, not on the raw
// i2c bus — docs/fireos6-port.md §2) ─────────────────────────────────────────

// A tsl2540 bound on i2c: the IIO bus must never be consulted — even when
// something there would match — so i2c wins outright.
func TestStatusOKFromI2CNeverConsultsIIO(t *testing.T) {
	fakeBus(t, map[string]bool{"tsl2540": true})
	iioRoot := t.TempDir()
	dir := filepath.Join(iioRoot, "tsl2584tsv")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "name"), []byte("tsl2584tsv\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "illuminance0_input"), []byte("999\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	iioGlob = filepath.Join(iioRoot, "*", "name")

	got := Report()
	if got.Code != StatusOK {
		t.Fatalf("code = %q, want %q", got.Code, StatusOK)
	}
	if len(got.Seen) != 1 {
		t.Fatalf("Seen = %v, want only the i2c bus (iio must not be consulted)", got.Seen)
	}
	if lux := Lux(); lux == nil || *lux != 42 {
		t.Fatalf("Lux() = %v, want the i2c reading (42), not the iio one", lux)
	}
}

// The Fire OS 6 shape: the i2c bus lists tsl2540 as board data only (nothing
// answers at 0x39, so no als_lux), and the fitted tsl2584tsv is bound over
// IIO instead. The fallback must find it, and Seen must carry both buses.
func TestStatusOKFromIIOWhenI2CUnbound(t *testing.T) {
	fakeBus(t, map[string]bool{"tsl2540": false})
	iioRoot := t.TempDir()
	dir := filepath.Join(iioRoot, "tsl2584tsv")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "name"), []byte("tsl2584tsv\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "illuminance0_input"), []byte("21\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	iioGlob = filepath.Join(iioRoot, "*", "name")

	got := Report()
	if got.Code != StatusOK {
		t.Fatalf("code = %q, want %q (detail %q)", got.Code, StatusOK, got.Detail)
	}
	if len(got.Seen) != 2 {
		t.Fatalf("Seen = %v, want the i2c tsl2540 entry plus the iio tsl2584tsv entry", got.Seen)
	}
	if !Present() {
		t.Fatal("Present() must be true with a readable IIO sensor")
	}
	if lux := Lux(); lux == nil || *lux != 21 {
		t.Fatalf("Lux() = %v, want 21", lux)
	}
}

// Named on the IIO bus but unbound — the IIO counterpart of
// TestStatusNoAttribute. Degenerate (IIO only registers a device once a
// driver's probe succeeds), but the attribute is what Lux() actually reads,
// so a missing one must still report no_attribute rather than crash.
func TestStatusNoAttributeFromIIO(t *testing.T) {
	fakeBus(t, nil)
	iioRoot := t.TempDir()
	dir := filepath.Join(iioRoot, "tsl2584tsv")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "name"), []byte("tsl2584tsv\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	iioGlob = filepath.Join(iioRoot, "*", "name")

	got := Report()
	if got.Code != StatusNoAttribute {
		t.Fatalf("code = %q, want %q", got.Code, StatusNoAttribute)
	}
	if Present() {
		t.Fatal("Present() must be false with no readable illuminance0_input")
	}
}

// ── The Show's als_ps driver ("<n> lux", docs/echo-show-crown.md §3) ────────

func TestParseLux(t *testing.T) {
	for in, want := range map[string]int{"2 lux\n": 2, "42\n": 42, "0 lux": 0, " 1234 lux \n": 1234} {
		if got, ok := parseLux([]byte(in)); !ok || got != want {
			t.Errorf("parseLux(%q) = %d, %v; want %d", in, got, ok, want)
		}
	}
	for _, bad := range []string{"", "\n", "lux", "x lux", "2 lx", "2 lux now", "2.5 lux"} {
		if got, ok := parseLux([]byte(bad)); ok {
			t.Errorf("parseLux(%q) = %d, want rejected", bad, got)
		}
	}
}

// writeAlsPs puts the Show's als_ps attribute in place with content.
func writeAlsPs(t *testing.T, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(alsPsPath), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(alsPsPath, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

// The Show shape: neither bus carries a known ALS, the stk3x1x's driver
// publishes "2 lux". It resolves, is Present, reads, and Seen names it
// alongside the bus devices.
func TestStatusOKFromAlsPs(t *testing.T) {
	fakeBus(t, map[string]bool{"tlv320aic3101": false, "rt5616": false})
	writeAlsPs(t, "2 lux\n")
	got := Report()
	if got.Code != StatusOK || got.Path != alsPsPath {
		t.Fatalf("code = %q path %q, want ok at %s (detail %q)", got.Code, got.Path, alsPsPath, got.Detail)
	}
	if len(got.Seen) != 3 || got.Seen[2] != "als_ps" {
		t.Fatalf("Seen = %v, want both i2c devices then als_ps", got.Seen)
	}
	if !Present() {
		t.Fatal("Present() must be true with a readable als_ps")
	}
	if lux := Lux(); lux == nil || *lux != 2 {
		t.Fatalf("Lux() = %v, want 2", lux)
	}
	writeAlsPs(t, "309 lux\n")
	if lux := Lux(); lux == nil || *lux != 309 {
		t.Fatalf("Lux() = %v after a change, want 309", lux)
	}
}

// On LineageOS the driver's polling is off until someone switches it on, so
// with EnablePolling the reader writes 1 to enable_als_polling before it
// first reads; on Fire OS (no EnablePolling) it never writes there.
func TestAlsPsPolling(t *testing.T) {
	fakeBus(t, nil)
	writeAlsPs(t, "0 lux\n")
	if !Present() {
		t.Fatal("Present() must be true with a readable als_ps")
	}
	if _, err := os.Stat(alsPsPollPath); !os.IsNotExist(err) {
		t.Fatalf("polling switched without EnablePolling: %v", err)
	}

	fakeBus(t, nil)
	writeAlsPs(t, "0 lux\n")
	EnablePolling()
	if !Present() {
		t.Fatal("Present() must be true with a readable als_ps")
	}
	if b, err := os.ReadFile(alsPsPollPath); err != nil || string(b) != "1" {
		t.Fatalf("enable_als_polling = %q, %v; want \"1\"", b, err)
	}
}

// An als_ps attribute that does not read as a level is not announced.
func TestStatusNoAttributeFromUnreadableAlsPs(t *testing.T) {
	fakeBus(t, nil)
	writeAlsPs(t, "error\n")
	got := Report()
	if got.Code != StatusNoAttribute {
		t.Fatalf("code = %q, want %q", got.Code, StatusNoAttribute)
	}
	if Present() {
		t.Fatal("Present() must be false when als_ps does not read as a level")
	}
}
