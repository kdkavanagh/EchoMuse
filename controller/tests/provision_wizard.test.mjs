// Tests for the provisioning wizard's pure pieces in dashboard.jsx: the
// disconnect-error filter, the platform check connect_android runs before it
// trusts the shell, the step order, and the boot hook it writes.
//
//     node controller/tests/provision_wizard.test.mjs
//
// Source extraction rather than import, for the reason wifi_scan.test.mjs
// gives: the dashboard compiles to a single classic script with no module
// boundary, so the alternative is a second copy that drifts.

import { readFileSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";

const HERE = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(HERE, "..", "static", "dashboard.jsx"), "utf8");

// Handles both arrow forms in this file: `= (x) => { ... };` with a body, and
// `= (x) => expr || expr;` without one. Scanning for a matching brace only
// works for the first, and silently swallows the following declaration for the
// second, which is how this test first failed with "already declared".
function liftArrow(name) {
  const start = src.indexOf(`const ${name} = (`);
  if (start < 0) {
    throw new Error(`dashboard.jsx no longer defines ${name}() — if it was `
                  + `renamed or moved, update this test to match`);
  }
  let depth = 0;
  for (let i = start; i < src.length; i++) {
    const ch = src[i];
    if (ch === "{" || ch === "(" || ch === "[") depth++;
    else if (ch === "}" || ch === ")" || ch === "]") depth--;
    else if (ch === ";" && depth === 0) return src.slice(start, i + 1);
  }
  throw new Error(`could not find the end of ${name}`);
}

// A `const NAME = …;` declaration of any shape, to its terminating `;` at
// bracket depth 0 (the same scan as liftArrow).
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

const lift = parts => import(
  "data:text/javascript;base64," + Buffer.from(parts.join("\n")).toString("base64"));

let failures = 0;
function check(name, cond, detail) {
  if (cond) return;
  failures++;
  console.error(`FAIL: ${name}${detail ? `\n      ${detail}` : ""}`);
}

// ─── Disconnect-shaped errors ─────────────────────────────────────────────────
//
// The in-flight transfer can throw BEFORE the WebUSB disconnect event lands —
// the two race, and on a real run the throw won. That put "Failed to execute
// 'transferOut' on 'USBDevice'" in the transcript as though it were a
// provisioning failure, and then sent the diagnostics probes at a device that
// was no longer plugged in.

const { _isDisconnectError } = await lift(
  [liftArrow("_isDisconnectError"), "export { _isDisconnectError };"]);

// The real one, from the transcript that prompted this.
check("the observed transferOut error is recognised",
  _isDisconnectError(new Error(
    "Failed to execute 'transferOut' on 'USBDevice': The device was disconnected.")));
check("the transferIn direction is recognised",
  _isDisconnectError(new Error(
    "Failed to execute 'transferIn' on 'USBDevice': The device was disconnected.")));
check("a NetworkError is recognised",
  _isDisconnectError(new Error("NetworkError: A transfer error has occurred.")));

// Provisioning failures must NOT be swallowed as disconnects: taking the
// abandon path on a genuine failure hides the real message and skips the
// diagnostics capture, which is the whole point of #87.
for (const msg of [
  "Service did not reach \"running\" within 90s — init.svc.echomuse=stopped, init.svc.mixer=running.",
  "The device did not reach this controller within 180s.",
  "Slot _b (inactive): echomuse.rc or /tmp did not read back as written.",
  "Install verification failed: /data/local/bin/server points to \"\"",
]) {
  check(`a real failure is not treated as a disconnect: ${msg.slice(0, 34)}`,
        !_isDisconnectError(new Error(msg)), msg);
}

check("a missing message does not throw", _isDisconnectError(undefined) === false);
check("an error with no message does not throw", _isDisconnectError({}) === false);

// ─── Platform check (connect_android) ───────────────────────────────────────
//
// Only a Fire OS 6 Dot already rooted via boot-root passes; everything else is
// refused before connect_android trusts the shell.

const { requireFireOS6 } = await lift(
  [liftFunctionDecl("requireFireOS6"), "export { requireFireOS6 };"]);

function refusal(opts) {
  try { requireFireOS6(opts); } catch (e) { return e; }
  return null;
}

check("Fire OS 6, rooted, passes",
  refusal({ release: '7.1.2', productName: 'biscuit_puffin', uid: '0' }) === null);

{
  const threw = refusal({ release: '5.1.1', productName: 'csm_biscuit', uid: '2000' });
  check("Android 5.x is refused as no longer supported",
    threw && /no longer supports/.test(threw.message), threw && threw.message);
  check("the Android 5.x refusal points at moving to Fire OS 6",
    threw && threw.message.includes('Fire OS 6') && threw.message.includes('docs/rooting.md'),
    threw && threw.message);
}

{
  const threw = refusal({ release: '7.1.2', productName: 'biscuit_puffin', uid: '2000' });
  check("Fire OS 6 without root is refused",
    threw && /boot-root/i.test(threw.message), threw && threw.message);
  check("the refusal points at the XDA thread",
    threw && threw.message.includes('xdaforums.com'), threw && threw.message);
}

{
  const threw = refusal({ release: '7.1.2', productName: 'something_else', uid: '0' });
  check("a 7.1.x device that isn't biscuit_puffin is refused",
    threw && /biscuit_puffin/.test(threw.message), threw && threw.message);
}

{
  const threw = refusal({ release: '6.0.1', productName: 'csm_biscuit', uid: '0' });
  check("anything else is refused",
    threw && /Wrong device/.test(threw.message), threw && threw.message);
}

// ─── Step list ──────────────────────────────────────────────────────────────

const { _WIZARD_STEPS } = await lift(
  [liftConst("_WIZARD_STEPS"), "export { _WIZARD_STEPS };"]);

check('connect_android is the first step', _WIZARD_STEPS[0]?.id === 'connect_android');

// The other system slot is written only after a boot from the first one has
// reached a controller: a bad install must leave a stock fallback slot.
const at = id => _WIZARD_STEPS.findIndex(s => s.id === id);
for (const [before, after] of [['install_boot_hook', 'reboot'], ['install_em', 'reboot'],
                               ['wifi', 'reboot'], ['reboot', 'verify_service'],
                               ['verify_service', 'confirm_link'],
                               ['confirm_link', 'mirror_boot_hook']]) {
  check(`${before} runs before ${after}`, at(before) >= 0 && at(before) < at(after),
    _WIZARD_STEPS.map(s => s.id).join(','));
}
check('mirror_boot_hook is the last step', at('mirror_boot_hook') === _WIZARD_STEPS.length - 1);

// Every step needs a label a human reads.
_WIZARD_STEPS.forEach(s => check(`${s.id} has a label`, !!s.label));

// ─── Boot hook content + idempotency ───────────────────────────────────────

const { _ECHOMUSE_RC, _rcInstalled } = await lift(
  [liftConst("_ECHOMUSE_RC"), liftFunctionDecl("_rcInstalled"),
   "export { _ECHOMUSE_RC, _rcInstalled };"]);

check("the rc declares the echomuse service against start_server.sh",
  _ECHOMUSE_RC.includes('service echomuse /system/bin/sh /data/local/bin/start_server.sh'));
check("the rc's seclabel is u:r:adbd:s0, not u:r:su:s0 (init refuses that transition)",
  _ECHOMUSE_RC.includes('seclabel u:r:adbd:s0') && !_ECHOMUSE_RC.includes('u:r:su:s0'));
check("the rc starts the service on sys.boot_completed",
  _ECHOMUSE_RC.includes('on property:sys.boot_completed=1') && _ECHOMUSE_RC.includes('start echomuse'));

check("a device already carrying the exact rc is up to date", _rcInstalled(_ECHOMUSE_RC));
check("a device with no file (empty cat) is not up to date", !_rcInstalled(''));
check("a device with a stale rc is not up to date", !_rcInstalled('service echomuse /old/path\n'));
// `cat`'s output reaches here already trimmed by Client.shell() — a
// trailing-newline-only difference must not look like drift.
check("trailing-newline-only differences still count as installed",
  _rcInstalled(_ECHOMUSE_RC.trim()));

if (failures) {
  console.error(`\n${failures} check(s) failed.`);
  process.exit(1);
}
console.log("provision_wizard: all checks passed.");
