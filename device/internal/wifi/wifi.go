// Package wifi implements safe WiFi network changes with automatic
// rollback, plus scan/status queries for the dashboard Connectivity tab.
//
//   - The config is a FULL replacement of wpa_supplicant.conf with a
//     single network block — no ambiguity about which AP it joins.
//   - wpa_cli needs BOTH -p /data/misc/wifi/sockets (non-default socket
//     dir) and -i wlan0.
//
// This package runs inside the root Go binary, so file writes use plain
// os.WriteFile. Ownership must still be restored to wifi:wifi
// (AID_WIFI=1010) mode 0660 or the supplicant can't read the config.
//
// Safety model (the connection to the controller dies mid-change, so the
// device owns the whole sequence):
//
//  1. Back up the current conf and drop a pending marker file.
//  2. Write the new conf and reload it into the supplicant (reloadConf).
//  3. Gates: associate to the TARGET SSID ≤45s → IPv4 on wlan0 ≤20s →
//     control WebSocket re-registered ≤90s. Any failure → restore the
//     backup the same way and report the failure once the connection
//     returns.
//  4. On success the controller sends wifi_commit, which deletes the
//     marker + backup. Until then the change is provisional.
//  5. Crash safety: if the marker exists at process start, a previous
//     switch never got committed — RecoverIfPending restores the backup
//     and reloads it, so a crash or power cycle mid-switch self-heals back
//     to the old network (same philosophy as the A/B binary slots).
//
// Fire OS 6 has no Android framework, and start_server.sh stops wifisvc
// before it powers the radio. radioUp powers the WLAN core and starts the
// supplicant, which reads the conf straight off disk, so reloadConf writes
// the conf, runs `wpa_cli reconfigure` (+ reassociate if that alone does
// not force a fresh association), then DHCP through the init service
// wifisvc itself starts. Its network HAL (libacehal_network.so) does
// `ctl.start dhcpcd-<iface>`, i.e. dhcpcd-wlan0 (/init.mt8163_amazon.rc:270,
// `/system/bin/dhcpcd wlan0 -AdLK`). The AOSP-style dhcpcd_wlan0
// (/init.mt8163.rc:986, `dhcpcd -BK -dd`) carries no interface: AOSP
// appends one as `ctl.start dhcpcd_wlan0:wlan0`, which this init refuses
// ("no such service"), and started bare it exits 1 within 60 ms
// (`control_start: No such file or directory`). dhcpcd-wlan0 leased in
// under a second and stays running, renewing the lease itself; its hooks
// set dhcp.wlan0.*, and names resolve through netmgrd's dnsproxyd (all on
// hardware, 2026-10-07). -K skips carrier watching, so it never notices a
// new association on its own: runDHCP restarts it on every join.
// netd is not running on this image, so there is nothing for `ndc` to
// talk to.
package wifi

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"os"
	"os/exec"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	confPath   = "/data/misc/wifi/wpa_supplicant.conf"
	backupPath = "/data/misc/wifi/wpa_supplicant.conf.echomuse-bak"
	markerPath = "/data/local/tmp/echomuse_wifi_pending"

	wpaSockDir = "/data/misc/wifi/sockets"
	iface      = "wlan0"

	// AID_WIFI — fixed uid/gid on Android; the supplicant reads the conf
	// as this user.
	aidWifi = 1010

	// 20s proved too tight on hardware for a network the supplicant
	// hasn't joined before — its scan cycle alone can eat most of it.
	// Reverts re-associate to a known network well inside 20s, so only
	// first-join pays the longer wait.
	associateTimeout = 45 * time.Second
	ipTimeout        = 20 * time.Second
	// The reconnect gate covers mDNS rediscovery plus the control client's
	// 5s retry cadence; generous because a false negative reverts a
	// perfectly good network change.
	reconnectTimeout = 90 * time.Second
)

// DHCP (package doc). dhcpServiceTimeout bounds init stopping a
// prior run; the lease itself is bounded by ipTimeout and judged by an IPv4
// address on wlan0, never by init state: a dhcpcd that exits at once still
// reads "running" for the moment it lives (57 ms on hardware).
const (
	dhcpService        = "dhcpcd-wlan0"
	dhcpServiceTimeout = 5 * time.Second

	// What wifisvc did before start_server.sh stops it at boot, observed on
	// hardware: MediaTek's libhardware_legacy wifi_load_driver powers the
	// WLAN core by writing /dev/wmtWifi (wlan0 appeared within 1 s), then
	// starts a supplicant. The wlan0-only wpa_supplicant service is used,
	// not p2p_supplicant: nothing here uses Wi-Fi Direct. Its control
	// socket answered ~3 s after start.
	wmtWifiPath       = "/dev/wmtWifi"
	supplicantService = "wpa_supplicant"
	radioTimeout      = 10 * time.Second
)

// Result is the outcome of a change attempt, reported to the controller
// as a wifi_result message once a connection exists to carry it.
type Result struct {
	OK    bool
	SSID  string
	Error string // empty on success
}

// Network is one scan result row.
type Network struct {
	SSID   string `json:"ssid"`
	Signal int    `json:"signal"`
}

type marker struct {
	NewSSID   string `json:"newSsid"`
	StartedAt int64  `json:"startedAt"`
}

var (
	mu       sync.Mutex
	inFlight bool
	// pending holds an unacknowledged Result until the controller's
	// wifi_commit clears it (see PendingResult).
	pending *Result
)

// ─── Queries ──────────────────────────────────────────────────────────────────

func wpaCli(args ...string) (string, error) {
	full := append([]string{"-p", wpaSockDir, "-i", iface}, args...)
	out, err := exec.Command("wpa_cli", full...).CombinedOutput()
	return string(out), err
}

// CurrentSSID returns the associated SSID, or "" when not associated.
func CurrentSSID() string {
	out, _ := wpaCli("status")
	if !strings.Contains(out, "wpa_state=COMPLETED") {
		return ""
	}
	for _, line := range strings.Split(out, "\n") {
		if v, ok := strings.CutPrefix(strings.TrimSpace(line), "ssid="); ok {
			return v
		}
	}
	return ""
}

// currentIPv4 returns the interface's IPv4 address, or "".
func currentIPv4() string {
	ifi, err := net.InterfaceByName(iface)
	if err != nil {
		return ""
	}
	addrs, err := ifi.Addrs()
	if err != nil {
		return ""
	}
	for _, a := range addrs {
		if ipn, ok := a.(*net.IPNet); ok {
			if v4 := ipn.IP.To4(); v4 != nil {
				return v4.String()
			}
		}
	}
	return ""
}

// Scan triggers a wpa_cli scan and returns networks sorted strongest
// first, deduped by SSID (strongest AP wins — multiple APs/bands share
// SSIDs). Safe while associated; expect a brief audio-free RF glitch.
func Scan() ([]Network, error) {
	if _, err := wpaCli("scan"); err != nil {
		return nil, fmt.Errorf("scan trigger: %w", err)
	}
	time.Sleep(4 * time.Second)
	out, err := wpaCli("scan_results")
	if err != nil {
		return nil, fmt.Errorf("scan_results: %w", err)
	}

	best := map[string]int{}
	for _, line := range strings.Split(out, "\n") {
		// bssid \t frequency \t signal \t flags \t ssid
		parts := strings.Split(line, "\t")
		if len(parts) < 5 {
			continue
		}
		ssid := strings.TrimSpace(parts[4])
		if ssid == "" || ssid == "SSID" {
			continue
		}
		// Hidden networks: wpa_cli prints the zeroed SSID bytes as literal
		// \xNN escapes (e.g. \x00\x00…). Unjoinable by name — drop them.
		if hiddenSSID.MatchString(ssid) {
			continue
		}
		sig, err := strconv.Atoi(strings.TrimSpace(parts[2]))
		if err != nil {
			continue
		}
		if cur, ok := best[ssid]; !ok || sig > cur {
			best[ssid] = sig
		}
	}
	nets := make([]Network, 0, len(best))
	for ssid, sig := range best {
		nets = append(nets, Network{SSID: ssid, Signal: sig})
	}
	sort.Slice(nets, func(i, j int) bool { return nets[i].Signal > nets[j].Signal })
	return nets, nil
}

// ─── Change with rollback ─────────────────────────────────────────────────────

// hiddenSSID matches scan_results entries that are entirely \xNN escape
// sequences — wpa_cli's rendering of hidden/zeroed SSIDs.
var hiddenSSID = regexp.MustCompile(`^(\\x[0-9a-fA-F]{2})+$`)

// validCred matches wpaConfEscape in the provisioning wizard: a literal
// " or \ can't be represented safely in a wpa_supplicant.conf quoted
// string, so reject rather than mis-escape.
var validCred = regexp.MustCompile(`["\\]`)

func validate(ssid, psk string) error {
	if ssid == "" {
		return fmt.Errorf("empty SSID")
	}
	if validCred.MatchString(ssid) || validCred.MatchString(psk) {
		return fmt.Errorf("SSID/passphrase contains a double-quote or backslash, which wpa_supplicant.conf cannot represent safely")
	}
	if psk != "" && (len(psk) < 8 || len(psk) > 63) {
		return fmt.Errorf("WPA passphrase must be 8–63 characters (got %d)", len(psk))
	}
	return nil
}

func getprop(key, fallback string) string {
	out, err := exec.Command("getprop", key).Output()
	if err != nil {
		return fallback
	}
	if v := strings.TrimSpace(string(out)); v != "" {
		return v
	}
	return fallback
}

// setprop sets an Android system property — the counterpart of
// `stop`/`start` in start_server.sh, used here to start and stop the
// device's own init services (the supplicant, dhcpService) instead of
// hand-rolled invocations.
func setprop(key, value string) error {
	out, err := exec.Command("setprop", key, value).CombinedOutput()
	if err != nil {
		return fmt.Errorf("setprop %s %s: %v (%s)", key, value, err, strings.TrimSpace(string(out)))
	}
	return nil
}

// svcState reads getprop init.svc.<name> — "running", "stopped",
// "stopping", or "" if the service was never started this boot.
func svcState(name string) string {
	return getprop("init.svc."+name, "")
}

// composeConf builds the full-replacement wpa_supplicant.conf — the same
// template the provisioning wizard writes. An empty psk produces an open
// (key_mgmt=NONE) network block.
func composeConf(ssid, psk string) string {
	network := []string{
		"network={",
		fmt.Sprintf("\tssid=%q", ssid),
	}
	if psk == "" {
		network = append(network, "\tkey_mgmt=NONE")
	} else {
		network = append(network,
			fmt.Sprintf("\tpsk=%q", psk),
			"\tkey_mgmt=WPA-PSK",
		)
	}
	network = append(network, "\tpriority=1", "}")

	lines := []string{
		"ctrl_interface=" + wpaSockDir,
		"driver_param=use_p2p_group_interface=1",
		"update_config=1",
		"device_name=" + getprop("ro.product.name", "echomuse"),
		"manufacturer=" + getprop("ro.product.manufacturer", "Amazon"),
		"model_name=" + getprop("ro.product.model", "AEOBC"),
		"model_number=" + getprop("ro.product.model", "AEOBC"),
		"serial_number=" + getprop("ro.serialno", getprop("ro.boot.serialno", "unknown")),
		"device_type=1-0050F204-9",
		"os_version=01020300",
		"config_methods=physical_display virtual_push_button",
		"p2p_no_group_iface=1",
		"external_sim=1",
		"wowlan_triggers=disconnect",
	}
	lines = append(lines, network...)
	return strings.Join(lines, "\n") + "\n"
}

func writeConf(content string) error {
	// Traverse bit on the dir — 666 here made every file inside
	// unopenable (provisioning finding).
	_ = os.Chmod("/data/misc/wifi", 0o770)
	if err := os.WriteFile(confPath, []byte(content), 0o660); err != nil {
		return fmt.Errorf("write %s: %w", confPath, err)
	}
	if err := os.Chown(confPath, aidWifi, aidWifi); err != nil {
		return fmt.Errorf("chown %s: %w", confPath, err)
	}
	return os.Chmod(confPath, 0o660)
}

// reloadConf swaps in a new wpa_supplicant.conf and gets it joined: there
// is no framework to race (see the package doc), so the conf is written
// first and reconfigure does the reload and DHCP.
func reloadConf(content string) error {
	if err := writeConf(content); err != nil {
		return err
	}
	return reconfigure()
}

// reconfigure reloads wpa_supplicant's config from disk and runs DHCP.
func reconfigure() error {
	if err := radioUp(); err != nil {
		return err
	}
	if _, err := wpaCli("reconfigure"); err != nil {
		return fmt.Errorf("wpa_cli reconfigure: %w", err)
	}
	if !waitFor("association after reconfigure", associateTimeout, associated) {
		// reconfigure does not always force a fresh association cycle by
		// itself (the conf's one network block may be unchanged from the
		// supplicant's point of view, e.g. identical SSID/PSK) — nudge it
		// before giving up.
		if _, err := wpaCli("reassociate"); err != nil {
			return fmt.Errorf("wpa_cli reassociate: %w", err)
		}
		if !waitFor("association after reassociate", associateTimeout, associated) {
			return fmt.Errorf("did not associate within %s", associateTimeout)
		}
	}
	_, err := runDHCP()
	return err
}

// runDHCP (re)starts dhcpService and waits up to ipTimeout for its
// lease, returning the address. A running copy is stopped first: -K means it
// would not notice the new association. init stops it with SIGKILL, so the
// old lease's address and routes stay on wlan0 (hardware); they are flushed
// before the restart, or a stale address would pass for the new lease.
func runDHCP() (string, error) {
	if st := svcState(dhcpService); st != "" && st != "stopped" {
		if err := setprop("ctl.stop", dhcpService); err != nil {
			return "", err
		}
		if !waitFor(dhcpService+" to stop", dhcpServiceTimeout,
			func() bool { return svcState(dhcpService) == "stopped" }) {
			return "", fmt.Errorf("%s stuck %q — could not restart it", dhcpService, svcState(dhcpService))
		}
	}
	if currentIPv4() != "" {
		if out, err := exec.Command("ifconfig", iface, "0.0.0.0").CombinedOutput(); err != nil {
			return "", fmt.Errorf("flush %s IPv4: %v (%s)", iface, err, strings.TrimSpace(string(out)))
		}
	}
	if err := setprop("ctl.start", dhcpService); err != nil {
		return "", err
	}
	if !waitFor("DHCP lease", ipTimeout, func() bool { return currentIPv4() != "" }) {
		return "", fmt.Errorf("no IPv4 address on %s within %s (%s is %q)", iface, ipTimeout, dhcpService, svcState(dhcpService))
	}
	return currentIPv4(), nil
}

// radioUp powers the WLAN core and starts the supplicant unless
// either is already up (wifisvc may have got there first on a later
// restart of the firmware). Idempotent.
func radioUp() error {
	present := func() bool { _, err := net.InterfaceByName(iface); return err == nil }
	if !present() {
		if err := os.WriteFile(wmtWifiPath, []byte("1"), 0); err != nil {
			return fmt.Errorf("power on WLAN core (%s): %w", wmtWifiPath, err)
		}
		if !waitFor(iface+" to appear", radioTimeout, present) {
			return fmt.Errorf("%s did not appear after powering the WLAN core", iface)
		}
	}
	if svcState(supplicantService) != "running" && svcState("p2p_supplicant") != "running" {
		if err := setprop("ctl.start", supplicantService); err != nil {
			return err
		}
	}
	if !waitFor("supplicant control socket", radioTimeout, func() bool { _, err := wpaCli("status"); return err == nil }) {
		return fmt.Errorf("%s did not answer on %s", supplicantService, wpaSockDir)
	}
	return nil
}

func waitFor(what string, timeout time.Duration, cond func() bool) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if cond() {
			return true
		}
		time.Sleep(time.Second)
	}
	log.Printf("[wifi] timed out waiting for %s (%s)", what, timeout)
	return false
}

func associated() bool {
	out, _ := wpaCli("status")
	return strings.Contains(out, "wpa_state=COMPLETED")
}

// associatedTo reports association specifically to the named network —
// bare wpa_state=COMPLETED is satisfied by the *old* network if the
// supplicant never actually restarted.
func associatedTo(ssid string) bool {
	return CurrentSSID() == ssid
}

// waitForAssociation polls for association to ssid, logging the raw
// supplicant state every 5s so a timeout in the field says what the
// supplicant was doing (SCANNING vs 4WAY_HANDSHAKE vs INTERFACE_DISABLED).
func waitForAssociation(ssid string, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	lastDiag := time.Now()
	for time.Now().Before(deadline) {
		if associatedTo(ssid) {
			return true
		}
		if time.Since(lastDiag) >= 5*time.Second {
			out, _ := wpaCli("status")
			state := "?"
			for _, line := range strings.Split(out, "\n") {
				if v, ok := strings.CutPrefix(strings.TrimSpace(line), "wpa_state="); ok {
					state = v
					break
				}
			}
			log.Printf("[wifi] waiting for association to %q — wpa_state=%s", ssid, state)
			lastDiag = time.Now()
		}
		time.Sleep(time.Second)
	}
	log.Printf("[wifi] timed out waiting for association to %q (%s)", ssid, timeout)
	return false
}

func setResult(r Result) {
	mu.Lock()
	pending = &r
	mu.Unlock()
}

// PendingResult returns the unacknowledged change outcome, if any,
// WITHOUT clearing it. Delivery is at-least-once: the result stays
// pending (and is re-sent on reconnect and on a retry ticker) until the
// controller acks with wifi_commit — a fire-and-forget send can vanish
// into a half-open TCP connection that the interface bounce killed but
// that still looks connected to the writer (seen on hardware 2026-07-11).
func PendingResult() *Result {
	mu.Lock()
	defer mu.Unlock()
	if pending == nil {
		return nil
	}
	r := *pending
	return &r
}

// Commit handles the controller's wifi_commit ack: the provisional state
// (marker + backup) is deleted so a future crash/restart keeps the new
// network, and the pending result stops being re-sent. The controller
// acks failure results too — the revert already removed marker/backup,
// so the removes are harmless no-ops there.
func Commit() {
	_ = os.Remove(markerPath)
	_ = os.Remove(backupPath)
	mu.Lock()
	pending = nil
	mu.Unlock()
	log.Println("[wifi] result acknowledged — backup and pending marker removed")
}

// Change switches to a new network with automatic rollback. Runs
// synchronously (call from a goroutine); connected must report whether
// the control WebSocket is currently registered with the controller.
// The outcome lands in PendingResult either way.
func Change(ssid, psk string, connected func() bool) {
	mu.Lock()
	if inFlight {
		mu.Unlock()
		setResult(Result{OK: false, SSID: ssid, Error: "another WiFi change is already in progress"})
		return
	}
	inFlight = true
	pending = nil
	mu.Unlock()
	defer func() {
		mu.Lock()
		inFlight = false
		mu.Unlock()
	}()

	if err := validate(ssid, psk); err != nil {
		setResult(Result{OK: false, SSID: ssid, Error: err.Error()})
		return
	}

	log.Printf("[wifi] change requested → %q", ssid)

	old, err := os.ReadFile(confPath)
	if err != nil {
		setResult(Result{OK: false, SSID: ssid, Error: fmt.Sprintf("cannot read current config: %v", err)})
		return
	}
	if err := os.WriteFile(backupPath, old, 0o600); err != nil {
		setResult(Result{OK: false, SSID: ssid, Error: fmt.Sprintf("cannot write backup: %v", err)})
		return
	}
	mk, _ := json.Marshal(marker{NewSSID: ssid, StartedAt: time.Now().Unix()})
	if err := os.WriteFile(markerPath, mk, 0o600); err != nil {
		setResult(Result{OK: false, SSID: ssid, Error: fmt.Sprintf("cannot write pending marker: %v", err)})
		return
	}

	revert := func(reason string) {
		log.Printf("[wifi] change to %q failed (%s) — reverting", ssid, reason)
		// Restore the same way (see reloadConf) — but if the restore
		// write fails, conf is beyond self-healing: leave the marker so
		// RecoverIfPending retries on next start.
		restoreErr := reloadConf(string(old))
		if restoreErr != nil {
			log.Printf("[wifi] REVERT FAILED: %v — marker left for recovery on restart", restoreErr)
		} else {
			_ = os.Remove(markerPath)
			_ = os.Remove(backupPath)
		}
		waitFor("re-association after revert", associateTimeout, associated)
		setResult(Result{OK: false, SSID: ssid, Error: reason})
	}

	if err := reloadConf(composeConf(ssid, psk)); err != nil {
		revert(err.Error())
		return
	}

	if !waitForAssociation(ssid, associateTimeout) {
		revert(fmt.Sprintf("did not associate to %q within %s (wrong passphrase or AP out of range?)", ssid, associateTimeout))
		return
	}
	log.Printf("[wifi] associated to %q", ssid)

	if !waitFor("IPv4 address", ipTimeout, func() bool { return currentIPv4() != "" }) {
		revert(fmt.Sprintf("associated to %q but no IP within %s (DHCP problem?)", ssid, ipTimeout))
		return
	}
	log.Printf("[wifi] got IP %s", currentIPv4())

	if !waitFor("controller reconnect", reconnectTimeout, connected) {
		revert(fmt.Sprintf("joined %q (IP %s) but could not reach the controller within %s — wrong VLAN or isolated network?", ssid, currentIPv4(), reconnectTimeout))
		return
	}

	// Connected on the new network. Marker + backup stay until the
	// controller acknowledges with wifi_commit.
	log.Printf("[wifi] change to %q succeeded — awaiting commit from controller", ssid)
	setResult(Result{OK: true, SSID: ssid})
}

// RecoverIfPending restores the pre-change config if a previous change
// never got committed (crash, power cycle, or a failed revert). Call
// once at process start, before the control client runs.
func RecoverIfPending() {
	mk, err := os.ReadFile(markerPath)
	if err != nil {
		return // no pending change — the normal case
	}
	var m marker
	_ = json.Unmarshal(mk, &m)
	log.Printf("[wifi] uncommitted change to %q found at startup — restoring previous network", m.NewSSID)

	backup, err := os.ReadFile(backupPath)
	if err != nil {
		// Marker without backup: the change already reverted its conf but
		// couldn't remove the marker, or the backup was lost. Nothing to
		// restore from — clear the marker and carry on with whatever conf
		// is in place.
		log.Printf("[wifi] no backup to restore (%v) — clearing marker", err)
		_ = os.Remove(markerPath)
		return
	}
	if err := reloadConf(string(backup)); err != nil {
		log.Printf("[wifi] startup restore failed: %v — leaving marker for next start", err)
		return
	}
	_ = os.Remove(markerPath)
	_ = os.Remove(backupPath)
	setResult(Result{OK: false, SSID: m.NewSSID, Error: "device restarted before the change was confirmed — previous network restored"})
}

// EnsureUp brings WiFi up at boot from the already-saved conf: nothing else
// does this once wifisvc is stopped (it was wifisvc's job on every previous
// boot). The supplicant keeps the saved network blocks from the last
// successful Change; it may already be associating by the time this runs,
// but nothing else runs its DHCP. Call once at startup, after
// RecoverIfPending.
func EnsureUp() {
	if err := radioUp(); err != nil {
		log.Printf("[wifi] boot radio bring-up failed: %v", err)
		return
	}
	if !associated() {
		if _, err := wpaCli("reconnect"); err != nil {
			log.Printf("[wifi] boot reconnect failed: %v", err)
		}
		if !waitFor("association at boot", associateTimeout, associated) {
			log.Println("[wifi] no saved network associated at boot")
			return
		}
	} else if ip := currentIPv4(); ip != "" {
		// A firmware restart: dhcpService outlives us and keeps the lease.
		log.Printf("[wifi] already joined to %q with %s", CurrentSSID(), ip)
		return
	}
	ip, err := runDHCP()
	if err != nil {
		log.Printf("[wifi] boot DHCP failed: %v", err)
		return
	}
	log.Printf("[wifi] joined %q at boot, leased %s", CurrentSSID(), ip)
}
