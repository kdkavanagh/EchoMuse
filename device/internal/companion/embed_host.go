//go:build !server

package companion

// The host build (go test, go vet) embeds no app: the installer reports
// that as its health error and never runs pm.
var (
	apk     []byte
	version string
)
