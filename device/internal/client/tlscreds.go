package client

import (
	"crypto/tls"
	"crypto/x509"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/gorilla/websocket"
	"github.com/wilbowes/EchoMuse/internal/proto"
)

// Link credentials pushed by the controller (provisioning wizard or the
// dashboard "Secure link" action; paths shared with controller em_api.py
// DEVICE_TLS_DIR). They are re-read on every dial, so a push takes effect on
// the next reconnect.
const (
	DefaultCAPath    = "/data/local/etc/echomuse/ca.pem" // the only trusted root
	DefaultTokenPath = "/data/local/etc/echomuse/token"  // sent as X-EM-Token

	// tlsServerName is the DNS SAN of the controller certificate
	// (controller/em_pki.py TLS_SERVER_NAME): an identity label, not a
	// resolvable name; discovery supplies the address.
	tlsServerName = "echomuse-controller"
)

// BuildUnix is the firmware build time in Unix seconds, set by compile.sh. It
// floors the certificate-validity clock: an Echo can boot with a bogus date
// until NTP syncs, and that must not strand it off the network.
var BuildUnix = ""

type linkCreds struct {
	tlsConf *tls.Config // nil: no CA installed, plain ws
	token   string      // "": no token installed
}

// loadLinkCreds reads the credential files; absent files are the normal
// unprovisioned state.
func loadLinkCreds(caPath, tokenPath string) linkCreds {
	var creds linkCreds
	if pem, err := os.ReadFile(caPath); err == nil {
		pool := x509.NewCertPool()
		if pool.AppendCertsFromPEM(pem) {
			creds.tlsConf = &tls.Config{
				RootCAs:    pool,
				ServerName: tlsServerName,
				MinVersion: tls.VersionTLS12,
				Time:       tlsNow,
			}
		} else {
			log.Printf("[tls] %s holds no valid PEM certificate; ignoring it", caPath)
		}
	}
	if tok, err := os.ReadFile(tokenPath); err == nil {
		creds.token = strings.TrimSpace(string(tok))
	}
	return creds
}

// header returns the upgrade headers every socket carries (WIRE §1).
func (c linkCreds) header() http.Header {
	h := http.Header{}
	if c.token != "" {
		h.Set(proto.HeaderToken, c.token)
	}
	return h
}

// dialer pins the controller CA; compression stays disabled (WIRE §1).
func (c linkCreds) dialer() *websocket.Dialer {
	return &websocket.Dialer{HandshakeTimeout: 10 * time.Second, TLSClientConfig: c.tlsConf}
}

// tlsNow is the verification clock: never earlier than the build time.
func tlsNow() time.Time {
	now := time.Now()
	if sec, err := strconv.ParseInt(BuildUnix, 10, 64); err == nil {
		if bt := time.Unix(sec, 0); now.Before(bt) {
			return bt
		}
	}
	return now
}
