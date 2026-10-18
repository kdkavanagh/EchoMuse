package companion

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

const (
	testToken     = "tok-secret"
	testPlugin    = "1.0.0-aaaaaaaaaaaa"
	kioskDump     = "Packages:\n  Package [me.jxl.kiosk_satellite] (1a2b3c):\n    versionCode=90 minSdk=26 targetSdk=34\n    versionName=2026.10.28\n"
	kioskNotFound = "Unable to find package: me.jxl.kiosk_satellite\n"
)

var testZIP = []byte("PK the plugin zip")

func zipHash(b []byte) string {
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

// fakeKiosk is Kiosk Satellite's remote API on 127.0.0.1:2324, as far as
// the installer uses it (PluginBridge.kt, plugin_manager.dart): commands
// answer {ok, data} or {ok: false, error}, a bad token 401.
type fakeKiosk struct {
	mu sync.Mutex
	kioskState
	calls     []string
	installed []byte // the last ZIP installPlugin took
}

type kioskState struct {
	down    bool   // remote admin not answering
	token   string // the token it accepts
	master  bool   // the plugin master switch
	plugins []pluginRecord
	refuse  string // installPlugin's refusal, "" to install
}

func (f *fakeKiosk) Do(req *http.Request) (*http.Response, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if req.URL.Host != "127.0.0.1:2324" {
		return nil, errors.New("dial " + req.URL.Host + ": connection refused")
	}
	if req.Method == http.MethodGet && req.URL.Path == "/api/health" {
		f.calls = append(f.calls, "health")
		if f.down {
			return nil, errors.New("dial tcp 127.0.0.1:2324: connect: connection refused")
		}
		return respond(http.StatusOK, `{"uptime":{"app":12}}`), nil
	}
	name, ok := strings.CutPrefix(req.URL.Path, "/api/commands/")
	if req.Method != http.MethodPost || !ok {
		return respond(http.StatusNotFound, "not found"), nil
	}
	f.calls = append(f.calls, name)
	if req.Header.Get("Authorization") != "Bearer "+f.token {
		return respond(http.StatusUnauthorized, `{"error":"unauthorized"}`), nil
	}
	var p struct {
		Data    string `json:"data"`
		Trusted bool   `json:"trusted"`
		Enabled *bool  `json:"enabled"`
		ID      string `json:"id"`
	}
	b, _ := io.ReadAll(req.Body)
	if err := json.Unmarshal(b, &p); err != nil {
		return respond(http.StatusBadRequest, `{"ok":false,"error":"bad params"}`), nil
	}
	ok200 := func(data any) (*http.Response, error) {
		body, _ := json.Marshal(map[string]any{"ok": true, "data": data})
		return respond(http.StatusOK, string(body)), nil
	}
	switch name {
	case "listPlugins":
		return ok200(f.plugins)
	case "getPluginState":
		return ok200(map[string]any{"enabled": f.master, "plugins": f.plugins})
	case "installPlugin":
		zip, err := base64.StdEncoding.DecodeString(p.Data)
		if err != nil || !p.Trusted {
			return respond(http.StatusBadRequest, `{"ok":false,"error":"Confirm that you trust the plugin author"}`), nil
		}
		if f.refuse != "" {
			body, _ := json.Marshal(map[string]any{"ok": false, "error": f.refuse})
			return respond(http.StatusBadRequest, string(body)), nil
		}
		f.installed = zip
		rec := pluginRecord{ID: PluginID, Version: testPlugin, SHA256: zipHash(zip)}
		if i := slices.IndexFunc(f.plugins, func(r pluginRecord) bool { return r.ID == PluginID }); i >= 0 {
			rec.Enabled = f.plugins[i].Enabled // a replacement keeps the enabled state
			f.plugins[i] = rec
		} else {
			f.plugins = append(f.plugins, rec) // a new plugin starts disabled
		}
		return ok200(f.plugins)
	case "setPluginsEnabled":
		if p.Enabled == nil {
			return respond(http.StatusBadRequest, `{"ok":false,"error":"Missing enabled flag"}`), nil
		}
		f.master = *p.Enabled
		return ok200(map[string]any{"enabled": f.master, "plugins": f.plugins})
	case "enablePlugin":
		i := slices.IndexFunc(f.plugins, func(r pluginRecord) bool { return r.ID == p.ID })
		if i < 0 {
			return respond(http.StatusBadRequest, `{"ok":false,"error":"Plugin is not installed"}`), nil
		}
		f.plugins[i].Enabled = true
		return ok200(f.plugins)
	}
	return respond(http.StatusBadRequest, `{"ok":false,"error":"Unknown command"}`), nil
}

func respond(status int, body string) *http.Response {
	return &http.Response{StatusCode: status, Status: http.StatusText(status), Body: io.NopCloser(strings.NewReader(body))}
}

func (f *fakeKiosk) commands() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return slices.Clone(f.calls)
}

func newTestKiosk(r *fakeRunner, api *fakeKiosk, token string) (*KioskInstaller, *Health) {
	h := &Health{}
	return NewKioskInstaller(KioskInstallerConfig{Runner: r, HTTP: api, Health: h, Token: token, Port: 2324,
		ZIP: testZIP, Version: testPlugin}), h
}

// Decision 2, 3 and 5: the plugin is installed exactly when its record is
// missing or its hash differs from the embedded ZIP's; the master switch is
// turned on only when off, the record enabled only when disabled; a 401 is
// token_rejected, no token no_token; Kiosk Satellite absent or its remote
// admin down stops the check early.
func TestKioskInstallerDecisions(t *testing.T) {
	current := pluginRecord{ID: PluginID, Version: testPlugin, Enabled: true, SHA256: zipHash(testZIP)}
	older := pluginRecord{ID: PluginID, Version: "1.0.0-bbbbbbbbbbbb", Enabled: true, SHA256: zipHash([]byte("older"))}
	other := pluginRecord{ID: "clock", Version: "2.0.0", Enabled: true, SHA256: zipHash([]byte("clock"))}
	disabled := current
	disabled.Enabled = false
	f, tr := false, true
	for _, tc := range []struct {
		name      string
		dump      string
		api       kioskState
		token     string
		calls     []string
		install   bool
		installed *bool // health kiosk.installed
		admin     *bool // health kiosk.remote_admin
		version   string
		switched  *bool
		err       string
		fails     int
	}{
		{name: "no package", dump: kioskNotFound, installed: &f, fails: 1},
		{name: "remote admin down", api: kioskState{down: true}, calls: []string{"health"},
			installed: &tr, admin: &f, err: "remote administration on port 2324: dial tcp 127.0.0.1:2324: connect: connection refused", fails: 1},
		{name: "no token", token: "-", calls: []string{"health"}, installed: &tr, admin: &tr, err: "no_token", fails: 1},
		{name: "401", api: kioskState{token: "older-token", master: true, plugins: []pluginRecord{current}},
			calls: []string{"health", "listPlugins"}, installed: &tr, admin: &tr, err: "token_rejected", fails: 1},
		{name: "same hash", api: kioskState{master: true, plugins: []pluginRecord{other, current}},
			calls:     []string{"health", "listPlugins", "getPluginState"},
			installed: &tr, admin: &tr, version: testPlugin, switched: &f},
		{name: "different hash", api: kioskState{master: true, plugins: []pluginRecord{older}},
			calls:   []string{"health", "listPlugins", "installPlugin", "getPluginState"},
			install: true, installed: &tr, admin: &tr, version: testPlugin, switched: &f},
		{name: "missing record", api: kioskState{master: true, plugins: []pluginRecord{other}},
			calls:   []string{"health", "listPlugins", "installPlugin", "getPluginState", "enablePlugin"},
			install: true, installed: &tr, admin: &tr, version: testPlugin, switched: &f},
		{name: "master switch off", api: kioskState{plugins: []pluginRecord{current}},
			calls:     []string{"health", "listPlugins", "getPluginState", "setPluginsEnabled"},
			installed: &tr, admin: &tr, version: testPlugin, switched: &tr},
		{name: "record disabled", api: kioskState{master: true, plugins: []pluginRecord{disabled}},
			calls:     []string{"health", "listPlugins", "getPluginState", "enablePlugin"},
			installed: &tr, admin: &tr, version: testPlugin, switched: &f},
		{name: "install refused", api: kioskState{master: true, refuse: "Plugin needs Android API 31", plugins: []pluginRecord{older}},
			calls:     []string{"health", "listPlugins", "installPlugin"},
			installed: &tr, admin: &tr, version: "1.0.0-bbbbbbbbbbbb", err: "installPlugin: Plugin needs Android API 31", fails: 1},
	} {
		dump := tc.dump
		if dump == "" {
			dump = kioskDump
		}
		token := tc.token
		switch token {
		case "":
			token = testToken
		case "-":
			token = ""
		}
		api := &fakeKiosk{kioskState: tc.api}
		if api.token == "" {
			api.token = testToken
		}
		r := &fakeRunner{replies: map[string]reply{"dumpsys package me.jxl.kiosk_satellite": {out: dump}}}
		k, h := newTestKiosk(r, api, token)
		k.attempt(context.Background(), actEnsure)

		if got := r.commands(); !slices.Equal(got, []string{"dumpsys package me.jxl.kiosk_satellite"}) {
			t.Errorf("%s: commands %q", tc.name, got)
		}
		if got := api.commands(); !slices.Equal(got, tc.calls) {
			t.Errorf("%s: API calls\n got %q\nwant %q", tc.name, got, tc.calls)
		}
		if tc.install != (api.installed != nil) || (tc.install && !bytes.Equal(api.installed, testZIP)) {
			t.Errorf("%s: installed %q", tc.name, api.installed)
		}
		if tc.install && tc.err == "" {
			rec := findPlugin(api.plugins)
			if rec == nil || !rec.Enabled || !api.master {
				t.Errorf("%s: plugin left %+v, master %v", tc.name, rec, api.master)
			}
		}
		st := h.Stats().Kiosk
		if !eqPtr(st.Installed, tc.installed) || !eqPtr(st.RemoteAdmin, tc.admin) || !eqPtr(st.Plugin.SwitchedOn, tc.switched) {
			t.Errorf("%s: installed %v remote admin %v switched on %v", tc.name, deref(st.Installed), deref(st.RemoteAdmin), deref(st.Plugin.SwitchedOn))
		}
		if tc.installed != nil && *tc.installed && (st.Version == nil || *st.Version != "2026.10.28") {
			t.Errorf("%s: Kiosk Satellite version %v", tc.name, deref(st.Version))
		}
		if got := deref(st.Plugin.Version); got != tc.version {
			t.Errorf("%s: plugin version %q, want %q", tc.name, got, tc.version)
		}
		if got := deref(st.Plugin.Error); got != tc.err {
			t.Errorf("%s: plugin error %q, want %q", tc.name, got, tc.err)
		}
		if strings.Contains(deref(st.Plugin.Error), testToken) {
			t.Errorf("%s: health carries the token", tc.name)
		}
		if k.fails != tc.fails {
			t.Errorf("%s: fails %d, want %d", tc.name, k.fails, tc.fails)
		}
	}
}

// The master switch, once this run turned it on, stays reported on.
func TestKioskSwitchedOnStays(t *testing.T) {
	api := &fakeKiosk{kioskState: kioskState{token: testToken, plugins: []pluginRecord{{ID: PluginID, Enabled: true, SHA256: zipHash(testZIP)}}}}
	r := &fakeRunner{replies: map[string]reply{"dumpsys": {out: kioskDump}}}
	k, h := newTestKiosk(r, api, testToken)
	k.attempt(context.Background(), actEnsure)
	k.attempt(context.Background(), actEnsure)
	if st := h.Stats().Kiosk.Plugin; st.SwitchedOn == nil || !*st.SwitchedOn || st.Error != nil {
		t.Fatalf("plugin health %+v", st)
	}
	if n := strings.Count(strings.Join(api.commands(), " "), "setPluginsEnabled"); n != 1 {
		t.Fatalf("setPluginsEnabled ran %d times", n)
	}
}

// Pacing as the app's installer, plus: a token or port change runs the
// check again at the next 10 s gap with the failure backoff reset, an
// unchanged config does not, and Check (a plugin version mismatch)
// requests a run.
func TestKioskInstallerPacing(t *testing.T) {
	t0 := time.Unix(1_000_000, 0)
	now := t0
	k := NewKioskInstaller(KioskInstallerConfig{Runner: &fakeRunner{}, HTTP: &fakeKiosk{}, Health: &Health{}, Port: 2324,
		ZIP: testZIP, Version: testPlugin, Now: func() time.Time { return now }})
	expect := func(at time.Duration, wantAct action, wantWait time.Duration) {
		t.Helper()
		now = t0.Add(at)
		a, wait := k.next(now)
		if a != wantAct || wait != wantWait {
			t.Fatalf("at %s: next = %s, %s; want %s, %s", at, a, wait, wantAct, wantWait)
		}
	}
	expect(0, actEnsure, 0)
	k.fails = 4 // no token, four times: the gap is 160 s
	k.Configure("", 2324)
	expect(time.Second, actNone, 29*time.Second)
	k.Configure(testToken, 2324)
	expect(2*time.Second, actNone, 8*time.Second)
	expect(10*time.Second, actEnsure, 0)

	now = t0.Add(11 * time.Second)
	k.SetLinked(true)
	expect(time.Hour, actNone, 0)
	k.Configure(testToken, 2400)
	expect(time.Hour, actEnsure, 0)
	k.Check()
	expect(time.Hour+5*time.Second, actNone, 5*time.Second)
	expect(time.Hour+10*time.Second, actEnsure, 0)
	if tok, port := k.settings(); tok != testToken || port != 2400 {
		t.Fatalf("settings %q %d", tok, port)
	}
}

// A firmware without an embedded plugin reports it and never calls Kiosk
// Satellite.
func TestKioskInstallerWithoutZIP(t *testing.T) {
	r := &fakeRunner{}
	api := &fakeKiosk{}
	h := &Health{}
	k := NewKioskInstaller(KioskInstallerConfig{Runner: r, HTTP: api, Health: h, Token: testToken, Port: 2324})
	done := make(chan struct{})
	go func() { k.Run(context.Background()); close(done) }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Run kept running without a plugin")
	}
	st := h.Stats().Kiosk.Plugin
	if st.Error == nil || *st.Error != "this firmware embeds no Kiosk Satellite plugin" || len(r.commands()) != 0 || len(api.commands()) != 0 {
		t.Fatalf("health %+v, commands %q, API %q", st, r.commands(), api.commands())
	}
}

// An installer failure never ends Run; only ctx does.
func TestKioskInstallerFailureNeverEndsRun(t *testing.T) {
	r := &fakeRunner{ran: make(chan struct{}, 1), replies: map[string]reply{"dumpsys": {out: kioskDump}}}
	api := &fakeKiosk{kioskState: kioskState{token: "another"}}
	k, h := newTestKiosk(r, api, testToken)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { k.Run(ctx); close(done) }()
	select {
	case <-r.ran:
	case <-time.After(5 * time.Second):
		t.Fatal("no attempt at start")
	}
	k.Check() // paced: waits out the failure gap rather than running now
	select {
	case <-done:
		t.Fatal("Run ended on an installer failure")
	case <-time.After(200 * time.Millisecond):
	}
	if st := h.Stats().Kiosk.Plugin; st.Error == nil || *st.Error != "token_rejected" {
		t.Fatalf("plugin health %+v", st)
	}
	if n := len(r.commands()); n != 1 {
		t.Fatalf("%d attempts, want 1: %q", n, r.commands())
	}
	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Run did not end with ctx")
	}
}

func eqPtr[T comparable](a, b *T) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

func deref[T any](p *T) T {
	var zero T
	if p == nil {
		return zero
	}
	return *p
}
