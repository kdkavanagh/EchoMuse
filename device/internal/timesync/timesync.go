// Package timesync keeps the wall clock set on Fire OS 6.
//
// On Fire OS 5 the framework owns the clock (NetworkTimeUpdateService, over
// NTP). Fire OS 6 has no framework. Amazon's `sntpd` (/system/bin/sntp -f)
// waits for ACE NetMgr to report the network up, and NetMgr is served by
// wifisvc (AIPC service 1, /dev/aipc/1). start_server.sh stops wifisvc
// (docs/fireos6-port.md §4.1), so sntpd never gets that far. It waits 20 s
// for the service, exits 1 (`aceNetMgr_init, status -1`) and init restarts
// it, forever, even with wlan0 leased and DNS working (hardware,
// 2026-10-07).
//
// At boot the kernel loads the clock from the RTC. The RTC keeps counting
// across a warm reboot. A Dot that has never been set reads 2010-01-01,
// MediaTek's RTC reset value. Amazon's time_update service
// (sntp_time_update.sh) then raises the clock to persist.sys.saved_time if
// that is later. sntp records saved_time on every sync but never writes the
// RTC.
//
// Run takes sntpd's place. Once a default route exists it runs Amazon's own
// client in one-shot mode. `sntp -o` skips NetMgr and uses the same servers
// as sntpd: ntp-g7g.amazon.com, then [0-3].north-america.pool.ntp.org. It
// then copies the clock to the RTC (`hwclock -w -u`), so a warm reboot keeps
// the time and a power loss falls back to saved_time. While no server
// answers it retries with backoff, and after a sync it resyncs daily, which
// is sntpd's own default interval.
//
// Nothing in EchoMuse needs the wall clock to be right:
//   - TLS verification never runs earlier than the build time (client.tlsNow).
//   - Alarms run on controller-trusted UTC mapped to CLOCK_MONOTONIC (alerts,
//     architecture §16.5).
//   - Go timers and deadlines are monotonic, so the step at the first sync is
//     harmless.
//
// This sync is for logs, file times and parity with Fire OS 5.
package timesync

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"

	"github.com/wilbowes/EchoMuse/internal/platform"
)

const (
	sntpPath  = "/system/bin/sntp"
	routePath = "/proc/net/route"

	routePoll = 5 * time.Second
	// sntp tries its five servers in turn with a 15 s default timeout each.
	syncTimeout    = 2 * time.Minute
	resyncInterval = 24 * time.Hour
	retryMin       = 10 * time.Second
	retryMax       = 10 * time.Minute

	rtfUp = 0x1 // RTF_UP, <linux/route.h>
)

// Run keeps the clock synced for the life of the process. It returns at once
// on Fire OS 5, whose framework owns the clock.
func Run() {
	if !platform.FireOS6() {
		return
	}
	if _, err := os.Stat(sntpPath); err != nil {
		log.Printf("[clock] %v: wall clock left unsynced", err)
		return
	}
	retry := retryMin
	for {
		waitForDefaultRoute()
		before := time.Now()
		if err := syncOnce(); err != nil {
			log.Printf("[clock] NTP sync failed: %v; retrying in %s", err, retry)
			time.Sleep(retry)
			retry = nextRetry(retry)
			continue
		}
		after := time.Now()
		// Wall-clock change minus the time sntp took: the step it applied.
		step := after.Round(0).Sub(before.Round(0)) - after.Sub(before)
		log.Printf("[clock] set from NTP (stepped %s)", step.Round(time.Millisecond))
		if out, err := exec.Command("hwclock", "-w", "-u").CombinedOutput(); err != nil {
			log.Printf("[clock] RTC not updated: %v (%s)", err, strings.TrimSpace(string(out)))
		}
		retry = retryMin
		time.Sleep(resyncInterval)
	}
}

// syncOnce runs Amazon's client once. It exits 0 once it has set the clock
// and 1 when no server answered.
func syncOnce() error {
	ctx, cancel := context.WithTimeout(context.Background(), syncTimeout)
	defer cancel()
	out, err := exec.CommandContext(ctx, sntpPath, "-o").CombinedOutput()
	if err != nil {
		return fmt.Errorf("sntp -o: %v (%s)", err, strings.TrimSpace(string(out)))
	}
	return nil
}

func waitForDefaultRoute() {
	for {
		if table, err := os.ReadFile(routePath); err == nil && hasDefaultRoute(string(table)) {
			return
		}
		time.Sleep(routePoll)
	}
}

// hasDefaultRoute reports whether a /proc/net/route table holds an IPv4
// default route that is up. NTP servers are off-link, so without one there
// is nothing to try.
func hasDefaultRoute(table string) bool {
	for _, line := range strings.Split(table, "\n") {
		f := strings.Fields(line)
		if len(f) < 4 || f[1] != "00000000" {
			continue
		}
		if flags, err := strconv.ParseUint(f[3], 16, 32); err == nil && flags&rtfUp != 0 {
			return true
		}
	}
	return false
}

func nextRetry(d time.Duration) time.Duration {
	return min(2*d, retryMax)
}
