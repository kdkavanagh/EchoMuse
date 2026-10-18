//go:build server

package companion

import _ "embed"

// apk and version are the companion app this firmware installs, ksplugin
// the Kiosk Satellite plugin ZIP. The firmware image build copies them into
// this directory from its companion stage before `go build -tags server`
// (docs/echo-show-companion-plan.md §4, §11.4).
var (
	//go:embed app.apk
	apk []byte
	//go:embed app.version
	version string
	//go:embed ksplugin.zip
	ksplugin []byte
)
