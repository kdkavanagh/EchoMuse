// Tests for _pmVerdict() in dashboard.jsx — what the wizard concludes from a
// `pm disable` / `pm hide` reply.
//
//     node controller/tests/pm_verdict.test.mjs
//
// Source extraction rather than import, for the reason wifi_scan.test.mjs
// gives: the dashboard compiles to a single classic script with no module
// boundary, so the alternative is a second copy that drifts.
//
// The distinction under test is between pm REFUSING a call and pm ANSWERING
// that the package is not installed. Counting only successes made those two
// identical, so a device whose image genuinely lacks the Alexa packages was
// told the package manager had rejected every call and to wait longer and
// retry, which can never help — and the wizard could never be completed (#91).

import { readFileSync } from "fs";
import { execFileSync } from "child_process";
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

const { _pmVerdict, _pmNotReady } = await import(
  "data:text/javascript;base64," + Buffer.from(
    liftConst("PM_VERDICT") + "\n" + liftArrow("_pmVerdict") + "\n" + liftArrow("_pmNotReady")
    + "\nexport { _pmVerdict, _pmNotReady };"
  ).toString("base64"));

let failures = 0;
function check(name, cond, detail) {
  if (cond) return;
  failures++;
  console.error(`FAIL: ${name}${detail ? `\n      ${detail}` : ""}`);
}

// Real output, from a successful run on 2026-08-08.
check("a disabled package is a success",
  _pmVerdict("Package amazon.speech.sim new state: disabled") === "disabled");
check("a hidden package is a success",
  _pmVerdict("Package com.amazon.whad new hidden state: true") === "disabled");

// Real output from #91. pm answered; the package is not installed.
const UNKNOWN =
  "Error: java.lang.IllegalArgumentException: Unknown package: com.amazon.echo.csm.oobe";
check("an unknown package is absent, not a rejection",
  _pmVerdict(UNKNOWN) === "absent", _pmVerdict(UNKNOWN));

// The two shapes of pm not being ready. Both must count as rejections, since
// waiting and retrying IS the right advice for them.
for (const out of [
  "Error: Could not access the Package Manager. Is the system running?",
  "java.lang.NullPointerException: Attempt to invoke interface method "
  + "'int java.util.ArrayList.size()' on a null object reference",
]) {
  check(`not-ready is a rejection: ${out.slice(0, 40)}`,
        _pmVerdict(out) === "rejected", _pmVerdict(out));
  check(`_pmNotReady still matches: ${out.slice(0, 40)}`, _pmNotReady(out));
}

// Anything unrecognised is treated as the BAD case on purpose: the cost of
// being wrong is continuing to WiFi with the Alexa stack live.
for (const out of ["", "Killed", "segfault", "Permission denied"]) {
  check(`unrecognised output is a rejection: ${out || "(empty)"}`,
        _pmVerdict(out) === "rejected", _pmVerdict(out));
}

// An absent package must not be mistaken for not-ready, or the retry path
// fires eleven times against a device that answered correctly every time.
check("absent is not confused with not-ready", !_pmNotReady(UNKNOWN));

if (failures) {
  console.error(`\n${failures} check(s) failed.`);
  process.exit(1);
}
console.log("pm_verdict: all checks passed.");

// ─── Step mode / banner classification ────────────────────────────────────────
//
// Pulling the cable powers the Dot off, and `reboot recovery` is a one-shot,
// so a replug is a cold boot into Android whatever phase the wizard is in.
// Reconnecting during the TWRP phase hands back an Android device that looks
// healthy. In Android `/dev/block/other-boot` points at boot_b, which holds
// amonet's unlock payload, so a retried Patch Boot Image there would write
// over the unlock.

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

const { _bannerMode, _WIZARD_STEPS, STEP } = await import(
  "data:text/javascript;base64," + Buffer.from(
    [liftConst("BOOT_MODE"), liftFunctionDecl("_bannerMode"),
     liftConst("_WIZARD_STEPS"), liftConst("STEP"),
     "export { _bannerMode, _WIZARD_STEPS, STEP };"].join("\n")
  ).toString("base64"));

// Real banners, from provisioning transcripts.
check("TWRP is recognised", _bannerMode("omni_biscuit") === "twrp",
      _bannerMode("omni_biscuit"));
check("Android is recognised", _bannerMode("csm_biscuit") === "android",
      _bannerMode("csm_biscuit"));

// The ordering trap: "omni_biscuit" contains "biscuit", so an Android-first
// test calls every TWRP device Android — which is the exact direction that
// lets a TWRP step run against Android.
check("TWRP is not mistaken for Android", _bannerMode("omni_biscuit") !== "android");

check("an unknown banner is not guessed at",
      _bannerMode("something_else") === "unknown", _bannerMode("something_else"));
check("an empty banner is not guessed at", _bannerMode("") === "unknown");
check("a missing banner is not guessed at", _bannerMode(undefined) === "unknown");

// The boot-image step must be a TWRP step. If this ever flips, the wizard
// would invite a write to boot_b.
const modeOf = id => _WIZARD_STEPS[STEP[id]].mode;
check("patch boot image is a TWRP step", modeOf("PATCH_BOOT") === "twrp", modeOf("PATCH_BOOT"));
check("connect device is an Android step", modeOf("CONNECT_ANDROID") === "android");
check("verify root is an Android step", modeOf("VERIFY_ROOT") === "android");

// Every step must have a mode, or the check silently does nothing for it.
_WIZARD_STEPS.forEach((s, i) => {
  check(`step ${i} (${s.id}) has a mode`, s.mode === "twrp" || s.mode === "android",
        String(s.mode));
});

// ─── Disconnect-shaped errors ─────────────────────────────────────────────────
//
// The in-flight transfer can throw BEFORE the WebUSB disconnect event lands —
// the two race, and on a real run the throw won. That put "Failed to execute
// 'transferOut' on 'USBDevice'" in the transcript as though it were a
// provisioning failure, and then sent the diagnostics probes at a device that
// was no longer plugged in.

const { _isDisconnectError } = await import(
  "data:text/javascript;base64," + Buffer.from(
    liftArrow("_isDisconnectError") + "\nexport { _isDisconnectError };"
  ).toString("base64"));

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
  "Hash mismatch — expected 18e46b16b25e… (Magisk-v17.3.zip)",
  "Did not associate to the network within 20s",
  "The package manager rejected 11 of 11 calls and disabled none.",
  "Install verification failed: /data/local/bin/server points to \"\"",
]) {
  check(`a real failure is not treated as a disconnect: ${msg.slice(0, 34)}`,
        !_isDisconnectError(new Error(msg)), msg);
}

check("a missing message does not throw", _isDisconnectError(undefined) === false);
check("an error with no message does not throw", _isDisconnectError({}) === false);

// ─── Platform classification (connect_android) ────────────────────────────────
//
// Fire OS 5, Fire OS 6 already rooted via boot-root, Fire OS 6 not yet
// rooted, and anything else — the four outcomes connect_android has to tell
// apart before it does anything irreversible (reboot to recovery on Fire OS
// 5, or trust an unrooted shell on Fire OS 6).

const { classifyPlatform, PLATFORM } = await import(
  "data:text/javascript;base64," + Buffer.from(
    [liftConst("PLATFORM"), liftFunctionDecl("classifyPlatform"),
     "export { classifyPlatform, PLATFORM };"].join("\n")
  ).toString("base64"));

check("Fire OS 5 is classified",
  classifyPlatform({ release: '5.1', productName: 'csm_biscuit', uid: null }).platform === PLATFORM.FIREOS5);

check("Fire OS 6, rooted, is classified",
  classifyPlatform({ release: '7.1.2', productName: 'biscuit_puffin', uid: '0' }).platform === PLATFORM.FIREOS6);

{
  let threw = null;
  try { classifyPlatform({ release: '7.1.2', productName: 'biscuit_puffin', uid: '2000' }); }
  catch (e) { threw = e; }
  check("Fire OS 6 without root is refused, not silently treated as Fire OS 5",
    threw && /boot-root/i.test(threw.message), threw && threw.message);
  check("the refusal points at the XDA thread",
    threw && threw.message.includes('xdaforums.com'), threw && threw.message);
}

{
  let threw = null;
  try { classifyPlatform({ release: '7.1.2', productName: 'something_else', uid: '0' }); }
  catch (e) { threw = e; }
  check("a 7.1.x device that isn't biscuit_puffin is refused",
    threw && /biscuit_puffin/.test(threw.message), threw && threw.message);
}

{
  let threw = null;
  try { classifyPlatform({ release: '6.0.1', productName: 'csm_biscuit', uid: null }); }
  catch (e) { threw = e; }
  check("anything else is refused",
    threw && /Wrong device/.test(threw.message), threw && threw.message);
}

// ─── rootShell ──────────────────────────────────────────────────────────────
//
// Fire OS 5 must send exactly the text it always has (`su -c <cmd>`). On
// Fire OS 6 the quoted command must still run with its own word splitting:
// the device's `sh -c` strips the outer quotes, so the result is run here
// through a host shell the same way.

const { rootShell } = await import(
  "data:text/javascript;base64," + Buffer.from(
    [liftConst("PLATFORM"), liftFunctionDecl("rootShell"), "export { rootShell };"].join("\n")
  ).toString("base64"));

check("Fire OS 5 wraps with su -c, byte-identical to before",
  rootShell('fireos5', "'pm disable foo' 2>&1") === "su -c 'pm disable foo' 2>&1");
check("Fire OS 6 runs a quoted multi-word command as words, not one name",
  execFileSync("sh", ["-c", rootShell('fireos6', "'echo a b; echo c'")]).toString() === "a b\nc\n");

// ─── Fire OS 6 step list ────────────────────────────────────────────────────

const { _WIZARD_STEPS_FIREOS6, _FIREOS6_NOT_APPLICABLE } = await import(
  "data:text/javascript;base64," + Buffer.from(
    [liftConst("BOOT_MODE"), liftConst("_WIZARD_STEPS_FIREOS6"), liftConst("_FIREOS6_NOT_APPLICABLE"),
     "export { _WIZARD_STEPS_FIREOS6, _FIREOS6_NOT_APPLICABLE };"].join("\n")
  ).toString("base64"));

// The other system slot is written only after a boot from the first one has
// reached a controller: a bad install must leave a stock fallback slot.
const at = id => _WIZARD_STEPS_FIREOS6.findIndex(s => s.id === id);
for (const [before, after] of [['install_boot_hook', 'reboot'], ['install_em', 'reboot'],
                               ['wifi', 'reboot'], ['reboot', 'verify_service'],
                               ['verify_service', 'confirm_link'],
                               ['confirm_link', 'mirror_boot_hook']]) {
  check(`Fire OS 6 runs ${before} before ${after}`, at(before) >= 0 && at(before) < at(after),
    _WIZARD_STEPS_FIREOS6.map(s => s.id).join(','));
}
check('mirror_boot_hook is the last Fire OS 6 step',
  at('mirror_boot_hook') === _WIZARD_STEPS_FIREOS6.length - 1);

// Every Fire OS 5-only step must be flagged not-applicable so it is shown,
// not silently missing, in a Fire OS 6 run — and absent from the Fire OS 6
// list itself, so runStep can never dispatch to it.
for (const id of ['connect_twrp', 'patch_boot', 'install_magisk', 'preseed_db',
                   'verify_root', 'disable_alexa', 'debloat']) {
  check(`${id} is marked not applicable on Fire OS 6`, _FIREOS6_NOT_APPLICABLE.has(id));
  check(`${id} is absent from the Fire OS 6 step list`,
    !_WIZARD_STEPS_FIREOS6.some(s => s.id === id));
}

// Nothing the Fire OS 6 list runs is itself marked not-applicable — the two
// sets must be disjoint, or a step could be shown both live and greyed out.
for (const s of _WIZARD_STEPS_FIREOS6) {
  check(`${s.id} is not also listed as not applicable`, !_FIREOS6_NOT_APPLICABLE.has(s.id));
}

// Every step from both platforms needs a label a human reads, same
// discipline the Fire OS 5 "every step must have a mode" check above uses.
_WIZARD_STEPS_FIREOS6.forEach(s => check(`${s.id} has a label`, !!s.label));

// ─── Boot hook content + idempotency ───────────────────────────────────────

const { _ECHOMUSE_RC, _rcInstalled } = await import(
  "data:text/javascript;base64," + Buffer.from(
    [liftConst("_ECHOMUSE_RC"), liftFunctionDecl("_rcInstalled"),
     "export { _ECHOMUSE_RC, _rcInstalled };"].join("\n")
  ).toString("base64"));

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
console.log("pm_verdict (fireos6 additions): all checks passed.");
