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
)

func (k Kind) String() string {
	switch k {
	case KindDot:
		return "dot"
	case KindCrown:
		return "crown"
	}
	return fmt.Sprintf("Kind(%d)", uint8(k))
}

// Platform is the session.hello value naming the image.
func (k Kind) Platform() proto.Platform {
	if k == KindCrown {
		return proto.PlatformFireOS6Crown
	}
	return proto.PlatformFireOS6
}

// ScratchDir is where the firmware writes throwaway files (heap profiles).
// The Dot's start_server.sh mounts a tmpfs on /tmp; the Show's rootfs is a
// read-only ramdisk with no /tmp, so it uses /data/local/tmp/echomuse,
// which its start_server.sh branch also logs to.
func (k Kind) ScratchDir() string {
	if k == KindCrown {
		return "/data/local/tmp/echomuse"
	}
	return "/tmp"
}

// The files and property that tell the images apart.
const (
	mixerLib   = "/system/lib/libmixerAPI.so"
	openSLLib  = "/system/lib/libOpenSLES.so"
	productKey = "ro.product.device"
	crown      = "crown"
)

// probe is what detection reads from the system, a seam for the tests.
type probe struct {
	exists  func(path string) bool
	getprop func(key string) string
}

// detect applies the rule: libmixerAPI.so present is the Dot (its only
// audio backend); ro.product.device=crown with libOpenSLES.so present is
// the Show; anything else is an image this firmware cannot run on.
func detect(p probe) (Kind, error) {
	if p.exists(mixerLib) {
		return KindDot, nil
	}
	product := p.getprop(productKey)
	if product == crown && p.exists(openSLLib) {
		return KindCrown, nil
	}
	return 0, fmt.Errorf("unsupported system image: no %s (Echo Dot, Fire OS 6), and %s=%q with libOpenSLES.so present=%v (Echo Show 8 needs %q with %s)",
		mixerLib, productKey, product, p.exists(openSLLib), crown, openSLLib)
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

var osVersion = sync.OnceValue(func() string { return getprop("ro.build.version.name") })

// OSVersion is the Fire OS build the device runs, as ro.build.version.name
// names it ("Fire OS 6.5.6.9 (NS6569/6009)" on a Dot, "Fire OS 6574.1
// (NS65741/8146)" on the Show). It is read once per process; empty when
// the property is unreadable, as off-device. Descriptive only.
func OSVersion() string { return osVersion() }
