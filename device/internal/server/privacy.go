package server

import (
	"log"
	"time"

	internalled "github.com/wilbowes/EchoMuse/internal/bindings/led"
)

// Keeping Amazon's privacy driver in step with our mute
// (internal/bindings/led/privacy.go). Both toggle on every press, so they
// agree until one changes without the other, which a reboot while muted does:
// we restore muted, the driver boots unmuted, and from then on each press
// leaves them opposite — including a live mic under a lit mute button.

type privacyAction int

const (
	privacyInSync privacyAction = iota
	// We are muted and the driver is not: tell it. Software can do this.
	privacyEnterDriver
	// The driver is muted and we are not. Software cannot take the driver
	// out, so mute ourselves: both then say muted and the mic is off, and
	// the next press unmutes both.
	privacyMuteOurs
)

func decidePrivacy(ours, driver bool) privacyAction {
	switch {
	case ours && !driver:
		return privacyEnterDriver
	case !ours && driver:
		return privacyMuteOurs
	}
	return privacyInSync
}

// privacySettle outlasts the driver's 300ms entry delay.
const privacySettle = 700 * time.Millisecond

// reconcilePrivacySoon checks the two agree once a press (or boot restore)
// has settled.
func (m *muteController) reconcilePrivacySoon() {
	if !internalled.PrivacyDriver() {
		return
	}
	time.AfterFunc(privacySettle, func() { m.reconcilePrivacy(5) })
}

func (m *muteController) reconcilePrivacy(retries int) {
	if entering, err := internalled.PrivacyEntering(); err == nil && entering && retries > 0 {
		time.AfterFunc(privacySettle, func() { m.reconcilePrivacy(retries - 1) })
		return
	}
	driver, err := internalled.PrivacyMuted()
	if err != nil {
		log.Printf("[privacy] %v", err)
		return
	}
	switch decidePrivacy(m.isMuted(), driver) {
	case privacyEnterDriver:
		log.Println("[privacy] driver was unmuted while we are muted — muting it")
		if err := internalled.EnterPrivacy(); err != nil {
			log.Printf("[privacy] %v", err)
		}
	case privacyMuteOurs:
		log.Println("[privacy] driver is muted and cannot be unmuted from software — muting to match")
		m.set(true)
	}
}
