// Package platform describes the system image the firmware runs on.
//
// EchoMuse runs only on Fire OS 6 (Android 7.1.2, API 25, product
// biscuit_puffin), the image amonet-biscuit v2.x boots: Amazon's headless
// "Puffin" build with no Java framework, audio owned by Amazon's `mixer`
// daemon (libmixerAPI), Wi-Fi driven by Amazon's `wifisvc`. See
// docs/fireos6-port.md.
package platform

import (
	"os/exec"
	"strings"
	"sync"
)

var osVersion = sync.OnceValue(func() string {
	out, err := exec.Command("getprop", "ro.build.version.name").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
})

// OSVersion is the Fire OS build the device runs, as ro.build.version.name
// names it ("Fire OS 6.5.6.9 (NS6569/6009)"). It is read once per process;
// empty when the property is unreadable, as off-device. Descriptive only.
func OSVersion() string { return osVersion() }
