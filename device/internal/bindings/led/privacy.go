package led

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

// Mute-button LED — the discrete red LED under the mic-off button, separate
// from the 12-LED ring, on SoC gpio444. Amazon's privacy driver (amz_priv.c,
// driven from kpd.c) owns that line and names it amz_priv_trig, so
// gpio444's sysfs node never appears under /sys/class/gpio and the LED is
// the driver's to light.
//
// What the source says, matching docs/fireos6-port.md §2's evidence
// (device attrs: enable power_button_state privacy_brightness privacy_state
// privacy_timer_on privacy_trigger shutdown_dialog_state state):
//   - It toggles its own state on every mute-button release: entering waits
//     300ms (privacy_timer_on reads 1 meanwhile), leaving is immediate.
//   - Software can ENTER privacy (write 1 to privacy_trigger) and can never
//     leave it: "ignore exit privacy mode from software". Only the button
//     unmutes the LED.
//   - It always boots unmuted, whatever our persisted mute says.
//
// Found by name under /sys/devices/soc, never by the keypad's address.
// Never read power_button_state: it reads a mute GPIO biscuit's device tree
// does not define, and it blocked the console on read.
var (
	privacyOnce sync.Once
	privacyPath string
)

var errNoPrivacyDriver = errors.New("privacy driver: amz_privacy not found under /sys/devices/soc")

func privacyDir() (string, error) {
	privacyOnce.Do(func() {
		matches, _ := filepath.Glob("/sys/devices/soc/*/amz_privacy/privacy_state")
		if len(matches) > 0 {
			privacyPath = filepath.Dir(matches[0])
		}
	})
	if privacyPath == "" {
		return "", errNoPrivacyDriver
	}
	return privacyPath, nil
}

// PrivacyDriver reports whether Amazon's privacy driver was found. Off-device
// (host tests) it is absent and there is nothing to reconcile.
func PrivacyDriver() bool {
	_, err := privacyDir()
	return err == nil
}

func readPrivacyFlag(name string) (bool, error) {
	dir, err := privacyDir()
	if err != nil {
		return false, err
	}
	b, err := os.ReadFile(filepath.Join(dir, name))
	if err != nil {
		return false, fmt.Errorf("privacy driver: read %s: %w", name, err)
	}
	return strings.TrimSpace(string(b)) == "1", nil
}

// PrivacyMuted is the driver's own mute state (and so its LED).
func PrivacyMuted() (bool, error) { return readPrivacyFlag("privacy_state") }

// PrivacyEntering reports a button-started entry still in its 300ms wait.
func PrivacyEntering() (bool, error) { return readPrivacyFlag("privacy_timer_on") }

// EnterPrivacy puts the driver in its muted state, lighting the LED. There is
// no inverse.
func EnterPrivacy() error {
	dir, err := privacyDir()
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(dir, "privacy_trigger"), []byte("1"), 0644); err != nil {
		return fmt.Errorf("privacy driver: write privacy_trigger: %w", err)
	}
	return nil
}

// SetMuteButtonLED lights the mute-button LED. On puts the driver in its
// muted state, and off does nothing: the button that unmuted us has already
// taken the driver out, and nothing else can.
func SetMuteButtonLED(on bool) error {
	if !on {
		return nil
	}
	if muted, err := PrivacyMuted(); err == nil && muted {
		return nil
	}
	return EnterPrivacy()
}
