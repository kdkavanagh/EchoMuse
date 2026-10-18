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
	"time"
)

const (
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
// overlay op denied (grant only). Attempts are paced by retryMin with
// backoff on failure; failures reach Health and the log only, never the
// caller.
type Installer struct {
	cfg InstallerConfig
	pacer
}

// NewInstaller returns an installer whose first Run attempt is due at once.
func NewInstaller(cfg InstallerConfig) *Installer {
	if cfg.APK == nil {
		cfg.APK, cfg.Version = apk, Version()
	}
	in := &Installer{cfg: cfg}
	in.pacer.init(cfg.Now)
	return in
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
	in.run(ctx, in.attempt)
}

// Reinstall asks for an unconditional reinstall (link version mismatch).
func (in *Installer) Reinstall() { in.request(actInstall) }

// Grant asks for the overlay appop to be applied again.
func (in *Installer) Grant() { in.request(actGrant) }

// attempt runs one action and records its outcome.
func (in *Installer) attempt(ctx context.Context, a action) {
	err := in.do(ctx, a)
	in.record(err != nil)
	if err != nil {
		if ctx.Err() == nil {
			log.Printf("[companion] %s: %v", a, err)
			in.cfg.Health.setError(err.Error())
		}
		return
	}
	in.cfg.Health.clearError()
}

func (in *Installer) do(ctx context.Context, a action) error {
	if a >= actEnsure {
		installed, err := packageVersion(ctx, in.cfg.Runner, Package)
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
	_, _ = runCommand(ctx, in.cfg.Runner, "pm", "uninstall", Package)
	out, err := runCommand(ctx, in.cfg.Runner, "pm", "install", "-r", path)
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
	out, err := runCommand(ctx, in.cfg.Runner, name, args...)
	if err == nil && bytes.Contains(out, []byte("Error:")) {
		err = fmt.Errorf("%s %s: %s", name, args[0], strings.TrimSpace(string(out)))
	}
	return err
}

// packageVersion is an installed package's versionName, "" when it is not
// installed.
func packageVersion(ctx context.Context, r Runner, pkg string) (string, error) {
	out, err := runCommand(ctx, r, "dumpsys", "package", pkg)
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

// runCommand runs one command under commandTimeout; a failure carries its
// output.
func runCommand(ctx context.Context, r Runner, name string, args ...string) ([]byte, error) {
	ctx, cancel := context.WithTimeout(ctx, commandTimeout)
	defer cancel()
	out, err := r.Run(ctx, name, args...)
	if err != nil {
		msg := strings.TrimSpace(string(out))
		if msg == "" {
			return out, fmt.Errorf("%s %s: %w", name, args[0], err)
		}
		return out, fmt.Errorf("%s %s: %w: %s", name, args[0], err, msg)
	}
	return out, nil
}
