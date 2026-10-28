const { useState, useEffect, useRef, useCallback, useMemo } = React;

// ─── Ingress ──────────────────────────────────────────────────────────────────

// Under Home Assistant Ingress the dashboard is mounted below a generated
// path (e.g. /api/hassio_ingress/<token>/), not at the site root — the
// server injects a matching <base href> (see em_api.py's
// _with_ingress_base). Every absolute "/api/..." path in this file has to
// become relative through this so it resolves under that base instead of
// bypassing it straight to the root. A no-op outside ingress, where the
// page's own URL already is the root.
function ingressPath(path) {
  return path.startsWith('/') ? `.${path}` : path;
}

function ingressWebSocketUrl(path) {
  const url = new URL(ingressPath(path), document.baseURI);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
}

// ─── API ──────────────────────────────────────────────────────────────────────

const API = {
  token: null,

  // All requests go through one code path: a URL relative to ingress, Bearer auth and
  // one error shape. Sessions use only Bearer headers (no cookie is ever set),
  // so anything the browser would fetch for itself — an <a download>, an
  // <audio src> — would 401; binary content comes through blob() and is
  // handed on as an object URL, which also keeps the token out of the URL bar.
  //
  // Resolves to the Response when it is OK. Otherwise throws a plain object:
  // {code:'not_authenticated', status:401} for a lapsed session, else the
  // controller's JSON error body ({error, code, …}) with `status` added, or
  // {code:'error', status} when the body was not JSON (a proxy's error page).
  // A network failure rejects with fetch's own TypeError.
  async request(path, { method = 'GET', body, json = false } = {}) {
    const headers = {};
    if (json) headers['Content-Type'] = 'application/json';
    if (this.token) headers['Authorization'] = `Bearer ${this.token}`;
    const r = await fetch(ingressPath(path), { method, headers, body });
    if (r.status === 401) throw { code: 'not_authenticated', status: 401 };
    if (!r.ok) {
      let data = { code: 'error' };
      try { data = await r.json(); } catch {}
      throw { ...data, status: r.status };
    }
    return r;
  },

  async _json(path, method, body) {
    const init = body === undefined ? { method } : { method, json: true, body: JSON.stringify(body) };
    return (await this.request(path, init)).json();
  },

  get(path)         { return this._json(path, 'GET'); },
  post(path, body)  { return this._json(path, 'POST', body); },
  patch(path, body) { return this._json(path, 'PATCH', body); },
  del(path)         { return this._json(path, 'DELETE'); },

  async blob(path) {
    return (await this.request(path)).blob();
  },

  // Multipart POST for uploads that carry more than the file itself (the
  // sound upload sends an id alongside it). Content-Type is deliberately
  // left unset so the browser writes the multipart boundary.
  async postForm(path, form) {
    return (await this.request(path, { method: 'POST', body: form })).json();
  },
};

// ─── Vocabularies ─────────────────────────────────────────────────────────────
//
// Closed sets of strings, one frozen object each; compare against these, never
// against a bare literal. Most mirror a controller (or firmware) enum and are
// wire values — each names its source, and must match it exactly. They carry
// the members the dashboard reads, not necessarily the whole enum.

// em_auth.Role
const ROLE = Object.freeze({ ADMIN: 'admin' });

// em_api.EventType — `type` of a message on the /api/events socket.
const EVENT_TYPE = Object.freeze({
  SNAPSHOT:                'snapshot',
  DEVICE_UPDATE:           'device_update',
  DEVICE_LOG:              'device_log',
  DEVICE_CONNECTED:        'device_connected',
  DEVICE_DISCONNECTED:     'device_disconnected',
  DEVICE_PENDING:          'device_pending',
  DEVICE_APPROVED:         'device_approved',
  DEVICE_DELETED:          'device_deleted',
  DEVICE_UPDATED:          'device_updated',
  DEVICE_UPDATE_FAILED:    'device_update_failed',
  DEVICE_AUTO_ROLLED_BACK: 'device_auto_rolled_back',
  DEVICE_ROLLED_BACK:      'device_rolled_back',
  DEVICE_UPDATE_QUEUE:     'device_update_queue',
  CONTROLLER_UPDATE:       'controller_update',
  TURN_COMPLETE:           'turn_complete',
  ALERTS:                  'alerts',
  HA_STATUS:               'ha_status',
});

// em_device_link.Capability — the ones a dashboard control is gated on.
const CAPABILITY = Object.freeze({
  DEVICE_WAKE:     'device_wake_v1',
  RENDER_PROGRESS: 'render_progress_v1',
  FOCUS_LEASES:    'focus_leases_v1',
  LEDS:            'leds',
  LED_ANIM:        'led_anim',
  BUTTON_HOLD:     'button_hold',
  OPEN_RULES:      'open_rules_v1',
  AFE_METADATA:    'afe_metadata_v1',
  WIFI_CONTROL:    'wifi_control',
  BLE_PROXY:       'ble_proxy',
  PRIVACY_MUTE:    'privacy_mute',
  SCREEN_WAKE:     'screen_wake',
  SCREEN_OVERLAY:  'screen_overlay',
  TURN_VIEW:       'turn_view_v1',
  KIOSK_PLUGIN:    'kiosk_plugin',
  NEAR_MISS:       'near_miss_v1',
});

// em_device_link.DevicePlatform — the image a device's session runs on.
const PLATFORM_LABEL = Object.freeze({
  fireos6:         'Echo Dot 2nd gen · Fire OS 6',
  fireos6_crown:   'Echo Show 8 1st gen · Fire OS 6',
  lineage18_crown: 'Echo Show 8 1st gen · LineageOS 18.1',
});

// deviceState()'s key: what the dashboard shows a device doing. Derived here
// from the device row's flags — the dashboard's own, not a controller enum.
const DEVICE_STATE = Object.freeze({
  PENDING: 'pending', OFFLINE: 'offline', MUTED: 'muted',
  SPEAKING: 'speaking', THINKING: 'thinking', LISTENING: 'listening', IDLE: 'idle',
});

// em_utterance.UtteranceKind — turns.trigger.
const TURN_TRIGGER = Object.freeze({
  WAKE: 'wake', BUTTON: 'button', REPLY: 'reply', HA_REPLY: 'ha_reply',
});

// A turn's recordings, by the last segment of their URL
// (/api/devices/{id}/turns/{turn}/<kind>): the wake clip, the utterance (the
// STT copy), and the turn recording its AFE chart plays.
const TURN_CLIP = Object.freeze({ WAKE: 'wake', MIC: 'audio', RECORDING: 'recording' });

// GET /api/devices/{id}/near_misses — each near miss's `recording`: whether
// the episode's audio was kept, and why not. PRUNED is derived at read time
// (the WAV was saved, retention has since removed it).
const NEAR_MISS_RECORDING = Object.freeze({
  SAVED: 'saved', OFF: 'off', NOT_PERMITTED: 'not_permitted', FAILED: 'failed', PRUNED: 'pruned',
});
const NEAR_MISS_UNRECORDED = Object.freeze({
  [NEAR_MISS_RECORDING.OFF]:           'not recorded — Save wake clips is off',
  [NEAR_MISS_RECORDING.NOT_PERMITTED]: 'not recorded — capture not permitted',
  [NEAR_MISS_RECORDING.FAILED]:        'recording failed',
  [NEAR_MISS_RECORDING.PRUNED]:        'recording no longer kept',
});
// wake.near_miss.reason — near_misses[].end_reason: BELOW is the score falling
// back under the floor; the rest are the detector invalidations that cut it.
const NEAR_MISS_END = Object.freeze({
  BELOW: 'below', GAP: 'gap', RESET: 'reset', MUTE: 'mute', OVERRUN: 'overrun',
});
// em_db.NearMissVerdict — near_misses[].verdict: what someone decided after
// listening to a near miss (PATCH /api/devices/{id}/near_misses/{nm}), or null
// while nobody has. A marked near miss keeps its recording past retention.
const NEAR_MISS_VERDICT = Object.freeze({
  MISSED_WAKE: 'missed_wake', CORRECTLY_IGNORED: 'correctly_ignored',
});

// em_session.TerminalReason — turns.terminal_reason.
const TERMINAL_REASON = Object.freeze({
  COMPLETED:        'completed',
  SUPERSEDED:       'superseded',
  REPLY_TIMEOUT:    'reply_timeout',
  INTERRUPTED:      'interrupted',
  NO_INPUT:         'no_input',
  MUTED:            'muted',
  SESSION_LOST:     'session_lost',
  ARBITRATION_LOST: 'arbitration_lost',
  VERIFIER_TIMEOUT: 'verifier_timeout',
  UNVERIFIED_WAKE:  'unverified_wake',
  SELF_OUTPUT:      'self_output',
  ECHO_ONLY:        'echo_only',
});

// em_session.FollowUp — turns.continuation, when it is not a TerminalReason.
const FOLLOW_UP = Object.freeze({
  PENDING:          'pending',
  ANSWERED:         'answered',
  WAKE:             'wake',
  PROMPT_CANCELLED: 'prompt_cancelled',
  PROMPT_FAILED:    'prompt_failed',
  NO_MIC:           'no_mic',
  CHAIN_LIMIT:      'chain_limit',
});

// em_session.TurnOutcome — turns.outcome of a current row.
const TURN_OUTCOME = Object.freeze({
  HA: 'ha', LOCAL_COMMAND: 'local_command', ALARM: 'alarm',
  ALARM_QUERY: 'alarm_query', TIMER: 'timer', CLARIFICATION: 'clarification', REPEAT: 'repeat',
});

// turns.outcome of a row from before the post-AFE cutover.
const LEGACY_OUTCOME = Object.freeze({ OK: 'ok', CANCELLED: 'cancelled' });

// em_session.WakeAttribution — turns.wake_attribution of an accepted wake.
const WAKE_ATTRIBUTION = Object.freeze({ VERIFIED: 'verified', IDLE: 'idle' });

// em_render.FinishReason | em_session.DialogEnd — turns.playback_reason.
const PLAYBACK_END = Object.freeze({
  DRAINED: 'drained', CANCELLED: 'cancelled', FAILED: 'failed', UNDERRUN: 'underrun',
  RESPONSE_TIMEOUT: 'response_timeout', FENCED: 'fenced',
});

// em_alerts.AlertNotice — `kind` of an `alerts` event.
const ALERT_NOTICE = Object.freeze({ TIMERS: 'timers' });

// em_alerts.OpState — alert journal operation state.
const ALERT_OP_STATE = Object.freeze({ PENDING: 'pending', APPLIED: 'applied', REJECTED: 'rejected' });

// em_alert_wire.RingKind — what is ringing (alert.state `active.kind`).
const RING_KIND = Object.freeze({ ALARM: 'alarm', TIMER: 'timer' });

// em_alert_wire.Weekday — alarm schedule days, Monday first.
const WEEKDAY = Object.freeze({
  MON: 'mon', TUE: 'tue', WED: 'wed', THU: 'thu', FRI: 'fri', SAT: 'sat', SUN: 'sun',
});

// Firmware alerts.WakeLockStatus / alerts.StoreStatus — alert.state health.
const WAKELOCK = Object.freeze({ HELD: 'held', RELEASED: 'released' });
const ALERT_STORE = Object.freeze({ OK: 'ok' });

// em_db.LogLevel / em_db.LogSource — a device log entry.
const LOG_LEVEL = Object.freeze({ INFO: 'info', WARN: 'warn', ERROR: 'error' });
const LOG_SOURCE = Object.freeze({ DEVICE: 'device' });

// ─── Helpers ──────────────────────────────────────────────────────────────────

// The two typefaces. Inline styles cannot use a CSS variable for a font
// family any more cleanly than this, and one constant beats 150 copies.
const MONO = "'DM Mono',monospace";
const SANS = "'DM Sans',sans-serif";

// A slug of a device that is safe in a file name, for downloads. It follows
// em_api._slug's rule, so a filtered archive names each file the same way
// its own download here does.
function fileSlug(label, id) {
  return (label || id).replace(/[^A-Za-z0-9]+/g, '-').replace(/^-+|-+$/g, '').toLowerCase() || 'device';
}

// Save an object URL under `filename` via a transient <a download>. The URL
// stays the caller's to revoke.
function downloadUrl(url, filename) {
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  a.click();
}

// Save a blob that nothing else holds on to. Its URL is revoked on a timer,
// not straight after the click: the click only starts the save, and revoking
// the URL under a download in progress can cancel it.
function downloadBlob(blob, filename) {
  const url = URL.createObjectURL(blob);
  downloadUrl(url, filename);
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

// Props that make a non-button element (a card, a list row, a selectable
// tile) operable from the keyboard and named as a button to assistive tech:
// focusable, and Enter/Space activate it like a click. `selected` reports a
// toggle/selection state as aria-pressed.
function pressable(onPress, { disabled = false, selected } = {}) {
  return {
    role: 'button',
    tabIndex: disabled ? -1 : 0,
    'aria-disabled': disabled || undefined,
    'aria-pressed': selected,
    onClick: disabled ? undefined : onPress,
    onKeyDown: disabled ? undefined : e => {
      if (e.target !== e.currentTarget) return;   // a nested control's own key
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onPress(e); }
    },
  };
}

// A graph SHA-256 is the registry key (§5.1); eight hex digits are enough to
// tell entries apart on screen.
function shortSha(sha) {
  return sha ? sha.slice(0, 8) : '—';
}

// Registry entry label: its spoken wake phrase plus the short graph hash.
// `models` is the /api/wake_models list; an unknown hash still shows itself.
function wakeModelLabel(models, sha) {
  if (!sha) return '—';
  const m = (models || []).find(x => x.graph_sha256 === sha);
  return m ? `${m.wake_phrase} · ${shortSha(sha)}` : shortSha(sha);
}

// em_wake_rules.RuleProfile / RuleCombine — the fields of a wake open or
// shadow rule (§5.2): {profile, windows, combine, threshold}.
const RULE_PROFILE = Object.freeze({ IDLE: 'idle', PLAYBACK: 'playback' });
const RULE_COMBINE = Object.freeze({ MEAN: 'mean', ALL: 'all' });
const RULE_COMBINE_LABEL = Object.freeze({
  [RULE_COMBINE.MEAN]: 'average', [RULE_COMBINE.ALL]: 'every window',
});
// A near miss's profile (near_misses[].profile), as it appears in the Activity list.
const NEAR_MISS_PROFILE_LABEL = Object.freeze({
  [RULE_PROFILE.IDLE]: 'idle', [RULE_PROFILE.PLAYBACK]: 'while playing',
});
// Who transcribes an utterance at a pause (§16.6), config `pauseAsr`; mirrors
// em_pause_asr.PauseAsrEngine.
const PAUSE_ASR_ENGINE = Object.freeze({ KROKO: 'kroko', WYOMING: 'wyoming' });
// Transcript correction (§16.8), config `transcriptFix`; mirrors
// em_text_fix.TextFixMode. The two vocabulary lists' limits mirror
// em_text_fix.MAX_LIST_ENTRIES / MAX_ENTRY_CHARS.
const TEXT_FIX_MODE = Object.freeze({ OFF: 'off', SHADOW: 'shadow', ON: 'on' });
const TEXT_FIX_MAX_ENTRIES = 200;
const TEXT_FIX_MAX_CHARS = 120;

// "idle · 2-window average ≥ 0.95" — a rule as the dashboard names it.
function wakeRuleText(r) {
  if (!r) return '—';
  const t = Number(r.threshold).toFixed(2);
  return r.combine === RULE_COMBINE.ALL
    ? `${r.profile} · ${r.windows}-window, every window ≥ ${t}`
    : `${r.profile} · ${r.windows}-window average ≥ ${t}`;
}

// A controller catalog read once per consumer: `empty` until it arrives, then
// the response plus `loaded: true`. `reload` re-reads it after an upload or
// delete. A failed read (signed out, controller restarting) keeps the last
// value.
function useCatalog(path, empty) {
  const [catalog, setCatalog] = useState({ ...empty, loaded: false });
  const reload = useCallback(async () => {
    try { setCatalog({ ...(await API.get(path)), loaded: true }); } catch {}
  }, [path]);
  useEffect(() => { reload(); }, [reload]);
  return [catalog, reload];
}

// The BCResNet registry (GET /api/wake_models → {active, models}).
function useWakeRegistry() {
  return useCatalog('/api/wake_models', WAKE_REGISTRY_EMPTY);
}
const WAKE_REGISTRY_EMPTY = Object.freeze({ active: null, models: [] });

function relTime(ts) {
  if (!ts) return '—';
  const d = Date.now() - ts * 1000;
  if (d < 60000) return `${Math.floor(d / 1000)}s ago`;
  if (d < 3600000) return `${Math.floor(d / 60000)}m ago`;
  if (d < 86400000) return `${Math.floor(d / 3600000)}h ago`;
  return `${Math.floor(d / 86400000)}d ago`;
}

// Every message on the shared events socket is re-emitted here, so a panel
// that needs live updates (alerts, timers) subscribes instead of having the
// stream drilled down through props.
const _eventSubs = new Set();
function subscribeEvents(fn) { _eventSubs.add(fn); return () => _eventSubs.delete(fn); }
function _emitEvent(msg) {
  _eventSubs.forEach(fn => { try { fn(msg); } catch (e) { console.error(e); } });
}

// Why a control is unavailable on a connected device that lacks a capability
// (§11.1). Features are gated on the capability set the device announced in
// session.hello, never on its firmware version. The controller's own reason
// (`capability_gaps`, em_device.capability_gap) comes first because it is based on
// the platform. An Echo Show reads "managed by the device", a Fire OS Show "no
// LED ring on this device", while an older Dot or an older LineageOS Show
// reads "firmware update available".
const CAPABILITY_REASONS = Object.freeze({
  [CAPABILITY.DEVICE_WAKE]:     'this firmware has no on-device wake detector — update it',
  [CAPABILITY.RENDER_PROGRESS]: 'this firmware cannot play device-local sounds — update it',
  [CAPABILITY.FOCUS_LEASES]:    'this firmware cannot duck music under a response — update it',
  [CAPABILITY.LED_ANIM]:        'this firmware cannot animate the ring, so it shows the listening colour instead — update it',
  [CAPABILITY.BUTTON_HOLD]:     'this firmware has no action-button event for a tap to fire — update it',
  [CAPABILITY.OPEN_RULES]:      'this firmware opens a wake only on the model threshold and runs no shadow rules — update it',
  [CAPABILITY.AFE_METADATA]:    'this firmware does not decode the native AFE metadata — update it',
  [CAPABILITY.NEAR_MISS]:       'this firmware does not report near-miss wakes — update it',
});

// The reason `cap` is missing, or null when the control is available.
// `capabilities` is liveCapabilities(): null means "no live device to ask"
// (the fleet view, or an offline device whose settings apply when it
// returns), and nothing is gated.
function capabilityGap(capabilities, cap) {
  if (capabilities == null || capabilities.names.includes(cap)) return null;
  return capabilities.gaps[cap] || CAPABILITY_REASONS[cap] || `needs the ${cap} capability — update this Echo`;
}

// The live capability set to gate on: null unless a session using the current
// protocol is up (see capabilityGap). `names` is what the device announced;
// `gaps` holds the controller's reason for each absent one whose reason depends on the platform.
function liveCapabilities(d) {
  return d.connected ? { names: d.capabilities || [], gaps: d.capability_gaps || {} } : null;
}

// Why the kiosk API URL and password are disabled: no screen_wake, or a
// device whose companion app lights the screen itself (screen_overlay), where
// they go unused (docs/echo-show-companion-plan.md decision 10). The toggle
// stays on screen_wake alone.
const SCREEN_OVERLAY_REASON = 'the EchoMuse app lights the screen';
function kioskApiGap(capabilities) {
  return capabilityGap(capabilities, CAPABILITY.SCREEN_WAKE)
    || (capabilities?.names.includes(CAPABILITY.SCREEN_OVERLAY) ? SCREEN_OVERLAY_REASON : null);
}

// A Status value that may be unknown: null reads "unavailable", never "no".
function _known(v, yes, no) {
  return v == null ? ['unavailable', 'var(--muted)'] : v ? [yes, 'var(--ok)'] : [no, 'var(--warn)'];
}

// The Status rows for a LineageOS Show's companion app, from
// `stats.companion` (protocol-v1 §stats): null when the device reports none,
// so a device without the app shows no row at all. A null value is unknown
// and reads "unavailable", never "no"; only `error` uses null for "none".
function companionRows(stats) {
  const c = stats?.companion;
  if (c == null) return null;
  return [
    ['Linked', ..._known(c.linked, 'Linked', 'Not linked')],
    ['App version', c.version ?? 'unavailable', c.version == null ? 'var(--muted)' : undefined],
    ['Overlay', ..._known(c.overlay, 'Allowed', 'Not allowed')],
    ['Last error', c.error ?? 'none', c.error == null ? undefined : 'var(--error)'],
  ];
}

// stats.companion.kiosk.plugin.error codes the dashboard acts on
// (protocol-v1 §4.8); any other error is Kiosk Satellite's own text.
const KIOSK_ERROR = Object.freeze({ TOKEN_REJECTED: 'token_rejected', NO_TOKEN: 'no_token' });
const KIOSK_ERROR_TEXT = Object.freeze({
  [KIOSK_ERROR.TOKEN_REJECTED]: 'Kiosk Satellite rejected the token — log in again (Config → Screen)',
  [KIOSK_ERROR.NO_TOKEN]: 'no token — log in to Kiosk Satellite (Config → Screen)',
});

// The Status rows for the Kiosk Satellite plugin that shows each turn
// (docs/echo-show-companion-plan.md §11), from `stats.companion.kiosk`: null
// when the device reports none; one row when Kiosk Satellite is not
// installed. Null values read "unavailable", as above.
function kioskRows(stats) {
  const k = stats?.companion?.kiosk;
  if (k == null) return null;
  if (k.installed === false) return [['Kiosk Satellite', 'Not installed', 'var(--muted)']];
  const p = k.plugin ?? {};
  return [
    ['Kiosk Satellite', k.installed == null ? 'unavailable' : k.version ?? 'installed, version unavailable',
     k.installed == null || k.version == null ? 'var(--muted)' : undefined],
    ['Remote admin', ..._known(k.remote_admin, 'Answering', 'Not answering')],
    ['Plugin', ..._known(p.linked, 'Linked', 'Not linked')],
    ['Plugin version', p.version ?? 'unavailable', p.version == null ? 'var(--muted)' : undefined],
    ['Plugins switched on', ...(p.switched_on == null ? ['unavailable', 'var(--muted)']
      : p.switched_on ? ['by EchoMuse', 'var(--warn)'] : ['no', undefined])],
    ['Plugin error', KIOSK_ERROR_TEXT[p.error] ?? p.error ?? 'none', p.error == null ? undefined : 'var(--error)'],
  ];
}

// The Kiosk Satellite health a prompt reads: the live report while the device
// is connected, else its last one (`kiosk`, kept past the session), since a
// queued update is for an offline device.
function kioskHealth(d) {
  return (d.connected ? d.stats?.companion?.kiosk : d.kiosk) ?? null;
}

// Why the dashboard asks for the Kiosk Satellite password before an update
// (docs/echo-show-companion-plan.md decision 4), or null when it need not:
// Kiosk Satellite is installed and EchoMuse holds no token for it, or its
// token was rejected.
function kioskLoginReason(d) {
  const k = kioskHealth(d);
  if (k?.installed !== true) return null;
  if (k.plugin?.error === KIOSK_ERROR.TOKEN_REJECTED) return 'Kiosk Satellite rejected the token EchoMuse holds.';
  return d.kioskTokenSet ? null : 'EchoMuse holds no Kiosk Satellite token for this Echo Show.';
}

// Simulated LED colour for a device the controller cannot use: offline.
const UNUSABLE_DOT = '#d4703a';

function deviceState(d) {
  const S = DEVICE_STATE;
  if (!d.approved)  return { key: S.PENDING,   label: 'Pending',   color: 'var(--accent-hi)', dot: '#8ab0d0' };
  if (!d.connected) return { key: S.OFFLINE,   label: 'Offline',   color: 'var(--warn)', dot: UNUSABLE_DOT };
  if (d.muted)      return { key: S.MUTED,     label: 'Muted',     color: 'var(--error)', dot: '#c04040' };
  if (d.speaking)   return { key: S.SPEAKING,  label: 'Speaking',  color: 'var(--accent)', dot: '#4080d0' };
  if (d.thinking)   return { key: S.THINKING,  label: 'Thinking',  color: 'var(--warn)', dot: '#a08020' };
  if (d.listening)  return { key: S.LISTENING, label: 'Listening', color: 'var(--ok)', dot: '#40906a' };
  return               { key: S.IDLE,      label: 'Idle',      color: 'var(--muted)', dot: '#aaaaaa' };
}

// The device's reported IP, or null when there is none worth showing (a
// loopback address is not one anything can reach the device on).
function deviceIp(d) {
  return d.ip && d.ip !== '127.0.0.1' ? d.ip : null;
}

// deviceIp for display. An offline device shows its last known address
// marked with `staleSuffix`.
function deviceIpText(d, staleSuffix) {
  const ip = deviceIp(d);
  return d.connected ? (ip || '—') : (ip ? `${ip}${staleSuffix}` : '—');
}

// Two versions describe a Dot, and they are different things. `os_version`
// is the Fire OS build it was rooted on (ro.build.version.name, e.g.
// "Fire OS 6.5.6.9 (NS6569/6009)"), which EchoMuse runs on and never
// changes; `firmware_ver` is the EchoMuse firmware this controller installs
// and updates. This is the OS without its parenthesised build code, for
// spots too narrow for the whole name; null when the device never reported it.
function osVersionShort(d) {
  return d.os_version ? d.os_version.replace(/\s*\([^)]*\)\s*$/, '') : null;
}

const LOG_LEVEL_COLOR = Object.freeze({
  [LOG_LEVEL.INFO]: 'var(--ok)', [LOG_LEVEL.WARN]: 'var(--warn)', [LOG_LEVEL.ERROR]: 'var(--error)',
});

function eventAccent(level) {
  return LOG_LEVEL_COLOR[level] || 'var(--muted)';
}

// ─── Components ───────────────────────────────────────────────────────────────

function Lcd({ label, value, color, size = 16 }) {
  return (
    <div className="em-lcd">
      {label && <div className="em-lcd__label">{label}</div>}
      {/* The glow is mixed, not hex-concatenated. It used to be `${color}88`,
          which worked only while every colour was a literal — the moment the
          call sites became var(--lcd-green) it produced `var(--lcd-green)88`,
          invalid CSS that drops the whole declaration. The glow silently
          disappeared. color-mix takes a var(); 0x88 is 53%. */}
      <div style={{ fontFamily: MONO, fontSize: size, color: color || 'var(--lcd-green)', lineHeight: 1,
                    textShadow: `0 0 8px color-mix(in srgb, ${color || 'var(--lcd-green)'} 53%, transparent)` }}>{value}</div>
    </div>
  );
}

function Pill({ children, accent, danger, disabled, onClick, small, big }) {
  // Variants as classes, not a ternary chain per property. :disabled is
  // handled in CSS, so it beats every variant without this having to know
  // the precedence.
  const cls = ['em-pill',
    small  && 'em-pill--small',
    big    && 'em-pill--big',
    accent && 'em-pill--accent',
    danger && 'em-pill--danger'].filter(Boolean).join(' ');
  return <button className={cls} onClick={onClick} disabled={disabled}>{children}</button>;
}

// ThemeToggle — light/dark, remembered per browser.
//
// The theme itself is applied by an inline script in dashboard.html so it is
// set before first paint; this only flips the attribute that script wrote and
// records the choice. Reading the attribute rather than holding the source of
// truth in React means the two can never disagree.
//
// Defaults to the OS preference until the user picks, and the pick then wins
// permanently — someone who chooses light at their desk does not want it
// flipping at sunset because their laptop does.
function ThemeToggle() {
  const [theme, setTheme] = useState(
    () => document.documentElement.getAttribute('data-theme') || 'light');

  function flip() {
    const next = theme === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-theme', next);
    try { localStorage.setItem('em-theme', next); } catch (e) { /* private mode */ }
    setTheme(next);
  }

  return (
    <IconButton onClick={flip}
      label={theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'}>
      {theme === 'dark' ? '☾' : '☀'}
    </IconButton>
  );
}

// IconButton — the round icon control in the header. One shell so the theme
// toggle, settings and sign-out are the same object at three sizes of glyph
// rather than three near-identical buttons.
//
// `label` is required and does double duty: the tooltip and the accessible
// name. An icon-only control has no visible text, so without it the button is
// a mystery to a screen reader and a guess to everyone else.
function IconButton({ onClick, label, danger, accent, busy, disabled, children }) {
  const cls = ['em-iconbtn', accent && 'em-iconbtn--accent', busy && 'em-iconbtn--busy']
    .filter(Boolean).join(' ');
  return (
    <button className={cls} onClick={onClick} title={label} aria-label={label}
            aria-busy={busy || undefined} disabled={disabled || busy}
            style={danger ? { color: 'var(--error)' } : undefined}>
      {children}
    </button>
  );
}

// Deploy-to-fleet: an arrow rising out of a tray. Up rather than down because
// this pushes firmware out to the devices; a download arrow would say the
// opposite of what the button does.
function DeployIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true"
         stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
      <path d="M8 10.5V2.5"/>
      <path d="M5 5.5 8 2.5l3 3"/>
      <path d="M2.5 10.5v2a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1v-2"/>
    </svg>
  );
}

// Sign-out mark: a door with an arrow leaving it. Drawn rather than set in
// type because Unicode has no unambiguous glyph for it — the near misses are
// power (⏻), which reads as "shut the device down", and escape (⎋), which
// almost nobody recognises.
function SignOutIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true"
         stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
      <path d="M6 2.5H3.5A1.5 1.5 0 0 0 2 4v8a1.5 1.5 0 0 0 1.5 1.5H6"/>
      <path d="M10.5 11 14 8l-3.5-3"/>
      <path d="M14 8H6"/>
    </svg>
  );
}

// SectionLabel — the small uppercase mono heading used throughout. One
// definition instead of the same inline style repeated per call site.
function SectionLabel({ children, style }) {
  return (
    <div className="em-label" style={style}>
      {children}
    </div>
  );
}

// Panel — bordered card grouping related controls. Gives tab content a
// consistent visual structure instead of floating elements.
function Panel({ label, children, style }) {
  return (
    <div className="em-panel" style={style}>
      {label && <SectionLabel>{label}</SectionLabel>}
      {children}
    </div>
  );
}

// CircleButton — the round header button (close, delete). One treatment
// everywhere instead of per-modal variants.
function CircleButton({ onClick, title, color, children }) {
  return (
    <button type="button" onClick={onClick} title={title} aria-label={title} style={{
      background: 'linear-gradient(180deg,var(--sunken),var(--border))', border: '1px solid var(--muted)',
      borderRadius: '50%', width: 28, height: 28, display: 'flex', alignItems: 'center',
      justifyContent: 'center', cursor: 'pointer', boxShadow: '0 1px 0 var(--sheen) inset',
      color: color || 'var(--text2)', fontSize: 15, fontWeight: 300, lineHeight: 1,
    }}>{children}</button>
  );
}

// ModalFrame — the fixed window the device and settings modals share: a
// blurred backdrop that closes the modal on a click outside it, and a fixed
// height (not maxHeight), so every tab renders in an identical frame — content
// scrolls inside, the window never resizes as you move between tabs.
function ModalFrame({ onClose, zIndex, label, children }) {
  return (
    <div style={{ position: 'fixed', inset: 0, background: 'rgba(180,176,168,0.5)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex, backdropFilter: 'blur(8px)' }}
      onClick={e => e.target === e.currentTarget && onClose()}>
      <div className="em-modal" role="dialog" aria-modal="true" aria-label={label}
        style={{ width: 'min(900px,95vw)', height: 'min(700px,90vh)', background: 'linear-gradient(170deg,var(--raised),var(--surface))', border: '1px solid var(--border)', borderRadius: 16, boxShadow: '0 24px 80px rgba(0,0,0,0.3),0 2px 0 var(--sheen) inset', display: 'flex', flexDirection: 'column', overflow: 'hidden', animation: 'fadeIn 0.15s ease' }}>
        {children}
      </div>
    </div>
  );
}

// TabBar — the raised folder tabs along the foot of a modal header, one style
// across the dashboard. `labels` maps a tab to its text; without it the tab
// value is shown (upper-cased by the style).
function TabBar({ tabs, active, onSelect, labels }) {
  return (
    <div className="em-tabs" role="tablist" style={{ display: 'flex', gap: 2 }}>
      {tabs.map(t => {
        const on = active === t;
        return (
          <button key={t} type="button" role="tab" aria-selected={on} onClick={() => onSelect(t)} style={{ background: on ? 'linear-gradient(180deg,var(--raised),var(--surface))' : 'transparent', border: on ? '1px solid var(--border-hard)' : '1px solid transparent', borderBottom: on ? '1px solid var(--surface)' : '1px solid transparent', borderRadius: '6px 6px 0 0', fontFamily: MONO, fontSize: 10, textTransform: 'uppercase', letterSpacing: '0.1em', padding: '7px 14px', cursor: 'pointer', color: on ? 'var(--text)' : 'var(--muted)', marginBottom: -1, transition: 'color 0.15s' }}>
            {labels ? labels[t] : t}
          </button>
        );
      })}
    </div>
  );
}

// DisclosureToggle — the ▸/▾ line that opens a collapsed section (release
// notes, a stage's Advanced controls). A real button, so it is reachable from
// the keyboard and announces whether it is open.
function DisclosureToggle({ open, onToggle, style, children }) {
  return (
    <button type="button" aria-expanded={open} onClick={onToggle} style={{
      background: 'none', border: 'none', padding: 0, width: '100%', textAlign: 'left',
      fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase',
      letterSpacing: '0.15em', cursor: 'pointer', userSelect: 'none',
      display: 'flex', alignItems: 'center', gap: 6, ...style,
    }}>
      <span aria-hidden="true">{open ? '▾' : '▸'}</span>
      {children}
    </button>
  );
}

function Slider({ label, sub, value, min, max, step = 1, unit = '', formatValue, onChange, disabled = false }) {
  const display = formatValue ? formatValue(value) : `${value}${unit}`;
  // minWidth: 0 (root + label div) and width: 100% on the range input for
  // the same reason as Toggle below: a grid item's min-width defaults to
  // its content, so a fixed-min-width native range input (~129px) plus the
  // label row forced narrow grid columns wider than their track — the
  // controls leaked past the panel edge on phone widths.
  return (
    <div style={{ marginBottom: 20, minWidth: 0 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', marginBottom: 7, minWidth: 0, gap: 8 }}>
        <div style={{ minWidth: 0 }}>
          <span style={{ fontFamily: MONO, fontSize: 11, color: disabled ? 'var(--muted)' : 'var(--text2)' }}>{label}</span>
          {sub && <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginLeft: 8 }}>{sub}</span>}
        </div>
        <Lcd value={display} size={12} />
      </div>
      {/* A control whose feature the device lacks is shown disabled WITH the
          reason (in sub), never as one that silently does nothing. */}
      <input type="range" min={min} max={max} step={step} value={value} disabled={disabled} aria-label={label}
        style={{ width: '100%', opacity: disabled ? 0.45 : 1 }}
        onChange={e => onChange(Number(e.target.value))} />
    </div>
  );
}

// A min/max pair on one track: two thumbs, `lo` never past `hi`, and the fill
// between them. Of two thumbs on one value, the one on top is the one that can
// still move (the max in the lower half, the min in the upper); either is also
// reachable from the keyboard. `onChange(lo, hi)`.
function RangeSlider({ label, lo, hi, min = 0, max = 1, step = 0.01, format = v => v, onChange }) {
  const frac = v => (v - min) / (max - min);
  const minOnTop = frac(lo) > 0.5;
  const thumb = (value, name, set, top) => (
    <input type="range" min={min} max={max} step={step} value={value} aria-label={name}
      style={{ zIndex: top ? 2 : 1 }} onChange={e => set(Number(e.target.value))}/>
  );
  return (
    <div style={{ minWidth: 0 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', gap: 8, marginBottom: 6 }}>
        <span style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)' }}>{label}</span>
        <Lcd value={`${format(lo)} – ${format(hi)}`} size={12}/>
      </div>
      <div className="em-range2">
        <div className="em-range2__track"/>
        {/* The thumbs' centres run from 7px to 100% − 7px. */}
        <div className="em-range2__fill" style={{
          left: `calc(7px + (100% - 14px) * ${frac(lo)})`, width: `calc((100% - 14px) * ${frac(hi) - frac(lo)})` }}/>
        {thumb(lo, `${label}: minimum`, v => onChange(Math.min(v, hi), hi), minOnTop)}
        {thumb(hi, `${label}: maximum`, v => onChange(lo, Math.max(v, lo)), !minOnTop)}
      </div>
    </div>
  );
}

function Toggle({ label, sub, value, onChange, disabled = false }) {
  // minWidth: 0 on the flex container and label lets long label/sub text
  // shrink and wrap instead of forcing the row (and the switch with it)
  // wider than the grid column — which pushed the switch past the edge of
  // the config dialog. flexShrink: 0 keeps the switch at full size.
  //
  // `disabled` is honoured here, not just styled: callers have passed it
  // since the table for bypassing native AFE landed, but the prop was ignored, so
  // a control that looked off and greyed out still WROTE the opposite value on a
  // click — the exact "silently does something else" that showing a disabled
  // control with a reason exists to prevent. Slider has always taken the same prop.
  return (
    <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 20, minWidth: 0, gap: 10 }}>
      <div style={{ minWidth: 0, flex: 1 }}>
        <span style={{ fontFamily: MONO, fontSize: 11, color: disabled ? 'var(--muted)' : 'var(--text2)' }}>{label}</span>
        {sub && <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginLeft: 8 }}>{sub}</span>}
      </div>
      {/* A real switch: focusable, operable from the keyboard, and announced
          with its label and state — it used to be a clickable <div>. */}
      <button type="button" role="switch" aria-checked={!!value} aria-label={label}
        disabled={disabled} onClick={() => onChange(!value)} style={{
        width: 36, height: 20, borderRadius: 10, cursor: disabled ? 'default' : 'pointer',
        position: 'relative', flexShrink: 0, opacity: disabled ? 0.45 : 1, padding: 0,
        background: value ? 'var(--accent)' : 'var(--muted)',
        border: value ? '1px solid var(--accent-deep)' : '1px solid var(--muted)',
        transition: 'background 0.15s',
      }}>
        <div style={{
          position: 'absolute', top: 2, left: value ? 17 : 2,
          width: 14, height: 14, borderRadius: 7,
          background: value ? 'var(--accent-tint)' : 'var(--bg)',
          transition: 'left 0.15s',
        }}/>
      </button>
    </div>
  );
}

// ─── EQ frequency response curve ─────────────────────────────────────────────

function EqCurve({ bands, fs = 22050 }) {
  const FREQS = [125, 250, 500, 1000, 2000, 3500, 5500, 8000];
  const Q = 1.4, DB_RANGE = 14, N = 130, F_MIN = 60, F_MAX = 11000;
  const W = 380, H = 90, PT = 8, PB = 20, PL = 8, PR = 8;
  const IW = W - PL - PR, IH = H - PT - PB;

  function peakCoeffs(fc, g) {
    const A = Math.pow(10, g/40), w0 = 2*Math.PI*fc/fs;
    const cw = Math.cos(w0), alpha = Math.sin(w0)/(2*Q), a0 = 1+alpha/A;
    return { b:[(1+alpha*A)/a0,(-2*cw)/a0,(1-alpha*A)/a0], a:[1,(-2*cw)/a0,(1-alpha/A)/a0] };
  }
  function loShelfCoeffs(fc, g) {
    const A = Math.pow(10, g/40), w0 = 2*Math.PI*fc/fs;
    const cw = Math.cos(w0), sw = Math.sin(w0), sqA = Math.sqrt(A), al = sw/Math.SQRT2;
    const a0 = (A+1)+(A-1)*cw+2*sqA*al;
    return { b:[A*((A+1)-(A-1)*cw+2*sqA*al)/a0, 2*A*((A-1)-(A+1)*cw)/a0, A*((A+1)-(A-1)*cw-2*sqA*al)/a0],
             a:[1, -2*((A-1)+(A+1)*cw)/a0, ((A+1)+(A-1)*cw-2*sqA*al)/a0] };
  }
  function hiShelfCoeffs(fc, g) {
    const A = Math.pow(10, g/40), w0 = 2*Math.PI*fc/fs;
    const cw = Math.cos(w0), sw = Math.sin(w0), sqA = Math.sqrt(A), al = sw/Math.SQRT2;
    const a0 = (A+1)-(A-1)*cw+2*sqA*al;
    return { b:[A*((A+1)+(A-1)*cw+2*sqA*al)/a0, -2*A*((A-1)+(A+1)*cw)/a0, A*((A+1)+(A-1)*cw-2*sqA*al)/a0],
             a:[1, 2*((A-1)-(A+1)*cw)/a0, ((A+1)-(A-1)*cw-2*sqA*al)/a0] };
  }
  function biquadMag({b, a}, f) {
    const w = 2*Math.PI*f/fs, c1=Math.cos(w), s1=Math.sin(w), c2=Math.cos(2*w), s2=Math.sin(2*w);
    const nR=b[0]+b[1]*c1+b[2]*c2, nI=-(b[1]*s1+b[2]*s2);
    const dR=1+a[1]*c1+a[2]*c2,    dI=-(a[1]*s1+a[2]*s2);
    return Math.sqrt((nR*nR+nI*nI)/(dR*dR+dI*dI));
  }

  const pts = Array.from({length:N}, (_,i) => Math.exp(Math.log(F_MIN) + i/(N-1)*Math.log(F_MAX/F_MIN)));
  const dbs = pts.map(f => {
    let mag = 1;
    bands.forEach((g,i) => {
      mag *= biquadMag(i===0 ? loShelfCoeffs(FREQS[i],g) : i===7 ? hiShelfCoeffs(FREQS[i],g) : peakCoeffs(FREQS[i],g), f);
    });
    return 20*Math.log10(Math.max(mag, 1e-10));
  });

  const xOf = f  => PL + IW*(Math.log(f/F_MIN)/Math.log(F_MAX/F_MIN));
  const yOf = db => PT + IH*(1 - (Math.max(-DB_RANGE, Math.min(DB_RANGE, db))+DB_RANGE)/(2*DB_RANGE));

  const line = pts.map((f,i) => `${i===0?'M':'L'}${xOf(f).toFixed(1)},${yOf(dbs[i]).toFixed(1)}`).join(' ');
  const fill = `${line} L${xOf(F_MAX).toFixed(1)},${yOf(0).toFixed(1)} L${xOf(F_MIN).toFixed(1)},${yOf(0).toFixed(1)}Z`;

  const dbTicks = [-12,-6,0,6,12];
  const fTicks  = [{f:125,label:'125'},{f:500,label:'500'},{f:1000,label:'1k'},{f:4000,label:'4k'},{f:8000,label:'8k'}];

  return (
    <svg viewBox={`0 0 ${W} ${H}`} style={{ width:'100%', display:'block', marginBottom:4, borderRadius:4, overflow:'hidden' }}>
      <rect x={PL} y={PT} width={IW} height={IH} fill="var(--hairline)" rx="2"/>
      {dbTicks.map(db => (
        <line key={db} x1={PL} x2={PL+IW} y1={yOf(db)} y2={yOf(db)}
          stroke={db===0?'rgba(0,0,0,0.18)':'var(--hairline)'}
          strokeWidth={db===0?1:0.5} strokeDasharray={db===0?undefined:'2,3'}/>
      ))}
      {fTicks.map(({f}) => (
        <line key={f} x1={xOf(f)} x2={xOf(f)} y1={PT} y2={PT+IH}
          stroke="var(--hairline)" strokeWidth={0.5}/>
      ))}
      <path d={fill} fill="rgba(64,88,120,0.10)"/>
      <path d={line} fill="none" stroke="var(--accent)" strokeWidth="1.5"
        style={{filter:'drop-shadow(0 0 4px rgba(64,88,120,0.4))'}}/>
      {dbTicks.filter(d=>d!==0).map(db => (
        <text key={db} x={PL+2} y={yOf(db)+4}
          style={{fontFamily:MONO,fontSize:6,fill:'rgba(0,0,0,0.28)'}}>{db>0?'+':''}{db}</text>
      ))}
      {fTicks.map(({f,label}) => (
        <text key={f} x={xOf(f)} y={H-4} textAnchor="middle"
          style={{fontFamily:MONO,fontSize:6,fill:'rgba(0,0,0,0.28)'}}>{label}</text>
      ))}
    </svg>
  );
}

// ─── WiFi signal bars ─────────────────────────────────────────────────────────

// 2.4 or 5GHz from the frequency the device reports, or null when it has not
// reported one (old firmware, or wpa_cli unavailable when the stat was taken).
// Null rather than a guess: "2.4GHz" shown for a device we cannot actually
// see the band of is worse than showing nothing, since the whole point is
// spotting a device that has quietly landed on the slower radio.
function wifiBand(freqMhz) {
  if (!freqMhz) return null;
  return freqMhz >= 4900 ? '5GHz' : '2.4GHz';
}

function SignalBars({ rssi }) {
  // 0 bars = no signal / null, 4 bars = excellent
  const level = rssi == null ? 0
              : rssi > -60   ? 4
              : rssi > -70   ? 3
              : rssi > -80   ? 2
              : rssi > -90   ? 1
              :                0;
  const off = 'var(--track)';
  const bars = [{h:4,y:11},{h:7,y:8},{h:10,y:5},{h:14,y:1}];
  return (
    <svg width={20} height={16} style={{ display:'block', flexShrink:0 }}>
      {bars.map((b,i) => (
        <rect key={i} x={i*5} y={b.y} width={4} height={b.h} rx={1}
          fill={i < level ? (level===1?'var(--error)':level===2?'var(--warn)':'var(--ok)') : off}/>
      ))}
    </svg>
  );
}

// Severity palette, shared with StatBar and SignalBars. The panel previously
// carried two: these desaturated tones and a brighter set (var(--error) and
// friends) that had crept in on the latency row, which is why the scalar
// metrics read as bolted on rather than designed.
const SEVERITY = Object.freeze({ OK: 'ok', WARN: 'warn', BAD: 'bad' });
const SEV = Object.freeze({
  [SEVERITY.OK]: 'var(--ok)', [SEVERITY.WARN]: 'var(--warn)', [SEVERITY.BAD]: 'var(--error)',
});

// MicroMeter is a 2px severity bar. It exists so the scalar metrics
// (link/latency/temp) share a visual grammar with the capacity bars above
// them, instead of being three bare numbers stacked at the end of the panel.
function MicroMeter({ pct, sev }) {
  return (
    <div style={{ height:2, borderRadius:1, background:'var(--track)', overflow:'hidden', marginTop:5 }}>
      {pct != null && <div style={{ height:'100%', width:`${Math.max(2, Math.min(100, pct))}%`,
        background:SEV[sev] ?? SEV[SEVERITY.OK], borderRadius:1, transition:'width 0.6s' }}/>}
    </div>
  );
}

// StatTile is one cell of the scalar row: label, value, optional glyph, and a
// headroom meter. `note` carries an exception worth seeing (thermal
// throttling), which is the only thing here that should ever shout.
//
// `sub` is the quiet counterpart: context that qualifies the value without
// being a problem, like which WiFi band a link reading was taken on. It is
// deliberately a separate prop rather than a second use of `note`, because
// `note` is red — routing neutral information through it would make every
// device look like it was in trouble.
function StatTile({ label, value, unit, sev = SEVERITY.OK, pct, glyph, note, sub }) {
  const dim = value == null;
  return (
    <div style={{ flex:'1 1 0', minWidth:0 }}>
      <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)',
                    textTransform:'uppercase', letterSpacing:'0.08em', whiteSpace:'nowrap' }}>{label}</div>
      {/* FIXED height, and the glyph is centre-aligned rather than
          baseline-aligned.

          A flex item with no text baseline of its own — Link's SignalBars is
          a 16px SVG — has its BOTTOM EDGE aligned to the row's baseline, so
          it sits taller than the text and grew the whole row. That pushed the
          Link tile's meter several pixels below Latency's and Temp's, whose
          glyphs are ordinary text. Pinning the height decouples the meters'
          vertical position from whatever a tile puts in this row, so the row
          of meters lines up by construction rather than by coincidence. */}
      <div style={{ display:'flex', alignItems:'baseline', gap:4, marginTop:3, height:18 }}>
        <span style={{ fontFamily:MONO, fontSize:12,
                       color: dim ? 'var(--muted)' : SEV[sev] ?? 'var(--text2)' }}>
          {dim ? '—' : value}
        </span>
        {!dim && unit && <span style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>{unit}</span>}
        {glyph && <span style={{ marginLeft:'auto', display:'flex', alignItems:'center',
                                 alignSelf:'center', flexShrink:0 }}>{glyph}</span>}
      </div>
      <MicroMeter pct={dim ? null : pct} sev={sev}/>
      {note && <div style={{ fontFamily:MONO, fontSize:9, color:SEV[SEVERITY.BAD], marginTop:3 }}>{note}</div>}
      {!note && sub && <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:3 }}>{sub}</div>}
    </div>
  );
}

function StatBar({ label, pct, text }) {
  const color = pct == null ? 'transparent'
              : pct > 85   ? 'var(--error)'
              : pct > 65   ? 'var(--warn)'
              :               'var(--ok)';
  return (
    <div style={{ marginBottom: 13 }}>
      <div style={{ display:'flex', justifyContent:'space-between', marginBottom:5 }}>
        <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', textTransform:'uppercase', letterSpacing:'0.08em' }}>{label}</span>
        <span style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)' }}>{text ?? '—'}</span>
      </div>
      <div style={{ height:3, borderRadius:2, background:'var(--track)', overflow:'hidden' }}>
        {pct != null && <div style={{ height:'100%', width:`${pct}%`, background:color, borderRadius:2, transition:'width 0.6s' }}/>}
      </div>
    </div>
  );
}



function LedRing({ state, size = 120 }) {
  const cx = size / 2, cy = size / 2, r = size * 0.38;
  const stateKey = state?.key || DEVICE_STATE.IDLE;
  const stateColor = state?.dot || '#aaaaaa';
  const isPending = stateKey === DEVICE_STATE.PENDING;
  const isOffline = stateKey === DEVICE_STATE.OFFLINE;

  const ledColor = isPending ? '#c8c8c8'
                 : isOffline ? '#d4703a'
                 : stateKey === DEVICE_STATE.MUTED ? '#c04040'
                 : stateKey === DEVICE_STATE.SPEAKING ? '#4080d0'
                 : stateKey === DEVICE_STATE.LISTENING ? '#40906a'
                 : stateKey === DEVICE_STATE.THINKING ? '#a08020'
                 : '#3a4a30';

  const shouldPulse = isPending || isOffline;
  const circumference = 2 * Math.PI * (size * 0.38);
  const segLen = circumference / 12 * 0.72;
  const gapLen = circumference / 12 * 0.28;

  return (
    <svg width={size} height={size} style={{ display: 'block', flexShrink: 0 }}>
      <defs>
        <radialGradient id={`shell-${size}`} cx="38%" cy="32%" r="65%">
          <stop offset="0%" stopColor="#505050"/>
          <stop offset="55%" stopColor="#2c2c2c"/>
          <stop offset="100%" stopColor="#181818"/>
        </radialGradient>
        <radialGradient id={`inner-${size}`} cx="42%" cy="36%" r="58%">
          <stop offset="0%" stopColor="#383838"/>
          <stop offset="100%" stopColor="#202020"/>
        </radialGradient>
        <filter id={`glow-${size}`} x="-60%" y="-60%" width="220%" height="220%">
          <feGaussianBlur stdDeviation="2.5" result="blur"/>
          <feMerge><feMergeNode in="blur"/><feMergeNode in="SourceGraphic"/></feMerge>
        </filter>
        <clipPath id={`clip-${size}`}><circle cx={cx} cy={cy} r={size*0.47}/></clipPath>
      </defs>
      <circle cx={cx} cy={cy} r={size*0.49} fill="#0d0d0d"/>
      <circle cx={cx} cy={cy} r={size*0.47} fill={`url(#shell-${size})`}/>
      <circle cx={cx} cy={cy} r={size*0.47} fill="none" stroke="rgba(255,255,255,0.07)" strokeWidth="1.2"/>
      <g clipPath={`url(#clip-${size})`}>
        <circle cx={cx} cy={cy} r={r} fill="none" stroke="#0b0b0b" strokeWidth={size*0.065}/>
        <circle cx={cx} cy={cy} r={r} fill="none"
          stroke={ledColor} strokeWidth={size*0.045}
          strokeDasharray={`${segLen} ${gapLen}`}
          transform={`rotate(-90 ${cx} ${cy})`}
          filter={stateKey !== DEVICE_STATE.IDLE ? `url(#glow-${size})` : undefined}
          style={shouldPulse ? { animation: 'ledpulse 1.8s ease-in-out infinite' } : undefined}
        />
        <circle cx={cx} cy={cy} r={r} fill="none"
          stroke="#141414" strokeWidth={size*0.065}
          strokeDasharray={`1.5 ${circumference/12 - 1.5}`}
          transform={`rotate(-90 ${cx} ${cy})`}
        />
      </g>
      <circle cx={cx} cy={cy} r={size*0.36} fill={`url(#inner-${size})`}/>
      <circle cx={cx} cy={cy} r={size*0.36} fill="none" stroke="rgba(255,255,255,0.04)" strokeWidth="0.8"/>
      <circle cx={cx} cy={cy} r={size*0.09} fill={stateColor} style={{ transition: 'fill 0.4s' }}
        filter={stateKey !== DEVICE_STATE.IDLE ? `url(#glow-${size})` : undefined}/>
      <ellipse cx={cx - size*0.07} cy={cy - size*0.08} rx={size*0.09} ry={size*0.055} fill="rgba(255,255,255,0.06)"/>
    </svg>
  );
}

// ─── Shell terminal ───────────────────────────────────────────────────────────

// Real terminal (xterm.js) over the device shell WebSocket.
//
// Mode is decided by the controller's shell_meta message (sent before any
// shell bytes):
//   pty:true  — device attached sh to a pseudo-terminal. Keystrokes go
//               raw in framed binary messages (0x00 = stdin, 0x01 =
//               resize cols/rows u16 BE); mksh does echo, line editing,
//               prompts, and full-screen apps (top, vi) work.
//   pty:false — pre-PTY firmware: raw pipe, no echo, no framing. Local
//               echo + line buffering emulate the old input box.
function Shell({ deviceId, token, height = 320 }) {
  const containerRef = useRef(null);

  useEffect(() => {
    const term = new window.Terminal({
      fontSize: 12,
      fontFamily: MONO,
      cursorBlink: true,
      scrollback: 5000,
      theme: {
        background: '#1c1f18', foreground: '#c8d4b0',
        cursor: '#9aba80', cursorAccent: '#1c1f18',
        selectionBackground: '#3a4430',
      },
    });
    const fit = new window.FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current);
    fit.fit();

    const sock = new WebSocket(ingressWebSocketUrl(`/api/devices/${deviceId}/shell?token=${token}`));
    sock.binaryType = 'arraybuffer';

    let pty = null;    // null until shell_meta arrives
    let lineBuf = '';  // legacy-mode local line buffer

    const sendResize = () => {
      if (pty !== true || sock.readyState !== 1) return;
      const b = new Uint8Array(5);
      b[0] = 0x01;
      b[1] = term.cols >> 8; b[2] = term.cols & 0xff;
      b[3] = term.rows >> 8; b[4] = term.rows & 0xff;
      sock.send(b);
    };

    sock.onopen = () => term.write(`\x1b[2mshell — ${deviceId}\x1b[0m\r\n`);
    sock.onmessage = e => {
      if (typeof e.data === 'string') {
        try {
          const m = JSON.parse(e.data);
          if (m.type === 'shell_meta') {
            pty = !!m.pty;
            if (pty) sendResize();
            else term.write('\x1b[2m[firmware has no PTY support — line mode; update the device for a full terminal]\x1b[0m\r\n');
            return;
          }
        } catch {}
        term.write(e.data);
        return;
      }
      term.write(new Uint8Array(e.data));
    };
    sock.onclose = () => term.write('\r\n\x1b[2mdisconnected\x1b[0m\r\n');
    sock.onerror = () => term.write('\r\n\x1b[31mconnection error\x1b[0m\r\n');

    const dataSub = term.onData(d => {
      if (sock.readyState !== 1) return;
      if (pty === true) {
        const enc = new TextEncoder().encode(d);
        const b = new Uint8Array(enc.length + 1);
        b[0] = 0x00;
        b.set(enc, 1);
        sock.send(b);
      } else {
        // Legacy pipe: sh has no TTY, so echo and line editing happen here.
        for (const ch of d) {
          if (ch === '\r') { term.write('\r\n'); sock.send(lineBuf + '\n'); lineBuf = ''; }
          else if (ch === '\x7f') { if (lineBuf) { lineBuf = lineBuf.slice(0, -1); term.write('\b \b'); } }
          else if (ch === '\x03') { sock.send('\x03'); term.write('^C\r\n'); lineBuf = ''; }
          else if (ch >= ' ') { lineBuf += ch; term.write(ch); }
        }
      }
    });
    const resizeSub = term.onResize(() => sendResize());
    const ro = new ResizeObserver(() => { try { fit.fit(); } catch {} });
    ro.observe(containerRef.current);

    return () => {
      ro.disconnect();
      dataSub.dispose();
      resizeSub.dispose();
      sock.close();
      term.dispose();
    };
  }, [deviceId, token]);

  return (
    <div style={{ background: '#1c1f18', border: '1px solid #1a1c16', borderRadius: 6, boxShadow: 'inset 0 2px 6px rgba(0,0,0,0.6)', padding: 10, height }}>
      <div ref={containerRef} style={{ height: '100%', width: '100%' }}/>
    </div>
  );
}

// ─── Recorded audio ───────────────────────────────────────────────────────────

// Plays recordings one at a time, each fetched through API.blob and held as an
// object URL (the browser cannot fetch them itself — see API.request).
// Starting a clip stops whichever was sounding; `playing` is its key, and a
// fetch overtaken by a later click never starts.
//
// With `cache` (the default) a clip's URL is kept for replay and download —
// playing then downloading costs one transfer, not two — until `forget`, a
// change of `scope`, or unmount; every object URL pins its blob in memory
// until revoked. Without it a URL lives only while its clip is the current
// one, for recordings too large to hold on to (ambient sessions run to tens of
// megabytes), and `url()` hands the caller a fresh URL that is theirs to
// revoke. A clip may play one stretch of itself (`stretch` {from, to}, seconds):
// `segment` is the stretch sounding, null while a whole clip plays.
function useClipPlayer(scope, { cache = true } = {}) {
  const [playing, setPlaying] = useState(null);
  const [segment, setSegment] = useState(null);
  const audioRef   = useRef(null);
  const urlsRef    = useRef({});    // key -> cached object URL
  const currentRef = useRef(null);  // uncached: the current clip's URL
  const seqRef     = useRef(0);     // bumped on every stop
  const timerRef   = useRef(null);  // ends a stretch

  const stop = () => {
    seqRef.current++;
    clearTimeout(timerRef.current); timerRef.current = null;
    if (audioRef.current) { audioRef.current.pause(); audioRef.current = null; }
    if (currentRef.current) { URL.revokeObjectURL(currentRef.current); currentRef.current = null; }
    setPlaying(null); setSegment(null);
  };

  const url = async (key, path) => {
    if (cache && urlsRef.current[key]) return urlsRef.current[key];
    const u = URL.createObjectURL(await API.blob(path));
    if (cache) urlsRef.current[key] = u;
    return u;
  };

  // Resolves false when the recording could not be fetched. Pressed again for
  // what is sounding, it stops; for another stretch of the same clip, it plays that.
  const toggle = async (key, path, stretch = null) => {
    const sameStretch = (segment?.from === stretch?.from && segment?.to === stretch?.to);
    const wasPlaying = playing === key && sameStretch;
    stop();
    if (wasPlaying) return true;
    const seq = seqRef.current;
    let u;
    try { u = await url(key, path); } catch { return false; }
    if (seq !== seqRef.current) {           // stopped or replaced meanwhile
      if (!cache) URL.revokeObjectURL(u);
      return true;
    }
    if (!cache) currentRef.current = u;
    const el = new Audio(u);
    const ended = () => { setPlaying(p => (p === key ? null : p)); setSegment(null); };
    el.onended = el.onerror = ended;
    if (stretch) {
      // Before the metadata loads this sets where playback starts. The timer
      // starts (again) whenever sound does, so a stall cannot cut the stretch short.
      el.currentTime = stretch.from;
      el.addEventListener('playing', () => {
        clearTimeout(timerRef.current);
        timerRef.current = setTimeout(() => { if (seq === seqRef.current) stop(); },
          Math.max(0, (stretch.to - el.currentTime) * 1000));
      });
    }
    audioRef.current = el;
    setPlaying(key); setSegment(stretch);
    el.play().catch(ended);
    return true;
  };

  const forget = key => {
    if (!urlsRef.current[key]) return;
    URL.revokeObjectURL(urlsRef.current[key]);
    delete urlsRef.current[key];
  };

  const forgetAll = () => {
    Object.values(urlsRef.current).forEach(URL.revokeObjectURL);
    urlsRef.current = {};
  };

  // The sounding clip's playhead in seconds; null when nothing is playing.
  const elapsed = () => (audioRef.current ? audioRef.current.currentTime : null);

  // Only refs and the state setter are touched, so the first render's
  // closures are safe to run at teardown.
  useEffect(() => () => { stop(); forgetAll(); }, [scope]);

  return { playing, segment, toggle, stop, url, forget, forgetAll, elapsed };
}

// ─── Turn observability (Activity tab) ───────────────────────────────────────
// Stat tiles and a bar that breaks down the stages for each recent turn. Colors
// passed six dataviz checks against the panel surface #dfdbd3: CVD ΔE 26.3,
// contrast ≥3:1, chroma ≥0.1. Identity is never color alone: the legend and
// tooltip name each stage.
const TURN_STAGES = [
  { key: 'listen',     label: 'Listening',  color: '#4468a8' },
  { key: 'transcribe', label: 'Transcribe', color: '#1f8a55' },
  { key: 'respond',    label: 'Respond',    color: '#96660a' },
];

// The stage durations stored in one turn row. The row stores durations, not offsets:
// audio_ms is the committed span (the whole time it was open when nothing
// committed), stt_ms the STT run, and respond is intent dispatch → first TTS
// URL. An unmeasured stage (null) contributes nothing.
function turnSegments(t) {
  const ms = v => (typeof v === 'number' && v > 0 ? v : 0);
  const listen     = t.commit_id ? ms(t.audio_ms) : ms(t.total_ms);
  const transcribe = ms(t.stt_ms);
  const respond    = ms(t.tts_url_ms);
  return { listen, transcribe, respond, shown: listen + transcribe + respond };
}

// turns.response_latency_ms: the end of the user's last word → the reply's
// first frame played, both on the Echo's own clock. Null: nothing played, no
// device timing, or a row older than the measurement.
const RESPONSE_LATENCY_TITLE = 'Response latency: the end of your last word to the first audio of the reply, timed on the Echo';

// Terminal reasons (§7) that mean a wake candidate was refused rather than a
// user's request failing: reported, but not counted against success.
const REFUSED_WAKE = new Set([
  TERMINAL_REASON.SELF_OUTPUT, TERMINAL_REASON.ECHO_ONLY, TERMINAL_REASON.UNVERIFIED_WAKE,
  TERMINAL_REASON.VERIFIER_TIMEOUT, TERMINAL_REASON.ARBITRATION_LOST,
]);

// Endings that are neither success nor failure: a silent reply window
// expiring, or a newer wake/button press taking the turn over.
const NEUTRAL_END = new Set([TERMINAL_REASON.REPLY_TIMEOUT, TERMINAL_REASON.SUPERSEDED]);

function turnReasonColor(reason) {
  if (reason === TERMINAL_REASON.COMPLETED) return 'var(--ok)';
  if (REFUSED_WAKE.has(reason) || NEUTRAL_END.has(reason)) return 'var(--muted)';
  return 'var(--warn)';
}

// A reply turn opens when its question finishes playing; nobody answering it
// (no_input) is a window expiring, not a failure.
const isReplyTrigger = trigger => trigger === TURN_TRIGGER.REPLY || trigger === TURN_TRIGGER.HA_REPLY;

// A row from before the post-AFE cutover has no terminal reason (every
// current turn is persisted at CLOSING with one); its ending is its legacy
// outcome, where "ok" meant success.
const isLegacyTurn = t => t.terminal_reason == null;
function turnEnd(t) {
  if (!isLegacyTurn(t)) {
    const quiet = t.terminal_reason === TERMINAL_REASON.NO_INPUT && isReplyTrigger(t.trigger);
    return { label: t.terminal_reason, color: quiet ? 'var(--muted)' : turnReasonColor(t.terminal_reason) };
  }
  const label = t.outcome || 'unknown';
  return { label, color: label === LEGACY_OUTCOME.OK ? 'var(--ok)'
                       : label === LEGACY_OUTCOME.CANCELLED ? 'var(--muted)' : 'var(--warn)' };
}
const turnSucceeded = t => (isLegacyTurn(t) ? t.outcome === LEGACY_OUTCOME.OK
                                            : t.terminal_reason === TERMINAL_REASON.COMPLETED);

// Turn times always carry the date: the list spans days.
const fmtTurnWhen = ts => new Date(ts * 1000).toLocaleString([], {
  month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
const fmtTurnWhenFull = ts => new Date(ts * 1000).toLocaleString([], {
  weekday: 'short', year: 'numeric', month: 'short', day: 'numeric',
  hour: '2-digit', minute: '2-digit', second: '2-digit' });

// How the endpoint reducer ended the utterance (§16.6 routes).
// Keyed by em_endpoint_policy.Route values (turns.commit_route).
const COMMIT_ROUTES = Object.freeze({
  A: 'pause after the command',
  B: 'complete command under background speech',
  R: 'pause in a reply to a Home Assistant prompt',
  fallback: 'stable complete prefix while speech continued',
});

// Whose words the endpoint committed on (§16.6): the commit event's `source`
// in a turn's decision trace; mirrors em_utterance.TextSource. KROKO and
// SERVER are also the keys of each pause in `utterance.pauses`.
const TEXT_SOURCE = Object.freeze({ STREAMING: 'streaming', KROKO: 'kroko', SERVER: 'server' });
const TEXT_SOURCES = Object.freeze({
  [TEXT_SOURCE.STREAMING]: "Kroko's streaming words (no pause decode stood)",
  [TEXT_SOURCE.KROKO]:     "Kroko's words at the pause",
  [TEXT_SOURCE.SERVER]:    "the Wyoming server's words",
});

// Who took the request after transcription.
const TURN_HANDLERS = Object.freeze({
  [TURN_OUTCOME.HA]:            'Home Assistant intent',
  [TURN_OUTCOME.ALARM]:         'EchoMuse alarm engine',
  [TURN_OUTCOME.TIMER]:         'EchoMuse timer cancel → Home Assistant HassCancelTimer (no conversation agent)',
  [TURN_OUTCOME.ALARM_QUERY]:   'EchoMuse alarm question (alarm engine)',
  [TURN_OUTCOME.CLARIFICATION]: 'EchoMuse (asked for AM or PM)',
  [TURN_OUTCOME.REPEAT]:        'EchoMuse replay of the last response (no conversation agent)',
  [TURN_OUTCOME.LOCAL_COMMAND]: 'Local command on the controller (Home Assistant not involved)',
});

const TRIGGERS = Object.freeze({
  [TURN_TRIGGER.WAKE]:     'Wake word',
  [TURN_TRIGGER.BUTTON]:   'Action button',
  [TURN_TRIGGER.REPLY]:    'Reply (no wake word)',
  [TURN_TRIGGER.HA_REPLY]: 'Reply to a Home Assistant prompt',
});

// What became of a question a turn's answer asked (turns.continuation: a
// FollowUp, or the TerminalReason that ended the reply expectation):
// [explanation, color].
const FOLLOWUPS = Object.freeze({
  [FOLLOW_UP.PENDING]:              ['reply window open', 'var(--muted)'],
  [FOLLOW_UP.ANSWERED]:             ['answered without the wake word', 'var(--ok)'],
  [FOLLOW_UP.WAKE]:                 ['a wake word or button press took over, with the question as context', 'var(--ok)'],
  [TERMINAL_REASON.REPLY_TIMEOUT]:  ['no reply within the 7 s reply window', 'var(--muted)'],
  [FOLLOW_UP.PROMPT_CANCELLED]:     ['the question was cut off before it finished, so no reply window opened', 'var(--warn)'],
  [FOLLOW_UP.PROMPT_FAILED]:        ['the question never finished playing (speech failed or timed out)', 'var(--warn)'],
  [FOLLOW_UP.NO_MIC]:               ['no reply window: the microphone stream was unavailable', 'var(--warn)'],
  [FOLLOW_UP.CHAIN_LIMIT]:          ['no reply window: already 5 replies or 60 s without the wake word', 'var(--muted)'],
  [TERMINAL_REASON.INTERRUPTED]:    ['the reply window’s microphone stream ended early', 'var(--warn)'],
  [TERMINAL_REASON.SUPERSEDED]:     ['a newer wake or button press ended it', 'var(--muted)'],
  [TERMINAL_REASON.MUTED]:          ['the microphone was muted', 'var(--muted)'],
  [TERMINAL_REASON.SESSION_LOST]:   ['the device disconnected', 'var(--warn)'],
});
const followup = key => FOLLOWUPS[key] || [key.replace(/_/g, ' '), 'var(--muted)'];
// Outcomes that mean the follow-up machinery failed rather than the user not answering.
const FOLLOWUP_BROKEN = new Set([
  FOLLOW_UP.PROMPT_CANCELLED, FOLLOW_UP.PROMPT_FAILED, FOLLOW_UP.NO_MIC,
  TERMINAL_REASON.INTERRUPTED, TERMINAL_REASON.SESSION_LOST,
]);

// How the spoken answer ended (render.finished reason, or a response limit).
const PLAYBACK_ENDS = Object.freeze({
  [PLAYBACK_END.DRAINED]:          'played to the end',
  [PLAYBACK_END.CANCELLED]:        'cut off',
  [PLAYBACK_END.FAILED]:           'failed to play',
  [PLAYBACK_END.UNDERRUN]:         'ran out of audio',
  [PLAYBACK_END.RESPONSE_TIMEOUT]: 'stalled or timed out',
  [PLAYBACK_END.FENCED]:           'superseded',
});

// Transcript correction (§16.8): a turn's `stt_fix`, the JSON string
// em_text_fix.FixRecord writes (null: correction was off, or the turn never
// reached it). Malformed JSON is ignored rather than shown half-read.
function parseTextFix(raw) {
  if (raw == null) return null;
  try {
    const v = typeof raw === 'string' ? JSON.parse(raw) : raw;
    return v && typeof v === 'object' && !Array.isArray(v) ? v : null;
  } catch (e) { return null; }
}
// em_text_fix.Reason — why the turn sent what it sent: [words, color].
const TEXT_FIX_REASONS = Object.freeze({
  applied:            ['the correction was sent', 'var(--ok)'],
  would_apply:        ['shadow: would have sent', 'var(--accent)'],
  original_matched:   ["Home Assistant's matcher already understood the heard text", 'var(--muted)'],
  no_candidate:       ['nothing to propose', 'var(--muted)'],
  no_valid_candidate: ["no candidate kept what was said and passed Home Assistant's matcher", 'var(--muted)'],
  ambiguous:          ['ambiguous: interpretations too close to choose between', 'var(--warn)'],
  contextual:         ['a follow-up in a conversation: bypassed', 'var(--muted)'],
  not_ready:          ["not ready: tables not built or Home Assistant's matcher unavailable", 'var(--warn)'],
  timeout:            ['not ready within 100 ms of the final transcript', 'var(--warn)'],
  error:              ['failed', 'var(--error)'],
  stale:              ['computed under a context that no longer held', 'var(--warn)'],
});
// em_text_fix.Rule / Source / Status.
const TEXT_FIX_RULES = Object.freeze({
  slot: 'slot name', name: 'name span', lead_in: 'wake-word lead-in',
  phrase: 'whole phrase', cutoff: 'cut-off phrase', numbers: 'number words',
});
const TEXT_FIX_SOURCES = Object.freeze({
  trigger: 'an HA trigger sentence', history: 'an earlier request HA answered',
  user_phrase: 'a preferred phrase', user_term: 'a preferred term', entity: 'an HA entity name',
  area: 'an HA area', floor: 'an HA floor', wake: 'the wake phrase', numbers: 'digits',
});
const TEXT_FIX_STATUSES = Object.freeze({
  ok: 'eligible', not_preserved: 'drops or changes what was said',
  not_validated: 'not checked (past the validation limit)', no_match: "HA's matcher does not accept it",
  catch_all: 'matched only by a free-text slot or wildcard', unresolved: 'the name did not resolve to its target',
  changed_resolved: 'changes a slot HA had resolved',
});
const fixWord = (map, k) => (k == null ? '—' : map[k] || String(k).replace(/_/g, ' '));

// A turn's correction record (turns.stt_fix). `trial`: a tried transcript's record, as On
// would send it (POST /api/text_fix/trial): worded as what would happen, with no mode.
function TextFixEvidence({ fix: f, quote, trial = false, defaultOpen = false }) {
  const [open, setOpen] = useState(defaultOpen);
  const muted = { color: 'var(--muted)' };
  const [reason, color] = trial && f.reason === 'applied' ? ['On would send the correction', 'var(--ok)']
    : TEXT_FIX_REASONS[f.reason] || [fixWord({}, f.reason), 'var(--muted)'];
  const num = v => (typeof v === 'number' ? v.toFixed(2) : '—');
  const candidates = Array.isArray(f.candidates) ? f.candidates : [];
  const verdicts = f.verdict && typeof f.verdict === 'object' ? f.verdict : null;
  // A record's verdict is null when HA's matcher recognized nothing (it is always asked).
  const verdict = v => (!v ? 'nothing recognized'
    : v.match ? `matched${v.intent ? ` ${v.intent}` : ''}${v.template ? ` via “${v.template}”` : ''}` : 'no match');
  const proposed = f.proposed != null;
  const timing = [
    f.cache && `cache ${f.cache}`,
    typeof f.latency_ms === 'number' && `${f.latency_ms} ms after the final transcript`,
    f.deadline_met != null && (f.deadline_met ? 'within the 100 ms deadline' : 'missed the 100 ms deadline'),
    typeof f.compute_ms === 'number' && `computed in ${f.compute_ms} ms`,
  ].filter(Boolean);
  return (
    <>
      <div>
        heard {quote(f.original)}
        {' → '}{proposed ? <>proposed {quote(f.proposed)}</> : <span style={muted}>no proposal</span>}
        {' → '}{trial ? 'would send' : 'sent'} {quote(f.sent)}
      </div>
      <div>
        <span style={{ color: f.applied ? 'var(--ok)' : 'var(--text2)' }}>
          {f.applied ? (trial ? 'would apply' : 'applied') : (trial ? 'would not apply' : 'not applied')}
        </span>
        {' · '}<span style={{ color }}>{reason}</span>
        {f.mode && !trial && <span style={muted}> · mode {f.mode}</span>}
      </div>
      {proposed && (
        <div style={muted}>
          {[`${fixWord(TEXT_FIX_RULES, f.rule)} from ${fixWord(TEXT_FIX_SOURCES, f.source)}`,
            `similarity ${num(f.raw)}`,
            f.coverage != null && `coverage ${num(f.coverage)}`,
            f.status && f.status !== 'ok' && fixWord(TEXT_FIX_STATUSES, f.status),
            f.preference_changed ? 'your preference chose this spelling'
              : f.preference > 0 ? `preference +${f.preference.toFixed(3)} (did not change the choice)` : null,
          ].filter(Boolean).join(' · ')}
        </div>
      )}
      {timing.length > 0 && <div style={muted}>{timing.join(' · ')}</div>}
      {f.error && <div style={{ color: 'var(--error)' }}>{f.error}</div>}
      {(candidates.length > 0 || verdicts) && (
        <div style={{ marginTop: 2 }}>
          <DisclosureToggle open={open} onToggle={() => setOpen(o => !o)}>
            Evidence{candidates.length ? ` · ${candidates.length} candidate${candidates.length === 1 ? '' : 's'}` : ''}
          </DisclosureToggle>
          {open && (
            <div style={{ marginTop: 4 }}>
              {verdicts && (
                <div style={muted}>
                  HA on the heard text: {verdict(verdicts.original)}
                  {proposed && <> · on the proposal: {verdict(verdicts.proposed)}</>}
                </div>
              )}
              {candidates.map((c, i) => (
                <div key={i} style={{ display: 'grid', gridTemplateColumns: '1fr auto', columnGap: 8 }}>
                  <span>
                    {quote(c.text)} <span style={muted}>{fixWord(TEXT_FIX_RULES, c.rule)} from {fixWord(TEXT_FIX_SOURCES, c.source)}</span>
                    {c.matched && <span style={muted} title={c.matched}> · HA: {c.intent || 'trigger'} “{c.matched}”</span>}
                  </span>
                  <span style={{ ...muted, textAlign: 'right' }}>
                    {[`${num(c.raw)}`, c.coverage != null && `cov ${num(c.coverage)}`,
                      c.preference > 0 && `pref +${c.preference.toFixed(3)}`, `rank ${num(c.rank)}`].filter(Boolean).join(' · ')}
                    {' · '}<span style={{ color: c.status === 'ok' ? 'var(--ok)' : 'var(--muted)' }}>{fixWord(TEXT_FIX_STATUSES, c.status)}</span>
                    {c.detail ? ` (${c.detail})` : ''}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </>
  );
}

// A turn's native AFE evidence (turns.afe_evidence), charted period by
// period: null is unavailable — the Echo did not report it. The span summaries
// stay in the decision trace and the turns API.
function NativeAfeEvidence({ evidence, deviceId, turnId, playback }) {
  if (evidence == null) {
    return <span style={{ color: 'var(--muted)' }}>unavailable — not reported for this turn ({CAPABILITY_REASONS[CAPABILITY.AFE_METADATA]}), or recorded before it existed</span>;
  }
  return <AfeChart deviceId={deviceId} turnId={turnId} onset={evidence.playback_onset} playback={playback}/>;
}

const CAPTURE_RATE = 16000;     // capture samples per second
const AFE_PERIOD_FRAMES = 10;   // em_audio_timeline.AFE_PERIOD_FRAMES: AFE frames per 80 ms period
const SILERO_CELL = 512;        // em_speech_worker.VAD_CELL_SAMPLES: one Silero analysis cell (32 ms)

// One lane's values as step paths in period units (y = 0 top … 1 bottom;
// values are 0–1 of the lane's scale), value i spanning x0 + i·dx … + dx: a
// period index by default, a Silero cell's place with x0 and dx. A null value
// breaks both paths: a period without data is a gap, never a zero.
function afeStepPaths(ys, x0 = 0, dx = 1) {
  let area = '', line = '', open = false;
  const x = i => +(x0 + i * dx).toFixed(3);
  ys.forEach((y, i) => {
    if (y == null) {
      if (open) area += 'V1Z';
      open = false;
      return;
    }
    const v = (1 - 0.94 * Math.max(0, Math.min(1, y))).toFixed(3);
    area += open ? `V${v}H${x(i + 1)}` : `M${x(i)},1V${v}H${x(i + 1)}`;
    line += open ? `V${v}H${x(i + 1)}` : `M${x(i)},${v}H${x(i + 1)}`;
    open = true;
  });
  if (open) area += 'V1Z';
  return { area, line };
}

// The AfeSeriesJson key placing each of a turn's recordings on the capture
// timeline: the chart's own button and a row's buttons drive one playhead.
const AFE_CLIP_SPANS = Object.freeze({
  [TURN_CLIP.WAKE]: 'wake_clip', [TURN_CLIP.MIC]: 'audio_clip', [TURN_CLIP.RECORDING]: 'recording',
});

const turnStageColor = key => TURN_STAGES.find(st => st.key === key).color;

// em_afe.TurnStage: a committed turn's processing, as bars beneath its
// utterance, colored as the stage breakdown above. Endpoint is the silence
// waited, in sample time; the rest come from the controller's clock.
const AFE_STAGE_ROWS = [
  { key: 'endpoint', label: 'Endpoint', color: turnStageColor('listen') },
  { key: 'stt',      label: 'STT',      color: turnStageColor('transcribe') },
  { key: 'intent',   label: 'Intent',   color: turnStageColor('respond') },
  { key: 'tts',      label: 'TTS',      color: turnStageColor('respond') },
  { key: 'reply',    label: 'Reply',    color: 'var(--text2)' },
];

// One AfeSeriesJson fetched from `url`: null while it loads, then { series },
// or { series: null, error } — a 404 is "no chart" and leaves error null.
function useAfeSeries(url) {
  const [load, setLoad] = useState({ url: null });
  useEffect(() => {
    let live = true;
    API.get(url)
      .then(series => { if (live) setLoad({ url, series }); })
      .catch(e => { if (live) setLoad({ url, series: null, error: e.status === 404 ? null : (e.error || e.message || 'request failed') }); });
    return () => { live = false; };
  }, [url]);
  return load.url === url ? load : null;
}

// What an AFE chart draws: a recording's the four native values, a turn's
// also the controller's Silero VAD beside the native one, its Playback lane
// and its Frames and Flags strips.
const AFE_LANES = Object.freeze({
  RECORDING: Object.freeze(['vad', 'dtd', 'erle', 'rms']),
  TURN:      Object.freeze(['vad', 'silero', 'dtd', 'erle', 'rms', 'playback', 'frames', 'flags']),
});

// One AFE_LANES row of series `s`: a value lane (traces as step paths, each
// on a fixed scale so charts compare by eye) or a strip; null when it has
// nothing to draw. `read(i)` is the row's part of period i's readout; `own`
// marks a row whose values do not come from the period's AFE record.
function afeLaneRow(s, key) {
  const scaled = (k, f) => s[k].map(v => (v == null ? null : f(v)));
  const fr = k => `${k} fr`;
  const valued = i => s.frames[i] > 0;    // a record with frames carries values
  const lane = (label, scale, color, traces, read) => ({
    label, scale, color, read, traces: traces.map(tr => ({ ...tr, paths: afeStepPaths(tr.ys, tr.x0, tr.dx) })),
  });
  switch (key) {
    case 'vad':
      return lane('Native VAD', '0–0.75', 'var(--ok)', [{ ys: scaled('vad_max', v => v / 0.75), fill: true }],
        i => valued(i) && `native VAD max ${s.vad_max[i].toFixed(2)} over ${fr(s.vad_frames[i])}`);
    case 'silero': {
      // At Silero's own 32 ms cells; a period reads the cells overlapping it.
      // Absent (an older row) or empty (a near miss, a recording): no lane.
      if (!s.silero?.length) return null;
      const cellsIn = i => {
        const a = s.start + i * s.period - s.silero_start;
        return s.silero.slice(Math.max(0, Math.floor(a / SILERO_CELL)), Math.max(0, Math.ceil((a + s.period) / SILERO_CELL)))
          .filter(p => p != null);
      };
      return { ...lane('Silero VAD', '0–1', 'var(--ok)',
        [{ ys: s.silero, fill: true, x0: (s.silero_start - s.start) / s.period, dx: SILERO_CELL / s.period }],
        i => { const ps = cellsIn(i); return ps.length > 0 && `Silero max ${Math.max(...ps).toFixed(2)} over ${ps.length} cells`; }),
      own: true };
    }
    case 'dtd':
      return lane('DTD', '0–1', 'var(--warn)', [{ ys: scaled('dtd_max', v => v), fill: true }],
        i => valued(i) && `DTD max ${s.dtd_max[i].toFixed(2)} over ${fr(s.dtd_frames[i])}`);
    case 'erle': {
      const top = s.erle_max.reduce((m, v) => (v != null && v > m ? v : m), 30);
      return lane('ERLE', `0–${top}`, 'var(--accent)', [{ ys: scaled('erle_max', v => v / top), fill: true }],
        i => valued(i) && (s.erle_max[i] != null
          ? `ERLE max ${s.erle_max[i]}, mean ${s.erle_mean[i]} over ${fr(s.erle_frames[i])}` : 'no ERLE'));
    }
    case 'rms':
      return lane('RMS dB', '−90…−10', 'var(--text2)', [
        { ys: scaled('rms_max_db', v => (v + 90) / 80), faint: true },
        { ys: scaled('rms_mean_db', v => (v + 90) / 80) },
      ], i => s.rms_max_db[i] != null
        && `RMS max ${s.rms_max_db[i]} dB${s.rms_mean_db[i] != null ? `, mean ${s.rms_mean_db[i]} dB` : ''}`);
    case 'playback':
      return lane('Playback', '0–100%', 'var(--muted)',
        [{ ys: s.playback.map((v, i) => (v == null ? null : v / s.frames[i])), fill: true }],
        i => valued(i) && [s.playback[i] ? `playback ${fr(s.playback[i])}` : 'no playback', `volume ${s.volume[i]}`]);
    case 'frames':
      return { label: 'Frames', frames: true,
        read: i => [valued(i) && `frames ${s.frames[i]}/${AFE_PERIOD_FRAMES}`, s.lost[i] ? `lost ${s.lost[i]}` : null] };
    case 'flags': {
      const flagged = Object.entries(s.flags);
      if (!flagged.length) return null;
      return { label: 'Flags', flags: true, hit: [...new Set(flagged.flatMap(([, idx]) => idx))],
        read: i => flagged.filter(([, idx]) => idx.includes(i)).map(([name]) => name.replace(/_/g, ' ')) };
    }
    default:
      return null;
  }
}

// Axis steps in seconds: the first whose span covers what is shown.
const AFE_TICK_STEPS = [[3, 0.5], [8, 1], [20, 2], [50, 5], [100, 10], [240, 30], [600, 60], [1200, 120], [Infinity, 300]];

// A chart that shows each period in one AfeSeriesJson, for a turn (AfeChart)
// and a sample clip or ambient recording (RecordingAfeChart): the `lanes` rows
// (AFE_LANES) under `bars` ({ label, span, color }, each with its duration)
// and across `guides` ({ at, border }), on the series' timeline, the axis in
// seconds from `anchor` (`anchorLabel`). Evidence only. Hovering reads one
// period out. While the recording covering `playSpan` plays, a playhead
// follows `elapsed()` (its seconds, null when silent), and the readout
// follows it while nothing is hovered. `children` sit under the chart, and
// `legend` is the readout's line while nothing is read.
function AfeLanes({ series: s, lanes, bars = [], guides = [], anchor, anchorLabel, playSpan, elapsed, label, legend, children }) {
  const [hover, setHover] = useState(null);   // period index under the pointer
  const [playAt, setPlayAt] = useState(null); // period index under the playhead
  const cursorRef = useRef(null);
  const elapsedRef = useRef(elapsed);
  elapsedRef.current = elapsed;
  // Paths once per series, not per hover: an ambient recording runs to 22,500 periods.
  const laneRows = useMemo(() => lanes.map(key => afeLaneRow(s, key)).filter(Boolean), [s, lanes]);
  useEffect(() => { setHover(null); }, [s]);

  // The playhead moves every frame by writing its own style, not through
  // state: the chart re-renders only when it crosses into another period.
  // WAV sample k is sample span[0] + k (AfeSeriesJson).
  useEffect(() => {
    setPlayAt(null);
    if (!playSpan) return undefined;
    const n = s.frames.length;
    let raf = 0, last = null;
    const tick = () => {
      const t = elapsedRef.current();
      if (t != null) {
        const x = Math.max(0, Math.min(n, (Math.min(playSpan[1], playSpan[0] + t * CAPTURE_RATE) - s.start) / s.period));
        if (cursorRef.current) cursorRef.current.style.left = `${x / n * 100}%`;
        const i = Math.max(0, Math.min(n - 1, Math.floor(x)));
        if (i !== last) { last = i; setPlayAt(i); }
      }
      raf = requestAnimationFrame(tick);
    };
    tick();
    return () => cancelAnimationFrame(raf);
  }, [s, playSpan]);

  const n = s.frames.length;
  const at = sample => (sample - s.start) / s.period;               // period units
  const secs = sample => (sample - anchor) / CAPTURE_RATE;
  const rows = [...bars, ...laneRows];
  const height = r => (r.traces ? 22 : r.span ? 10 : 8);
  const end = s.start + n * s.period;
  const shownGuides = guides.filter(g => g.at != null && g.at >= s.start && g.at <= end);
  const clip = x => Math.max(0, Math.min(n, x));
  const pct = sample => `${clip(at(sample)) / n * 100}%`;
  const fmtDur = span => `${((span[1] - span[0]) / CAPTURE_RATE).toFixed(2)} s`;

  const plot = (r, body) => (
    <svg width="100%" height={height(r)} viewBox={`0 0 ${n} 1`} preserveAspectRatio="none" style={{ display: 'block' }}>
      <rect x={0} y={0} width={n} height={1} fill="var(--hairline)"/>
      {body}
    </svg>
  );
  const cell = r => {
    if (r.span) {
      // The bar, and its duration after its end (before its start near the right edge).
      const a = clip(at(r.span[0])), b = clip(at(r.span[1]));
      const before = b / n > 0.85;
      return (
        <div style={{ position: 'relative' }}>
          {plot(r, <rect x={a} y={0.1} width={Math.max(0, b - a)} height={0.8} fill={r.color}/>)}
          <span style={{ position: 'absolute', top: 0, left: `${(before ? a : b) / n * 100}%`, fontSize: 8,
            lineHeight: `${height(r)}px`, color: 'var(--muted)', whiteSpace: 'nowrap', pointerEvents: 'none',
            padding: before ? '0 3px 0 0' : '0 0 0 3px', transform: before ? 'translateX(-100%)' : 'none' }}>
            {fmtDur(r.span)}
          </span>
        </div>
      );
    }
    if (r.frames) {
      return plot(r, s.frames.map((f, i) => f === AFE_PERIOD_FRAMES ? null : (
        <rect key={i} x={i} y={0} width={1} height={1} fill={f ? 'var(--warn)' : 'var(--error)'}/>
      )));
    }
    if (r.flags) {
      return plot(r, r.hit.map(i => <rect key={i} x={i} y={0} width={1} height={1} fill="var(--error)"/>));
    }
    return plot(r, r.traces.map((tr, k) => (
      <React.Fragment key={k}>
        {tr.fill && <path d={tr.paths.area} fill={r.color} fillOpacity={0.22}/>}
        <path d={tr.paths.line} fill="none" stroke={r.color} strokeWidth={1.25} strokeOpacity={tr.faint ? 0.45 : 1}
          vectorEffect="non-scaling-stroke"/>
      </React.Fragment>
    )));
  };

  const t0 = secs(s.start), t1 = secs(end);
  const step = AFE_TICK_STEPS.find(([most]) => t1 - t0 <= most)[1];
  const ticks = [];
  for (let k = Math.ceil(t0 / step); k * step <= t1; k++) ticks.push(k * step);
  const sign = t => (t < 0 ? '−' : '+');
  const fmtT = (t, digits) => (Math.abs(t) < 1e-9 ? '0' : `${sign(t)}${Math.abs(t).toFixed(digits)}`);
  const fmtTick = t => (step >= 60 && Math.abs(t) >= 1e-9 ? `${sign(t)}${Math.abs(t) / 60}m` : fmtT(t, step < 1 ? 1 : 0));

  const pick = e => {
    const box = e.currentTarget.getBoundingClientRect();
    setHover(Math.max(0, Math.min(n - 1, Math.floor((e.clientX - box.left) / box.width * n))));
  };
  const readout = i => {
    const a = s.start + i * s.period;
    const within = bars.filter(r => r.span[0] < a + s.period && r.span[1] > a).map(r => r.label);
    const head = [`${fmtT(secs(a), 2)} s`, within.length > 0 && within.join(', ')].filter(Boolean).join(' · ');
    if (s.frames[i] == null) return [head, 'no record', ...laneRows.filter(r => r.own).map(r => r.read(i))].filter(Boolean).join(' · ');
    return [head, !s.frames[i] && 'no frames', ...laneRows.flatMap(r => [r.read(i)].flat())].filter(Boolean).join(' · ');
  };
  const shownAt = hover ?? playAt;

  const labelStyle = { color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.06em', fontSize: 8, lineHeight: 1.1, overflow: 'hidden', whiteSpace: 'nowrap' };
  return (
    <div style={{ marginTop: 6 }}>
      <div role="img" aria-label={label}
        style={{ display: 'grid', gridTemplateColumns: '64px 1fr', columnGap: 6, rowGap: 3,
          gridTemplateRows: [...rows.map(r => `${height(r)}px`), '12px'].join(' ') }}>
        {rows.map((r, k) => (
          <React.Fragment key={r.label}>
            <div style={{ ...labelStyle, gridColumn: 1, gridRow: k + 1, alignSelf: 'center' }}>
              {r.label}{r.scale && <div style={{ textTransform: 'none', letterSpacing: 0 }}>{r.scale}</div>}
            </div>
            <div style={{ gridColumn: 2, gridRow: k + 1 }}>{cell(r)}</div>
          </React.Fragment>
        ))}
        <div style={{ gridColumn: 2, gridRow: `1 / ${rows.length + 1}`, position: 'relative', cursor: 'crosshair' }}
          onPointerMove={pick} onPointerDown={pick} onPointerLeave={() => setHover(null)}>
          {shownGuides.map((g, k) => (
            <div key={k} style={{ position: 'absolute', top: 0, bottom: 0, left: pct(g.at), borderLeft: g.border, pointerEvents: 'none' }}/>
          ))}
          {hover != null && (
            <div style={{ position: 'absolute', top: 0, bottom: 0, left: `${(hover + 0.5) / n * 100}%`, borderLeft: '1px solid var(--text)', opacity: 0.5, pointerEvents: 'none' }}/>
          )}
          {playSpan && <>
            <div style={{ position: 'absolute', top: 0, bottom: 0, left: pct(playSpan[0]),
              width: `${(clip(at(playSpan[1])) - clip(at(playSpan[0]))) / n * 100}%`,
              background: 'var(--hairline)', pointerEvents: 'none' }}/>
            <div ref={cursorRef} style={{ position: 'absolute', top: -2, bottom: -2, left: pct(playSpan[0]),
              marginLeft: -1, borderLeft: '2px solid var(--text)', pointerEvents: 'none' }}/>
          </>}
        </div>
        <div style={{ ...labelStyle, gridColumn: 1, gridRow: rows.length + 1, textTransform: 'none', letterSpacing: 0 }}>
          s from {anchorLabel}
        </div>
        <div style={{ gridColumn: 2, gridRow: rows.length + 1, position: 'relative', fontSize: 8, color: 'var(--muted)' }}>
          {ticks.map(t => {
            const p = (t - t0) / (t1 - t0) * 100;
            return (
              <span key={t} style={{ position: 'absolute', left: `${p}%`, lineHeight: 1.4,
                transform: `translateX(${p < 4 ? 0 : p > 96 ? -100 : -50}%)` }}>
                {fmtTick(t)}
              </span>
            );
          })}
        </div>
      </div>
      {children}
      {/* Two lines reserved: a readout wraps, and growing on hover would shift what follows. */}
      <div style={{ color: shownAt != null ? 'var(--text2)' : 'var(--muted)', minHeight: 34, marginTop: 4 }}>
        {shownAt != null ? readout(shownAt) : legend}
      </div>
    </div>
  );
}

// One turn's native AFE series (em_afe.AfeSeriesJson), fetched when the turn
// is opened: each 80 ms period from the second before the wake through the
// utterance and on through the spoken answer (a committed turn keeps a
// response audio subscription until a second after it ends, at most 30 s past the commit).
// Bars above the lanes place the wake, the utterance and the controller's
// processing. The turn recording (or a row's wake clip or utterance) draws a
// playhead over the span it covers.
function AfeChart({ deviceId, turnId, onset, playback }) {
  const load = useAfeSeries(`/api/devices/${deviceId}/turns/${turnId}/afe`);
  const muted = { color: 'var(--muted)' };
  if (!load) return <div style={muted}>loading the per-period chart…</div>;
  const s = load.series;
  if (!s) {
    return load.error
      ? <div style={{ color: 'var(--warn)' }}>per-period chart unavailable: {load.error}</div>
      : <div style={muted}>no per-period chart for this turn: no AFE record arrived, the turn is older than the newest 1000, or it was recorded before the chart existed</div>;
  }

  const playingKind = playback.playing;
  const stages = s.stages || {};      // absent: written before the chart placed them
  const bars = [
    s.support && { label: 'Wake', span: s.support, color: 'var(--lcd-amber)' },
    s.utterance && { label: 'Utterance', span: s.utterance, color: 'var(--accent-hi)' },
    ...AFE_STAGE_ROWS.filter(r => stages[r.key]).map(r => ({ label: r.label, span: stages[r.key], color: r.color })),
  ].filter(Boolean);
  const onsetShown = onset != null && onset >= s.start && onset <= s.start + s.frames.length * s.period;
  const guides = [
    s.support && { at: s.support[0], border: '1px solid var(--muted)' },
    s.support && { at: s.support[1], border: '1px dashed var(--muted)' },
    { at: onset, border: '1px dotted var(--lcd-amber)' },
  ].filter(Boolean);

  // The turn recording: the whole turn, canonical mic, from where the chart
  // starts through the spoken answer (a turn that never committed, through
  // its close). Not kept stays a disabled control with the reason.
  const recording = s.recording;
  const recordingReason = recording === undefined ? 'recorded before the chart kept the whole turn'
    : recording === null ? 'not recorded — Save utterances (Config → Speech) records the whole turn'
    : playback.kept(TURN_CLIP.RECORDING) === false ? 'no longer kept' : null;
  const playingRecording = playingKind === TURN_CLIP.RECORDING;

  return (
    <AfeLanes series={s} lanes={AFE_LANES.TURN} bars={bars} guides={guides}
      anchor={s.support ? s.support[0] : s.utterance ? s.utterance[0] : s.start} anchorLabel={s.support ? 'wake' : 'start'}
      playSpan={playingKind ? s[AFE_CLIP_SPANS[playingKind]] : null} elapsed={playback.elapsed}
      label="Native AFE values for each 80 ms period of this turn and the controller's Silero VAD, with its processing stages"
      legend={<>each 80 ms period · Silero VAD: the controller's speech probability for each 32 ms cell, as its decisions read it · gaps: no data · Frames: short (amber) or empty (red) periods · processing bars use the controller's clock and can run ~0.3 s early (Playback is the Echo's own){onsetShown && ' · dotted: playback began'} · hover to read one</>}>
      <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: '4px 12px', marginTop: 6 }}>
        <Pill small disabled={recordingReason != null} onClick={() => playback.toggle(TURN_CLIP.RECORDING)}>
          {playingRecording ? '▮ Stop recording' : '▶ Play recording'}
        </Pill>
        {recordingReason && <span style={muted}>{recordingReason}</span>}
      </div>
    </AfeLanes>
  );
}

// A sample clip's or ambient recording's native AFE series (em_afe.AfeTrack,
// GET …/samples|ambient/{name}/afe): VAD, DTD, ERLE and RMS over the whole
// WAV, on its own timeline; playing it from its row draws the playhead.
function RecordingAfeChart({ url, playing, elapsed }) {
  const load = useAfeSeries(url);
  const muted = { color: 'var(--muted)' };
  if (!load) return <div style={muted}>loading the chart…</div>;
  const s = load.series;
  if (!s) {
    return load.error
      ? <div style={{ color: 'var(--warn)' }}>chart unavailable: {load.error}</div>
      : <div style={muted}>no chart for this recording</div>;
  }
  return (
    <AfeLanes series={s} lanes={AFE_LANES.RECORDING} anchor={0} anchorLabel="start"
      playSpan={playing ? s.recording : null} elapsed={elapsed}
      label="Native AFE values for each 80 ms period of this recording"
      legend="each 80 ms period of the Echo's native AFE · gaps: no data · ▶ Play draws a playhead · hover to read one"/>
  );
}

// One stored recording of the Samples tab (a sample clip or an ambient
// recording): its controls in a row, `info` after its time, and with Chart
// its AFE chart under them, the row's playback driving the playhead. A
// recording without a series keeps the control, disabled (AfeRecordingNote
// says why).
function RecordingRow({ entry, info, chartUrl, chartOpen, onChart, playing, elapsed, children }) {
  return (
    <div style={{ borderTop: '1px solid var(--hairline)', padding: '6px 0' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--text2)', minWidth: 150 }}>
          {new Date(entry.ts * 1000).toLocaleString()}
        </span>
        {info}
        <span style={{ flex: 1 }}/>
        <Pill small disabled={!entry.afe} onClick={onChart}>{chartOpen ? 'Hide chart' : 'Chart'}</Pill>
        {children}
      </div>
      {chartOpen && entry.afe && (
        <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--text2)', lineHeight: 1.7 }}>
          <RecordingAfeChart url={chartUrl} playing={playing} elapsed={elapsed}/>
        </div>
      )}
    </div>
  );
}

// What a recording's Chart shows, and why one has none.
function AfeRecordingNote({ device }) {
  const gap = capabilityGap(liveCapabilities(device), CAPABILITY.AFE_METADATA);
  return (
    <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', marginBottom: 8 }}>
      Chart: the Echo&apos;s native VAD, DTD, ERLE and RMS for every 80 ms of the recording.
      {gap ? ` Not recorded: ${gap}.` : ' Greyed out where none was recorded (recordings from before charts, or from firmware without the native AFE metadata).'}
    </div>
  );
}

// A turn's decision trace (§11.3), fetched for the detail stages that read it.
// `load.turnId` trails `turnId` while a fetch is in flight; a 404 is no trace, not an error.
function useTurnTrace(deviceId, turnId) {
  const [load, setLoad] = useState({ turnId: null });
  useEffect(() => {
    let live = true;
    setLoad({ turnId: null });
    API.get(`/api/devices/${deviceId}/turns/${turnId}/trace`)
      .then(trace => { if (live) setLoad({ turnId, trace }); })
      .catch(e => { if (live) setLoad({ turnId, error: e.status === 404 ? null : (e.error || e.message || 'request failed') }); });
    return () => { live = false; };
  }, [deviceId, turnId]);
  return load;
}

// em_speech_worker.VerificationResult — a trace's verification.result.
const VERIFICATION_RESULT = Object.freeze({ PASS: 'pass', FAIL: 'fail', TIMEOUT: 'timeout' });

// Refusals only a wake heard while the Echo was playing can reach (§6.1): each went through the verifier.
const VERIFIED_REFUSAL = new Set([TERMINAL_REASON.VERIFIER_TIMEOUT, TERMINAL_REASON.UNVERIFIED_WAKE,
  TERMINAL_REASON.SELF_OUTPUT, TERMINAL_REASON.ECHO_ONLY]);

// Wake verification (§16.6), from the decision trace of a wake heard while the
// Echo was playing: what the verifier's fresh Kroko decode heard, how far it
// was from the wake word, and where the time went against the deadline —
// the mic covering the verifier's input (480 ms past the wake), the decode's
// wait for a thread and its own run, and the verdict's arrival. A verdict
// that came after the candidate was refused is still shown: a timeout on a
// wake word the verifier did hear is a deadline problem, not a hearing one.
function WakeVerification({ deviceId, turnId }) {
  const load = useTurnTrace(deviceId, turnId);
  const muted = { color: 'var(--muted)' };
  if (load.turnId !== turnId) return <span style={muted}>loading…</span>;
  if (load.error) return <span style={{ color: 'var(--warn)' }}>unavailable: {load.error}</span>;
  const v = load.trace ? load.trace.verification : null;
  if (!v) return <span style={muted}>recorded before the verification was kept</span>;
  const ms = x => (x == null ? '—' : `${x} ms`);
  const verdict = {
    [VERIFICATION_RESULT.PASS]: <span style={{ color: 'var(--ok)' }}>passed</span>,
    [VERIFICATION_RESULT.FAIL]: <span style={{ color: 'var(--warn)' }}>failed: not the wake word</span>,
    [VERIFICATION_RESULT.TIMEOUT]: <span style={{ color: 'var(--warn)' }}>timed out</span>,
  }[v.result] || <span style={muted}>no verdict</span>;
  return (
    <>
      <div>
        {v.text != null ? <span style={{ color: 'var(--text)' }}>“{v.text}”</span> : <span style={muted}>nothing decoded</span>}
        {v.distance != null && <span style={muted}> · distance {v.distance.toFixed(2)}</span>}
        {' · '}{verdict}
        {v.result === VERIFICATION_RESULT.TIMEOUT && v.text_passes != null && (
          <span style={muted}> · its text would have {v.text_passes ? 'passed' : 'failed'}</span>
        )}
      </div>
      <div style={muted}>
        {v.input_ready_ms != null
          ? <>input complete {ms(v.input_ready_ms)} after the wake</>
          : <>input never complete: the mic reached {ms(v.mic_past_open_ms)} past the wake of the 480 ms needed</>}
        {v.decode_ms != null && <> · decode {ms(v.decode_ms)} after {ms(v.queued_ms)} waiting for a thread</>}
        {v.answered_ms != null && <> · verdict at {ms(v.answered_ms)}</>}
        {' · '}decided at {ms(v.decided_ms)}, deadline {ms(v.deadline_ms)}
        {v.late && <span style={{ color: 'var(--warn)' }}> · verdict came {v.answered_ms - v.decided_ms} ms after the decision</span>}
        {v.error && <span style={{ color: 'var(--warn)' }}> · {v.error}</span>}
      </div>
    </>
  );
}

// Pause transcription (§16.6), from the turn's decision trace: at each pause
// Kroko re-decodes the utterance so far and, with a Wyoming server set in
// Pause transcription, the server transcribes the same audio, both started at
// the same moment. Each transcriber's words, its time from the pause, and
// when its words were judged (audio time after Kroko's decode); ★ marks the
// words the endpoint committed on.
function PauseTranscripts({ deviceId, turnId }) {
  const load = useTurnTrace(deviceId, turnId);

  const muted = { color: 'var(--muted)' };
  if (load.turnId !== turnId) return <span style={muted}>loading…</span>;
  const u = load.trace ? load.trace.utterance : null;
  if (!u) {
    return load.error
      ? <span style={{ color: 'var(--warn)' }}>unavailable: {load.error}</span>
      : <span style={muted}>no decision trace: the turn is older than the newest 1000, or was recorded before traces were kept</span>;
  }
  if (!u.pauses) return <span style={muted}>recorded before each transcriber's words and times were kept</span>;

  const commit = (u.endpoint || []).find(e => e.event === 'commit');
  const source = commit ? commit.source : null;
  // The committed words: the latest pause whose words from that transcriber are the committed text.
  let won = -1;
  if (source === TEXT_SOURCE.KROKO || source === TEXT_SOURCE.SERVER) {
    won = u.pauses.map(p => !!p[source] && p[source].judged === u.stable_prefix).lastIndexOf(true);
  }
  const plain = s => s.toLowerCase().replace(/[^a-z0-9' ]+/g, ' ').split(/\s+/).filter(Boolean).join(' ');
  const quote = s => (s ? <span style={{ color: 'var(--text)' }}>“{s}”</span> : <span style={muted}>nothing</span>);
  const status = (p, key, h) => {
    if (h.text == null) {
      if (!h.error) return <span style={muted}>no answer before the turn was decided</span>;
      if (h.error === 'superseded') return <span style={muted}>dropped: speech went on</span>;
      return <span style={{ color: 'var(--warn)' }}>{h.error === 'timeout' ? 'timed out' : h.error}</span>;
    }
    if (h.at == null) {
      return <span style={muted}>
        {h.text === '' ? 'empty: not judged'
          : key === TEXT_SOURCE.KROKO ? "not judged: the server's words were already in"
          : 'not judged: the turn was decided first'}
      </span>;
    }
    const later = Math.round((h.at - p.through) * 1000 / CAPTURE_RATE);
    return <span style={muted} title="audio time from Kroko's decode at this pause to the block its words were first judged at">
      judged {later > 0 ? `${later} ms later` : 'at the pause'}
    </span>;
  };
  const line = (p, i, key, label) => {
    const h = p[key];
    if (!h) return null;
    const winner = i === won && key === source;
    return (
      <React.Fragment key={key}>
        <span style={{ color: 'var(--ok)' }} title={winner ? 'committed on these words' : undefined}>{winner ? '★' : ''}</span>
        <span>{label}</span>
        <span style={{ textAlign: 'right' }}>{h.ms != null ? `${h.ms} ms` : '—'}</span>
        <span>
          {h.text != null && <>{quote(h.text)}{h.judged != null && plain(h.judged) !== plain(h.text) && <> → {quote(h.judged)}</>} · </>}
          {status(p, key, h)}
        </span>
      </React.Fragment>
    );
  };
  return (
    <>
      <div>
        {commit ? <>committed on {TEXT_SOURCES[source] || source}</> : 'not committed'}
        {won >= 0 && (() => {
          const other = source === TEXT_SOURCE.SERVER ? TEXT_SOURCE.KROKO : TEXT_SOURCE.SERVER;
          const h = u.pauses[won][other];
          return h && h.judged === u.stable_prefix
            ? <span style={muted}> · {other === TEXT_SOURCE.KROKO ? 'Kroko' : 'the server'} heard the same</span> : null;
        })()}
        {u.pauses.length > 0 && u.pauses.every(p => !p.server) && <span style={muted}> · Kroko alone</span>}
      </div>
      {u.pauses.length === 0 && <div style={muted}>no pause was transcribed before the turn was decided</div>}
      {u.pauses.map((p, i) => (
        <div key={p.through} style={{ marginTop: 2 }}>
          <div style={muted}>pause {i + 1} · decoded {((p.through - u.start) / CAPTURE_RATE).toFixed(2)} s into the utterance</div>
          <div style={{ display: 'grid', gridTemplateColumns: '10px 56px 52px 1fr', columnGap: 8 }}>
            {line(p, i, TEXT_SOURCE.KROKO, 'Kroko')}
            {line(p, i, TEXT_SOURCE.SERVER, 'Wyoming')}
          </div>
        </div>
      ))}
    </>
  );
}

// One turn, stage by stage: what woke it, what the controller's streaming
// recognizer heard and why it stopped listening, what each pause transcriber
// heard and whose words it ended on, what Home Assistant
// transcribed and what was sent on, who handled it, what was said back, and
// what became of any follow-up question. A stage that never ran (a refused
// wake, no speech, a failed STT) is omitted rather than shown empty. Its
// recordings and fetches for each turn belong to the turn's own device (`device_id`);
// `deviceLabel`, given across devices, names that device in the header.
function TurnDetail({ turn: t, turns, deviceLabel, onSelect, playback }) {
  // Pre-cutover rows stored negative sentinels for unmeasured stages.
  const measured = v => typeof v === 'number' && v >= 0;
  const fmtS = ms => (measured(ms) ? (ms / 1000).toFixed(2) + ' s' : '—');
  const quote = s => (s ? <span style={{ color: 'var(--text)' }}>“{s}”</span> : <span style={{ color: 'var(--muted)' }}>—</span>);
  const refused = REFUSED_WAKE.has(t.terminal_reason);
  const legacy = isLegacyTurn(t);
  const end = turnEnd(t);
  // Follow-ups link the question and its reply by the controller's turn id.
  const question = t.reply_to ? turns.find(x => x.turn_uuid === t.reply_to) : null;
  const answers = t.turn_uuid ? turns.filter(x => x.reply_to === t.turn_uuid) : [];
  const jump = (x, label) => (
    <button type="button" key={x.turn_id} onClick={() => onSelect(x.turn_id)}
      style={{ background: 'none', border: 'none', padding: 0, cursor: 'pointer', font: 'inherit', color: 'var(--accent)', textDecoration: 'underline' }}>
      {label}
    </button>
  );
  const stage = (label, body) => <DetailStage label={label}>{body}</DetailStage>;
  // Turns that ran an HA intent: its conversation run, or EchoMuse's HassTimerStatus/HassCancelTimer.
  const handledByHa = t.outcome === TURN_OUTCOME.HA || t.outcome === TURN_OUTCOME.TIMER;
  const agent = t.intent_local == null ? null : t.intent_local ? "HA's built-in agent" : 'the conversation agent';
  const fix = parseTextFix(t.stt_fix);
  return (
    <div style={DETAIL_BOX}>
      <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', paddingBottom: 4 }}>
        {deviceLabel && <span style={{ color: 'var(--text)' }}>{deviceLabel}</span>}
        <span style={{ color: 'var(--text)' }}>{fmtTurnWhenFull(t.ts)}</span>
        <span>{TRIGGERS[t.trigger] || t.trigger}</span>
        <span style={{ color: end.color }}>{end.label.replace(/_/g, ' ')}</span>
        {legacy && <span style={{ color: 'var(--muted)' }}>recorded before the post-AFE cutover</span>}
        <span style={{ marginLeft: 'auto' }}>total {fmtS(t.total_ms)}</span>
      </div>

      {t.trigger === TURN_TRIGGER.WAKE && stage('Wake', <>
        {t.wake_model || '—'} ({shortSha(t.wake_model_sha256)}) · score {t.wake_score != null ? t.wake_score.toFixed(3) : '—'}
        {' '}/ threshold {t.wake_threshold != null ? t.wake_threshold.toFixed(2) : '—'}
        {' · '}{refused ? <span style={{ color: 'var(--warn)' }}>refused: {t.terminal_reason.replace(/_/g, ' ')}</span>
          : legacy ? null
          : t.wake_attribution === WAKE_ATTRIBUTION.VERIFIED ? 'heard while the Echo was playing; verified' : 'heard while idle'}
      </>)}

      {t.trigger === TURN_TRIGGER.WAKE && !legacy
        && (t.wake_attribution === WAKE_ATTRIBUTION.VERIFIED || VERIFIED_REFUSAL.has(t.terminal_reason))
        && stage('Verification', <WakeVerification deviceId={t.device_id} turnId={t.turn_id}/>)}

      {/* The wake clip's windows, as the controller scored them once it was saved. */}
      {t.trigger === TURN_TRIGGER.WAKE && (t.wake_windows || playback?.kept(TURN_CLIP.WAKE)) && stage('Windows',
        <ClipWindowsView windows={t.wake_windows} threshold={t.wake_threshold}
          playing={playback?.playing === TURN_CLIP.WAKE}
          playingBest={playback?.playing === TURN_CLIP.WAKE && playback.segment != null}
          elapsed={playback?.elapsed} onPlayBest={playback?.kept(TURN_CLIP.WAKE) ? s => playback.toggle(TURN_CLIP.WAKE, s) : null}/>)}

      {!refused && !legacy && stage('Heard (EchoMuse)', <>
        {quote(t.asr_text)}
        <div style={{ color: 'var(--muted)' }}>
          {t.commit_route
            ? <>ended: {COMMIT_ROUTES[t.commit_route] || t.commit_route}
                {t.endpoint_class ? <> · text judged {t.endpoint_class.replace(/_/g, ' ')}</> : null}
                {' · '}waited {fmtS(t.endpoint_ms)} after speech · utterance {fmtS(t.audio_ms)}</>
            : <>not committed — {end.label.replace(/_/g, ' ')}</>}
        </div>
      </>)}

      {!refused && !legacy && stage('Pause ASR', <PauseTranscripts deviceId={t.device_id} turnId={t.turn_id}/>)}

      {!legacy && stage('Native AFE', <NativeAfeEvidence evidence={t.afe_evidence} deviceId={t.device_id}
        turnId={t.turn_id} playback={playback}/>)}

      {(t.stt_raw != null || (legacy && (t.stt_text || measured(t.stt_ms)))) && stage('Transcribed (HA)', <>
        {quote(t.stt_raw ?? t.stt_text)}
        <div style={{ color: 'var(--muted)' }}>
          speech-to-text {fmtS(t.stt_ms)}
          {t.stt_raw != null && t.stt_text != null && t.stt_text !== t.stt_raw && <> · {fix?.applied ? 'wake word removed' : 'sent on as'} {quote(t.stt_text)}{fix?.applied ? '' : ' (wake word removed)'}</>}
          {fix?.applied && <> · <span style={{ color: 'var(--ok)' }}>corrected before sending</span></>}
        </div>
      </>)}

      {fix && stage('Correction', <TextFixEvidence key={t.turn_id} fix={fix} quote={quote}/>)}

      {t.outcome && !legacy && stage('Handled by', <>
        {TURN_HANDLERS[t.outcome] || t.outcome.replace(/_/g, ' ')}
        {t.outcome === TURN_OUTCOME.LOCAL_COMMAND && t.stt_text ? <> · {quote(t.stt_text)}</> : null}
        {handledByHa && (agent || t.response_type || measured(t.intent_ms)) && (
          <div style={{ color: 'var(--muted)' }}>
            {[agent && `answered by ${agent}`, t.response_type && t.response_type.replace(/_/g, ' '),
              measured(t.intent_ms) && `intent ${fmtS(t.intent_ms)}`].filter(Boolean).join(' · ')}
          </div>
        )}
      </>)}

      {(t.response_text || measured(t.tts_url_ms) || measured(t.playback_ms) || measured(t.first_audio_ms) || t.playback_reason) && stage('Spoke', <>
        {quote(t.response_text)}
        {(measured(t.tts_url_ms) || measured(t.playback_ms) || measured(t.first_audio_ms) || t.playback_reason) && (
          <div style={{ color: 'var(--muted)' }}>
            {[measured(t.tts_url_ms) && `audio ready ${fmtS(t.tts_url_ms)} after dispatch`,
              (measured(t.response_latency_ms) || measured(t.first_audio_ms)) && `first audio ${[
                measured(t.response_latency_ms) && `${fmtS(t.response_latency_ms)} after speech ended`,
                measured(t.first_audio_ms) && `${fmtS(t.first_audio_ms)} after endpoint`].filter(Boolean).join(', ')}`,
              measured(t.playback_ms) && `audible ${fmtS(t.playback_ms)}`].filter(Boolean).join(' · ')}
            {t.playback_reason && <>
              {(measured(t.tts_url_ms) || measured(t.first_audio_ms) || measured(t.playback_ms)) && ' · '}
              <span style={{ color: t.playback_reason === PLAYBACK_END.DRAINED ? undefined : 'var(--warn)' }}>
                {PLAYBACK_ENDS[t.playback_reason] || t.playback_reason.replace(/_/g, ' ')}
              </span>
            </>}
          </div>
        )}
      </>)}

      {(t.continuation || t.reply_to) && stage('Follow-up', <>
        {t.reply_to && (
          <div>
            answered the question {question
              ? <>{quote(question.response_text)} from {jump(question, fmtTurnWhen(question.ts))}</>
              : 'of a turn no longer in this list'}
          </div>
        )}
        {t.continuation && (
          <div>
            <span style={{ color: 'var(--muted)' }}>
              {t.outcome === TURN_OUTCOME.HA ? 'Home Assistant asked a follow-up' : 'EchoMuse asked a follow-up'}:{' '}
            </span>
            <span style={{ color: followup(t.continuation)[1] }}>{followup(t.continuation)[0]}</span>
            {answers.map(a => <React.Fragment key={a.turn_id}> · {jump(a, `reply at ${fmtTurnWhen(a.ts)}`)}</React.Fragment>)}
          </div>
        )}
        {t.conversation_id && (
          <div style={{ color: 'var(--muted)' }} title={t.conversation_id}>
            HA conversation …{t.conversation_id.slice(-8)}
          </div>
        )}
      </>)}

      <div style={{ borderTop: '1px solid var(--track)', paddingTop: 4, color: 'var(--muted)', fontSize: 9 }}>
        reference coverage {t.reference_coverage != null ? `${Math.round(t.reference_coverage * 100)}%` : '—'}
        {t.policy_hash ? <> · policy {t.policy_hash}</> : null}
        {t.commit_id ? <> · commit {shortSha(t.commit_id)}</> : null}
        {t.turn_uuid ? <> · <span title={`controller log: "turn ${t.turn_uuid} trace"`}>trace {shortSha(t.turn_uuid)}</span></> : null}
      </div>
    </div>
  );
}

// The detail box under an Activity list, and one labelled stage of it.
const DETAIL_BOX = Object.freeze({
  marginTop: 10, background: 'var(--hairline)', border: '1px solid var(--track)', borderRadius: 6,
  padding: '8px 12px', fontFamily: MONO, fontSize: 10, color: 'var(--text2)', lineHeight: 1.7,
});
function DetailStage({ label, children }) {
  return (
    <div style={{ display: 'grid', gridTemplateColumns: '112px 1fr', gap: 12, padding: '5px 0', borderTop: '1px solid var(--track)' }}>
      <span style={{ color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.08em', fontSize: 9, paddingTop: 1 }}>{label}</span>
      <div>{children}</div>
    </div>
  );
}

// How a near miss ended (near_misses[].end_reason), for its detail.
const NEAR_MISS_END_TEXT = Object.freeze({
  [NEAR_MISS_END.BELOW]:   'ended as the score fell back under the floor',
  [NEAR_MISS_END.GAP]:     'cut short by a gap in the audio',
  [NEAR_MISS_END.RESET]:   'cut short by a detector reset',
  [NEAR_MISS_END.MUTE]:    'cut short by muting',
  [NEAR_MISS_END.OVERRUN]: 'cut short by a detector overrun',
});

// Each verdict as Activity shows it: `answer` on its button in the detail,
// `label` on its filter, `tag` in the row's ending column, `key` the shortcut
// that sets it, `title` what it means, and `kept` what marking it did.
const NEAR_MISS_VERDICTS = Object.freeze({
  [NEAR_MISS_VERDICT.MISSED_WAKE]: {
    answer: 'Yes — missed wake', label: 'Missed wake', tag: 'missed wake', key: 'y', color: 'var(--warn)',
    title: 'The wake word was said and the Echo stayed quiet: a positive for wake-word training',
    kept: 'Marked as a missed wake',
  },
  [NEAR_MISS_VERDICT.CORRECTLY_IGNORED]: {
    answer: 'No — correctly ignored', label: 'Correctly ignored', tag: 'ignored', key: 'n', color: 'var(--ok)',
    title: 'It was not the wake word, so staying quiet was right: a negative for wake-word training',
    kept: 'Marked as correctly ignored',
  },
});
// The shortcut that clears a verdict and the one that plays or stops the recording.
const NEAR_MISS_KEY = Object.freeze({ CLEAR: 'u', PLAY: 'p' });

// The wake model's windows over a saved clip, as the controller scored them
// once it was saved (em_clip_windows; `windows` is ClipWindowsJson, null until
// scored): a bar per window at its centre, as tall as its score, the best
// window's stretch shaded, the wake `threshold` dashed, and the clip's playhead
// while it plays (`playing`, `elapsed()` seconds into the clip).
// `onPlayBest({from, to})` plays the best window alone, in seconds (`playingBest`
// while it does); null: no clip to play.
function ClipWindowsView({ windows: w, threshold, playing, playingBest, elapsed, onPlayBest }) {
  const [registry] = useWakeRegistry();
  const cursorRef = useRef(null);
  const elapsedRef = useRef(elapsed);
  elapsedRef.current = elapsed;
  const muted = { color: 'var(--muted)' };
  const duration = w ? w.samples / CAPTURE_RATE : 0;

  // The playhead moves every frame by writing its own style, as AfeLanes' does.
  useEffect(() => {
    if (!playing || !w) return undefined;
    let raf = 0;
    const tick = () => {
      const t = elapsedRef.current();
      if (t != null && cursorRef.current) cursorRef.current.style.left = `${Math.min(100, t / duration * 100)}%`;
      raf = requestAnimationFrame(tick);
    };
    tick();
    return () => cancelAnimationFrame(raf);
  }, [playing, w, duration]);

  if (!w) return <span style={muted}>not scored yet: the controller scores each saved clip in the background</span>;
  const span = w.window / CAPTURE_RATE;
  const best = w.scores.reduce((b, s) => (!b || s[1] > b[1] ? s : b), null);
  const x = sample => sample / w.samples * 1000;
  const H = 40;
  const fmt = s => `${s.toFixed(2)} s`;
  return (
    <div>
      <div style={{ position: 'relative', height: H + 2, marginBottom: 3 }}>
        <svg viewBox={`0 0 1000 ${H}`} preserveAspectRatio="none" width="100%" height={H}
          style={{ display: 'block', background: 'var(--track)', borderRadius: 3 }}
          aria-label={best ? `Best window ${fmt(best[0] / CAPTURE_RATE)} to ${fmt(best[0] / CAPTURE_RATE + span)}, score ${best[1].toFixed(2)}` : 'No window scored'}>
          {best && <rect x={x(best[0])} y={0} width={x(w.window)} height={H} fill="var(--accent)" opacity={0.18}/>}
          {threshold != null && <line x1={0} x2={1000} y1={H - threshold * H} y2={H - threshold * H}
            stroke="var(--warn)" strokeDasharray="4 3" vectorEffect="non-scaling-stroke"/>}
          {w.scores.map(([start, p]) => (
            <rect key={start} x={x(start + w.window / 2) - 4} width={8} y={H - p * H} height={Math.max(1, p * H)}
              fill={best && start === best[0] ? 'var(--accent)' : 'var(--muted)'}>
              <title>{`${fmt(start / CAPTURE_RATE)}–${fmt(start / CAPTURE_RATE + span)}: ${p.toFixed(3)}`}</title>
            </rect>
          ))}
        </svg>
        {playing && <div ref={cursorRef} style={{ position: 'absolute', top: 0, bottom: 2, width: 1, left: 0,
          background: 'var(--text)', pointerEvents: 'none' }}/>}
      </div>
      <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: '4px 8px' }}>
        {best ? <>
          <span>best window {fmt(best[0] / CAPTURE_RATE)}–{fmt(best[0] / CAPTURE_RATE + span)} · {best[1].toFixed(3)}</span>
          {onPlayBest && <Pill small onClick={() => onPlayBest({ from: best[0] / CAPTURE_RATE, to: best[0] / CAPTURE_RATE + span })}>
            {playingBest ? '▮ Stop' : '▶ Best window'}
          </Pill>}
        </> : <span style={muted}>no window loud enough to score</span>}
      </div>
      <div style={muted}>
        {w.scores.length} windows of {fmt(span)}, every 160 ms across {fmt(duration)}, scored again on the controller
        {' '}with {wakeModelLabel(registry.models, w.graph_sha256)}; unsmoothed, so not the Echo's own numbers
        {threshold != null && <> · dashed: wake threshold {threshold.toFixed(2)}</>}
      </div>
    </div>
  );
}

// One near miss in detail: what nearly woke the Echo and how close it came,
// its recording (`recording` its NEAR_MISS_RECORDING, pruned derived), the
// wake model's windows over it with `onPlayBest` (`playingBest` while the best
// one sounds), its `verdict` with the buttons that set it through
// `onMark(verdict or null)` (`markGap`: why it cannot be marked, else null;
// `markError`: why the last mark failed), and the native AFE chart around it
// with the recording's playhead.
function NearMissDetail({ near: n, deviceLabel, recording, playing, playingBest, onPlay, onPlayBest, onDownload,
                          elapsed, verdict, onMark, markGap, markError }) {
  const [registry] = useWakeRegistry();
  const muted = { color: 'var(--muted)' };
  const fmtS = ms => (ms / 1000).toFixed(2) + ' s';
  return (
    <div style={DETAIL_BOX}>
      <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', paddingBottom: 4 }}>
        {deviceLabel && <span style={{ color: 'var(--text)' }}>{deviceLabel}</span>}
        <span style={{ color: 'var(--text)' }}>{fmtTurnWhenFull(n.ts)}</span>
        <span style={{ color: 'var(--lcd-amber)' }}>near miss</span>
        <span>{NEAR_MISS_PROFILE_LABEL[n.profile] || n.profile}</span>
        <span style={n.end_reason === NEAR_MISS_END.BELOW ? undefined : { color: 'var(--warn)' }}>
          {NEAR_MISS_END_TEXT[n.end_reason] || n.end_reason}
        </span>
        <span style={{ marginLeft: 'auto' }}>scored over {fmtS(n.duration_ms)}</span>
      </div>
      <DetailStage label="Score">
        peak {n.peak.toFixed(3)} · floor {n.floor.toFixed(2)}
        {n.wake_threshold != null && <> · wake threshold {n.wake_threshold.toFixed(2)}</>}
        <div style={muted}>
          {wakeModelLabel(registry.models, n.wake_model_sha256)}
          {n.wake_threshold != null && ` · the peak reached ${Math.round(n.peak / n.wake_threshold * 100)}% of the threshold`}
        </div>
      </DetailStage>
      <DetailStage label="Recording">
        {recording === NEAR_MISS_RECORDING.SAVED ? (
          <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: '4px 8px' }}>
            <Pill small onClick={onPlay}>{playing && !playingBest ? '▮ Stop' : '▶ Play'}</Pill>
            <Pill small onClick={onDownload}>⤓ Download</Pill>
            {n.audio_ms != null && <span style={muted}>{fmtS(n.audio_ms)}, 300 ms either side of the scored stretch</span>}
          </div>
        ) : <span style={muted}>{NEAR_MISS_UNRECORDED[recording] || recording}</span>}
      </DetailStage>
      {(recording === NEAR_MISS_RECORDING.SAVED || n.windows) && (
        <DetailStage label="Windows">
          <ClipWindowsView windows={n.windows} threshold={n.wake_threshold} playing={playing} playingBest={playingBest}
            elapsed={elapsed} onPlayBest={recording === NEAR_MISS_RECORDING.SAVED ? onPlayBest : null}/>
        </DetailStage>
      )}
      <DetailStage label="Verdict">
        <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: '4px 8px' }}>
          <span style={{ color: 'var(--text)' }}>Was the wake word said?</span>
          {/* Pill takes no title; each wrapper says what its verdict means. */}
          {Object.values(NEAR_MISS_VERDICT).map(v => (
            <span key={v} title={NEAR_MISS_VERDICTS[v].title}>
              <Pill small accent={verdict === v} disabled={!!markGap} onClick={() => onMark(v)}>
                {NEAR_MISS_VERDICTS[v].key.toUpperCase()} · {NEAR_MISS_VERDICTS[v].answer}
              </Pill>
            </span>
          ))}
          {verdict && !markGap && (
            <span title="Clear the verdict: the near miss goes back to the ones to review, and retention may remove its recording">
              <Pill small onClick={() => onMark(null)}>{NEAR_MISS_KEY.CLEAR.toUpperCase()} · Clear</Pill>
            </span>
          )}
        </div>
        <div style={markError ? { color: 'var(--warn)' } : muted}>
          {markError || markGap || <>
            {verdict && `${NEAR_MISS_VERDICTS[verdict].kept}: its recording is kept until the mark is cleared. `}
            {Object.values(NEAR_MISS_VERDICT).map(v => NEAR_MISS_VERDICTS[v].key.toUpperCase()).join(' or ')} marks
            it, then moves to the next near miss to review and plays it; {NEAR_MISS_KEY.PLAY.toUpperCase()} plays or
            stops this one.
          </>}
        </div>
      </DetailStage>
      <DetailStage label="Native AFE"><NearMissAfeChart near={n} playing={playing} elapsed={elapsed}/></DetailStage>
      <div style={{ borderTop: '1px solid var(--track)', paddingTop: 4, color: 'var(--muted)', fontSize: 9 }}>
        <span title={n.near_miss_uuid}>near miss {shortSha(n.near_miss_uuid)}</span> · row {n.id}
      </div>
    </div>
  );
}

// A near miss's native AFE series (GET …/near_misses/{nm}/afe): every 80 ms
// period from a second before the scored stretch through its recording, the
// stretch as a bar, and the recording's playhead while it plays.
function NearMissAfeChart({ near: n, playing, elapsed }) {
  const load = useAfeSeries(`/api/devices/${n.device_id}/near_misses/${n.id}/afe`);
  const muted = { color: 'var(--muted)' };
  if (!load) return <div style={muted}>loading the per-period chart…</div>;
  const s = load.series;
  if (!s) {
    return load.error
      ? <div style={{ color: 'var(--warn)' }}>per-period chart unavailable: {load.error}</div>
      : <div style={muted}>no per-period chart: one is taken only with the recording (Save wake clips on), on firmware reporting native AFE metadata; near misses from before charts existed, or older than the newest 1000, have none</div>;
  }
  return (
    <AfeLanes series={s} lanes={AFE_LANES.TURN}
      bars={[{ label: 'Near miss', span: s.support, color: 'var(--lcd-amber)' }]}
      guides={[{ at: s.support[0], border: '1px solid var(--muted)' }, { at: s.support[1], border: '1px dashed var(--muted)' }]}
      anchor={s.support[0]} anchorLabel="near miss" playSpan={playing ? s.recording : null} elapsed={elapsed}
      label="Native AFE values for each 80 ms period around this near miss"
      legend="each 80 ms period · gaps: no data · Near miss: the stretch the wake score was scored over · hover to read one"/>
  );
}

// The number of rows each Activity list asks for: one device's tab, or the view across devices.
const ACTIVITY_LIMIT = Object.freeze({ DEVICE: 50, FLEET: 200 });

// Which near misses an Activity list shows: none, every one, the recorded ones
// nobody has marked yet, or the ones marked with one verdict.
const NEAR_FILTER = Object.freeze({
  HIDDEN: 'hidden', ALL: 'all', TO_REVIEW: 'to_review',
  MISSED_WAKE: NEAR_MISS_VERDICT.MISSED_WAKE, CORRECTLY_IGNORED: NEAR_MISS_VERDICT.CORRECTLY_IGNORED,
});
const NEAR_FILTERS = Object.freeze([
  { id: NEAR_FILTER.HIDDEN, label: 'Hide', title: 'List the turns alone' },
  { id: NEAR_FILTER.ALL, label: 'All',
    title: 'Every near miss: a wake score at or above the near-miss floor that did not wake it' },
  { id: NEAR_FILTER.TO_REVIEW, label: 'To review', title: 'The recorded near misses nobody has marked yet' },
  ...[NEAR_FILTER.MISSED_WAKE, NEAR_FILTER.CORRECTLY_IGNORED].map(v => ({
    id: v, label: NEAR_MISS_VERDICTS[v].label, title: `Marked ${NEAR_MISS_VERDICTS[v].label.toLowerCase()}: ${NEAR_MISS_VERDICTS[v].title}`,
  })),
]);

// One list of voice turns and near misses, with every row naming its `device_id`
// and the oldest first: one device's on its Activity tab, or, with `fleet`,
// the Activity view's across devices, where each row names its device and
// `onOpenDevice(id)` opens it. `labels` maps each device id that rows may name
// to its label; `device` is the one device the rows are for (its archive of
// wake clips, and the clip player's scope), else null. `nearMissGap` is the
// reason the rows can hold no near miss (no device in view reports them),
// else null; `state` is the device's {label, color} tile, or null; `limit` is
// the number of rows requested; `listHeight` is the scrolling list's maximum height;
// `emptyNote` is what an empty list says instead of "nothing recorded yet";
// `isAdmin` lets the near misses be marked.
function TurnObservability({ turns, nearMisses, nearMissGap, labels, device = null, fleet = false, onOpenDevice,
                             recordingsOn, state = null, limit, listHeight = 230, emptyNote = null, isAdmin = false }) {
  const [hover, setHover] = useState(null);       // key of the hovered row in `rows`
  const [selected, setSelected] = useState(null); // key of the detailed row; null follows the newest turn
  const [nearFilter, setNearFilter] = useState(NEAR_FILTER.ALL); // which near misses the list shows
  // Verdicts marked here, by near miss id, until the polled list agrees: a mark
  // shows at once, and a failed one goes back to what it was.
  const [marks, setMarks] = useState({});
  const [markError, setMarkError] = useState('');
  const [focusKey, setFocusKey] = useState(null); // a row to focus and scroll to once it is rendered
  const rootRef = useRef(null);
  // Whether the last click or focus landed in this list or its detail: the
  // review keys act only then, so a list behind a device's modal stays still.
  const engaged = useRef(false);

  // Saved audio — play in place or download the WAV, through one cached
  // object URL per clip (useClipPlayer).
  //
  // A turn can carry three different recordings: the wake clip (the 1.4s that
  // crossed the threshold), the utterance (the command that followed, as sent
  // to STT) and the turn recording its AFE chart plays (the whole turn).
  // Everything here is therefore keyed `<turn_id>:<kind>` rather than by the
  // turn alone — one key means one <audio> element, so starting any clip
  // stops the other instead of leaving them talking over each other.
  const { WAKE, MIC, RECORDING } = TURN_CLIP;
  const clipKey = (t, kind) => `${t.turn_id}:${kind}`;
  const clipPath = (t, kind) => `/api/devices/${t.device_id}/turns/${t.turn_id}/${kind}`;

  const player = useClipPlayer(device);
  const playing = player.playing;             // clip key currently sounding
  const [gone, setGone]         = useState(() => new Set()); // 404 = pruned
  const [zipping, setZipping]   = useState(false);
  const [zipError, setZipError] = useState('');

  const slugOf = id => fileSlug(labels[id], id);
  const nameOf = id => labels[id] || id;

  // Retention keeps a small number of files per device, far fewer than the
  // turns in its history, so a row that names a recording that no longer exists is ordinary.
  // Mark it gone and drop its controls rather than surfacing an error. Each
  // kind of clip has its own count, rather than sharing one count per turn,
  // so a turn routinely still has its wake clip long after its utterance
  // was pruned.
  const markGone = key => setGone(g => new Set(g).add(key));

  const toggleAudio = async (t, kind, stretch = null) => {
    const key = clipKey(t, kind);
    if (!(await player.toggle(key, clipPath(t, kind), stretch))) markGone(key);
  };

  const downloadAudio = async (t, kind) => {
    const key = clipKey(t, kind);
    let url;
    try { url = await player.url(key, clipPath(t, kind)); } catch { markGone(key); return; }
    const when = new Date(t.ts * 1000).toISOString().slice(0, 19).replace(/[:T]/g, '');
    // The kind is in the filename because both clips of one turn land in the
    // same downloads folder seconds apart, and only one of them is the
    // training negative anyone went looking for.
    downloadUrl(url, `${slugOf(t.device_id)}-${when}${kind === WAKE ? '-wake' : ''}.wav`);
  };

  // A near miss's recording (GET …/near_misses/{id}/recording). Its key is
  // also its row's, and never collides with a turn clip's `<turn_id>:<kind>`,
  // so the one player stops whichever else is sounding.
  const nearKey  = n => `near:${n.id}`;
  const nearPath = n => `/api/devices/${n.device_id}/near_misses/${n.id}/recording`;
  const toggleNear = async (n, stretch = null) => {
    if (!(await player.toggle(nearKey(n), nearPath(n), stretch))) markGone(nearKey(n));
  };
  const downloadNear = async n => {
    let url;
    try { url = await player.url(nearKey(n), nearPath(n)); } catch { markGone(nearKey(n)); return; }
    downloadUrl(url, `${slugOf(n.device_id)}-nearmiss${n.id}.wav`);
  };

  // The shown turn's recordings for its AFE chart: which one is sounding,
  // whether each is kept (null: never saved; false: pruned since), and the
  // playhead. One player, so the chart and these rows stop each other. The
  // turn recording has no row column: its series says whether it was saved.
  // `segment` is the stretch of the sounding clip that plays ({from, to} s), or
  // null while all of it does.
  const playbackFor = t => ({
    playing: [WAKE, MIC, RECORDING].find(kind => playing === clipKey(t, kind)) || null,
    segment: player.segment,
    kept: kind => (kind === RECORDING ? !gone.has(clipKey(t, kind))
      : (kind === WAKE ? t.wake_file : t.audio_file) ? !gone.has(clipKey(t, kind)) : null),
    toggle: (kind, stretch = null) => toggleAudio(t, kind, stretch),
    elapsed: player.elapsed,
  });

  // Every wake clip the device still holds, in one archive — the hand-off to
  // a retraining run. Deliberately not "the clips for the turns on screen":
  // the store keeps hundreds and this list is the last 50, so most of the false
  // positives worth training on are already off the bottom of it.
  // Unlike the URLs for each clip there is nothing to replay it for, so it is not
  // kept.
  async function doWakeZip() {
    setZipping(true); setZipError('');
    try {
      downloadBlob(await API.blob(`/api/devices/${device}/wakeclips.zip`), `${slugOf(device)}-wakeclips.zip`);
    } catch (e) {
      setZipError(e.error || e.message || 'Could not build the archive');
    }
    setZipping(false);
  }

  const anyAudio = turns.some(t => t.audio_file);
  const anyWake  = turns.some(t => t.wake_file);

  // Success is over requests, so refused wake candidates and superseded turns
  // are left out of the denominator; refusals get their own tile.
  const refused = turns.filter(t => REFUSED_WAKE.has(t.terminal_reason));
  const requests = turns.filter(t => !REFUSED_WAKE.has(t.terminal_reason)
                                   && t.terminal_reason !== TERMINAL_REASON.SUPERSEDED);
  const done = requests.filter(turnSucceeded);
  const successPct = requests.length ? Math.round(done.length / requests.length * 100) : null;
  // Commit → TTS URL (STT run plus intent), for completed turns that measured both.
  const replies = done
    .filter(t => typeof t.stt_ms === 'number' && typeof t.tts_url_ms === 'number')
    .map(t => t.stt_ms + t.tts_url_ms)
    .sort((a, b) => a - b);
  const medianReply = replies.length ? replies[Math.floor(replies.length / 2)] : null;
  const fmtS = ms => (ms / 1000).toFixed(1) + 's';
  // Follow-up questions whose fate is known; a broken one (cut off, no mic,
  // disconnected) is a fault, not a user who stayed quiet.
  const asked = turns.filter(t => t.continuation && t.continuation !== FOLLOW_UP.PENDING);
  const answered = asked.filter(t => t.continuation === FOLLOW_UP.ANSWERED || t.continuation === FOLLOW_UP.WAKE);
  const brokenFollowups = asked.filter(t => FOLLOWUP_BROKEN.has(t.continuation));

  // All buffered turns (up to 50), newest first — rendered inside their own
  // scrollable box so a long history never scrolls the stat tiles (or the
  // rest of the tab) out of view.
  const recent = turns.slice().reverse();
  const scale = Math.max(3000, ...recent.map(t => turnSegments(t).shown));
  // A near miss's recording, `pruned` once a fetch of it has 404ed.
  const nearRecording = n => (n.recording === NEAR_MISS_RECORDING.SAVED && gone.has(nearKey(n))
    ? NEAR_MISS_RECORDING.PRUNED : n.recording);
  // A near miss's verdict: the one marked here until the polled list agrees, else the list's.
  const verdictOf = n => (n.id in marks ? marks[n.id] : n.verdict ?? null);
  // To review: it has a recording to listen to, and nobody has marked it.
  const toReview = n => verdictOf(n) == null && nearRecording(n) === NEAR_MISS_RECORDING.SAVED;
  const nearIn = (filter, n) => filter === NEAR_FILTER.ALL
    || (filter === NEAR_FILTER.TO_REVIEW ? toReview(n) : verdictOf(n) === filter);
  const reviewCount = nearMisses.filter(toReview).length;
  // Near misses interleave with the turns by time, newest first. Without
  // near_miss_v1 the device reports none, and the panel says why instead.
  const nearShown = !nearMissGap && nearFilter !== NEAR_FILTER.HIDDEN;
  const rows = [
    ...recent.map(t => ({ key: `turn:${t.turn_id}`, ts: t.ts, turn: t })),
    ...(nearShown ? nearMisses.slice().reverse().filter(n => nearIn(nearFilter, n))
      .map(n => ({ key: nearKey(n), ts: n.ts, near: n })) : []),
  ].sort((a, b) => b.ts - a.ts);
  // The detailed row: the one clicked while it is still listed, else the newest turn.
  const shown = rows.find(r => r.key === selected) || rows.find(r => r.turn) || rows[0] || null;

  // A mark the polled list now agrees with is no longer needed here.
  useEffect(() => {
    const listed = new Map(nearMisses.map(n => [String(n.id), n.verdict ?? null]));
    setMarks(m => {
      const left = Object.entries(m).filter(([id, v]) => listed.get(id) !== v);
      return left.length === Object.keys(m).length ? m : Object.fromEntries(left);
    });
  }, [nearMisses]);

  // Why `n` cannot be marked, or null: only an administrator marks, and only a
  // near miss with a recording to listen to, unless it has a mark to change.
  const markGapOf = n => (!isAdmin ? 'Only an administrator can mark near misses.'
    : verdictOf(n) == null && nearRecording(n) !== NEAR_MISS_RECORDING.SAVED
      ? 'There is no recording to judge this near miss by.' : null);

  // Shows `n` in the detail, its row focused (so the review keys reach this
  // list) and scrolled into view, and plays its recording.
  const review = n => {
    setSelected(nearKey(n));
    setFocusKey(nearKey(n));
    if (playing !== nearKey(n)) toggleNear(n);
  };
  useEffect(() => {
    if (!focusKey) return;
    const row = rootRef.current?.querySelector(`[data-row="${focusKey}"]`);
    if (row) { row.focus({ preventScroll: true }); row.scrollIntoView({ block: 'nearest' }); }
    setFocusKey(null);
  }, [focusKey]);

  // The next near miss to review listed after `n`, wrapping to the top; null when none is.
  const nextToReview = n => {
    const at = rows.findIndex(r => r.near?.id === n.id);
    return [...rows.slice(at + 1), ...rows.slice(0, Math.max(at, 0))]
      .find(r => r.near && toReview(r.near))?.near || null;
  };

  // Marks `n` with `verdict` (null clears it), shown at once and reverted if
  // the controller refuses. A verdict moves the review on to the next near miss.
  const mark = async (n, verdict) => {
    if (markGapOf(n)) return;
    const before = verdictOf(n);
    setMarks(m => ({ ...m, [n.id]: verdict }));
    setMarkError('');
    if (verdict != null) {
      const next = nextToReview(n);
      if (next) review(next);
    }
    try {
      await API.patch(`/api/devices/${n.device_id}/near_misses/${n.id}`, { verdict });
    } catch (e) {
      setMarks(m => ({ ...m, [n.id]: before }));
      setMarkError(`Near miss ${n.id} was not marked: ${e.error || e.message || 'the request failed'}`);
    }
  };

  // Starts a review at the newest near miss to review, listing the ones to
  // review if the list hides them.
  const startReview = () => {
    const first = nearMisses.slice().reverse().find(toReview);
    if (!first) return;
    if (nearFilter !== NEAR_FILTER.ALL && nearFilter !== NEAR_FILTER.TO_REVIEW) setNearFilter(NEAR_FILTER.TO_REVIEW);
    review(first);
  };

  // The review keys, while a near miss is shown and this list is the one in
  // use (no other modal over it): each verdict's key marks it, CLEAR clears its
  // mark, PLAY plays or stops it. Never while typing or with a modifier held.
  const onReviewKey = useRef(null);
  onReviewKey.current = e => {
    const n = shown?.near;
    const root = rootRef.current;
    if (!n || !root || !engaged.current || e.ctrlKey || e.metaKey || e.altKey || e.repeat) return;
    if (e.target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName)) return;
    if (![...document.querySelectorAll('[aria-modal="true"]')].every(m => m.contains(root))) return;
    const key = e.key.toLowerCase();
    const verdict = Object.values(NEAR_MISS_VERDICT).find(v => NEAR_MISS_VERDICTS[v].key === key);
    if (verdict || (key === NEAR_MISS_KEY.CLEAR && verdictOf(n) != null)) {
      e.preventDefault();
      mark(n, verdict || null);
    } else if (key === NEAR_MISS_KEY.PLAY && nearRecording(n) === NEAR_MISS_RECORDING.SAVED) {
      e.preventDefault();
      toggleNear(n);
    }
  };
  useEffect(() => {
    const onKey = e => onReviewKey.current(e);
    const onTouch = e => { engaged.current = !!rootRef.current?.contains(e.target); };
    window.addEventListener('keydown', onKey);
    window.addEventListener('pointerdown', onTouch, true);
    window.addEventListener('focusin', onTouch, true);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('pointerdown', onTouch, true);
      window.removeEventListener('focusin', onTouch, true);
    };
  }, []);

  const nearGapNote = nearMissGap && (
    <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', margin: '0 0 8px' }}>
      Near misses unavailable — {nearMissGap}
    </div>
  );

  // The row controls are bare glyphs rather than Pills: four of them share
  // the last column of a 14px-tall row, and anything with a border in there
  // reads as a second timeline.
  const glyphBtn = { background: 'none', border: 'none', padding: 0, cursor: 'pointer', fontSize: 10, lineHeight: 1 };

  // Across devices, each row names its device, as a button that opens it.
  const deviceCell = id => fleet && (
    <button type="button" onClick={e => { e.stopPropagation(); onOpenDevice(id); }} title={`Open ${nameOf(id)}`}
      style={{ ...glyphBtn, fontFamily: MONO, fontSize: 9, color: 'var(--text2)', width: 96, flexShrink: 0,
        textAlign: 'left', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
      {nameOf(id)}
    </button>
  );

  // One near miss, in the turn rows' columns so the two read as one
  // timeline: an amber marker where a turn has its trace number, the scores
  // where a turn has its stage bar, and its verdict where a turn has its
  // ending. Pressed, it shows its detail.
  const nearRow = n => {
    const key = nearKey(n);
    const recording = nearRecording(n);
    const saved = recording === NEAR_MISS_RECORDING.SAVED;
    const sounding = playing === key;
    const isSel = shown?.key === key;
    const verdict = verdictOf(n) && NEAR_MISS_VERDICTS[verdictOf(n)];
    const facts = [
      `peak ${n.peak.toFixed(2)}`,
      `floor ${n.floor.toFixed(2)}`,
      n.wake_threshold != null && `wake ${n.wake_threshold.toFixed(2)}`,
      NEAR_MISS_PROFILE_LABEL[n.profile] || n.profile,
      n.end_reason !== NEAR_MISS_END.BELOW && `cut short: ${n.end_reason}`,
    ].filter(Boolean).join(' · ');
    return (
      <div key={key} data-row={key}
        {...pressable(() => setSelected(key), { selected: isSel })}
        onMouseEnter={() => setHover(key)} onMouseLeave={() => setHover(null)}
        style={{ display: 'flex', alignItems: 'center', gap: 10, padding: '3px 0', cursor: 'pointer', borderRadius: 4,
          background: isSel ? 'var(--accent-tint)' : hover === key ? 'var(--hairline)' : 'transparent' }}>
        <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--lcd-amber)', width: 44, flexShrink: 0, textAlign: 'right' }}
          title={`Near miss ${n.near_miss_uuid}${n.wake_model_sha256 ? ` · model ${shortSha(n.wake_model_sha256)}` : ''}`}>
          ◇
        </span>
        <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', width: 86, flexShrink: 0, textAlign: 'right', whiteSpace: 'nowrap' }}>{fmtTurnWhen(n.ts)}</span>
        {deviceCell(n.device_id)}
        <span style={{ flex: 1, minWidth: 0, fontFamily: MONO, fontSize: 9, color: 'var(--text2)', whiteSpace: 'nowrap',
          overflow: 'hidden', textOverflow: 'ellipsis', borderLeft: '2px dashed var(--lcd-amber)', paddingLeft: 6 }}
          title="The wake score reached the near-miss floor without opening a wake: peak smoothed score, the floor, and the threshold that would have woken it">
          {facts}
          {!saved && <span style={{ color: 'var(--muted)' }}> · {NEAR_MISS_UNRECORDED[recording] || recording}</span>}
        </span>
        <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--text2)', width: 34, flexShrink: 0 }}>{fmtS(n.duration_ms)}</span>
        <span style={{ width: 40, flexShrink: 0 }}/>
        <span style={{ width: 10, flexShrink: 0 }}/>
        <span style={{ fontFamily: MONO, fontSize: 8, textTransform: 'uppercase', letterSpacing: '0.08em', width: 62, flexShrink: 0,
          whiteSpace: 'nowrap', color: verdict ? verdict.color : 'var(--lcd-amber)' }}
          title={verdict ? `Marked ${verdict.label.toLowerCase()}: ${verdict.title}` : 'Nobody has marked this near miss yet'}>
          {verdict ? verdict.tag : 'near miss'}
        </span>
        <span style={{ width: 60, flexShrink: 0, display: 'flex', gap: 4, justifyContent: 'flex-end' }}>
          {saved && (<>
            <button type="button" onClick={e => { e.stopPropagation(); toggleNear(n); }}
              aria-label={sounding ? 'Stop the near miss' : 'Play the near miss'}
              title={sounding ? 'Stop' : `Play the ${n.audio_ms != null ? fmtS(n.audio_ms) + ' ' : ''}recording of what nearly woke it`}
              style={{ ...glyphBtn, color: sounding ? 'var(--warn)' : 'var(--lcd-amber)' }}>
              {sounding ? '▮' : '▷'}
            </button>
            <button type="button" onClick={e => { e.stopPropagation(); downloadNear(n); }}
              aria-label="Download the near miss"
              title="Download the near miss — a sound that nearly woke it belongs in wake-word training"
              style={{ ...glyphBtn, color: 'var(--muted)' }}>⤓</button>
          </>)}
        </span>
      </div>
    );
  };

  return (
    <div ref={rootRef}>
      {/* Stat tiles */}
      <div style={{ display: 'flex', gap: 12, alignItems: 'flex-end', flexWrap: 'wrap', marginBottom: 16 }}>
        {state && <Lcd label="State" value={state.label} color={state.color} size={16}/>}
        <Lcd label={`Turns (last ${limit})`} value={turns.length} color="var(--lcd-green)" size={16}/>
        <Lcd label="Success" value={successPct != null ? successPct + '%' : '—'}
             color={successPct == null ? 'var(--lcd-dim)' : successPct >= 80 ? 'var(--lcd-green)' : 'var(--lcd-amber)'} size={16}/>
        <Lcd label="Median reply" value={medianReply != null ? fmtS(medianReply) : '—'} color="var(--lcd-dim)" size={16}/>
        <Lcd label="Refused wakes" value={refused.length}
             color={refused.length > 0 ? 'var(--lcd-amber)' : 'var(--lcd-dim)'} size={16}/>
        <Lcd label="Follow-ups answered" value={asked.length ? `${answered.length}/${asked.length}` : '—'}
             color={brokenFollowups.length > 0 ? 'var(--lcd-amber)' : 'var(--lcd-dim)'} size={16}/>
      </div>

      {turns.length === 0 && (nearMissGap || nearMisses.length === 0) ? (
        <div>
          <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--muted)', marginBottom: 8 }}>
            {emptyNote || <>No voice turns{nearMissGap ? '' : ' or near misses'} recorded yet — history starts when {fleet ? 'an Echo' : 'the device'} is next used.</>}
          </div>
          {nearGapNote}
        </div>
      ) : (
        <div style={{ position: 'relative' }}>
          {/* Legend */}
          <div style={{ display: 'flex', gap: 14, marginBottom: 10 }}>
            {TURN_STAGES.map(s => (
              <span key={s.key} style={{ display: 'inline-flex', alignItems: 'center', gap: 5, fontFamily: MONO, fontSize: 9, color: 'var(--text2)', textTransform: 'uppercase', letterSpacing: '0.08em' }}>
                <span style={{ width: 8, height: 8, borderRadius: 2, background: s.color, display: 'inline-block' }}/>
                {s.label}
              </span>
            ))}
            <span title={RESPONSE_LATENCY_TITLE} style={{ display: 'inline-flex', alignItems: 'center', gap: 5, fontFamily: MONO, fontSize: 9, color: 'var(--text2)', textTransform: 'uppercase', letterSpacing: '0.08em' }}>
              ↦ response latency
            </span>
            <span style={{ marginLeft: 'auto', display: 'inline-flex', alignItems: 'center', gap: 12 }}>
              {(recordingsOn || anyAudio) && (
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.08em' }}>
                  ▶ hear the mic{anyAudio ? '' : ' — next turn'}
                </span>
              )}
              {anyWake && (
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--lcd-amber)', textTransform: 'uppercase', letterSpacing: '0.08em' }}>
                  ▷ what woke it
                </span>
              )}
              {zipError && (
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--warn)' }}>{zipError}</span>
              )}
              {/* Pill takes no title, so the explanation hangs off a wrapper
                  — this is the button someone presses before a retraining
                  run, and the label alone does not say that. */}
              {anyWake && device && (
                <span title="Download every wake clip this device still has, as one .zip — the false positives to feed back into wake-word training">
                  <Pill small disabled={zipping} onClick={doWakeZip}>
                    {zipping ? 'Building…' : 'Download wake clips (.zip)'}
                  </Pill>
                </span>
              )}
            </span>
          </div>
          {/* Which near misses are listed, and the review that marks them. */}
          {!nearMissGap && (
            <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap', marginBottom: 10 }}>
              <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--lcd-amber)', textTransform: 'uppercase', letterSpacing: '0.08em', marginRight: 4 }}>
                ◇ Near misses
              </span>
              {/* Pill takes no title; each wrapper says what its filter lists. */}
              {NEAR_FILTERS.map(f => (
                <span key={f.id} title={f.title}>
                  <Pill small accent={nearFilter === f.id} onClick={() => setNearFilter(f.id)}>
                    {f.label}{f.id === NEAR_FILTER.HIDDEN ? '' : ` (${nearMisses.filter(n => nearIn(f.id, n)).length})`}
                  </Pill>
                </span>
              ))}
              {isAdmin && (
                <span style={{ marginLeft: 'auto' }}
                  title="Show the newest near miss nobody has marked and play it. Y or N then marks it and moves on to the next one; P plays it again.">
                  <Pill small accent disabled={!reviewCount} onClick={startReview}>
                    {reviewCount ? '▶ Start review' : 'Nothing to review'}
                  </Pill>
                </span>
              )}
            </div>
          )}
          {nearGapNote}

          {/* One stacked bar per turn and one line per near miss, newest
              first — own scroll container */}
          <div style={{ maxHeight: listHeight, overflowY: 'auto', paddingRight: 4 }}>
          {rows.length === 0 && (
            <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', padding: '3px 0' }}>
              No turns, and no near misses under this filter.
            </div>
          )}
          {rows.map(({ key, turn: t, near }) => {
            if (near) return nearRow(near);
            const seg = turnSegments(t);
            const end = turnEnd(t);
            const isSel = shown?.key === key;
            return (
              <div key={key}
                {...pressable(() => setSelected(key), { selected: isSel })}
                onMouseEnter={() => setHover(key)} onMouseLeave={() => setHover(null)}
                style={{ display: 'flex', alignItems: 'center', gap: 10, padding: '3px 0', cursor: 'pointer', borderRadius: 4,
                  background: isSel ? 'var(--accent-tint)' : hover === key ? 'var(--hairline)' : 'transparent' }}>
                {/* The trace number: the key of GET …/turns/{n}/trace. The title
                    carries the controller-log id (turn_uuid) of the same turn. */}
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', width: 44, flexShrink: 0, textAlign: 'right', whiteSpace: 'nowrap' }}
                  title={`Trace ${t.turn_id} — GET /api/devices/${t.device_id}/turns/${t.turn_id}/trace${t.turn_uuid ? ` · controller log id ${t.turn_uuid}` : ''}`}>
                  #{t.turn_id}
                </span>
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', width: 86, flexShrink: 0, textAlign: 'right', whiteSpace: 'nowrap' }}>{fmtTurnWhen(t.ts)}</span>
                {deviceCell(t.device_id)}
                <div style={{ flex: 1, display: 'flex', height: 14, alignItems: 'stretch' }}>
                  {TURN_STAGES.map(s => seg[s.key] > 0 && (
                    <div key={s.key} style={{
                      width: `${seg[s.key] / scale * 100}%`, background: s.color,
                      borderRadius: 3, marginRight: 2, minWidth: 3,
                    }}/>
                  ))}
                </div>
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--text2)', width: 34, flexShrink: 0 }}>{fmtS(seg.shown)}</span>
                {/* Slot reserved when unmeasured, so the columns stay aligned. */}
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--text)', width: 40, flexShrink: 0, whiteSpace: 'nowrap' }}
                  title={typeof t.response_latency_ms === 'number' ? RESPONSE_LATENCY_TITLE : undefined}>
                  {typeof t.response_latency_ms === 'number' ? `↦${fmtS(t.response_latency_ms)}` : ''}
                </span>
                {/* ↩ answered a question; ? asked one, colored by what became of it. */}
                <span style={{ fontFamily: MONO, fontSize: 10, width: 10, flexShrink: 0, textAlign: 'center',
                  color: t.continuation ? followup(t.continuation)[1] : 'var(--text2)' }}
                  title={t.continuation ? `Asked a follow-up: ${followup(t.continuation)[0]}`
                    : t.reply_to ? 'Answered a follow-up question' : undefined}>
                  {t.continuation ? '?' : t.reply_to ? '↩' : ''}
                </span>
                <span style={{ fontFamily: MONO, fontSize: 8, textTransform: 'uppercase', letterSpacing: '0.08em', width: 62, flexShrink: 0, color: end.color }}>
                  {end.label.replace(/_/g, ' ')}
                </span>
                {/* The turn's saved audio, in the order it happened: the wake
                    clip that opened the turn, then the utterance that
                    followed. The slot is reserved even when a turn has
                    neither, so the columns stay aligned as the retention
                    windows roll — and the two roll at very different rates,
                    which is why each pair tests its own key. */}
                <span style={{ width: 60, flexShrink: 0, display: 'flex', gap: 4, justifyContent: 'flex-end' }}>
                  {t.wake_file && !gone.has(clipKey(t, WAKE)) && (<>
                    <button type="button" onClick={e => { e.stopPropagation(); toggleAudio(t, WAKE); }}
                      aria-label={playing === clipKey(t, WAKE) ? 'Stop the wake clip' : 'Play the wake clip'}
                      title={playing === clipKey(t, WAKE) ? 'Stop'
                        : 'Play the 1.4s that crossed the wake threshold — what triggered it, not the command'}
                      style={{ ...glyphBtn, color: playing === clipKey(t, WAKE) ? 'var(--warn)' : 'var(--lcd-amber)' }}>
                      {playing === clipKey(t, WAKE) ? '▮' : '▷'}
                    </button>
                    <button type="button" onClick={e => { e.stopPropagation(); downloadAudio(t, WAKE); }}
                      aria-label="Download the wake clip"
                      title="Download the wake clip — a false trigger belongs in wake-word training"
                      style={{ ...glyphBtn, color: 'var(--muted)' }}>⤓</button>
                  </>)}
                  {t.audio_file && !gone.has(clipKey(t, MIC)) && (<>
                    <button type="button" onClick={e => { e.stopPropagation(); toggleAudio(t, MIC); }}
                      aria-label={playing === clipKey(t, MIC) ? 'Stop the mic audio' : 'Play the mic audio'}
                      title={playing === clipKey(t, MIC) ? 'Stop' : 'Play the mic audio for this turn'}
                      style={{ ...glyphBtn, color: playing === clipKey(t, MIC) ? 'var(--warn)' : 'var(--text2)' }}>
                      {playing === clipKey(t, MIC) ? '▮' : '▶'}
                    </button>
                    <button type="button" onClick={e => { e.stopPropagation(); downloadAudio(t, MIC); }}
                      aria-label="Download the mic audio" title="Download the WAV"
                      style={{ ...glyphBtn, color: 'var(--muted)' }}>⤓</button>
                  </>)}
                </span>
              </div>
            );
          })}
          </div>

          {/* The detail of the selected row (the newest turn until one is clicked) */}
          {shown?.turn && <TurnDetail turn={shown.turn} turns={turns} deviceLabel={fleet ? nameOf(shown.turn.device_id) : null}
            onSelect={id => setSelected(`turn:${id}`)} playback={playbackFor(shown.turn)}/>}
          {shown?.near && <NearMissDetail near={shown.near} deviceLabel={fleet ? nameOf(shown.near.device_id) : null}
            recording={nearRecording(shown.near)} playing={playing === nearKey(shown.near)}
            playingBest={playing === nearKey(shown.near) && player.segment != null}
            onPlay={() => toggleNear(shown.near)} onPlayBest={s => toggleNear(shown.near, s)}
            onDownload={() => downloadNear(shown.near)} elapsed={player.elapsed}
            verdict={verdictOf(shown.near)} onMark={v => mark(shown.near, v)} markGap={markGapOf(shown.near)}
            markError={markError}/>}
        </div>
      )}
    </div>
  );
}

// ─── Activity across devices ─────────────────────────────────────────────────
// The page's Activity view: every device's voice turns and near misses as one
// list by time (GET /api/turns, /api/near_misses), or one device's through its
// own endpoints, with the device tab's turn detail. It reloads every 10 s and as
// each turn in view lands (turn_complete); near misses raise no event, so the
// poll brings them in. A range of wake scores narrows the list, and the
// archives take every saved clip that matches the same filters, not only the rows
// listed.

// The Activity view's archives (GET /api/wakeclips.zip, /api/near_misses.zip,
// which `query.verdict` narrows to the near misses marked with it); each value
// is also the word the controller uses in the archive file name.
const ARCHIVE = Object.freeze({
  WAKES: 'wakes', MISSED_WAKES: 'missedwakes', CORRECTLY_IGNORED: 'correctlyignored', NEAR_MISSES: 'nearmisses',
});
const ARCHIVES = Object.freeze({
  [ARCHIVE.WAKES]: {
    path: '/api/wakeclips.zip', query: {}, label: 'Confirmed wakes', noun: 'wake clip',
    title: 'The wake clip of every confirmed wake (a wake turn not refused) with a wake score in the range, from the devices shown: '
      + 'every clip still kept (the newest 1,000 per device, saved while Save wake clips is on), not only the turns listed',
  },
  [ARCHIVE.MISSED_WAKES]: {
    path: '/api/near_misses.zip', query: { verdict: NEAR_MISS_VERDICT.MISSED_WAKE },
    label: 'Missed wakes', noun: 'near miss marked missed wake',
    title: 'The recording of every near miss marked Missed wake, with a peak score in the range, from the devices shown, '
      + 'not only the ones listed: the wake word was said and the Echo stayed quiet, the positives for wake-word training',
  },
  [ARCHIVE.CORRECTLY_IGNORED]: {
    path: '/api/near_misses.zip', query: { verdict: NEAR_MISS_VERDICT.CORRECTLY_IGNORED },
    label: 'Correctly ignored', noun: 'near miss marked correctly ignored',
    title: 'The recording of every near miss marked Correctly ignored, with a peak score in the range, from the devices shown, '
      + 'not only the ones listed: it was not the wake word, the negatives for wake-word training',
  },
  [ARCHIVE.NEAR_MISSES]: {
    path: '/api/near_misses.zip', query: {}, label: 'All near misses', noun: 'near-miss recording',
    title: 'The recording of every near miss, marked or not, with a peak score in the range, from the devices shown: '
      + 'every recording still kept (the newest 1,000 per device, recorded while Save wake clips is on, and every marked one), '
      + 'not only the near misses listed',
  },
});
const SCORE_RANGE_ALL = Object.freeze([0, 1]);
// em_clip_windows: an archive's clips whole, or cut to a fixed length around
// the window the wake model scored highest (or every window at or above a
// score), from the windows the controller scored once each clip was saved. The
// bounds are the controller's; the length starts at BCResNet's window.
const CROP_SECONDS = Object.freeze({ MIN: 0.1, MAX: 10, DEFAULT: '1.4' });
// Pill takes no title; these go on the wrappers.
const CROP_TITLE = Object.freeze({
  WHOLE: 'Each clip as saved: it runs as long as the Echo\'s score stayed up, plus 300 ms either side, so the lengths vary '
    + '(about 2–3.6 s). Training keeps only a clip\'s first 1.4 s, which is rarely the part that scored.',
  CROP: 'Cut this length around a window of each clip, from the windows the controller scored (one 1.4 s window every '
    + '160 ms, with the wake model that scored the clip on the Echo) once the clip was saved. A clip it has not reached '
    + 'yet is scored before the archive is cut.',
  BEST: 'One crop per clip, around the window that scored highest',
  EVERY: 'One crop around every window that scored at or above this, unsmoothed. Windows are 160 ms apart, so neighbouring '
    + 'crops overlap; each crop\'s name starts with its clip\'s, so a training split can keep a clip\'s crops together.',
});

function FleetActivity({ devices, globalConfig, onOpenDevice, isAdmin }) {
  const [choice, setChoice] = useState(null); // device_id shown alone, or null for every device
  const [load, setLoad] = useState(null);     // {scope, turns, nearMisses} of the newest load; turns null: it failed
  const [[lo, hi], setRange] = useState(SCORE_RANGE_ALL);
  // How the archives cut each clip: whole, or `seconds` around the best window,
  // or (`every`) around each window scoring at least `min`. Text, as typed.
  const [cut, setCut] = useState(() => ({
    crop: false, seconds: CROP_SECONDS.DEFAULT, every: false,
    min: (globalConfig?.nearMissThreshold ?? 0.17).toFixed(2),
  }));
  const [archiving, setArchiving] = useState(null);       // the ARCHIVE being downloaded
  const [scoring, setScoring] = useState(null);           // saved clips the controller has still to score; null: unknown
  const [archiveError, setArchiveError] = useState('');
  const approved = devices.filter(d => d.approved);
  // A device deleted while chosen falls back to every device.
  const scope = approved.some(d => d.device_id === choice) ? choice : null;

  useEffect(() => {
    let live = true;
    let seq = 0;
    const base = scope ? `/api/devices/${scope}` : '/api';
    const fetchAll = () => {
      const n = ++seq;
      Promise.all([
        API.get(`${base}/turns?limit=${ACTIVITY_LIMIT.FLEET}`).catch(() => null),
        API.get(`${base}/near_misses?limit=${ACTIVITY_LIMIT.FLEET}`).catch(() => null),
        API.get('/api/clip_windows').catch(() => null),
      ]).then(([turns, nearMisses, windows]) => {
        if (!live || n !== seq) return;   // overtaken by a newer load
        setScoring(windows?.pending ?? null);
        // A failed request keeps what this scope last showed.
        setLoad(prev => {
          const kept = prev?.scope === scope ? prev : null;
          return { scope, turns: turns ?? kept?.turns ?? null, nearMisses: nearMisses ?? kept?.nearMisses ?? [] };
        });
      });
    };
    fetchAll();
    const iv = setInterval(fetchAll, 10000);
    const unsubscribe = subscribeEvents(msg => {
      if (msg.type === EVENT_TYPE.TURN_COMPLETE && (!scope || msg.device_id === scope)) fetchAll();
    });
    return () => { live = false; clearInterval(iv); unsubscribe(); };
  }, [scope]);

  const nameOf = d => d.label || d.device_id;
  const labels = Object.fromEntries(devices.map(d => [d.device_id, d.label]));
  const inView = scope ? approved.filter(d => d.device_id === scope) : approved;
  const recordingsOn = inView.some(d => effectiveConfig(globalConfig, d).saveUtterances);
  // Devices in view without near_miss_v1. When none has it the list explains why
  // it holds no near miss; when only some lack it, the note above names them.
  const gaps = inView.map(d => [d, capabilityGap(liveCapabilities(d), CAPABILITY.NEAR_MISS)]).filter(([, g]) => g);
  const gapText = gaps.map(([d, g]) => (scope ? g : `${nameOf(d)}: ${g}`)).join(' · ');
  const noneReport = inView.length > 0 && gaps.length === inView.length;
  const current = load?.scope === scope ? load : null;
  const muted = { fontFamily: MONO, fontSize: 11, color: 'var(--muted)' };

  // The range of wake scores: a turn by its wake score, a near miss by its peak,
  // with both ends inclusive (as the archives). At the whole range nothing is
  // filtered; narrowed, a turn with no score (button, reply) is outside it.
  const fmtScore = v => v.toFixed(2);
  const narrowed = lo > SCORE_RANGE_ALL[0] || hi < SCORE_RANGE_ALL[1];
  const inRange = v => v != null && v >= lo && v <= hi;
  const turns = current?.turns && narrowed ? current.turns.filter(t => inRange(t.wake_score)) : current?.turns;
  const nearMisses = current && narrowed ? current.nearMisses.filter(n => inRange(n.peak)) : current?.nearMisses;
  const unscored = narrowed && current?.turns ? current.turns.filter(t => t.wake_score == null).length : 0;

  // The crop the archives ask for (null: whole clips), or why it cannot be asked.
  const cropSeconds = Number(cut.seconds);
  const cropMin = Number(cut.min);
  const cutError = !cut.crop ? ''
    : !(cut.seconds.trim() && cropSeconds >= CROP_SECONDS.MIN && cropSeconds <= CROP_SECONDS.MAX)
      ? `Crop length must be ${CROP_SECONDS.MIN}–${CROP_SECONDS.MAX} s`
      : cut.every && !(cut.min.trim() && cropMin >= 0 && cropMin <= 1) ? 'Window score must be 0–1' : '';
  const cropQuery = !cut.crop || cutError ? {}
    : { crop: cropSeconds.toFixed(2), ...(cut.every ? { window_min: cropMin.toFixed(2) } : {}) };
  // As em_api._archive_name names it.
  const cropSuffix = !cropQuery.crop ? ''
    : `-crop${cropQuery.crop}s${cropQuery.window_min ? `-min${cropQuery.window_min}` : ''}`;
  const numberField = { width: 52, fontFamily: MONO, fontSize: 11, boxSizing: 'border-box' };
  const capsLabel = { fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.08em' };

  async function download(kind) {
    const a = ARCHIVES[kind];
    const query = new URLSearchParams({ min: fmtScore(lo), max: fmtScore(hi), ...a.query, ...cropQuery });
    if (scope) query.set('device', scope);
    setArchiving(kind); setArchiveError('');
    try {
      downloadBlob(await API.blob(`${a.path}?${query}`),
        `echomuse-${kind}${scope ? `-${fileSlug(labels[scope], scope)}` : ''}-${fmtScore(lo)}-${fmtScore(hi)}${cropSuffix}.zip`);
    } catch (e) {
      // A cropped archive's 404 can also mean no window reached the score; the controller says which.
      setArchiveError(e.status === 404 && !cropQuery.crop ? `${a.label}: no saved ${a.noun} matches`
        : `${a.label}: ${e.error || e.message || 'download failed'}`);
    }
    setArchiving(null);
  }

  return (
    <Panel label={`Voice activity — ${scope ? nameOf(inView[0]) : 'every device'}`}>
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', marginBottom: 14 }}>
        <Pill small accent={!scope} onClick={() => setChoice(null)}>All devices</Pill>
        {approved.map(d => (
          <Pill key={d.device_id} small accent={scope === d.device_id} onClick={() => setChoice(d.device_id)}>
            {nameOf(d)}
          </Pill>
        ))}
      </div>
      <div style={{ display: 'flex', gap: '8px 16px', alignItems: 'flex-end', flexWrap: 'wrap', marginBottom: 14 }}>
        <div style={{ flex: '0 1 280px', minWidth: 200 }}>
          <RangeSlider label="Wake score" lo={lo} hi={hi} format={fmtScore} onChange={(a, b) => setRange([a, b])}/>
        </div>
        <span style={{ flex: '1 1 180px', fontFamily: MONO, fontSize: 9, color: 'var(--muted)', lineHeight: 1.6 }}>
          turns by their wake score, near misses by their peak
          {narrowed && (<>
            {unscored > 0 && ` · ${unscored} turn${unscored === 1 ? '' : 's'} without one (button, reply) hidden`}
            {' · '}
            <button type="button" onClick={() => setRange(SCORE_RANGE_ALL)} style={{
              background: 'none', border: 'none', padding: 0, cursor: 'pointer', font: 'inherit',
              color: 'var(--accent)', textDecoration: 'underline' }}>
              show all
            </button>
          </>)}
        </span>
        <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 6, marginLeft: 'auto' }}>
          <span style={{ display: 'inline-flex', flexWrap: 'wrap', gap: 6, alignItems: 'center', justifyContent: 'flex-end' }}>
            {archiveError && <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--warn)' }}>{archiveError}</span>}
            <span style={capsLabel}>⤓ .zip</span>
            {/* Pill takes no title; the wrapper says what each archive holds. */}
            {Object.values(ARCHIVE).map(kind => (
              <span key={kind} title={ARCHIVES[kind].title}>
                <Pill small disabled={archiving != null || !!cutError} onClick={() => download(kind)}>
                  {archiving === kind ? (cropQuery.crop && scoring > 0 ? 'Scoring…' : 'Building…') : ARCHIVES[kind].label}
                </Pill>
              </span>
            ))}
          </span>
          {/* How each archive cuts its clips. */}
          <span style={{ display: 'inline-flex', flexWrap: 'wrap', gap: 6, alignItems: 'center', justifyContent: 'flex-end' }}>
            {cutError && <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--warn)' }}>{cutError}</span>}
            {scoring > 0 && (
              <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)' }}
                title="The controller scores the wake model's windows over each saved clip in the background, once. A cropped archive scores any clip it has not reached yet before cutting.">
                scoring saved clips · {scoring} left
              </span>
            )}
            <span style={capsLabel}>each clip</span>
            <span title={CROP_TITLE.WHOLE}>
              <Pill small accent={!cut.crop} onClick={() => setCut(c => ({ ...c, crop: false }))}>Whole</Pill>
            </span>
            <span title={CROP_TITLE.CROP}>
              <Pill small accent={cut.crop} onClick={() => setCut(c => ({ ...c, crop: true }))}>Crop</Pill>
            </span>
            {cut.crop && (<>
              <label title={CROP_TITLE.CROP} style={{ ...muted, display: 'inline-flex', alignItems: 'center', gap: 4 }}>
                <input type="number" min={CROP_SECONDS.MIN} max={CROP_SECONDS.MAX} step="0.1" value={cut.seconds}
                  aria-label="Crop length in seconds" style={numberField}
                  onChange={e => setCut(c => ({ ...c, seconds: e.target.value }))}/>
                s around
              </label>
              <span title={CROP_TITLE.BEST}>
                <Pill small accent={!cut.every} onClick={() => setCut(c => ({ ...c, every: false }))}>the best window</Pill>
              </span>
              <span title={CROP_TITLE.EVERY}>
                <Pill small accent={cut.every} onClick={() => setCut(c => ({ ...c, every: true }))}>every window ≥</Pill>
              </span>
              <input type="number" min="0" max="1" step="0.01" value={cut.min} disabled={!cut.every}
                title={CROP_TITLE.EVERY} aria-label="Lowest window score to crop around"
                style={{ ...numberField, opacity: cut.every ? 1 : 0.45 }}
                onChange={e => setCut(c => ({ ...c, min: e.target.value }))}/>
            </>)}
          </span>
        </div>
      </div>
      {!noneReport && gaps.length > 0 && (
        <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', marginBottom: 8 }}>
          Near misses unavailable on {gapText}
        </div>
      )}
      {!current ? <div style={muted}>loading…</div>
        : turns == null ? <div style={{ ...muted, color: 'var(--warn)' }}>Could not load the activity — retrying every 10 s</div>
        : <TurnObservability turns={turns} nearMisses={nearMisses}
            nearMissGap={noneReport ? gapText : null} labels={labels} fleet
            onOpenDevice={onOpenDevice} recordingsOn={recordingsOn} limit={ACTIVITY_LIMIT.FLEET} listHeight="45vh" isAdmin={isAdmin}
            emptyNote={narrowed ? `No turn or near miss with a wake score of ${fmtScore(lo)}–${fmtScore(hi)} among the newest ${ACTIVITY_LIMIT.FLEET} of each.` : null}/>}
    </Panel>
  );
}

// ─── Connectivity tab ─────────────────────────────────────────────────────────
// WiFi for each device: shows the current connection and switches networks
// safely (the device runs the switch and rolls it back automatically if it
// fails — see internal/wifi in the firmware). The change is sent from here
// without waiting for the outcome: POST returns 202, the device drops off
// while it switches, and the outcome arrives as a
// device_update event carrying device.wifi.{pending,last_result}.

function ConnectivityTab({ device, row }) {
  const [networks, setNetworks]   = useState(null);   // null = never scanned
  const [scanning, setScanning]   = useState(false);
  const [scanError, setScanError] = useState('');
  const [ssid, setSsid]           = useState('');
  const [psk, setPsk]             = useState('');
  const [showPsk, setShowPsk]     = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [submitError, setSubmitError] = useState('');

  const s       = device.stats || null;
  const wifi    = device.wifi || {};
  const pending = wifi.pending || null;
  const result  = wifi.last_result || null;
  const currentSsid = s?.wifiSsid || null;

  async function doScan() {
    setScanning(true); setScanError('');
    try {
      const r = await API.post(`/api/devices/${device.device_id}/wifi/scan`, {});
      setNetworks(r.networks || []);
    } catch (e) {
      setScanError(e.error || e.message || 'Scan failed');
    }
    setScanning(false);
  }

  async function doSwitch() {
    setConfirming(false); setSubmitError('');
    try {
      await API.post(`/api/devices/${device.device_id}/wifi`, { ssid, psk });
      // Pending state arrives via the device_update push event.
    } catch (e) {
      setSubmitError(e.error || e.message || 'Request failed');
    }
  }

  const busy  = !!pending;
  const valid = ssid && (!psk || (psk.length >= 8 && psk.length <= 63)) &&
                !/["\\]/.test(ssid) && !/["\\]/.test(psk);
  // wifi_control: the firmware manages Wi-Fi for EchoMuse. On an Echo Show
  // Android owns the network ("managed by the device"); the controller
  // refuses these requests there too.
  const wifiGap = capabilityGap(liveCapabilities(device), CAPABILITY.WIFI_CONTROL);
  const locked  = busy || !!wifiGap;

  return (
    <div style={{ minHeight:'100%', display:'flex', flexDirection:'column', gap:16 }}>

      {/* Outcome banners — pending wins over last result */}
      {pending && (
        <div style={{ background:'rgba(64,88,120,0.10)', border:'1px solid rgba(64,88,120,0.25)', borderRadius:8, padding:'12px 16px' }}>
          <div style={{ fontFamily:MONO, fontSize:11, color:'var(--accent)' }}>
            Switching to “{pending.ssid}” — the device will drop offline while it changes network.
          </div>
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginTop:4 }}>
            If it can't associate, get an IP, or reach this controller, it rolls back to the previous
            network automatically and reports the failure here (allow ~2 minutes).
          </div>
        </div>
      )}
      {!pending && result && (
        <div style={{ background: result.ok ? 'rgba(40,96,64,0.08)' : 'rgba(192,96,26,0.08)', border:`1px solid ${result.ok ? 'rgba(40,96,64,0.25)' : 'rgba(192,96,26,0.3)'}`, borderRadius:8, padding:'12px 16px' }}>
          <div style={{ fontFamily:MONO, fontSize:11, color: result.ok ? 'var(--ok)' : 'var(--warn)' }}>
            {result.ok
              ? `Switched to “${result.ssid}” successfully.`
              : `Change to “${result.ssid}” failed — previous network restored.`}
          </div>
          {!result.ok && result.error && (
            <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginTop:4 }}>{result.error}</div>
          )}
        </div>
      )}

      <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:16, alignItems:'start' }}>
        <Panel label="Current connection">
          {row('Network', currentSsid || '—')}
          {row('IP', deviceIp(device) || '—')}
          <div style={{ display:'flex', justifyContent:'space-between', alignItems:'center' }}>
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', textTransform:'uppercase', letterSpacing:'0.08em' }}>Signal</span>
            <div style={{ display:'flex', alignItems:'center', gap:8 }}>
              <span style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)' }}>{s?.wifiRssi != null ? `${s.wifiRssi} dBm` : '—'}</span>
              <SignalBars rssi={s?.wifiRssi ?? null}/>
            </div>
          </div>
          {!s && <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:8 }}>waiting for device stats…</div>}
        </Panel>

        <Panel label="Visible networks">
          <div style={{ display:'flex', alignItems:'center', gap:10, marginBottom:8 }}>
            <Pill small disabled={scanning || !device.connected || locked} onClick={doScan}>
              {scanning ? 'Scanning…' : networks ? 'Rescan' : 'Scan'}
            </Pill>
            {wifiGap && <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>{wifiGap}</span>}
            {scanError && <span style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)' }}>{scanError}</span>}
          </div>
          {networks && networks.length === 0 && (
            <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>No networks found.</div>
          )}
          {networks && networks.length > 0 && (
            <div style={{ maxHeight:170, overflowY:'auto' }}>
              {networks.map(n => (
                <div key={n.ssid} {...pressable(() => setSsid(n.ssid), { disabled: locked, selected: ssid === n.ssid })}
                  style={{ display:'flex', justifyContent:'space-between', alignItems:'center', padding:'5px 8px', borderRadius:6, cursor: locked ? 'default' : 'pointer', background: ssid === n.ssid ? 'rgba(64,88,120,0.12)' : 'transparent' }}>
                  <span style={{ fontFamily:MONO, fontSize:11, color: ssid === n.ssid ? 'var(--accent)' : 'var(--text)' }}>
                    {n.ssid}{n.ssid === currentSsid ? '  ← current' : ''}
                  </span>
                  <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>{n.signal} dBm</span>
                </div>
              ))}
            </div>
          )}
        </Panel>
      </div>

      <Panel label="Change network">
        {wifiGap ? (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)', marginBottom:12 }}>
            Wi-Fi changes unavailable: {wifiGap}
          </div>
        ) : (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginBottom:12 }}>
            The device applies the change itself and rolls back automatically if the new network doesn't
            work out — including when it connects but can't reach this controller (wrong VLAN, isolated
            guest network). The previous network is only discarded once the device reports back here.
          </div>
        )}
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr auto', gap:12, alignItems:'end' }}>
          <div>
            <div style={{ fontFamily:MONO, fontSize:9, color:'var(--text2)', letterSpacing:'0.08em', marginBottom:4 }}>SSID</div>
            <input type="text" value={ssid} disabled={locked} onChange={e => setSsid(e.target.value)} aria-label="SSID"
              placeholder="Network name" style={{ width:'100%', boxSizing:'border-box' }}/>
          </div>
          <div>
            <div style={{ fontFamily:MONO, fontSize:9, color:'var(--text2)', letterSpacing:'0.08em', marginBottom:4 }}>Passphrase</div>
            <div style={{ display:'flex', gap:6 }}>
              <input type={showPsk ? 'text' : 'password'} value={psk} disabled={locked} onChange={e => setPsk(e.target.value)} aria-label="Passphrase"
                placeholder="WPA passphrase (blank = open)" style={{ flex:1, boxSizing:'border-box' }}/>
              <Pill small onClick={() => setShowPsk(v => !v)}>{showPsk ? 'Hide' : 'Show'}</Pill>
            </div>
          </div>
          {!confirming ? (
            <Pill accent disabled={!valid || locked || !device.connected} onClick={() => setConfirming(true)}>Switch…</Pill>
          ) : (
            <div style={{ display:'flex', gap:8 }}>
              <Pill danger onClick={doSwitch}>Confirm switch</Pill>
              <Pill small onClick={() => setConfirming(false)}>Cancel</Pill>
            </div>
          )}
        </div>
        {ssid && !valid && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)', marginTop:8 }}>
            {/["\\]/.test(ssid + psk)
              ? 'SSID/passphrase cannot contain " or \\ characters.'
              : 'WPA passphrase must be 8–63 characters (leave blank for an open network).'}
          </div>
        )}
        {submitError && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)', marginTop:8 }}>{submitError}</div>
        )}
        {!device.connected && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)', marginTop:8 }}>Device offline — connect it before changing networks.</div>
        )}
      </Panel>
    </div>
  );
}

// ─── Wake-word sample collection ──────────────────────────────────────────────
//
// Records this device's mic continuously and cuts it into clips at the
// silences, for training a custom wake word on the array that will actually
// hear it (see em_samples.py). The mode SUSPENDS the assistant on that
// device, which is the one thing this panel must never let anyone discover
// by accident — hence the warning above the switch rather than below it, and
// the state repeated in the device header.

function SamplesTab({ device, isAdmin }) {
  const [data, setData]       = useState(null);   // null = not fetched yet
  const [busy, setBusy]       = useState(false);
  const [error, setError]     = useState('');
  const [confirmWipe, setConfirmWipe] = useState(false);
  const [zipping, setZipping] = useState(false);
  const [chartOf, setChartOf] = useState(null);   // clip name whose chart is open

  const id   = device.device_id;
  const on   = !!device.collectMode;
  const slug = fileSlug(device.label, id);
  // Clip URLs are cached for replay and download (useClipPlayer).
  const player   = useClipPlayer(id);
  const playing  = player.playing;                // clip name currently sounding
  const clipPath = name => `/api/devices/${id}/samples/${name}`;

  const load = async () => {
    try {
      setData(await API.get(`/api/devices/${id}/samples`));
      setError('');
    } catch (e) {
      setError(e.error || e.message || 'Could not read samples');
    }
  };

  // Poll while collecting — clips appear on their own, and a panel that only
  // updated on a click would look like nothing was being recorded.
  useEffect(() => {
    load();
    if (!on) return;
    const iv = setInterval(load, 5000);
    return () => clearInterval(iv);
  }, [id, on]);

  async function setMode(enabled) {
    setBusy(true); setError('');
    try {
      await API.post(`/api/devices/${id}/collect`, { enabled });
      // device.collectMode arrives on the events socket; refresh the list so
      // a flushed final clip shows up immediately on stop.
      await load();
    } catch (e) {
      setError(e.error || e.message || 'Could not change the mode');
    }
    setBusy(false);
  }

  async function doZip() {
    setZipping(true); setError('');
    try {
      downloadBlob(await API.blob(`/api/devices/${id}/samples.zip`), `${slug}-samples.zip`);
    } catch (e) {
      setError(e.error || e.message || 'Could not build the archive');
    }
    setZipping(false);
  }

  async function doDelete(name) {
    player.stop();
    try {
      await API.del(`/api/devices/${id}/samples/${name}`);
      player.forget(name);
      setData(d => d && { ...d, clips: d.clips.filter(c => c.name !== name),
                          count: Math.max(0, d.count - 1) });
    } catch (e) {
      setError(e.error || e.message || 'Could not delete that clip');
    }
  }

  async function doWipe() {
    player.stop();
    setConfirmWipe(false);
    try {
      await API.del(`/api/devices/${id}/samples`);
      player.forgetAll();
      await load();
    } catch (e) {
      setError(e.error || e.message || 'Could not delete the samples');
    }
  }

  const clips = data?.clips || [];
  const totalMb = data ? (data.bytes / 1048576).toFixed(1) : '—';
  const totalS  = data ? Math.round(data.ms / 1000) : 0;

  return (
    <div style={{ minHeight:'100%', display:'flex', flexDirection:'column', gap:16 }}>

      <Panel label="Sample collection">
        <div style={{ fontFamily:SANS, fontSize:12, color:'var(--text2)', lineHeight:1.55, marginBottom:14 }}>
          Records this device&apos;s microphone continuously and cuts it into
          clips at the silences — say the wake word around the room and each
          one lands here as a WAV. The audio is exactly what the wake model
          scores, so the clips train against the array that will hear them.
        </div>
        <div style={{ background:'linear-gradient(160deg,var(--lcd-face),var(--lcd-deep))', border:'1px solid var(--lcd-line)', borderRadius:6, padding:'10px 12px', marginBottom:16 }}>
          <span style={{ fontFamily:MONO, fontSize:10, color:'var(--lcd-amber)', lineHeight:1.6 }}>
            While collecting, this device answers nothing — no wake word, no
            button turn, and nothing reaches Home Assistant. Its ring throbs
            magenta so it is obvious from the room.
          </span>
        </div>

        <div style={{ display:'flex', gap:16, alignItems:'flex-end', flexWrap:'wrap', marginBottom:16 }}>
          <Lcd label="Mode"    value={on ? 'COLLECTING' : 'OFF'} color={on ? 'var(--lcd-amber)' : 'var(--lcd-dim)'}/>
          <Lcd label="Clips"   value={data ? String(data.count) : '—'} color="var(--lcd-green)"/>
          <Lcd label="Audio"   value={data ? `${totalS}s` : '—'} color="var(--lcd-dim)"/>
          <Lcd label="On disk" value={data ? `${totalMb} MB` : '—'} color="var(--lcd-dim)"/>
        </div>

        <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap' }}>
          <Pill big accent={!on} danger={on}
                disabled={!isAdmin || busy || (!on && !!device.ambientMode)}
                onClick={() => setMode(!on)}>
            {busy ? '…' : on ? 'Stop collecting' : 'Start collecting'}
          </Pill>
          {/* Mutually exclusive with ambient recording, and refused by the
              endpoint too: both modes want the same frames, and a segmenter
              running under an ambient session cuts the wake word out of the
              room noise it was recording. */}
          {!on && !!device.ambientMode && (
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)' }}>
              Stop ambient recording first — both modes want the same audio
            </span>
          )}
          {!device.connected && (
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)' }}>
              {on ? 'Armed — collection resumes when the device reconnects'
                  : 'Device offline — this will take effect on its next connect'}
            </span>
          )}
          {device.connected && on && (
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
              {device.collectClips || 0} clip(s) this session
              {device.collectLastMs ? ` · last ${device.collectLastMs}ms` : ''}
            </span>
          )}
        </div>
        {/* What the segmenter is hearing. A room whose floor sits just under
            the open threshold and a muted mic both produce an empty list;
            these three numbers are what tell them apart without a log tail. */}
        {data?.live && (
          <div style={{ marginTop:14, borderTop:'1px solid var(--hairline)', paddingTop:10,
                        fontFamily:MONO, fontSize:9, color:'var(--muted)', lineHeight:1.7 }}>
            Room floor {data.live.floor_db}dBFS · clips open above {data.live.open_db}dBFS
            {' · '}{data.live.frames} frame(s) heard
            {data.live.dropped_short ? ` · ${data.live.dropped_short} too short to keep` : ''}
            {data.live.truncated ? ` · ${data.live.truncated} cut at the length cap` : ''}
            <br/>
            Nothing appearing? Speak closer to the device — a clip starts when the
            room gets ~12dB louder than its own floor.
          </div>
        )}
        {error && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--error)', marginTop:10 }}>{error}</div>
        )}
      </Panel>

      <Panel label={`Clips (${data ? data.count : '…'}${data ? ` of ${data.keep} kept` : ''})`}>
        <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap', marginBottom:12 }}>
          <Pill small disabled={!clips.length || zipping} onClick={doZip}>
            {zipping ? 'Building…' : 'Download all (.zip)'}
          </Pill>
          <Pill small onClick={load}>Refresh</Pill>
          <span style={{ flex:1 }}/>
          {isAdmin && !confirmWipe && (
            <Pill small danger disabled={!clips.length} onClick={() => setConfirmWipe(true)}>Delete all</Pill>
          )}
          {isAdmin && confirmWipe && (
            <>
              <span style={{ fontFamily:MONO, fontSize:9, color:'var(--error)' }}>Delete {clips.length} clip(s)?</span>
              <Pill small danger onClick={doWipe}>Confirm</Pill>
              <Pill small onClick={() => setConfirmWipe(false)}>Cancel</Pill>
            </>
          )}
        </div>

        {!clips.length && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
            {on ? 'Listening — say the wake word.' : 'Nothing collected yet.'}
          </div>
        )}

        {clips.length > 0 && <AfeRecordingNote device={device}/>}
        <div style={{ display:'flex', flexDirection:'column' }}>
          {clips.map(c => (
            <RecordingRow key={c.name} entry={c} chartUrl={`${clipPath(c.name)}/afe`}
              chartOpen={chartOf === c.name} onChart={() => setChartOf(o => (o === c.name ? null : c.name))}
              playing={playing === c.name} elapsed={player.elapsed}
              info={<span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', minWidth:60 }}>
                {(c.ms / 1000).toFixed(1)}s
              </span>}>
              <Pill small onClick={() => player.toggle(c.name, clipPath(c.name))}>{playing === c.name ? '■ Stop' : '▶ Play'}</Pill>
              <Pill small onClick={async () => {
                try { downloadUrl(await player.url(c.name, clipPath(c.name)), `${slug}-${c.name}`); } catch {}
              }}>Download</Pill>
              {isAdmin && <Pill small danger onClick={() => doDelete(c.name)}>Delete</Pill>}
            </RecordingRow>
          ))}
        </div>
      </Panel>

      <AmbientPanel device={device} isAdmin={isAdmin}/>
    </div>
  );
}


// ─── Ambient recording ────────────────────────────────────────────────────────
//
// The other half of a training set (see em_ambient.py): the mic simply held
// open, and one WAV of the whole session when it is switched off. Room noise
// has no onsets to cut on, so nothing is segmented — which makes the elapsed
// time the only feedback this mode can give while it runs, and the reason it
// is on screen rather than implied. Suspends the assistant exactly as sample
// collection does, and is mutually exclusive with it.

function AmbientPanel({ device, isAdmin }) {
  const [data, setData]   = useState(null);
  const [busy, setBusy]   = useState(false);
  const [error, setError] = useState('');
  const [confirmWipe, setConfirmWipe] = useState(false);
  const [chartOf, setChartOf] = useState(null);   // recording whose chart is open

  const id        = device.device_id;
  const on        = !!device.ambientMode;
  const collecting = !!device.collectMode;
  const slug      = fileSlug(device.label, id);
  const filePath  = name => `/api/devices/${id}/ambient/${name}`;
  // These are tens of megabytes each, so unlike a sample clip the blob is
  // NOT cached: holding two of them pinned is a browser tab using 100MB to
  // remember audio nobody is listening to any more. Each lives only while it
  // is the one playing.
  const player  = useClipPlayer(id, { cache: false });
  const playing = player.playing;

  const load = async () => {
    try {
      setData(await API.get(`/api/devices/${id}/ambient`));
      setError('');
    } catch (e) {
      setError(e.error || e.message || 'Could not read recordings');
    }
  };

  // Poll while recording: the open file's length is the only thing that
  // moves, and it is exactly what tells a working mode from a stalled one.
  useEffect(() => {
    load();
    if (!on) return;
    const iv = setInterval(load, 3000);
    return () => clearInterval(iv);
  }, [id, on]);

  async function setMode(enabled) {
    setBusy(true); setError('');
    try {
      await API.post(`/api/devices/${id}/ambient`, { enabled });
      // device.ambientMode arrives on the events socket; reload so the
      // finished file appears the moment it is switched off — that file is
      // the entire point of the mode.
      await load();
    } catch (e) {
      setError(e.error || e.message || 'Could not change the mode');
    }
    setBusy(false);
  }

  async function doDelete(name) {
    player.stop();
    try {
      await API.del(`/api/devices/${id}/ambient/${name}`);
      await load();
    } catch (e) {
      setError(e.error || e.message || 'Could not delete that recording');
    }
  }

  async function doWipe() {
    player.stop();
    setConfirmWipe(false);
    try {
      await API.del(`/api/devices/${id}/ambient`);
      await load();
    } catch (e) {
      setError(e.error || e.message || 'Could not delete the recordings');
    }
  }

  const clock = ms => {
    const s = Math.max(0, Math.round((ms || 0) / 1000));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  };

  const files   = data?.clips || [];
  const totalMb = data ? (data.bytes / 1048576).toFixed(1) : '—';
  // The live length comes from the poll rather than the events socket: state
  // pushes only happen on a mode change or a rolled file, so between them
  // the socket's ambientMs is the value it had minutes ago.
  const liveMs  = data?.live ? data.live.ms : (device.ambientMs || 0);
  const capMin  = data ? Math.round(data.maxMs / 60000) : 30;

  return (
    <>
      <Panel label="Ambient recording">
        <div style={{ fontFamily:SANS, fontSize:12, color:'var(--text2)', lineHeight:1.55, marginBottom:14 }}>
          Holds this device&apos;s microphone open and keeps everything it
          hears. Stopping writes one WAV covering the whole session — the room
          as it actually sounds, which is the negative material a wake model is
          trained against and what a threshold is tuned on. Nothing is cut into
          clips: room noise has no pauses to cut at.
        </div>
        <div style={{ background:'linear-gradient(160deg,var(--lcd-face),var(--lcd-deep))', border:'1px solid var(--lcd-line)', borderRadius:6, padding:'10px 12px', marginBottom:16 }}>
          <span style={{ fontFamily:MONO, fontSize:10, color:'var(--lcd-amber)', lineHeight:1.6 }}>
            While recording, this device answers nothing — no wake word, no
            button turn, and nothing reaches Home Assistant. Its ring throbs
            magenta so it is obvious from the room. Recordings roll into a new
            file every {capMin} minutes.
          </span>
        </div>

        <div style={{ display:'flex', gap:16, alignItems:'flex-end', flexWrap:'wrap', marginBottom:16 }}>
          <Lcd label="Mode"   value={on ? 'RECORDING' : 'OFF'} color={on ? 'var(--lcd-amber)' : 'var(--lcd-dim)'}/>
          <Lcd label="Elapsed" value={on ? clock(liveMs) : '—'} color="var(--lcd-green)"/>
          <Lcd label="Files"  value={data ? String(data.count) : '—'} color="var(--lcd-dim)"/>
          <Lcd label="On disk" value={data ? `${totalMb} MB` : '—'} color="var(--lcd-dim)"/>
        </div>

        <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap' }}>
          <Pill big accent={!on} danger={on}
                disabled={!isAdmin || busy || (!on && collecting)}
                onClick={() => setMode(!on)}>
            {busy ? '…' : on ? 'Stop and save' : 'Start recording'}
          </Pill>
          {!on && collecting && (
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)' }}>
              Stop sample collection first — both modes want the same audio
            </span>
          )}
          {!device.connected && (
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)' }}>
              {on ? 'Armed — a new file starts when the device reconnects'
                  : 'Device offline — this will take effect on its next connect'}
            </span>
          )}
          {device.connected && on && (
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
              writing now — the file appears here when you stop
              {data?.session ? ` · ${data.session} already saved this session` : ''}
            </span>
          )}
        </div>
        {error && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--error)', marginTop:10 }}>{error}</div>
        )}
      </Panel>

      <Panel label={`Recordings (${data ? data.count : '…'}${data ? ` of ${data.keep} kept` : ''})`}>
        <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap', marginBottom:12 }}>
          <Pill small onClick={load}>Refresh</Pill>
          <span style={{ flex:1 }}/>
          {isAdmin && !confirmWipe && (
            <Pill small danger disabled={!files.length} onClick={() => setConfirmWipe(true)}>Delete all</Pill>
          )}
          {isAdmin && confirmWipe && (
            <>
              <span style={{ fontFamily:MONO, fontSize:9, color:'var(--error)' }}>Delete {files.length} recording(s)?</span>
              <Pill small danger onClick={doWipe}>Confirm</Pill>
              <Pill small onClick={() => setConfirmWipe(false)}>Cancel</Pill>
            </>
          )}
        </div>

        {!files.length && (
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
            {on ? 'Recording — stop to save the file.' : 'Nothing recorded yet.'}
          </div>
        )}

        {files.length > 0 && <AfeRecordingNote device={device}/>}
        <div style={{ display:'flex', flexDirection:'column' }}>
          {files.map(f => (
            <RecordingRow key={f.name} entry={f} chartUrl={`${filePath(f.name)}/afe`}
              chartOpen={chartOf === f.name} onChart={() => setChartOf(o => (o === f.name ? null : f.name))}
              playing={playing === f.name} elapsed={player.elapsed}
              info={<>
                <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', minWidth:60 }}>
                  {clock(f.ms)}
                </span>
                <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', minWidth:70 }}>
                  {(f.bytes / 1048576).toFixed(1)} MB
                </span>
              </>}>
              <Pill small onClick={() => player.toggle(f.name, filePath(f.name))}>{playing === f.name ? '■ Stop' : '▶ Play'}</Pill>
              <Pill small onClick={async () => {
                try { downloadBlob(await API.blob(filePath(f.name)), `${slug}-ambient-${f.name}`); } catch {}
              }}>Download</Pill>
              {isAdmin && <Pill small danger onClick={() => doDelete(f.name)}>Delete</Pill>}
            </RecordingRow>
          ))}
        </div>
      </Panel>
    </>
  );
}

// Device wake telemetry is the last `wake.stats` report (§11.1). A missing
// measurement stays unavailable rather than looking like a healthy zero.
function WakeHealth({ device, registry, expected }) {
  const w = device.wake_stats;
  const gap = capabilityGap(liveCapabilities(device), CAPABILITY.DEVICE_WAKE);
  const unavailable = gap || w?.wake_unavailable;
  const reason = gap || (w?.wake_unavailable
    ? `${w.wake_unavailable.replace(/_/g, ' ')}${w.detail ? ` — ${w.detail}` : ''}`
    : w ? 'ready' : 'waiting for the first wake report');
  // Native AFE decoder health (wake.stats.afe): unavailable with the reason on
  // firmware without afe_metadata_v1; absent stats are no data, never zeros.
  const afe = device.afe_stats;
  const afeGap = capabilityGap(liveCapabilities(device), CAPABILITY.AFE_METADATA);
  const afeTrouble = afe && (afe.frames < afe.frames_expected || afe.invalid || afe.gaps || afe.lost_frames);
  const afeText = afeGap ? `unavailable — ${afeGap}`
    : afe ? `${afe.frames}/${afe.frames_expected} frames valid · ${afe.invalid} invalid · ${afe.syncs} syncs · ${afe.gaps} gaps · ${afe.lost_frames} lost`
    : 'no data yet';
  const near = w?.near_misses || [];
  const nearPeak = near.reduce((n, x) => Math.max(n, Number(x.peak) || 0), 0);
  return (
    <Panel label={`Wake health — ${wakeModelLabel(registry.models, expected)}`}>
      <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px' }}>
        <div>
          <div style={{ display:'flex', justifyContent:'space-between', padding:'8px 0', borderBottom:'1px solid var(--hairline)' }}>
            <span style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)' }}>Availability</span>
            <span style={{ fontFamily:MONO, fontSize:11, color:unavailable ? 'var(--warn)' : 'var(--ok)' }}>{reason}</span>
          </div>
          <div style={{ display:'flex', justifyContent:'space-between', padding:'8px 0', borderBottom:'1px solid var(--hairline)' }}>
            <span style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)' }}>Installed graph</span>
            <span style={{ fontFamily:MONO, fontSize:11, color:w?.graph_sha256 && expected && w.graph_sha256 !== expected ? 'var(--warn)' : 'var(--text2)' }}>
              {shortSha(w?.graph_sha256)}
            </span>
          </div>
          <div style={{ display:'flex', justifyContent:'space-between', gap:12, padding:'8px 0', borderBottom:'1px solid var(--hairline)' }}>
            <span style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)', whiteSpace:'nowrap' }}>Native AFE</span>
            <span style={{ fontFamily:MONO, fontSize:11, textAlign:'right',
              color:afeGap || !afe ? 'var(--muted)' : afeTrouble ? 'var(--warn)' : 'var(--text2)' }}>
              {afeText}
            </span>
          </div>
          <div style={{ display:'flex', justifyContent:'space-between', padding:'8px 0' }}>
            <span style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)' }}>Last report</span>
            <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)' }}>
              {w?.received_ms ? relTime(w.received_ms / 1000) : '—'}
            </span>
          </div>
        </div>
        <div style={{ display:'flex', gap:10, flexWrap:'wrap' }}>
          <Lcd label="Max inference" value={w?.infer_max_ms != null ? w.infer_max_ms.toFixed(1) : '—'} color="var(--lcd-dim)" size={14}/>
          <Lcd label="Overruns" value={w?.hops_dropped ?? '—'} color={(w?.hops_dropped || 0) > 0 ? 'var(--lcd-amber)' : 'var(--lcd-dim)'} size={14}/>
          <Lcd label="Near misses" value={w ? near.length : '—'} color={near.length ? 'var(--lcd-amber)' : 'var(--lcd-dim)'} size={14}/>
          <Lcd label="Near-miss peak" value={w ? nearPeak.toFixed(3) : '—'} color="var(--lcd-dim)" size={14}/>
          <Lcd label="Candidates" value={w?.candidates_opened ?? '—'} color="var(--lcd-dim)" size={14}/>
          <Lcd label="Inference errors" value={w?.inference_errors ?? '—'} color={(w?.inference_errors || 0) > 0 ? 'var(--lcd-amber)' : 'var(--lcd-dim)'} size={14}/>
        </div>
      </div>
      <WakeShadowTable device={device}/>
    </Panel>
  );
}

// wake_shadow_events.kind — how a shadow episode no live candidate overlapped
// was resolved (§5.2).
const SHADOW_EVENT = Object.freeze({ UNMATCHED: 'unmatched', RETRIED: 'retried' });
const SHADOW_EVENT_LABEL = Object.freeze({
  [SHADOW_EVENT.UNMATCHED]: 'would-be false wake',
  [SHADOW_EVENT.RETRIED]:   'likely rescued miss',
});

// A canonical rule key ("idle:2:mean:0.90") back into a rule.
function wakeRuleFromKey(key) {
  const [profile, windows, combine, threshold] = String(key || '').split(':');
  return combine ? { profile, windows: Number(windows), combine, threshold: Number(threshold) } : null;
}

const SHADOW_COLUMNS = 'minmax(170px,2.2fr) minmax(48px,0.6fr) minmax(110px,1.3fr) minmax(150px,1.7fr) minmax(70px,0.8fr) minmax(70px,0.8fr)';

// What each shadow rule would have done over the last 7 days, from the
// per-hour counters the Dot reports in wake.stats.shadow (§5.2, §5.4). A rule
// with no evaluated hours has measured nothing, so every figure is "—".
function WakeShadowTable({ device }) {
  const [shadow, setShadow] = useState(null);
  useEffect(() => {
    let live = true;
    const load = () => API.get(`/api/devices/${device.device_id}/wake_shadow?days=7`)
      .then(r => { if (live) setShadow(r); })
      .catch(() => {});
    load();
    const iv = setInterval(load, 60000);
    return () => { live = false; clearInterval(iv); };
  }, [device.device_id]);
  const gap = capabilityGap(liveCapabilities(device), CAPABILITY.OPEN_RULES);
  const rules = shadow?.rules || [];
  const events = shadow?.events || [];
  const head = { fontFamily:MONO, fontSize:9, color:'var(--muted)', textTransform:'uppercase', letterSpacing:'0.08em', padding:'0 0 6px' };
  const cell = { fontFamily:MONO, fontSize:10, color:'var(--text2)', padding:'6px 0', borderTop:'1px solid var(--hairline)' };
  const rate = v => (v != null ? `${v.toFixed(2)}/h` : '—');
  return (
    <div style={{ marginTop:16 }}>
      <SectionLabel>Shadow rules (7 days)</SectionLabel>
      {gap && <div style={{ fontFamily:MONO, fontSize:9, color:'var(--warn)', marginBottom:8 }}>No new shadow data: {gap}</div>}
      {!shadow ? (
        <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>Loading…</div>
      ) : !rules.length ? (
        <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>No shadow rules configured and none reported in the last 7 days.</div>
      ) : (
        <div style={{ overflowX:'auto' }}>
          <div style={{ display:'grid', gridTemplateColumns:SHADOW_COLUMNS, columnGap:12, minWidth:680 }}>
            <span style={head}>Rule</span>
            <span style={head} title="hours of scored audio in this rule's profile">Hours</span>
            <span style={head} title="real wakes this rule would have opened earlier / real wakes it also opened · mean lead">Opened earlier</span>
            <span style={head} title="episodes no real wake followed: count · per hour (95% upper bound)">Would-be false wakes</span>
            <span style={head} title="episodes a live wake followed within 10 s — likely misses the user repeated">Rescued misses</span>
            <span style={head} title="real wakes this rule would have missed">Missed wakes</span>
            {rules.map(r => {
              const measured = r.hours > 0;
              const dim = r.active ? {} : { opacity:0.45 };
              const n = v => (measured && v != null ? String(v) : '—');
              return (
                <React.Fragment key={r.key}>
                  <span style={{ ...cell, ...dim }} title={r.active ? r.key : `${r.key} — no longer configured`}>
                    {wakeRuleText(r.rule || wakeRuleFromKey(r.key))}{r.active ? '' : ' · inactive'}
                  </span>
                  <span style={{ ...cell, ...dim }}>{measured ? r.hours.toFixed(r.hours < 10 ? 1 : 0) : '—'}</span>
                  <span style={{ ...cell, ...dim }}>
                    {measured ? `${r.matched_earlier} / ${r.matched}${r.mean_lead_ms != null ? ` · ${Math.round(r.mean_lead_ms)} ms` : ''}` : '—'}
                  </span>
                  <span style={{ ...cell, ...dim, color:measured && r.unmatched > 0 ? 'var(--warn)' : cell.color }}>
                    {measured ? `${r.unmatched} · ${rate(r.unmatched_per_hour)} (≤ ${rate(r.unmatched_per_hour_upper95)})` : '—'}
                  </span>
                  <span style={{ ...cell, ...dim }}>{n(r.retried)}</span>
                  <span style={{ ...cell, ...dim, color:measured && r.live_only > 0 ? 'var(--warn)' : cell.color }}>{n(r.live_only)}</span>
                </React.Fragment>
              );
            })}
          </div>
        </div>
      )}
      {events.length > 0 && (
        <div style={{ marginTop:14 }}>
          <div style={head}>Recent shadow events</div>
          {events.map((e, i) => (
            <div key={`${e.ts}-${e.rule_key}-${i}`} style={{ display:'flex', gap:12, padding:'5px 0', borderTop:'1px solid var(--hairline)', fontFamily:MONO, fontSize:10 }}
              title={Array.isArray(e.raws) ? `last scores ${e.raws.map(x => Number(x).toFixed(3)).join(', ')}` : undefined}>
              <span style={{ color:'var(--muted)', minWidth:150 }}>{e.ts != null ? new Date(e.ts * 1000).toLocaleString() : '—'}</span>
              <span style={{ color:'var(--text2)', flex:1, minWidth:0 }}>{wakeRuleText(wakeRuleFromKey(e.rule_key))}</span>
              <span style={{ color:e.kind === SHADOW_EVENT.UNMATCHED ? 'var(--warn)' : 'var(--text2)', minWidth:130 }}>{SHADOW_EVENT_LABEL[e.kind] || e.kind}</span>
              <span style={{ color:'var(--text2)', minWidth:70, textAlign:'right' }}>peak {e.peak_raw != null ? e.peak_raw.toFixed(3) : '—'}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// Percentiles of the device's response latency (RESPONSE_LATENCY_TITLE) per
// window, from GET /api/devices/{id}/response_latency. Turns that played no
// reply, or lacked the Echo's timing, are not in them: a window without any
// reads "—", never zero. Refetched as each of this device's turns lands.
const LATENCY_WINDOW_LABEL = Object.freeze({ 24: 'Last 24 hours', 168: 'Last 7 days', 720: 'Last 30 days' });
const LATENCY_PERCENTILES = Object.freeze(['p50', 'p90', 'p95', 'p99']);

function ResponseLatency({ device }) {
  const [stats, setStats] = useState(null);
  useEffect(() => {
    let live = true;
    const load = () => API.get(`/api/devices/${device.device_id}/response_latency`)
      .then(r => { if (live) setStats(r); })
      .catch(() => {});
    load();
    const iv = setInterval(load, 60000);     // windows also slide with no new turn
    const unsubscribe = subscribeEvents(msg => {
      if (msg.type === EVENT_TYPE.TURN_COMPLETE && msg.device_id === device.device_id) load();
    });
    return () => { live = false; clearInterval(iv); unsubscribe(); };
  }, [device.device_id]);
  const head = { fontFamily:MONO, fontSize:9, color:'var(--muted)', textTransform:'uppercase', letterSpacing:'0.08em', padding:'0 0 6px' };
  const cell = { fontFamily:MONO, fontSize:11, color:'var(--text2)', padding:'6px 0', borderTop:'1px solid var(--hairline)' };
  const sec = ms => (ms != null ? `${(ms / 1000).toFixed(2)} s` : '—');
  return (
    <Panel label="Response latency">
      <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginBottom:10 }}>
        End of your last word → first audio of the reply, timed on the Echo · turns that played no reply are not counted
      </div>
      {!stats ? (
        <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>Loading…</div>
      ) : (
        <div style={{ display:'grid', gridTemplateColumns:'minmax(110px,1.6fr) repeat(5,minmax(48px,1fr))', columnGap:12 }}>
          <span style={head}>Window</span>
          <span style={head} title="turns that measured a response latency">Turns</span>
          {LATENCY_PERCENTILES.map(p => <span key={p} style={head}>{p}</span>)}
          {stats.windows.map(w => (
            <React.Fragment key={w.hours}>
              <span style={cell}>{LATENCY_WINDOW_LABEL[w.hours] || `Last ${w.hours} hours`}</span>
              <span style={cell}>{w.turns}</span>
              {LATENCY_PERCENTILES.map(p => (
                <span key={p} style={{ ...cell, color:w[p] != null ? 'var(--text)' : 'var(--muted)' }}>{sec(w[p])}</span>
              ))}
            </React.Fragment>
          ))}
        </div>
      )}
    </Panel>
  );
}

function formatAlertTime(value) {
  if (!value) return '—';
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? value : d.toLocaleString([], {
    weekday:'short', month:'short', day:'numeric', hour:'2-digit', minute:'2-digit'
  });
}

// Timers are display copies; alarms and every dashboard operation go through
// AlertEngine (§10.5). The panel never manufactures a ring from a countdown.
function AlertPanel({ device, isAdmin }) {
  const [payload, setPayload] = useState(null);
  const [calendarUrl, setCalendarUrl] = useState(null);
  const [loading, setLoading] = useState(true);
  const [name, setName] = useState('');
  const [time, setTime] = useState('07:00');
  const [date, setDate] = useState('');
  const [days, setDays] = useState([]);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState(null);

  const load = useCallback(async () => {
    try {
      const [a, h] = await Promise.all([
        API.get(`/api/devices/${device.device_id}/alerts`),
        API.get('/api/ha/status').catch(() => null),
      ]);
      setPayload(a);
      setCalendarUrl(h?.calendar_url || null);
    } catch (e) {
      setResult({ ok:false, text:e.error || 'Alert status is unavailable' });
    }
    setLoading(false);
  }, [device.device_id]);

  // The engine republishes its timer display copies once a second (§10.8), so
  // those events update the list in place; every other alert event refetches.
  useEffect(() => {
    load();
    return subscribeEvents(msg => {
      if (msg.type === EVENT_TYPE.ALERTS && msg.device_id === device.device_id) {
        if (msg.kind === ALERT_NOTICE.TIMERS && Array.isArray(msg.data?.timers)) {
          setPayload(p => p?.status ? { ...p, status:{ ...p.status, timers:msg.data.timers } } : p);
        } else {
          load();
        }
      }
      if (msg.type === EVENT_TYPE.HA_STATUS) setPayload(p => p ? { ...p, ha:msg.features } : p);
    });
  }, [load, device.device_id]);

  const status = payload?.status;
  const timers = status?.timers || [];
  const occurrences = status?.occurrences || [];
  const pending = (status?.operations || []).filter(x => x.state === ALERT_OP_STATE.PENDING);
  const recent = (status?.operations || []).filter(x => x.state !== ALERT_OP_STATE.PENDING).slice(0, 8);
  const alertState = status?.alert_state;
  const active = alertState?.active;
  const features = payload?.ha || {};

  async function createAlarm() {
    setBusy(true); setResult(null);
    try {
      const r = await API.post(`/api/devices/${device.device_id}/alarms`, {
        time, days, name:name.trim(), date:date || null,
      });
      setResult({ ok:!!r.ok, text:r.ok
        ? (r.delivery_pending ? 'Stored in Home Assistant; delivery to the Echo is pending.'
          : r.armed_on_endpoint ? 'Stored in Home Assistant and armed on the Echo.'
          : r.pending ? 'Home Assistant is still confirming this alarm.' : 'Alarm saved.')
        : (r.error || 'Alarm was not saved') });
      if (r.ok) { setName(''); setDate(''); await load(); }
    } catch (e) { setResult({ ok:false, text:e.error || 'Alarm was not saved' }); }
    setBusy(false);
  }

  async function cancelAlarm(o) {
    // The engine cancels whole schedules matched by name and time (§10.3).
    if (!confirm(`Cancel "${o.label}"? A repeating alarm is removed with every future occurrence.`)) return;
    setBusy(true); setResult(null);
    const matchTime = String(o.due_local || '').match(/T(\d\d:\d\d(?::\d\d)?)/)?.[1] || '';
    try {
      const r = await API.post(`/api/devices/${device.device_id}/alarms/cancel`, {
        name:o.label, time:matchTime, all:false,
      });
      setResult({ ok:!!r.ok, text:r.ok
        ? (r.delivery_pending ? 'Removed from Home Assistant; cancellation delivery is pending.' : 'Alarm cancelled.')
        : (r.error || 'Alarm was not cancelled') });
      await load();
    } catch (e) { setResult({ ok:false, text:e.error || 'Alarm was not cancelled' }); }
    setBusy(false);
  }

  const featureEntries = Object.entries(features);
  return (
    <div style={{ minHeight:'100%', display:'flex', flexDirection:'column', gap:16 }}>
      <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:16 }}>
        <Panel label="Now">
          {active ? (
            <div>
              <div style={{ fontFamily:MONO, fontSize:13, color:'var(--warn)', marginBottom:6 }}>
                {active.kind === RING_KIND.TIMER ? 'Timer' : 'Alarm'} ringing · {active.name || active.id}
              </div>
              <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
                {active.foreground ? 'foreground' : 'background while a turn is open'}
              </div>
            </div>
          ) : <div style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)' }}>Nothing is ringing.</div>}
          <div style={{ marginTop:14 }}>
            {timers.length === 0 ? (
              <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
                No active Home Assistant timers in this controller&apos;s display copy.
              </div>
            ) : timers.map(t => {
              const left = Math.max(0, t.remaining_seconds || 0);
              return <div key={t.id} style={{ display:'flex', justifyContent:'space-between', padding:'7px 0', borderTop:'1px solid var(--hairline)' }}>
                <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)' }}>{t.name || 'Timer'}</span>
                <span style={{ fontFamily:MONO, fontSize:11, color:'var(--lcd-green)' }}>
                  {Math.floor(left/60)}:{String(left%60).padStart(2,'0')}{t.active ? '' : ' · paused'}
                </span>
              </div>;
            })}
          </div>
        </Panel>
        <Panel label="Delivery health">
          <div style={{ fontFamily:MONO, fontSize:10, lineHeight:1.8, color:'var(--text2)' }}>
            <div>Calendar subscription {status ? <span style={{ color:status.calendar_live ? 'var(--ok)' : 'var(--warn)' }}>{status.calendar_live ? 'live' : 'unavailable'}</span> : '—'}</div>
            <div>Endpoint {status ? <span style={{ color:status.online ? 'var(--ok)' : 'var(--warn)' }}>{status.online ? 'online' : 'offline'}</span> : '—'}</div>
            <div>Delivery sequence {status ? `${status.acked_sequence} / ${status.sequence}` : '—'}</div>
            <div>Clock <span style={{ color:alertState?.clock_trusted ? 'var(--ok)' : 'var(--warn)' }}>{alertState ? (alertState.clock_trusted ? 'trusted' : 'untrusted') : '—'}</span></div>
            <div>Wakelock <span style={{ color:alertState?.wakelock === WAKELOCK.HELD || alertState?.wakelock === WAKELOCK.RELEASED ? 'var(--ok)' : 'var(--warn)' }}>{alertState?.wakelock || '—'}</span></div>
            <div>Alert store <span style={{ color:alertState?.store === ALERT_STORE.OK ? 'var(--ok)' : 'var(--warn)' }}>{alertState?.store || '—'}</span></div>
          </div>
          {(status?.flags || []).map(flag => (
            <div key={flag} style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>{flag.replace(/_/g,' ')}</div>
          ))}
        </Panel>
      </div>

      <Panel label="Alarm schedules and occurrences">
        {loading ? <div style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)' }}>Loading…</div>
        : !status ? <div style={{ fontFamily:MONO, fontSize:11, color:'var(--warn)' }}>This endpoint&apos;s alarm calendar is unavailable.</div>
        : occurrences.length === 0 ? <div style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)' }}>No upcoming alarms.</div>
        : occurrences.map(o => (
          <div key={o.occurrence_id} style={{ display:'flex', alignItems:'center', gap:12, padding:'9px 0', borderBottom:'1px solid var(--hairline)' }}>
            <div style={{ flex:1, minWidth:0 }}>
              <div style={{ fontFamily:SANS, fontSize:12, fontWeight:600, color:'var(--text2)' }}>{o.label}</div>
              <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:2 }}>
                {formatAlertTime(o.due_local)} · schedule {shortSha(o.schedule_id)}
              </div>
            </div>
            <span style={{ fontFamily:MONO, fontSize:9, color:o.armed_on_endpoint ? 'var(--ok)' : 'var(--warn)' }}>
              {o.armed_on_endpoint ? 'armed on endpoint' : o.delivery_pending ? 'delivery pending' : o.stored_in_ha ? 'stored in HA' : 'unconfirmed'}
            </span>
            {isAdmin && <Pill small danger disabled={busy} onClick={() => cancelAlarm(o)}>Cancel</Pill>}
          </div>
        ))}
        {isAdmin && (
          <div style={{ marginTop:18, paddingTop:14, borderTop:'1px solid var(--hairline)' }}>
            <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:12 }}>
              <label style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)' }}>Time
                <input type="time" value={time} onChange={e => setTime(e.target.value)} style={{ display:'block', width:'100%', marginTop:5, boxSizing:'border-box' }}/>
              </label>
              <label style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)' }}>Name
                <input type="text" value={name} maxLength={120} placeholder="Alarm" onChange={e => setName(e.target.value)} style={{ display:'block', width:'100%', marginTop:5, boxSizing:'border-box' }}/>
              </label>
            </div>
            <div style={{ display:'flex', alignItems:'center', gap:8, flexWrap:'wrap', marginTop:12 }}>
              {Object.values(WEEKDAY).map(d => (
                <button type="button" key={d} className={'em-pill em-pill--small' + (days.includes(d) ? ' em-pill--accent' : '')}
                  aria-pressed={days.includes(d)}
                  onClick={() => { setDate(''); setDays(xs => xs.includes(d) ? xs.filter(x => x !== d) : [...xs,d]); }}>
                  {d}
                </button>
              ))}
              <label style={{ marginLeft:'auto', fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>
                one date <input type="date" value={date} disabled={days.length > 0} onChange={e => { setDays([]); setDate(e.target.value); }} style={{ marginLeft:6 }}/>
              </label>
            </div>
            <div style={{ display:'flex', alignItems:'center', gap:10, marginTop:14 }}>
              <Pill accent disabled={busy || !time} onClick={createAlarm}>{busy ? 'Working…' : 'Create alarm'}</Pill>
              <span style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>
                {days.length ? `Repeats ${days.join(', ')}` : date ? `One time on ${date}` : 'One time at the next occurrence'}
              </span>
            </div>
          </div>
        )}
        {result && <div style={{ marginTop:12, fontFamily:MONO, fontSize:10, color:result.ok ? 'var(--ok)' : 'var(--error)' }}>{result.text}</div>}
      </Panel>

      <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:16 }}>
        <Panel label="Home Assistant provisioning">
          {featureEntries.length === 0 ? <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>No provisioning report.</div>
          : featureEntries.map(([key, value]) => (
            <div key={key} style={{ display:'flex', justifyContent:'space-between', gap:12, padding:'6px 0', borderBottom:'1px solid var(--hairline)' }}>
              <span style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)' }}>{key.replace(/_/g,' ')}</span>
              <span style={{ fontFamily:MONO, fontSize:9, color:value?.ok ? 'var(--ok)' : 'var(--warn)', textAlign:'right' }}>
                {value?.ok ? 'ready' : value?.detail || 'unavailable'}
              </span>
            </div>
          ))}
          {calendarUrl && <a href={calendarUrl} target="_blank" rel="noreferrer" style={{ display:'inline-block', marginTop:12, fontFamily:MONO, fontSize:10, color:'var(--accent)' }}>Open Home Assistant calendar editor →</a>}
        </Panel>
        <Panel label={`Journal operations · ${pending.length} pending`}>
          {[...pending, ...recent].length === 0 ? <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>No operations in the last 24 hours.</div>
          : [...pending, ...recent].map(op => (
            <div key={op.op_id} style={{ padding:'6px 0', borderBottom:'1px solid var(--hairline)' }}>
              <div style={{ display:'flex', justifyContent:'space-between', gap:10 }}>
                <span style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)' }}>{op.action.replace(/_/g,' ')}</span>
                <span style={{ fontFamily:MONO, fontSize:9, color:op.state === ALERT_OP_STATE.PENDING ? 'var(--warn)' : op.state === ALERT_OP_STATE.APPLIED ? 'var(--ok)' : 'var(--error)' }}>{op.state} · step {op.step}</span>
              </div>
              <div style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)', marginTop:2 }}>{shortSha(op.op_id)} · {op.source} · {new Date(op.created_ms).toLocaleTimeString()}</div>
            </div>
          ))}
        </Panel>
      </div>
    </div>
  );
}

// ─── Bundled firmware: behind, and the queued install ────────────────────────

// The update-available rule: the version a device last reported — kept while
// it is offline — differs from the bundled one. `firmware` is the binary
// bundled with this controller, the only one it installs, and is null until
// /api/firmware answers.
function firmwareBehind(d, firmware) {
  return !!(firmware && d.firmware_ver && d.firmware_ver !== firmware.version);
}

// The Kiosk Satellite password prompt (docs/echo-show-companion-plan.md
// decision 4). POST /api/devices/{id}/kiosk_login logs in at the Echo Show's
// own Kiosk Satellite and keeps only the token it issues, which the firmware
// installs the turn-view plugin with. Always skippable: the light bar works
// without the plugin. `reason` says why it is asked, when it was not asked for.
function KioskLogin({ deviceId, reason, onDone, onSkip, skipLabel = 'Skip' }) {
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  async function login() {
    if (!password || busy) return;
    setBusy(true); setError('');
    try {
      await API.post(`/api/devices/${deviceId}/kiosk_login`, { password });
    } catch (e) {
      setError(e.error || 'Login failed');
      setBusy(false);
      return;
    }
    setPassword(''); setBusy(false);
    onDone();
  }

  return (
    <div style={{ display:'flex', flexDirection:'column', gap:8, padding:12, marginTop:8,
                  border:'1px solid var(--border)', borderRadius:8, background:'var(--card)' }}>
      {reason && <div style={{ fontFamily:MONO, fontSize:10, color:'var(--warn)' }}>{reason}</div>}
      <div style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)', lineHeight:1.6 }}>
        Kiosk Satellite's Remote Administration password lets the firmware install the EchoMuse
        plugin, which shows each request and answer full screen. Remote Administration must be on,
        with a password set (Kiosk Satellite → Settings → Device → Remote Administration). EchoMuse
        logs in once and keeps only the token Kiosk Satellite issues, never the password; changing
        the password later does not revoke that token.
      </div>
      <div style={{ display:'flex', gap:8, alignItems:'center', flexWrap:'wrap' }}>
        <input type="password" value={password} autoComplete="off" placeholder="Remote Administration password"
          aria-label="Kiosk Satellite Remote Administration password" disabled={busy}
          onChange={e => setPassword(e.target.value)} onKeyDown={e => { if (e.key === 'Enter') login(); }}
          style={{ flex:'1 1 220px', minWidth:0 }}/>
        <Pill accent disabled={busy || !password} onClick={login}>{busy ? 'Logging in…' : 'Log in'}</Pill>
        <Pill disabled={busy} onClick={onSkip}>{skipLabel}</Pill>
      </div>
      {error && <div style={{ fontFamily:MONO, fontSize:10, color:'var(--error)' }}>{error}</div>}
    </div>
  );
}

// Runs an update action at once, or after the Kiosk Satellite prompt when
// kioskLoginReason says one is due (decision 4: before a firmware update or a
// queued one). Returns [gate(action), the prompt or null]; logging in and
// skipping both go on to the action.
function useKioskGate(device) {
  const [held, setHeld] = useState(null);   // {action, reason} waiting on the prompt
  const gate = action => {
    const reason = kioskLoginReason(device);
    if (reason) setHeld({ action, reason });
    else action();
  };
  const release = () => { const h = held; setHeld(null); if (h) h.action(); };
  const prompt = held && (
    <KioskLogin deviceId={device.device_id} onDone={release} onSkip={release} skipLabel="Skip, update anyway"
      reason={`${held.reason} Log in before the update so the new firmware can install its Kiosk Satellite plugin.`}/>
  );
  return [gate, prompt];
}

// The Screen section's Kiosk Satellite token: set or not (never its value)
// and a login at any time (decision 4). Outside the section's scope dimming:
// the token belongs to the device, not to a config scope.
function KioskTokenRow({ deviceId, tokenSet, reason, gap, disabled }) {
  const [open, setOpen] = useState(false);
  return (
    <div style={{ marginTop:12, paddingTop:10, borderTop:'1px solid var(--hairline)' }}>
      <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap' }}>
        <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)' }}>Kiosk Satellite token</span>
        <span style={{ fontFamily:MONO, fontSize:11, color: tokenSet ? 'var(--ok)' : 'var(--muted)' }}>
          {tokenSet ? 'set' : 'not set'}
        </span>
        {!open && (
          <Pill small disabled={disabled || !!gap} onClick={() => setOpen(true)}>
            {tokenSet ? 'Log in again' : 'Log in to Kiosk Satellite'}
          </Pill>
        )}
      </div>
      {gap && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>Kiosk Satellite plugin unavailable: {gap}</div>}
      {!gap && reason && !open && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>{reason}</div>}
      {open && <KioskLogin deviceId={deviceId} onDone={() => setOpen(false)} onSkip={() => setOpen(false)} skipLabel="Cancel"/>}
    </div>
  );
}

// Install-on-reconnect for an approved device that is offline and behind:
// POST/DELETE /api/devices/{id}/update/queue. The controller keeps the request
// across restarts and runs it when the device next connects. It names no
// version — what installs is whatever this controller bundles at that moment.
// `device.update_queued_at` (live over the event socket) is the only state.
function QueuedInstall({ device, firmware, small }) {
  const [busy, setBusy]   = useState(false);
  const [error, setError] = useState('');
  const [kioskGate, kioskPrompt] = useKioskGate(device);
  const queued = device.update_queued_at != null;

  async function toggle() {
    setBusy(true); setError('');
    try {
      if (queued) await API.del(`/api/devices/${device.device_id}/update/queue`);
      else        await API.post(`/api/devices/${device.device_id}/update/queue`);
    } catch (e) { setError(e.error || (queued ? 'Cancel failed' : 'Could not queue the install')); }
    setBusy(false);
  }

  return (
    <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap' }}>
      {queued && (
        <span style={{ fontFamily:MONO, fontSize: small ? 10 : 11, color:'var(--accent)' }}>
          Queued {relTime(device.update_queued_at)} — installs {firmware ? firmware.version : 'the bundled firmware'} on reconnect
        </span>
      )}
      <Pill small={small} accent={!queued && !busy} disabled={busy || !!kioskPrompt}
            onClick={() => queued ? toggle() : kioskGate(toggle)}>
        {busy ? '…' : queued ? 'Cancel queued install' : 'Install when it reconnects'}
      </Pill>
      {error && <span style={{ fontFamily:MONO, fontSize:10, color:'var(--error)' }}>{error}</span>}
      {kioskPrompt && <div style={{ flexBasis:'100%' }}>{kioskPrompt}</div>}
    </div>
  );
}


// ─── Device detail modal ──────────────────────────────────────────────────────

// The device modal's tabs; each value is also its (upper-cased) label.
const DETAIL_TAB = Object.freeze({
  APPROVE: 'approve', STATUS: 'status', ACTIVITY: 'activity', ALERTS: 'alerts',
  SAMPLES: 'samples', CONFIG: 'config', CONSOLE: 'console', UPDATES: 'updates', LOGS: 'logs',
});

function Detail({ device, token, onClose, onApprove, isAdmin, globalConfig, onDeviceConfigChange, firmware }) {
  const [tabChoice, setTab] = useState(DETAIL_TAB.STATUS);
  // Seed from the EFFECTIVE config, not the raw stored one — see
  // effectiveConfig(). A migrated row's stored dict is not the truth.
  const [config, setConfig] = useState(() => effectiveConfig(globalConfig, device));
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  // Which config sections this device overrides (ids from CONFIG_SECTIONS).
  // Empty = follows the fleet entirely, which is what use_global_config
  // meant before per-section scoping.
  const [sections, setSections] = useState(device.config_sections ?? []);
  const [logs, setLogs] = useState([]);
  const [logsLoading, setLogsLoading] = useState(false);
  const [pushLog, setPushLog] = useState([]);
  const [pushing, setPushing] = useState(false);
  const [approveLabel, setApproveLabel] = useState(device.label || '');
  const [approving, setApproving] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(device.label || '');
  const [renameSaving, setRenameSaving] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [securing, setSecuring] = useState(false);
  const [debloating, setDebloating] = useState(false);
  const [turns, setTurns] = useState([]);
  const [nearMisses, setNearMisses] = useState([]);
  const [registry] = useWakeRegistry();
  const state = deviceState(device);
  const needsUpdate = firmwareBehind(device, firmware);
  const updateRunning = pushing || !!device.update_in_progress;
  const [kioskGate, kioskPrompt] = useKioskGate(device);

  // "samples" is admin-only for the reason the endpoint behind it is: the
  // mode suspends the assistant on this device, which is not something a
  // read-only viewer should be able to do to everyone else in the house.
  const T = DETAIL_TAB;
  const TABS = !device.approved ? [T.APPROVE]
    : (isAdmin ? [T.STATUS, T.ACTIVITY, T.ALERTS, T.SAMPLES, T.CONFIG, T.CONSOLE, T.UPDATES, T.LOGS]
               : [T.STATUS, T.ACTIVITY, T.ALERTS, T.CONFIG, T.LOGS]);
  // The tab on screen is always one the bar offers. The chosen one can fall
  // out of the set underneath the modal — a device opened while still pending
  // (whose only tab is Approve) — and rendering it anyway showed a panel with
  // no tab selected above it.
  const tab = TABS.includes(tabChoice) ? tabChoice : TABS[0];

  useEffect(() => {
    if (tab === DETAIL_TAB.LOGS) {
      setLogsLoading(true);
      API.get(`/api/devices/${device.device_id}/logs?limit=50`)
        .then(setLogs).catch(console.error)
        .finally(() => setLogsLoading(false));
    }
  }, [tab, device.device_id]);

  // Turn observability — fetch on Activity tab entry, refresh every 10s while
  // the tab is open (turn history is in-memory on the controller). The near
  // misses listed beside the turns ride the same poll.
  useEffect(() => {
    if (tab !== DETAIL_TAB.ACTIVITY) return;
    let live = true;
    const id = device.device_id;
    const load = () => {
      API.get(`/api/devices/${id}/turns?limit=${ACTIVITY_LIMIT.DEVICE}`)
        .then(t => { if (live) setTurns(Array.isArray(t) ? t : []); })
        .catch(() => {});
      API.get(`/api/devices/${id}/near_misses?limit=${ACTIVITY_LIMIT.DEVICE}`)
        .then(n => { if (live) setNearMisses(Array.isArray(n) ? n : []); })
        .catch(() => {});
    };
    load();
    const iv = setInterval(load, 10000);
    return () => { live = false; clearInterval(iv); };
  }, [tab, device.device_id]);

  function setConf(k, v) { setConfig(c => ({ ...c, [k]: v })); setDirty(true); }

  async function pushConfig() {
    setSaving(true);
    try {
      // Send the scoping plus the full effective config. The controller
      // keeps only the values belonging to overridden sections, so sending
      // everything is safe and keeps the clobber guard satisfied (it sees no
      // in-scope key going missing).
      const body = { config_sections: sections, ...config };
      const res = await API.post(`/api/devices/${device.device_id}/config`, body);
      setDirty(false);
      // Keep parent device list in sync so re-opening the modal is consistent
      if (onDeviceConfigChange) {
        onDeviceConfigChange(device.device_id, {
          config: res.config,
          config_sections: res.config_sections,
          use_global_config: res.use_global_config,
        });
      }
    } catch(e) { alert(e.error || 'Failed to push config'); }
    setSaving(false);
  }

  async function doSecureLink() {
    // Pushes CA + link token over the shell plane, then the controller
    // bounces the connection; the device redials over wss. The "Link" row
    // flips to wss (TLS) on the next device-list refresh after reconnect.
    setSecuring(true);
    try {
      await API.post(`/api/devices/${device.device_id}/secure_link`, {});
    } catch(e) { alert(e.error || 'Secure link failed'); }
    // Leave the button disabled briefly — transfer + reconnect takes ~10s.
    setTimeout(() => setSecuring(false), 15000);
  }

  async function doDebloat() {
    // Syncs start_server.sh and stops its service denylist now. Needed
    // because the OTA-time sync cannot reach a device already running the
    // latest firmware. Idempotent.
    setDebloating(true);
    try {
      await API.post(`/api/devices/${device.device_id}/debloat`, {});
      alert('Debloat started: Amazon\'s services are being stopped now, no reboot needed — '
        + 'watch the device log for the result.');
    } catch(e) { alert(e.error || 'Debloat failed'); }
    setTimeout(() => setDebloating(false), 8000);
  }

  async function doUpdate() {
    setPushing(true);
    setPushLog([firmware ? `Installing bundled EchoMuse firmware ${firmware.version}…` : 'Installing bundled EchoMuse firmware…']);
    try {
      const res = await API.post(`/api/devices/${device.device_id}/update`);
      setPushLog(l => [...l, `Deploying ${res.version} — waiting for reconnect…`]);
      _pollReconnect(res.version);
    } catch(e) {
      setPushLog([`Error: ${e.error || 'Update failed'}`]);
      setPushing(false);
    }
  }

  // One reconnect poll at a time, and none outliving the modal: closing it
  // mid-update used to leave the poll running for its full two minutes.
  const reconnectPoll = useRef(null);
  useEffect(() => () => clearInterval(reconnectPoll.current), []);

  function _pollReconnect(targetVersion) {
    let attempts = 0;
    let wasDisconnected = false;
    clearInterval(reconnectPoll.current);
    const poll = reconnectPoll.current = setInterval(async () => {
      attempts++;
      try {
        const devices = await API.get('/api/devices');
        const d = devices.find(x => x.device_id === device.device_id);
        // Track when the device goes offline during the restart cycle.
        // The rollback check must only fire after observing a disconnect —
        // otherwise it triggers mid-transfer while the device is still
        // connected and running the old firmware.
        if (!d?.connected) wasDisconnected = true;
        if (d?.connected && d?.firmware_ver === targetVersion) {
          setPushLog(l => [...l, `✓ Running ${targetVersion}`, '✓ Update complete']);
          clearInterval(poll); setPushing(false);
        } else if (d?.update_error && !d?.update_in_progress) {
          // Controller recorded a terminal failure (transfer failed, slot
          // detect failed, exception…) — report it now instead of letting
          // the poll run out its 2-minute timeout.
          setPushLog(l => [...l, `✗ ${d.update_error}`]);
          clearInterval(poll); setPushing(false);
        } else if (wasDisconnected && d?.connected && d?.firmware_ver && d.firmware_ver !== targetVersion) {
          setPushLog(l => [...l, `⚠ Device reconnected on ${d.firmware_ver} — auto-rolled back`]);
          clearInterval(poll); setPushing(false);
        } else if (attempts > 40) {
          setPushLog(l => [...l, 'Timed out — check device logs']);
          clearInterval(poll); setPushing(false);
        }
      } catch(e) { clearInterval(poll); setPushing(false); }
    }, 3000);
  }

  async function doRollback() {
    setPushing(true); setPushLog([`Rolling back to ${device.firmware_previous}…`]);
    try {
      await API.post(`/api/devices/${device.device_id}/rollback`, {});
      _pollReconnect(device.firmware_previous);
    } catch(e) {
      setPushLog([`Error: ${e.error || 'Rollback failed'}`]);
      setPushing(false);
    }
  }

  async function doApprove() {
    if (!approveLabel.trim()) { alert('Please enter a label'); return; }
    setApproving(true);
    try {
      await API.post(`/api/devices/${device.device_id}/approve`, { label: approveLabel });
      onApprove();
      onClose();
    } catch(e) { alert(e.error || 'Approval failed'); }
    setApproving(false);
  }

  async function doRename() {
    const trimmed = renameValue.trim();
    if (!trimmed) { alert('Label cannot be empty'); return; }
    if (trimmed === device.label) { setRenaming(false); return; }
    setRenameSaving(true);
    try {
      // PATCH /api/devices/{id} — confirmed against em_api.py: requires
      // {label}, broadcasts a device_update event over /api/events that
      // App's WebSocket listener already applies to live device state,
      // so no manual setDevices() needed here.
      await API.patch(`/api/devices/${device.device_id}`, { label: trimmed });
      setRenaming(false);
    } catch(e) {
      alert(e.error || 'Rename failed');
    }
    setRenameSaving(false);
  }

  async function doDelete() {
    setDeleting(true);
    try {
      // DELETE /api/devices/{id} — confirmed against em_api.py. Broadcasts
      // device_deleted, which App's WebSocket listener already filters out
      // of device state, so closing here is enough — no manual cleanup.
      await API.del(`/api/devices/${device.device_id}`);
      onClose();
    } catch(e) {
      alert(e.error || 'Delete failed');
      setDeleting(false);
      setConfirmDelete(false);
    }
  }

  const row = (k, v, c) => (
    <div style={{ display: 'flex', justifyContent: 'space-between', padding: '8px 0', borderBottom: '1px solid var(--hairline)' }}>
      <span style={{ fontFamily: MONO, fontSize: 12, color: 'var(--muted)' }}>{k}</span>
      <span style={{ fontFamily: MONO, fontSize: 12, color: c || 'var(--text)', fontWeight: 600 }}>{v}</span>
    </div>
  );

  return (
    <ModalFrame onClose={onClose} zIndex={100} label={device.label || device.device_id}>
        {/* Header */}
        <div className="em-modal-head" style={{ background: 'linear-gradient(180deg,var(--card),var(--bg))', borderBottom: '1px solid var(--border-hard)', padding: '20px 24px 0', boxShadow: '0 1px 0 var(--sheen) inset' }}>
          <div className="em-modal-headrow" style={{ display: 'flex', alignItems: 'center', gap: 20, marginBottom: 16 }}>
            <LedRing state={state} size={72}/>
            <div style={{ flex: 1, minWidth: 0 }}>
              {renaming ? (
                <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                  <input
                    type="text" value={renameValue} autoFocus aria-label="Device label"
                    onChange={e => setRenameValue(e.target.value)}
                    onKeyDown={e => {
                      if (e.key === 'Enter') doRename();
                      if (e.key === 'Escape') { setRenaming(false); setRenameValue(device.label || ''); }
                    }}
                    style={{ fontFamily: SANS, fontSize: 20, fontWeight: 600, padding: '4px 8px', maxWidth: 280 }}
                  />
                  <Pill small onClick={doRename} disabled={renameSaving}>{renameSaving ? 'Saving…' : 'Save'}</Pill>
                  <Pill small onClick={() => { setRenaming(false); setRenameValue(device.label || ''); }}>Cancel</Pill>
                </div>
              ) : (
                <div
                  {...(isAdmin ? pressable(() => setRenaming(true)) : {})}
                  title={isAdmin ? 'Click to rename' : undefined}
                  style={{
                    fontFamily: SANS, fontSize: 26, color: 'var(--text)', fontWeight: 600,
                    letterSpacing: '-0.02em', lineHeight: 1, cursor: isAdmin ? 'pointer' : 'default',
                    display: 'inline-block',
                  }}>
                  {device.label || <span style={{ color: 'var(--muted)', fontSize: 20 }}>{device.device_id.slice(0,8)}…</span>}
                </div>
              )}
              <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginTop: 4, letterSpacing: '0.05em' }}>
                {deviceIpText(device, ' (last seen)')} · {device.device_id} · {device.os_version || 'OS unknown'} · EchoMuse {device.firmware_ver || 'unknown'}
                {needsUpdate && <span style={{ color: 'var(--warn)', marginLeft: 10 }}>Update available</span>}
                {device.update_queued_at != null && <span style={{ color: 'var(--accent)', marginLeft: 10 }}>Install queued for reconnect</span>}
              </div>
            </div>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
              <div style={{ background: 'linear-gradient(160deg,var(--lcd-face),var(--lcd-deep))', border: '1px solid var(--lcd-line)', borderRadius: 6, padding: '5px 12px', boxShadow: 'inset 0 1px 3px rgba(0,0,0,0.5)' }}>
                <span style={{ fontFamily: MONO, fontSize: 11, color: state.dot, textShadow: `0 0 8px ${state.dot}88`, letterSpacing: '0.05em' }}>{state.label.toUpperCase()}</span>
              </div>
              {/* A collecting device is silent by design, which from every
                  other panel is indistinguishable from a broken one. Say so
                  next to the state, not only on the tab that caused it. */}
              {device.collectMode && (
                <span className="em-pill em-pill--small em-pill--accent"
                      title="Collecting wake-word samples — voice turns suspended"
                      style={{ display: 'inline-block', pointerEvents: 'none',
                               fontFamily: MONO, letterSpacing: '0.05em' }}>
                  COLLECTING
                </span>
              )}
              {/* And ambient recording, for the same reason again. Its own
                  badge rather than reusing COLLECTING: this one is holding a
                  file open, so "how long has it been on" is the question,
                  and it is the mode most likely to be left running. */}
              {device.ambientMode && (
                <span className="em-pill em-pill--small em-pill--accent"
                      title="Recording ambient audio — voice turns suspended"
                      style={{ display: 'inline-block', pointerEvents: 'none',
                               fontFamily: MONO, letterSpacing: '0.05em' }}>
                  RECORDING
                </span>
              )}
              {/* Same reasoning for capture mode: it suspends voice turns
                  too. Its own badge rather than reusing COLLECTING — the
                  two are cleared by different things, and a capture badge
                  that outlives the script that armed it is the one case
                  someone will need to recognise. */}
              {device.captureMode && (
                <span className="em-pill em-pill--small em-pill--accent"
                      title="Capturing to a webhook — voice turns suspended"
                      style={{ display: 'inline-block', pointerEvents: 'none',
                               fontFamily: MONO, letterSpacing: '0.05em' }}>
                  CAPTURING
                </span>
              )}
              {/* §11.3: microphone audio leaves the device outside a turn only
                  under a diagnostic audio subscription, and the dashboard says so while it
                  is open. */}
              {device.diagnostic && (
                <span className="em-pill em-pill--small em-pill--danger"
                      title="A diagnostic audio subscription is streaming live microphone audio; wakes are refused"
                      style={{ display: 'inline-block', pointerEvents: 'none',
                               fontFamily: MONO, letterSpacing: '0.05em' }}>
                  MIC LIVE
                </span>
              )}
              {isAdmin && !confirmDelete && (
                <CircleButton onClick={() => setConfirmDelete(true)} title="Delete device" color="var(--error)">🗑</CircleButton>
              )}
              {isAdmin && confirmDelete && (
                <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                  <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--error)' }}>Delete?</span>
                  <Pill small danger disabled={deleting} onClick={doDelete}>{deleting ? '…' : 'Confirm'}</Pill>
                  <Pill small onClick={() => setConfirmDelete(false)} disabled={deleting}>Cancel</Pill>
                </div>
              )}
              <CircleButton onClick={onClose} title="Close">×</CircleButton>
            </div>
          </div>
          <TabBar tabs={TABS} active={tab} onSelect={setTab}/>
        </div>

        {/* Body */}
        <div className="em-modal-body" style={{ flex: 1, overflowY: 'auto', padding: 24 }}>

          {/* APPROVE */}
          {tab === DETAIL_TAB.APPROVE && (
            <div style={{ maxWidth: 400 }}>
              <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.15em', marginBottom: 16 }}>New Device — Pending Approval</div>
              {row('Serial', device.device_id)}
              {row('IP', deviceIp(device) || '—')}
              {row('First seen', relTime(device.first_seen))}
              <div style={{ marginTop: 24, marginBottom: 8 }}>
                <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)', marginBottom: 8 }}>Label</div>
                <input type="text" value={approveLabel} onChange={e => setApproveLabel(e.target.value)} placeholder="e.g. Kitchen" aria-label="Label" onKeyDown={e => e.key === 'Enter' && doApprove()}/>
                <div style={{ fontFamily: SANS, fontSize: 11, color: 'var(--muted)', marginTop: 6 }}>
                  Names the device everywhere — the dashboard, and “{approveLabel.trim() || '…'} Voice Assistant” in Home Assistant.
                </div>
              </div>
              {/* Approval is the consequential act on this screen, not a
                  formality: it admits the device to the fleet, opens a voice
                  satellite for it and starts accepting its microphone. The
                  button used to be a default-weight pill reading "Approve
                  Device", which read as an acknowledgement rather than a
                  decision — say what it does, then size it like it matters. */}
              <div style={{ marginTop: 24, background: 'linear-gradient(160deg,var(--text),var(--bg))', border: '1px solid var(--border)', borderRadius: 8, padding: '14px 16px' }}>
                <div style={{ fontFamily: SANS, fontSize: 12, color: 'var(--text2)', lineHeight: 1.5, marginBottom: 14 }}>
                  Approving adds this device to your fleet: it receives the fleet
                  configuration, gets a voice satellite Home Assistant can drive,
                  and its microphone starts streaming to the controller when woken.
                </div>
                <Pill big accent disabled={approving || !approveLabel.trim()} onClick={doApprove}>
                  {approving ? 'Approving…' : 'Approve & Add to Fleet'}
                </Pill>
                {!approveLabel.trim() && (
                  <div style={{ fontFamily: SANS, fontSize: 11, color: 'var(--muted)', marginTop: 10 }}>
                    Enter a label above to continue.
                  </div>
                )}
              </div>
            </div>
          )}

          {/* STATUS */}
          {tab === DETAIL_TAB.STATUS && (() => {
            const s = device.stats || null;
            // cpuPct comes from the aggregate /proc/stat line, so it is a
            // share of ONLINE capacity — and MTK parks 3 of this SoC's 4 cores
            // when idle. The same work reads as half the percentage once a
            // second core comes up, so the core count belongs next to it or
            // the number invites the wrong conclusion.
            const cpuText  = s?.cpuPct != null
              ? `${s.cpuPct.toFixed(0)}%` + (s.coresOnline ? ` · ${s.coresOnline}/${s.coresTotal ?? '?'} cores` : '')
              : null;
            // Thermals: mtktscpu is the CPU zone, maxTempC the hottest of all
            // 11 zones (the PMIC and board sensors can run warmer). Amber past
            // 70C, red past 85C — well below this SoC's limits, because the
            // point is early warning. thermalCoreLimit below coresTotal means
            // the thermal governor is already capping capacity, which is the
            // signal that actually matters and shows up before temperature
            // looks alarming.
            const tempC    = s?.cpuTempC ?? null;
            const tempHot  = s?.maxTempC ?? null;
            const throttled = s?.thermalCoreLimit != null && s?.coresTotal != null
                              && s.thermalCoreLimit < s.coresTotal;
            const ramText  = s?.memUsedMb != null ? `${s.memUsedMb} / ${s.memTotalMb} MB` : null;
            const ramPct   = s?.memTotalMb? s.memUsedMb/s.memTotalMb*100 : null;
            const stoPct   = s?.storageTotalMb ? s.storageUsedMb/s.storageTotalMb*100 : null;
            const stoText  = s?.storageTotalMb != null
              ? `${(s.storageUsedMb/1024).toFixed(1)} / ${(s.storageTotalMb/1024).toFixed(1)} GB` : null;
            // Privacy mute is shown only from a device with a hardware mute it
            // reports (privacy_mute); elsewhere it is unavailable, with why.
            const muteGap  = capabilityGap(liveCapabilities(device), CAPABILITY.PRIVACY_MUTE);
            const muteText = !device.connected ? '—'
              : muteGap ? `unavailable — ${muteGap}`
              : device.muted == null ? '—' : device.muted ? 'Muted' : 'Off';
            return (
              <div style={{ minHeight:'100%', display:'flex', flexDirection:'column', gap:16 }}>
                <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:16 }}>
                  <Panel label="Device">
                    {row('IP', deviceIpText(device, ' (last seen)'))}
                    {row('OS', device.os_version || '—')}
                    {row('Platform', device.platform ? (PLATFORM_LABEL[device.platform] || device.platform) : '—')}
                    {row('EchoMuse firmware', device.firmware_ver || '—')}
                    {row('WiFi network', s?.wifiSsid || '—')}
                    {row('ESPHome port', device.esphome_port != null ? String(device.esphome_port) : '—')}
                    {/* One row, not two. "Connected: Yes" plus "Last seen"
                        was redundant in both directions — while connected the
                        last-seen time says nothing, and while offline the
                        Yes/No says nothing the timestamp doesn't. Merging
                        them frees a row for Volume without the panel growing. */}
                    {row('Status',
                         device.connected ? 'Online' : `Offline · last seen ${relTime(device.last_seen)}`,
                         device.connected ? 'var(--ok)' : 'var(--warn)')}
                    {row('Volume', device.volume_gap ? `unavailable — ${device.volume_gap}`
                         : device.volume != null
                         ? `${Math.round(device.volume * 100)}%`
                         : (s?.volumePct != null ? `${s.volumePct}%` : '—'),
                         device.volume_gap ? 'var(--muted)' : undefined)}
                    {row('Mute', muteText, muteGap ? 'var(--muted)' : device.muted ? 'var(--error)' : undefined)}
                    {row('Link', device.connected ? (device.linkTls ? 'wss (TLS)' : 'plain ws') : '—',
                         device.connected ? (device.linkTls ? 'var(--ok)' : 'var(--warn)') : undefined)}
                    {row('Config', (() => {
                      const n = (device.config_sections ?? []).length;
                      const total = Object.keys(CONFIG_SECTIONS).length;
                      return n === 0 ? 'Fleet' : `Local override (${n} of ${total})`;
                    })())}
                    {isAdmin && device.connected && !device.linkTls && (
                      <div style={{ marginTop: 8 }}>
                        <Pill small accent disabled={securing} onClick={doSecureLink}>
                          {securing ? 'Securing…' : 'Secure link'}
                        </Pill>
                      </div>
                    )}
                  </Panel>
                  <Panel label="Resources">
                    <StatBar label="CPU"     pct={s?.cpuPct}    text={cpuText}/>
                    <StatBar label="RAM"     pct={ramPct}        text={ramText}/>
                    <StatBar label="Storage" pct={stoPct}        text={stoText}/>
                    {/* Scalar health metrics as one deliberate row rather than
                        three label/value lines stacked after the bars. Ordered
                        by how often they are the answer on this hardware: the
                        link first (the usual suspect — the RF counters are
                        useless, so RSSI and RTT are all there is), thermals
                        last because they are almost always boring, and loudly
                        when they are not. Each carries a headroom meter so the
                        row rhymes with the capacity bars above it. */}
                    <div style={{ display:'flex', gap:14, marginTop:2 }}>
                      {/* Band alongside RSSI, because one SSID spanning both
                          radios lets a device re-associate to the slower one
                          silently: measured on this fleet, 2.4GHz runs about
                          60 Mbps against 143 on 5GHz. A device knocked off
                          5GHz can then sit on 2.4 indefinitely with nothing
                          on screen to say so. Shown as a note rather than its
                          own tile — it qualifies the link reading, it is not
                          a separate health metric. */}
                      <StatTile
                        label="Link" value={s?.wifiRssi != null ? s.wifiRssi : null} unit="dBm"
                        sev={s?.wifiRssi == null ? SEVERITY.OK : s.wifiRssi > -70 ? SEVERITY.OK : s.wifiRssi > -80 ? SEVERITY.WARN : SEVERITY.BAD}
                        pct={s?.wifiRssi == null ? null : Math.max(0, Math.min(100, (s.wifiRssi + 95) / 35 * 100))}
                        glyph={<SignalBars rssi={s?.wifiRssi ?? null}/>}
                        sub={[wifiBand(s?.wifiFreqMhz),
                              s?.linkSpeedMbps ? `${s.linkSpeedMbps} Mbps` : null]
                             .filter(Boolean).join(' · ') || null}
                      />
                      {/* Amber past 200ms, red past 1s — the same thresholds the
                          RTT instrumentation counts excursions against. */}
                      <StatTile
                        label="Latency" value={device.rttMs != null ? device.rttMs : null} unit="ms"
                        sev={device.rttMs == null ? SEVERITY.OK : device.rttMs >= 1000 ? SEVERITY.BAD : device.rttMs >= 200 ? SEVERITY.WARN : SEVERITY.OK}
                        pct={device.rttMs == null ? null : Math.min(100, device.rttMs / 500 * 100)}
                      />
                      {/* Scaled 20-90C so the meter shows real headroom; this
                          SoC idles ~33C. thermalCoreLimit below the core count
                          means the governor is already capping capacity, which
                          bites before any temperature looks alarming — so that
                          is the one thing in this row allowed to shout. */}
                      <StatTile
                        label="Temp" value={tempC != null ? tempC.toFixed(1) : null} unit="°C"
                        sev={tempC == null ? SEVERITY.OK : tempC >= 85 ? SEVERITY.BAD : tempC >= 70 ? SEVERITY.WARN : SEVERITY.OK}
                        pct={tempC == null ? null : Math.max(0, Math.min(100, (tempC - 20) / 70 * 100))}
                        note={throttled ? `throttled ${s.thermalCoreLimit}/${s.coresTotal}` : null}
                        glyph={tempHot != null && tempC != null && tempHot > tempC + 1
                          ? <span style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>{tempHot.toFixed(1)} max</span>
                          : null}
                      />
                    </div>
                    {!s && <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:8 }}>waiting for device stats…</div>}
                  </Panel>
                </div>
                {device.approved && <ResponseLatency device={device}/>}
                {device.approved && (
                  <WakeHealth device={device} registry={registry}
                    expected={effectiveConfig(globalConfig, device).wakeModel || registry.active}/>
                )}
                {device.bleProxy && (() => {
                  const b  = s?.ble || null;          // device-side scanner stats
                  const bp = device.bleProxy;         // controller-side proxy state
                  const haState = bp.haSubscribed ? 'Streaming to HA'
                    : bp.haConnected ? 'HA connected (not subscribed)'
                    : bp.listening ? 'Waiting for HA' : 'Port down (device offline)';
                  return (
                    <Panel label="Bluetooth proxy">
                      <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px' }}>
                        <div>
                          {row('Scanner', b ? (b.scanning ? 'Scanning' : 'Stopped') : '—', b?.scanning ? 'var(--ok)' : undefined)}
                          {row('Adverts seen', b ? String(b.advertsSeen ?? 0) : '—')}
                          {row('Nearby devices (5 min)', b ? String(b.uniqueAddrs ?? 0) : '—')}
                          {row('BT address', b?.bdAddr || '—')}
                        </div>
                        <div>
                          {row('Home Assistant', haState, bp.haSubscribed ? 'var(--ok)' : undefined)}
                          {row('Forwarded to HA', String(bp.advertsForwarded ?? 0))}
                          {row('ESPHome port', String(bp.port))}
                          {row('HCI errors / restarts', b ? `${b.hciErrors ?? 0} / ${b.restarts ?? 0}` : '—')}
                        </div>
                      </div>
                    </Panel>
                  );
                })()}
                {/* A LineageOS Show's companion app (stats.companion): the
                    light bar and touch zones, and below them the Kiosk
                    Satellite plugin that shows each turn (`kiosk`). Health,
                    not a capability — either can be down while the session
                    is up. */}
                {(() => {
                  const rows = companionRows(s);
                  const kiosk = kioskRows(s);
                  const cells = part => part.map(([label, text, color]) => (
                    <React.Fragment key={label}>{row(label, text, color)}</React.Fragment>
                  ));
                  const half = list => Math.ceil(list.length / 2);
                  return rows && (
                    <Panel label="Companion app">
                      <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px' }}>
                        <div>{cells(rows.slice(0, 2))}</div>
                        <div>{cells(rows.slice(2))}</div>
                      </div>
                      {kiosk && (
                        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px',
                                                          marginTop:10, paddingTop:10, borderTop:'1px solid var(--hairline)' }}>
                          <div>{cells(kiosk.slice(0, half(kiosk)))}</div>
                          <div>{cells(kiosk.slice(half(kiosk)))}</div>
                        </div>
                      )}
                    </Panel>
                  );
                })()}
              </div>
            );
          })()}

          {/* ACTIVITY — voice-turn observability; its own tab (genuinely
              useful, was cramped at the bottom of Status). */}
          {tab === DETAIL_TAB.ACTIVITY && (() => {
            const cfgEff = effectiveConfig(globalConfig, device);
            const model = registry.models.find(m => m.graph_sha256 === cfgEff.wakeModel);
            const thr = model
              ? ` · idle ${model.thresholds.idle.toFixed(2)} / playback ${model.thresholds.playback.toFixed(2)}`
              : '';
            return (
              <div style={{ minHeight:'100%', display:'flex', flexDirection:'column' }}>
                <Panel label={`Voice activity — ${wakeModelLabel(registry.models, cfgEff.wakeModel)}${thr}`} style={{ flex:1 }}>
                  <TurnObservability
                    turns={turns}
                    nearMisses={nearMisses}
                    nearMissGap={capabilityGap(liveCapabilities(device), CAPABILITY.NEAR_MISS)}
                    labels={{ [device.device_id]: device.label }}
                    device={device.device_id}
                    recordingsOn={cfgEff.saveUtterances}
                    state={{ label: state.label.toUpperCase(), color: state.dot }}
                    limit={ACTIVITY_LIMIT.DEVICE}
                    isAdmin={isAdmin}
                  />
                </Panel>
              </div>
            );
          })()}

          {/* ALERTS — timers, this endpoint's alarms and their delivery */}
          {tab === DETAIL_TAB.ALERTS && <AlertPanel device={device} isAdmin={isAdmin}/>}

          {/* SAMPLES — wake-word training capture */}
          {tab === DETAIL_TAB.SAMPLES && <SamplesTab device={device} isAdmin={isAdmin}/>}

          {/* CONFIG */}
          {tab === DETAIL_TAB.CONFIG && (
            <div>
              {/* Network (WiFi) — always per-device, kept above and visually
                  separate from the fleet-inheritable config below. */}
              <div style={{ paddingBottom: 24, marginBottom: 24, borderBottom: '1px solid var(--line, var(--track))' }}>
                <ConnectivityTab device={device} row={row}/>
              </div>

              {/* Scoping summary. Each section below carries its own
                  Fleet/Device switch — this is just the roll-up plus a way
                  back to fully inheriting. */}
              {isAdmin && globalConfig && (
                <div style={{
                  display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12,
                  background: sections.length ? 'rgba(40,96,64,0.08)' : 'rgba(64,88,120,0.08)',
                  border: `1px solid ${sections.length ? 'rgba(40,96,64,0.2)' : 'rgba(64,88,120,0.2)'}`,
                  borderRadius: 8, padding: '12px 16px', marginBottom: 24, flexWrap: 'wrap',
                }}>
                  <div>
                    <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)' }}>
                      {sections.length
                        ? `Local override (${sections.length} of ${Object.keys(CONFIG_SECTIONS).length})`
                        : 'Following fleet config'}
                    </div>
                    <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginTop: 3 }}>
                      {sections.length
                        ? `Overriding: ${sections.map(s => SECTION_LABELS[s] || s).join(', ')} — everything else tracks the fleet`
                        : 'Switch any section below to Device to customise just that part'}
                    </div>
                  </div>
                  {sections.length > 0 && (
                    <Pill onClick={() => { setSections([]); setDirty(true); }}>
                      Revert all to fleet
                    </Pill>
                  )}
                </div>
              )}

              {/* Config form — each stage editable only when scoped to Device */}
              <DeviceConfigForm
                config={config}
                onChange={(k, v) => setConf(k, v)}
                disabled={!isAdmin}
                sections={sections}
                capabilities={liveCapabilities(device)}
                deviceId={device.device_id}
                deviceConnected={!!device.connected}
                kioskTokenSet={!!device.kioskTokenSet}
                kioskReason={kioskLoginReason(device)}
                onScopeChange={(id, local) => {
                  setSections(prev => local
                    ? [...prev, id]
                    : prev.filter(s => s !== id));
                  setDirty(true);
                }}
              />

              {isAdmin && dirty && (
                <div style={{ display: 'flex', gap: 10, marginTop: 24 }}>
                  <Pill accent disabled={saving} onClick={pushConfig}>
                    {saving ? 'Pushing…' : 'Push config'}
                  </Pill>
                  <Pill onClick={() => {
                    setConfig(effectiveConfig(globalConfig, device));
                    setSections(device.config_sections ?? []);
                    setDirty(false);
                  }}>Cancel</Pill>
                </div>
              )}
            </div>
          )}

          {/* CONSOLE — fills the whole tab frame */}
          {tab === DETAIL_TAB.CONSOLE && (
            device.connected
              ? <div style={{ height: '100%' }}><Shell deviceId={device.device_id} token={token} height="100%"/></div>
              : <div style={{ fontFamily: MONO, fontSize: 12, color: 'var(--warn)' }}>Device offline — console unavailable</div>
          )}

          {/* UPDATES */}
          {tab === DETAIL_TAB.UPDATES && (
            <div style={{ minHeight:'100%', display:'flex', flexDirection:'column', gap:16 }}>

              {/* EchoMuse firmware state. The OS beneath it is a separate
                  version this controller neither installs nor updates. */}
              <Panel label="EchoMuse firmware">
                <div style={{ display:'flex', alignItems:'flex-end', justifyContent:'space-between', gap:16, flexWrap:'wrap' }}>
                  <div style={{ display:'flex', gap:16, alignItems:'flex-end' }}>
                    <Lcd label="On device"  value={device.firmware_ver || '—'} color={needsUpdate ? 'var(--lcd-amber)' : 'var(--lcd-green)'}/>
                    <Lcd label="Bundled"  value={firmware?.version || '—'} color="var(--lcd-dim)"/>
                    {device.firmware_previous && (
                      <Lcd label="Rollback slot" value={device.firmware_previous} color="var(--lcd-dim)"/>
                    )}
                  </div>
                  <div style={{ display:'flex', alignItems:'center', gap:12 }}>
                    <span style={{ fontFamily:MONO, fontSize:11, color: needsUpdate ? 'var(--warn)' : 'var(--ok)' }}>
                      {needsUpdate ? 'Update available'
                        : firmware ? 'Up to date' : 'Bundled firmware unknown'}
                    </span>
                  </div>
                </div>
                {/* Action bar, directly under the version it acts on. The
                    only firmware this controller installs is the one bundled
                    in its image, so there is nothing to choose: one action
                    while the device differs from it, plus rollback. Online
                    it installs now; offline it is queued for the reconnect.
                    Whenever it cannot act, it says why. */}
                <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap', marginTop:16 }}>
                  {(device.connected || !needsUpdate || updateRunning) && (
                    <Pill accent={needsUpdate && device.connected && !updateRunning}
                          disabled={!needsUpdate || !device.connected || updateRunning || !!kioskPrompt}
                          onClick={() => kioskGate(doUpdate)}>
                      {updateRunning ? 'Updating…'
                        : firmware ? `Install bundled ${firmware.version}` : 'Install bundled EchoMuse firmware'}
                    </Pill>
                  )}
                  {(device.update_queued_at != null || (!device.connected && needsUpdate && !updateRunning)) && (
                    <QueuedInstall device={device} firmware={firmware}/>
                  )}
                  {device.firmware_previous && (
                    <Pill disabled={!device.connected || updateRunning} onClick={doRollback}>
                      Roll back to {device.firmware_previous}
                    </Pill>
                  )}
                  <span style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', lineHeight:1.5, flex:'1 1 220px', minWidth:0 }}>
                    A/B slots — the previous binary stays available, and the device
                    rolls itself back if an update fails to start.
                  </span>
                </div>
                {kioskPrompt}
                <div style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)', lineHeight:1.5, marginTop:10 }}>
                  {!firmware ? 'The bundled firmware version is not known yet.'
                    : updateRunning ? 'An update is in progress — wait for it to finish.'
                    : !needsUpdate
                      ? (device.firmware_ver
                          ? `Already running the bundled firmware ${firmware.version}.`
                          : 'This device has not reported its firmware version yet.')
                    : !device.connected
                      ? 'Offline, so it cannot install now. Queue it and the controller installs the '
                        + 'firmware it bundles at the moment the device reconnects — whatever version that '
                        + 'is then, not a pinned one. The queue survives a controller restart; cancel it '
                        + 'any time before the device returns.'
                      : null}
                </div>
                <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', lineHeight:1.5, marginTop:12 }}>
                  Runs on {device.os_version || 'an OS this device has not reported'}. The OS is
                  separate from EchoMuse: updating the firmware here never changes it.
                </div>
              </Panel>

              {/* Maintenance — device-side payloads that are not the firmware
                  binary. These used to sit on the Status tab beside Secure
                  link, which was the wrong home: Status describes what a device
                  IS, and re-applying a payload is something you DO. It belongs
                  next to deploy and rollback. */}
              {isAdmin && (
                <Panel label="Maintenance">
                  <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', lineHeight:1.6, marginBottom:14 }}>
                    Re-apply the debloat: re-sync the start script, which carries the service
                    denylist, and stop Amazon's services now — no reboot needed. The same list is
                    stopped on every boot and with every firmware update; this is for a device
                    already on the current firmware. Idempotent. On an Echo Show with Fire OS it
                    also re-syncs the Alexa hide lists and re-hides any listed app that has become
                    visible. A LineageOS Show has nothing to debloat.
                  </div>
                  <div style={{ display:'flex', alignItems:'center', gap:10, flexWrap:'wrap' }}>
                    <Pill small disabled={!device.connected || debloating || !!device.debloat_gap} onClick={doDebloat}>
                      {debloating ? 'Applying…' : 'Re-apply debloat'}
                    </Pill>
                    {device.debloat_gap && (
                      <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>{device.debloat_gap}</span>
                    )}
                  </div>
                </Panel>
              )}

              {/* Activity console — always present so the layout never jumps
                  when a deploy starts */}
              <div className="em-inset" style={{ '--em-inset-pad':'14px', fontFamily:MONO, fontSize:12, minHeight:96, flex:1 }}>
                {pushLog.length === 0 && !pushing && (
                  <span style={{ color:'var(--lcd-faint)' }}>— no deploy activity this session —</span>
                )}
                {pushLog.map((line, i) => (
                  <div key={i} style={{
                    color: line.startsWith('✓') ? 'var(--lcd-green)'
                         : line.startsWith('⚠') ? 'var(--warn)'
                         : line.startsWith('Error') ? 'var(--error)'
                         : 'var(--lcd-faint)',
                    marginBottom:4,
                    textShadow: line.startsWith('✓') ? '0 0 8px rgba(140,200,100,0.4)' : 'none',
                  }}>{line}</div>
                ))}
                {pushing && <span style={{ color:'var(--lcd-faint)' }}>▌</span>}
              </div>
            </div>
          )}

          {/* LOGS */}
          {tab === DETAIL_TAB.LOGS && (
            <div>
              <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.15em', marginBottom: 16 }}>Device logs</div>
              {logsLoading ? (
                <div style={{ fontFamily: MONO, fontSize: 12, color: 'var(--muted)' }}>Loading…</div>
              ) : logs.length === 0 ? (
                <div style={{ fontFamily: MONO, fontSize: 12, color: 'var(--muted)' }}>No logs</div>
              ) : logs.map((entry, i) => (
                <div key={i} style={{ display: 'flex', gap: 12, alignItems: 'baseline', padding: '8px 0', borderBottom: '1px solid var(--hairline)' }}>
                  <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--faint)', minWidth: 60, flexShrink: 0 }}>{new Date(entry.ts).toLocaleTimeString()}</span>
                  <span style={{ fontFamily: MONO, fontSize: 9, color: eventAccent(entry.level), textTransform: 'uppercase', letterSpacing: '0.1em', minWidth: 48, flexShrink: 0 }}>{entry.level}</span>
                  <span style={{ fontFamily: MONO, fontSize: 9, color: entry.source === LOG_SOURCE.DEVICE ? 'var(--lcd-faint)' : 'var(--accent-deep)', textTransform: 'uppercase', letterSpacing: '0.08em', minWidth: 64, flexShrink: 0 }}>{entry.source}</span>
                  <span style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)' }}>{entry.message}</span>
                </div>
              ))}
            </div>
          )}
        </div>
    </ModalFrame>
  );
}

// ─── Device card ──────────────────────────────────────────────────────────────

// A version value on a card: right-aligned, cut with an ellipsis rather than
// wrapping the card taller (the full OS name is the title).
const CARD_VERSION = Object.freeze({ color: 'var(--muted)', textAlign: 'right', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' });

function Card({ device, onClick, firmware }) {
  const state = deviceState(device);
  const isPending = !device.approved;
  // Behind the bundled firmware (approved devices only — a pending one is
  // offered nothing until approved), and whether an install awaits its reconnect.
  const behind = !isPending && firmwareBehind(device, firmware);
  const queued = device.update_queued_at != null;
  const versionTitle = queued ? `Install of the bundled firmware queued — runs when the device reconnects`
    : behind ? `Not on the bundled firmware ${firmware.version} — open the device's Updates tab`
    : undefined;

  return (
    <div {...pressable(onClick)} aria-label={`${device.label || device.device_id} — ${state.label}`}
      style={{ background: 'linear-gradient(160deg,var(--card),var(--bg))', border: '1px solid var(--border)', borderRadius: 14, cursor: 'pointer', boxShadow: '0 4px 16px var(--track),0 1px 0 var(--sheen) inset', transition: 'box-shadow 0.15s,transform 0.1s', userSelect: 'none', opacity: isPending ? 0.85 : 1 }}
      onMouseEnter={e => { e.currentTarget.style.boxShadow = '0 8px 28px rgba(0,0,0,0.18),0 1px 0 var(--sheen) inset'; e.currentTarget.style.transform = 'translateY(-1px)'; }}
      onMouseLeave={e => { e.currentTarget.style.boxShadow = '0 4px 16px var(--track),0 1px 0 var(--sheen) inset'; e.currentTarget.style.transform = 'translateY(0)'; }}>
      <div style={{ background: 'linear-gradient(180deg,var(--sunken),var(--sunken))', borderBottom: '1px solid var(--border-hard)', borderRadius: '13px 13px 0 0', padding: '10px 16px', display: 'flex', justifyContent: 'space-between', alignItems: 'center', boxShadow: '0 1px 0 var(--sheen) inset' }}>
        <span style={{ fontFamily: SANS, fontSize: 14, color: 'var(--text)', fontWeight: 600, letterSpacing: '-0.01em' }}>
          {device.label || <span style={{ color: 'var(--muted)', fontSize: 12 }}>{device.device_id.slice(0, 8)}…</span>}
        </span>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          {isPending && (
            // Chrome sized this box off the DM Mono line box rather than the
            // glyphs, so 1px symmetric padding rendered visibly bottom-heavy
            // next to the 14px label. inline-flex + lineHeight:1 makes the
            // height the text's own; the trimmed paddingRight cancels the
            // trailing letter-space Chrome leaves after the final N, which is
            // what made the word look shunted left inside its own badge.
            <div style={{ display: 'inline-flex', alignItems: 'center', background: 'linear-gradient(160deg,var(--lcd-face),var(--lcd-deep))', border: '1px solid var(--lcd-line)', borderRadius: 3, padding: '3px 6px', paddingRight: 'calc(6px - 0.1em)', fontFamily: MONO, fontSize: 9, lineHeight: 1, color: 'var(--accent-lit)', letterSpacing: '0.1em' }}>PENDING</div>
          )}
        </div>
      </div>
      <div style={{ display: 'flex', justifyContent: 'center', padding: '20px 0 12px' }}>
        <LedRing state={state} size={120}/>
      </div>
      <div style={{ padding: '0 16px 16px' }}>
        <div className="em-inset" style={{ '--em-inset-radius':'6px', '--em-inset-pad':'7px 12px', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <span style={{ fontFamily: MONO, fontSize: 11, color: state.dot, letterSpacing: '0.12em', textShadow: `0 0 8px ${state.dot}88` }}>{state.label.toUpperCase()}</span>
          <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--lcd-dim)', letterSpacing: '0.08em' }}>{deviceIpText(device, ' ↑')}</span>
        </div>
        {/* Both versions, each named: the OS the Dot was rooted on, and the
            EchoMuse firmware this controller installs on top of it. */}
        <div style={{ display: 'grid', gridTemplateColumns: 'auto minmax(0,1fr)', columnGap: 8, rowGap: 3, marginTop: 10, fontFamily: MONO, fontSize: 9, letterSpacing: '0.05em' }}>
          <span style={{ color: 'var(--faint)' }}>OS</span>
          <span title={device.os_version || undefined} style={CARD_VERSION}>{osVersionShort(device) || '—'}</span>
          <span style={{ color: 'var(--faint)' }}>EchoMuse</span>
          <span title={versionTitle}
                style={{ ...CARD_VERSION, color: queued ? 'var(--accent)' : behind ? 'var(--warn)' : 'var(--muted)' }}>
            {device.firmware_ver || '—'}{queued ? ' · queued' : ''}
          </span>
        </div>
      </div>
    </div>
  );
}

// ─── Provisioning Wizard ──────────────────────────────────────────────────────

// ADB-over-WebUSB client — thin wrapper around @yume-chan/adb 2.1.0.
// Lazy-loads from esm.sh on first use (dynamic import works in classic scripts).
// Exposes the same interface the wizard step runners expect:
//   Client.requestDevice() -> client
//   client.connect()
//   client.shell(cmd)   -> string
//   client.push(path, Uint8Array, onProgress?)
//   client.pull(path)   -> Uint8Array
//   client.root()       -> adbd's answer to `adb root`
//   client.reopen()     -> client, the same device after adbd restarted
//   client.close()
const _ADB = (() => {
  // Module cache — loaded once on first requestDevice() call.
  let _mods = null;

  async function _load(logFn) {
    if (_mods) return _mods;
    logFn('Loading ADB library from esm.sh…');
    const [webUsbMod, adbMod] = await Promise.all([
      import('https://esm.sh/@yume-chan/adb-daemon-webusb@2.1.0?bundle&deps=@yume-chan/adb@2.1.0'),
      import('https://esm.sh/@yume-chan/adb@2.1.0?bundle'),
    ]);
    _mods = {
      manager:       webUsbMod.AdbDaemonWebUsbDeviceManager,
      Transport:     adbMod.AdbDaemonTransport,
      Adb:           adbMod.Adb,
      defaultAuths:  adbMod.ADB_DEFAULT_AUTHENTICATORS,
    };
    logFn('ADB library loaded.');
    return _mods;
  }

  // Drain a WHATWG ReadableStream<Uint8Array> into a single Uint8Array.
  async function _readAll(stream) {
    const reader = stream.getReader();
    const chunks = [];
    let total = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      total += value.length;
    }
    const out = new Uint8Array(total);
    let off = 0;
    for (const c of chunks) { out.set(c, off); off += c.length; }
    return out;
  }

  // Track the last usbDevice so we can release it before reconnecting.
  let _lastUsbDevice = null;

  class Client {
    constructor(adb, transport, banner, serial) {
      this._adb = adb;
      this._transport = transport;
      this.banner = banner;  // product name string, e.g. "omni_biscuit" or "csm_biscuit"
      // Carried so a WebUSB 'disconnect' event can be matched to this client
      // rather than to any other USB device the operator happens to unplug.
      this.serial = serial ?? null;
      this._log = () => {};
    }

    // Spawn a command and return its stdout as a trimmed string.
    // Must use noneProtocol — shellProtocol requires Android 7+.
    async shell(cmd) {
      const proc = await this._adb.subprocess.noneProtocol.spawn(cmd);
      const out = await _readAll(proc.output);
      return new TextDecoder().decode(out).replace(/\r\n/g, '\n').trim();
    }

    // Push bytes to a remote path via `cat >`.
    // stdin is a WritableStream<Uint8Array>; we write in 64 KB chunks.
    async push(remotePath, data, onProgress) {
      const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
      // The per-phase lines exist to localise a stall — which phase it hung in
      // is the whole diagnostic — but they are four lines per push, and a
      // provision makes eight sub-megabyte pushes that complete instantly.
      // Narrate the transfers that can actually stall; one line for the rest.
      const chatty = bytes.length >= 1024 * 1024;
      if (chatty) this._log(`push: opening cat > '${remotePath}' (${(bytes.length/1024/1024).toFixed(1)} MB)`);
      const proc  = await this._adb.subprocess.noneProtocol.spawn(`cat > '${remotePath}'`);
      if (chatty) this._log('push: stream open, writing chunks…');
      const writer = proc.stdin.getWriter();
      const SZ = 64 * 1024;
      for (let i = 0; i < bytes.length; i += SZ) {
        await writer.write(bytes.subarray(i, Math.min(i + SZ, bytes.length)));
        onProgress?.((i + SZ) / bytes.length);
      }
      if (chatty) this._log('push: all chunks written, closing stdin…');
      await writer.close();
      onProgress?.(1);
      this._log(chatty ? 'push: done.'
                       : `push: '${remotePath}' (${(bytes.length/1024).toFixed(0)} KB) done.`);
      // No drain — a cat that does not close stdout when stdin closes would
      // hang _readAll forever. The next shell command provides sequencing.
    }

    // Pull a remote file as a Uint8Array via `cat`.
    async pull(remotePath) {
      this._log(`pull: cat '${remotePath}'`);
      const proc = await this._adb.subprocess.noneProtocol.spawn(`cat '${remotePath}'`);
      this._log('pull: draining output…');
      const out = await _readAll(proc.output);
      this._log(`pull: done (${(out.length/1024/1024).toFixed(1)} MB)`);
      return out;
    }

    async close() {
      try { await this._transport.close(); } catch {}
    }

    // adb's `root:` service, as `adb root` sends it: Adb.createSocketAndWait
    // (@yume-chan/adb 2.1.0 src/adb.ts), the call the library's own
    // AdbTcpIpService makes for `tcpip:`; 2.1.0 has no root helper. adbd
    // answers one line and closes the socket (LineageOS 18.1's
    // restart_root_service): "restarting adbd as root" (it then restarts as
    // uid 0, which drops this connection and re-enumerates the USB device),
    // "adbd is already running as root", or a refusal, "ADB Root access is
    // disabled by system setting - …" while Rooted debugging is off. Returns
    // that line, trimmed; null when the connection went down before the
    // answer arrived, which only the restart does.
    async root() {
      try {
        return (await this._adb.createSocketAndWait('root:')).trim();
      } catch {
        return null;
      }
    }

    // The same device again after adbd restarted (root()), without the
    // picker: waits for this connection to drop (Adb.disconnected; 10 s at
    // most, then it is closed anyway), finds the device by its serial among
    // those the browser has already been granted
    // (AdbDaemonWebUsbDeviceManager.getDevices, @yume-chan/adb-daemon-webusb
    // 2.1.0 src/manager.ts) and authenticates again. A restarted adbd
    // enumerates as the same USB device with the same serial, so the grant
    // holds. Returns the new Client.
    async reopen(logFn = () => {}, timeoutMs = 60000) {
      if (!this.serial) {
        throw new Error('The device reported no USB serial, so it cannot be found again without '
                      + 'the picker. Click Reconnect.');
      }
      const { manager } = await _load(logFn);
      await Promise.race([this._adb.disconnected, new Promise(r => setTimeout(r, 10000))]);
      await this.close();
      logFn(`Waiting for ${this.serial} to come back over USB…`);
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        await new Promise(r => setTimeout(r, 1000));
        const usbDevice = (await manager.BROWSER.getDevices()).find(d => d.serial === this.serial);
        if (!usbDevice) continue;
        try {
          return await _open(usbDevice, logFn);
        } catch (e) {
          logFn(`Not answering yet (${e.message}) — retrying…`);
        }
      }
      throw new Error(`${this.serial} did not come back over USB within ${timeoutMs / 1000}s of `
                    + 'adbd restarting. Unplug it and plug it back in, click Reconnect, then Retry.');
    }

    // ── Static factory ──────────────────────────────────────────────────────

    // Open the browser USB picker, load the library, authenticate, return a
    // ready Client.  logFn is optional — wizard passes addLog.
    static async requestDevice(logFn = () => {}) {
      if (!navigator.usb) {
        throw new Error(
          'WebUSB not available — requires a secure context (HTTPS or localhost). ' +
          'Access the dashboard at http://localhost:8768, or enable ' +
          'chrome://flags/#unsafely-treat-insecure-origin-as-secure for this origin.'
        );
      }

      const { manager } = await _load(logFn);

      // Release any previous connection — calling connect() on an already-claimed
      // interface hangs indefinitely. This happens on retry after a reboot.
      if (_lastUsbDevice) {
        try { await _lastUsbDevice.disconnect(); } catch {}
        _lastUsbDevice = null;
      }

      logFn('Requesting USB device — select the Echo Dot from the picker…');
      const usbDevice = await manager.BROWSER.requestDevice();
      if (!usbDevice) throw new Error('No device selected.');
      logFn(`Device selected: ${usbDevice.name ?? usbDevice.serial ?? 'unknown'}`);
      return _open(usbDevice, logFn);
    }
  }

  // Connect to a picked (or already granted) device and authenticate.
  async function _open(usbDevice, logFn) {
    const { Transport, Adb, defaultAuths } = await _load(logFn);
    _lastUsbDevice = usbDevice;

    logFn('Opening USB connection…');
    const connection = await usbDevice.connect();

    logFn('Authenticating ADB…');
    const transport = await Transport.authenticate({
      serial:         usbDevice.serial ?? 'echomuse',
      connection,
      authenticators: defaultAuths,
    });
    logFn('ADB authenticated.');

    const adb = new Adb(transport);
    const banner = adb.banner?.product ?? '(unknown)';
    logFn(`Connected. Banner: ${banner}`);

    return new Client(adb, transport, banner, usbDevice.serial ?? null);
  }

  return { Client };
})();

// ── AddDeviceTile ──

function AddDeviceTile({ onClick }) {
  const [hover, setHover] = useState(false);
  return (
    <div
      {...pressable(onClick)}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        border: `2px dashed ${hover ? 'var(--text2)' : 'var(--border-hard)'}`,
        // 12 -> 14 to match Card's corner. minHeight is only the floor for an
        // empty fleet; with any device present the grid row sets the height.
        borderRadius: 14, minHeight: 244, display: 'flex', flexDirection: 'column',
        alignItems: 'center', justifyContent: 'center', gap: 8, cursor: 'pointer',
        transition: 'border-color 0.15s, opacity 0.15s', opacity: hover ? 1 : 0.6,
        userSelect: 'none',
      }}
    >
      <div style={{ fontSize: 28, color: hover ? 'var(--text2)' : 'var(--border-hard)', lineHeight: 1 }}>+</div>
      <div style={{ fontFamily: MONO, fontSize: 9, color: hover ? 'var(--text2)' : 'var(--border-hard)', letterSpacing: '0.12em', textTransform: 'uppercase' }}>Provision Device</div>
    </div>
  );
}

// ── ProvisionWizard ──

// The wizard's own vocabularies: a step's state and a transcript line's tone.
const STEP_STATE = Object.freeze({ PENDING: 'pending', RUNNING: 'running', DONE: 'done', ERROR: 'error' });
const LOG_TONE = Object.freeze({ INFO: 'info', OK: 'ok', WARN: 'warn', ERROR: 'error', HEAD: 'head' });

// Dashboard-palette step states — same tones the rest of the UI uses
// (accent slate for activity, deep green for done, rust for error).
const STEP_STATE_STYLE = Object.freeze({
  [STEP_STATE.PENDING]: { color: 'var(--muted)',  icon: '○' },
  [STEP_STATE.RUNNING]: { color: 'var(--accent)', icon: '◌' },
  [STEP_STATE.DONE]:    { color: 'var(--ok)',     icon: '●' },
  [STEP_STATE.ERROR]:   { color: 'var(--warn)',   icon: '✕' },
});

const LOG_TONE_COLOR = Object.freeze({
  [LOG_TONE.ERROR]: 'var(--error)', [LOG_TONE.OK]: 'var(--ok)', [LOG_TONE.WARN]: 'var(--warn)',
});

// The devices the wizard provisions, by their em_device_link.DevicePlatform
// value: the string the firmware announces in session.hello once installed.
const WIZARD_PLATFORM = Object.freeze({
  DOT: 'fireos6', CROWN: 'fireos6_crown', CROWN_LINEAGE: 'lineage18_crown',
});

// What a step is called on each, for "not applicable" notes.
const WIZARD_PLATFORM_NAME = Object.freeze({
  [WIZARD_PLATFORM.DOT]: 'Echo Dot', [WIZARD_PLATFORM.CROWN]: 'Echo Show 8',
  [WIZARD_PLATFORM.CROWN_LINEAGE]: 'Echo Show 8 (LineageOS)',
});

// Both Echo Show images: Android owns their Wi-Fi, their audio service is
// audioserver, and the server logs under /data/local/tmp/echomuse.
function _isShow(platform) {
  return platform === WIZARD_PLATFORM.CROWN || platform === WIZARD_PLATFORM.CROWN_LINEAGE;
}

// /system/lib/libasp.so of our LineageOS 18.1 build for crown: the NS6571
// AFE, which writes the v3.3 metadata the firmware decodes
// (docs/echo-show-lineage-plan.md §1 "ROM", docs/echo-show-crown.md §8.4).
// Upstream builds carry another libasp.so (v0.6: no metadata at all), so
// this hash is what connect_android pins a LineageOS Show to, in place of a
// Fire OS build id.
const _LINEAGE_LIBASP_SHA1 = 'e3816eb3efd4654c708d3891a5f907b79f3ebce3';

// Whether `sha1sum /system/lib/libasp.so` (its trimmed output) names ours.
function _isOurLibasp(sha1sumOut) {
  return (sha1sumOut || '').trim().split(/\s+/)[0].toLowerCase() === _LINEAGE_LIBASP_SHA1;
}

// What connect_android requires of the device's own properties before it
// trusts the shell, and which device that makes it:
//  - Echo Dot 2nd gen: Fire OS 6 (Android 7.1.x, biscuit_puffin) with the adb
//    shell already root via boot-root.zip;
//  - Echo Show 8 1st gen: Fire OS 6 (Android 7.1.x, product crown) with the
//    adb shell already root via the crown thread's boot-root.img;
//  - Echo Show 8 1st gen on LineageOS 18.1 (Android 11, product crown,
//    ro.lineage.version set): our build, by its libasp.so, with the adb
//    shell root once Developer options' "Rooted debugging" let adbd restart
//    as root (connect_android asks for that before calling this).
// Returns the WIZARD_PLATFORM; throws the refusal. Pure and side-effect-free
// so every outcome is testable without a device.
//
// `uid` is the trimmed stdout of `id -u`; `productName` and `productDevice`
// are ro.product.name and ro.product.device; `lineageVersion` is
// ro.lineage.version (empty on Fire OS); `libaspSha1` is sha1sum's output
// for /system/lib/libasp.so and `rootReply` adbd's answer to `adb root`,
// both asked of a LineageOS Show only.
function classifyDevice(opts) {
  const { release, productName, productDevice, uid, lineageVersion, libaspSha1, rootReply } = opts;
  const rel = release || '';
  const lineage = (lineageVersion || '').trim();
  const crown = productName === 'crown' || productDevice === 'crown';
  if (rel.startsWith('5.')) {
    throw new Error(
      `This Dot runs Fire OS 5 (Android ${rel}), which EchoMuse no longer supports. Move it to `
      + `Fire OS 6 first (amonet-biscuit v2, then boot-root.zip) — see docs/rooting.md in the `
      + `EchoMuse repository.`);
  }
  if (crown && lineage) {
    if (!rel.startsWith('11')) {
      throw new Error(
        `This Echo Show 8 runs LineageOS ${lineage} (Android ${rel}); EchoMuse supports `
        + `LineageOS 18.1 (Android 11) only — see docs/rooting.md.`);
    }
    if (!_isOurLibasp(libaspSha1)) {
      const sha = (libaspSha1 || '').trim().split(/\s+/)[0];
      throw new Error(
        `This Echo Show 8 runs LineageOS ${lineage}, but not the build EchoMuse needs: its `
        + `/system/lib/libasp.so is ${sha || '(unreadable)'}, not the NS6571 AFE `
        + `(${_LINEAGE_LIBASP_SHA1.slice(0, 8)}…) whose metadata EchoMuse reads. Flash a build of `
        + `our fork (branch crown-ns6571-asp of kdkavanagh/android_device_amazon_crown and `
        + `kdkavanagh/android_vendor_amazon_crown) — see docs/rooting.md — then Retry.`);
    }
    if (uid !== '0') {
      throw new Error(
        `This Echo Show 8 runs LineageOS ${lineage} but the adb shell is not root `
        + `(id -u = "${uid || '(unknown)'}"${rootReply ? `; adbd said "${rootReply}"` : ''}). `
        + `On the Show, open Developer options (Settings → System) and switch on "USB debugging" `
        + `and "Rooted debugging" (if Developer options is missing, tap Build number in the `
        + `Settings → About page seven times first), then Retry — see docs/rooting.md.`);
    }
    return WIZARD_PLATFORM.CROWN_LINEAGE;
  }
  if (!rel.startsWith('7.1')) {
    throw new Error(
      `Expected Fire OS 6 (Android 7.1.x: an Echo Dot 2nd gen, biscuit_puffin, or an Echo `
      + `Show 8 1st gen, crown) or LineageOS 18.1 on an Echo Show 8, got Android ${rel}. `
      + `Wrong device?`);
  }
  if (crown) {
    if (uid !== '0') {
      throw new Error(
        `This is an Echo Show 8 (crown, Android ${rel}) but the adb shell is not root `
        + `(id -u = "${uid || '(unknown)'}"). Unlock it and flash boot-root.img as R0rt1z2's `
        + `crown thread describes first — see https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/`);
    }
    return WIZARD_PLATFORM.CROWN;
  }
  if (productName !== 'biscuit_puffin') {
    throw new Error(
      `Android ${rel} but product is "${productName || '(unknown)'}", not "biscuit_puffin" `
      + `(Echo Dot 2nd gen) or "crown" (Echo Show 8 1st gen). Wrong device?`);
  }
  if (uid !== '0') {
    throw new Error(
      `This Dot is Fire OS 6 (Android ${rel}, biscuit_puffin) but the adb shell is not root `
      + `(id -u = "${uid || '(unknown)'}"). Fire OS 6 needs R0rt1z2's boot-root.zip flashed `
      + `from TWRP first — see https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-dot-2nd-gen-2016-biscuit.4761416/`);
  }
  return WIZARD_PLATFORM.DOT;
}

// /system/etc/init/echomuse.rc, written byte-for-byte by install_boot_hook
// (see docs/fireos6-port.md Phase 3). `seclabel
// u:r:adbd:s0`, not `u:r:su:s0`: init refuses the transition into `su`
// while enforcing (confirmed on hardware). `adbd` is only the way in:
// boot-root lets it move itself into `su`, and start_server.sh does so
// before starting the server, because wpa_supplicant's replies to `adbd`
// are denied. The `aipc` primary group is what the mixer requires. The
// trailing newline is load-bearing: Android 7's init parser acts on a line
// at its newline and drops an unterminated last line, which would be the
// `start echomuse` trigger.
const _ECHOMUSE_RC = `service echomuse /system/bin/sh /data/local/bin/start_server.sh
    class late_start
    user root
    group aipc audio system inet wifi net_admin net_raw bluetooth net_bt_stack wakelock input
    seclabel u:r:adbd:s0
    disabled

on property:sys.boot_completed=1
    start echomuse
`;

// The Echo Show 8's echomuse.rc (docs/echo-show-port-plan.md decisions 10
// and 11). uid 0 with primary group `audio`: `aipc` is the Dot's mixer's
// requirement and means nothing here. No wifi, bluetooth, net_admin or
// input groups either: the Show's Wi-Fi, Bluetooth and keys are Android's,
// and the firmware leaves them alone. The same way in as the Dot, measured
// on the Show (docs/echo-show-crown.md §10.4): init starts the script in
// `adbd`, where audioserver refuses its binder replies (no capture, no
// `pm`), and start_server.sh moves itself into `su`, where both work.
// boot_completed matters more here: the script's re-hiding needs the
// package manager. Same trailing-newline rule.
const _ECHOMUSE_RC_CROWN = `service echomuse /system/bin/sh /data/local/bin/start_server.sh
    class late_start
    user root
    group audio system inet net_raw wakelock
    seclabel u:r:adbd:s0
    disabled

on property:sys.boot_completed=1
    start echomuse
`;

// The Echo Show 8's echomuse.rc on LineageOS 18.1 (docs/echo-show-lineage-
// plan.md decision 9). There init may start the service in `su` itself, a
// defined, permissive domain in this userdebug policy, so there is no
// `adbd` way in and no move: measured with a test service of the same
// shape, which recorded at boot (docs/echo-show-crown.md §8.5). `root` and
// `audio` for the capture, `inet` for the controller link; nothing else is
// the firmware's there. Same trailing-newline rule.
const _ECHOMUSE_RC_LINEAGE = `service echomuse /system/bin/sh /data/local/bin/start_server.sh
    class late_start
    user root
    group root audio inet
    seclabel u:r:su:s0
    disabled

on property:sys.boot_completed=1
    start echomuse
`;

// Where install_boot_hook puts _ECHOMUSE_ADDOND_LINEAGE.
const _ADDOND_PATH = '/system/addon.d/50-echomuse.sh';

// The addon.d v2 script that keeps the LineageOS boot hook across a
// LineageOS update (decision 9): the updater's backuptool runs every
// /system/addon.d script around the system write, and this one backs up
// and restores /system/etc/init/echomuse.rc, nothing else. Written 0755,
// byte-for-byte; the shape is LineageOS 18.1's own 50-lineage.sh, and a
// dirty flash with a script of this shape kept the .rc (docs/echo-show-
// crown.md §8.5). The binary and its state are on /data, which an update
// does not touch; a clean flash loses both files, and re-running the wizard
// writes them again.
const _ECHOMUSE_ADDOND_LINEAGE = `#!/sbin/sh
#
# ADDOND_VERSION=2
#
# /system/addon.d/50-echomuse.sh
# During a LineageOS upgrade, this script backs up EchoMuse's init service,
# /system/etc/init/echomuse.rc; /system is formatted and reinstalled, then
# the file is restored. Written by the EchoMuse provisioning wizard.
#

. /tmp/backuptool.functions

list_files() {
cat <<EOF
etc/init/echomuse.rc
EOF
}

case "$1" in
  backup)
    list_files | while read FILE DUMMY; do
      backup_file $S/"$FILE"
    done
  ;;
  restore)
    list_files | while read FILE REPLACEMENT; do
      R=""
      [ -n "$REPLACEMENT" ] && R="$S/$REPLACEMENT"
      [ -f "$C/$S/$FILE" ] && restore_file $S/"$FILE" "$R"
    done
  ;;
  pre-backup)
    # Stub
  ;;
  post-backup)
    # Stub
  ;;
  pre-restore)
    # Stub
  ;;
  post-restore)
    # Stub
  ;;
esac
`;

// The boot hook a platform gets.
function _rcFor(platform) {
  if (platform === WIZARD_PLATFORM.CROWN) return _ECHOMUSE_RC_CROWN;
  if (platform === WIZARD_PLATFORM.CROWN_LINEAGE) return _ECHOMUSE_RC_LINEAGE;
  return _ECHOMUSE_RC;
}

// Whether the file already on device matches what install_boot_hook would
// write — the remount-write-remount dance is skipped when it does, so a
// re-run (or a second device provisioned from the same session) doesn't
// pay for a write that changes nothing. `existing` is `cat`'s output,
// already trimmed by Client.shell(); `rc` is trimmed to match.
function _rcInstalled(existing, rc = _ECHOMUSE_RC) {
  return (existing || '').trim() === rc.trim();
}

// The Fire OS 6 build EchoMuse is developed and tested against. "NS6569/6009"
// is the parenthesised build code inside ro.build.version.name on the device
// this was ported and tested on ("Fire OS 6.5.6.9 (NS6569/6009)", both
// slots) — ro.build.version.incremental ("0011980470660" there) is a
// separate, less legible counter, so the readable code is what's compared
// and what's shown. A device on a different build is the first thing worth
// knowing when it behaves oddly (see #79).
//
// A warning, not a refusal: an untested build may well provision fine, we
// just have no evidence either way, and blocking someone whose device works
// would be the worse error. Mirrored in docs/rooting.md, pinned by test.
const _TESTED_FIREOS6_BUILD = 'NS6569/6009';
const _TESTED_FIREOS6_NAME  = 'Fire OS 6.5.6.9';
// The Echo Show 8's, the same way: the one unit the port was measured on
// reads "Fire OS 6574.1 (NS65741/8146)" (docs/echo-show-crown.md §1). It
// matters more here than on the Dot: the Show's volume control uses binder
// transaction codes pinned for this build.
const _TESTED_CROWN_BUILD = 'NS65741/8146';
const _TESTED_CROWN_NAME  = 'Fire OS 6574.1';

// The untested-build warning for a device, or null when it runs the tested
// build (or says nothing about its build). `buildIds` are
// ro.build.version.name and ro.build.display.id: either may carry the code.
// A LineageOS Show has no Fire OS build id to compare; its pin is the
// libasp.so hash classifyDevice already required.
function untestedBuild(platform, buildIds) {
  if (platform === WIZARD_PLATFORM.CROWN_LINEAGE) return null;
  const [code, name] = platform === WIZARD_PLATFORM.CROWN
    ? [_TESTED_CROWN_BUILD, _TESTED_CROWN_NAME]
    : [_TESTED_FIREOS6_BUILD, _TESTED_FIREOS6_NAME];
  const ids = (buildIds || []).filter(Boolean);
  if (!ids.length || ids.some(id => id.includes(code))) return null;
  return `Untested OS build — EchoMuse is developed against ${name} (${code}) on the `
       + `${WIZARD_PLATFORM_NAME[platform]}. Other Fire OS 6 builds may behave differently.`;
}

// The address Android's own Wi-Fi holds, from `ip -4 addr show wlan0`, or
// null. The Echo Show joins Wi-Fi from its own setup screen or Settings, and
// EchoMuse never configures it (docs/echo-show-port-plan.md decision 7), so
// connect_android refuses a Show that is not already on a network.
function _wlanAddress(ipOut) {
  const m = /^\s*inet (\d{1,3}(?:\.\d{1,3}){3})\//m.exec(ipOut || '');
  return m ? m[1] : null;
}

// The Echo Show's HOME activities, and the one Android would start.
// `--brief` prints one `package/class` line per activity (the class
// sometimes in its short ".Class" form) under an "Activity #N:" header
// and a priority line; resolve-activity prints the priority line and the
// winner (android/…ResolverActivity when Android would ask).
const _HOME_QUERY = 'cmd package query-activities --brief '
                  + '-a android.intent.action.MAIN -c android.intent.category.HOME 2>&1';
const _HOME_RESOLVE = 'cmd package resolve-activity --brief '
                    + '-a android.intent.action.MAIN -c android.intent.category.HOME 2>&1';

// The `package/class` components in either command's output, as {pkg, cls}.
function _homeActivities(out) {
  const found = [];
  for (const line of (out || '').split('\n')) {
    const m = /^\s*([A-Za-z][\w.]*)\/([\w.$]+)\s*$/.exec(line);
    if (m) found.push({ pkg: m[1], cls: m[2] });
  }
  return found;
}

// The HOME activities that can stand in for Amazon's: not Amazon's own
// (com.amazon.*: the setup screen and the launcher are hidden by the
// wizard), and not Settings' FallbackHome, the blank placeholder Android
// shows when nothing else resolves. Hiding Amazon's with none of these left
// boots the Show to a blank screen (docs/echo-show-crown.md §9.4).
function _kioskHomes(activities) {
  return activities.filter(a => !a.pkg.startsWith('com.amazon.') && !a.cls.endsWith('FallbackHome'));
}

const _component = a => `${a.pkg}/${a.cls}`;

// The same component however it is printed: Android shortens a class in
// its own package to ".Class".
const _fullClass = a => (a.cls.startsWith('.') ? a.pkg + a.cls : a.cls);
const _sameComponent = (a, b) => !!a && !!b && a.pkg === b.pkg && _fullClass(a) === _fullClass(b);

// Kiosks whose HOME activity ships disabled. Fully Kiosk Browser (measured,
// 1.61.4 on the Show) declares `.LauncherReplacement` disabled and enables
// it only from its own launcher setting, so it is not a HOME candidate
// after a plain install. check_home enables it when the kiosk is installed
// and no other non-Amazon HOME exists, which is what that setting does.
const _KNOWN_KIOSK_HOMES = Object.freeze([
  Object.freeze({ pkg: 'de.ozerov.fully', cls: '.LauncherReplacement', name: 'Fully Kiosk Browser' }),
]);

// The known kiosk whose disabled HOME activity check_home should enable, or
// null: only when no kiosk HOME is offered at all and the kiosk is
// installed. `installed` is a Set of package names.
function _kioskToEnable(homes, installed) {
  if (_kioskHomes(homes).length) return null;
  return _KNOWN_KIOSK_HOMES.find(k => installed.has(k.pkg)) || null;
}

// The kiosk neutralize_alexa makes the default home: a known kiosk first,
// else the first candidate. Null without one.
function _chooseKiosk(kiosks) {
  return kiosks.find(a => _KNOWN_KIOSK_HOMES.some(k => _sameComponent(a, k))) || kiosks[0] || null;
}

// The packages of the enabled input methods, from `ime list -s` (one
// `package/class` per line).
function _imePackages(imeListOut) {
  return new Set(_homeActivities(imeListOut).map(a => a.pkg));
}

// Of the packages about to be hidden, those that must be kept because they
// hold every enabled keyboard: hiding the only IME leaves the Show with no
// keyboard at all (findings §9.3 keeps com.amazon.bluestone.keyboard until
// another is installed and enabled). Empty when another IME survives.
function _imeKeep(toHide, imePackages) {
  const hiding = new Set(toHide);
  const survivors = [...imePackages].filter(p => !hiding.has(p));
  return survivors.length ? [] : [...imePackages].filter(p => hiding.has(p));
}

// Amazon packages the Show cannot do without, measured on it (2026-10-17):
// never hidden, whatever a list says, and unhidden if an earlier list hid
// them. The reason is logged when either happens.
const _CROWN_KEEP_VISIBLE = Object.freeze({
  'amazon.speech.sim': 'hidden, KnightSystemUI crash-loops 10 minutes after boot and four '
    + 'Amazon apps crash with dialogs on screen; its microphone is denied instead',
  'com.amazon.kindle.otter.oobe': 'hidden, every Settings page crashes, and with them the '
    + "Show's own Wi-Fi and Bluetooth settings",
});

// Of `pkgs`, the ones _CROWN_KEEP_VISIBLE protects.
function _keepVisible(pkgs) {
  return pkgs.filter(p => Object.hasOwn(_CROWN_KEEP_VISIBLE, p));
}

// Whether `appops get <pkg> RECORD_AUDIO` reports the microphone denied
// ("RECORD_AUDIO: ignore", optionally followed by "; time=…").
const _micDenied = out => /^\s*RECORD_AUDIO: ignore\b/m.test(out || '');

// KnightSettings on a Show never registered with Amazon has no config
// bundle: no bundle_location in this file, and every Settings activity
// crashes (FileNotFoundException: null/preference-settings.json), leaving
// the Wi-Fi and Bluetooth pages unreachable. The measured fix is one
// broadcast to its AccountPropertyChangeReceiver, which then writes the
// bundle (bundle_location read /data/user/0/com.android.settings/files/bundle_a).
const _SETTINGS_BUNDLE_PREFS =
  '/data/data/com.android.settings/shared_prefs/com.amazon.settings.CONFIG_BUNDLE_STORE.xml';
const _hasSettingsBundle = xml => /\bbundle_location\b/.test(xml || '');
// DANGER. The same receiver also handles com.amazon.dcp.sso.action.account.removed,
// and on a provisioned device that one FACTORY-RESETS the Show (KnightSettings
// DeviceUtils.resetDevice), wiping /data and EchoMuse with it. Only this exact
// action, to this explicit component, is ever sent; never a broadcast that
// could reach the receiver with any other action. Pinned by test.
const _SETTINGS_BUNDLE_BROADCAST = 'am broadcast -a com.amazon.dcp.sso.broadcast.CORPFMHasChanged '
  + '-n com.android.settings/com.amazon.settings.AccountPropertyChangeReceiver';

// The Show's own Wi-Fi and Bluetooth pages, for the kiosk's app launcher:
// the implicit android.settings.WIFI_SETTINGS intent does not resolve on
// this build, but these explicit components open the pages (measured).
const _CROWN_SETTINGS_PAGES = Object.freeze({
  'Wi-Fi':     'com.android.settings/.Settings$WifiSettingsActivity',
  'Bluetooth': 'com.android.settings/.Settings$BluetoothSettingsActivity',
});

// What a `pm hide` / `pm unhide` call said. The distinction that matters is
// between pm REFUSING and pm ANSWERING that the package is not there:
// `Unknown package: <name>` means the call worked and this build lacks the
// package (#91). Anything unrecognised counts as rejected, deliberately:
// the cost of being wrong is an Alexa package left live. Success reads
// "Package <name> new hidden state: true" (or false, for unhide).
const PM_VERDICT = Object.freeze({ DONE: 'done', ABSENT: 'absent', REJECTED: 'rejected' });
const _pmVerdict = (out) => {
  if (/new hidden state: (true|false)/.test(out || '')) return PM_VERDICT.DONE;
  if (/Unknown package/i.test(out || '')) return PM_VERDICT.ABSENT;
  return PM_VERDICT.REJECTED;
};

// The package names in `pm list packages` output.
function _listedPackages(out) {
  const pkgs = new Set();
  for (const line of (out || '').split('\n')) {
    const m = /^package:(\S+)\s*$/.exec(line.trim());
    if (m) pkgs.add(m[1]);
  }
  return pkgs;
}

// Where the server's own output goes: the Dot's tmpfs /tmp, and on the Show
// (no /tmp at all, on either image) /data/local/tmp/echomuse. Mirrors LOG in
// start_server.sh.
function _serverLog(platform) {
  return _isShow(platform) ? '/data/local/tmp/echomuse/server.log' : '/tmp/server.log';
}

// The EchoMuse app on a LineageOS Show (docs/echo-show-companion-plan.md):
// the firmware installs and starts it, and listens for it on loopback port
// _COMPANION_PORT.
const _COMPANION_PKG = 'org.echomuse.companion';
const _COMPANION_PORT = 8771;

// Kiosk Satellite (docs/echo-show-companion-plan.md §11): the kiosk whose
// plugin shows each turn full screen, installed by the firmware once
// EchoMuse holds a token for its remote API.
const _KIOSK_PKG = 'me.jxl.kiosk_satellite';

// Whether `pm path` output says Kiosk Satellite is installed, for a device
// pending approval, which reports no stats.
function _kioskOnDevice(pmPath) {
  return /^package:/m.test(pmPath || '');
}

// Whether `stats.companion` says the app is linked with its overlay allowed.
function _companionReady(companion) {
  return companion?.linked === true && companion?.overlay === true;
}

// The same two facts read on the device itself, for a device pending
// approval, which has no session and so no stats: an established connection
// from the app's uid to the firmware's port in /proc/net/tcp{,6} (`tcp`,
// whose rows carry the uid in their eighth field), and the overlay op allowed
// in `appops get` (`ops`).
function _companionOnDevice(tcp, uid, ops) {
  const port = _COMPANION_PORT.toString(16).toUpperCase().padStart(4, '0');
  const linked = /^\d+$/.test(uid) && (tcp || '').split('\n').some(line => {
    const f = line.trim().split(/\s+/);
    return f.length > 7 && f[2].endsWith(`:${port}`) && f[3] === '01' && f[7] === uid;
  });
  return { linked, overlay: /SYSTEM_ALERT_WINDOW: allow\b/.test(ops || '') };
}

// WiFi security labels, used in the network picker and in error messages.
// Module scope so WifiPanel and the wizard's step runners share one set.
const _SECURITY_LABEL = {
  wpa2: 'WPA2', wpa3: 'WPA3', open: 'Open', wep: 'WEP', enterprise: 'Enterprise',
};

// Steps, in order. One list for every device: each names the platforms it
// runs on (`on`); until connect_android has said which device this is,
// every step shows, and afterwards the other devices' steps show as not
// applicable. `crownDesc` replaces `desc` on either Echo Show image, and
// `lineageDesc` replaces both on LineageOS.
//
// Echo Dot (docs/fireos6-port.md Phase 3/4). boot-root.zip already has the
// shell at uid 0, and the only boot-time change EchoMuse makes is the one
// init .rc written live from this same Android session.
//
//  connect_android    — connect in Android mode, verify FireOS 6 + root
//  install_boot_hook  — echomuse.rc + /tmp on the RUNNING slot only    [auto]
//  install_em         — push bundled firmware + startup script        [auto]
//  wifi               — write wpa_supplicant.conf only, no live join   [inputs]
//  reboot             — snapshot the controller's row, reboot         [button]
//  reconnect          — reconnect ADB after reboot                    [button]
//  verify_service     — confirm init.svc.echomuse / init.svc.mixer    [auto]
//  confirm_link       — this controller heard the device after boot   [auto]
//  mirror_boot_hook   — only now, the same hook on the other slot     [auto]
//
// The other slot is written last, after a boot from the hooked slot has
// proven the whole chain (init, start script, Wi-Fi, controller link). A bad
// install then leaves the other slot stock, which the bootloader falls back
// to as a known-good boot instead of a second copy of the fault.
//
// Echo Show 8 (docs/echo-show-port-plan.md Phases 3/4). Already on Wi-Fi,
// joined from the device (connect_android refuses it otherwise), and one
// system partition, so no wifi and no mirror step:
//
//  connect_android    — Fire OS 6 crown + root + on Wi-Fi
//  check_home         — a non-Amazon HOME activity (the kiosk) exists  [auto]
//  install_boot_hook  — echomuse.rc, /system remounted rw then ro      [auto]
//  install_em         — push bundled firmware + startup script        [auto]
//  neutralize_alexa   — setup flags, kiosk as HOME, pm hide the lists  [button]
//  reboot, reconnect, verify_service (echomuse + audioserver), confirm_link
//
// Echo Show 8 on LineageOS 18.1 (docs/echo-show-lineage-plan.md Phase L4).
// No Amazon apps, so no kiosk check and no neutralisation; the kiosk is the
// owner's. adbd starts as uid 2000 after every boot, so connect_android and
// reconnect restart it as root (`adb root`, allowed by "Rooted debugging"):
//
//  connect_android    — LineageOS 18.1 crown + our libasp + root + Wi-Fi
//  install_boot_hook  — echomuse.rc + addon.d script, / remounted rw then ro
//  install_em         — push bundled firmware + startup script        [auto]
//  reboot, reconnect, verify_service (echomuse + audioserver), confirm_link
//  verify_companion   — the firmware's app linked, overlay allowed     [auto]
//  kiosk_plugin       — Kiosk Satellite installed: its password → token [auto, then input]
//
// `id` is also what a diagnostics upload names the failed step by.
const _ALL_PLATFORMS = Object.values(WIZARD_PLATFORM);
const _WIZARD_STEPS = [
  { id: 'connect_android',   label: 'Connect Device',    on: _ALL_PLATFORMS,
    desc: 'Connect the Echo device via USB, booted and rooted. Echo Dot: Fire OS 6 rooted via boot-root.zip, "AEOBC" in the USB picker. Echo Show 8: rooted via boot-root.img, already on Wi-Fi (joined on the device), your kiosk app installed. Echo Show 8 on LineageOS 18.1: our build, "USB debugging" and "Rooted debugging" switched on in Developer options, already on Wi-Fi.' },
  { id: 'check_home',        label: 'Check Kiosk',       on: [WIZARD_PLATFORM.CROWN],
    desc: "Confirm a home-screen app other than Amazon's is installed (your kiosk). Fully Kiosk Browser ships its home screen switched off; the wizard switches it on (pm enable), as Fully's own launcher setting would. Amazon's launchers are hidden later; with no other home screen the display would stay blank." },
  { id: 'install_boot_hook', label: 'Install Boot Hook', on: _ALL_PLATFORMS,
    desc: 'Write the echomuse init service to /system/etc/init/ on the system slot the Dot is running. The other slot is left stock until this one is proven.',
    crownDesc: 'Write the echomuse init service to /system/etc/init/, remounting /system read-write for the write and read-only again after it.',
    lineageDesc: 'Write the echomuse init service to /system/etc/init/ and its addon.d script to /system/addon.d/ (a LineageOS update keeps the service), remounting / read-write for the writes and read-only again after them.' },
  { id: 'install_em',        label: 'Install EchoMuse',  on: _ALL_PLATFORMS,
    desc: 'Push the EchoMuse firmware bundled with this controller, and its startup script, to the device.' },
  { id: 'wifi',              label: 'Configure WiFi',    on: [WIZARD_PLATFORM.DOT],
    desc: "Write the WiFi network into wpa_supplicant.conf — the firmware brings WiFi up itself at boot." },
  { id: 'neutralize_alexa',  label: 'Neutralise Alexa',  on: [WIZARD_PLATFORM.CROWN],
    desc: "Mark setup complete, repair Settings, make your kiosk the home screen, hide Amazon's Alexa apps (Tier 1) with pm hide and deny the microphone to the Amazon apps that must stay (Alexa's wake engine, Amazon's keyboard) — all reversible. Tier 2 is optional." },
  { id: 'reboot',            label: 'Reboot',            on: _ALL_PLATFORMS,
    desc: 'Reboot device to start the echomuse service.' },
  { id: 'reconnect',         label: 'Reconnect',         on: _ALL_PLATFORMS,
    desc: 'Re-connect ADB as soon as the device appears as "AEOBC" in the USB picker.',
    crownDesc: 'Re-connect ADB as soon as the Echo Show appears in the USB picker.',
    lineageDesc: 'Re-connect ADB as soon as the Echo Show appears in the USB picker. adbd starts unprivileged after a reboot, so the wizard restarts it as root again.' },
  { id: 'verify_service',    label: 'Verify Service',    on: _ALL_PLATFORMS,
    desc: "Confirm the echomuse service and Amazon's mixer are both running.",
    crownDesc: "Confirm the echomuse service and Android's audioserver are both running." },
  { id: 'confirm_link',      label: 'Confirm Link',      on: _ALL_PLATFORMS,
    desc: 'Wait for the device to join WiFi and reach this controller (a new device shows up pending approval).',
    crownDesc: 'Wait for the Echo Show to reach this controller over its Wi-Fi (a new device shows up pending approval).' },
  { id: 'mirror_boot_hook',  label: 'Mirror Boot Hook',  on: [WIZARD_PLATFORM.DOT],
    desc: 'Now that the hooked slot is proven, write the same hook to the other system slot, so a bootloader fallback still runs EchoMuse.' },
  { id: 'verify_companion',  label: 'Verify App',        on: [WIZARD_PLATFORM.CROWN_LINEAGE],
    desc: "Wait for the EchoMuse app, which the firmware installs and starts, to link to the firmware with its overlay allowed: the light bar and touch zones along the bottom of the screen." },
  { id: 'kiosk_plugin',      label: 'Kiosk Satellite',   on: [WIZARD_PLATFORM.CROWN_LINEAGE],
    desc: "If Kiosk Satellite is installed, log in to its remote API once so the firmware can install the EchoMuse plugin that shows each request and answer full screen. Optional: skip it and the light bar still works." },
];

// Whether a step runs for `platform` (null before connect_android: all do).
function _stepApplies(step, platform) {
  return !platform || step.on.includes(platform);
}

// The ids of the steps one device runs, in order.
function _stepsFor(platform) {
  return _WIZARD_STEPS.filter(s => _stepApplies(s, platform)).map(s => s.id);
}

// The first step at or after index `from` that runs for `platform`
// (_WIZARD_STEPS.length past the last), and the last one it runs.
function _nextStep(from, platform) {
  let i = from;
  while (i < _WIZARD_STEPS.length && !_stepApplies(_WIZARD_STEPS[i], platform)) i++;
  return i;
}
function _lastStep(platform) {
  let i = _WIZARD_STEPS.length - 1;
  while (i > 0 && !_stepApplies(_WIZARD_STEPS[i], platform)) i--;
  return i;
}

// A step's description for the device at hand.
function _stepDesc(step, platform) {
  if (platform === WIZARD_PLATFORM.CROWN_LINEAGE && step.lineageDesc) return step.lineageDesc;
  return _isShow(platform) && step.crownDesc ? step.crownDesc : step.desc;
}

// Steps that establish an ADB connection rather than needing one, steps
// that need no input, and steps with their own input panel.
const CONNECT_STEPS = new Set(['connect_android', 'reconnect']);
const AUTO_STEPS = new Set(['check_home', 'install_boot_hook', 'install_em', 'verify_service',
                            'confirm_link', 'mirror_boot_hook', 'verify_companion', 'kiosk_plugin']);
const INPUT_STEPS = new Set(['wifi']);

// ── WifiPanel ──

function WifiPanel({ adb, wifiSsid, setWifiSsid, wifiPsk, setWifiPsk, onScan, networks, onConnect, onSkip, onAbort }) {
  const [scanning, setScanning] = useState(false);
  const [showPsk, setShowPsk]   = useState(false);

  async function doScan() {
    setScanning(true);
    await onScan();
    setScanning(false);
  }

  return (
    <div style={{ marginBottom: 12, display: 'flex', flexDirection: 'column', gap: 10 }}>

      {/* Scan row */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <Pill small onClick={doScan} disabled={scanning || !adb}>
          {scanning ? 'Scanning…' : 'Scan for networks'}
        </Pill>
        {networks.length > 0 && (
          <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)' }}>
            {networks.length} network{networks.length !== 1 ? 's' : ''} found
          </span>
        )}
      </div>

      {/* Network list */}
      {networks.length > 0 && (
        <div style={{
          border: '1px solid var(--border-soft)', borderRadius: 6, overflow: 'hidden',
          maxHeight: 140, overflowY: 'auto',
        }}>
          {networks.map(n => {
            // A network the radio cannot join is shown, greyed, with the
            // reason. Hiding it would leave someone hunting for a network
            // they can see on their phone; offering it silently costs them
            // twenty seconds of SCANNING and no explanation.
            const blocked = n.blocker;
            return (
            <div key={n.ssid}
              {...pressable(() => setWifiSsid(n.ssid), { disabled: !!blocked, selected: wifiSsid === n.ssid })}
              title={blocked || ''}
              style={{
                padding: '6px 10px', display: 'flex', justifyContent: 'space-between', alignItems: 'center',
                background: wifiSsid === n.ssid ? 'rgba(64,88,120,0.12)' : 'transparent',
                borderBottom: '1px solid var(--sunken)',
                cursor: blocked ? 'not-allowed' : 'pointer',
                opacity: blocked ? 0.5 : 1,
              }}>
              <span style={{ fontFamily: MONO, fontSize: 11, color: wifiSsid === n.ssid ? 'var(--accent)' : 'var(--text)' }}>
                {n.ssid}
              </span>
              <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)' }}>
                {[n.securityLabel, (n.bands || []).join('+'), `${n.signal} dBm`]
                  .filter(Boolean).join(' · ')}
              </span>
            </div>
          );})}
        </div>
      )}

      {/* Manual SSID entry */}
      <div>
        <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--text2)', letterSpacing: '0.08em', marginBottom: 4 }}>SSID</div>
        <input
          type="text" value={wifiSsid} onChange={e => setWifiSsid(e.target.value)} aria-label="SSID"
          placeholder="Select above or type network name"
          style={{ width: '100%', boxSizing: 'border-box' }}
        />
      </div>

      {/* Password */}
      <div>
        <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--text2)', letterSpacing: '0.08em', marginBottom: 4 }}>PASSWORD</div>
        <div style={{ display: 'flex', gap: 6 }}>
          <input
            type={showPsk ? 'text' : 'password'} value={wifiPsk} onChange={e => setWifiPsk(e.target.value)} aria-label="Password"
            placeholder="WPA passphrase" style={{ flex: 1, boxSizing: 'border-box' }}
            onKeyDown={e => e.key === 'Enter' && wifiSsid && onConnect()}
          />
          <button type="button" onClick={() => setShowPsk(v => !v)} style={{
            background: 'var(--hairline)', border: '1px solid var(--border-soft)', borderRadius: 6,
            fontFamily: MONO, fontSize: 9, color: 'var(--muted)',
            padding: '0 8px', cursor: 'pointer', flexShrink: 0,
          }}>{showPsk ? 'hide' : 'show'}</button>
        </div>
      </div>

      {/* Actions */}
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <Pill accent onClick={onConnect} disabled={!wifiSsid || !adb}>Connect</Pill>
        <Pill small onClick={onSkip}>Skip (already connected)</Pill>
        <Pill small danger onClick={onAbort}>Abort provisioning</Pill>
      </div>
    </div>
  );
}

// ── ReadoptPrompt ──
//
// connect_android's question when the Dot's serial is already registered.
// Everything the controller keeps for a Dot (name, settings, approval, the
// ESPHome port and so its Home Assistant device, the alarm calendar) is
// keyed by ro.serialno. That comes from the Dot's idme area and survives a
// reflash, so keeping the row is what lets a reflashed Dot come back as
// itself rather than as a device to set up again.
function ReadoptPrompt({ serial, device, onKeep, onAbort }) {
  const name = device.label || device.device_id;
  const text = { fontFamily: MONO, fontSize: 11, color: 'var(--text)', lineHeight: 1.6 };
  return (
    <div role="group" aria-label={`Already registered as ${name}`} style={{
      marginBottom: 12, padding: '12px 14px', display: 'flex', flexDirection: 'column', gap: 10,
      border: '1px solid var(--warn)', borderRadius: 8, background: 'var(--hairline)',
    }}>
      <div style={{ fontFamily: SANS, fontSize: 13, fontWeight: 600, color: 'var(--text)' }}>
        Already registered as "{name}"
      </div>
      <div style={text}>
        Serial {serial} (Fire OS 6) is this
        controller's {device.approved ? 'approved' : 'pending'} device "{name}":{' '}
        {device.connected ? 'online now' : `last seen ${relTime(device.last_seen)}`},
        EchoMuse firmware {device.firmware_ver || 'unknown'}.
      </div>
      <div style={text}>
        {device.approved
          ? <>Continue reinstalls EchoMuse and keeps that device: it rejoins as "{name}" with
              its settings, Home Assistant device and alarms.</>
          : <>Continue reinstalls EchoMuse and keeps that entry: it rejoins as "{name}",
              still waiting for approval.</>}
      </div>
      {device.connected && (
        <div style={{ ...text, color: 'var(--warn)' }}>
          It is connected to this controller right now, so EchoMuse is already running on it.
          Continuing reinstalls over that install.
        </div>
      )}
      <div style={text}>Abort stops here; nothing has been written to the Dot.</div>
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <Pill accent onClick={onKeep}>Continue as "{name}"</Pill>
        <Pill danger onClick={onAbort}>Abort</Pill>
      </div>
    </div>
  );
}

function ProvisionWizard({ onClose, knownDevices }) {
  const [step, setStep]         = useState(0);
  const [stepState, setStepState] = useState(_WIZARD_STEPS.map(() => STEP_STATE.PENDING));
  const [log, setLog]           = useState([]);
  const transcriptRef           = useRef([]);
  const [running, setRunning]   = useState(false);
  // `adb` renders; adbRef is the live handle for code already in flight. A
  // step is an async function from the render it started in, so its `adb`
  // is whatever the handle was when the button was pressed — null for a
  // connect step, however many handles it has opened since.
  const [adb, setAdbState]      = useState(null);
  const adbRef                  = useRef(null);
  const setAdb = c => { adbRef.current = c; setAdbState(c); };
  const steps  = _WIZARD_STEPS;
  const stepId = steps[step]?.id;
  const [wifiSsid, setWifiSsid] = useState('');
  const [wifiPsk, setWifiPsk]   = useState('');
  const [wifiNetworks, setWifiNetworks] = useState([]);
  // Which device connect_android found (a WIZARD_PLATFORM; null before it
  // has). platformRef for code already in flight, as adbRef.
  const [platform, setPlatformState] = useState(null);
  const platformRef = useRef(null);
  const setPlatform = p => { platformRef.current = p; setPlatformState(p); };
  // neutralize_alexa's opt-in: hide Tier 2 too (off by default).
  const [hideTier2, setHideTier2] = useState(false);
  const [duplicateDeviceId, setDuplicateDeviceId] = useState(null);
  // connect_android's open question when the Dot's serial is already
  // registered ({serial, device}); readoptAnswer resolves it.
  const [readoptAsk, setReadoptAsk] = useState(null);
  const readoptAnswer = useRef(null);
  // kiosk_plugin's open password prompt ({serial, reason}); kioskAnswer ends it.
  const [kioskAsk, setKioskAsk] = useState(null);
  const kioskAnswer = useRef(null);
  // The registered device this run keeps, once the operator chose to.
  const [readopting, setReadopting] = useState(null);
  const [progress, setProgress] = useState(null);
  const [diagnostics, setDiagnostics] = useState(null);
  const logRef = useRef(null);
  // Bumped whenever the step in flight is abandoned — by the cable being
  // pulled, or by the operator cancelling. A step that later settles compares
  // this against the value it captured and drops its result on the floor
  // rather than marking a step done that nobody is waiting on any more.
  const stepEpoch = useRef(0);
  // Set immediately before a teardown WE asked for — the reboot step, or
  // connect_android closing its session on an abort. Without it the
  // disconnect listener races the rest of the step: observed on a real run,
  // the event landed while `running` was still true, and a step that had
  // just worked was marked failed with "the step was abandoned".
  const expectDisconnect = useRef(false);
  // confirm_link's pre-reboot snapshot ({serial, lastSeen}), taken by the
  // reboot step.
  const linkBaseline = useRef(null);

  // Errors thrown by an in-flight transfer when the device goes away. These
  // race the disconnect event and can arrive first, in which case the catch in
  // runStep would report a WebUSB internal as though it were a provisioning
  // failure and then go and probe the absent device for diagnostics.
  const _isDisconnectError = (e) =>
    /disconnect|transferOut|transferIn|NetworkError|device was lost/i.test(
      e?.message || '');

  function addLog(msg, type = LOG_TONE.INFO) {
    // 200 lines truncated a normal successful run — the transcript above the
    // fold is exactly the part you need when a late step fails for an early
    // reason, and it was being thrown away. A whole provision is ~300 lines,
    // so this holds several runs' worth; it is text in memory, not a cost
    // worth optimising.
    //
    // Mirrored in a ref for the same reason as adbRef: the diagnostics upload
    // runs inside a step, where `log` is the transcript from before the step
    // began — without its own output, which is the part that failed.
    transcriptRef.current = [...transcriptRef.current, { msg, type }].slice(-5000);
    setLog(transcriptRef.current);
    setTimeout(() => { if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight; }, 30);
  }
  function markStep(i, st) { setStepState(s => { const n = [...s]; n[i] = st; return n; }); }

  // A provisioning payload from the controller, as a Response. The step
  // runners report e.message, so a refusal becomes an Error naming the HTTP
  // status (`status` kept on it); `what` completes the sentence.
  async function fetchProvision(path, what = '', init) {
    try {
      return await API.request(path, init);
    } catch (e) {
      if (e instanceof Error) throw e;          // the network, not the controller
      const err = new Error(`Controller returned ${e.status}${what ? ` ${what}` : ''}`);
      err.status = e.status;
      throw err;
    }
  }

  // Abandon whatever step is in flight.
  //
  // The ADB calls a step awaits do NOT reliably reject when the device goes
  // away: `shell` waits on a stream reader that simply never produces, so the
  // step neither resolves nor throws. `running` stays true, every button here
  // is gated on `!running`, and the wizard sits there looking busy with no
  // Retry, no Reconnect and no way out but reloading the page and losing the
  // transcript. Observed by pulling the cable during Configure WiFi.
  //
  // A timeout on `shell` would be the wrong instrument: confirm_link waits
  // three minutes and a firmware push takes as long as the cable allows, so
  // any timeout loose enough to be safe is too loose to be useful.
  // `idx` defaults to the step on screen, which is what the disconnect listener
  // wants. runStep passes its own index explicitly: the two are the same in
  // practice, but the caller that knows should say so rather than rely on it.
  function abandonStep(reason, idx = step) {
    stepEpoch.current++;
    // A step waiting on the re-adoption question unwinds as an abort.
    answerReadopt(false);
    answerKiosk(false);
    setRunning(false);
    markStep(idx, STEP_STATE.ERROR);
    addLog(reason, LOG_TONE.ERROR);
  }

  // connect_android awaits the operator's answer to ReadoptPrompt. Abandoning
  // the step or closing the wizard answers "abort", so the step closes its
  // ADB session rather than holding it, which would leave the next connect
  // hanging at "Authenticating ADB…".
  function askReadopt(ask) {
    return new Promise(resolve => {
      readoptAnswer.current = resolve;
      setReadoptAsk(ask);
    });
  }
  function answerReadopt(keep) {
    const resolve = readoptAnswer.current;
    readoptAnswer.current = null;
    setReadoptAsk(null);
    if (resolve) resolve(keep);
  }
  useEffect(() => () => answerReadopt(false), []);

  // The browser knows the cable was pulled; nothing was listening.
  //
  // Matched on serial so unplugging an unrelated USB device does not abort a
  // provision. When either serial is unavailable the event is treated as ours:
  // a spurious abort costs a Retry, and a missed one costs the hang above.
  useEffect(() => {
    if (!navigator.usb) return;
    const onDisconnect = (e) => {
      if (expectDisconnect.current) {
        // A reboot we asked for. Consumed rather than left set, so the next
        // unexpected disconnect is still reported.
        expectDisconnect.current = false;
        return;
      }
      if (!adb && !running) return;
      const theirs = adb?.serial && e.device?.serialNumber
                   && e.device.serialNumber !== adb.serial;
      if (theirs) return;
      setAdb(null);
      if (running) {
        abandonStep('Device disconnected. The step was abandoned — reconnect and retry.');
      } else {
        addLog('Device disconnected.', LOG_TONE.WARN);
      }
    };
    navigator.usb.addEventListener('disconnect', onDisconnect);
    return () => navigator.usb.removeEventListener('disconnect', onDisconnect);
  }, [adb, running, step]);

  // What to ask the device when a step fails (#87).
  //
  // Diagnosing #79 and #82 each took several rounds of asking the reporter to
  // run `getprop` and `wpa_cli` by hand, and by the time they answered the
  // device had usually been retried or rebooted, so the state at the moment of
  // failure was gone. This collects it while it is still true.
  //
  // Everything here is READ-ONLY and cheap. It runs on a device that has just
  // failed something, which is the worst moment to be issuing commands that
  // change anything, and it must not turn one failure into two.
  //
  // The keys must match `ProvisionProbe` in em_support.py, which drops any
  // name it does not recognise — a probe added here and not there collects
  // output that is silently discarded. `tests/test_support.py` pins the pair.
  //
  // Sent RAW. The controller does the redaction, because those rules and their
  // tests live in em_support.py and a second copy here would drift from them
  // without anyone noticing until a file carried an SSID.
  //
  // The common probes, then each device's own: the Dot's are asked of every
  // device connect_android has not classified, as before the Show existed.
  const _PROVISION_PROBES = {
    props:         'getprop',
    root:          'id 2>&1',
    selinux:       'getenforce 2>&1',
    storage:       'df /data 2>&1',
    services:      'getprop | grep init.svc',
    echomuse_service: 'getprop init.svc.echomuse',
    echomuse_rc:      'cat /system/etc/init/echomuse.rc 2>&1',
  };
  const _PROVISION_PROBES_DOT = {
    wpa_status:    'wpa_cli -p /data/misc/wifi/sockets -i wlan0 status 2>&1',
    wpa_scan:      'wpa_cli -p /data/misc/wifi/sockets -i wlan0 scan_results 2>&1',
    wpa_caps:      'wpa_cli -p /data/misc/wifi/sockets -i wlan0 get_capability key_mgmt 2>&1',
    processes:     "ps | grep -E 'wpa_supplicant|SmartHomeWifid' | grep -v grep",
    data_property: 'ls /data/property 2>&1',
    // Service state and active slot (docs/fireos6-port.md Phase 4).
    mixer_service:    'getprop init.svc.mixer',
    mixer_streams:    'ls -l /data/mixer_streams 2>&1',
    slot_suffix:      'getprop ro.boot.slot_suffix',
  };
  // docs/echo-show-port-plan.md Phase 4: the audio service, the kiosk as
  // HOME, how much is hidden and which tiers, setup state, the privacy
  // mute, and whether Android is on Wi-Fi.
  const _PROVISION_PROBES_CROWN = {
    audioserver_service: 'getprop init.svc.audioserver',
    home_activities: _HOME_QUERY,
    home_resolved:   _HOME_RESOLVE,
    hidden_packages: 'echo installed=$(pm list packages -u 2>/dev/null | wc -l) visible=$(pm list packages 2>/dev/null | wc -l)',
    crown_tiers:     'cat /data/local/etc/echomuse/crown_tiers 2>&1',
    provisioned:     'echo device_provisioned=$(settings get global device_provisioned) user_setup_complete=$(settings get secure user_setup_complete)',
    amazon_gating:   'cat /sys/devices/platform/amazon-gating/state 2>&1',
    wlan:            'ip -4 addr show wlan0 2>&1',
    mic_deny:        'while read -r p _; do case "$p" in ""|"#"*) ;; *) echo "$p: $(appops get $p RECORD_AUDIO </dev/null 2>&1)" ;; esac; done < /data/local/etc/echomuse/crown_mic_deny_packages.txt 2>&1',
    settings_bundle: `grep -o bundle_location ${_SETTINGS_BUNDLE_PREFS} 2>&1`,
  };
  // docs/echo-show-lineage-plan.md Phase L4: the audio service, whether
  // Android is on Wi-Fi, the privacy mute, which libasp.so this build ships
  // (ours: the NS6571 AFE), and whether the addon.d script that keeps the
  // boot hook across an update is there. ro.lineage.version comes with the
  // props probe, init.svc.echomuse with echomuse_service.
  const _PROVISION_PROBES_LINEAGE = {
    audioserver_service: 'getprop init.svc.audioserver',
    wlan:            'ip -4 addr show wlan0 2>&1',
    amazon_gating:   'cat /sys/devices/platform/amazon-gating/state 2>&1',
    libasp_sha1:     'sha1sum /system/lib/libasp.so 2>&1',
    addond_script:   `ls -l ${_ADDOND_PATH} 2>&1`,
  };
  const _PLATFORM_PROBES = {
    [WIZARD_PLATFORM.CROWN]: _PROVISION_PROBES_CROWN,
    [WIZARD_PLATFORM.CROWN_LINEAGE]: _PROVISION_PROBES_LINEAGE,
  };

  async function collectProvisionDiagnostics(c, stepIdx, err) {
    const probes = {};
    const asked = { ..._PROVISION_PROBES,
                    ...(_PLATFORM_PROBES[platformRef.current] || _PROVISION_PROBES_DOT) };
    for (const [name, cmd] of Object.entries(asked)) {
      try {
        // Bounded per probe. The device has just failed something and may be
        // half gone; without this, one unanswered command hangs the whole
        // collection and the operator gets nothing at all.
        probes[name] = await Promise.race([
          c.shell(cmd),
          new Promise((_, rej) => setTimeout(() => rej(new Error('timeout')), 8000)),
        ]);
      } catch (e) {
        probes[name] = `<probe failed: ${e.message}>`;
      }
    }
    return API.post('/api/provision/diagnostics', {
      step:  steps[stepIdx]?.id || String(stepIdx),
      error: err?.message || '',
      probes,
      transcript: transcriptRef.current.map(l => l.msg),
      selected_ssid: wifiSsid || null,
    });
  }

  async function captureDiagnostics(stepIdx, err) {
    const c = adbRef.current;
    if (!c) {
      // No connection means no probes, and a button that downloads a file
      // containing nothing but the error would be worse than no button.
      addLog('No ADB connection, so device state could not be captured.', LOG_TONE.WARN);
      return;
    }
    addLog('Capturing device state for diagnostics…');
    try {
      setDiagnostics(await collectProvisionDiagnostics(c, stepIdx, err));
      addLog('Device state captured — "Download diagnostics" below.', LOG_TONE.OK);
    } catch (e) {
      // Never let the diagnostic path bury the real failure.
      addLog(`Could not capture device state: ${e.error || e.message}`, LOG_TONE.WARN);
    }
  }

  function downloadDiagnostics() {
    downloadBlob(new Blob([JSON.stringify(diagnostics, null, 2)], { type: 'application/json' }),
                 `echomuse-provision-${new Date().toISOString().slice(0,19).replace(/[:T]/g,'')}.json`);
  }

  // ── Step runners ──

  // decision 11 of docs/echo-show-lineage-plan.md: LineageOS's adbd runs as
  // uid 2000 after every boot, even with "Rooted debugging" on, and adb's
  // `root:` service restarts it as uid 0 (Client.root). The restart drops
  // the USB connection and re-enumerates the device, so the wizard expects
  // that disconnect and finds the same device again by its serial
  // (Client.reopen). Returns the client to use from here on and adbd's
  // answer, which a refusal quotes.
  async function restartAdbdAsRoot(c) {
    addLog('The adb shell is not root: asking adbd to restart as root (adb root)…');
    expectDisconnect.current = true;
    const reply = await c.root();
    addLog(`  adbd: ${reply === null ? '(no answer: the connection dropped)' : reply || '(no answer)'}`);
    if (reply !== null && !/restarting adbd as root/i.test(reply)) {
      // No restart, so no disconnect is coming: the next one is real.
      expectDisconnect.current = false;
      return { client: c, reply };
    }
    setAdb(null);
    const next = await c.reopen(addLog);
    next._log = msg => addLog(`  adb: ${msg}`);
    setAdb(next);
    addLog('Reconnected to the restarted adbd.', LOG_TONE.OK);
    return { client: next, reply: reply || '' };
  }

  async function runConnectAndroid() {
    // requestDevice() handles USB open + ADB auth in one call.
    let c = await _ADB.Client.requestDevice(addLog);
    c._log = msg => addLog(`  adb: ${msg}`);
    setAdb(c);
    const model   = await c.shell('getprop ro.product.model');
    const release = await c.shell('getprop ro.build.version.release');
    const name    = await c.shell('getprop ro.product.name');
    const device  = await c.shell('getprop ro.product.device');
    const serial  = await c.shell('getprop ro.serialno') || await c.shell('getprop ro.boot.serialno');
    const fwBuild = await c.shell('getprop ro.build.version.incremental');
    const fwName  = await c.shell('getprop ro.build.version.name');
    const display = await c.shell('getprop ro.build.display.id');
    addLog(`Model: ${model || '(unknown)'}  Build: Android ${release}  Codename: ${name || '(unknown)'}  Serial: ${serial || '(unknown)'}`);
    addLog(`OS: ${fwName || '(unknown)'}  ${fwBuild || ''}`);
    // The adb shell's uid tells a rooted device apart from one that
    // boot-root has not been flashed onto yet. On LineageOS, a Show running
    // our build (its libasp.so) gets adbd restarted as root first; a refusal
    // there means "Rooted debugging" is off.
    let uid = await c.shell('id -u');
    const lineageVersion = await c.shell('getprop ro.lineage.version');
    let libaspSha1 = '', rootReply = '';
    if (lineageVersion && (name === 'crown' || device === 'crown')) {
      libaspSha1 = await c.shell('sha1sum /system/lib/libasp.so 2>&1');
      addLog(`LineageOS ${lineageVersion}  libasp.so: ${libaspSha1.split(/\s+/)[0] || '(unreadable)'}`);
      if (uid !== '0' && _isOurLibasp(libaspSha1)) {
        ({ client: c, reply: rootReply } = await restartAdbdAsRoot(c));
        uid = await c.shell('id -u');
      }
    }
    const kind = classifyDevice({ release, productName: name, productDevice: device, uid,
                                  lineageVersion, libaspSha1, rootReply });
    setPlatform(kind);
    const untested = untestedBuild(kind, [fwName, display]);
    if (untested) addLog(untested, LOG_TONE.WARN);
    if (kind === WIZARD_PLATFORM.DOT && model && !model.toLowerCase().includes('amazon')
        && !name.toLowerCase().includes('biscuit')) {
      addLog('Warning: device may not be an Echo Dot 2nd gen — proceeding anyway.', LOG_TONE.WARN);
    }
    if (_isShow(kind)) {
      // The Show's Wi-Fi is Android's, joined on the device; EchoMuse never
      // configures it, so there is no Wi-Fi step to fall back on.
      const addr = _wlanAddress(await c.shell('ip -4 addr show wlan0 2>&1'));
      if (!addr) {
        throw new Error(
          'This Echo Show is not on Wi-Fi. Join your network on the Show itself (the setup '
          + "screen's Wi-Fi page, or Settings → Network once setup is done), then Retry. "
          + 'EchoMuse does not configure Wi-Fi on the Echo Show.');
      }
      addLog(`On Wi-Fi: wlan0 has ${addr}, joined from the device.`, LOG_TONE.OK);
    }

    // A serial this controller already knows is a Dot coming back: reflashed
    // or being reinstalled. device_id IS ro.serialno (the firmware's
    // client.GetSerialNo; em_api.py _merge_device), and every row the
    // controller keeps for a Dot is keyed by it, so the operator chooses:
    // reinstall and keep that device, or stop. The install steps skip or
    // replace what is already on the Dot. Deleting the device and retrying
    // is still how to start over as new.
    const match = serial ? (knownDevices || []).find(d => d.device_id === serial) : null;
    if (match) {
      const name = match.label || match.device_id;
      addLog(`Serial ${serial} is already registered with this controller as "${name}".`, LOG_TONE.WARN);
      if (!await askReadopt({ serial, device: match })) {
        // Close the live ADB session before throwing — otherwise the
        // transport stays open and _lastUsbDevice keeps pointing at it.
        // On retry, requestDevice() disconnects the WebUSB interface but
        // the device-side adbd session was never told to close, so the
        // next Transport.authenticate() races a half-torn-down session
        // and hangs at "Authenticating ADB…". Mirrors the clean-exit
        // close()/setAdb(null) a few lines below.
        //
        // Closing the transport can surface as a USB disconnect, and this
        // path is about to throw a message the operator needs to read. The
        // listener must not overwrite it with "the step was abandoned".
        expectDisconnect.current = true;
        await c.close();
        setAdb(null);
        const err = new Error(
          `Stopped before writing anything to the device. Retry to be asked again, or delete `
          + `"${name}" from the controller to provision it as a new device.`
        );
        err.matchedDeviceId = match.device_id;
        throw err;
      }
      setReadopting(match);
      setDuplicateDeviceId(null);
      addLog(`Re-adopting: once installed, the device rejoins as "${name}" with its existing settings.`,
             LOG_TONE.OK);
    }

    // boot-root already has the shell at uid 0, and the one boot-time
    // change (install_boot_hook) is written live from this same session.
    // Stay connected.
    addLog(kind === WIZARD_PLATFORM.CROWN_LINEAGE
      ? `Echo Show 8 confirmed (crown, LineageOS ${lineageVersion}, our libasp.so, adb root, on Wi-Fi). Continuing…`
      : kind === WIZARD_PLATFORM.CROWN
      ? 'Echo Show 8 confirmed (crown, Fire OS 6, adb root, on Wi-Fi). Continuing…'
      : 'FireOS 6 confirmed (biscuit_puffin, rooted via boot-root.zip). Continuing…', LOG_TONE.OK);
    return c;
  }

  async function runReboot(c) {
    // confirm_link's baseline: this device's controller row as it stands
    // before the boot whose link it has to prove (absent for a new device).
    const serial = (await c.shell('getprop ro.serialno')).trim();
    const row = (await API.get('/api/devices')).find(x => x.device_id === serial);
    linkBaseline.current = { serial, lastSeen: row ? row.last_seen : null };
    addLog('Sending reboot command…');
    expectDisconnect.current = true;
    try { await c.shell('reboot'); } catch {}
    await c.close();
    setAdb(null);
    // The only thing worth waiting for here is the device appearing over
    // USB: verify_service polls for the service itself.
    addLog('Device rebooting to Android. Click Reconnect as soon as it appears in the USB picker — '
         + 'the wizard waits for Android to finish booting by itself.', LOG_TONE.WARN);
    return null;
  }

  async function runReconnect() {
    let c = await _ADB.Client.requestDevice(addLog);
    c._log = msg => addLog(`  adb: ${msg}`);
    setAdb(c);
    addLog('ADB connected.', LOG_TONE.OK);
    // `adb root` does not survive a reboot on LineageOS: adbd comes back
    // unprivileged, so restart it as root again, as connect_android did. The
    // service checks read properties and need no root; the server log and
    // the diagnostics do, so a refusal here is a warning.
    if (platformRef.current === WIZARD_PLATFORM.CROWN_LINEAGE && (await c.shell('id -u')) !== '0') {
      const { client, reply } = await restartAdbdAsRoot(c);
      c = client;
      if ((await c.shell('id -u')) !== '0') {
        addLog(`adbd stayed unprivileged (${reply || 'no answer'}), so the server log cannot be read `
             + 'from here. Check "Rooted debugging" in Developer options.', LOG_TONE.WARN);
      }
    }
    return c;
  }

  // Re-establish ADB from ANY step, without running that step.
  //
  // Reconnecting used to be reachable only from the three connection steps, so
  // unplugging the cable anywhere else was unrecoverable inside the wizard:
  // the handle held in `adb` is dead, Retry hands that dead handle straight
  // back to the step, and the operator is left on a step that cannot succeed
  // with no way to get a working connection. Reported in #91 as being stuck on
  // step 9 with no way to press Retry or reach the USB picker.
  //
  // Unplugging a device is a normal thing to do when something has gone wrong,
  // so it must not be a state the wizard cannot leave.
  async function reconnectAdb() {
    setRunning(true);
    try {
      // Drop the old handle first. WebUSB will not hand out a second claim on
      // the same interface, so a stale client that still believes it is open
      // makes the picker fail with a permissions error that reads as though
      // the operator picked the wrong device.
      if (adb) { try { await adb.close(); } catch {} setAdb(null); }
      addLog('Reconnecting to the device…');
      await runReconnect();
    } catch (e) {
      addLog(`Reconnect failed: ${e.message}`, LOG_TONE.ERROR);
    }
    setRunning(false);
  }

  async function scanWifi(c) {
    addLog('Scanning for WiFi networks…');
    // There is no `svc wifi` to enable first — Amazon's wifisvc already has
    // p2p_supplicant running at this same socket path before the wizard
    // ever connects.
    //
    // wpa_cli on this build needs BOTH -p (socket dir, since
    // ctrl_interface=/data/misc/wifi/sockets is non-default) AND -i wlan0
    // (interface) explicitly — without -p it sometimes silently works by
    // luck of default-selecting the only non-p2p interface, but once other
    // client sockets exist in the dir (e.g. from system/smarthome
    // processes) it mis-selects one of those instead and fails with
    // "Operation not permitted". -i alone without -p fails outright with
    // "Failed to connect to non-global ctrl_ifname". Always pass both.
    await c.shell('wpa_cli -p /data/misc/wifi/sockets -i wlan0 scan');
    await new Promise(r => setTimeout(r, 3000));
    const raw = await c.shell('wpa_cli -p /data/misc/wifi/sockets -i wlan0 scan_results');
    addLog('Scan complete.');
    return parseScanResults(raw);
  }

  // Parse wpa_cli scan_results: bssid / frequency / signal / flags / ssid.
  //
  // The frequency and flags columns used to be read and thrown away, which
  // is how a WPA3 network came to look identical to a WPA2 one in the picker
  // and produced twenty seconds of SCANNING with an error that named nothing
  // (#82). They are the two facts that explain most association failures on
  // this hardware, so they are kept.
  function parseScanResults(raw) {
    const networks = [];
    for (const line of (raw || '').split('\n')) {
      const parts = line.split('\t');
      if (parts.length < 5) continue;
      const ssid = parts[4].trim();
      if (!ssid || ssid === 'SSID') continue;
      const freq   = parseInt(parts[1], 10);
      const signal = parseInt(parts[2], 10);
      const flags  = parts[3] || '';
      const band   = freq >= 4900 ? '5GHz' : (freq > 0 ? '2.4GHz' : '');

      const existing = networks.find(n => n.ssid === ssid);
      if (!existing) {
        networks.push({ ssid, signal, freq, flags, bands: band ? [band] : [] });
      } else {
        // Same SSID on more than one AP or band. Keep the strongest for the
        // headline numbers, but remember every band it was seen on.
        if (band && !existing.bands.includes(band)) existing.bands.push(band);
        if (signal > existing.signal) {
          existing.signal = signal;
          existing.freq = freq;
          existing.flags = flags;
        }
      }
    }
    // Derived here rather than in the panel, so WifiPanel stays presentational
    // and the same objects can be reused by the failure diagnostics.
    for (const n of networks) {
      n.security = classifySecurity(n.flags);
      n.securityLabel = _SECURITY_LABEL[n.security] || n.security;
      n.blocker = securityBlocker(n.security);
    }
    networks.sort((a, b) => b.signal - a.signal);
    return networks;
  }

  // What can this device actually join?
  //
  // Measured on hardware (`wpa_cli get_capability key_mgmt`): NONE,
  // IEEE8021X, WPA-EAP, WPA-PSK. There is no SAE, so WPA3-Personal is not a
  // configuration problem, it is impossible on this radio — hence 'wpa3'
  // being refused outright rather than attempted. Mixed WPA2/WPA3 APs
  // advertise both and are joinable through the WPA2 half.
  function classifySecurity(flags) {
    const f = (flags || '').toUpperCase();
    const hasPsk = f.includes('WPA-PSK') || f.includes('WPA2-PSK') || f.includes('PSK');
    if (f.includes('EAP')) return 'enterprise';
    if (f.includes('SAE') && !hasPsk) return 'wpa3';
    if (hasPsk) return 'wpa2';
    if (f.includes('WEP')) return 'wep';
    return 'open';
  }

  // Why a network cannot be used, or null when it can.
  function securityBlocker(security) {
    if (security === 'wpa3') {
      return 'WPA3 only. This Dot has no SAE support, so it cannot join. '
           + 'Enable WPA2 compatibility mode on the router or hotspot.';
    }
    if (security === 'enterprise') {
      return 'Enterprise (802.1X). The wizard cannot configure this.';
    }
    if (security === 'wep') {
      return 'WEP. The wizard cannot configure this.';
    }
    return null;
  }

  // Quote a value for safe embedding inside a wpa_supplicant.conf network
  // block. SSIDs/PSKs containing a literal " or \ would break the file
  // format — reject rather than mis-escape, since this is config content,
  // not a shell string.
  function wpaConfEscape(value) {
    if (/["\\]/.test(value)) {
      throw new Error(`Value contains a double-quote or backslash character, which wpa_supplicant.conf cannot represent safely: "${value}"`);
    }
    return value;
  }

  async function runConfigWifi(c, ssid, psk) {
    if (!ssid) throw new Error('No SSID selected.');
    wpaConfEscape(ssid);
    wpaConfEscape(psk);

    // What the scan said about this network, if it was picked from the list.
    // A typed SSID that no scan saw is treated as hidden, which needs
    // scan_ssid=1 or wpa_supplicant never probes for it and sits in SCANNING
    // indefinitely.
    const known  = (wifiNetworks || []).find(n => n.ssid === ssid);
    const hidden = !known;
    const security = known ? known.security : (psk ? 'wpa2' : 'open');

    const blocker = securityBlocker(security);
    if (blocker) throw new Error(`Cannot join "${ssid}": ${blocker}`);

    if (security === 'wpa2' && !psk) {
      throw new Error(`"${ssid}" needs a password.`);
    }
    if (known) {
      addLog(`Network: ${_SECURITY_LABEL[security] || security}`
           + `${known.bands.length ? ', ' + known.bands.join(' + ') : ''}`
           + `, ${known.signal} dBm`);
    } else {
      addLog(`"${ssid}" was not in the last scan — configuring it as a hidden network.`, LOG_TONE.WARN);
    }

    // Write the conf and nothing else. There is no `svc wifi` behind this
    // image (no framework at all) and no live radio to join during
    // provisioning — the firmware stops Amazon's wifisvc and brings WiFi up
    // itself from this same conf at its next boot (docs/fireos6-port.md §2,
    // §4 Phase 2). The ADB session is already uid 0 (boot-root), so the conf
    // is pushed to a temp path and moved into place.
    addLog('Reading device identity…');
    const deviceName   = await c.shell('getprop ro.product.name')          || 'echomuse';
    const manufacturer = await c.shell('getprop ro.product.manufacturer')  || 'Amazon';
    const model        = await c.shell('getprop ro.product.model')        || 'AEOBC';
    const serial       = await c.shell('getprop ro.serialno')             || await c.shell('getprop ro.boot.serialno') || 'unknown';

    // Full config replacement — single network only, no ambiguity about
    // which AP it joins. Deliberately drops any prior (e.g. Alexa-era)
    // network entries.
    const confLines = [
      'ctrl_interface=/data/misc/wifi/sockets',
      'driver_param=use_p2p_group_interface=1',
      'update_config=1',
      `device_name=${deviceName}`,
      `manufacturer=${manufacturer}`,
      `model_name=${model}`,
      `model_number=${model}`,
      `serial_number=${serial}`,
      'device_type=1-0050F204-9',
      'os_version=01020300',
      'config_methods=physical_display virtual_push_button',
      'p2p_no_group_iface=1',
      'external_sim=1',
      'wowlan_triggers=disconnect',
      'network={',
      `\tssid="${ssid}"`,
      // The device reports NONE among its supported key_mgmt values, so an
      // open network is configurable as such rather than as WPA-PSK.
      ...(security === 'open'
            ? ['\tkey_mgmt=NONE']
            : [`\tpsk="${psk}"`, '\tkey_mgmt=WPA-PSK']),
      // Without this, wpa_supplicant only ever joins networks that appear in
      // a passive scan, so a hidden SSID never associates and reports nothing
      // more useful than SCANNING.
      ...(hidden ? ['\tscan_ssid=1'] : []),
      '\tpriority=1',
      '}',
      '',
    ].join('\n');

    addLog(`Writing config for "${ssid}"…`);
    await c.push('/data/local/tmp/wpa_supplicant.conf.new', new TextEncoder().encode(confLines));
    await c.shell('chmod 770 /data/misc/wifi');
    await c.shell('cp /data/local/tmp/wpa_supplicant.conf.new /data/misc/wifi/wpa_supplicant.conf');
    await c.shell('chown wifi:wifi /data/misc/wifi/wpa_supplicant.conf');
    await c.shell('chmod 660 /data/misc/wifi/wpa_supplicant.conf');
    await c.shell('rm -f /data/local/tmp/wpa_supplicant.conf.new');

    const onDevice = await c.shell('cat /data/misc/wifi/wpa_supplicant.conf 2>&1');
    if (!onDevice.includes(`ssid="${ssid}"`)) {
      throw new Error(`Config at /data/misc/wifi/wpa_supplicant.conf does not contain ssid="${ssid}" after write. On-device content:\n${onDevice}`);
    }
    addLog('Config written and verified on device. The EchoMuse firmware brings WiFi up '
         + 'itself at boot — not joined live during provisioning.', LOG_TONE.OK);
  }

  async function runInstallEchoMuse(c) {
    // There is no framework to mount emulated storage: /sdcard is a dangling
    // link to /storage/self/primary (observed on hardware), so pushes are
    // staged in /data/local/tmp.
    const stage = '/data/local/tmp';
    // The firmware bundled in this controller's image — the only binary it
    // ever installs, here and over the air. Fetched over the provisioning
    // route because a Dot mid-wizard has no device-link session, so
    // /api/devices/{id}/update cannot reach it yet.
    addLog('Fetching the EchoMuse firmware bundled with this controller…');
    const resp = await fetchProvision('/api/provision/firmware', 'fetching the bundled firmware.');
    const buf = await resp.arrayBuffer();
    const ver = resp.headers.get('X-Firmware-Version');
    addLog(`Installing EchoMuse firmware ${ver || '(version not reported)'}: ${(buf.byteLength/1024/1024).toFixed(1)} MB (${buf.byteLength.toLocaleString()} bytes) → ${stage}/server_new`);
    await c.push(`${stage}/server_new`, new Uint8Array(buf),
      pct => setProgress({ label: 'Uploading binary', pct }));
    setProgress(null);

    // Wipe any pre-existing install before writing fresh. A device that's
    // been through OTA before (or a previous, possibly-failed, wizard run)
    // can have server, server_a, AND server_b all present — OTA's slot
    // logic deliberately keeps the inactive slot around for rollback, but
    // that's the wrong default for a fresh provision: there's no good
    // "previous version" here, and leaving stale state behind is exactly
    // what let the GitHub-install bug silently keep an old dev build in
    // place. Each step is checked individually rather than && chained —
    // that's what let the original bug stay silent in the first place.
    addLog('Clearing any pre-existing EchoMuse install…');
    // adb's shell runs with umask 000, so a bare mkdir leaves the directory
    // init runs as root world-writable; with SELinux permissive (LineageOS)
    // file modes are all that keeps an app from replacing the binary.
    await c.shell('mkdir -p /data/local/bin && chmod 0755 /data/local/bin');
    const rmOut = (await c.shell('rm -f /data/local/bin/server /data/local/bin/server_a /data/local/bin/server_b 2>&1')).trim();
    if (rmOut) addLog(`  → ${rmOut}`);
    // Confirm the symlink itself is gone — readlink is already proven on
    // this device (the OTA pipeline's slot detection relies on it, always
    // with 2>/dev/null, never 2>&1). Confirmed on hardware: readlink on a
    // missing target prints an error message rather than returning truly
    // empty output, so capturing stderr here would corrupt the "empty
    // means gone" check below. Discard stderr instead, matching the
    // existing proven pattern in em_api.py exactly.
    const linkAfterClear = (await c.shell('readlink /data/local/bin/server 2>/dev/null')).trim();
    if (linkAfterClear) {
      throw new Error(`Failed to clear pre-existing install — /data/local/bin/server still links to "${linkAfterClear}" after rm. Check permissions/mount state with "mount" before retrying.`);
    }
    // Deliberately NOT separately checking that server_a/server_b are
    // gone via `ls`, `test -f`, or c.pull()/cat: readlink above just
    // demonstrated that this device's toolbox/mksh emits error TEXT for
    // a missing target rather than empty output, on a command this
    // codebase already trusted to behave the "normal" way. cat is a
    // strong candidate to do the same (`cat: ...: No such file`), which
    // would leak into c.pull()'s captured output and make this check
    // false-positive on a perfectly clean device — turning a working
    // provision into a hard abort, which is worse than the silent-stale
    // bug this whole block exists to fix. The rm output above is already
    // logged for visibility, and the install verification below checks
    // server_a's PRESENCE with correct content after the fresh write —
    // checking something exists with known content is safe to verify;
    // checking something doesn't exist, on this device, has already
    // proven not to be straightforward. If rm silently failed on a
    // locked/mounted-readonly server_a, the subsequent cp in the install
    // step would either overwrite it (fine) or fail loudly and get
    // caught by that verification anyway.
    addLog('Cleared.', LOG_TONE.OK);

    addLog('Installing to /data/local/bin/ (A slot)…');
    // Each step checked individually instead of && chained — the original
    // bug here was a chained mkdir/cp/chmod/ln with no stderr capture and
    // no output check, so a silent cp/ln failure (disk full, permission,
    // anything) would short-circuit the chain before ln -sf ran. With the
    // directory now guaranteed empty above, a partial failure here is
    // unambiguous: if cp fails, server_a simply won't exist, and the
    // verification below catches it precisely rather than guessing.
    const cpOut = (await c.shell(`cp ${stage}/server_new /data/local/bin/server_a 2>&1`)).trim();
    if (cpOut) addLog(`  → cp: ${cpOut}`);
    const chmodOut = (await c.shell('chmod 755 /data/local/bin/server_a 2>&1')).trim();
    if (chmodOut) addLog(`  → chmod: ${chmodOut}`);
    const lnOut = (await c.shell('ln -sf server_a /data/local/bin/server 2>&1')).trim();
    if (lnOut) addLog(`  → ln: ${lnOut}`);

    // Verify the symlink actually points where we just told it to, and
    // that the bytes on disk match what we pushed. Deliberately NOT using
    // `wc -c` or any other shell tool here that hasn't already been
    // proven on this device — this device has burned multiple sessions on
    // assumed-present tools turning out missing (awk/cut/head/printf/
    // which all confirmed absent), and a verification step that throws a
    // false positive because of a missing tool is worse than no
    // verification at all. c.pull() is already proven (it's how every
    // other pull in this wizard works), so reuse it for the size check
    // instead of trusting a new shell command's availability.
    const linkTarget = (await c.shell('readlink /data/local/bin/server 2>/dev/null')).trim();
    if (linkTarget !== 'server_a') {
      throw new Error(`Install verification failed: /data/local/bin/server points to "${linkTarget || '(empty — symlink missing)'}", expected "server_a". The cp/ln chain likely failed — check the install output above and free space on /data with "df".`);
    }
    const installedBytes = await c.pull('/data/local/bin/server_a');
    if (installedBytes.length !== buf.byteLength) {
      throw new Error(`Install verification failed: /data/local/bin/server_a is ${installedBytes.length.toLocaleString()} bytes on device, expected ${buf.byteLength.toLocaleString()}. The copy likely failed or was truncated — check free space on /data.`);
    }
    addLog(`Verified: server → server_a (${installedBytes.length.toLocaleString()} bytes, matches pushed binary).`, LOG_TONE.OK);

    addLog('Fetching startup script from controller…');
    const resp2 = await fetchProvision('/api/provision/start_script');
    const script = await resp2.text();
    // Same "Text file busy" risk as wificfg.sh — push + immediate chmod/exec
    // can race with the cat process. start_server.sh isn't executed
    // immediately here (only copied), so push() is safe for this one.
    await c.push(`${stage}/start_server.sh`, new TextEncoder().encode(script));
    await c.shell(`cp ${stage}/start_server.sh /data/local/bin/start_server.sh && chmod 755 /data/local/bin/start_server.sh`);
    addLog('EchoMuse installed.', LOG_TONE.OK);

    // Device-link TLS credentials — pushed pre-first-contact so the very
    // first connection this device ever makes to the controller is wss +
    // token-authenticated. The controller mints the token against the
    // serial (a pending device row is created if needed; approval flow is
    // unchanged). A 503 means this controller has no TLS listener
    // (cryptography package missing) — provision proceeds plain, and the
    // dashboard "Secure link" action can retrofit credentials later.
    addLog('Fetching device-link TLS credentials…');
    const serial = (await c.shell('getprop ro.serialno')).trim();
    if (!serial) {
      addLog('Could not read device serial — skipping TLS credential install.', LOG_TONE.WARN);
    } else {
      const creds = await fetchProvision('/api/provision/tls_credentials', 'fetching TLS credentials.', {
        method: 'POST', json: true, body: JSON.stringify({ device_id: serial }),
      }).then(r => r.json(), e => { if (e.status === 503) return null; throw e; });
      if (!creds) {
        addLog('Controller has no TLS listener — device will connect over plain ws.', LOG_TONE.WARN);
      } else {
        await c.push(`${stage}/em-ca.pem`, new TextEncoder().encode(creds.ca_pem));
        await c.push(`${stage}/em-token`, new TextEncoder().encode(creds.token));
        await c.shell(`mkdir -p ${creds.dir} && cp ${stage}/em-ca.pem ${creds.dir}/ca.pem && cp ${stage}/em-token ${creds.dir}/token && chmod 644 ${creds.dir}/ca.pem && chmod 600 ${creds.dir}/token && rm -f ${stage}/em-ca.pem ${stage}/em-token`);
        const tlsListing = (await c.shell(`ls ${creds.dir} 2>&1`)).trim();
        if (!tlsListing.includes('ca.pem') || !tlsListing.includes('token')) {
          throw new Error(`TLS credential install verification failed — ${creds.dir} contains: "${tlsListing}".`);
        }
        addLog('TLS credentials installed — device will connect over wss.', LOG_TONE.OK);
      }
    }
    // WiFi (the Fire OS Show: neutralisation) and the reboot are separate
    // steps — this one only has to leave the binary and startup script
    // installed and verified.
    addLog(platformRef.current === WIZARD_PLATFORM.CROWN
      ? 'EchoMuse installed — neutralising Alexa and the reboot are separate steps.'
      : platformRef.current === WIZARD_PLATFORM.CROWN_LINEAGE
      ? 'EchoMuse installed — the reboot is a separate step.'
      : 'EchoMuse installed — WiFi and reboot are separate steps.', LOG_TONE.OK);
  }

  // echomuse.rc and the empty /tmp mountpoint start_server.sh
  // puts a tmpfs on (Fire OS 6 ships no /tmp) go onto the system slots from
  // this live Android session — no boot image write and no
  // sepolicy change (docs/fireos6-port.md Phase 3). Both slots end up hooked,
  // because the bootloader falls back to the other slot after failed boots
  // and an unhooked slot comes up as stock Alexa — but in two stages:
  // install_boot_hook writes only the running slot, and mirror_boot_hook
  // writes the other one only after confirm_link has seen a boot from the
  // first reach this controller. Until then the other slot stays stock, a
  // known-good fallback if the install is bad. Writing the inactive slot is
  // safe because verity is off for both: androidboot.veritymode=disabled
  // comes from the shared (amonet) bootloader, not either boot image, whose
  // cmdlines are identical (checked on hardware). A slot already carrying
  // both is skipped, so a retry pays only for what is missing.
  async function activeSlot(c) {
    const slot = (await c.shell('getprop ro.boot.slot_suffix')).trim();
    if (slot !== '_a' && slot !== '_b') {
      throw new Error(`Unexpected active slot "${slot}" (ro.boot.slot_suffix) — expected _a or _b.`);
    }
    return slot;
  }

  async function withStaged(c, path, content, fn) {
    await c.push(path, new TextEncoder().encode(content));
    try {
      await fn();
    } finally {
      await c.shell(`rm -f ${path}`);
    }
  }

  function withStagedRc(c, rc, fn) {
    return withStaged(c, '/data/local/tmp/echomuse.rc.new', rc, fn);
  }

  async function runInstallBootHook(c) {
    if (platformRef.current === WIZARD_PLATFORM.CROWN) {
      await withStagedRc(c, _ECHOMUSE_RC_CROWN, () => installBootHookCrown(c));
      return;
    }
    if (platformRef.current === WIZARD_PLATFORM.CROWN_LINEAGE) {
      await withStagedRc(c, _ECHOMUSE_RC_LINEAGE, () => withStaged(
        c, '/data/local/tmp/50-echomuse.sh.new', _ECHOMUSE_ADDOND_LINEAGE,
        () => installBootHookLineage(c)));
      return;
    }
    const active = await activeSlot(c);
    await withStagedRc(c, _ECHOMUSE_RC, () => installBootHookActive(c, active));
    addLog(`The other slot stays stock until a boot from ${active} reaches this controller `
         + '(Mirror Boot Hook).');
  }

  // The Echo Show has one system partition, mounted read-only on its own
  // (/system, ext4, verity off), under a ramdisk root that is rebuilt at
  // every boot: so no slots, no mirror step and no /tmp mountpoint, which
  // start_server.sh does not use there. Undo: remount /system read-write,
  // delete /system/etc/init/echomuse.rc, remount read-only (docs/rooting.md).
  async function bootHookPresentCrown(c) {
    return _rcInstalled(await c.shell('cat /system/etc/init/echomuse.rc 2>/dev/null'),
                        _ECHOMUSE_RC_CROWN);
  }

  async function installBootHookCrown(c) {
    if (await bootHookPresentCrown(c)) {
      addLog('Boot hook already in place.', LOG_TONE.OK);
      return;
    }
    addLog('Remounting /system read-write…');
    const rw = await c.shell('mount -o rw,remount /system 2>&1');
    if (rw.trim()) addLog(`  → ${rw.trim()}`);
    try {
      const out = (await c.shell('(cp /data/local/tmp/echomuse.rc.new /system/etc/init/echomuse.rc '
                               + '&& chmod 0644 /system/etc/init/echomuse.rc && sync) 2>&1')).trim();
      if (out) addLog(`  → ${out}`);
    } finally {
      // Always attempt the remount back, even if the write above threw.
      const ro = await c.shell('mount -o ro,remount /system 2>&1');
      if (ro.trim()) addLog(`  → ${ro.trim()}`);
    }
    if (!await bootHookPresentCrown(c)) {
      throw new Error('/system/etc/init/echomuse.rc did not read back as written.');
    }
    addLog('/system/etc/init/echomuse.rc written and verified; /system is read-only again.',
           LOG_TONE.OK);
  }

  // LineageOS 18.1 mounts the system partition as / (system-as-root), with
  // no verity and no overlayfs, so `mount -o remount,rw /` makes
  // /system/etc/init and /system/addon.d writable (measured,
  // docs/echo-show-crown.md §8.5). Both files are compared byte for byte
  // and the addon.d script's mode with them, so a re-run writes only what
  // is missing or stale. Undo: remount / read-write, delete both files,
  // remount read-only (docs/rooting.md).
  async function bootHookPresentLineage(c) {
    const rc = await c.shell('cat /system/etc/init/echomuse.rc 2>/dev/null');
    const addond = await c.shell(`cat ${_ADDOND_PATH} 2>/dev/null`);
    const mode = (await c.shell(`stat -c %a ${_ADDOND_PATH} 2>/dev/null`)).trim();
    return _rcInstalled(rc, _ECHOMUSE_RC_LINEAGE)
        && _rcInstalled(addond, _ECHOMUSE_ADDOND_LINEAGE) && mode === '755';
  }

  async function installBootHookLineage(c) {
    if (await bootHookPresentLineage(c)) {
      addLog('Boot hook and its addon.d script already in place.', LOG_TONE.OK);
      return;
    }
    addLog('Remounting / read-write…');
    const rw = await c.shell('mount -o remount,rw / 2>&1');
    if (rw.trim()) addLog(`  → ${rw.trim()}`);
    try {
      const out = (await c.shell('(cp /data/local/tmp/echomuse.rc.new /system/etc/init/echomuse.rc '
                               + '&& chmod 0644 /system/etc/init/echomuse.rc '
                               + '&& mkdir -p /system/addon.d '
                               + `&& cp /data/local/tmp/50-echomuse.sh.new ${_ADDOND_PATH} `
                               + `&& chmod 0755 ${_ADDOND_PATH} && sync) 2>&1`)).trim();
      if (out) addLog(`  → ${out}`);
    } finally {
      // Always attempt the remount back, even if the write above threw.
      const ro = await c.shell('mount -o remount,ro / 2>&1');
      if (ro.trim()) addLog(`  → ${ro.trim()}`);
    }
    if (!await bootHookPresentLineage(c)) {
      throw new Error(`/system/etc/init/echomuse.rc or ${_ADDOND_PATH} did not read back as written.`);
    }
    addLog(`/system/etc/init/echomuse.rc and ${_ADDOND_PATH} written and verified; / is `
         + 'read-only again. A LineageOS update keeps the boot hook; a clean flash needs this '
         + 'wizard again.', LOG_TONE.OK);
  }

  // The HOME activities the Show offers, logged; throws unless one of them
  // can replace Amazon's (_kioskHomes). Returns those. Candidates come from
  // query-activities, not resolve-activity: until Amazon's setup screen
  // (priority 101) is hidden, it wins resolution over any preferred home
  // (measured), so resolution is only checked after hiding. A known kiosk
  // installed with its HOME activity switched off (_kioskToEnable) is
  // switched on here, as its own launcher setting would.
  async function requireKioskHome(c) {
    let all = _homeActivities(await c.shell(_HOME_QUERY));
    addLog(`Home activities: ${all.map(_component).join(', ') || '(none)'}`);
    const enable = _kioskToEnable(all, (await packageSets(c)).installed);
    if (enable) {
      const comp = _component(enable);
      addLog(`${enable.name} is installed, but its home screen is switched off: it ships that `
           + 'way, and its own launcher setting switches it on. Switching it on now: '
           + `pm enable ${comp}`);
      const out = (await c.shell(`pm enable ${comp} 2>&1`)).trim();
      if (out) addLog(`  → ${out}`);
      all = _homeActivities(await c.shell(_HOME_QUERY));
      addLog(`Home activities now: ${all.map(_component).join(', ') || '(none)'}`);
    }
    const kiosks = _kioskHomes(all);
    if (!kiosks.length) {
      throw new Error(
        "No home-screen app other than Amazon's is installed. Install your kiosk (Fully Kiosk "
        + 'Browser, for example: `adb shell settings put global disable_bouncer 1`, then '
        + '`adb install <apk>`), switch on its home-screen (launcher) setting if it has one, '
        + "and Retry. Amazon's launchers are hidden later, and with no other home screen the "
        + 'Show would boot to a blank screen.');
    }
    return kiosks;
  }

  async function runCheckHome(c) {
    const kiosks = await requireKioskHome(c);
    addLog(`Kiosk home: ${kiosks.map(_component).join(', ')}.`, LOG_TONE.OK);
  }

  // The visible and the installed (visible or hidden) packages.
  async function packageSets(c) {
    const visibleOut = await c.shell('pm list packages 2>&1');
    if (!visibleOut.includes('package:')) {
      throw new Error(`The package manager is not answering (${visibleOut || 'no output'}). `
                    + 'Give the Show a moment to finish booting and Retry.');
    }
    return { visible: _listedPackages(visibleOut),
             installed: _listedPackages(await c.shell('pm list packages -u 2>&1')) };
  }

  async function memAvailable(c) {
    const m = /MemAvailable:\s*(\d+)/.exec(await c.shell('cat /proc/meminfo'));
    return m ? `${Math.round(Number(m[1]) / 1024)} MB` : '(unknown)';
  }

  // KnightSettings' config bundle (_SETTINGS_BUNDLE_PREFS): written by one
  // broadcast when missing, which it is on a Show never registered with
  // Amazon. Without it no Settings page opens, Wi-Fi and Bluetooth included.
  async function ensureSettingsBundle(c) {
    const read = async () => _hasSettingsBundle(await c.shell(`cat ${_SETTINGS_BUNDLE_PREFS} 2>/dev/null`));
    if (await read()) {
      addLog('Settings already has its config bundle.', LOG_TONE.OK);
      return;
    }
    addLog("Settings has no config bundle (the Show was never registered with Amazon), so its "
         + 'pages crash. Asking it to write one…');
    // A Settings process left in its crashed state makes the broadcast wait
    // 60 s for it (measured); a fresh one answers at once.
    await c.shell('am force-stop com.android.settings');
    const out = (await c.shell(`${_SETTINGS_BUNDLE_BROADCAST} 2>&1`)).trim();
    if (out) addLog(`  → ${out}`);
    for (let i = 0; i < 10; i++) {
      if (await read()) {
        addLog('Settings config bundle written; Settings pages open again.', LOG_TONE.OK);
        return;
      }
      await new Promise(r => setTimeout(r, 1000));
    }
    throw new Error(`Settings still has no config bundle (no bundle_location in ${_SETTINGS_BUNDLE_PREFS}), `
                  + "so the Show's Wi-Fi and Bluetooth pages would not open. Retry.");
  }

  // docs/echo-show-crown.md §9.4's order: a kiosk HOME must exist, then the
  // setup flags, Settings' config bundle, the kiosk as preferred HOME, the
  // hiding, and the microphone denials. The lists come from the controller
  // (device_payloads/) and are installed beside the tiers they apply, so
  // start_server.sh re-applies them at every boot; the tiers file is written
  // before hiding, so a run that stops part-way is finished by the next
  // boot. Tier 2 follows the operator's choice both ways: unticked on a later
  // run, its packages are unhidden again.
  async function runNeutralizeAlexa(c, withTier2) {
    const kiosks = await requireKioskHome(c);
    addLog(`MemAvailable before: ${await memAvailable(c)}.`);

    for (const [ns, key] of [['global', 'device_provisioned'], ['secure', 'user_setup_complete']]) {
      await c.shell(`settings put ${ns} ${key} 1`);
      const v = (await c.shell(`settings get ${ns} ${key}`)).trim();
      if (v !== '1') throw new Error(`settings ${ns} ${key} reads "${v}" after setting it to 1.`);
    }
    addLog('Setup marked complete (device_provisioned=1, user_setup_complete=1).', LOG_TONE.OK);
    await ensureSettingsBundle(c);

    // The default home, set now; it only takes effect once Amazon's setup
    // screen is hidden below, and is checked after that.
    const kiosk = _chooseKiosk(kiosks);
    if (kiosks.length > 1) {
      addLog(`More than one home app besides Amazon's (${kiosks.map(_component).join(', ')}); `
           + `making ${_component(kiosk)} the default. Change it later in the Show's Settings.`,
             LOG_TONE.WARN);
    }
    {
      const out = (await c.shell(`cmd package set-home-activity ${_component(kiosk)} 2>&1`)).trim();
      addLog(`Default home: ${_component(kiosk)} (${out || 'no output'}).`,
             /Success/.test(out) ? undefined : LOG_TONE.WARN);
    }

    const resp = await fetchProvision('/api/provision/crown_lists', 'fetching the hide and mic-deny lists.');
    const { dir, tiers_file: tiersFile, hide: lists, mic_deny: micDeny } = await resp.json();
    const stage = '/data/local/tmp';
    await c.shell(`mkdir -p ${dir}`);
    for (const l of [...lists, micDeny]) {
      await c.push(`${stage}/${l.file}`, new TextEncoder().encode(l.text));
      const out = (await c.shell(`cp ${stage}/${l.file} ${dir}/${l.file} && chmod 644 ${dir}/${l.file} `
                               + `&& rm -f ${stage}/${l.file} 2>&1`)).trim();
      if (out) addLog(`  → ${out}`);
    }
    const tiers = withTier2 ? ['1', '2'] : ['1'];
    const before = (await c.shell(`cat ${tiersFile} 2>/dev/null`)).split(/\s+/).filter(Boolean);
    await c.shell(`echo ${tiers.join(' ')} > ${tiersFile}`);
    const written = (await c.shell(`cat ${tiersFile} 2>&1`)).trim();
    if (written !== tiers.join(' ')) {
      throw new Error(`${tiersFile} reads "${written}" after writing "${tiers.join(' ')}".`);
    }

    let { visible, installed } = await packageSets(c);
    const applied = lists.filter(l => tiers.includes(l.tier)).flatMap(l => l.packages);
    // Never hidden: the packages the Show cannot do without, whatever a list
    // says, and the last keyboard. One hidden by an earlier list is unhidden.
    for (const pkg of _keepVisible(applied)) {
      addLog(`Not hiding ${pkg} although a list names it: ${_CROWN_KEEP_VISIBLE[pkg]}.`, LOG_TONE.WARN);
    }
    for (const pkg of Object.keys(_CROWN_KEEP_VISIBLE).filter(p => installed.has(p) && !visible.has(p))) {
      const out = (await c.shell(`pm unhide ${pkg} 2>&1`)).trim();
      addLog(`Unhid ${pkg}, which an earlier run hid: ${_CROWN_KEEP_VISIBLE[pkg]}. (${out || 'no output'})`,
             LOG_TONE.WARN);
    }
    const imeKeep = _imeKeep(applied, _imePackages(await c.shell('ime list -s 2>&1')));
    if (imeKeep.length) {
      addLog(`Not hiding ${imeKeep.join(', ')}: it holds every enabled keyboard. Install and `
           + 'enable another keyboard first.', LOG_TONE.WARN);
    }
    const keep = new Set([..._keepVisible(applied), ...imeKeep]);
    ({ visible, installed } = await packageSets(c));
    for (const l of lists) {
      const hide = tiers.includes(l.tier);
      if (!hide && !before.includes(l.tier)) continue;   // never applied: leave it alone
      const verb = hide ? 'hide' : 'unhide';
      // Only what needs the call: pm is slow per package, and an already
      // hidden (or, for unhide, visible) package needs nothing.
      const todo = l.packages.filter(p => installed.has(p) && !keep.has(p)
                                          && (hide ? visible.has(p) : !visible.has(p)));
      const absent = l.packages.filter(p => !installed.has(p));
      addLog(`Tier ${l.tier}: ${hide ? 'hiding' : 'unhiding (no longer chosen)'} `
           + `${todo.length} of ${l.packages.length} packages`
           + (absent.length ? ` (${absent.length} not on this build)` : '') + '…');
      const rejected = [];
      for (const pkg of todo) {
        const out = (await c.shell(`pm ${verb} ${pkg} 2>&1`)).trim();
        const verdict = _pmVerdict(out);
        if (verdict === PM_VERDICT.REJECTED) rejected.push(`${pkg}: ${out || 'no output'}`);
        addLog(`  ${pkg} → ${verdict === PM_VERDICT.DONE ? `${verb === 'hide' ? 'hidden' : 'visible'}`
                          : verdict === PM_VERDICT.ABSENT ? 'not installed on this build'
                          : (out || 'no output')}`,
               verdict === PM_VERDICT.DONE ? undefined : LOG_TONE.WARN);
      }
      if (rejected.length && hide) {
        throw new Error(`The package manager refused to hide ${rejected.length} Tier ${l.tier} `
                      + `package(s): ${rejected.join('; ')}. Retry; every boot also re-hides them.`);
      }
    }

    ({ visible, installed } = await packageSets(c));
    for (const l of lists.filter(x => tiers.includes(x.tier))) {
      const left = l.packages.filter(p => visible.has(p) && !keep.has(p));
      if (left.length) throw new Error(`Tier ${l.tier} packages still visible: ${left.join(', ')}.`);
    }
    addLog(`${installed.size - visible.size} of ${installed.size} installed packages are hidden.`,
           LOG_TONE.OK);

    // The Amazon apps that stay but could record: their RECORD_AUDIO app op
    // set to ignore, so AudioFlinger refuses their recorders even with the
    // permission granted. Persistent; start_server.sh re-asserts it per boot.
    const micPkgs = micDeny.packages.filter(p => installed.has(p));
    addLog(`Denying the microphone to ${micPkgs.join(', ') || 'nothing (none installed)'}…`);
    const stillAllowed = [];
    for (const pkg of micPkgs) {
      if (!_micDenied(await c.shell(`appops get ${pkg} RECORD_AUDIO 2>&1`))) {
        const out = (await c.shell(`appops set ${pkg} RECORD_AUDIO ignore 2>&1`)).trim();
        if (out) addLog(`  ${pkg}: ${out}`, LOG_TONE.WARN);
      }
      const now = (await c.shell(`appops get ${pkg} RECORD_AUDIO 2>&1`)).trim();
      if (_micDenied(now)) addLog(`  ${pkg} → microphone denied`);
      else stillAllowed.push(`${pkg} (${now || 'no output'})`);
    }
    if (stillAllowed.length) {
      throw new Error(`The microphone is still allowed to ${stillAllowed.join('; ')}: it could take `
                    + "the Show's one recording from EchoMuse. Retry.");
    }

    // With Amazon's launchers hidden, HOME must resolve to the kiosk chosen
    // above.
    const home = _homeActivities(await c.shell(_HOME_RESOLVE)).pop();
    if (!_sameComponent(home, kiosk)) {
      throw new Error(`HOME resolves to ${home ? _component(home) : 'nothing'}, not `
                    + `${_component(kiosk)}, so the Show would not boot to your kiosk. Retry, or `
                    + 'undo with `pm unhide` on the Tier 1 list (docs/rooting.md).');
    }
    addLog(`Home screen: ${_component(home)}.`, LOG_TONE.OK);
    addLog(`MemAvailable after: ${await memAvailable(c)}.`);
    addLog("For the Show's own Wi-Fi and Bluetooth settings, add these to your kiosk's app "
         + 'launcher: ' + Object.entries(_CROWN_SETTINGS_PAGES).map(([n, comp]) => `${n} ${comp}`).join(', ')
         + ' (Android\'s generic Wi-Fi settings link does not open on this build).');
    addLog(`Undo: pm unhide each package in ${dir}/${lists.map(l => l.file).join(' and ')}, `
         + `appops set <package> RECORD_AUDIO default for each in ${dir}/${micDeny.file}, `
         + `then delete ${tiersFile} (docs/rooting.md).`);
  }

  // The device-link hello is the only evidence that the whole chain works:
  // init started the service, start_server.sh ran, the firmware joined WiFi,
  // found this controller over mDNS and authenticated. A new device is
  // refused as pending approval, so `connected` stays false — but every
  // refused attempt refreshes its row's last_seen and ip (em_device.
  // register_device), and that change against the pre-reboot snapshot is
  // the signal. Approved devices (auto-approval, or a re-adopted one) show as
  // connected instead.
  async function runConfirmLink(c) {
    const serial = (await c.shell('getprop ro.serialno')).trim();
    const base = linkBaseline.current;
    if (!serial) throw new Error('Could not read the device serial (ro.serialno).');
    if (!base || base.serial !== serial) {
      throw new Error('There is no pre-reboot snapshot of this device to compare against. '
                    + 'Go back to the Reboot step and run it again.');
    }
    addLog(`Waiting for ${serial} to reach this controller…`);
    const TIMEOUT_MS = 180000;
    const started = Date.now();
    let lastNote = -1;
    while (Date.now() - started < TIMEOUT_MS) {
      let d = null;
      try {
        d = (await API.get('/api/devices')).find(x => x.device_id === serial) || null;
      } catch (e) {
        addLog(`  Could not read /api/devices (${e.error || e.message || e}) — retrying.`, LOG_TONE.WARN);
      }
      if (d && (d.connected || d.last_seen !== base.lastSeen)) {
        const state = !d.approved ? 'pending approval' : d.connected ? 'connected' : 'approved';
        addLog(`${serial} reached this controller from ${d.ip || 'an unknown address'} `
             + `after ${Math.round((Date.now() - started) / 1000)}s — ${state}.`, LOG_TONE.OK);
        return;
      }
      const elapsed = Math.floor((Date.now() - started) / 1000);
      if (elapsed - lastNote >= 15) {
        lastNote = elapsed;
        addLog(`  [${elapsed}s] not heard from yet.`);
      }
      await new Promise(r => setTimeout(r, 3000));
    }
    const crown = _isShow(platformRef.current);
    const serverLog = _serverLog(platformRef.current);
    const tail = await c.shell(
      `grep -E '\\[wifi\\]|\\[clock\\]|mDNS|link|session' ${serverLog} 2>&1 | tail -n 25`);
    addLog(`${crown ? 'Link' : 'WiFi and link'} lines from ${serverLog}:\n${tail || '(none)'}`,
           LOG_TONE.WARN);
    throw new Error(`The device did not reach this controller within ${TIMEOUT_MS / 1000}s. `
                  + (crown
                      ? 'Check the Show is still on Wi-Fi (its Settings) and can reach this '
                        + 'controller, fix the cause above, then Retry.'
                      : 'The other system slot has not been touched, so a fallback boot still '
                        + 'comes up as stock Alexa. Fix the cause above, then Retry.'));
  }

  async function runMirrorBootHook(c) {
    const active = await activeSlot(c);
    // The slot that just proved itself must be the one carrying the hook. If
    // the bootloader had switched slots, the Dot would be running the
    // unhooked one, and mirroring "from" it would be guesswork.
    if (!await bootHookPresent(c, '')) {
      throw new Error(`The running slot ${active} does not carry the boot hook, so this boot `
                    + 'did not come from the slot Install Boot Hook wrote. Not touching the other slot.');
    }
    await withStagedRc(c, _ECHOMUSE_RC, () => installBootHookOther(c, active === '_a' ? '_b' : '_a'));
    addLog('Both system slots now carry the boot hook.', LOG_TONE.OK);
  }

  // The write for a system root mounted read-write at `root` ('' for the
  // running one). /tmp is created already labelled system_file: a plain
  // mkdir would label it rootfs, which policy refuses on the ext4 system
  // root even from boot-root's permissive domain (`avc: denied { associate }
  // … tcontext=u:object_r:labeledfs:s0`, observed on hardware).
  function bootHookWriteCmd(root) {
    return `cp /data/local/tmp/echomuse.rc.new ${root}/system/etc/init/echomuse.rc `
         + `&& chmod 0644 ${root}/system/etc/init/echomuse.rc `
         + `&& { [ -d ${root}/tmp ] || mkdir -Z u:object_r:system_file:s0 ${root}/tmp; } `
         + `&& chmod 0755 ${root}/tmp && sync`;
  }

  async function bootHookPresent(c, root) {
    const rc = await c.shell(`cat ${root}/system/etc/init/echomuse.rc 2>/dev/null`);
    const tmp = (await c.shell(`[ -d ${root}/tmp ] && echo yes`)).trim() === 'yes';
    return _rcInstalled(rc) && tmp;
  }

  async function installBootHookActive(c, slot) {
    if (await bootHookPresent(c, '')) {
      addLog(`Slot ${slot} (active): boot hook already in place.`, LOG_TONE.OK);
      return;
    }
    addLog(`Slot ${slot} (active): remounting / read-write…`);
    const rw = await c.shell('mount -o rw,remount / 2>&1');
    if (rw.trim()) addLog(`  → ${rw.trim()}`);
    try {
      const out = (await c.shell(`(${bootHookWriteCmd('')}) 2>&1`)).trim();
      if (out) addLog(`  → ${out}`);
    } finally {
      // Always attempt the remount back, even if the write above threw —
      // leaving / writable is not a state to abandon the device in.
      const ro = await c.shell('mount -o ro,remount / 2>&1');
      if (ro.trim()) addLog(`  → ${ro.trim()}`);
    }
    if (!await bootHookPresent(c, '')) {
      throw new Error(`Slot ${slot} (active): echomuse.rc or /tmp did not read back as written.`);
    }
    addLog(`Slot ${slot} (active): written and verified.`, LOG_TONE.OK);
  }

  async function installBootHookOther(c, slot) {
    const mnt = `/data/local/tmp/em_system${slot}`;
    const dev = `/dev/block/platform/bootdevice/by-name/system${slot}`;
    const mountAs = async (mode) => {
      const out = (await c.shell(`mkdir -p ${mnt} && mount -t ext4 -o ${mode} ${dev} ${mnt} 2>&1`)).trim();
      if (out) throw new Error(`Slot ${slot} (inactive): could not mount ${dev} ${mode}: ${out}`);
    };
    const unmount = async () => (await c.shell(`umount ${mnt} 2>&1; rmdir ${mnt} 2>&1`)).trim();

    await mountAs('ro');
    let otherBuild, present;
    try {
      otherBuild = (await c.shell(`grep '^ro.build.fingerprint=' ${mnt}/system/build.prop`)).trim()
        .replace('ro.build.fingerprint=', '') || '(unknown)';
      present = await bootHookPresent(c, mnt);
    } finally {
      await unmount();
    }
    // The full fingerprint: display.id alone can match across builds.
    const activeBuild = (await c.shell('getprop ro.build.fingerprint')).trim();
    addLog(`Slot ${slot} (inactive): ${otherBuild}.`);
    if (otherBuild !== activeBuild) {
      addLog(`  Differs from the active slot's ${activeBuild}. EchoMuse is tested on the active build; `
           + 'this slot only runs if the bootloader falls back to it.', LOG_TONE.WARN);
    }
    if (present) {
      addLog(`Slot ${slot} (inactive): boot hook already in place.`, LOG_TONE.OK);
      return;
    }

    await mountAs('rw');
    try {
      const out = (await c.shell(`(${bootHookWriteCmd(mnt)}) 2>&1`)).trim();
      if (out) addLog(`  → ${out}`);
    } finally {
      const u = await unmount();
      if (u) addLog(`  → ${u}`, LOG_TONE.WARN);
    }
    // Read back through a fresh read-only mount: what that slot's next boot sees.
    await mountAs('ro');
    try {
      present = await bootHookPresent(c, mnt);
    } finally {
      await unmount();
    }
    if (!present) {
      throw new Error(`Slot ${slot} (inactive): echomuse.rc or /tmp did not read back as written.`);
    }
    addLog(`Slot ${slot} (inactive): written and verified.`, LOG_TONE.OK);
  }

  // The echomuse service starts on `sys.boot_completed=1`
  // (measured 11.7s after boot on hardware — docs/fireos6-port.md Phase 3),
  // and the audio service is required for it to have anything to talk to:
  // Amazon's mixer on the Dot, Android's audioserver on the Show, whose full
  // framework also takes longer to reach boot_completed. On failure, tail
  // the server's own log (_serverLog — the path start_server.sh writes, see
  // device_payloads/start_server.sh) rather than just naming the stuck
  // property.
  async function runVerifyService(c) {
    const crown = _isShow(platformRef.current);
    const audio = crown ? 'audioserver' : 'mixer';
    addLog(`Waiting for echomuse and ${audio} to start…`);
    const TIMEOUT_MS = crown ? 180000 : 90000;
    const started = Date.now();
    let echomuseState = '', audioState = '', lastNote = -1;
    while (Date.now() - started < TIMEOUT_MS) {
      echomuseState = (await c.shell('getprop init.svc.echomuse')).trim();
      audioState    = (await c.shell(`getprop init.svc.${audio}`)).trim();
      if (echomuseState === 'running' && audioState === 'running') {
        addLog(`init.svc.echomuse=running, init.svc.${audio}=running `
             + `(after ${Math.round((Date.now() - started) / 1000)}s).`, LOG_TONE.OK);
        return;
      }
      const elapsed = Math.floor((Date.now() - started) / 1000);
      if (elapsed - lastNote >= 10) {
        lastNote = elapsed;
        addLog(`  [${elapsed}s] init.svc.echomuse=${echomuseState || '(unset)'}, `
             + `init.svc.${audio}=${audioState || '(unset)'}`);
      }
      await new Promise(r => setTimeout(r, 2000));
    }
    const serverLog = _serverLog(platformRef.current);
    const tail = await c.shell(`tail -c 4000 ${serverLog} 2>&1`);
    addLog(`Tail of ${serverLog}:\n${tail || '(empty or missing)'}`, LOG_TONE.WARN);
    throw new Error(`Service did not reach "running" within ${TIMEOUT_MS / 1000}s — `
                   + `init.svc.echomuse=${echomuseState || '(unset)'}, `
                   + `init.svc.${audio}=${audioState || '(unset)'}.`);
  }

  // The EchoMuse app on a LineageOS Show is the firmware's to install and
  // start (docs/echo-show-companion-plan.md decision 5), so this step waits
  // for the firmware to report it: `stats.companion`, read through the
  // controller. Stats arrive at session ready and every 30 s after, and a
  // device's last stats outlive its session, so only a report other than the
  // one on hand when the step first saw the device counts: a report from
  // before the reboot must not pass it. A device pending approval has no
  // session and so no stats; for it the same two facts are read over adb
  // (_companionOnDevice).
  async function runVerifyCompanion(c) {
    const serial = (await c.shell('getprop ro.serialno')).trim();
    if (!serial) throw new Error('Could not read the device serial (ro.serialno).');
    addLog(`Waiting for the EchoMuse app on ${serial} to link with its overlay allowed…`);
    const TIMEOUT_MS = 60000;
    const started = Date.now();
    const fmt = v => v == null ? 'unavailable' : String(v);
    let baseline, health = null, note = 'no report yet', lastNote = -1;
    while (Date.now() - started < TIMEOUT_MS) {
      let d = null;
      try {
        d = (await API.get('/api/devices')).find(x => x.device_id === serial) || null;
      } catch (e) {
        addLog(`  Could not read /api/devices (${e.error || e.message || e}) — retrying.`, LOG_TONE.WARN);
      }
      const after = `after ${Math.round((Date.now() - started) / 1000)}s`;
      if (d?.connected) {
        const report = JSON.stringify(d.stats ?? null);
        if (baseline === undefined) baseline = report;
        if (report !== baseline) {
          health = d.stats?.companion ?? null;
          note = health ? `linked=${fmt(health.linked)}, overlay=${fmt(health.overlay)}`
                        : 'the stats report carries no companion health';
          if (_companionReady(health)) {
            addLog(`EchoMuse app ${health.version || '(version unavailable)'} linked, overlay allowed `
                 + `(${after}).`, LOG_TONE.OK);
            return;
          }
        } else {
          note = 'waiting for a stats report from this session';
        }
      } else if (d && !d.approved) {
        const uid = (await c.shell(`stat -c %u /data/data/${_COMPANION_PKG} 2>/dev/null`)).trim();
        health = _companionOnDevice(await c.shell('cat /proc/net/tcp /proc/net/tcp6 2>/dev/null'), uid,
                                    await c.shell(`appops get ${_COMPANION_PKG} SYSTEM_ALERT_WINDOW 2>&1`));
        note = `pending approval, read on the device: linked=${health.linked}, overlay=${health.overlay}`;
        if (_companionReady(health)) {
          addLog(`EchoMuse app linked, overlay allowed (${after}; read on the device, since ${serial} `
               + 'is pending approval — its Status tab shows the app once approved).', LOG_TONE.OK);
          return;
        }
      } else {
        note = d ? 'approved but not connected' : 'not known to this controller';
      }
      const elapsed = Math.floor((Date.now() - started) / 1000);
      if (elapsed - lastNote >= 15) {
        lastNote = elapsed;
        addLog(`  [${elapsed}s] ${note}`);
      }
      await new Promise(r => setTimeout(r, 3000));
    }
    const serverLog = _serverLog(platformRef.current);
    const lines = await c.shell(`grep -F '[companion]' ${serverLog} 2>&1 | tail -n 25`);
    addLog(`[companion] lines from ${serverLog}:\n${lines || '(none)'}`, LOG_TONE.WARN);
    const pkg = await c.shell(`dumpsys package ${_COMPANION_PKG} 2>&1 | grep -E `
      + `'Package \\[|versionName|versionCode|userId=|lastUpdateTime|installed=|SYSTEM_ALERT_WINDOW'`);
    addLog(`dumpsys package ${_COMPANION_PKG}:\n${pkg || '(not installed)'}`, LOG_TONE.WARN);
    throw new Error(`The EchoMuse app did not report linked with its overlay allowed within `
                  + `${TIMEOUT_MS / 1000}s (${note}). EchoMuse itself is running and reached this `
                  + 'controller; the firmware retries the app every 30 s, so fix the cause above, then Retry.');
  }

  // Decision 4 of the companion plan: when Kiosk Satellite is installed, ask
  // for its Remote Administration password, which the controller turns into
  // the token the firmware installs the plugin with. Installed is read from
  // stats.companion.kiosk, or with `pm path` for a device pending approval.
  // Skipping finishes the step: the light bar works without the plugin.
  async function runKioskPlugin(c) {
    const serial = (await c.shell('getprop ro.serialno')).trim();
    if (!serial) throw new Error('Could not read the device serial (ro.serialno).');
    const d = (await API.get('/api/devices')).find(x => x.device_id === serial) || null;
    if (!d) throw new Error(`${serial} is not known to this controller.`);
    const installed = d.connected && d.stats?.companion?.kiosk
      ? d.stats.companion.kiosk.installed === true
      : _kioskOnDevice(await c.shell(`pm path ${_KIOSK_PKG} 2>/dev/null`));
    if (!installed) {
      addLog('Kiosk Satellite is not installed: nothing to log in to.', LOG_TONE.OK);
      return;
    }
    const reason = d.connected ? kioskLoginReason(d) : (d.kioskTokenSet ? null : 'EchoMuse holds no Kiosk Satellite token for this Echo Show.');
    if (!reason) {
      addLog('Kiosk Satellite is installed and EchoMuse already holds its token.', LOG_TONE.OK);
      return;
    }
    addLog(`Kiosk Satellite is installed. ${reason}`);
    const loggedIn = await askKiosk({ serial, reason });
    addLog(loggedIn ? 'Kiosk Satellite token stored; the firmware installs the plugin with it.'
                    : 'Kiosk Satellite login skipped: the plugin is not installed until you log in from Config → Screen.',
           loggedIn ? LOG_TONE.OK : LOG_TONE.WARN);
  }
  function askKiosk(ask) {
    return new Promise(resolve => {
      kioskAnswer.current = resolve;
      setKioskAsk(ask);
    });
  }
  function answerKiosk(loggedIn) {
    const resolve = kioskAnswer.current;
    kioskAnswer.current = null;
    setKioskAsk(null);
    if (resolve) resolve(loggedIn);
  }

  // ── Step executor ──

  async function runStep(stepIdx) {
    // Captured up front. If the cable is pulled mid-step the handler above
    // bumps this and has already set the UI to a usable state, so everything
    // below must become a no-op — including the failure path, which would
    // otherwise spend ~100s probing a device that is not there.
    const epoch = stepEpoch.current;
    const abandoned = () => epoch !== stepEpoch.current;

    setRunning(true);
    markStep(stepIdx, STEP_STATE.RUNNING);
    // One banner per step. The transcript is the only record of a provision
    // and people paste it when something goes wrong — without these it is a
    // single 200-line stream with no way to tell which step a message
    // belongs to, or which one a failure happened in.
    // Numbered among the steps this device runs (all of them before
    // connect_android has classified it).
    const runs = _stepsFor(platformRef.current);
    addLog(`── ${runs.indexOf(steps[stepIdx].id) + 1}/${runs.length}  `
         + `${steps[stepIdx].label.toUpperCase()} ──`, LOG_TONE.HEAD);
    let c = adb;
    try {
      // Every step but the two connection steps needs a live handle. Passing
      // a null one through produced an error naming a property of undefined,
      // which says nothing about the cable having been unplugged.
      if (!CONNECT_STEPS.has(steps[stepIdx].id) && !c) {
        throw new Error('There is no ADB connection. Click Reconnect, pick the '
                      + 'device from the USB picker, then Retry this step.');
      }
      switch (steps[stepIdx].id) {
        case 'connect_android':   c = await runConnectAndroid(); break;
        case 'check_home':        await runCheckHome(c); break;
        case 'install_boot_hook': await runInstallBootHook(c); break;
        case 'install_em':        await runInstallEchoMuse(c); break;
        case 'wifi':              await runConfigWifi(c, wifiSsid, wifiPsk); break;
        case 'neutralize_alexa':  await runNeutralizeAlexa(c, hideTier2); break;
        case 'reboot':            await runReboot(c); break;
        case 'reconnect':         c = await runReconnect(); break;
        case 'verify_service':    await runVerifyService(c); break;
        case 'confirm_link':      await runConfirmLink(c); break;
        case 'mirror_boot_hook':  await runMirrorBootHook(c); break;
        case 'verify_companion':  await runVerifyCompanion(c); break;
        case 'kiosk_plugin':      await runKioskPlugin(c); break;
      }
      if (abandoned()) return;
      markStep(stepIdx, STEP_STATE.DONE);
      // The next step this device runs: connect_android has just said
      // which device it is, so platformRef, not the render's `platform`.
      const next = _nextStep(stepIdx + 1, platformRef.current);
      if (next < steps.length) setStep(next);
    } catch (e) {
      // A step abandoned mid-flight may still throw on its way out, once the
      // transport notices. The UI already says what happened; saying it again
      // in the language of whatever call happened to fail is noise.
      if (abandoned()) return;
      // The in-flight transfer can also throw BEFORE the disconnect event
      // arrives — the two race, and on a real run the throw won. That put
      // "Failed to execute 'transferOut' on 'USBDevice'" in the transcript as
      // though it were a provisioning failure, and then sent the diagnostics
      // probes at a device that was no longer plugged in. Take the same exit
      // the listener would have.
      if (_isDisconnectError(e)) {
        abandonStep('Device disconnected. The step was abandoned — reconnect and retry.', stepIdx);
        setAdb(null);
        return;
      }
      addLog(`Error: ${e.message}`, LOG_TONE.ERROR);
      markStep(stepIdx, STEP_STATE.ERROR);
      if (e.matchedDeviceId) setDuplicateDeviceId(e.matchedDeviceId);
      // Collect device state while it is still the state that failed. A
      // duplicate-device stop is our own bookkeeping and has nothing to ask
      // the device about.
      if (!e.matchedDeviceId) await captureDiagnostics(stepIdx, e);
    }
    if (!abandoned()) setRunning(false);
  }

  // Auto-advance steps that need no user input once adb is connected.
  useEffect(() => {
    if (!AUTO_STEPS.has(stepId) || running || stepState[step] !== STEP_STATE.PENDING) return;
    if (adb) { runStep(step); return; }
    addLog(`"${steps[step].label}" needs an ADB connection and there isn't one — `
         + `reconnect the device and click Retry.`, LOG_TONE.ERROR);
    markStep(step, STEP_STATE.ERROR);
  }, [step, running, adb]);

  const cur    = steps[step];
  const isDone = step === _lastStep(platform) && stepState[step] === STEP_STATE.DONE;

  // Buttons are shown for manual steps; auto steps start themselves.


  return (
    /* Same overlay + frame treatment as the Detail and Settings modals —
       warm blurred backdrop, fixed 900×700 frame, gradient header band,
       circular close button. */
    <div style={{
      position: 'fixed', inset: 0, zIndex: 200,
      background: 'rgba(180,176,168,0.5)', display: 'flex', alignItems: 'center', justifyContent: 'center',
      backdropFilter: 'blur(8px)',
    }}>
      <div role="dialog" aria-modal="true" aria-label="Provision Echo device" style={{
        background: 'linear-gradient(170deg,var(--raised),var(--surface))', border: '1px solid var(--border)',
        borderRadius: 16, width: 'min(900px,95vw)', height: 'min(700px,90vh)',
        display: 'flex', flexDirection: 'column', overflow: 'hidden',
        boxShadow: '0 24px 80px rgba(0,0,0,0.3),0 2px 0 var(--sheen) inset',
        animation: 'fadeIn 0.15s ease',
      }}>

        {/* Header */}
        <div style={{ background: 'linear-gradient(180deg,var(--card),var(--bg))', borderBottom: '1px solid var(--border-hard)', padding: '20px 24px 16px', boxShadow: '0 1px 0 var(--sheen) inset', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <div>
            <div style={{ fontFamily: SANS, fontSize: 22, fontWeight: 600, color: 'var(--text)', letterSpacing: '-0.02em' }}>
              {platform ? `Provision ${WIZARD_PLATFORM_NAME[platform]}` : 'Provision Echo device'}
            </div>
            <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', letterSpacing: '0.12em', textTransform: 'uppercase', marginTop: 4 }}>
              {platform === WIZARD_PLATFORM.CROWN
                ? 'Chrome/Edge only · USB cable · Fire OS 6 rooted via boot-root.img · on Wi-Fi · kiosk installed'
                : platform === WIZARD_PLATFORM.CROWN_LINEAGE
                  ? 'Chrome/Edge only · USB cable · LineageOS 18.1, our build · USB and Rooted debugging on · on Wi-Fi'
                : platform === WIZARD_PLATFORM.DOT
                  ? 'Chrome/Edge only · USB-A cable · Fire OS 6 rooted via boot-root.zip'
                  : 'Chrome/Edge only · USB cable · Echo Dot 2nd gen (Fire OS 6) or Echo Show 8 1st gen (Fire OS 6 or LineageOS 18.1), rooted'}
            </div>
          </div>
          <CircleButton onClick={onClose} title="Close">×</CircleButton>
        </div>

        <div style={{ display: 'flex', flex: 1, overflow: 'hidden', minHeight: 0 }}>

          {/* Step list */}
          <div style={{ width: 176, borderRight: '1px solid var(--border)', background: 'var(--hairline)', padding: '12px 0', overflowY: 'auto', flexShrink: 0 }}>
            {steps.map((s, i) => {
              const st = stepState[i]; const active = i === step;
              // The other device's steps, once connect_android has said
              // which device this is: shown, so both lists stay legible,
              // but struck through and never run.
              const na = !_stepApplies(s, platform);
              return (
                <div key={s.id}
                  title={na ? `Not applicable on the ${WIZARD_PLATFORM_NAME[platform]}` : undefined}
                  style={{
                    padding: '6px 14px', display: 'flex', alignItems: 'center', gap: 7,
                    background: active ? 'var(--hairline)' : 'transparent',
                    cursor: 'default',
                    opacity: na ? 0.4 : running && !active ? 0.5 : 1,
                  }}>
                  <span style={{ fontFamily: MONO, fontSize: 11, color: na ? 'var(--muted)' : STEP_STATE_STYLE[st].color, flexShrink: 0 }}>{na ? '–' : STEP_STATE_STYLE[st].icon}</span>
                  <span style={{ fontFamily: MONO, fontSize: 9, color: active ? 'var(--text)' : 'var(--muted)', letterSpacing: '0.04em', lineHeight: 1.4, textDecoration: na ? 'line-through' : 'none' }}>
                    {s.label}{na ? ' · n/a' : ''}
                  </span>
                </div>
              );
            })}
          </div>

          {/* Content */}
          <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden', padding: '18px 22px 14px' }}>

            {/* Step title + desc */}
            <div style={{ marginBottom: 14 }}>
              <div style={{ fontFamily: SANS, fontSize: 14, fontWeight: 600, color: 'var(--text)', marginBottom: 4 }}>{cur.label}</div>
              <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)' }}>{_stepDesc(cur, platform)}</div>
            </div>

            {/* connect_android is waiting on this answer (askReadopt). */}
            {readoptAsk && (
              <ReadoptPrompt {...readoptAsk}
                onKeep={() => answerReadopt(true)} onAbort={() => answerReadopt(false)}/>
            )}

            {/* ── Step-specific controls ── */}

            {/* kiosk_plugin is waiting on the password (askKiosk). */}
            {kioskAsk && (
              <KioskLogin deviceId={kioskAsk.serial} reason={kioskAsk.reason}
                onDone={() => answerKiosk(true)} onSkip={() => answerKiosk(false)}/>
            )}

            {/* Connect / reconnect buttons */}
            {CONNECT_STEPS.has(stepId) && stepState[step] === STEP_STATE.PENDING && !running && (
              <div style={{ marginBottom: 10 }}>
                <Pill onClick={() => runStep(step)}>
                  {stepId === 'connect_android' ? 'Connect Device' : 'Reconnect Device'}
                </Pill>
              </div>
            )}

            {/* Reboot button */}
            {stepId === 'reboot' && stepState[step] === STEP_STATE.PENDING && !running && (
              <div style={{ marginBottom: 10 }}>
                <Pill onClick={() => runStep(step)}>Reboot</Pill>
              </div>
            )}

            {/* Neutralise Alexa: the Tier 2 opt-in, then the button. The
                switch stays while the step can still run (and on Retry). */}
            {stepId === 'neutralize_alexa' && stepState[step] !== STEP_STATE.DONE && !running && (
              <div style={{ marginBottom: 10 }}>
                <Toggle
                  label="Also hide Tier 2"
                  sub="Amazon's cloud, smart-home radio and telemetry apps. Best once Tier 1 has been checked on this Show (Wi-Fi and Bluetooth still work from its Settings); unticked on a later run, they are unhidden."
                  value={hideTier2} onChange={setHideTier2}/>
                {stepState[step] === STEP_STATE.PENDING && (
                  <Pill onClick={() => runStep(step)}>Neutralise Alexa</Pill>
                )}
              </div>
            )}

            {/* WiFi configuration */}
            {stepId === 'wifi' && stepState[step] !== STEP_STATE.DONE && !running && (
              <WifiPanel
                adb={adb}
                wifiSsid={wifiSsid} setWifiSsid={setWifiSsid}
                wifiPsk={wifiPsk}   setWifiPsk={setWifiPsk}
                onScan={() => scanWifi(adb).then(nets => setWifiNetworks(nets)).catch(e => addLog(`Scan failed: ${e.message}`, LOG_TONE.ERROR))}
                networks={wifiNetworks}
                onConnect={() => { if (wifiSsid) runStep(step); }}
                onSkip={() => { markStep(step, STEP_STATE.DONE); setStep(_nextStep(step + 1, platform)); }}
                onAbort={() => { markStep(step, STEP_STATE.ERROR); addLog('WiFi skipped — provision incomplete.', LOG_TONE.WARN); }}
              />
            )}

            {/* The only control available while a step is in flight. The
                disconnect listener handles the cable being pulled, but a step
                can stall for reasons the browser does not report — a device
                that has stopped answering while still enumerated, say — and
                without this the only way out is reloading the page, which
                loses the transcript the operator would otherwise paste into
                an issue. */}
            {running && !readoptAsk && (
              <div style={{ marginBottom: 10, display: 'flex', gap: 8 }}>
                <Pill danger onClick={() => abandonStep('Step cancelled.')}>Cancel step</Pill>
              </div>
            )}

            {/* Retry button — re-runs the step directly (runStep marks it running).
                Excludes steps with their own dedicated retry UI above
                (WifiPanel) — that already gives a complete retry path with
                fresh input, so a second generic "Retry" here would just
                compete with it. */}
            {!running && stepState[step] === STEP_STATE.ERROR && !INPUT_STEPS.has(stepId) && (
              <div style={{ marginBottom: 10, display: 'flex', gap: 8 }}>
                <Pill onClick={() => runStep(step)}>Retry</Pill>
                {/* Reachable from every step, not just the connection ones.
                    Unplugging the cable is a normal reaction to something
                    going wrong, and it used to leave the wizard holding a dead
                    handle with no way to get a live one (#91). */}
                {!CONNECT_STEPS.has(stepId) && (
                  <Pill onClick={reconnectAdb}>{adb ? 'Reconnect' : 'Reconnect device'}</Pill>
                )}
                {/* Collection is automatic on failure; sharing is deliberate.
                    The file is redacted controller-side (no SSIDs, BSSIDs or
                    IPs) so it is safe to attach to a public issue, but it is
                    still the operator's call whether to. */}
                {diagnostics && (
                  <Pill onClick={downloadDiagnostics}>Download diagnostics</Pill>
                )}
                {stepId === 'connect_android' && duplicateDeviceId && (
                  <Pill danger onClick={async () => {
                    try {
                      await API.del(`/api/devices/${duplicateDeviceId}`);
                      addLog(`Deleted "${duplicateDeviceId}" from controller. You can retry now.`, LOG_TONE.OK);
                      setDuplicateDeviceId(null);
                      markStep(step, STEP_STATE.PENDING);
                    } catch (e) {
                      addLog(`Delete failed: ${e.error || e.message || 'unknown error'} — check /api/devices/{id} DELETE exists in em_api.py.`, LOG_TONE.ERROR);
                    }
                  }}>Delete "{duplicateDeviceId}" from controller</Pill>
                )}
              </div>
            )}

            {/* The input steps run their own buttons above and are excluded
                from the Retry block, which left them with no way to reconnect
                either. A dead handle is a dead handle whichever step is
                showing. */}
            {!running && stepState[step] === STEP_STATE.ERROR && INPUT_STEPS.has(stepId) && (
              <div style={{ marginBottom: 10, display: 'flex', gap: 8 }}>
                <Pill onClick={reconnectAdb}>{adb ? 'Reconnect' : 'Reconnect device'}</Pill>
                {diagnostics && (
                  <Pill onClick={downloadDiagnostics}>Download diagnostics</Pill>
                )}
              </div>
            )}

            {/* Progress bar — accent slate, same as toggles/sliders */}
            {progress && (
              <div style={{ margin: '6px 0 10px' }}>
                <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', marginBottom: 4 }}>{progress.label}</div>
                <div style={{ height: 4, background: 'var(--sunken)', borderRadius: 2 }}>
                  <div style={{ height: '100%', width: `${Math.min(100, (progress.pct || 0) * 100).toFixed(0)}%`, background: 'var(--accent)', borderRadius: 2, transition: 'width 0.2s' }}/>
                </div>
              </div>
            )}

            {/* Done message */}
            {isDone && (
              <div style={{ margin: '6px 0 10px', display: 'flex', flexDirection: 'column', gap: 10 }}>
                <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--ok)', lineHeight: 1.7 }}>
                  Provisioning complete. The device has rebooted and will discover the controller via mDNS,
                  {readopting
                    ? ` rejoining as "${readopting.label || readopting.device_id}" with its existing settings within ~30s.`
                    : ' appearing in the dashboard as a pending device within ~30s.'}
                </div>
                <div><Pill accent onClick={onClose}>Done</Pill></div>
              </div>
            )}

            {/* Log output — same console treatment as the Updates tab.
                The copy action matters more than it looks: this transcript is
                the entire record of a provision, and it is what gets pasted
                when something needs diagnosing. Selecting it by hand out of a
                scrolling box loses the top of it. */}
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', marginTop: 10 }}>
              <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.15em' }}>
                Output{log.length > 0 ? ` — ${log.length} lines` : ''}
              </span>
              {log.length > 0 && (
                <Pill small onClick={() => {
                  const text = log.map(e => e.msg).join('\n');
                  navigator.clipboard.writeText(text)
                    .then(() => addLog('(transcript copied to clipboard)'))
                    .catch(() => addLog('Clipboard blocked by the browser — select the text manually.', LOG_TONE.WARN));
                }}>Copy log</Pill>
              )}
            </div>
            <div
              ref={logRef}
              style={{
                flex: 1, minHeight: 0, overflowY: 'auto',
                background: 'linear-gradient(160deg,var(--lcd-face),var(--lcd-bg))',
                border: '1px solid var(--lcd-line)', borderRadius: 8,
                boxShadow: 'inset 0 2px 6px rgba(0,0,0,0.5)',
                padding: '10px 14px',
                fontFamily: MONO, fontSize: 10, lineHeight: 1.7,
                marginTop: 10,
              }}
            >
              {log.length === 0
                ? <span style={{ color: 'var(--lcd-faint)' }}>— no output yet —</span>
                : log.map((e, i) => (
                  // HEAD is a step banner, not output — it is what turns 200
                  // undifferentiated lines into something you can scan for the
                  // step that went wrong.
                  <div key={i} style={e.type === LOG_TONE.HEAD
                    ? { color: 'var(--accent-lit)', letterSpacing: '0.12em', marginTop: i === 0 ? 0 : 10, paddingTop: 6, borderTop: i === 0 ? 'none' : '1px solid var(--lcd-faint)' }
                    : { color: LOG_TONE_COLOR[e.type] || 'var(--lcd-green)' }}>
                    {e.msg}
                  </div>
                ))
              }
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

// ─── DeviceConfigForm ─────────────────────────────────────────────────────────
// Shared by the per-device Config tab and the fleet settings panel.
// The config rendered as the actual signal path: numbered stages from the
// microphones to the speaker, each labelled with WHERE it runs (device /
// controller) and WHAT it affects (wake stream / button turns / playback).
// Stage-specific advanced controls live inside their stage's disclosure, so
// "tucked away" never means "unclear what it belongs to".
// disabled=true = read-only (used when device is on global config).

// ScopeChip — small badge saying where a stage runs / what it affects.
function ScopeChip({ children, tone }) {
  const colors = {
    device:     { bg: 'rgba(40,96,64,0.12)',  border: 'rgba(40,96,64,0.35)',  text: 'var(--ok)' },
    controller: { bg: 'rgba(64,88,120,0.12)', border: 'rgba(64,88,120,0.35)', text: 'var(--accent)' },
    scope:      { bg: 'var(--hairline)',     border: 'rgba(0,0,0,0.15)',     text: 'var(--text2)' },
  }[tone || 'scope'];
  return (
    <span style={{
      fontFamily: MONO, fontSize: 8, textTransform: 'uppercase',
      letterSpacing: '0.1em', padding: '3px 8px', borderRadius: 4,
      background: colors.bg, border: `1px solid ${colors.border}`, color: colors.text,
      whiteSpace: 'nowrap',
    }}>{children}</span>
  );
}

// EqSliders — one vertical fader per band, ±12 dB. Live-updates eqBands so
// the curve above redraws as you drag.
function EqSliders({ bands, onChange, disabled }) {
  const FREQ_LABELS = ['125', '250', '500', '1k', '2k', '3.5k', '5.5k', '8k'];
  return (
    <div style={{ display: 'flex', justifyContent: 'space-between', gap: 2, ...(disabled ? { opacity: 0.45, pointerEvents: 'none' } : {}) }}>
      {bands.map((g, i) => (
        <div key={i} style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', flex: 1, minWidth: 0 }}>
          <div style={{ fontFamily: MONO, fontSize: 8, color: g !== 0 ? 'var(--accent)' : 'var(--muted)', marginBottom: 2, fontWeight: g !== 0 ? 600 : 400 }}>
            {(g > 0 ? '+' : '') + g}
          </div>
          {/* Native vertical slider via writing-mode — a rotate() transform
              renders fine but breaks drag gestures (pointer capture math
              stays in the untransformed axis, so only clicks land).
              orient="vertical" covers older Firefox. */}
          <input type="range" min={-12} max={12} step={1} value={g} orient="vertical"
            aria-label={`${FREQ_LABELS[i]} Hz band`}
            onChange={e => { const nb = [...bands]; nb[i] = Number(e.target.value); onChange(nb); }}
            style={{ writingMode: 'vertical-lr', direction: 'rtl', WebkitAppearance: 'slider-vertical', width: 20, height: 76, cursor: 'pointer' }}/>
          <div style={{ fontFamily: MONO, fontSize: 8, color: 'var(--muted)', marginTop: 2 }}>{FREQ_LABELS[i]}</div>
        </div>
      ))}
    </div>
  );
}

// Stage / StageAdvanced — module-scope so React preserves component
// identity across DeviceConfigForm renders (inner definitions would remount
// the subtree every render, breaking slider drags mid-gesture).

// Mirror of em_config_sections.SECTIONS — which config keys each stage owns,
// so a stage can be scoped to the fleet or to this device independently.
//
// Python is canonical. This literal is deliberately plain JSON (no comments,
// no trailing commas, double quotes) because tests/test_config_sections.py
// parses it straight out of this file and fails if the two ever drift — a
// control sitting under a toggle that does not govern it would look fine and
// be silently wrong.
const CONFIG_SECTIONS = {
  "playback": ["eqBands", "eqLoudness", "duckDb"],
  "wakeword": ["wakeModel", "nearMissThreshold", "saveWakeClips", "wakeArbitrationMs", "wakeSound", "wakeOpenRules", "wakeShadowRules"],
  "microphones": ["nsAsr", "saveUtterances", "extendedUtterances", "pauseAsr"],
  "ring": ["ledScene", "ledListenColor", "ledThinkColor", "meterAttack", "meterDecay", "meterFloor", "meterGamma", "meterRef", "meterCurve"],
  "advanced": ["buttonSingleTapEvent", "buttonMultiTapMs"],
  "bluetooth": ["bleProxyEnabled"],
  "timers": ["timerSound", "timerRingSeconds", "timerRingGapSeconds", "alarmSound"],
  "transcript": ["transcriptFix", "transcriptFixTerms", "transcriptFixPhrases", "transcriptFixPronunciations"],
  "screen": ["screenWake", "screenWakeUrl", "screenWakePassword", "kioskPort", "turnViewLinger"]
};

// Display labels for the section ids, and the reverse key -> section index
// that lets a write be gated by the section owning the key it touches.
// 'Ring' is the LED ring; 'Timers' is the alarm that rings — unrelated, and
// kept apart so a device can take its own alarm sound without forking its
// LED scene too.
const SECTION_LABELS = {
  playback: 'Playback', wakeword: 'Wake word', microphones: 'Speech',
  ring: 'Ring', advanced: 'Button', bluetooth: 'Bluetooth',
  timers: 'Timers', transcript: 'Transcript correction', screen: 'Screen',
};
const KEY_SECTION = {};
Object.entries(CONFIG_SECTIONS).forEach(([sid, keys]) => {
  keys.forEach(k => { KEY_SECTION[k] = sid; });
});
// Mirror of em_config_sections.STATE_KEYS — always the device's own,
// never fleet-inherited, whatever the section scoping says.
const STATE_KEYS = ['startupVolume'];

// Effective config = fleet, with the device's own values layered over it for
// the sections it overrides. This must FILTER rather than blind-merge
// device.config: a row migrated from the pre-v8 boolean still carries values
// for every section, so a plain merge shows a device's stale settings while
// every stage claims to be following the fleet (observed on Office, which
// displayed hey_rhasspy/standard while actually running hey_mycroft/malevolent).
function effectiveConfig(globalConfig, device) {
  const secs = device.config_sections ?? [];
  const own  = device.config || {};
  const out  = { ...(globalConfig || {}) };
  Object.keys(own).forEach(k => {
    const sec = KEY_SECTION[k];
    if ((sec && secs.includes(sec)) || STATE_KEYS.includes(k)) out[k] = own[k];
  });
  return out;
}

// ScopeToggle — per-stage Fleet/Device switch. Shown only on a device's
// config (the fleet view has nothing to inherit from).
// The two states are filled SOLID rather than tinted. At the 0.12-0.16 alpha
// the rest of the page uses, this sat beside the ScopeChips in the same
// colours and read as another chip — a label, not something you could press.
// Solid fill plus a "SCOPE" caption makes it legible as a control and makes
// the current state obvious across a scrolling page.
const SCOPE_FLEET  = 'var(--accent)';  // same blue as the "controller" ScopeChip
const SCOPE_DEVICE = 'var(--ok)';  // same green as the "device" ScopeChip

function ScopeToggle({ local, onChange, disabled }) {
  const btn = (active, label, next, title) => (
    <button
      type="button"
      title={title}
      disabled={disabled}
      onClick={() => !disabled && onChange(next)}
      style={{
        fontFamily: MONO, fontSize: 9, letterSpacing: '0.1em',
        textTransform: 'uppercase', padding: '4px 10px', border: 'none',
        cursor: disabled ? 'default' : 'pointer',
        background: active ? (next ? SCOPE_DEVICE : SCOPE_FLEET) : 'transparent',
        color: active ? 'var(--raised)' : 'var(--muted)',
        fontWeight: active ? 600 : 400,
        transition: 'background 0.15s, color 0.15s',
      }}>{label}</button>
  );
  return (
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 7, opacity: disabled ? 0.5 : 1 }}>
      <span style={{
        fontFamily: MONO, fontSize: 8, letterSpacing: '0.12em',
        textTransform: 'uppercase', color: 'var(--muted)',
      }}>Scope</span>
      <span style={{
        display: 'inline-flex', borderRadius: 6, overflow: 'hidden',
        border: `1px solid ${local ? SCOPE_DEVICE : SCOPE_FLEET}`,
        background: 'var(--hairline)',
      }}>
        {btn(!local, 'Fleet', false, 'This section follows the fleet-wide setting')}
        {btn(local, 'Device', true, 'Override this section for this device only')}
      </span>
    </span>
  );
}

function Stage({ n, title, chips, desc, children, scope, dim, after }) {
  return (
    <Panel>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, marginBottom: 6, flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', alignItems: 'baseline', gap: 10 }}>
          <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)' }}>{n}</span>
          <span style={{ fontFamily: SANS, fontSize: 14, fontWeight: 600, color: 'var(--text)' }}>{title}</span>
        </div>
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>{chips}{scope}</div>
      </div>
      <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', lineHeight: 1.6, marginBottom: 14 }}>{desc}</div>
      {/* dim: a section following the fleet is shown read-only rather than
          hidden, so you can still see what it is inheriting. `after` is
          read-only content that stays usable whatever the scope. */}
      <div style={dim}>{children}</div>
      {after}
    </Panel>
  );
}

function StageAdvanced({ open, onToggle, disabledStyle, children }) {
  return (
    <div style={{ marginTop: 14, borderTop: '1px solid var(--hairline)', paddingTop: 10 }}>
      <DisclosureToggle open={open} onToggle={onToggle}>Advanced</DisclosureToggle>
      {open && <div style={{ marginTop: 14, ...disabledStyle }}>{children}</div>}
    </div>
  );
}

function WakeModelPicker({ value, onChange, disabled }) {
  const [registry, reload] = useWakeRegistry();
  const [uploadOpen, setUploadOpen] = useState(false);
  const [graph, setGraph] = useState(null);
  const [sidecar, setSidecar] = useState(null);
  const [wakePhrase, setWakePhrase] = useState('ophelia');
  const [verifyCore, setVerifyCore] = useState('ophel');
  const [thresholds, setThresholds] = useState({
    idle:0.90, playback:0.65, reference:0.30,
  });
  const [uploading, setUploading] = useState(false);
  const [validation, setValidation] = useState(null);

  async function upload() {
    if (!graph || !sidecar) {
      setValidation({ ok:false, text:'Choose both the BCResNet .onnx graph and its .json sidecar.' });
      return;
    }
    setUploading(true); setValidation(null);
    try {
      const form = new FormData();
      form.append('graph', graph);
      form.append('sidecar', sidecar);
      Object.entries(thresholds).forEach(([k, v]) => form.append(k, String(v)));
      form.append('wake_phrase', wakePhrase.trim().toLowerCase());
      form.append('verify_core', verifyCore.trim().toLowerCase());
      const r = await API.postForm('/api/wake_models/upload', form);
      await reload();
      const probe = Math.max(...Object.values(r.model?.probe || {}).map(Number).filter(Number.isFinite), 0);
      setValidation({ ok:true, text:`Validated ${shortSha(r.model?.graph_sha256)} · probe max ${probe.toFixed(4)}. Select it below to activate.` });
      setGraph(null); setSidecar(null);
    } catch (e) {
      setValidation({ ok:false, text:e.error || 'Model validation failed' });
    }
    setUploading(false);
  }

  async function remove(model) {
    if (!confirm(`Delete registry model ${model.wake_phrase} (${shortSha(model.graph_sha256)})?`)) return;
    try {
      await API.del(`/api/wake_models/${encodeURIComponent(model.graph_sha256)}`);
      await reload();
    } catch (e) { setValidation({ ok:false, text:e.error || 'Model is still in use' }); }
  }

  const missing = registry.loaded && value && !registry.models.some(m => m.graph_sha256 === value);
  return (
    <div>
      <div style={{ display:'grid', gridTemplateColumns:'repeat(auto-fill,minmax(190px,1fr))', gap:8 }}>
        {registry.models.map(m => {
          const selected = value === m.graph_sha256;
          const probe = Math.max(...Object.values(m.probe || {}).map(Number).filter(Number.isFinite), 0);
          return (
            <div key={m.graph_sha256} {...pressable(() => onChange(m.graph_sha256), { disabled, selected })} style={{
              background:selected ? 'linear-gradient(160deg,var(--accent-tint),var(--accent-line))' : 'linear-gradient(160deg,var(--raised),var(--surface))',
              border:`1px solid ${selected ? 'var(--accent)' : 'var(--border-soft)'}`,
              borderRadius:8, padding:'10px 12px', cursor:disabled ? 'default' : 'pointer', position:'relative',
            }}>
              <div style={{ display:'flex', justifyContent:'space-between', gap:8 }}>
                <div style={{ fontFamily:SANS, fontSize:12, fontWeight:600, color:'var(--text2)' }}>{m.wake_phrase}</div>
                {m.active && <span style={{ fontFamily:MONO, fontSize:8, color:'var(--ok)', textTransform:'uppercase' }}>active</span>}
              </div>
              <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:3 }}>graph {shortSha(m.graph_sha256)} · sidecar {shortSha(m.sidecar_sha256)}</div>
              <div style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)', marginTop:6, lineHeight:1.5 }}>
                idle {m.thresholds.idle.toFixed(2)} · playback {m.thresholds.playback.toFixed(2)}<br/>
                reference {m.thresholds.reference.toFixed(2)}<br/>
                validation probe max {probe.toFixed(4)}
              </div>
              {!disabled && !selected && !(m.in_use_by || []).length && (
                <button type="button" onClick={e => { e.stopPropagation(); remove(m); }} title="Delete model"
                  aria-label={`Delete model ${m.wake_phrase}`}
                  style={{ position:'absolute', right:7, bottom:5, background:'none', border:'none', color:'var(--muted)', cursor:'pointer' }}>×</button>
              )}
            </div>
          );
        })}
        {missing && (
          <div style={{ border:'1px solid var(--error)', borderRadius:8, padding:'10px 12px' }}>
            <div style={{ fontFamily:MONO, fontSize:10, color:'var(--error)' }}>Missing registry model</div>
            <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:3 }}>{value}</div>
          </div>
        )}
        <div {...pressable(() => setUploadOpen(x => !x), { disabled })} aria-expanded={uploadOpen} style={{
          border:'1px dashed var(--border-hard)', borderRadius:8, padding:'10px 12px',
          cursor:disabled ? 'default' : 'pointer', background:'var(--surface)',
        }}>
          <div style={{ fontFamily:SANS, fontSize:12, fontWeight:600, color:'var(--text2)' }}>+ BCResNet model</div>
          <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:3 }}>upload and validate graph + sidecar</div>
        </div>
      </div>
      {uploadOpen && (
        <div style={{ marginTop:12, border:'1px solid var(--border-soft)', borderRadius:8, padding:12 }}>
          <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:12 }}>
            <label style={{ fontFamily:MONO, fontSize:9, color:'var(--text2)' }}>ONNX graph
              <input type="file" accept=".onnx" onChange={e => setGraph(e.target.files[0] || null)} style={{ display:'block', marginTop:5, maxWidth:'100%' }}/>
            </label>
            <label style={{ fontFamily:MONO, fontSize:9, color:'var(--text2)' }}>JSON sidecar
              <input type="file" accept=".json" onChange={e => setSidecar(e.target.files[0] || null)} style={{ display:'block', marginTop:5, maxWidth:'100%' }}/>
            </label>
            <label style={{ fontFamily:MONO, fontSize:9, color:'var(--text2)' }}>Wake phrase
              <input value={wakePhrase} onChange={e => setWakePhrase(e.target.value)} style={{ display:'block', width:'100%', marginTop:5, boxSizing:'border-box' }}/>
            </label>
            <label style={{ fontFamily:MONO, fontSize:9, color:'var(--text2)' }}>Verification core
              <input value={verifyCore} onChange={e => setVerifyCore(e.target.value)} style={{ display:'block', width:'100%', marginTop:5, boxSizing:'border-box' }}/>
            </label>
          </div>
          <div style={{ display:'grid', gridTemplateColumns:'repeat(3,1fr)', gap:8, marginTop:12 }}>
            {Object.entries(thresholds).map(([key, v]) => (
              <label key={key} style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)' }}>{key}
                <input type="number" min="0.01" max="0.99" step="0.01" value={v}
                  onChange={e => setThresholds(t => ({ ...t, [key]:Number(e.target.value) }))}
                  style={{ display:'block', width:'100%', marginTop:4, boxSizing:'border-box' }}/>
              </label>
            ))}
          </div>
          <div style={{ display:'flex', alignItems:'center', gap:10, marginTop:12 }}>
            <Pill accent disabled={uploading || !graph || !sidecar} onClick={upload}>{uploading ? 'Validating…' : 'Validate & add'}</Pill>
            <span style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>Validation checks graph IO, sidecar agreement and silence/noise/tone probes.</span>
          </div>
          {validation && <div style={{ marginTop:10, fontFamily:MONO, fontSize:9, color:validation.ok ? 'var(--ok)' : 'var(--error)' }}>{validation.text}</div>}
        </div>
      )}
    </div>
  );
}

// em_wake_rules limits: extra open rules beside the two baseline rules, and
// shadow rules. The controller validates every write against the same ones.
const MAX_EXTRA_OPEN_RULES = 4;
const MAX_SHADOW_RULES = 8;
const NEW_WAKE_RULE = Object.freeze({
  profile: RULE_PROFILE.IDLE, windows: 2, combine: RULE_COMBINE.MEAN, threshold: 0.95,
});

// An editable list of wake rules ({profile, windows, combine, threshold}),
// shared by the extra open rules and the shadow rules.
function WakeRuleRows({ rules, onChange, max, disabled }) {
  const list = Array.isArray(rules) ? rules : [];
  const put = (i, patch) => onChange(list.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const label = { fontFamily:MONO, fontSize:8, color:'var(--muted)' };
  const field = { display:'block', width:'100%', marginTop:4, boxSizing:'border-box' };
  return (
    <div>
      {list.map((r, i) => (
        <div key={i} style={{ display:'grid', gridTemplateColumns:'1fr 0.7fr 1.2fr 0.9fr auto', gap:8, alignItems:'end', marginTop:8 }}>
          <label style={label}>profile
            <select value={r.profile} disabled={disabled} onChange={e => put(i, { profile:e.target.value })} style={field}>
              {Object.values(RULE_PROFILE).map(p => <option key={p} value={p}>{p}</option>)}
            </select>
          </label>
          <label style={label}>windows
            <select value={r.windows} disabled={disabled} onChange={e => put(i, { windows:Number(e.target.value) })} style={field}>
              {[1, 2, 3].map(n => <option key={n} value={n}>{n}</option>)}
            </select>
          </label>
          <label style={label}>combine
            <select value={r.combine} disabled={disabled} onChange={e => put(i, { combine:e.target.value })} style={field}>
              {Object.values(RULE_COMBINE).map(c => <option key={c} value={c}>{RULE_COMBINE_LABEL[c]}</option>)}
            </select>
          </label>
          <label style={label}>threshold
            <input type="number" min="0.30" max="0.99" step="0.01" value={r.threshold} disabled={disabled}
              onChange={e => put(i, { threshold:Math.round(Number(e.target.value) * 100) / 100 })} style={field}/>
          </label>
          <button type="button" disabled={disabled} onClick={() => onChange(list.filter((_, j) => j !== i))}
            title="Remove rule" aria-label={`Remove rule ${wakeRuleText(r)}`}
            style={{ background:'none', border:'none', color:'var(--muted)', cursor:disabled ? 'default' : 'pointer', paddingBottom:4 }}>×</button>
        </div>
      ))}
      <div style={{ display:'flex', alignItems:'center', gap:10, marginTop:10 }}>
        <Pill small disabled={disabled || list.length >= max} onClick={() => onChange([...list, { ...NEW_WAKE_RULE }])}>+ Add rule</Pill>
        <span style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>{list.length} of {max}</span>
      </div>
    </div>
  );
}

// Wake open rules (§5.2): the selected model's two baseline rules, always on
// and read-only, then the extra open rules and the shadow rules the Echo
// evaluates without acting on. `gap` is why the device in scope cannot take
// them (no open_rules_v1), shown with both lists disabled.
function WakeRulesEditor({ config, set, disabled, gap }) {
  const [registry, reload] = useWakeRegistry();
  const sha = config.wakeModel || registry.active;
  const model = registry.models.find(m => m.graph_sha256 === sha);
  // A model uploaded and selected in the picker after this copy was read.
  useEffect(() => { if (registry.loaded && sha && !model) reload(); }, [sha]);
  const thr = p => (model ? model.thresholds[p].toFixed(2) : '—');
  const off = disabled || !!gap;
  const title = { fontFamily:MONO, fontSize:11, color:off ? 'var(--muted)' : 'var(--text2)' };
  const sub = { fontFamily:MONO, fontSize:10, color:'var(--muted)', marginLeft:8 };
  return (
    <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', marginTop:4 }}>
      <div>
        <span style={title}>Open rules</span>
        <span style={sub}>a wake opens when any rule for the current profile fires</span>
        <div style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)', marginTop:8, paddingBottom:6, borderBottom:'1px solid var(--hairline)' }}>
          3-window average ≥ {thr(RULE_PROFILE.IDLE)} idle / {thr(RULE_PROFILE.PLAYBACK)} playback
          <span style={{ color:'var(--muted)' }}> — from the model, always on</span>
        </div>
        <WakeRuleRows rules={config.wakeOpenRules} max={MAX_EXTRA_OPEN_RULES} disabled={off}
          onChange={v => set('wakeOpenRules', v)}/>
      </div>
      <div>
        <span style={title}>Shadow rules</span>
        <span style={sub}>evaluated on the Echo and scored on Status; they never open a wake</span>
        <WakeRuleRows rules={config.wakeShadowRules} max={MAX_SHADOW_RULES} disabled={off}
          onChange={v => set('wakeShadowRules', v)}/>
      </div>
      <div style={{ gridColumn:'1 / -1', marginTop:10, fontFamily:MONO, fontSize:9, color:gap ? 'var(--warn)' : 'var(--muted)' }}>
        {gap ? `Open and shadow rules unavailable: ${gap}`
          : 'Shadow rules never change behaviour. Saving either list reconnects the Echo for a moment.'}
      </div>
    </div>
  );
}

// Pause transcription (§16.6), config `pauseAsr`. Shape and limits mirror
// em_pause_asr.parse_pause_asr; the API rejects anything else. "Check" asks the
// server what it offers (admin only: the controller connects to that address).
const PAUSE_ASR_DEFAULT = Object.freeze({
  engine: PAUSE_ASR_ENGINE.KROKO, host: '', port: 10300, model: '', language: 'en',
});
function PauseAsrControls({ value, onChange }) {
  const v = { ...PAUSE_ASR_DEFAULT, ...(value || {}) };
  const put = patch => onChange({ ...v, ...patch });
  const remote = v.engine === PAUSE_ASR_ENGINE.WYOMING;
  const [server, setServer] = useState(null);   // {ok, text, models}
  const listId = useRef(`pause-asr-models-${Math.random().toString(36).slice(2)}`).current;
  async function check() {
    setServer({ ok: null, text: 'checking…', models: [] });
    try {
      const r = await API.get(`/api/speech/wyoming?host=${encodeURIComponent(v.host)}&port=${v.port}`);
      const programs = r.programs || [];
      setServer({
        ok: programs.length > 0,
        text: programs.length
          ? 'reachable: ' + programs.map(p => `${p.name}${p.version ? ` ${p.version}` : ''}`).join(', ')
          : 'reachable, but it offers no speech-to-text',
        models: programs.flatMap(p => p.models.map(m => m.name)),
      });
    } catch (e) {
      setServer({ ok: false, text: e.error || 'unreachable', models: [] });
    }
  }
  const label = { fontFamily:MONO, fontSize:8, color:'var(--muted)' };
  const field = { display:'block', width:'100%', marginTop:4, boxSizing:'border-box' };
  return (
    <div style={{ marginTop: 16 }}>
      <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)' }}>Pause transcription</span>
      <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginLeft:8 }}>
        at each pause Kroko re-decodes the request so far; a Wyoming speech-to-text server (Home
        Assistant's, for example) can transcribe the same audio. Nothing waits for it: whether the
        request is complete is judged on Kroko's words at once, then on the server's when they arrive
      </span>
      <div style={{ display:'grid', gridTemplateColumns:'1.1fr 1.6fr 0.7fr 1.8fr 0.7fr auto', gap:8, alignItems:'end', marginTop:8 }}>
        <label style={label}>engine
          <select value={v.engine} onChange={e => put({ engine:e.target.value })} style={field}>
            <option value={PAUSE_ASR_ENGINE.KROKO}>Kroko (local)</option>
            <option value={PAUSE_ASR_ENGINE.WYOMING}>Wyoming server</option>
          </select>
        </label>
        <label style={label}>host
          <input type="text" value={v.host} disabled={!remote} placeholder="wyoming-faster-whisper"
            onChange={e => { put({ host:e.target.value.trim() }); setServer(null); }} style={field}/>
        </label>
        <label style={label}>port
          <input type="number" min="1" max="65535" step="1" value={v.port} disabled={!remote}
            onChange={e => { put({ port:Math.round(Number(e.target.value)) }); setServer(null); }} style={field}/>
        </label>
        <label style={label}>model
          <input type="text" list={listId} value={v.model} disabled={!remote} placeholder="server default"
            onChange={e => put({ model:e.target.value })} style={field}/>
          <datalist id={listId}>{(server?.models || []).map(m => <option key={m} value={m}/>)}</datalist>
        </label>
        <label style={label}>language
          <input type="text" value={v.language} disabled={!remote} placeholder="server default"
            onChange={e => put({ language:e.target.value.trim() })} style={field}/>
        </label>
        <button type="button" disabled={!remote || !v.host} onClick={check}>Check</button>
      </div>
      {remote && server && (
        <div style={{ fontFamily:MONO, fontSize:10, marginTop:6,
          color: server.ok === null ? 'var(--muted)' : server.ok ? 'var(--ok)' : 'var(--error)' }}>
          {server.text}
        </div>
      )}
    </div>
  );
}

// Transcript correction (§16.8), config `transcriptFix`.
const TEXT_FIX_MODE_OPTIONS = Object.freeze([
  [TEXT_FIX_MODE.OFF, 'Off'], [TEXT_FIX_MODE.SHADOW, 'Shadow'], [TEXT_FIX_MODE.ON, 'On'],
]);
const TEXT_FIX_MODE_HELP = Object.freeze({
  [TEXT_FIX_MODE.OFF]:    'no correction work: every request goes to Home Assistant as heard',
  [TEXT_FIX_MODE.SHADOW]: 'records what On would have sent on each turn in Activity, without changing what is sent',
  [TEXT_FIX_MODE.ON]:     "sends a correction only when Home Assistant's sentence matcher accepts it and it is ready within 100 ms of the final transcript; otherwise the request as heard",
});

// `transcriptFixTerms` / `transcriptFixPhrases`: ordered, highest preference
// first, exactly in the stored order. The checks mirror
// em_text_fix.parse_list, so a list the API would refuse never reaches the
// config: an entry is committed only once it passes, and until then the
// reason stands beside the field. Display case is kept; duplicates are judged
// on letters and digits alone, casefolded (em_text_fix.match_key), so
// "YouTube TV" duplicates "youtube t.v.".
const textFixEntry = s => String(s).split(/\s+/).filter(Boolean).join(' ');
const textFixKey = s => s.toLowerCase().replace(/ß/g, 'ss').replace(/[^a-z0-9]/g, '');
function textFixEntryError(entry, list, self = -1) {
  const key = textFixKey(entry);
  if (!key) return 'entries must contain a letter or digit';
  if ([...entry].length > TEXT_FIX_MAX_CHARS) return `entries are at most ${TEXT_FIX_MAX_CHARS} characters`;
  const dup = list.findIndex((x, i) => i !== self && textFixKey(textFixEntry(x)) === key);
  return dup < 0 ? null : `“${entry}” duplicates “${list[dup]}” (#${dup + 1})`;
}
function textFixListError(list) {
  if (list.length > TEXT_FIX_MAX_ENTRIES) return `at most ${TEXT_FIX_MAX_ENTRIES} entries`;
  for (let i = 0; i < list.length; i++) {
    if (typeof list[i] !== 'string') return 'must be a list of strings';
    const err = textFixEntryError(textFixEntry(list[i]), list.slice(0, i));
    if (err) return err;
  }
  return null;
}

// One ordered list: add at the bottom, click an entry to edit it (Enter or
// leaving the field commits, Escape cancels), × removes, and drag a row
// (HTML5 drag-and-drop) or press its ↑/↓ to reorder. A drop puts the dragged
// entry at the target row's place; the accent line shows which side.
function TextFixListEditor({ label, sub, noun, placeholder, items, onChange, disabled }) {
  const list = Array.isArray(items) ? items : [];
  const [draft, setDraft] = useState('');
  const [draftError, setDraftError] = useState(null);
  const [editing, setEditing] = useState(null);   // { index, text, error }
  const [drag, setDrag] = useState(null);         // { from, over }
  const listKey = list.join('\n');
  // A list replaced underneath (scope switch, fleet reload) ends any edit.
  useEffect(() => { setEditing(null); setDrag(null); }, [listKey]);
  const listError = textFixListError(list);
  const full = list.length >= TEXT_FIX_MAX_ENTRIES;
  const busy = disabled || editing != null;
  const commit = next => { if (!textFixListError(next)) onChange(next); };

  function add() {
    const entry = textFixEntry(draft);
    if (!entry) return;
    const err = full ? `at most ${TEXT_FIX_MAX_ENTRIES} entries` : textFixEntryError(entry, list);
    if (err) { setDraftError(err); return; }
    commit([...list, entry]);
    setDraft(''); setDraftError(null);
  }
  function saveEdit() {
    if (!editing) return;
    const entry = textFixEntry(editing.text);
    if (entry === list[editing.index]) { setEditing(null); return; }
    const err = textFixEntryError(entry, list, editing.index);
    if (err) { setEditing({ ...editing, error: err }); return; }
    commit(list.map((x, i) => (i === editing.index ? entry : x)));
    setEditing(null);
  }
  function move(from, to) {
    if (from === to || to < 0 || to >= list.length) return;
    const next = [...list];
    next.splice(to, 0, next.splice(from, 1)[0]);
    commit(next);
  }
  function remove(i) { setEditing(null); commit(list.filter((_, j) => j !== i)); }

  const field = { display:'block', width:'100%', boxSizing:'border-box' };
  const error = { fontFamily:MONO, fontSize:9, color:'var(--error)', marginTop:3 };
  const glyph = on => ({ background:'none', border:'none', padding:'0 3px', fontFamily:MONO, fontSize:11,
    color:on ? 'var(--text2)' : 'var(--muted)', opacity:on ? 1 : 0.4, cursor:on ? 'pointer' : 'default' });
  const line = edge => `2px solid ${edge ? 'var(--accent)' : 'transparent'}`;
  return (
    <div style={{ marginBottom:16 }}>
      <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)' }}>{label}</span>
      <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginLeft:8 }}>{sub}</span>
      <div role="list" aria-label={label} style={{ marginTop:8 }}>
        {list.length === 0 && <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', padding:'3px 0' }}>none</div>}
        {list.map((x, i) => {
          const isEditing = editing?.index === i;
          const target = drag != null && drag.over === i && drag.from !== i;
          return (
            <div key={`${i}:${x}`} role="listitem"
              draggable={!busy}
              onDragStart={e => {
                e.dataTransfer.effectAllowed = 'move';
                e.dataTransfer.setData('text/plain', x);   // Firefox starts no drag without data
                setDrag({ from: i, over: i });
              }}
              onDragOver={e => {
                if (!drag) return;   // a drag from elsewhere is not a reorder
                e.preventDefault();
                e.dataTransfer.dropEffect = 'move';
                if (drag.over !== i) setDrag({ ...drag, over: i });
              }}
              onDrop={e => { if (!drag) return; e.preventDefault(); move(drag.from, i); setDrag(null); }}
              onDragEnd={() => setDrag(null)}
              style={{ display:'grid', gridTemplateColumns:'12px 30px 1fr auto', gap:6, alignItems:'center', padding:'2px 0',
                borderTop:line(target && drag.from > i), borderBottom:line(target && drag.from < i),
                opacity:drag?.from === i ? 0.4 : 1 }}>
              <span aria-hidden="true" title={busy ? undefined : 'Drag to reorder'}
                style={{ fontFamily:MONO, fontSize:11, color:'var(--muted)', cursor:busy ? 'default' : 'grab' }}>⠿</span>
              <span title={i === 0 ? 'highest preference' : undefined}
                style={{ fontFamily:MONO, fontSize:9, textAlign:'right', color:i === 0 ? 'var(--accent)' : 'var(--muted)' }}>#{i + 1}</span>
              {isEditing ? (
                <div>
                  <input type="text" autoFocus value={editing.text} aria-label={`Edit ${noun} #${i + 1}`}
                    aria-invalid={!!editing.error}
                    onChange={e => setEditing({ index: i, text: e.target.value, error: null })}
                    onKeyDown={e => {
                      if (e.key === 'Enter') { e.preventDefault(); saveEdit(); }
                      else if (e.key === 'Escape') { e.preventDefault(); setEditing(null); }
                    }}
                    onBlur={saveEdit} style={field}/>
                  {editing.error && <div style={error}>{editing.error} — not saved</div>}
                </div>
              ) : (
                <span {...pressable(() => setEditing({ index: i, text: x, error: null }), { disabled: busy })}
                  title={busy ? undefined : 'Edit'} aria-label={`Edit ${noun} #${i + 1}: ${x}`}
                  style={{ fontFamily:SANS, fontSize:12, color:'var(--text)', overflowWrap:'anywhere', cursor:busy ? 'default' : 'text' }}>{x}</span>
              )}
              <span style={{ display:'flex', gap:2 }}>
                <button type="button" disabled={busy || i === 0} onClick={() => move(i, i - 1)}
                  title="Move up (higher preference)" aria-label={`Move ${noun} “${x}” up`} style={glyph(!busy && i > 0)}>↑</button>
                <button type="button" disabled={busy || i === list.length - 1} onClick={() => move(i, i + 1)}
                  title="Move down (lower preference)" aria-label={`Move ${noun} “${x}” down`} style={glyph(!busy && i < list.length - 1)}>↓</button>
                {/* mousedown kept from blurring an open edit, which would commit it first. */}
                <button type="button" disabled={disabled} onMouseDown={e => e.preventDefault()} onClick={() => remove(i)}
                  title="Remove" aria-label={`Remove ${noun} “${x}”`} style={glyph(!disabled)}>×</button>
              </span>
            </div>
          );
        })}
      </div>
      <div style={{ display:'grid', gridTemplateColumns:'1fr auto', gap:8, alignItems:'center', marginTop:8 }}>
        <input type="text" value={draft} disabled={disabled || full} placeholder={placeholder}
          aria-label={`Add a ${noun}`} aria-invalid={!!draftError}
          onChange={e => { setDraft(e.target.value); setDraftError(null); }}
          onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); add(); } }} style={field}/>
        <Pill small disabled={disabled || full || !textFixEntry(draft)} onClick={add}>+ Add</Pill>
      </div>
      {draftError && <div style={error}>{draftError}</div>}
      <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', marginTop:4 }}>
        {list.length} of {TEXT_FIX_MAX_ENTRIES}{full ? ' (full)' : ''} · #1 weighs most; drag a row or use ↑ ↓ to reorder
      </div>
      {listError && <div style={error}>The stored list is invalid ({listError}); edits are held until it is fixed.</div>}
    </div>
  );
}

const TEXT_FIX_NOT_READY_RETRY_MS = 3000;

// `transcriptFixPronunciations`: {word: ARPAbet}, the pronunciation the correction compares a
// word by, in place of CMUdict's (em_phonetics). The checks mirror em_phonetics.parse_guides.
// Below the guides, the words of Home Assistant's trigger sentences, earlier requests and names
// that have no pronunciation under the saved guides (GET /api/text_fix/unknown_words): text
// with one is never compared by sound; clicking one starts its guide.
const ARPABET = Object.freeze([   // CMU Pronouncing Dictionary symbols, with CMU's example words
  ['AA', 'odd'], ['AE', 'at'], ['AH', 'hut'], ['AO', 'ought'], ['AW', 'cow'], ['AY', 'hide'], ['B', 'be'],
  ['CH', 'cheese'], ['D', 'dee'], ['DH', 'thee'], ['EH', 'Ed'], ['ER', 'hurt'], ['EY', 'ate'], ['F', 'fee'],
  ['G', 'green'], ['HH', 'he'], ['IH', 'it'], ['IY', 'eat'], ['JH', 'gee'], ['K', 'key'], ['L', 'lee'],
  ['M', 'me'], ['N', 'knee'], ['NG', 'ping'], ['OW', 'oat'], ['OY', 'toy'], ['P', 'pee'], ['R', 'read'],
  ['S', 'sea'], ['SH', 'she'], ['T', 'tea'], ['TH', 'theta'], ['UH', 'hood'], ['UW', 'two'], ['V', 'vee'],
  ['W', 'we'], ['Y', 'yield'], ['Z', 'zee'], ['ZH', 'seizure'],
]);
const ARPABET_SYMBOLS = new Set([...ARPABET.map(([s]) => s), 'AX']);
const TEXT_FIX_MAX_GUIDES = 200;
const TEXT_FIX_MAX_GUIDE_PHONEMES = 30;
const TEXT_FIX_MAX_WORD_CHARS = 40;
const guideWord = s => String(s).trim().toLowerCase().replace(/\u2019/g, "'");
const guidePhonemes = s => String(s).trim().split(/\s+/).filter(Boolean).map(p => p.toUpperCase()).join(' ');
function guideError(word, phonemes, guides, self = null) {
  if (!/^[a-z0-9]+(?:'[a-z]+)?$/.test(word) || word.length > TEXT_FIX_MAX_WORD_CHARS)
    return `“${word}” is not one word of letters and digits (at most ${TEXT_FIX_MAX_WORD_CHARS})`;
  if (word !== self && Object.keys(guides).some(w => guideWord(w) === word)) return `“${word}” already has a pronunciation`;
  const symbols = phonemes.split(' ').filter(Boolean);
  if (!symbols.length) return 'type its phonemes, e.g. P L EH K S';
  const bad = symbols.find(p => { const m = /^([A-Z]+)([012]?)$/.exec(p); return !m || !ARPABET_SYMBOLS.has(m[1]); });
  if (bad) return `unknown phoneme “${bad}”`;
  if (symbols.length > TEXT_FIX_MAX_GUIDE_PHONEMES) return `at most ${TEXT_FIX_MAX_GUIDE_PHONEMES} phonemes`;
  return null;
}

function TextFixPronunciationEditor({ guides, onChange, disabled, deviceId }) {
  const map = guides && typeof guides === 'object' && !Array.isArray(guides) ? guides : {};
  const words = Object.keys(map).sort();
  const [draft, setDraft] = useState({ word: '', phonemes: '', error: null });
  const [editing, setEditing] = useState(null);   // { word, phonemes, error }
  const [unknown, setUnknown] = useState(null);   // [{word, count}] | {error}
  const [showAll, setShowAll] = useState(false);
  const [reference, setReference] = useState(false);
  const phonemesRef = useRef(null);
  const saved = JSON.stringify(map);
  const full = words.length >= TEXT_FIX_MAX_GUIDES;

  // Not ready (tables rebuilding after a Home Assistant change, or HA reconnecting): ask again
  // every TEXT_FIX_NOT_READY_RETRY_MS while this is shown.
  useEffect(() => {
    let live = true, timer = null;
    const params = deviceId ? `?device_id=${encodeURIComponent(deviceId)}` : '';
    const load = () => API.get(`/api/text_fix/unknown_words${params}`)
      .then(r => { if (live) setUnknown(r.words || []); })
      .catch(e => {
        if (!live) return;
        setUnknown({ error: e?.error || e?.message || 'unavailable' });
        if (e?.code === 'not_ready') timer = setTimeout(load, TEXT_FIX_NOT_READY_RETRY_MS);
      });
    load();
    return () => { live = false; clearTimeout(timer); };
  }, [deviceId, saved]);
  useEffect(() => { setEditing(null); }, [saved]);

  function add() {
    const word = guideWord(draft.word), phonemes = guidePhonemes(draft.phonemes);
    if (!word) return;
    const err = full ? `at most ${TEXT_FIX_MAX_GUIDES} words` : guideError(word, phonemes, map);
    if (err) { setDraft({ ...draft, error: err }); return; }
    onChange({ ...map, [word]: phonemes });
    setDraft({ word: '', phonemes: '', error: null });
  }
  function saveEdit() {
    if (!editing) return;
    const phonemes = guidePhonemes(editing.phonemes);
    if (phonemes === guidePhonemes(map[editing.word])) { setEditing(null); return; }
    const err = guideError(editing.word, phonemes, map, editing.word);
    if (err) { setEditing({ ...editing, error: err }); return; }
    onChange({ ...map, [editing.word]: phonemes });
    setEditing(null);
  }
  function remove(word) {
    setEditing(null);
    const next = { ...map };
    delete next[word];
    onChange(next);
  }
  function start(word) {
    setDraft({ word, phonemes: '', error: null });
    setTimeout(() => phonemesRef.current?.focus(), 0);
  }

  const small = { fontFamily:MONO, fontSize:10, color:'var(--muted)', lineHeight:1.6 };
  const error = { fontFamily:MONO, fontSize:9, color:'var(--error)', marginTop:3 };
  const glyph = on => ({ background:'none', border:'none', padding:'0 3px', fontFamily:MONO, fontSize:11,
    color:on ? 'var(--text2)' : 'var(--muted)', opacity:on ? 1 : 0.4, cursor:on ? 'pointer' : 'default' });
  const missing = Array.isArray(unknown) ? unknown : [];
  const shown = showAll ? missing : missing.slice(0, 24);
  return (
    <div style={{ marginTop:4 }}>
      <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)' }}>Pronunciations</span>
      <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginLeft:8 }}>
        how a word sounds, in ARPAbet: misheard words are matched by sound, and a word with no pronunciation never is
      </span>
      <div role="list" aria-label="Pronunciations" style={{ marginTop:8 }}>
        {words.length === 0 && <div style={{ ...small, padding:'3px 0' }}>none: every word is said as the CMU dictionary says it</div>}
        {words.map(word => (
          <div key={word} role="listitem"
            style={{ display:'grid', gridTemplateColumns:'minmax(90px, 160px) 1fr auto', gap:8, alignItems:'center', padding:'2px 0' }}>
            <span style={{ fontFamily:SANS, fontSize:12, color:'var(--text)', overflowWrap:'anywhere' }}>{word}</span>
            {editing?.word === word ? (
              <div>
                <input type="text" autoFocus value={editing.phonemes} aria-label={`Phonemes of ${word}`}
                  aria-invalid={!!editing.error}
                  onChange={e => setEditing({ word, phonemes: e.target.value, error: null })}
                  onKeyDown={e => {
                    if (e.key === 'Enter') { e.preventDefault(); saveEdit(); }
                    else if (e.key === 'Escape') { e.preventDefault(); setEditing(null); }
                  }}
                  onBlur={saveEdit} style={{ width:'100%', boxSizing:'border-box', fontFamily:MONO }}/>
                {editing.error && <div style={error}>{editing.error} — not saved</div>}
              </div>
            ) : (
              <span {...pressable(() => setEditing({ word, phonemes: map[word], error: null }), { disabled })}
                title={disabled ? undefined : 'Edit'} aria-label={`Edit the phonemes of ${word}: ${map[word]}`}
                style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)', cursor:disabled ? 'default' : 'text' }}>{map[word]}</span>
            )}
            <button type="button" disabled={disabled} onMouseDown={e => e.preventDefault()} onClick={() => remove(word)}
              title="Remove" aria-label={`Remove the pronunciation of ${word}`} style={glyph(!disabled)}>×</button>
          </div>
        ))}
      </div>
      <div style={{ display:'grid', gridTemplateColumns:'minmax(90px, 160px) 1fr auto', gap:8, alignItems:'center', marginTop:8 }}>
        <input type="text" value={draft.word} disabled={disabled || full} placeholder="youtubetv" aria-label="Word"
          onChange={e => setDraft({ ...draft, word: e.target.value, error: null })}
          onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); phonemesRef.current?.focus(); } }}
          style={{ width:'100%', boxSizing:'border-box' }}/>
        <input type="text" ref={phonemesRef} value={draft.phonemes} disabled={disabled || full} placeholder="Y UW T UW B T IY V IY"
          aria-label="Its phonemes" aria-invalid={!!draft.error}
          onChange={e => setDraft({ ...draft, phonemes: e.target.value, error: null })}
          onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); add(); } }}
          style={{ width:'100%', boxSizing:'border-box', fontFamily:MONO }}/>
        <Pill small disabled={disabled || full || !draft.word.trim()} onClick={add}>+ Add</Pill>
      </div>
      {draft.error && <div style={error}>{draft.error}</div>}
      <div style={{ ...small, marginTop:4 }}>
        {words.length} of {TEXT_FIX_MAX_GUIDES} · a guide replaces the dictionary's pronunciation of its word · to borrow a
        similar word's phonemes, type it in Try a transcript below
      </div>
      <div style={{ marginTop:4 }}>
        <DisclosureToggle open={reference} onToggle={() => setReference(o => !o)}>ARPAbet symbols</DisclosureToggle>
        {reference && (
          <div style={{ ...small, marginTop:4, display:'grid', gridTemplateColumns:'repeat(auto-fill, minmax(110px, 1fr))', gap:'0 12px' }}>
            {ARPABET.map(([s, example]) => <span key={s}><b style={{ color:'var(--text2)' }}>{s}</b> {example}</span>)}
            <span style={{ gridColumn:'1 / -1' }}>Stress digits (EH1) are allowed and ignored; AX is the same as AH.</span>
          </div>
        )}
      </div>
      <div style={{ ...small, marginTop:10 }}>
        {unknown == null ? 'checking Home Assistant’s words…'
          : unknown.error ? `Home Assistant’s words could not be checked: ${unknown.error}`
          : missing.length === 0 ? 'Every word in Home Assistant’s trigger sentences, names and earlier requests has a pronunciation.'
          : <>
              {missing.length} word{missing.length === 1 ? '' : 's'} in Home Assistant’s trigger sentences, names and earlier
              requests {missing.length === 1 ? 'has' : 'have'} no pronunciation, so text with {missing.length === 1 ? 'it' : 'them'} is
              never matched by sound{disabled ? '' : '; click one to add its pronunciation'}:
            </>}
      </div>
      {missing.length > 0 && (
        <div style={{ display:'flex', flexWrap:'wrap', gap:6, marginTop:6 }}>
          {shown.map(({ word, count }) => (
            <span key={word} title={`in ${count} of Home Assistant’s trigger wordings, names and requests`}>
              <Pill small disabled={disabled || full} onClick={() => start(word)}>
                {word} <span style={{ color:'var(--muted)' }}>{count}</span>
              </Pill>
            </span>
          ))}
          {missing.length > shown.length && (
            <Pill small onClick={() => setShowAll(true)}>+{missing.length - shown.length} more</Pill>
          )}
        </div>
      )}
    </div>
  );
}

// Try a transcript (POST /api/text_fix/trial, §16.8): what transcript correction would send for
// text heard as a standalone command on this speaker (the fleet's settings in the fleet view),
// as On would, with the saved settings. Only Home Assistant's sentence matcher is asked; nothing
// runs. Results follow the typing, debounced; a response to an older input is dropped.
const TEXT_FIX_TRIAL_DEBOUNCE_MS = 400;
const LOCAL_ROUTES = Object.freeze({   // em_session.LocalRoute
  repeat: '“say that again”', alarm: 'an alarm request', alarm_query: 'a question about alarms',
  timer_cancel: 'a timer cancel',
});

function TextFixTester({ deviceId }) {
  const [text, setText] = useState('');
  const [wake, setWake] = useState(true);
  const [trial, setTrial] = useState(null);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  const seq = useRef(0);
  const timer = useRef(null);

  const run = useCallback(async (t, w, mine) => {
    if (!t.trim()) { setTrial(null); setError(null); setBusy(false); return; }
    setBusy(true);
    try {
      const r = await API.post('/api/text_fix/trial', { text: t, wake: w, ...(deviceId ? { device_id: deviceId } : {}) });
      if (mine !== seq.current) return;
      setTrial(r); setError(null);
    } catch (e) {
      if (mine !== seq.current) return;
      setTrial(null); setError(e?.error || e?.message || 'failed');
    } finally {
      if (mine === seq.current) setBusy(false);
    }
  }, [deviceId]);

  useEffect(() => {
    const mine = ++seq.current;
    timer.current = setTimeout(() => run(text, wake, mine), text.trim() ? TEXT_FIX_TRIAL_DEBOUNCE_MS : 0);
    return () => clearTimeout(timer.current);
  }, [text, wake, run]);

  const now = () => { clearTimeout(timer.current); run(text, wake, ++seq.current); };
  const muted = { color: 'var(--muted)' };
  const small = { fontFamily: MONO, fontSize: 10, color: 'var(--text2)', lineHeight: 1.6 };
  const quote = s => (s ? <span style={{ color: 'var(--text)' }}>“{s}”</span> : <span style={muted}>—</span>);
  const rec = trial?.record;
  const routeNote = trial?.route && (trial.route === 'timer_cancel'
    ? 'EchoMuse cancels a running timer itself; with none running, it goes to Home Assistant as below.'
    : `EchoMuse handles this itself as ${LOCAL_ROUTES[trial.route] || trial.route}: correction never sees it.`);
  const context = trial && [
    deviceId ? (trial.satellite ? "matched as this speaker's HA satellite" : 'no HA satellite matched: no room context')
      : "fleet settings, HA's preferred pipeline, no room context",
    `language ${trial.language || "HA's default"}`,
    trial.wake_phrase ? `as a wake turn (“${trial.wake_phrase}”)` : 'not a wake turn',
    rec?.context?.tables != null && `tables r${rec.context.tables}`,
  ].filter(Boolean).join(' · ');
  return (
    <div style={{ marginTop: 14 }}>
      <span style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)' }}>Try a transcript</span>
      <span style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginLeft: 8 }}>
        type what speech-to-text heard to see what On would send, with the saved settings; nothing runs
      </span>
      <div style={{ display: 'flex', gap: 10, alignItems: 'center', marginTop: 8 }}>
        <input type="text" value={text} placeholder="RMV alarm" aria-label="Transcript to try" maxLength={300}
          onChange={e => setText(e.target.value)}
          onKeyDown={e => {
            if (e.key === 'Enter') { e.preventDefault(); now(); }
            if (e.key === 'Escape') { e.preventDefault(); setText(''); }
          }}
          style={{ flex: 1, minWidth: 0 }}/>
        <label style={{ ...small, display: 'flex', gap: 5, alignItems: 'center', whiteSpace: 'nowrap' }}
          title="A wake turn may also drop leftover wake-word sounds at the start">
          <input type="checkbox" checked={wake} onChange={e => setWake(e.target.checked)}/>after the wake word
        </label>
      </div>
      <div aria-live="polite" style={{ ...small, marginTop: 8, minHeight: 16 }}>
        {busy && !trial && !error && <span style={muted}>asking Home Assistant's matcher…</span>}
        {error && <div style={{ color: 'var(--error)' }}>{error}</div>}
        {trial && (
          <div style={{ opacity: busy ? 0.6 : 1 }}>
            {routeNote && <div style={{ color: 'var(--warn)' }}>{routeNote}</div>}
            {rec && <TextFixEvidence key={trial.text} fix={rec} quote={quote} trial defaultOpen/>}
            <Sounded label="heard sounds like" words={trial.heard_words}/>
            {trial.proposed_words && <Sounded label="proposal sounds like" words={trial.proposed_words}/>}
            <div style={muted}>{context}</div>
            {trial.mode !== 'on' && (
              <div style={muted}>
                {deviceId ? 'This speaker' : 'The fleet'} is set to {trial.mode === 'shadow'
                  ? 'Shadow: it records what On would send and sends the transcript as heard.'
                  : 'Off: no correction runs.'}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

// How the trial's words were pronounced (em_phonetics), each with where it came from; a word
// with none is marked: text with it is never compared by sound.
const PRONUNCIATION_SOURCES = Object.freeze({ guide: 'your guide', dictionary: 'CMU dictionary',
  number: 'number', letters: 'said as letters' });
function Sounded({ label, words }) {
  if (!Array.isArray(words) || !words.length) return null;
  return (
    <div style={{ color: 'var(--muted)' }}>
      {label}{' '}
      {words.map((w, i) => (
        <span key={i} title={w.source ? PRONUNCIATION_SOURCES[w.source] || w.source : 'no pronunciation: add one under Pronunciations'}
          style={{ marginRight: 8, whiteSpace: 'nowrap' }}>
          <span style={{ color: w.source ? 'var(--text2)' : 'var(--error)' }}>{w.word}</span>{' '}
          {w.source
            ? <span>/{w.phonemes.join(' | ')}/{w.source !== 'dictionary' ? ` (${PRONUNCIATION_SOURCES[w.source] || w.source})` : ''}</span>
            : <span style={{ color: 'var(--error)' }}>no pronunciation</span>}
        </span>
      ))}
    </div>
  );
}

// The sound catalog (GET /api/sounds).
function useSounds() {
  return useCatalog('/api/sounds', SOUND_CATALOG_EMPTY);
}
const SOUND_CATALOG_EMPTY = Object.freeze({ sounds: [], default_id: 'default', unresolved: [] });

function SoundPicker({ label, configKey, value, onChange, disabled, catalog, reload, deviceId, deviceConnected, capabilities }) {
  const fileRef = useRef(null);
  const [previewing, setPreviewing] = useState(null);
  const [message, setMessage] = useState(null);
  const previewGap = deviceId && !deviceConnected ? 'device offline' : capabilityGap(capabilities, CAPABILITY.RENDER_PROGRESS);
  const missing = catalog.loaded && value && !catalog.sounds.some(s => s.id === value);
  // §18.4: an unresolvable effective sound stays stored but rings the
  // device's built-in fallback; em_sounds reports it per config scope.
  const unresolved = (catalog.unresolved || []).some(u =>
    u.key === configKey && u.scope === (deviceId || 'global'));

  async function upload(file) {
    if (!file) return;
    const id = (file.name.replace(/\.[^.]+$/, '') || 'sound')
      .replace(/[^A-Za-z0-9_.-]/g, '-').slice(0,64) || 'sound';
    const form = new FormData();
    form.append('sound', file); form.append('id', id);
    try {
      const r = await API.postForm('/api/sounds/upload', form);
      await reload();
      if (r.sound?.id) onChange(r.sound.id);
    } catch (e) { setMessage({ ok:false, text:e.error || 'Sound upload failed' }); }
  }

  async function remove(sound) {
    if (!confirm(`Delete sound "${sound.id}"?`)) return;
    try { await API.del(`/api/sounds/${encodeURIComponent(sound.id)}`); await reload(); }
    catch (e) { setMessage({ ok:false, text:e.error || 'Sound is still in use' }); }
  }

  async function preview(soundId) {
    if (!deviceId || previewGap) return;
    if (previewing === soundId) {
      await API.post(`/api/devices/${deviceId}/sounds/stop`, {}).catch(() => {});
      setPreviewing(null); return;
    }
    try {
      const r = await API.post(`/api/devices/${deviceId}/sounds/preview`, { sound_id:soundId });
      if (!r.ok) throw r;
      setPreviewing(soundId);
      setTimeout(() => setPreviewing(x => x === soundId ? null : x), 10000);
    } catch (e) { setMessage({ ok:false, text:e.error || 'Preview unavailable' }); }
  }

  return (
    <div style={{ marginBottom:16 }}>
      <div style={{ fontFamily:MONO, fontSize:10, color:'var(--text2)', marginBottom:7 }}>{label}</div>
      <div style={{ display:'grid', gridTemplateColumns:'repeat(auto-fill,minmax(140px,1fr))', gap:8 }}>
        <div {...pressable(() => onChange(''), { disabled, selected: !value })} style={{
          background:!value ? 'linear-gradient(160deg,var(--accent-tint),var(--accent-line))' : 'var(--surface)',
          border:`1px solid ${!value ? 'var(--accent)' : 'var(--border-soft)'}`, borderRadius:8, padding:'8px 10px',
          cursor:disabled ? 'default' : 'pointer',
        }}>
          <div style={{ fontFamily:SANS, fontSize:12, fontWeight:600, color:'var(--text2)' }}>Default</div>
          <div style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)', marginTop:3 }}>catalog “{catalog.default_id || 'default'}”, then built-in fallback</div>
        </div>
        {[...catalog.sounds, ...(missing ? [{ id:value, missing:true }] : [])].map(s => (
          <div key={s.id} {...pressable(() => onChange(s.id), { disabled, selected: value === s.id })} style={{
            background:value === s.id ? 'linear-gradient(160deg,var(--accent-tint),var(--accent-line))' : 'var(--surface)',
            border:`1px solid ${value === s.id ? 'var(--accent)' : s.missing ? 'var(--error)' : 'var(--border-soft)'}`,
            borderRadius:8, padding:'8px 10px', position:'relative', cursor:disabled ? 'default' : 'pointer',
          }}>
            <div style={{ fontFamily:SANS, fontSize:12, fontWeight:600, color:'var(--text2)' }}>{s.id}</div>
            <div style={{ fontFamily:MONO, fontSize:8, color:s.missing ? 'var(--error)' : 'var(--muted)', marginTop:3 }}>
              {s.missing ? 'missing — built-in fallback'
                : `${s.seconds != null ? `${s.seconds}s` : 'duration unavailable'}${s.shortened ? ' · shortened to 10s' : ''}`}
            </div>
            {!disabled && !s.missing && value !== s.id && (
              <button type="button" onClick={e => { e.stopPropagation(); remove(s); }} title="Delete sound"
                aria-label={`Delete sound ${s.id}`}
                style={{ position:'absolute', top:3, right:5, background:'none', border:'none', color:'var(--muted)', cursor:'pointer' }}>×</button>
            )}
            {deviceId && !s.missing && (
              <button type="button" onClick={e => { e.stopPropagation(); preview(s.id); }} disabled={!!previewGap}
                title={previewGap || 'Play this sound once on the device'}
                style={{ marginTop:7, background:'none', border:'none', padding:0, fontFamily:MONO, fontSize:8, color:previewGap ? 'var(--muted)' : 'var(--accent)', cursor:previewGap ? 'default' : 'pointer' }}>
                {previewing === s.id ? '■ stop preview' : '▶ preview'}
              </button>
            )}
          </div>
        ))}
        <div {...pressable(() => fileRef.current?.click(), { disabled })} style={{ border:'1px dashed var(--border-hard)', borderRadius:8, padding:'8px 10px', cursor:disabled ? 'default' : 'pointer' }}>
          <div style={{ fontFamily:SANS, fontSize:12, fontWeight:600, color:'var(--text2)' }}>+ Upload sound</div>
          <div style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)', marginTop:3 }}>mp3, wav, flac, ogg, m4a</div>
          <input ref={fileRef} type="file" accept=".mp3,.wav,.flac,.ogg,.m4a,audio/*" style={{ display:'none' }}
            onChange={e => { upload(e.target.files[0]); e.target.value=''; }}/>
        </div>
      </div>
      {unresolved && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>This sound cannot be resolved; rings use the built-in fallback tone.</div>}
      {deviceId && previewGap && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>Preview unavailable: {previewGap}</div>}
      {message && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:message.ok ? 'var(--ok)' : 'var(--error)' }}>{message.text}</div>}
    </div>
  );
}


function DeviceConfigForm({ config, onChange, disabled, sections, onScopeChange,
                            capabilities = null, deviceId = null, deviceConnected = false,
                            kioskTokenSet = null, kioskReason = null }) {
  // sections == null is the fleet view: nothing to inherit from, so every
  // control is live and no capability is guessed for a particular device.
  const scoped = Array.isArray(sections);
  const isLocal = id => !scoped || sections.includes(id);
  const set = (k, v) => {
    if (disabled) return;
    if (scoped && !isLocal(KEY_SECTION[k])) return;
    onChange(k, v);
  };
  const scopeEl = id => scoped
    ? <ScopeToggle local={isLocal(id)} disabled={disabled}
        onChange={local => onScopeChange && onScopeChange(id, local)}/>
    : null;
  const secStyle = id => (disabled || !isLocal(id))
    ? { opacity:0.45, pointerEvents:'none' }
    : {};
  const wakeGap = capabilityGap(capabilities, CAPABILITY.DEVICE_WAKE);
  const renderGap = capabilityGap(capabilities, CAPABILITY.RENDER_PROGRESS);
  const focusGap = capabilityGap(capabilities, CAPABILITY.FOCUS_LEASES);
  const holdGap = capabilityGap(capabilities, CAPABILITY.BUTTON_HOLD);
  const rulesGap = capabilityGap(capabilities, CAPABILITY.OPEN_RULES);
  // Without led_anim the controller can only send static LEDs: the thinking
  // spinner and the speaking meter both fall back to the solid listening
  // colour (em_device._send_led), so their settings would do nothing.
  const animGap = capabilityGap(capabilities, CAPABILITY.LED_ANIM);
  // No `leds` at all (a Fire OS Show has no ring): every ring control is moot.
  const ringGap = capabilityGap(capabilities, CAPABILITY.LEDS);
  const ringStyle = ringGap ? { opacity: 0.45, pointerEvents: 'none' } : {};
  const bleGap = capabilityGap(capabilities, CAPABILITY.BLE_PROXY);
  const screenGap = capabilityGap(capabilities, CAPABILITY.SCREEN_WAKE);
  const kioskGap = kioskApiGap(capabilities);
  // A LineageOS Show's Kiosk Satellite plugin (docs/echo-show-companion-plan.md
  // §11): the remote API port it is installed through, and the turn view.
  const pluginGap = capabilityGap(capabilities, CAPABILITY.KIOSK_PLUGIN);
  const turnViewGap = capabilityGap(capabilities, CAPABILITY.TURN_VIEW);
  const [catalog, reloadSounds] = useSounds();

  const bands = config.eqBands ?? [0,0,0,0,0,0,0,0];
  const RING_SCENES = [
    { value: 'standard',   label: 'Standard',   swatches: ['#00b400'] },
    { value: 'airy',       label: 'Airy',       swatches: ['#5096c8', '#96cdff'] },
    { value: 'malevolent', label: 'Malevolent', swatches: ['#6e002d', '#d22d00'] },
    { value: 'pride',      label: 'Pride',      swatches: ['#bf0000', '#bf7700', '#a9bf00', '#00bf2c', '#0055bf', '#8b00bf'] },
    { value: 'custom',     label: 'Custom',     swatches: null },
  ];
  const EQ_PRESETS = [['Flat',[0,0,0,0,0,0,0,0]], ['Clarity',[0,0,0,0,0,7,4,2]], ['Warmth',[0,3,2,0,-2,0,0,0]]];
  const activeEqPreset = (EQ_PRESETS.find(([, vals]) => JSON.stringify(vals) === JSON.stringify(bands)) || [null])[0];

  const [advRing, setAdvRing] = useState(false);

  const inputStyle = disabled ? { opacity: 0.45, pointerEvents: 'none' } : {};
  // em_db.DEFAULT_DEVICE_CONFIG: shadow until someone chooses.
  const fixMode = Object.values(TEXT_FIX_MODE).includes(config.transcriptFix) ? config.transcriptFix : TEXT_FIX_MODE.SHADOW;


  // Ordered by how often each section gets touched: playback and wake word
  // are everyday knobs, the speech copy is set-and-forget, and the button
  // gestures come after the LED ring.
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>

      {/* 01 PLAYBACK */}
      <Stage n="01" title="Playback"
        chips={<><ScopeChip tone="controller">Controller</ScopeChip><ScopeChip tone="device">Speaker</ScopeChip></>}
        desc="Audio the controller streams to the speaker — Home Assistant responses and music — passes through this EQ. Alarms and timers ring from the Echo itself and are not equalised. Presets set the faders; drag any fader for a custom curve."
        scope={scopeEl('playback')} dim={secStyle('playback')}>
        <div className="em-grid2" style={{ display: 'grid', gridTemplateColumns: '1.4fr 1fr', gap: 28, alignItems: 'start' }}>
          <div>
            <EqCurve bands={bands}/>
            <EqSliders bands={bands} onChange={nb => set('eqBands', nb)} disabled={disabled}/>
            <div style={{ display: 'flex', gap: 6, marginTop: 12, alignItems: 'center', ...inputStyle }}>
              {EQ_PRESETS.map(([label, vals]) => (
                <Pill key={label} small accent={activeEqPreset === label} onClick={() => set('eqBands', vals)}>{label}</Pill>
              ))}
              {!activeEqPreset && (
                <span style={{ fontFamily: MONO, fontSize: 9, color: 'var(--accent)', textTransform: 'uppercase', letterSpacing: '0.1em' }}>· Custom</span>
              )}
            </div>
          </div>
          <div>
            <div style={inputStyle}>
              <Toggle label="Speech boost" sub="presence boost for voice" value={config.eqLoudness ?? false} onChange={v => set('eqLoudness', v)}/>
            </div>
            <div style={inputStyle}>
              <Slider label="Duck depth" disabled={!!focusGap}
                sub={focusGap || "how far music drops under a voice response — content keeps playing under the dialog focus lease"}
                value={config.duckDb ?? -18} min={-40} max={0} step={1} unit="dB"
                onChange={v => set('duckDb', v)}/>
            </div>
            {/* The startup-volume slider used to live here and was removed
                (2026-07-25): volume is persisted device STATE, not a setting.
                The controller writes it from every volume_state report and
                the device only re-applies it on the first config push per
                run — so moving this slider did nothing until the device
                restarted, and any real volume change overwrote it. Current
                volume is now shown read-only on the Status tab. */}
            <div style={{ marginTop: 8, fontFamily: MONO, fontSize: 10, color: 'var(--muted)', lineHeight: 1.6 }}>
              Volume is remembered per device and restored after a reboot.
              Change it from Home Assistant or the device buttons; the current
              level is shown on the Status tab.
            </div>
          </div>
        </div>
      </Stage>

      {/* 02 WAKE WORD */}
      <Stage n="02" title="Wake word"
        chips={<><ScopeChip tone="device">Device</ScopeChip><ScopeChip tone="controller">Registry</ScopeChip></>}
        desc="The Echo scores BCResNet every 160 ms. Thresholds and verification words belong to each validated registry model; choosing a model names its graph SHA-256 to every device."
        scope={scopeEl('wakeword')} dim={secStyle('wakeword')}>
        <WakeModelPicker value={config.wakeModel || ''} onChange={v => set('wakeModel', v)} disabled={disabled || !!wakeGap}/>
        {wakeGap && <div style={{ marginTop:8, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>Wake controls unavailable: {wakeGap}</div>}
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', marginTop:16, ...inputStyle }}>
          <Toggle label="Wake chime"
            sub={renderGap || 'plays the moment the Echo hears the wake word (on supporting firmware); while it is playing audio, once the wake is confirmed; and when a follow-up question finishes, to say it is listening for your answer. An error tone says it stopped listening when nothing followed the wake word (on supporting firmware)'}
            value={!renderGap && (config.wakeSound ?? false)}
            disabled={!!renderGap}
            onChange={v => set('wakeSound', v)}/>
          <Toggle label="Save wake clips"
            sub="keeps bounded candidate audio for tuning, and records each near miss; play or download them from Activity"
            value={config.saveWakeClips ?? false}
            onChange={v => set('saveWakeClips', v)}/>
          <Slider label="Arbitration window"
            sub="the first Echo to claim a wake suppresses the others; 0 disables"
            value={config.wakeArbitrationMs ?? 700} min={0} max={2000} step={50} unit="ms"
            onChange={v => set('wakeArbitrationMs', v)}/>
          {/* Rounded: the controller takes at most two decimals, and a range
              input's stepped value is a float. */}
          <Slider label="Near-miss floor"
            sub="wake scores at or above this that do not wake the Echo are listed in Activity; raise it in a noisy room. Changing it briefly reconnects the device"
            value={config.nearMissThreshold ?? 0.17} min={0.05} max={0.60} step={0.01}
            formatValue={v => v.toFixed(2)}
            onChange={v => set('nearMissThreshold', Math.round(v * 100) / 100)}/>
        </div>
        <div style={inputStyle}>
          <WakeRulesEditor config={config} set={set} disabled={disabled} gap={rulesGap}/>
        </div>
      </Stage>

      {/* 03 SPEECH (section id "microphones") */}
      <Stage n="03" title="Speech"
        chips={<ScopeChip tone="controller">Speech copy</ScopeChip>}
        desc="The copy of each utterance the controller sends to Home Assistant speech-to-text, whether it is kept, and who transcribes a request at its pauses. The Echo's own audio front end owns capture, beamforming and echo cancellation; nothing here changes what it hears."
        scope={scopeEl('microphones')} dim={secStyle('microphones')}>
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', ...inputStyle }}>
          <Toggle label="Noise suppression"
            sub="apply DTLN only to the copy sent to speech-to-text"
            value={config.nsAsr ?? false} onChange={v => set('nsAsr', v)}/>
          <Toggle label="Save utterances"
            sub="keep bounded recordings on the controller: each request as sent to speech-to-text and, from Echos that report native AFE, the whole turn through the spoken answer; play or download them from Activity"
            value={config.saveUtterances ?? false} onChange={v => set('saveUtterances', v)}/>
          <Toggle label="Extended utterances"
            sub={(config.extendedUtterances ?? false)
              ? 'allow up to 30 seconds for dictation and long requests'
              : 'standard 15-second limit; turn on for dictation and long requests'}
            value={config.extendedUtterances ?? false} onChange={v => set('extendedUtterances', v)}/>
        </div>
        <div style={inputStyle}>
          <PauseAsrControls value={config.pauseAsr} onChange={v => set('pauseAsr', v)}/>
        </div>
      </Stage>

      {/* 04 RING */}
      <Stage n="04" title="Ring"
        chips={<ScopeChip tone="controller">Controller</ScopeChip>}
        desc="Colours for the LED ring during conversations — the solid listening ring and the thinking spinner. The red mute ring and cyan volume arc never change; red always means the mics are off."
        scope={scopeEl('ring')} dim={secStyle('ring')}>
        {ringGap && <div style={{ marginBottom:8, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>Ring controls unavailable: {ringGap}</div>}
        <div className="em-grid2" style={{ display: 'grid', gridTemplateColumns: '1.4fr 1fr', gap: 24, alignItems: 'start', ...ringStyle }}>
          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 6, ...inputStyle }}>
            {RING_SCENES.map(sc => (
              <div key={sc.value} {...pressable(() => set('ledScene', sc.value), { disabled: disabled || !!ringGap, selected: (config.ledScene ?? 'standard') === sc.value })} style={{
                background: (config.ledScene ?? 'standard') === sc.value
                  ? 'linear-gradient(160deg,var(--accent-tint),var(--accent-line))'
                  : 'linear-gradient(160deg,var(--raised),var(--surface))',
                border: `1px solid ${(config.ledScene ?? 'standard') === sc.value ? 'var(--accent)' : 'var(--border-soft)'}`,
                borderRadius: 8, padding: '8px 10px',
                cursor: disabled ? 'default' : 'pointer',
                transition: 'border-color 0.15s, background 0.15s',
              }}>
                <div style={{ fontFamily: SANS, fontSize: 12, fontWeight: 600, color: 'var(--lcd-line)' }}>{sc.label}</div>
                <div style={{ display: 'flex', gap: 3, marginTop: 4 }}>
                  {(sc.value === 'custom'
                    ? [config.ledListenColor ?? '#00b400', config.ledThinkColor ?? '#00c800']
                    : sc.swatches
                  ).map((c, i) => (
                    <span key={i} style={{ width: 10, height: 10, borderRadius: '50%', background: c, border: '1px solid rgba(0,0,0,0.15)' }}/>
                  ))}
                </div>
              </div>
            ))}
          </div>
          {(config.ledScene ?? 'standard') === 'custom' && (
            <div style={inputStyle}>
              <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)', marginBottom: 8 }}>Custom colours</div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 10 }}>
                <input type="color" value={config.ledListenColor ?? '#00b400'} disabled={disabled || !!ringGap}
                  aria-label="Listening colour"
                  onChange={e => set('ledListenColor', e.target.value)}
                  style={{ width: 36, height: 28, padding: 0, border: '1px solid var(--border)', borderRadius: 6, background: 'none', cursor: 'pointer' }}/>
                <div>
                  <div style={{ fontFamily: SANS, fontSize: 12, fontWeight: 600 }}>Listening</div>
                  <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)' }}>solid ring while recording</div>
                </div>
              </div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                <input type="color" value={config.ledThinkColor ?? '#00c800'} disabled={disabled || !!animGap}
                  aria-label="Thinking colour"
                  onChange={e => set('ledThinkColor', e.target.value)}
                  style={{ width: 36, height: 28, padding: 0, border: '1px solid var(--border)', borderRadius: 6, background: 'none', cursor: animGap ? 'default' : 'pointer', opacity: animGap ? 0.45 : 1 }}/>
                <div>
                  <div style={{ fontFamily: SANS, fontSize: 12, fontWeight: 600 }}>Thinking</div>
                  <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)' }}>{animGap || 'spinner while processing'}</div>
                </div>
              </div>
            </div>
          )}
        </div>
        <StageAdvanced open={advRing} onToggle={() => setAdvRing(o => !o)} disabledStyle={inputStyle}>
          <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--text2)', lineHeight: 1.6, marginBottom: 12 }}>
            While a response plays, the ring throbs with the live speaker level. These shape how
            hard it throbs — the device renders it locally, so changes apply on the next response
            with no restart. Defaults are tuned for speech; raise Decay and Gamma for a punchier
            ring, lower them for a calmer one.
          </div>
          {animGap && <div style={{ marginBottom:12, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>Meter controls unavailable: {animGap}</div>}
          <div className="em-grid2" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '4px 24px' }}>
            <Slider label="Decay" sub="how fast it falls — higher tracks individual syllables"
              value={config.meterDecay ?? 0.30} min={0.02} max={1} step={0.02} disabled={!!animGap}
              onChange={v => set('meterDecay', v)}/>
            <Slider label="Attack" sub="how fast it rises on a peak"
              value={config.meterAttack ?? 0.6} min={0.05} max={1} step={0.05} disabled={!!animGap}
              onChange={v => set('meterAttack', v)}/>
            <Slider label="Gamma" sub="contrast — higher makes the swing more visible"
              value={config.meterGamma ?? 2.2} min={1} max={3.5} step={0.1} disabled={!!animGap}
              onChange={v => set('meterGamma', v)}/>
            <Slider label="Floor" sub="brightness during silence; 0 = fully dark between words"
              value={config.meterFloor ?? 0.06} min={0} max={0.6} step={0.02} disabled={!!animGap}
              onChange={v => set('meterFloor', v)}/>
            <Slider label="Reference" sub="speaker level mapped to full brightness — lower = more sensitive"
              value={config.meterRef ?? 0.22} min={0.02} max={1} step={0.02} disabled={!!animGap}
              onChange={v => set('meterRef', v)}/>
            <Slider label="Curve" sub="below 1 lifts quiet consonants into view"
              value={config.meterCurve ?? 0.7} min={0.3} max={2} step={0.05} disabled={!!animGap}
              onChange={v => set('meterCurve', v)}/>
          </div>
        </StageAdvanced>
      </Stage>

      {/* 05 BUTTON (section id "advanced") */}
      <Stage n="05" title="Button"
        chips={<ScopeChip tone="device">Action button</ScopeChip>}
        desc="What a tap on the action button does: start a voice turn, or fire Home Assistant's action-button event."
        scope={scopeEl('advanced')} dim={secStyle('advanced')}>
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', ...inputStyle }}>
          <Toggle label="Tap sends an event"
            sub={holdGap || "tap fires the Home Assistant action-button event instead of starting a turn; bind destructive actions to a hold"}
            value={!holdGap && (config.buttonSingleTapEvent ?? false)}
            disabled={!!holdGap}
            onChange={v => set('buttonSingleTapEvent', v)}/>
          <Slider label="Multi-tap window"
            sub="0 = off; coalesces quick taps at the cost of delaying each tap"
            value={config.buttonMultiTapMs ?? 0} min={0} max={600} step={50} unit="ms"
            disabled={!!holdGap || !(config.buttonSingleTapEvent ?? false)}
            onChange={v => set('buttonMultiTapMs', v)}/>
        </div>
      </Stage>

      {/* 06 BLUETOOTH */}
      <Stage n="06" title="Bluetooth"
        chips={<><ScopeChip tone="device">Device</ScopeChip><ScopeChip tone="controller">Controller</ScopeChip></>}
        desc="Turns the device into a Home Assistant Bluetooth proxy: it passively listens for BLE advertisements (presence beacons, temperature sensors) and forwards them to HA as a separate ESPHome device — independent of the voice assistant. Enabling permanently switches the Dot's Bluetooth chip away from Android's stack (Bluetooth speaker pairing, never used by EchoMuse, stops being possible)."
        scope={scopeEl('bluetooth')} dim={secStyle('bluetooth')}>
        <div className="em-grid2" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '0 24px', ...inputStyle }}>
          <Toggle label="Bluetooth proxy" sub={bleGap || 'passive BLE scan → HA (Bermuda, BLE sensors)'}
            value={!bleGap && (config.bleProxyEnabled ?? false)} disabled={!!bleGap}
            onChange={v => set('bleProxyEnabled', v)}/>
        </div>
      </Stage>

      {/* 07 TIMERS AND ALARMS */}
      <Stage n="07" title="Timers & alarms"
        chips={<><ScopeChip tone="controller">HA clocks</ScopeChip><ScopeChip tone="device">Rings</ScopeChip></>}
        desc="Home Assistant owns timer countdowns and alarm calendars. A ring loops the whole selected sound; alarms without their own sound use the default alarm sound. Preview plays a sound once on this Echo."
        scope={scopeEl('timers')} dim={secStyle('timers')}>
        <SoundPicker label="Timer sound" configKey="timerSound" value={config.timerSound || ''} onChange={v => set('timerSound', v)}
          disabled={disabled} catalog={catalog} reload={reloadSounds} deviceId={deviceId}
          deviceConnected={deviceConnected} capabilities={capabilities}/>
        <SoundPicker label="Default alarm sound" configKey="alarmSound" value={config.alarmSound || ''} onChange={v => set('alarmSound', v)}
          disabled={disabled} catalog={catalog} reload={reloadSounds} deviceId={deviceId}
          deviceConnected={deviceConnected} capabilities={capabilities}/>
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', ...inputStyle }}>
          <Slider label="Gap between repeats"
            sub="quiet between complete timer-sound loops"
            value={config.timerRingGapSeconds ?? 2.0} min={0} max={10} step={0.1} unit="s"
            onChange={v => set('timerRingGapSeconds', v)}/>
          <Slider label="Timer ring limit"
            sub="how long an unanswered timer may ring"
            value={config.timerRingSeconds ?? 900} min={30} max={1800} step={30}
            formatValue={v => v >= 60 ? `${Math.round(v/60)} min` : `${v}s`}
            onChange={v => set('timerRingSeconds', v)}/>
        </div>
      </Stage>

      {/* 08 TRANSCRIPT CORRECTION (section id "transcript") */}
      <Stage n="08" title="Transcript correction"
        chips={<><ScopeChip tone="controller">Controller</ScopeChip><ScopeChip tone="controller">HA matcher</ScopeChip></>}
        desc="After Home Assistant's speech-to-text and EchoMuse's own commands, a misheard request can be repaired before Home Assistant runs it. Candidates come from HA's trigger sentences, its entity, area and floor names, requests it answered before, and the terms and phrases below; HA's own sentence matcher must fully accept one without running it. Text HA already understands is never changed, and follow-ups are never corrected. Each turn's correction is shown in Activity."
        scope={scopeEl('transcript')} dim={secStyle('transcript')}
        after={<TextFixTester deviceId={deviceId}/>}>
        <div style={inputStyle}>
          <div role="group" aria-label="Transcript correction mode" style={{ display:'flex', alignItems:'center', gap:6, flexWrap:'wrap' }}>
            <span style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)', marginRight:4 }}>Mode</span>
            {TEXT_FIX_MODE_OPTIONS.map(([m, label]) => (
              <Pill key={m} small accent={fixMode === m} disabled={disabled} onClick={() => set('transcriptFix', m)}>{label}</Pill>
            ))}
          </div>
          <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginTop:6, lineHeight:1.6 }}>{TEXT_FIX_MODE_HELP[fixMode]}</div>
        </div>
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', marginTop:16, alignItems:'start', ...inputStyle }}>
          <TextFixListEditor label="Preferred terms" noun="term" placeholder="Plex, YouTube TV, Cherub Rock"
            sub="titles, apps and names to prefer when a word was misheard; a term is never a command and never changes a verb on its own"
            items={config.transcriptFixTerms} disabled={disabled}
            onChange={v => set('transcriptFixTerms', v)}/>
          <TextFixListEditor label="Preferred phrases" noun="phrase" placeholder="watch Plex"
            sub="complete commands you say often, matched against the whole request"
            items={config.transcriptFixPhrases} disabled={disabled}
            onChange={v => set('transcriptFixPhrases', v)}/>
        </div>
        <div style={{ marginTop:4, ...inputStyle }}>
          <TextFixPronunciationEditor guides={config.transcriptFixPronunciations} disabled={disabled} deviceId={deviceId}
            onChange={v => set('transcriptFixPronunciations', v)}/>
        </div>
      </Stage>

      {/* 09 SCREEN */}
      <Stage n="09" title="Screen"
        chips={<ScopeChip tone="device">Kiosk</ScopeChip>}
        desc="An Echo Show's screen belongs to its kiosk app, which keeps its own motion wake and screen timeout. With this on, the wake word also lights the screen, through Fully Kiosk Browser's local REST API: enable Remote Administration in Fully's settings and enter its password here. The URL is the kiosk API's root; the default reaches Fully on the Echo Show itself. On LineageOS, firmware with the EchoMuse app lights the screen through the app and needs neither. With Kiosk Satellite as the kiosk, the firmware installs an EchoMuse plugin that shows each request and answer full screen, through Kiosk Satellite's remote API: turn on Remote Administration with a password there, and log in below."
        scope={scopeEl('screen')} dim={secStyle('screen')}
        after={deviceId && kioskTokenSet != null && (
          <KioskTokenRow deviceId={deviceId} tokenSet={kioskTokenSet} reason={kioskReason}
            gap={pluginGap} disabled={disabled}/>
        )}>
        {screenGap && <div style={{ marginBottom:8, fontFamily:MONO, fontSize:9, color:'var(--warn)' }}>Screen controls unavailable: {screenGap}</div>}
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', ...inputStyle }}>
          <Toggle label="Light the screen on wake"
            sub={screenGap || 'turns the kiosk screen on the moment the Echo hears the wake word'}
            value={!screenGap && (config.screenWake ?? false)} disabled={!!screenGap}
            onChange={v => set('screenWake', v)}/>
        </div>
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1.6fr 1fr', gap:12, marginTop:8, ...inputStyle }}>
          <label style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)' }}>kiosk API URL
            <input type="text" value={config.screenWakeUrl ?? ''} disabled={!!kioskGap} placeholder="http://127.0.0.1:2323"
              onChange={e => set('screenWakeUrl', e.target.value.trim())}
              style={{ display:'block', width:'100%', marginTop:4, boxSizing:'border-box' }}/>
          </label>
          <label style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)' }}>Remote Administration password
            <input type="password" value={config.screenWakePassword ?? ''} disabled={!!kioskGap}
              autoComplete="new-password" onChange={e => set('screenWakePassword', e.target.value)}
              style={{ display:'block', width:'100%', marginTop:4, boxSizing:'border-box' }}/>
          </label>
        </div>
        {kioskGap && !screenGap && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>Kiosk API unused: {kioskGap}</div>}
        <div className="em-grid2" style={{ display:'grid', gridTemplateColumns:'1fr 1fr', gap:'0 24px', marginTop:12, ...inputStyle }}>
          <Slider label="Turn view stays up"
            sub={turnViewGap || 'after a turn ends, before the full-screen view hides (0: at once)'}
            value={config.turnViewLinger ?? 6} min={0} max={60} step={1} unit="s"
            onChange={v => { if (!turnViewGap) set('turnViewLinger', v); }}/>
          <label style={{ fontFamily:MONO, fontSize:8, color:'var(--muted)' }}>Kiosk Satellite API port
            <input type="number" min={1} max={65535} value={config.kioskPort ?? 2324} disabled={!!pluginGap}
              onChange={e => { const v = parseInt(e.target.value, 10); if (v >= 1 && v <= 65535) set('kioskPort', v); }}
              style={{ display:'block', width:'100%', marginTop:4, boxSizing:'border-box' }}/>
          </label>
        </div>
        {pluginGap && <div style={{ marginTop:6, fontFamily:MONO, fontSize:9, color:'var(--muted)' }}>Kiosk Satellite plugin unavailable: {pluginGap}</div>}
      </Stage>
    </div>
  );
}


// ─── Deploy-all modal ─────────────────────────────────────────────────────────
// Fleet-wide OTA from the main dashboard. Uses POST /api/firmware/deploy
// (installs the firmware bundled with this controller on every connected,
// approved device not already reporting it; approved devices that are offline
// and behind are offered the install-on-reconnect queue). Progress is read live from the
// `devices` prop — the parent's WebSocket keeps it fresh, so each row updates
// as devices drop for reboot and reconnect on the new version.

function DeployAllModal({ firmware, devices, deployState, onStarted, onDismiss, onClose }) {
  const [running, setRunning] = useState(false);
  const [error, setError]     = useState('');
  // Guard against setState after the modal is closed mid-request — the deploy
  // POST returns fast (server backgrounds the work), but closing during that
  // window otherwise logs a React unmounted-update error.
  const mounted = useRef(true);
  useEffect(() => () => { mounted.current = false; }, []);

  // The view is driven by the persisted deployState (survives close/reopen),
  // not local state. Present → show progress; absent → show the confirm screen.
  const view = deployState;
  const target = view?.version || firmware?.version;
  const byId = Object.fromEntries(devices.map(d => [d.device_id, d]));
  const label = d => d?.label || d?.device_id || '?';
  // Mirrors the server's selection: it skips only devices already reporting
  // the bundled version.
  const eligible = devices.filter(d =>
    d.approved && d.connected && d.firmware_ver !== firmware?.version);
  // Behind but offline: the deploy cannot reach them now, so each gets the
  // same install-on-reconnect queue as its Updates tab.
  const offlineBehind = devices.filter(d => d.approved && !d.connected && firmwareBehind(d, firmware));

  const SKIP_REASONS = {
    offline:            'offline',
    not_approved:       'not approved',
    already_current:    'already up to date',
    update_in_progress: 'update already running',
  };

  // An offline row with its queue control, on the confirm and the progress
  // screen alike.
  const offlineRow = (d, note) => (
    <div key={d.device_id} style={{ fontFamily: MONO, fontSize: 11, padding: '6px 0', borderBottom: '1px solid var(--hairline)' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 6 }}>
        <span style={{ color: 'var(--text2)' }}>{label(d)}</span>
        <span style={{ color: 'var(--muted)' }}>{note} · {d.firmware_ver || '?'}</span>
      </div>
      <QueuedInstall small device={d} firmware={firmware}/>
    </div>
  );
  const QUEUE_NOTE = 'Offline devices cannot install now. Queue one and it installs whatever firmware '
    + 'this controller bundles when it next reconnects (not a pinned version); the queue survives a '
    + 'controller restart and can be cancelled here or on the device\'s Updates tab.';

  function statusFor(id) {
    const d = byId[id];
    if (!d)                              return { text: 'unknown',      color: 'var(--muted)' };
    if (d.connected && d.firmware_ver === target)
                                         return { text: '✓ updated',    color: 'var(--ok)' };
    // A recorded failure is terminal — without this the row (and the header
    // progress pill) sat at "updating…" forever after an aborted update.
    if (d.update_error)                  return { text: `✗ ${d.update_error}`, color: 'var(--error)' };
    if (!d.connected)                    return { text: 'rebooting…',   color: 'var(--warn)' };
    return { text: 'updating…', color: 'var(--accent)' };
  }

  const started = view?.started || [];
  // Failed counts as done — the deploy reached a terminal state for that
  // device, it just wasn't success.
  const terminal = id => {
    const d = byId[id];
    return d && ((d.connected && d.firmware_ver === target) || d.update_error);
  };
  const failedCount = started.filter(id => byId[id]?.update_error &&
    !(byId[id].connected && byId[id].firmware_ver === target)).length;
  const allDone = started.length > 0 && started.every(terminal);

  async function deploy() {
    setRunning(true); setError('');
    try {
      const res = await API.post('/api/firmware/deploy');
      onStarted(res); // lift to App so it persists across close/reopen
    } catch (e) {
      if (mounted.current) setError(e.error || 'Deploy failed');
    }
    if (mounted.current) setRunning(false);
  }

  return (
    <div onClick={onClose} style={{ position: 'fixed', inset: 0, background: 'rgba(30,28,24,0.45)', zIndex: 60, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
      <div onClick={e => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Deploy to fleet" style={{ background: 'linear-gradient(170deg,var(--raised),var(--surface))', border: '1px solid var(--border)', borderRadius: 14, padding: '28px 32px', width: 440, maxWidth: '92vw', boxShadow: '0 24px 80px rgba(0,0,0,0.3)' }}>
        <div style={{ fontFamily: SANS, fontSize: 16, fontWeight: 600, color: 'var(--text)', marginBottom: 4 }}>
          Deploy to fleet
        </div>
        <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginBottom: 18 }}>
          Target: bundled EchoMuse firmware {firmware?.version || '—'} · devices update over WiFi and auto-roll-back on failure · the device OS is not changed
        </div>

        {!view ? (
          <>
            <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--text2)', marginBottom: 16, lineHeight: 1.8 }}>
              {eligible.length === 0
                ? 'Every connected device is already on the bundled EchoMuse firmware.'
                : <>Will update <b>{eligible.length}</b> device{eligible.length === 1 ? '' : 's'}:{' '}
                    {eligible.map(d => `${label(d)} (${d.firmware_ver || '?'})`).join(', ')}</>}
            </div>
            {offlineBehind.length > 0 && (
              <div style={{ marginBottom: 16 }}>
                <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', lineHeight: 1.6, marginBottom: 6 }}>{QUEUE_NOTE}</div>
                {offlineBehind.map(d => offlineRow(d, 'offline'))}
              </div>
            )}
            {error && <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--error)', marginBottom: 12 }}>{error}</div>}
            <div style={{ display: 'flex', gap: 10 }}>
              <Pill accent disabled={running || eligible.length === 0} onClick={deploy}>
                {running ? 'Starting…' : `Deploy EchoMuse firmware ${firmware?.version || ''}`.trim()}
              </Pill>
              <Pill onClick={onClose}>Cancel</Pill>
            </div>
          </>
        ) : (
          <>
            {(view.started || []).map(id => {
              const s = statusFor(id);
              return (
                <div key={id} style={{ display: 'flex', justifyContent: 'space-between', fontFamily: MONO, fontSize: 11, padding: '5px 0', borderBottom: '1px solid var(--hairline)' }}>
                  <span style={{ color: 'var(--text2)' }}>{label(byId[id])}</span>
                  <span style={{ color: s.color }}>{s.text} {byId[id]?.firmware_ver ? `· ${byId[id].firmware_ver}` : ''}</span>
                </div>
              );
            })}
            {(view.skipped || []).map(s => (s.reason === 'offline' && byId[s.device_id]
              ? offlineRow(byId[s.device_id], 'skipped — offline')
              : (
              <div key={s.device_id} style={{ display: 'flex', justifyContent: 'space-between', fontFamily: MONO, fontSize: 11, padding: '5px 0', borderBottom: '1px solid var(--hairline)' }}>
                <span style={{ color: 'var(--muted)' }}>{label(byId[s.device_id])}</span>
                <span style={{ color: 'var(--muted)' }}>skipped — {SKIP_REASONS[s.reason] || s.reason}</span>
              </div>
            )))}
            {(view.skipped || []).some(s => s.reason === 'offline') && (
              <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', lineHeight: 1.6, marginTop: 10 }}>{QUEUE_NOTE}</div>
            )}
            {(view.started || []).length === 0 && (view.skipped || []).length === 0 && (
              <div style={{ fontFamily: MONO, fontSize: 11, color: 'var(--muted)' }}>Nothing to do.</div>
            )}
            <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', marginTop: 14 }}>
              {allDone
                ? (failedCount > 0
                    ? `Finished — ${failedCount} device${failedCount === 1 ? '' : 's'} failed (see device logs).`
                    : 'All devices updated.')
                : 'Updates run in the background — you can close this and reopen it from the header to check progress.'}
            </div>
            <div style={{ marginTop: 14, display: 'flex', gap: 10 }}>
              {allDone
                ? <Pill accent onClick={() => { onDismiss(); onClose(); }}>Done</Pill>
                : <Pill onClick={onClose}>Close (keeps running)</Pill>}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

// ─── SettingsPanel ─────────────────────────────────────────────────────────────
// Gear icon → modal with tabs: fleet config, account, and (admins) support.

const SETTINGS_TAB = Object.freeze({ FLEET: 'fleet', ACCOUNT: 'account', SUPPORT: 'support' });
const SETTINGS_TAB_LABELS = Object.freeze({
  [SETTINGS_TAB.FLEET]: 'Config', [SETTINGS_TAB.ACCOUNT]: 'Account', [SETTINGS_TAB.SUPPORT]: 'Support',
});

function SettingsPanel({ globalConfig, onGlobalConfigChange, onClose, username, isAdmin }) {
  const [tab, setTab]             = useState(SETTINGS_TAB.FLEET);
  const [config, setConfig]       = useState({ ...globalConfig });
  const [dirty, setDirty]         = useState(false);
  const [saving, setSaving]       = useState(false);

  const [curPw, setCurPw]         = useState('');
  const [newPw, setNewPw]         = useState('');
  const [confirmPw, setConfirmPw] = useState('');
  const [pwSaving, setPwSaving]   = useState(false);
  const [pwMsg, setPwMsg]         = useState(null); // {ok, text}

  const [bundling, setBundling]   = useState(false);
  const [bundle, setBundle]       = useState(null);  // {url, name, bytes}
  const [bundleErr, setBundleErr] = useState(null);

  // Object URLs pin their blob in memory until revoked; the panel closing is
  // the last moment we can still reach this one.
  useEffect(() => () => { if (bundle) URL.revokeObjectURL(bundle.url); }, [bundle]);

  async function collectBundle() {
    setBundling(true); setBundleErr(null);
    if (bundle) URL.revokeObjectURL(bundle.url);
    setBundle(null);
    try {
      // API.blob, not an <a href>: sessions are Bearer-header-only, so a
      // browser-initiated request would 401 (see the note on API.blob).
      const b = await API.blob('/api/support/bundle');
      const now = new Date();
      const p = n => String(n).padStart(2, '0');
      const stamp = `${now.getFullYear()}${p(now.getMonth()+1)}${p(now.getDate())}`
                  + `-${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}`;
      setBundle({ url: URL.createObjectURL(b), name: `echomuse-support-${stamp}.json`, bytes: b.size });
    } catch(e) {
      setBundleErr(e.error || 'Failed to collect bundle');
    }
    setBundling(false);
  }

  function setConf(k, v) { setConfig(c => ({ ...c, [k]: v })); setDirty(true); setSaveMsg(null); }

  // Inline, non-blocking save feedback — was a browser alert(), which
  // demanded a click to dismiss for what is a routine success message.
  const [saveMsg, setSaveMsg] = useState(null); // {ok, text}

  async function saveGlobalConfig() {
    setSaving(true);
    try {
      const res = await API.post('/api/global/config', config);
      onGlobalConfigChange(config);
      setDirty(false);
      const n = res.pushed_to?.length ?? 0;
      setSaveMsg({ ok: true, text: n > 0
        ? `Saved — pushed live to ${n} device${n === 1 ? '' : 's'} on fleet config`
        : 'Saved' });
    } catch(e) {
      setSaveMsg({ ok: false, text: e.error || 'Failed to save global config' });
    }
    setSaving(false);
  }

  async function changePassword() {
    setPwMsg(null);
    if (newPw !== confirmPw) { setPwMsg({ ok: false, text: 'New passwords do not match' }); return; }
    if (newPw.length < 8)    { setPwMsg({ ok: false, text: 'Password must be at least 8 characters' }); return; }
    setPwSaving(true);
    try {
      await API.post('/api/auth/change-password', { current_password: curPw, new_password: newPw });
      setPwMsg({ ok: true, text: 'Password updated' });
      setCurPw(''); setNewPw(''); setConfirmPw('');
    } catch(e) {
      setPwMsg({ ok: false, text: e.error || 'Failed to change password' });
    }
    setPwSaving(false);
  }

  // Support is admin-only because the endpoint is: the bundle spans the whole
  // fleet, so a tab a non-admin can only be refused by is worse than no tab.
  const TABS = isAdmin ? [SETTINGS_TAB.FLEET, SETTINGS_TAB.ACCOUNT, SETTINGS_TAB.SUPPORT]
                       : [SETTINGS_TAB.FLEET, SETTINGS_TAB.ACCOUNT];

  return (
    <ModalFrame onClose={onClose} zIndex={200} label="Settings">

        {/* Header */}
        <div className="em-modal-head" style={{ background:'linear-gradient(180deg,var(--card),var(--bg))', borderBottom:'1px solid var(--border-hard)', padding:'20px 24px 0', boxShadow:'0 1px 0 var(--sheen) inset' }}>
          <div style={{ display:'flex', alignItems:'center', justifyContent:'space-between', marginBottom:16 }}>
            <div style={{ fontFamily:SANS, fontSize:22, color:'var(--text)', fontWeight:600, letterSpacing:'-0.02em' }}>Settings</div>
            <CircleButton onClick={onClose} title="Close">×</CircleButton>
          </div>
          {/* Same raised folder-tab treatment as the device Detail modal —
              one tab style across the dashboard. */}
          <TabBar tabs={TABS} active={tab} onSelect={setTab} labels={SETTINGS_TAB_LABELS}/>
        </div>

        {/* Body */}
        <div className="em-modal-body" style={{ overflowY:'auto', padding:'24px 28px 32px', flex:1 }}>

          {tab === SETTINGS_TAB.FLEET && (
            <>
              <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginBottom:20, lineHeight:1.6 }}>
                Default config applied to all devices unless overridden per-device.
              </div>
              <DeviceConfigForm config={config} onChange={setConf} disabled={!isAdmin}/>
              {dirty && (
                <div style={{ display:'flex', gap:10, marginTop:24 }}>
                  <Pill accent disabled={saving} onClick={saveGlobalConfig}>{saving ? 'Saving…' : 'Save & push to fleet'}</Pill>
                  <Pill onClick={() => { setConfig({...globalConfig}); setDirty(false); setSaveMsg(null); }}>Revert</Pill>
                </div>
              )}
              {saveMsg && (
                <div style={{ marginTop: 14, fontFamily: MONO, fontSize: 11,
                  color: saveMsg.ok ? 'var(--ok)' : 'var(--error)' }}>
                  {saveMsg.ok ? '✓ ' : ''}{saveMsg.text}
                </div>
              )}
            </>
          )}

          {tab === SETTINGS_TAB.ACCOUNT && (
            <div style={{ maxWidth: 360 }}>
              <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)', textTransform:'uppercase', letterSpacing:'0.15em', marginBottom:20 }}>Change Password · {username}</div>
              {[
                ['Current password', curPw, setCurPw],
                ['New password',     newPw, setNewPw],
                ['Confirm new',      confirmPw, setConfirmPw],
              ].map(([label, val, setter]) => (
                <div key={label} style={{ marginBottom:16 }}>
                  <div style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)', marginBottom:6 }}>{label}</div>
                  <input type="password" value={val} onChange={e => setter(e.target.value)} aria-label={label}
                    style={{ width:'100%', boxSizing:'border-box' }}/>
                </div>
              ))}
              {pwMsg && (
                <div style={{ fontFamily:MONO, fontSize:11, color: pwMsg.ok ? 'var(--ok)' : 'var(--error)', marginBottom:12 }}>
                  {pwMsg.text}
                </div>
              )}
              <Pill accent disabled={pwSaving || !curPw || !newPw || !confirmPw} onClick={changePassword}>
                {pwSaving ? 'Updating…' : 'Update password'}
              </Pill>
            </div>
          )}

          {tab === SETTINGS_TAB.SUPPORT && (
            <div style={{ maxWidth: 520 }}>
              <div style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)', marginBottom:20, lineHeight:1.6 }}>
                A single file describing the fleet's state, to attach to a GitHub issue.
              </div>

              <div className="em-panel" style={{ padding:'16px 18px', marginBottom:18 }}>
                <div className="em-label" style={{ marginBottom:10 }}>What it contains</div>
                <div style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)', lineHeight:1.7 }}>
                  Controller, EchoMuse firmware and device OS versions, device capabilities, config,
                  and the last 24 hours of turns, metrics and logs.
                </div>
                <div className="em-label" style={{ margin:'16px 0 10px' }}>What it never contains</div>
                <div style={{ fontFamily:MONO, fontSize:11, color:'var(--text2)', lineHeight:1.7 }}>
                  Transcripts or recordings, device names, Wi-Fi networks,
                  addresses, tokens or passwords. Fields are allowlisted, so
                  anything new is left out until it is added deliberately.
                </div>
              </div>

              <div style={{ display:'flex', gap:10, alignItems:'center' }}>
                <Pill accent disabled={bundling} onClick={collectBundle}>
                  {bundling ? 'Collecting…' : bundle ? 'Collect again' : 'Collect bundle'}
                </Pill>
                {bundle && (
                  <a href={bundle.url} download={bundle.name} className="em-pill em-pill--accent"
                     style={{ textDecoration:'none' }}>
                    Download {(bundle.bytes / 1024).toFixed(0)} KB
                  </a>
                )}
              </div>

              {bundleErr && (
                <div style={{ marginTop:14, fontFamily:MONO, fontSize:11, color:'var(--error)' }}>
                  {bundleErr}
                </div>
              )}
              {bundle && (
                <div style={{ marginTop:14, fontFamily:MONO, fontSize:11, color:'var(--muted)', lineHeight:1.6 }}>
                  Worth opening before you post it — it is plain JSON.
                </div>
              )}
            </div>
          )}

        </div>
    </ModalFrame>
  );
}

// ─── App ──────────────────────────────────────────────────────────────────────

// The page's views below the summary; each value is also its (upper-cased)
// tab label and, but for the default, its URL hash.
const PAGE_VIEW = Object.freeze({ DEVICES: 'devices', ACTIVITY: 'activity' });

function App() {
  // The session is read once: signing in and out both happen on the landing
  // page, which this one navigates to. API.token is set here, synchronously,
  // rather than in an effect — child effects run before a parent's, so an
  // effect would leave any request a child makes on mount unauthenticated.
  const [token] = useState(() => {
    const t = localStorage.getItem('em_token');
    API.token = t;
    return t;
  });
  const [role] = useState(() => localStorage.getItem('em_role'));
  const [devices, setDevices] = useState([]);
  const [selected, setSelected] = useState(null);
  // The firmware bundled in this controller's image ({version, size, sha256})
  // — the only binary it installs. Fixed for the controller's lifetime, so it
  // is read once with the rest of the initial state.
  const [firmware, setFirmware] = useState(null);
  const [ctrlRelease, setCtrlRelease] = useState(null);
  const [ctrlNotesOpen, setCtrlNotesOpen] = useState(false);
  const [status, setStatus] = useState(null);
  const [loadError, setLoadError] = useState(null);
  const [showWizard, setShowWizard] = useState(false);
  const [showDeployAll, setShowDeployAll] = useState(false);
  // Fleet deploy runs entirely server-side (per-device background tasks), so
  // it outlives the modal. deployState persists {version, started, skipped}
  // at the App level so you can close the modal and reopen it (via the header
  // pill) to see live progress — the per-device rows read from `devices`,
  // which the events WebSocket keeps fresh. null = no deploy tracked.
  const [deployState, setDeployState] = useState(null);
  const [showSettings, setShowSettings] = useState(false);
  const [globalConfig, setGlobalConfig] = useState(null);
  // Bumped to open a fresh events socket after the last one closed.
  const [socketEpoch, setSocketEpoch] = useState(0);
  // The view on screen, kept in the URL hash so a reload stays on it. The
  // URL is spelled out: a bare "#activity" resolves against the page's
  // <base href> (the ingress root), not the dashboard.
  const [view, setView] = useState(() =>
    (location.hash === `#${PAGE_VIEW.ACTIVITY}` ? PAGE_VIEW.ACTIVITY : PAGE_VIEW.DEVICES));
  function selectView(v) {
    setView(v);
    history.replaceState(null, '', location.pathname + location.search + (v === PAGE_VIEW.DEVICES ? '' : `#${v}`));
  }

  const isAdmin = role === ROLE.ADMIN;

  function handleLogout() {
    API.post('/api/auth/logout', {}).catch(() => {});
    API.token = null;
    localStorage.removeItem('em_token');
    localStorage.removeItem('em_role');
    // The landing page owns sign-in — green-ring login form.
    location.replace('.');
  }

  // Re-read the fleet. Failures are left to the next poll or event.
  const refreshDevices = () => API.get('/api/devices').then(setDevices).catch(() => {});

  // Load initial data
  useEffect(() => {
    if (!token) return;
    Promise.all([
      API.get('/api/devices'),
      API.get('/api/system/status'),
      API.get('/api/firmware').catch(() => null),
      API.get('/api/global/config').catch(() => null),
      API.get('/api/releases/controller').catch(() => null),
    ]).then(([devs, stat, fw, gcfg, ctrl]) => {
      setDevices(devs);
      setStatus(stat);
      setFirmware(fw);
      setCtrlRelease(ctrl);
      if (gcfg) setGlobalConfig(gcfg);
    }).catch(e => {
      if (e.code === 'not_authenticated') { handleLogout(); }
      else setLoadError(e.error || 'Failed to load');
    });
  }, [token]);

  // Live events WebSocket
  useEffect(() => {
    if (!token) return;
    const ws = new WebSocket(ingressWebSocketUrl(`/api/events?token=${token}`));
    let closedByUs = false;
    let reconnect = null;

    ws.onmessage = e => {
      const msg = JSON.parse(e.data);
      _emitEvent(msg);
      switch(msg.type) {
        case EVENT_TYPE.SNAPSHOT:
          setDevices(msg.devices);
          break;
        case EVENT_TYPE.DEVICE_UPDATE:
          // Merge partial state directly — no API round trip needed
          if (msg.state) {
            setDevices(prev => prev.map(d =>
              d.device_id === msg.device_id ? { ...d, ...msg.state } : d
            ));
          }
          break;
        case EVENT_TYPE.DEVICE_LOG:
        case EVENT_TYPE.ALERTS:
        case EVENT_TYPE.HA_STATUS:
          // Detail panels consume these through subscribeEvents.
          break;
        case EVENT_TYPE.TURN_COMPLETE:
          // The Activity views fetch their own lists (subscribeEvents).
          break;
        case EVENT_TYPE.DEVICE_CONNECTED:
          setDevices(prev => prev.map(d =>
            d.device_id === msg.device_id ? { ...d, connected: true } : d
          ));
          break;
        case EVENT_TYPE.DEVICE_DISCONNECTED:
          console.log('[ws] device_disconnected:', msg.device_id);
          setDevices(prev => prev.map(d =>
            d.device_id === msg.device_id
              ? { ...d, connected: false, speaking: false, listening: false, thinking: false }
              : d
          ));
          break;
        case EVENT_TYPE.DEVICE_UPDATE_QUEUE:
          // A queued install was set or cleared: merge it, no round trip.
          setDevices(prev => prev.map(d =>
            d.device_id === msg.device_id ? { ...d, update_queued_at: msg.queued_at } : d
          ));
          break;
        case EVENT_TYPE.DEVICE_UPDATED:
        case EVENT_TYPE.DEVICE_ROLLED_BACK:
        case EVENT_TYPE.DEVICE_AUTO_ROLLED_BACK:
        case EVENT_TYPE.DEVICE_UPDATE_FAILED:
        case EVENT_TYPE.DEVICE_APPROVED:
        case EVENT_TYPE.DEVICE_PENDING:
          // Full refresh for structural changes
          refreshDevices();
          break;
        case EVENT_TYPE.CONTROLLER_UPDATE:
          // The controller polls GitHub hourly; a dashboard left open should
          // learn about a new controller without a reload.
          setCtrlRelease(msg);
          break;
        case EVENT_TYPE.DEVICE_DELETED:
          setDevices(prev => prev.filter(d => d.device_id !== msg.device_id));
          break;
      }
    };

    // Reconnect 5s after the socket drops (controller restart, network
    // blip) by bumping the epoch this effect depends on. This used to call
    // setToken(t => t), which React treats as no change: nothing re-ran, so
    // one dropped socket ended live alert/timer events until a reload — the
    // device poll below hid it for the fleet view, nothing did for the rest.
    ws.onclose = () => {
      if (closedByUs) return;
      reconnect = setTimeout(() => setSocketEpoch(n => n + 1), 5000);
    };

    // Polling fallback — catches anything the WebSocket misses
    const poll = setInterval(refreshDevices, 5000);

    return () => {
      closedByUs = true;
      clearTimeout(reconnect);
      ws.close();
      clearInterval(poll);
    };

  }, [token, socketEpoch]);

  // No session (direct visit, expired token, logged out) — the landing
  // page owns auth: it validates any stored token and shows the right
  // form (login vs first-run setup).
  if (!token) { location.replace('.'); return null; }

  const online   = devices.filter(d => d.connected).length;
  const approved = devices.filter(d => d.approved);
  const pending  = devices.filter(d => !d.approved);
  const updates  = approved.filter(d => firmwareBehind(d, firmware)).length;
  const active   = approved.filter(d => d.speaking || d.listening || d.thinking).length;

  const selectedDevice = selected ? devices.find(d => d.device_id === selected) : null;

  return (
    <div className="em-page" style={{ minHeight: '100vh', padding: '32px 36px 60px' }}>

      {/* Header */}
      <div className="em-header" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-end', marginBottom: 36 }}>
        <div style={{ display: 'flex', alignItems: 'baseline', gap: 14 }}>
          <div style={{ fontFamily: SANS, fontSize: 28, color: 'var(--text)', fontWeight: 600, letterSpacing: '-0.02em' }}>EchoMuse</div>
          <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)', letterSpacing: '0.12em', textTransform: 'uppercase' }}>Device Management</div>
          {status?.controller_version && (
            <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)' }}>{status.controller_version}</div>
          )}
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
          <div style={{ fontFamily: MONO, fontSize: 10, color: 'var(--muted)' }}>{role}</div>
          <ThemeToggle/>
          <IconButton onClick={() => setShowSettings(true)} label="Settings">⚙</IconButton>
          <IconButton onClick={handleLogout} label="Sign out" danger><SignOutIcon/></IconButton>
        </div>
      </div>


      {/* Controller update notice.
          Rendered ONLY when a newer controller-v* tag exists, so it is an
          alert rather than permanent chrome — a panel that is always present
          stops being read.

          There is deliberately no update button. The controller is a
          container the user owns and updates with their own docker tooling;
          an in-app "update" would have to restart the process serving the
          page, mid-request, with no way to report the outcome. So this shows
          the version, what changed, and the exact command — everything needed
          to decide — and leaves the doing to them. */}
      {ctrlRelease?.available && ctrlRelease?.version && (
        <div className="em-ctrl-update" style={{
          background: 'var(--notice-bg)',
          border: '1px solid var(--notice-line)', borderRadius: 8,
          padding: '14px 18px', marginBottom: 24,
        }}>
          <div style={{ display:'flex', alignItems:'center', gap:12, flexWrap:'wrap' }}>
            <span style={{ fontFamily:MONO, fontSize:8, color:'var(--warn)',
                           textTransform:'uppercase', letterSpacing:'0.15em' }}>
              Controller update
            </span>
            <span style={{ fontFamily:MONO, fontSize:14, color:'var(--warn)' }}>
              {ctrlRelease.version}
            </span>
            <span style={{ fontFamily:MONO, fontSize:10, color:'var(--muted)' }}>
              running {ctrlRelease.current || status?.controller_version || '—'}
            </span>
            {ctrlRelease.notes && (
              <DisclosureToggle open={ctrlNotesOpen} onToggle={() => setCtrlNotesOpen(o => !o)}
                                style={{ width:'auto', marginLeft:'auto' }}>
                What&apos;s in it
              </DisclosureToggle>
            )}
          </div>
          {ctrlNotesOpen && (
            <div style={{ marginTop:12, borderTop:'1px solid rgba(255,255,255,0.06)', paddingTop:12 }}>
              <pre style={{
                fontFamily:MONO, fontSize:10, lineHeight:1.65,
                color:'var(--text2)', whiteSpace:'pre-wrap', wordBreak:'break-word',
                margin:0, maxHeight:320, overflowY:'auto',
              }}>{ctrlRelease.notes}</pre>
              <div style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)',
                            marginTop:14, lineHeight:1.6 }}>
                Update it yourself, from wherever your compose file lives:
              </div>
              <pre style={{
                fontFamily:MONO, fontSize:10, color:'var(--lcd-green)',
                background:'rgba(0,0,0,0.35)', border:'1px solid rgba(0,0,0,0.5)',
                borderRadius:6, padding:'10px 12px', margin:'8px 0 0', overflowX:'auto',
              }}>docker compose pull &amp;&amp; docker compose up -d</pre>
              {ctrlRelease.release_url && (
                <a href={ctrlRelease.release_url} target="_blank" rel="noreferrer"
                   style={{ fontFamily:MONO, fontSize:9, color:'var(--muted)',
                            display:'inline-block', marginTop:10 }}>
                  View tag on GitHub →
                </a>
              )}
            </div>
          )}
        </div>
      )}

      {/* Summary */}
      <div className="em-summary" style={{ display: 'flex', gap: 10, marginBottom: 36 }}>
        {[
          ['Online', `${online}/${approved.length}`, online === approved.length ? 'var(--ok)' : 'var(--warn)'],
          ['Active', active, active > 0 ? 'var(--accent)' : 'var(--muted)'],
          ['Updates', updates, updates > 0 ? 'var(--warn)' : 'var(--muted)'],
          ['Pending', pending.length, pending.length > 0 ? 'var(--accent-hi)' : 'var(--muted)'],
        ].map(([label, val, c]) => (
          <div key={label} className="em-inset" style={{ flex: 1 }}>
            <div style={{ fontFamily: MONO, fontSize: 8, color: 'var(--lcd-dim)', textTransform: 'uppercase', letterSpacing: '0.15em', marginBottom: 6 }}>{label}</div>
            <div style={{ fontFamily: MONO, fontSize: 24, color: c, lineHeight: 1, textShadow: `0 0 12px ${c}66` }}>{val}</div>
          </div>
        ))}
        {firmware && (
          <div className="em-summary-release em-inset" style={{ flex: 2, display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <div>
              <div style={{ fontFamily: MONO, fontSize: 8, color: 'var(--lcd-dim)', textTransform: 'uppercase', letterSpacing: '0.15em', marginBottom: 6 }}>Bundled EchoMuse Firmware</div>
              <div style={{ fontFamily: MONO, fontSize: 18, color: 'var(--lcd-green)', lineHeight: 1 }}>{firmware.version}</div>
            </div>
            {/* Actions as ONE flex child, not several.
                space-between distributes across every child it has, so with
                the version block and each button as siblings it spread them
                evenly over a double-width panel — the buttons ended up
                marooned in the middle. Grouping them leaves two children:
                version left, actions right. */}
            <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexShrink: 0 }}>
            {isAdmin && (() => {
              const byId = Object.fromEntries(devices.map(d => [d.device_id, d]));
              const started = deployState ? (deployState.started || []) : [];
              const done = started.filter(id => {
                const d = byId[id];
                return d && d.connected && d.firmware_ver === deployState.version;
              }).length;
              // Failures are terminal too — otherwise one aborted update
              // pinned the pill at "Deploying…" until the page was reloaded.
              const failed = started.filter(id => {
                const d = byId[id];
                return d && d.update_error &&
                  !(d.connected && d.firmware_ver === deployState.version);
              }).length;
              const complete = started.length > 0 && done + failed === started.length;
              // While a deploy is in flight the progress pill replaces the
              // Deploy all button — both open the same modal, and offering a
              // second deploy mid-run reads as a broken control. The button
              // returns once the fleet is done.
              const inFlight = deployState && !complete;
              return (<>
                {!inFlight && (
                  <IconButton accent onClick={() => setShowDeployAll(true)}
                              label={`Deploy bundled EchoMuse firmware ${firmware.version} to all devices`}><DeployIcon/></IconButton>
                )}
                {deployState && (
                  <Pill small onClick={() => setShowDeployAll(true)}>
                    {complete
                      ? (failed > 0
                          ? `⚠ ${deployState.version}: ${done} ok, ${failed} failed`
                          : `✓ Fleet on ${deployState.version}`)
                      : `Deploying ${deployState.version} — ${done}/${started.length}`}
                  </Pill>
                )}
              </>);
            })()}
            </div>
          </div>
        )}
      </div>

      <div style={{ borderBottom: '1px solid var(--border-hard)', marginBottom: 28 }}>
        <TabBar tabs={[PAGE_VIEW.DEVICES, PAGE_VIEW.ACTIVITY]} active={view} onSelect={selectView}/>
      </div>

      {view === PAGE_VIEW.ACTIVITY && (
        <FleetActivity devices={devices} globalConfig={globalConfig} onOpenDevice={setSelected} isAdmin={isAdmin}/>
      )}

      {/* Pending devices */}
      {view === PAGE_VIEW.DEVICES && pending.length > 0 && (
        <>
          <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--accent-hi)', textTransform: 'uppercase', letterSpacing: '0.15em', marginBottom: 14 }}>
            Pending Approval · {pending.length}
          </div>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill,minmax(190px,1fr))', gap: 12, marginBottom: 36 }}>
            {pending.map(d => <Card key={d.device_id} device={d} onClick={() => setSelected(d.device_id)}/>)}
          </div>
        </>
      )}

      {/* Device grid */}
      {view === PAGE_VIEW.DEVICES && (approved.length > 0 || isAdmin) && (
        <>
          {approved.length > 0 && (
            <div style={{ fontFamily: MONO, fontSize: 9, color: 'var(--muted)', textTransform: 'uppercase', letterSpacing: '0.15em', marginBottom: 14 }}>
              Devices · {approved.length}
            </div>
          )}
          {/* gridAutoRows:1fr equalises every row to the tallest item, so the
              provisioning tile is the same size as a device card instead of
              collapsing to its own content when it wraps onto a row alone.
              A matching height rather than a matching magic number — the card
              can gain a row without this drifting. */}
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill,minmax(190px,1fr))', gridAutoRows: '1fr', gap: 12, marginBottom: 48 }}>
            {approved.map(d => <Card key={d.device_id} device={d} firmware={firmware} onClick={() => setSelected(d.device_id)}/>)}
            {isAdmin && <AddDeviceTile onClick={() => setShowWizard(true)}/>}
          </div>
        </>
      )}

      {view === PAGE_VIEW.DEVICES && devices.length === 0 && !loadError && !isAdmin && (
        <div style={{ textAlign: 'center', padding: '60px 0', fontFamily: MONO, fontSize: 12, color: 'var(--muted)' }}>
          No devices yet — power on an EchoMuse device to see it appear here
        </div>
      )}

      {loadError && (
        <div style={{ textAlign: 'center', padding: '60px 0', fontFamily: MONO, fontSize: 12, color: 'var(--error)' }}>{loadError}</div>
      )}

      {/* Provisioning wizard */}
      {showWizard && (
        <ProvisionWizard onClose={() => setShowWizard(false)} knownDevices={devices}/>
      )}

      {/* Fleet-wide OTA — the deploy itself is server-side; this modal is
          just the view, backed by App-level deployState so it survives close. */}
      {showDeployAll && (
        <DeployAllModal
          firmware={firmware}
          devices={devices}
          deployState={deployState}
          onStarted={setDeployState}
          onDismiss={() => setDeployState(null)}
          onClose={() => setShowDeployAll(false)}
        />
      )}

      {/* Settings panel */}
      {showSettings && globalConfig && (
        <SettingsPanel
          globalConfig={globalConfig}
          onGlobalConfigChange={setGlobalConfig}
          onClose={() => setShowSettings(false)}
          username={role}
          isAdmin={isAdmin}
        />
      )}

      {/* Detail modal */}
      {selectedDevice && (
        <Detail
          device={selectedDevice}
          token={token}
          onClose={() => setSelected(null)}
          onApprove={refreshDevices}
          isAdmin={isAdmin}
          globalConfig={globalConfig}
          firmware={firmware}
          onDeviceConfigChange={(device_id, patch) =>
            setDevices(prev => prev.map(d =>
              d.device_id === device_id ? { ...d, ...patch } : d
            ))
          }
        />
      )}
    </div>
  );
}

ReactDOM.createRoot(document.getElementById('root')).render(<App/>);
