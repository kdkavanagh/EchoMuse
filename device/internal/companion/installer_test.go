package companion

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

const (
	testVersion = "cmp-aaaaaaaaaaaa"
	pkgDump     = "Packages:\n  Package [org.echomuse.companion] (1a2b3c):\n    versionCode=1 minSdk=30 targetSdk=30\n    versionName=%s\n"
)

var errExit = errors.New("exit status 1")

// fakeRunner answers commands by prefix and records each command line.
type fakeRunner struct {
	mu      sync.Mutex
	calls   []string
	replies map[string]reply
	apk     []byte // what pm install found at its path
	ran     chan struct{}
}

type reply struct {
	out string
	err error
}

func (f *fakeRunner) Run(_ context.Context, name string, args ...string) ([]byte, error) {
	line := strings.Join(append([]string{name}, args...), " ")
	f.mu.Lock()
	defer f.mu.Unlock()
	f.calls = append(f.calls, line)
	if name == "pm" && args[0] == "install" {
		f.apk, _ = os.ReadFile(args[len(args)-1])
	}
	if f.ran != nil {
		select {
		case f.ran <- struct{}{}:
		default:
		}
	}
	for prefix, r := range f.replies {
		if strings.HasPrefix(line, prefix) {
			return []byte(r.out), r.err
		}
	}
	return nil, nil
}

func (f *fakeRunner) commands() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return slices.Clone(f.calls)
}

func newTestInstaller(t *testing.T, r *fakeRunner) (*Installer, *Health, string) {
	t.Helper()
	dir := filepath.Join(t.TempDir(), "scratch")
	h := &Health{}
	return NewInstaller(InstallerConfig{Runner: r, Dir: dir, Health: h, APK: []byte("the apk"), Version: testVersion}), h, dir
}

func installed(version string) reply {
	return reply{out: strings.Replace(pkgDump, "%s", version, 1)}
}

// Decision 4: the app is (re)installed exactly when the installed
// versionName differs from the embedded one, uninstalling first; the
// overlay op and the service start follow every check.
func TestInstallerDecisions(t *testing.T) {
	appops := "appops set org.echomuse.companion SYSTEM_ALERT_WINDOW allow"
	am := "am start-foreground-service -n org.echomuse.companion/.LinkService"
	for _, tc := range []struct {
		name    string
		act     action
		replies map[string]reply
		install bool
		want    []string // after dumpsys / install
		err     string
	}{
		{name: "missing app", act: actEnsure, replies: map[string]reply{
			"dumpsys":    {out: "Unable to find package: org.echomuse.companion\n"},
			"pm install": {out: "Success\n"},
		}, install: true, want: []string{appops, am}},
		{name: "same version", act: actEnsure, replies: map[string]reply{"dumpsys": installed(testVersion)}, want: []string{appops, am}},
		{name: "different version", act: actEnsure, replies: map[string]reply{
			"dumpsys":    installed("cmp-bbbbbbbbbbbb"),
			"pm install": {out: "Success\n"},
		}, install: true, want: []string{appops, am}},
		{name: "link mismatch reinstalls the same version", act: actInstall, replies: map[string]reply{
			"dumpsys":    installed(testVersion),
			"pm install": {out: "Success\n"},
		}, install: true, want: []string{appops, am}},
		{name: "overlay denied grants only", act: actGrant, want: []string{appops}},
		{name: "failing pm", act: actEnsure, replies: map[string]reply{
			"dumpsys":    installed("cmp-bbbbbbbbbbbb"),
			"pm install": {out: "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE]\n", err: errExit},
		}, install: true, err: "pm install: exit status 1: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE]"},
		{name: "pm without Success", act: actEnsure, replies: map[string]reply{
			"dumpsys":    installed("cmp-bbbbbbbbbbbb"),
			"pm install": {out: "Failure [INSTALL_FAILED_INVALID_APK]\n"},
		}, install: true, err: "pm install: Failure [INSTALL_FAILED_INVALID_APK]"},
		{name: "failing am", act: actEnsure, replies: map[string]reply{
			"dumpsys": installed(testVersion),
			"am":      {out: "Error: Not found; no service started.\n"},
		}, want: []string{appops, am}, err: "am start-foreground-service: Error: Not found; no service started."},
		{name: "failing dumpsys", act: actEnsure, replies: map[string]reply{
			"dumpsys": {out: "Can't find service: package\n", err: errExit},
		}, err: "dumpsys package: exit status 1: Can't find service: package"},
	} {
		r := &fakeRunner{replies: tc.replies}
		in, h, dir := newTestInstaller(t, r)
		in.attempt(context.Background(), tc.act)

		var want []string
		if tc.act >= actEnsure {
			want = append(want, "dumpsys package org.echomuse.companion")
		}
		if tc.install {
			want = append(want, "pm uninstall org.echomuse.companion", "pm install -r "+filepath.Join(dir, "companion.apk"))
		}
		want = append(want, tc.want...)
		if got := r.commands(); !slices.Equal(got, want) {
			t.Errorf("%s: commands\n got %q\nwant %q", tc.name, got, want)
		}
		if tc.install && !bytes.Equal(r.apk, []byte("the apk")) {
			t.Errorf("%s: pm install read %q", tc.name, r.apk)
		}
		if _, err := os.Stat(filepath.Join(dir, "companion.apk")); !errors.Is(err, os.ErrNotExist) {
			t.Errorf("%s: scratch APK left behind: %v", tc.name, err)
		}
		st := h.Stats()
		switch {
		case tc.err == "" && st.Error != nil:
			t.Errorf("%s: health error %q", tc.name, *st.Error)
		case tc.err != "" && (st.Error == nil || *st.Error != tc.err):
			t.Errorf("%s: health error %v, want %q", tc.name, st.Error, tc.err)
		}
		if fails := map[bool]int{false: 0, true: 1}[tc.err != ""]; in.fails != fails {
			t.Errorf("%s: fails %d, want %d", tc.name, in.fails, fails)
		}
	}
}

// Decision 5's pacing: the first check is due at once; nothing runs while
// the app is linked; 30 s after the link drops (or after the last attempt)
// the check runs again; requests wait out the gap, which doubles per
// failure in a row up to retryMax.
func TestInstallerPacing(t *testing.T) {
	t0 := time.Unix(1_000_000, 0)
	now := t0
	in := NewInstaller(InstallerConfig{Runner: &fakeRunner{}, Health: &Health{}, APK: []byte("x"), Version: testVersion,
		Now: func() time.Time { return now }})
	expect := func(at time.Duration, wantAct action, wantWait time.Duration) {
		t.Helper()
		now = t0.Add(at)
		a, wait := in.next(now)
		if a != wantAct || wait != wantWait {
			t.Fatalf("at %s: next = %s, %s; want %s, %s", at, a, wait, wantAct, wantWait)
		}
	}

	expect(0, actEnsure, 0)
	expect(time.Second, actNone, 29*time.Second) // unlinked: recheck 30 s after the attempt
	now = t0.Add(2 * time.Second)
	in.SetLinked(true)
	expect(time.Hour, actNone, 0)
	now = t0.Add(time.Hour)
	in.SetLinked(false)
	expect(time.Hour+29*time.Second, actNone, time.Second)
	expect(time.Hour+30*time.Second, actEnsure, 0)

	// Three failures in a row: the gap is 80 s, and a request waits it out.
	in.fails = 3
	in.Reinstall()
	expect(time.Hour+40*time.Second, actNone, 70*time.Second)
	expect(time.Hour+110*time.Second, actInstall, 0)
	in.Grant()
	in.Reinstall()
	expect(time.Hour+111*time.Second, actNone, 79*time.Second)
	expect(time.Hour+190*time.Second, actInstall, 0) // the larger request wins

	in.fails = 20
	if g := in.gap(); g != retryMax {
		t.Fatalf("gap after 20 failures %s, want %s", g, retryMax)
	}
	in.fails = 0
	if g := in.gap(); g != retryMin {
		t.Fatalf("gap after success %s, want %s", g, retryMin)
	}
}

// A firmware without an embedded app reports it and never runs pm.
func TestInstallerWithoutAPK(t *testing.T) {
	r := &fakeRunner{}
	h := &Health{}
	in := NewInstaller(InstallerConfig{Runner: r, Dir: t.TempDir(), Health: h})
	done := make(chan struct{})
	go func() { in.Run(context.Background()); close(done) }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Run kept running without an APK")
	}
	if st := h.Stats(); st.Error == nil || *st.Error != "this firmware embeds no companion app" || len(r.commands()) != 0 {
		t.Fatalf("health %+v, commands %q", st, r.commands())
	}
}

// Risk table: an installer failure never ends Run; only ctx does.
func TestInstallerFailureNeverEndsRun(t *testing.T) {
	r := &fakeRunner{ran: make(chan struct{}, 1), replies: map[string]reply{"": {out: "boom", err: errExit}}}
	in, h, _ := newTestInstaller(t, r)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { in.Run(ctx); close(done) }()
	select {
	case <-r.ran:
	case <-time.After(5 * time.Second):
		t.Fatal("no attempt at start")
	}
	in.Grant() // paced: waits out the failure gap rather than running now
	select {
	case <-done:
		t.Fatal("Run ended on an installer failure")
	case <-time.After(200 * time.Millisecond):
	}
	if st := h.Stats(); st.Error == nil || !strings.Contains(*st.Error, "boom") {
		t.Fatalf("health %+v", st)
	}
	if n := len(r.commands()); n != 1 {
		t.Fatalf("%d commands, want the one failed dumpsys: %q", n, r.commands())
	}
	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Run did not end with ctx")
	}
}
