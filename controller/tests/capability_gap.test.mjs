// Tests for the dashboard's capability gating in dashboard.jsx: which reason a
// disabled control shows when the live device lacks the capability it needs.
//
//     node controller/tests/capability_gap.test.mjs
//
// Source extraction rather than import, for the reason wifi_scan.test.mjs
// gives: the dashboard compiles to a single classic script with no module
// boundary, so the alternative is a second copy that drifts.
//
// The platform-dependent reasons ("managed by the device" on an Echo Show,
// "no LED ring on this device" on a Fire OS Show, "firmware update available"
// on an older Dot or LineageOS Show) are the controller's
// (em_device.capability_gap, sent as `capability_gaps`); the dashboard must
// show those first and fall back to its own per-capability text.

import { readFileSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";

const HERE = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(HERE, "..", "static", "dashboard.jsx"), "utf8");

function liftConst(name) {
  const start = src.indexOf(`const ${name} = `);
  if (start < 0) throw new Error(`dashboard.jsx no longer defines ${name}`);
  let depth = 0;
  for (let i = start; i < src.length; i++) {
    const ch = src[i];
    if (ch === "{" || ch === "(" || ch === "[") depth++;
    else if (ch === "}" || ch === ")" || ch === "]") depth--;
    else if (ch === ";" && depth === 0) return src.slice(start, i + 1);
  }
  throw new Error(`could not find the end of ${name}`);
}

function liftFunctionDecl(name) {
  const start = src.indexOf(`function ${name}(`);
  if (start < 0) throw new Error(`dashboard.jsx no longer defines ${name}()`);
  let depth = 0, seen = false, i = src.indexOf("{", start);
  for (; i < src.length; i++) {
    if (src[i] === "{") { depth++; seen = true; }
    else if (src[i] === "}") { depth--; if (seen && depth === 0) break; }
  }
  return src.slice(start, i + 1);
}

const code = [
  liftConst("CAPABILITY"),
  liftConst("CAPABILITY_REASONS"),
  liftConst("PLATFORM_LABEL"),
  liftConst("SCREEN_OVERLAY_REASON"),
  liftFunctionDecl("capabilityGap"),
  liftFunctionDecl("liveCapabilities"),
  liftFunctionDecl("kioskApiGap"),
  liftFunctionDecl("_known"),
  liftFunctionDecl("companionRows"),
  liftConst("KIOSK_ERROR"),
  liftConst("KIOSK_ERROR_TEXT"),
  liftFunctionDecl("kioskRows"),
  liftFunctionDecl("kioskHealth"),
  liftFunctionDecl("kioskLoginReason"),
  "export { CAPABILITY, CAPABILITY_REASONS, PLATFORM_LABEL, capabilityGap, liveCapabilities,",
  "         kioskApiGap, companionRows, kioskRows, kioskLoginReason };",
].join("\n");
const { CAPABILITY, CAPABILITY_REASONS, PLATFORM_LABEL, capabilityGap, liveCapabilities,
        kioskApiGap, companionRows, kioskRows, kioskLoginReason } = await import(
  "data:text/javascript;base64," + Buffer.from(code).toString("base64"));

let failures = 0;
function check(name, actual, expected) {
  if (actual === expected) return;
  failures++;
  console.error(`FAIL: ${name}\n      expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

const REQUIRED = ["audio_timeline_v1", "uplink_leases_v1", "device_wake_v1", "render_reference_v1",
                  "render_progress_v1", "focus_leases_v1", "alert_cache_v1", "turn_protocol_v1"];

// Every platform the controller admits has a label on the Status tab.
{
  const py = readFileSync(join(HERE, "..", "em_device_link.py"), "utf8");
  const block = py.slice(py.indexOf("class DevicePlatform("), py.indexOf("class RejectReason("));
  const platforms = [...block.matchAll(/^\s+[A-Z0-9_]+ = "([a-z0-9_]+)"/gm)].map(m => m[1]);
  check("DevicePlatform is read", platforms.length > 0, true);
  for (const p of platforms) check(`${p} has a platform label`, typeof PLATFORM_LABEL[p], "string");
}

// Each Echo Show image as /api/devices reports it (em_api._merge_device).
// Both leave Wi-Fi, BLE and mute to the device; only Fire OS's lacks the ring
// and the button, which a LineageOS Show's companion app puts on screen.
const SHOW = [...REQUIRED, "local_wake_chime", "open_rules_v1", "screen_wake"];
const SHOW_GAPS = {
  wifi_control: "managed by the device", ble_proxy: "managed by the device",
  privacy_mute: "mute not supported on this device yet",
};
const SHOWS = {
  fireos6_crown: {
    connected: true, platform: "fireos6_crown", capabilities: SHOW,
    capability_gaps: {
      ...SHOW_GAPS,
      leds: "no LED ring on this device", led_anim: "no LED ring on this device",
      buttons: "no action button on this device", button_hold: "no action button on this device",
      turn_view_v1: "needs LineageOS on this Echo Show", kiosk_plugin: "needs LineageOS on this Echo Show",
    },
  },
  lineage18_crown: {
    connected: true, platform: "lineage18_crown",
    capabilities: [...SHOW, "leds", "led_anim", "buttons", "button_hold", "screen_overlay", "turn_view_v1", "kiosk_plugin"],
    capability_gaps: SHOW_GAPS,
  },
};
for (const [platform, show] of Object.entries(SHOWS)) {
  const s = liveCapabilities(show);
  check(`${platform}: Wi-Fi is managed by the device`, capabilityGap(s, CAPABILITY.WIFI_CONTROL), "managed by the device");
  check(`${platform}: BLE proxy is managed by the device`, capabilityGap(s, CAPABILITY.BLE_PROXY), "managed by the device");
  check(`${platform}: mute is not supported yet`, capabilityGap(s, CAPABILITY.PRIVACY_MUTE), "mute not supported on this device yet");
  check(`${platform}: screen wake is available`, capabilityGap(s, CAPABILITY.SCREEN_WAKE), null);
  check(`${platform}: the AFE controls fall back to the dashboard's own reason`,
        capabilityGap(s, CAPABILITY.AFE_METADATA), CAPABILITY_REASONS[CAPABILITY.AFE_METADATA]);
}
{
  const s = liveCapabilities(SHOWS.fireos6_crown);
  check("fireos6_crown: no ring, so no ring animation either", capabilityGap(s, CAPABILITY.LED_ANIM), "no LED ring on this device");
  check("fireos6_crown: the ring controls are gated on leds", capabilityGap(s, CAPABILITY.LEDS), "no LED ring on this device");
  check("fireos6_crown: no action button", capabilityGap(s, CAPABILITY.BUTTON_HOLD), "no action button on this device");
  check("fireos6_crown: the kiosk API lights the screen", kioskApiGap(s), null);
  check("fireos6_crown: no turn view", capabilityGap(s, CAPABILITY.TURN_VIEW), "needs LineageOS on this Echo Show");
  check("fireos6_crown: no Kiosk Satellite plugin", capabilityGap(s, CAPABILITY.KIOSK_PLUGIN), "needs LineageOS on this Echo Show");
}
{
  const s = liveCapabilities(SHOWS.lineage18_crown);
  check("lineage18_crown: the ring controls are available", capabilityGap(s, CAPABILITY.LEDS), null);
  check("lineage18_crown: ring animation is available", capabilityGap(s, CAPABILITY.LED_ANIM), null);
  check("lineage18_crown: the action button is available", capabilityGap(s, CAPABILITY.BUTTON_HOLD), null);
  check("lineage18_crown: the app lights the screen, not the kiosk API", kioskApiGap(s),
        "the EchoMuse app lights the screen");
  check("lineage18_crown: the turn view is available", capabilityGap(s, CAPABILITY.TURN_VIEW), null);
  check("lineage18_crown: the plugin is available", capabilityGap(s, CAPABILITY.KIOSK_PLUGIN), null);
}
{
  // LineageOS firmware from before the companion app.
  const UPDATE = "firmware update available";
  const s = liveCapabilities({
    connected: true, platform: "lineage18_crown", capabilities: SHOW,
    capability_gaps: { ...SHOW_GAPS, leds: UPDATE, led_anim: UPDATE, buttons: UPDATE,
                       button_hold: UPDATE, screen_overlay: UPDATE, turn_view_v1: UPDATE, kiosk_plugin: UPDATE },
  });
  check("older lineage18_crown: the ring waits for the update", capabilityGap(s, CAPABILITY.LEDS), UPDATE);
  check("older lineage18_crown: the button waits for the update", capabilityGap(s, CAPABILITY.BUTTON_HOLD), UPDATE);
  check("older lineage18_crown: the kiosk API still lights the screen", kioskApiGap(s), null);
  check("older lineage18_crown: the plugin waits for the update", capabilityGap(s, CAPABILITY.KIOSK_PLUGIN), UPDATE);
  check("older lineage18_crown: the turn view waits for the update", capabilityGap(s, CAPABILITY.TURN_VIEW), UPDATE);
}
// A Dot on firmware predating the Dot-only capabilities and led_anim.
const olderDot = {
  connected: true, platform: "fireos6",
  capabilities: [...REQUIRED, "leds", "buttons", "button_hold"],
  capability_gaps: {
    wifi_control: "firmware update available", ble_proxy: "firmware update available",
    privacy_mute: "firmware update available", screen_wake: "no screen on this device",
    screen_overlay: "no screen on this device", turn_view_v1: "no screen on this device",
    kiosk_plugin: "no screen on this device",
  },
};
const d = liveCapabilities(olderDot);
check("older Dot: Wi-Fi waits for the update", capabilityGap(d, CAPABILITY.WIFI_CONTROL), "firmware update available");
check("older Dot: BLE proxy waits for the update", capabilityGap(d, CAPABILITY.BLE_PROXY), "firmware update available");
check("older Dot: mute waits for the update", capabilityGap(d, CAPABILITY.PRIVACY_MUTE), "firmware update available");
check("older Dot: no screen", capabilityGap(d, CAPABILITY.SCREEN_WAKE), "no screen on this device");
check("older Dot: led_anim keeps the dashboard's reason",
      capabilityGap(d, CAPABILITY.LED_ANIM), CAPABILITY_REASONS[CAPABILITY.LED_ANIM]);
check("older Dot: the ring itself is available", capabilityGap(d, CAPABILITY.LEDS), null);
check("older Dot: no screen, so no screen overlay", capabilityGap(d, CAPABILITY.SCREEN_OVERLAY), "no screen on this device");
check("older Dot: no screen, so no turn view", capabilityGap(d, CAPABILITY.TURN_VIEW), "no screen on this device");
check("older Dot: no screen, so no kiosk API", kioskApiGap(d), "no screen on this device");

// Nothing is gated without a live session to ask: the fleet view, or an
// offline device whose settings apply when it returns.
check("offline: nothing gated", liveCapabilities({ ...olderDot, connected: false }), null);
check("fleet view: nothing gated", capabilityGap(null, CAPABILITY.WIFI_CONTROL), null);
check("fleet view: the kiosk API is not gated", kioskApiGap(null), null);
// A response from a controller predating capability_gaps still gates.
check("no gaps object: generic reason",
      capabilityGap(liveCapabilities({ connected: true, capabilities: REQUIRED }), CAPABILITY.WIFI_CONTROL),
      "needs the wifi_control capability — update this Echo");

// The Status tab's companion-app rows (stats.companion, LineageOS Show only):
// none at all without the field, and a null value is "unavailable", never a
// "no" the device did not say.
const text = rows => rows && rows.map(([label, value]) => `${label}=${value}`).join(", ");
check("Dot stats: no companion rows", companionRows({ cpuPct: 4, companion: null }), null);
check("no stats: no companion rows", companionRows(null), null);
check("app never linked this run",
      text(companionRows({ companion: { version: null, linked: false, overlay: null, error: null } })),
      "Linked=Not linked, App version=unavailable, Overlay=unavailable, Last error=none");
check("app linked, overlay allowed",
      text(companionRows({ companion: { version: "cmp-0123456789ab", linked: true, overlay: true, error: null } })),
      "Linked=Linked, App version=cmp-0123456789ab, Overlay=Allowed, Last error=none");
check("overlay revoked, installer error",
      text(companionRows({ companion: { version: "cmp-0123456789ab", linked: true, overlay: false,
                                        error: "pm install: exit 1" } })),
      "Linked=Linked, App version=cmp-0123456789ab, Overlay=Not allowed, Last error=pm install: exit 1");
check("unknown link state is unavailable",
      text(companionRows({ companion: { version: null, linked: null, overlay: null, error: null } })),
      "Linked=unavailable, App version=unavailable, Overlay=unavailable, Last error=none");

// The Kiosk Satellite rows (stats.companion.kiosk): one row without it
// installed, "unavailable" for every unknown, and the two error codes in words.
check("no kiosk report: no kiosk rows", kioskRows({ companion: { linked: true } }), null);
check("Kiosk Satellite not installed: one row",
  text(kioskRows({ companion: { kiosk: { installed: false, version: null, remote_admin: null,
                                         plugin: { version: null, linked: false, error: null, switched_on: null } } } })),
  "Kiosk Satellite=Not installed");
check("installed, plugin linked",
  text(kioskRows({ companion: { kiosk: { installed: true, version: "2026.10.28", remote_admin: true,
    plugin: { version: "1.0.0-0123456789ab", linked: true, error: null, switched_on: false } } } })),
  "Kiosk Satellite=2026.10.28, Remote admin=Answering, Plugin=Linked, Plugin version=1.0.0-0123456789ab, "
  + "Plugins switched on=no, Plugin error=none");
check("unknown fields are unavailable, never no",
  text(kioskRows({ companion: { kiosk: { installed: null, version: null, remote_admin: null,
    plugin: { version: null, linked: null, error: "token_rejected", switched_on: null } } } })),
  "Kiosk Satellite=unavailable, Remote admin=unavailable, Plugin=unavailable, Plugin version=unavailable, "
  + "Plugins switched on=unavailable, Plugin error=Kiosk Satellite rejected the token — log in again (Config → Screen)");

// When the dashboard asks for the password before an update (decision 4):
// Kiosk Satellite installed, and no token held or the held one rejected.
// Offline, the last report (`kiosk`) stands in for the live stats.
const kiosk = (installed, error = null) => ({ installed, version: null, remote_admin: true,
  plugin: { version: null, linked: false, error, switched_on: null } });
const show = (k, tokenSet, connected = true) => ({ connected, kioskTokenSet: tokenSet,
  stats: connected ? { companion: { kiosk: k } } : null, kiosk: k });
check("installed, no token: asks", typeof kioskLoginReason(show(kiosk(true), false)), "string");
check("installed, token held: does not ask", kioskLoginReason(show(kiosk(true), true)), null);
check("installed, token rejected: asks again",
  typeof kioskLoginReason(show(kiosk(true, "token_rejected"), true)), "string");
check("not installed: does not ask", kioskLoginReason(show(kiosk(false), false)), null);
check("installed unknown: does not ask", kioskLoginReason(show(kiosk(null), false)), null);
check("no report at all: does not ask", kioskLoginReason({ connected: true, kioskTokenSet: false, stats: {} }), null);
check("offline with the last report: asks before a queued update",
  typeof kioskLoginReason(show(kiosk(true), false, false)), "string");

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("capability_gap: all checks passed");
