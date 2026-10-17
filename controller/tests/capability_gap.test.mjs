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
// "firmware update available" on an older Dot) are the controller's
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
  liftFunctionDecl("capabilityGap"),
  liftFunctionDecl("liveCapabilities"),
  "export { CAPABILITY, CAPABILITY_REASONS, PLATFORM_LABEL, capabilityGap, liveCapabilities };",
].join("\n");
const { CAPABILITY, CAPABILITY_REASONS, PLATFORM_LABEL, capabilityGap, liveCapabilities } = await import(
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

// An Echo Show as /api/devices reports it (em_api._merge_device), on either
// image: the controller names the same reasons for both.
for (const platform of ["fireos6_crown", "lineage18_crown"]) {
  const show = {
    connected: true, platform,
    capabilities: [...REQUIRED, "local_wake_chime", "open_rules_v1", "screen_wake"],
    capability_gaps: {
      leds: "no LED ring on this device", led_anim: "no LED ring on this device",
      buttons: "no action button on this device", button_hold: "no action button on this device",
      wifi_control: "managed by the device", ble_proxy: "managed by the device",
      privacy_mute: "mute not supported on this device yet",
    },
  };
  const s = liveCapabilities(show);
  check(`${platform}: Wi-Fi is managed by the device`, capabilityGap(s, CAPABILITY.WIFI_CONTROL), "managed by the device");
  check(`${platform}: BLE proxy is managed by the device`, capabilityGap(s, CAPABILITY.BLE_PROXY), "managed by the device");
  check(`${platform}: mute is not supported yet`, capabilityGap(s, CAPABILITY.PRIVACY_MUTE), "mute not supported on this device yet");
  check(`${platform}: no ring, so no ring animation either`, capabilityGap(s, CAPABILITY.LED_ANIM), "no LED ring on this device");
  check(`${platform}: the ring controls are gated on leds`, capabilityGap(s, CAPABILITY.LEDS), "no LED ring on this device");
  check(`${platform}: no action button`, capabilityGap(s, CAPABILITY.BUTTON_HOLD), "no action button on this device");
  check(`${platform}: screen wake is available`, capabilityGap(s, CAPABILITY.SCREEN_WAKE), null);
  check(`${platform}: the AFE controls fall back to the dashboard's own reason`,
        capabilityGap(s, CAPABILITY.AFE_METADATA), CAPABILITY_REASONS[CAPABILITY.AFE_METADATA]);
}
// A Dot on firmware predating the Dot-only capabilities and led_anim.
const olderDot = {
  connected: true, platform: "fireos6",
  capabilities: [...REQUIRED, "leds", "buttons", "button_hold"],
  capability_gaps: {
    wifi_control: "firmware update available", ble_proxy: "firmware update available",
    privacy_mute: "firmware update available", screen_wake: "no screen on this device",
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

// Nothing is gated without a live session to ask: the fleet view, or an
// offline device whose settings apply when it returns.
check("offline: nothing gated", liveCapabilities({ ...olderDot, connected: false }), null);
check("fleet view: nothing gated", capabilityGap(null, CAPABILITY.WIFI_CONTROL), null);
// A response from a controller predating capability_gaps still gates.
check("no gaps object: generic reason",
      capabilityGap(liveCapabilities({ connected: true, capabilities: REQUIRED }), CAPABILITY.WIFI_CONTROL),
      "needs the wifi_control capability — update this Echo");

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("capability_gap: all checks passed");
