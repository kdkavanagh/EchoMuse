package companion

import (
	"bytes"
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const (
	// linkDownAfter is how long the app may stay unlinked, after the link
	// dropped or after the last attempt, before the installer runs again
	// (plan decision 5).
	linkDownAfter = 30 * time.Second
	// retryMin separates any two attempts; each failure in a row doubles it
	// up to retryMax, so a broken pm is not hammered every 30 s.
	retryMin = 10 * time.Second
	retryMax = 5 * time.Minute
	// commandTimeout bounds one pm/appops/am/dumpsys run.
	commandTimeout = 2 * time.Minute
	// service is the app's foreground service.
	service = Package + "/.LinkService"
)

// Runner runs one command and returns its combined output.
type Runner interface {
	Run(ctx context.Context, name string, args ...string) ([]byte, error)
}

// ExecRunner runs commands as child processes.
type ExecRunner struct{}

func (ExecRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	return exec.CommandContext(ctx, name, args...).CombinedOutput()
}

// action is what one installer attempt does; a larger one includes the
// smaller ones' work.
type action uint8

const (
	actNone    action = iota
	actGrant          // re-apply the overlay appop (the app reported it denied)
	actEnsure         // install when the version differs, grant, start the service
	actInstall        // reinstall unconditionally, grant, start (link version mismatch)
)

func (a action) String() string {
	switch a {
	case actGrant:
		return "grant"
	case actEnsure:
		return "ensure"
	case actInstall:
		return "reinstall"
	default:
		return "none"
	}
}

// InstallerConfig wires an Installer. Zero APK/Version select the embedded
// app; Now nil selects time.Now.
type InstallerConfig struct {
	Runner Runner
	// Dir is scratch space pm can read the APK from (platform.ScratchDir).
	Dir     string
	Health  *Health
	APK     []byte
	Version string
	Now     func() time.Time
}

// Installer installs the embedded app by version and keeps it running
// (plan decisions 4, 5): at start, whenever the app has been unlinked for
// 30 s, on a link version mismatch (reinstall) and when the app reports its
// overlay op denied (grant only). Attempts run one at a time on Run's
// goroutine, paced by retryMin with backoff on failure; failures reach
// Health and the log only, never the caller.
type Installer struct {
	cfg  InstallerConfig
	wake chan struct{}

	mu     sync.Mutex
	want   action    // requested, not yet run
	linked bool      // an app is linked past hello
	downAt time.Time // when the app was last seen unlinked (start, link drop)
	last   time.Time // the last attempt; zero before the first
	fails  int       // attempts failed in a row
}

// NewInstaller returns an installer whose first Run attempt is due at once.
func NewInstaller(cfg InstallerConfig) *Installer {
	if cfg.APK == nil {
		cfg.APK, cfg.Version = apk, Version()
	}
	if cfg.Now == nil {
		cfg.Now = time.Now
	}
	return &Installer{cfg: cfg, wake: make(chan struct{}, 1), want: actEnsure, downAt: cfg.Now()}
}

// Run attempts installs until ctx ends. With no app embedded it records
// that in Health and returns at once.
func (in *Installer) Run(ctx context.Context) {
	if len(in.cfg.APK) == 0 || in.cfg.Version == "" {
		const msg = "this firmware embeds no companion app"
		log.Printf("[companion] %s", msg)
		in.cfg.Health.setError(msg)
		return
	}
	for {
		a, wait := in.next(in.cfg.Now())
		if a != actNone {
			in.attempt(ctx, a)
			continue
		}
		var timer *time.Timer
		var fire <-chan time.Time
		if wait > 0 {
			timer = time.NewTimer(wait)
			fire = timer.C
		}
		select {
		case <-ctx.Done():
		case <-in.wake:
		case <-fire:
		}
		if timer != nil {
			timer.Stop()
		}
		if ctx.Err() != nil {
			return
		}
	}
}

// SetLinked reports an app linked past hello, or the link dropping.
func (in *Installer) SetLinked(linked bool) {
	in.mu.Lock()
	in.linked = linked
	if !linked {
		in.downAt = in.cfg.Now()
	}
	in.mu.Unlock()
	in.notify()
}

// Reinstall asks for an unconditional reinstall (link version mismatch).
func (in *Installer) Reinstall() { in.request(actInstall) }

// Grant asks for the overlay appop to be applied again.
func (in *Installer) Grant() { in.request(actGrant) }

func (in *Installer) request(a action) {
	in.mu.Lock()
	in.want = max(in.want, a)
	in.mu.Unlock()
	in.notify()
}

func (in *Installer) notify() {
	select {
	case in.wake <- struct{}{}:
	default:
	}
}

// next is the attempt due at now, or actNone and how long until one may be
// (0: none until a request).
func (in *Installer) next(now time.Time) (action, time.Duration) {
	in.mu.Lock()
	defer in.mu.Unlock()
	a, due := in.want, time.Time{}
	if !in.linked {
		recheck := in.downAt
		if in.last.After(recheck) {
			recheck = in.last
		}
		recheck = recheck.Add(linkDownAfter)
		if !now.Before(recheck) {
			a = max(a, actEnsure)
		} else {
			due = recheck
		}
	}
	if a == actNone {
		if due.IsZero() {
			return actNone, 0
		}
		return actNone, due.Sub(now)
	}
	if !in.last.IsZero() {
		if paced := in.last.Add(in.gap()); now.Before(paced) {
			return actNone, paced.Sub(now)
		}
	}
	in.want, in.last = actNone, now
	return a, 0
}

// gap is the pause after the last attempt: retryMin, doubled per failure
// in a row, at most retryMax.
func (in *Installer) gap() time.Duration {
	return min(retryMin<<min(in.fails, 8), retryMax)
}

// attempt runs one action and records its outcome.
func (in *Installer) attempt(ctx context.Context, a action) {
	err := in.run(ctx, a)
	in.mu.Lock()
	if err != nil {
		in.fails++
	} else {
		in.fails = 0
	}
	in.mu.Unlock()
	if err != nil {
		if ctx.Err() == nil {
			log.Printf("[companion] %s: %v", a, err)
			in.cfg.Health.setError(err.Error())
		}
		return
	}
	in.cfg.Health.clearError()
}

func (in *Installer) run(ctx context.Context, a action) error {
	if a >= actEnsure {
		installed, err := in.installedVersion(ctx)
		if err != nil {
			return err
		}
		if a == actInstall || installed != in.cfg.Version {
			log.Printf("[companion] installing %s over %q", in.cfg.Version, installed)
			if err := in.install(ctx); err != nil {
				return err
			}
		}
	}
	if err := in.must(ctx, "appops", "set", Package, "SYSTEM_ALERT_WINDOW", "allow"); err != nil {
		return err
	}
	if a >= actEnsure {
		return in.must(ctx, "am", "start-foreground-service", "-n", service)
	}
	return nil
}

// installedVersion is the installed app's versionName, "" when the app is
// not installed.
func (in *Installer) installedVersion(ctx context.Context) (string, error) {
	out, err := in.cmd(ctx, "dumpsys", "package", Package)
	if err != nil {
		return "", err
	}
	for line := range strings.Lines(string(out)) {
		if v, ok := strings.CutPrefix(strings.TrimSpace(line), "versionName="); ok {
			return v, nil
		}
	}
	return "", nil
}

// install writes the embedded APK to scratch, uninstalls whatever app is
// there (a key change or a downgrade cannot install over it; decision 4)
// and installs.
func (in *Installer) install(ctx context.Context) error {
	if err := os.MkdirAll(in.cfg.Dir, 0o755); err != nil {
		return err
	}
	path := filepath.Join(in.cfg.Dir, "companion.apk")
	if err := os.WriteFile(path, in.cfg.APK, 0o644); err != nil {
		return err
	}
	defer os.Remove(path)
	// Not installed is the expected failure here.
	_, _ = in.cmd(ctx, "pm", "uninstall", Package)
	out, err := in.cmd(ctx, "pm", "install", "-r", path)
	if err != nil {
		return err
	}
	if !bytes.Contains(out, []byte("Success")) {
		return fmt.Errorf("pm install: %s", strings.TrimSpace(string(out)))
	}
	return nil
}

// must runs a command whose failure may still exit 0: appops and am report
// an unknown package or service as an "Error:" line.
func (in *Installer) must(ctx context.Context, name string, args ...string) error {
	out, err := in.cmd(ctx, name, args...)
	if err == nil && bytes.Contains(out, []byte("Error:")) {
		err = fmt.Errorf("%s %s: %s", name, args[0], strings.TrimSpace(string(out)))
	}
	return err
}

// cmd runs one command under commandTimeout; a failure carries its output.
func (in *Installer) cmd(ctx context.Context, name string, args ...string) ([]byte, error) {
	ctx, cancel := context.WithTimeout(ctx, commandTimeout)
	defer cancel()
	out, err := in.cfg.Runner.Run(ctx, name, args...)
	if err != nil {
		msg := strings.TrimSpace(string(out))
		if msg == "" {
			return out, fmt.Errorf("%s %s: %w", name, args[0], err)
		}
		return out, fmt.Errorf("%s %s: %w: %s", name, args[0], err, msg)
	}
	return out, nil
}
