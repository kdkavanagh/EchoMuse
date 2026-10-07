// Package platform tells the two Echo Dot Gen 2 system images apart.
//
// Fire OS 5 (Android 5.1, API 22) is the image amonet-biscuit v1.x unlocks:
// a full Android framework, audio through AudioFlinger/OpenSL ES, Wi-Fi
// through the framework's `svc wifi`.
//
// Fire OS 6 (Android 7.1.2, API 25, product biscuit_puffin) is the only image
// amonet-biscuit v2.x can boot: Amazon's headless "Puffin" build with no Java
// framework, audio owned by Amazon's `mixer` daemon (libmixerAPI), Wi-Fi
// driven by Amazon's `wifisvc`. See docs/fireos6-port.md.
//
// scripts/start_server.sh makes the same decision from the same property, so
// the supervisor and the firmware can never disagree about which image runs.
package platform

import (
	"os/exec"
	"strconv"
	"strings"
	"sync"
)

// fireOS6MinSDK is Fire OS 6's ro.build.version.sdk (Android 7.1 = 25).
const fireOS6MinSDK = 25

var fireOS6 = sync.OnceValue(func() bool {
	out, err := exec.Command("getprop", "ro.build.version.sdk").Output()
	if err != nil {
		return false
	}
	return isFireOS6SDK(string(out))
})

// FireOS6 reports whether the device runs Fire OS 6. It reads the SDK level
// once per process; off-device (no getprop) it reports Fire OS 5.
func FireOS6() bool { return fireOS6() }

func isFireOS6SDK(prop string) bool {
	sdk, err := strconv.Atoi(strings.TrimSpace(prop))
	return err == nil && sdk >= fireOS6MinSDK
}
