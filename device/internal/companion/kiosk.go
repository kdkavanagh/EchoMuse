package companion

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"strconv"
	"sync"
	"time"

	"github.com/wilbowes/EchoMuse/internal/proto"
)

const (
	// apiTimeout bounds one call to Kiosk Satellite's remote API; an
	// installPlugin took 1.07 s on the unit (plan §11.5 K0).
	apiTimeout = 30 * time.Second
	// maxAPIResponse bounds the response bodies read: listPlugins carries
	// every installed plugin's manifest.
	maxAPIResponse = 4 << 20
)

var (
	// errTokenRejected and errNoToken read as the health codes the
	// controller acts on (plan §11.2 decision 3).
	errTokenRejected = errors.New(proto.KioskTokenRejected)
	errNoToken       = errors.New(proto.KioskNoToken)
	// errNoKiosk: Kiosk Satellite is not installed. Not an error in
	// health, but it backs the checks off like one.
	errNoKiosk = errors.New("Kiosk Satellite is not installed")
)

// HTTPClient sends one request: an *http.Client on the device.
type HTTPClient interface {
	Do(*http.Request) (*http.Response, error)
}

// KioskInstallerConfig wires a KioskInstaller. Token and Port are the
// config's kioskToken and kioskPort at start. Zero ZIP/Version select the
// embedded plugin; Now nil selects time.Now.
type KioskInstallerConfig struct {
	Runner  Runner
	HTTP    HTTPClient
	Health  *Health
	Token   string
	Port    int
	ZIP     []byte
	Version string
	Now     func() time.Time
}

// KioskInstaller installs the embedded plugin into Kiosk Satellite by hash
// and keeps it enabled, over Kiosk Satellite's remote API on 127.0.0.1
// (plan §11.2 decisions 2, 3, 5): at start, when kioskToken or kioskPort
// changes, when the plugin has been unlinked for 30 s, and when it speaks
// another link or plugin version. Paced as the app's Installer; failures
// reach Health and the log only, never the caller. The token is never
// logged.
type KioskInstaller struct {
	cfg  KioskInstallerConfig
	hash string // sha256 of cfg.ZIP, as listPlugins reports it
	pacer

	settingsMu sync.Mutex
	token      string
	port       int
}

// NewKioskInstaller returns an installer whose first Run attempt is due at
// once.
func NewKioskInstaller(cfg KioskInstallerConfig) *KioskInstaller {
	if cfg.ZIP == nil {
		cfg.ZIP, cfg.Version = ksplugin, PluginVersion()
	}
	sum := sha256.Sum256(cfg.ZIP)
	k := &KioskInstaller{cfg: cfg, hash: hex.EncodeToString(sum[:]), token: cfg.Token, port: cfg.Port}
	k.pacer.init(cfg.Now)
	return k
}

// Run keeps the plugin installed until ctx ends. With no plugin embedded it
// records that in Health and returns at once.
func (k *KioskInstaller) Run(ctx context.Context) {
	if len(k.cfg.ZIP) == 0 || k.cfg.Version == "" {
		const msg = "this firmware embeds no Kiosk Satellite plugin"
		log.Printf("[kiosk] %s", msg)
		k.cfg.Health.setPluginError(msg)
		return
	}
	k.run(ctx, k.attempt)
}

// Configure takes the config's kioskToken and kioskPort. A change runs the
// check again, with the failure backoff reset: the dashboard has just
// stored a token, and its user is waiting for the plugin.
func (k *KioskInstaller) Configure(token string, port int) {
	k.settingsMu.Lock()
	changed := token != k.token || port != k.port
	k.token, k.port = token, port
	k.settingsMu.Unlock()
	if !changed {
		return
	}
	k.record(false)
	k.request(actEnsure)
}

// Check asks for the check to run (the plugin speaks another link or
// plugin version).
func (k *KioskInstaller) Check() { k.request(actEnsure) }

func (k *KioskInstaller) settings() (string, int) {
	k.settingsMu.Lock()
	defer k.settingsMu.Unlock()
	return k.token, k.port
}

// attempt runs one check and records its outcome.
func (k *KioskInstaller) attempt(ctx context.Context, _ action) {
	err := k.do(ctx)
	k.record(err != nil)
	switch {
	case err == nil, errors.Is(err, errNoKiosk):
		k.cfg.Health.setPluginError("")
	case ctx.Err() == nil:
		log.Printf("[kiosk] %v", err)
		k.cfg.Health.setPluginError(err.Error())
	}
}

// do is decision 2: Kiosk Satellite installed, its remote admin answering
// and a token configured, the plugin is installed when its record is
// missing or its hash differs from the embedded ZIP's, the master switch
// is turned on when off, and the plugin enabled when disabled.
func (k *KioskInstaller) do(ctx context.Context) error {
	token, port := k.settings()
	version, err := packageVersion(ctx, k.cfg.Runner, KioskPackage)
	if err != nil {
		return err
	}
	k.cfg.Health.setKioskPackage(version)
	if version == "" {
		return errNoKiosk
	}
	if err := k.remoteAdmin(ctx, port); err != nil {
		k.cfg.Health.setRemoteAdmin(false)
		return fmt.Errorf("remote administration on port %d: %w", port, err)
	}
	k.cfg.Health.setRemoteAdmin(true)
	if token == "" {
		return errNoToken
	}
	api := kioskAPI{http: k.cfg.HTTP, base: "http://127.0.0.1:" + strconv.Itoa(port), token: token}

	var plugins []pluginRecord
	if err := api.command(ctx, "listPlugins", struct{}{}, &plugins); err != nil {
		return err
	}
	rec := findPlugin(plugins)
	if rec != nil {
		k.cfg.Health.setPluginVersion(rec.Version)
	}
	if rec == nil || rec.SHA256 != k.hash {
		log.Printf("[kiosk] installing plugin %s", k.cfg.Version)
		params := installParams{Data: base64.StdEncoding.EncodeToString(k.cfg.ZIP), Trusted: true}
		if err := api.command(ctx, "installPlugin", params, nil); err != nil {
			return err
		}
		k.cfg.Health.setPluginVersion(k.cfg.Version)
	}

	var state pluginState
	if err := api.command(ctx, "getPluginState", struct{}{}, &state); err != nil {
		return err
	}
	if !state.Enabled {
		// It also resumes the other plugins already enabled (§11.6).
		log.Printf("[kiosk] turning Kiosk Satellite's plugins on")
		if err := api.command(ctx, "setPluginsEnabled", enabledParams{Enabled: true}, nil); err != nil {
			return err
		}
	}
	k.cfg.Health.setSwitchedOn(!state.Enabled)
	rec = findPlugin(state.Plugins)
	if rec == nil {
		return errors.New("getPluginState lists no " + PluginID + " plugin")
	}
	if !rec.Enabled {
		log.Printf("[kiosk] enabling plugin %s", rec.Version)
		return api.command(ctx, "enablePlugin", idParams{ID: PluginID}, nil)
	}
	return nil
}

// remoteAdmin checks that GET /api/health answers; it needs no token.
func (k *KioskInstaller) remoteAdmin(ctx context.Context, port int) error {
	ctx, cancel := context.WithTimeout(ctx, apiTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, "http://127.0.0.1:"+strconv.Itoa(port)+"/api/health", nil)
	if err != nil {
		return err
	}
	resp, err := k.cfg.HTTP.Do(req)
	if err != nil {
		return err
	}
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, maxAPIResponse))
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("GET /api/health: %s", resp.Status)
	}
	return nil
}

// kioskAPI is Kiosk Satellite's authenticated command API.
type kioskAPI struct {
	http  HTTPClient
	base  string
	token string
}

// command POSTs /api/commands/<name> with params and decodes the result's
// data into out (nil: ignored). A 401 is errTokenRejected; a refusal
// carries Kiosk Satellite's message.
func (a kioskAPI) command(ctx context.Context, name string, params, out any) error {
	body, err := json.Marshal(params)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(ctx, apiTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, a.base+"/api/commands/"+name, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+a.token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := a.http.Do(req)
	if err != nil {
		return fmt.Errorf("%s: %w", name, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusUnauthorized {
		return errTokenRejected
	}
	var r struct {
		OK    bool            `json:"ok"`
		Data  json.RawMessage `json:"data"`
		Error string          `json:"error"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, maxAPIResponse)).Decode(&r); err != nil {
		return fmt.Errorf("%s: %s: %w", name, resp.Status, err)
	}
	if !r.OK {
		if r.Error == "" {
			r.Error = resp.Status
		}
		return fmt.Errorf("%s: %s", name, r.Error)
	}
	if out != nil {
		if err := json.Unmarshal(r.Data, out); err != nil {
			return fmt.Errorf("%s: %w", name, err)
		}
	}
	return nil
}

// pluginRecord is one listPlugins / getPluginState plugins entry, as far as
// it is read here.
type pluginRecord struct {
	ID      string `json:"id"`
	Version string `json:"version"`
	Enabled bool   `json:"enabled"`
	SHA256  string `json:"sha256"`
}

// pluginState is getPluginState's data: the master switch and the plugins.
type pluginState struct {
	Enabled bool           `json:"enabled"`
	Plugins []pluginRecord `json:"plugins"`
}

type installParams struct {
	Data    string `json:"data"`
	Trusted bool   `json:"trusted"`
}

type enabledParams struct {
	Enabled bool `json:"enabled"`
}

type idParams struct {
	ID string `json:"id"`
}

func findPlugin(plugins []pluginRecord) *pluginRecord {
	for i := range plugins {
		if plugins[i].ID == PluginID {
			return &plugins[i]
		}
	}
	return nil
}
