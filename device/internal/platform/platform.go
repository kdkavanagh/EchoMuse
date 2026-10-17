// Package platform identifies the system image the firmware runs on. One
// binary serves every supported device; cmd/server.go picks the audio
// backend and hardware bindings from Detect's answer once at start.
//
//   - KindDot: Echo Dot Gen 2 on Fire OS 6 (Android 7.1.2, API 25,
//     biscuit_puffin), the image amonet-biscuit v2.x boots. Amazon's
//     headless "Puffin" build: no Java framework, audio owned by Amazon's
//     `mixer` daemon (libmixerAPI), Wi-Fi driven by the firmware itself.
//     See docs/fireos6-port.md.
//   - KindCrown: Echo Show 8 1st gen on Fire OS 6 (product crown) with the
//     full Android framework: audio through AudioFlinger and Amazon's HAL,
//     reached with OpenSL ES; Wi-Fi, Bluetooth, time and volume belong to
//     Android. See docs/echo-show-crown.md and docs/echo-show-port-plan.md.
//   - KindCrownLineage: the same Echo Show 8 on LineageOS 18.1 (Android 11,
//     API 30) built from our fork with the NS6571 libasp.so: audio through
//     AAudio, the rest Android's as on Fire OS. See
//     docs/echo-show-crown.md §8.4–8.5 and docs/echo-show-lineage-plan.md.
package platform

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"

	"github.com/wilbowes/EchoMuse/internal/proto"
)

// Kind is a supported device image.
type Kind uint8

const (
	KindDot Kind = iota + 1
	KindCrown
	KindCrownLineage
)

func (k Kind) String() string {
	switch k {
	case KindDot:
		return "dot"
	case KindCrown:
		return "crown"
	case KindCrownLineage:
		return "crown_lineage"
	}
	return fmt.Sprintf("Kind(%d)", uint8(k))
}

// Platform is the session.hello value naming the image.
func (k Kind) Platform() proto.Platform {
	switch k {
	case KindCrown:
		return proto.PlatformFireOS6Crown
	case KindCrownLineage:
		return proto.PlatformLineage18Crown
	}
	return proto.PlatformFireOS6
}

// Show reports whether the image is an Echo Show, on either OS: Android
// owns Wi-Fi, Bluetooth, the clock, the keys and volume there.
func (k Kind) Show() bool { return k == KindCrown || k == KindCrownLineage }

// ScratchDir is where the firmware writes throwaway files (heap profiles).
// The Dot's start_server.sh mounts a tmpfs on /tmp; the Show's rootfs has no
// /tmp on either OS, so it uses /data/local/tmp/echomuse, which its
// start_server.sh branches also log to.
func (k Kind) ScratchDir() string {
	if k.Show() {
		return "/data/local/tmp/echomuse"
	}
	return "/tmp"
}

// The files and properties that tell the images apart.
const (
	mixerLib   = "/system/lib/libmixerAPI.so"
	openSLLib  = "/system/lib/libOpenSLES.so"
	productKey = "ro.product.device"
	lineageKey = "ro.lineage.version"
	sdkKey     = "ro.build.version.sdk"
	crown      = "crown"
	// fireOS6SDK is Fire OS 6's API level (Android 7.1). The Fire OS
	// Show's volume codes are pinned to it: on Android 11 code 10 is
	// setStreamVolume, not getStreamMaxVolume.
	fireOS6SDK = "25"
)

// probe is what detection reads from the system, a seam for the tests.
type probe struct {
	exists  func(path string) bool
	getprop func(key string) string
}

// detect applies the rules in order (docs/echo-show-lineage-plan.md
// decision 2): libmixerAPI.so present is the Dot (its only audio backend);
// ro.product.device=crown with ro.lineage.version set is the LineageOS
// Show; crown at API 25 with libOpenSLES.so present is the Fire OS Show;
// anything else is an image this firmware cannot run on.
func detect(p probe) (Kind, error) {
	if p.exists(mixerLib) {
		return KindDot, nil
	}
	product, lineage, sdk := p.getprop(productKey), p.getprop(lineageKey), p.getprop(sdkKey)
	if product == crown && lineage != "" {
		return KindCrownLineage, nil
	}
	if product == crown && sdk == fireOS6SDK && p.exists(openSLLib) {
		return KindCrown, nil
	}
	return 0, fmt.Errorf("unsupported system image: no %s (Echo Dot, Fire OS 6); %s=%q %s=%q %s=%q, libOpenSLES.so present=%v "+
		"(Echo Show 8 needs %q with %s set, or with %s=%s and %s)",
		mixerLib, productKey, product, lineageKey, lineage, sdkKey, sdk, p.exists(openSLLib),
		crown, lineageKey, sdkKey, fireOS6SDK, openSLLib)
}

var detected = sync.OnceValues(func() (Kind, error) {
	return detect(probe{
		exists:  func(path string) bool { _, err := os.Stat(path); return err == nil },
		getprop: getprop,
	})
})

// Detect identifies the image once per process; later calls return the
// same answer.
func Detect() (Kind, error) { return detected() }

func getprop(key string) string {
	out, err := exec.Command("getprop", key).Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

var osVersion = sync.OnceValue(func() string { return osVersionFrom(getprop) })

// osVersionFrom names the build: Fire OS sets ro.build.version.name, which
// LineageOS leaves empty; there ro.lineage.version names it instead.
func osVersionFrom(getprop func(string) string) string {
	if name := getprop("ro.build.version.name"); name != "" {
		return name
	}
	if v := getprop(lineageKey); v != "" {
		return "LineageOS " + v
	}
	return ""
}

// OSVersion is the build the device runs: ro.build.version.name on Fire OS
// ("Fire OS 6.5.6.9 (NS6569/6009)" on a Dot, "Fire OS 6574.1
// (NS65741/8146)" on the Show), "LineageOS " plus ro.lineage.version on
// LineageOS ("LineageOS 18.1-20261017-UNOFFICIAL-crown"). It is read once
// per process; empty when the properties are unreadable, as off-device.
// Descriptive only.
func OSVersion() string { return osVersion() }
