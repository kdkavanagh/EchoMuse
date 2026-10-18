//go:build !server

package companion

// The host build (go test, go vet) embeds no app and no plugin: the
// installers report that as their health errors and never run pm or call
// Kiosk Satellite.
var (
	apk      []byte
	version  string
	ksplugin []byte
)
