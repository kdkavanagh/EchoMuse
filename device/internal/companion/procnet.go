package companion

import (
	"bufio"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"net/netip"
	"os"
	"strconv"
	"strings"
	"syscall"
)

// ProcNetFiles are the kernel's TCP socket tables. Android's Java sockets
// may be dual-stack, which puts the app's end in tcp6 as ::ffff:127.0.0.1
// (plan decision 6).
var ProcNetFiles = []string{"/proc/net/tcp", "/proc/net/tcp6"}

// tcpTimeWait is a closed socket's lingering row (st 06): it belongs to no
// process and carries uid 0.
const tcpTimeWait = "06"

var errNoRow = errors.New("no socket row")

// ownerUID is the uid owning path: for an app's data directory, the app's.
func ownerUID(path string) (int, error) {
	fi, err := os.Stat(path)
	if err != nil {
		return 0, err
	}
	st, ok := fi.Sys().(*syscall.Stat_t)
	if !ok {
		return 0, fmt.Errorf("%s: no owner", path)
	}
	return int(st.Uid), nil
}

// peerUID is the uid owning the far end of an accepted loopback connection:
// the row, in any of files, whose local address is the connection's remote
// one and whose remote address is its local one. IPv4-mapped IPv6 rows
// match their IPv4 addresses.
func peerUID(files []string, local, remote netip.AddrPort) (int, error) {
	local = netip.AddrPortFrom(local.Addr().Unmap(), local.Port())
	remote = netip.AddrPortFrom(remote.Addr().Unmap(), remote.Port())
	for _, path := range files {
		uid, err := findRow(path, remote, local)
		if err == nil {
			return uid, nil
		}
		if !errors.Is(err, errNoRow) && !errors.Is(err, os.ErrNotExist) {
			return 0, err
		}
	}
	return 0, fmt.Errorf("%w for %s → %s", errNoRow, remote, local)
}

// findRow scans one /proc/net/tcp{,6} table for the live socket bound to
// local and connected to remote, and returns its uid.
func findRow(path string, local, remote netip.AddrPort) (int, error) {
	f, err := os.Open(path)
	if err != nil {
		return 0, err
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Scan() // header
	for sc.Scan() {
		// sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid ...
		fields := strings.Fields(sc.Text())
		if len(fields) < 8 || fields[3] == tcpTimeWait {
			continue
		}
		l, err := parseSocketAddr(fields[1])
		if err != nil || l != local {
			continue
		}
		r, err := parseSocketAddr(fields[2])
		if err != nil || r != remote {
			continue
		}
		uid, err := strconv.Atoi(fields[7])
		if err != nil {
			return 0, fmt.Errorf("%s: uid %q: %w", path, fields[7], err)
		}
		return uid, nil
	}
	if err := sc.Err(); err != nil {
		return 0, err
	}
	return 0, errNoRow
}

// parseSocketAddr decodes a /proc/net/tcp{,6} address, "0100007F:2243" or
// "0000000000000000FFFF00000100007F:2243": the address as 32-bit words,
// each printed in host byte order, then the port in hex. An IPv4-mapped
// address is returned as IPv4.
func parseSocketAddr(s string) (netip.AddrPort, error) {
	addrHex, portHex, ok := strings.Cut(s, ":")
	if !ok || (len(addrHex) != 8 && len(addrHex) != 32) {
		return netip.AddrPort{}, fmt.Errorf("socket address %q", s)
	}
	port, err := strconv.ParseUint(portHex, 16, 16)
	if err != nil {
		return netip.AddrPort{}, fmt.Errorf("socket address %q: %w", s, err)
	}
	raw, err := hex.DecodeString(addrHex)
	if err != nil {
		return netip.AddrPort{}, fmt.Errorf("socket address %q: %w", s, err)
	}
	for i := 0; i < len(raw); i += 4 {
		binary.NativeEndian.PutUint32(raw[i:], binary.BigEndian.Uint32(raw[i:]))
	}
	addr, _ := netip.AddrFromSlice(raw)
	return netip.AddrPortFrom(addr.Unmap(), uint16(port)), nil
}
