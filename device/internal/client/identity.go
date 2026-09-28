package client

import (
	"log"
	"net"
	"os/exec"
	"strings"
)

// Version is the firmware version, set by compile.sh through -ldflags.
var Version = "dev"

// GetSerialNo returns ro.serialno, the stable endpoint identifier.
func GetSerialNo() string {
	out, err := exec.Command("getprop", "ro.serialno").Output()
	if err != nil {
		log.Printf("[client] could not read ro.serialno: %v", err)
		return "unknown-device"
	}
	serial := strings.TrimSpace(string(out))
	if serial == "" {
		return "unknown-device"
	}
	return serial
}

// LocalIP returns the address of the interface that routes off-host, or ""
// when there is none. It is resolved on every call because a Wi-Fi change
// moves it.
func LocalIP() string {
	conn, err := net.Dial("udp", "8.8.8.8:80")
	if err != nil {
		return ""
	}
	defer conn.Close()
	ip := conn.LocalAddr().(*net.UDPAddr).IP.String()
	if i := strings.IndexByte(ip, '%'); i >= 0 {
		ip = ip[:i]
	}
	return ip
}
