package companion

import (
	"encoding/binary"
	"fmt"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const procHeader = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"

// procAddr renders addr the way the kernel prints it in /proc/net/tcp
// (IPv4) or tcp6 (IPv6): 32-bit words in host byte order, then the port.
func procAddr(ap netip.AddrPort) string {
	raw := ap.Addr().AsSlice()
	var b strings.Builder
	for i := 0; i < len(raw); i += 4 {
		fmt.Fprintf(&b, "%08X", binary.NativeEndian.Uint32(raw[i:]))
	}
	return fmt.Sprintf("%s:%04X", b.String(), ap.Port())
}

func procRow(local, remote netip.AddrPort, state string, uid int) string {
	return fmt.Sprintf("   0: %s %s %s 00000000:00000000 00:00000000 00000000 %5d        0 4242 1 0000000000000000 20 4 30 10 -1\n",
		procAddr(local), procAddr(remote), state, uid)
}

func writeProc(t *testing.T, name string, rows ...string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte(procHeader+strings.Join(rows, "")), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestParseSocketAddr(t *testing.T) {
	for in, want := range map[string]string{
		procAddr(netip.MustParseAddrPort("127.0.0.1:8771")):           "127.0.0.1:8771",
		procAddr(netip.MustParseAddrPort("[::ffff:127.0.0.1]:41000")): "127.0.0.1:41000",
		procAddr(netip.MustParseAddrPort("[::1]:5")):                  "[::1]:5",
	} {
		got, err := parseSocketAddr(in)
		if err != nil || got.String() != want {
			t.Errorf("parseSocketAddr(%q) = %v, %v; want %s", in, got, err, want)
		}
	}
	// The kernel's own spelling on a little-endian device.
	if binary.NativeEndian.Uint16([]byte{1, 0}) == 1 {
		if got, err := parseSocketAddr("0100007F:2243"); err != nil || got.String() != "127.0.0.1:8771" {
			t.Errorf("0100007F:2243 = %v, %v", got, err)
		}
		if got, err := parseSocketAddr("0000000000000000FFFF00000100007F:A028"); err != nil || got.String() != "127.0.0.1:41000" {
			t.Errorf("mapped = %v, %v", got, err)
		}
	}
	for _, bad := range []string{"", "0100007F", "0100007:2243", "0100007F:XYZ", "GG00007F:2243"} {
		if _, err := parseSocketAddr(bad); err == nil {
			t.Errorf("parseSocketAddr(%q) accepted", bad)
		}
	}
}

// The far end's row is found by its local/remote pair in tcp or, for a
// dual-stack Java socket, as IPv4-mapped loopback in tcp6; the firmware's
// own row and lingering TIME_WAIT rows never match.
func TestPeerUID(t *testing.T) {
	server := netip.MustParseAddrPort("127.0.0.1:8771")
	app := netip.MustParseAddrPort("127.0.0.1:41000")
	mappedApp := netip.MustParseAddrPort("[::ffff:127.0.0.1]:41000")
	mappedServer := netip.MustParseAddrPort("[::ffff:127.0.0.1]:8771")
	own := procRow(server, app, "01", 0)

	tcp := writeProc(t, "tcp", own, procRow(app, server, "01", 10123))
	if uid, err := peerUID([]string{tcp}, server, app); err != nil || uid != 10123 {
		t.Errorf("tcp: uid %d, %v; want 10123", uid, err)
	}

	tcp4 := writeProc(t, "tcp", own)
	tcp6 := writeProc(t, "tcp6", procRow(mappedApp, mappedServer, "01", 10123))
	if uid, err := peerUID([]string{tcp4, tcp6}, server, app); err != nil || uid != 10123 {
		t.Errorf("tcp6 mapped: uid %d, %v; want 10123", uid, err)
	}

	stale := writeProc(t, "tcp", own, procRow(app, server, "06", 0), procRow(app, server, "01", 10123))
	if uid, err := peerUID([]string{stale}, server, app); err != nil || uid != 10123 {
		t.Errorf("TIME_WAIT row: uid %d, %v; want 10123", uid, err)
	}

	if _, err := peerUID([]string{writeProc(t, "tcp", own), filepath.Join(t.TempDir(), "missing")}, server, app); err == nil {
		t.Error("no row for the far end, yet a uid")
	}
}

func TestOwnerUID(t *testing.T) {
	uid, err := ownerUID(t.TempDir())
	if err != nil || uid != os.Getuid() {
		t.Fatalf("ownerUID = %d, %v; want %d", uid, err, os.Getuid())
	}
	if _, err := ownerUID(filepath.Join(t.TempDir(), "missing")); err == nil {
		t.Fatal("owner of a missing directory")
	}
}
