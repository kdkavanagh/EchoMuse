//go:build server

package main

import (
	"bufio"
	"log"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/wilbowes/EchoMuse/internal/bindings/als"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/wifi"
)

// ─── Hardware stats collection (retained `stats` body, WIRE §4.8) ─────────────

// statsCollector holds the state stats reports carry between calls: the
// previous network counters (reported as per-interval deltas) and the cached
// wpa_cli link info. collect runs from the 30 s ticker and after every
// session.ready, so calls are serialized by mu.
type statsCollector struct {
	mu   sync.Mutex
	net  netCounters
	link linkInfoCache
}

// collect reads one stats body.
func (c *statsCollector) collect() proto.Stats {
	c.mu.Lock()
	defer c.mu.Unlock()
	cpuPct := cpuPercent()
	memUsed, memTotal := memStats()
	stoUsed, stoTotal := storageStats()
	rssi := wifiRSSI()
	tx, rx, txErr, txDrop, rxCrc := c.net.deltas()
	speed, freq, bssid := c.link.get()
	cpuC, maxC, coreLimit := thermals()
	return proto.Stats{
		AmbientLux:       als.Lux(),
		CPUTempC:         cpuC,
		MaxTempC:         maxC,
		CoresOnline:      coresOnline(),
		CoresTotal:       coresTotal(),
		ThermalCoreLimit: coreLimit,
		CPUPct:           cpuPct,
		MemUsedMb:        memUsed,
		MemTotalMb:       memTotal,
		StorageUsedMb:    stoUsed,
		StorageTotalMb:   stoTotal,
		WifiRssi:         rssi,
		WifiSsid:         wifi.CurrentSSID(),
		LinkSpeedMbps:    speed,
		WifiFreqMhz:      freq,
		WifiBssid:        bssid,
		TxBytes:          tx,
		RxBytes:          rx,
		TxErrors:         txErr,
		TxDropped:        txDrop,
		RxCrcErrors:      rxCrc,
	}
}

// ─── Network telemetry ────────────────────────────────────────────────────────

// netCounters holds the previous sysfs read so stats can be reported as
// per-interval deltas.
type netCounters struct {
	tx, rx, txErr, txDrop, rxCrc uint64
	primed                       bool
}

// deltas returns tx/rx bytes and error counts accumulated since the
// previous call, read from /sys/class/net/wlan0/statistics/. Plain file
// reads — no process spawn — so this is cheap enough for every stats tick.
// The first call primes the baseline and reports zeros.
func (prev *netCounters) deltas() (tx, rx, txErr, txDrop, rxCrc uint64) {
	read := func(name string) uint64 {
		b, err := os.ReadFile("/sys/class/net/wlan0/statistics/" + name)
		if err != nil {
			return 0
		}
		v, _ := strconv.ParseUint(strings.TrimSpace(string(b)), 10, 64)
		return v
	}
	ctx, crx := read("tx_bytes"), read("rx_bytes")
	cErr, cDrop, cCrc := read("tx_errors"), read("tx_dropped"), read("rx_crc_errors")

	// delta guards against counter resets (interface bounce) by clamping
	// a negative difference to 0 rather than reporting a huge number.
	delta := func(cur, prev uint64) uint64 {
		if cur < prev {
			return 0
		}
		return cur - prev
	}
	if prev.primed {
		tx = delta(ctx, prev.tx)
		rx = delta(crx, prev.rx)
		txErr = delta(cErr, prev.txErr)
		txDrop = delta(cDrop, prev.txDrop)
		rxCrc = delta(cCrc, prev.rxCrc)
	}
	*prev = netCounters{tx: ctx, rx: crx, txErr: cErr, txDrop: cDrop, rxCrc: cCrc, primed: true}
	return
}

// linkInfoCache holds the last wpa_cli result and when it was taken.
type linkInfoCache struct {
	speed, freq int
	bssid       string
	at          time.Time
}

// linkInfoInterval — how often the wpa_cli subprocess is actually run.
// Unlike everything else in a stats report this costs a process spawn, and
// PHY rate / band / AP change on the scale of minutes, not seconds. Cached
// values are reused between refreshes so every stats message still carries
// the fields.
const linkInfoInterval = 2 * time.Minute

// get returns negotiated PHY rate (Mbps), frequency (MHz) and BSSID.
//
// Requires the -p control-socket path: plain `wpa_cli -i wlan0` answers
// UNKNOWN COMMAND on FireOS because the default socket dir doesn't exist.
// Returns zero values if wpa_supplicant isn't reachable — the fields are
// omitempty, so the controller sees them absent rather than wrong.
func (c *linkInfoCache) get() (speed, freq int, bssid string) {
	if time.Since(c.at) < linkInfoInterval {
		return c.speed, c.freq, c.bssid
	}
	c.at = time.Now()

	out, err := exec.Command("wpa_cli", "-p", "/data/misc/wifi/sockets",
		"-i", "wlan0", "signal_poll").Output()
	if err == nil {
		for _, line := range strings.Split(string(out), "\n") {
			k, v, ok := strings.Cut(strings.TrimSpace(line), "=")
			if !ok {
				continue
			}
			n, convErr := strconv.Atoi(v)
			if convErr != nil {
				continue
			}
			switch k {
			case "LINKSPEED":
				c.speed = n
			case "FREQUENCY":
				c.freq = n
			}
		}
	}
	if out, err := exec.Command("wpa_cli", "-p", "/data/misc/wifi/sockets",
		"-i", "wlan0", "status").Output(); err == nil {
		for _, line := range strings.Split(string(out), "\n") {
			if v, ok := strings.CutPrefix(strings.TrimSpace(line), "bssid="); ok {
				c.bssid = v
				break
			}
		}
	}
	return c.speed, c.freq, c.bssid
}

// cpuPercent samples /proc/stat twice over 500ms and returns utilisation %.
func cpuPercent() float64 {
	type snap struct{ total, idle uint64 }

	read := func() (snap, bool) {
		f, err := os.Open("/proc/stat")
		if err != nil {
			return snap{}, false
		}
		defer f.Close()
		sc := bufio.NewScanner(f)
		for sc.Scan() {
			line := sc.Text()
			if !strings.HasPrefix(line, "cpu ") {
				continue
			}
			fields := strings.Fields(line)[1:] // skip "cpu"
			var vals [8]uint64
			for i := 0; i < len(fields) && i < 8; i++ {
				vals[i], _ = strconv.ParseUint(fields[i], 10, 64)
			}
			// user nice system idle iowait irq softirq steal
			idle := vals[3] + vals[4] // idle + iowait
			total := vals[0] + vals[1] + vals[2] + vals[3] +
				vals[4] + vals[5] + vals[6] + vals[7]
			return snap{total, idle}, true
		}
		return snap{}, false
	}

	s1, ok1 := read()
	time.Sleep(500 * time.Millisecond)
	s2, ok2 := read()
	if !ok1 || !ok2 {
		return 0
	}
	dTotal := float64(s2.total - s1.total)
	if dTotal <= 0 {
		return 0
	}
	dIdle := float64(s2.idle - s1.idle)
	pct := (1 - dIdle/dTotal) * 100
	// Round to one decimal place
	return math.Round(pct*10) / 10
}

// memStats reads /proc/meminfo and returns (used MB, total MB).
func memStats() (usedMb, totalMb int) {
	f, err := os.Open("/proc/meminfo")
	if err != nil {
		return 0, 0
	}
	defer f.Close()

	var totalKb, availKb uint64
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) < 2 {
			continue
		}
		val, _ := strconv.ParseUint(fields[1], 10, 64)
		switch fields[0] {
		case "MemTotal:":
			totalKb = val
		case "MemAvailable:":
			availKb = val
		}
	}
	if totalKb == 0 {
		return 0, 0
	}
	usedKb := totalKb - availKb
	return int(usedKb / 1024), int(totalKb / 1024)
}

// storageStats returns (used MB, total MB) for /data via statfs.
func storageStats() (usedMb, totalMb int) {
	var st syscall.Statfs_t
	if err := syscall.Statfs("/data", &st); err != nil {
		return 0, 0
	}
	bsize := uint64(st.Bsize)
	total := st.Blocks * bsize
	free := st.Bfree * bsize
	used := total - free
	const mb = 1024 * 1024
	return int(used / mb), int(total / mb)
}

// selfRSSKb reads the process's resident set size from /proc/self/status —
// the OS's ground truth, against which the Go runtime numbers in the [mem]
// log line are compared. 0 if unreadable.
func selfRSSKb() int {
	f, err := os.Open("/proc/self/status")
	if err != nil {
		return 0
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) >= 2 && fields[0] == "VmRSS:" {
			kb, _ := strconv.Atoi(fields[1])
			return kb
		}
	}
	return 0
}

// wifiRSSI reads /proc/net/wireless and returns the signal level in dBm,
// or nil if the interface is not available.
//
// Some kernels encode the level field as a positive offset (0–255) rather
// than signed dBm; values > 0 are adjusted by subtracting 256 to recover
// the actual dBm reading (e.g. 206 → -50 dBm).
func wifiRSSI() *int {
	f, err := os.Open("/proc/net/wireless")
	if err != nil {
		return nil
	}
	defer f.Close()

	sc := bufio.NewScanner(f)
	lineNum := 0
	for sc.Scan() {
		lineNum++
		if lineNum <= 2 {
			continue // skip two header lines
		}
		fields := strings.Fields(sc.Text())
		// fields: [iface status link level noise ...]
		if len(fields) < 4 {
			continue
		}
		// level is fields[3], may have a trailing "."
		rssiStr := strings.TrimRight(fields[3], ".")
		rssi, err := strconv.Atoi(rssiStr)
		if err != nil {
			continue
		}
		// Correct offset encoding used by some kernels
		if rssi > 0 {
			rssi -= 256
		}
		// Sanity check — valid RSSI is roughly -30 to -100 dBm
		if rssi < -120 || rssi > 0 {
			continue
		}
		return &rssi
	}
	return nil
}

// ─── Thermals and core hotplug ────────────────────────────────────────────────
//
// Two facts about this SoC make these worth reporting, and they are related.
//
// The MT8163 is a QUAD-core Cortex-A53, but MediaTek's hotplug strategy parks
// cores that are not needed: /sys/devices/system/cpu/online is usually just
// "0". A second core comes online only after utilisation holds above
// /proc/hps/up_threshold (80%) for up_times (2) samples. So a device sitting
// at 54% is not near a ceiling — it is comfortably inside one core's budget
// with three more parked.
//
// That directly undermines cpuPct, which is derived from the aggregate
// /proc/stat line and is therefore a share of ONLINE capacity: the same
// absolute work halves its reported percentage the moment a second core
// appears. Reporting coresOnline alongside it is what makes the number
// interpretable rather than merely available.
//
// Thermals matter for the opposite reason — to show there is nothing to worry
// about, or to show when there is. thermalCoreLimit is the sharpest indicator
// this SoC offers: it is how many cores the thermal governor will currently
// permit, so anything below 4 means throttling has begun, which shows up as
// capacity loss long before a temperature reading looks alarming.

// thermalZones maps a zone type ("mtktscpu") to its temp file. Resolved once —
// the names are stable for the life of the boot, and rescanning 11 sysfs
// directories every 30s to learn nothing would be silly.
var (
	thermalOnce   sync.Once
	thermalByType map[string]string
)

func resolveThermalZones() map[string]string {
	thermalOnce.Do(func() {
		thermalByType = map[string]string{}
		dirs, err := filepath.Glob("/sys/class/thermal/thermal_zone*")
		if err != nil {
			return
		}
		for _, d := range dirs {
			b, err := os.ReadFile(filepath.Join(d, "type"))
			if err != nil {
				continue
			}
			thermalByType[strings.TrimSpace(string(b))] = filepath.Join(d, "temp")
		}
		log.Printf("[thermal] %d zones: %s", len(thermalByType), strings.Join(zoneTypes(), " "))
	})
	return thermalByType
}

func zoneTypes() []string {
	out := make([]string, 0, len(thermalByType))
	for t := range thermalByType {
		out = append(out, t)
	}
	sort.Strings(out)
	return out
}

// readMilliC reads a sysfs temperature (millidegrees C) as degrees.
func readMilliC(path string) (float64, bool) {
	b, err := os.ReadFile(path)
	if err != nil {
		return 0, false
	}
	n, err := strconv.Atoi(strings.TrimSpace(string(b)))
	if err != nil {
		return 0, false
	}
	// Sanity bound: a plausible reading is roughly -20..150C. Some MTK zones
	// report a sentinel (0, or a huge value) when their sensor is not wired,
	// and averaging that into a trend would quietly ruin it — the same reason
	// the RF counters are deliberately not surfaced.
	c := float64(n) / 1000.0
	if c < -20 || c > 150 {
		return 0, false
	}
	return c, true
}

// thermals returns the CPU zone temperature, the hottest zone of any kind, and
// how many cores the thermal governor currently permits.
//
// mtktscpu is the SoC/CPU zone. The hottest-of-all figure is reported too
// because the PMIC and board sensors can run warmer than the CPU, and a device
// in trouble will not necessarily show it on the zone you thought to watch.
func thermals() (cpuC *float64, maxC *float64, coreLimit int) {
	zones := resolveThermalZones()
	if c, ok := readMilliC(zones["mtktscpu"]); ok {
		cpuC = &c
	}
	var hottest float64
	var any bool
	for _, p := range zones {
		if c, ok := readMilliC(p); ok && (!any || c > hottest) {
			hottest, any = c, true
		}
	}
	if any {
		maxC = &hottest
	}
	// /proc/hps/num_limit_thermal — cores the thermal governor allows. Absent
	// on a kernel without MTK HPS, reported as 0 = unknown rather than 0 cores.
	if b, err := os.ReadFile("/proc/hps/num_limit_thermal"); err == nil {
		if n, err := strconv.Atoi(strings.TrimSpace(string(b))); err == nil {
			coreLimit = n
		}
	}
	return cpuC, maxC, coreLimit
}

// coresOnline counts online CPUs from /sys/devices/system/cpu/online, whose
// format is a range list ("0", "0-3", "0,2-3").
func coresOnline() int {
	b, err := os.ReadFile("/sys/devices/system/cpu/online")
	if err != nil {
		return 0
	}
	n := 0
	for _, part := range strings.Split(strings.TrimSpace(string(b)), ",") {
		if part == "" {
			continue
		}
		lo, hi, found := strings.Cut(part, "-")
		a, err1 := strconv.Atoi(strings.TrimSpace(lo))
		if !found {
			if err1 == nil {
				n++
			}
			continue
		}
		z, err2 := strconv.Atoi(strings.TrimSpace(hi))
		if err1 == nil && err2 == nil && z >= a {
			n += z - a + 1
		}
	}
	return n
}

// hpsCoreFloor is the minimum number of CPU cores kept online.
//
// The MT8163 has four Cortex-A53 cores and MediaTek's hotplug strategy parks
// all but one, bringing a second up only after utilisation holds above
// /proc/hps/up_threshold (80%) for up_times (2) samples. That is a sensible
// default for an idle appliance and a poor one for this workload: the mic
// pipeline has a hard 160ms deadline (the ALSA ring's whole depth) and now
// shares a core with wake word inference that runs in ~31ms bursts. Time-
// slicing those on one core works — measured, zero stalls — but it works with
// no margin for a coincidence, and it depends on hotplug reacting in time to a
// burst that has already started.
//
// A floor of 2 lets the two actually run in parallel, and leaves up_threshold
// to scale to 3 and 4 exactly as before. The cost is one A53 core out of idle,
// which on a mains-powered device sitting at 33C is not a real cost: measured
// +0.3C at the PMIC and no change at the CPU zone.
//
// Set via num_base_perf_serv, which is HPS's core-count FLOOR (the num_limit_*
// files are its ceilings, all 4 here). Deliberately NOT done by writing
// cpu1/online directly: HPS would re-park it within down_times samples, and
// fighting the governor is how you get a setting that appears to work and
// silently stops.
const hpsCoreFloor = 2

// applyCoreFloor raises the hotplug floor, best-effort.
//
// procfs, so it does not survive a reboot — which is why it lives here, in the
// binary, rather than in a provisioning script: it travels with the firmware
// and re-applies on every start. Absent on a kernel without MTK HPS, in which
// case there is nothing to do and nothing to warn about.
func applyCoreFloor() {
	const path = "/proc/hps/num_base_perf_serv"
	before, err := os.ReadFile(path)
	if err != nil {
		return // not an MTK HPS kernel
	}
	if strings.TrimSpace(string(before)) == strconv.Itoa(hpsCoreFloor) {
		return
	}
	if err := os.WriteFile(path, []byte(strconv.Itoa(hpsCoreFloor)), 0o644); err != nil {
		log.Printf("[cpu] could not raise core floor to %d: %v", hpsCoreFloor, err)
		return
	}
	log.Printf("[cpu] core floor %s -> %d (online=%d, hotplug still scales above up_threshold)",
		strings.TrimSpace(string(before)), hpsCoreFloor, coresOnline())
}

// coresTotal is how many cores the SoC has, online or parked. Reported so a
// "1 of 4 online" reads as a power state rather than a one-core device — which
// is how the MT8163's hotplug behaviour gets misread.
func coresTotal() int {
	b, err := os.ReadFile("/sys/devices/system/cpu/present")
	if err != nil {
		return 0
	}
	n := 0
	for _, part := range strings.Split(strings.TrimSpace(string(b)), ",") {
		lo, hi, found := strings.Cut(part, "-")
		a, err1 := strconv.Atoi(strings.TrimSpace(lo))
		if !found {
			if err1 == nil {
				n++
			}
			continue
		}
		z, err2 := strconv.Atoi(strings.TrimSpace(hi))
		if err1 == nil && err2 == nil && z >= a {
			n += z - a + 1
		}
	}
	return n
}
