// Tests for the provisioning wizard's pure pieces in dashboard.jsx: the
// disconnect-error filter, the platform check connect_android runs before it
// trusts the shell, the step order, and the boot hook it writes.
//
//     node controller/tests/provision_wizard.test.mjs
//
// Source extraction rather than import, for the reason wifi_scan.test.mjs
// gives: the dashboard compiles to a single classic script with no module
// boundary, so the alternative is a second copy that drifts.

import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from "fs";
import { spawnSync } from "child_process";
import { tmpdir } from "os";
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
// bracket depth 0 (the same scan as liftArrow). Column-aligned `NAME  =`
// declarations count too. A template literal is skipped whole: the boot
// hook payloads are shell text, full of brackets and `;;`.
function liftConst(name) {
  const m = new RegExp(`const ${name} +=`).exec(src);
  const start = m ? m.index : -1;
  if (start < 0) throw new Error(`dashboard.jsx no longer defines ${name}`);
  let depth = 0;
  for (let i = start; i < src.length; i++) {
    const ch = src[i];
    if (ch === "`") {
      for (i++; i < src.length && src[i] !== "`"; i++) if (src[i] === "\\") i++;
    }
    else if (ch === "{" || ch === "(" || ch === "[") depth++;
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
// A rooted Fire OS 6 Dot, a rooted Echo Show 8 or an Echo Show 8 on our
// LineageOS build with root adb passes, and says which it is; everything
// else is refused before connect_android trusts the shell.

const WIZARD_PLATFORM_SRC = [liftConst("WIZARD_PLATFORM"), liftConst("WIZARD_PLATFORM_NAME"),
                             liftFunctionDecl("_isShow"), liftConst("_LINEAGE_LIBASP_SHA1"),
                             liftFunctionDecl("_isOurLibasp")];
const { classifyDevice, WIZARD_PLATFORM, _isOurLibasp } = await lift(
  [...WIZARD_PLATFORM_SRC, liftFunctionDecl("classifyDevice"),
   "export { classifyDevice, WIZARD_PLATFORM, _isOurLibasp };"]);

// The wizard provisions exactly the images the controller admits.
{
  const py = readFileSync(join(HERE, "..", "em_device_link.py"), "utf8");
  const block = py.slice(py.indexOf("class DevicePlatform("), py.indexOf("class RejectReason("));
  const admitted = [...block.matchAll(/^\s+[A-Z0-9_]+ = "([a-z0-9_]+)"/gm)].map(m => m[1]).sort();
  check("WIZARD_PLATFORM is em_device_link.DevicePlatform",
    admitted.length > 0 && Object.values(WIZARD_PLATFORM).sort().join() === admitted.join(),
    `${Object.values(WIZARD_PLATFORM).sort()} vs ${admitted}`);
}

function refusal(opts) {
  try { classifyDevice(opts); } catch (e) { return e; }
  return null;
}

// The Dot, as connect_android reads it (ro.product.device is "biscuit" there).
const DOT = { release: '7.1.2', productName: 'biscuit_puffin', productDevice: 'biscuit', uid: '0' };
// The Show as measured (docs/echo-show-crown.md §1): product crown.
const SHOW = { release: '7.1.2', productName: 'crown', productDevice: 'crown', uid: '0' };

check("Fire OS 6 Dot, rooted, passes as the Dot",
  refusal(DOT) === null && classifyDevice(DOT) === 'fireos6');
check("the Dot passes without ro.product.device, as before the Show",
  classifyDevice({ ...DOT, productDevice: undefined }) === WIZARD_PLATFORM.DOT);
check("Echo Show 8, rooted, passes as the Show",
  refusal(SHOW) === null && classifyDevice(SHOW) === 'fireos6_crown');
check("the Show is recognised by ro.product.device alone",
  classifyDevice({ ...SHOW, productName: 'something' }) === WIZARD_PLATFORM.CROWN);

{
  const threw = refusal({ release: '5.1.1', productName: 'csm_biscuit', uid: '2000' });
  check("Android 5.x is refused as no longer supported",
    threw && /no longer supports/.test(threw.message), threw && threw.message);
  check("the Android 5.x refusal points at moving to Fire OS 6",
    threw && threw.message.includes('Fire OS 6') && threw.message.includes('docs/rooting.md'),
    threw && threw.message);
}

{
  const threw = refusal({ ...DOT, uid: '2000' });
  check("Fire OS 6 without root is refused",
    threw && /boot-root/i.test(threw.message), threw && threw.message);
  check("the refusal points at the XDA thread",
    threw && threw.message.includes('xdaforums.com'), threw && threw.message);
}

{
  const threw = refusal({ ...SHOW, uid: '2000' });
  check("an Echo Show without root is refused",
    threw && /Echo Show 8/.test(threw.message) && /boot-root/.test(threw.message),
    threw && threw.message);
  check("the Show's refusal points at the crown thread, not the Dot's",
    threw && threw.message.includes('echo-show-8-1st-gen-2019-crown'), threw && threw.message);
}

{
  const threw = refusal({ release: '7.1.2', productName: 'something_else', uid: '0' });
  check("a 7.1.x device that is neither biscuit_puffin nor crown is refused",
    threw && /biscuit_puffin/.test(threw.message) && /crown/.test(threw.message),
    threw && threw.message);
}

{
  const threw = refusal({ release: '6.0.1', productName: 'csm_biscuit', uid: '0' });
  check("anything else is refused",
    threw && /Wrong device/.test(threw.message), threw && threw.message);
  const show9 = refusal({ ...SHOW, release: '9' });
  check("a crown on another Android, not LineageOS, is refused",
    show9 && /Wrong device/.test(show9.message), show9 && show9.message);
  const show11 = refusal({ ...SHOW, release: '11' });
  check("a crown on Android 11 without ro.lineage.version is refused",
    show11 && /Wrong device/.test(show11.message), show11 && show11.message);
}

// The Show on our LineageOS 18.1 build (docs/echo-show-crown.md §8.5): no
// Fire OS build name, ro.lineage.version set, and sha1sum's line for the
// NS6571 libasp.so (docs/echo-show-lineage-plan.md §1 "ROM").
const OUR_LIBASP = 'e3816eb3efd4654c708d3891a5f907b79f3ebce3  /system/lib/libasp.so';
const LINEAGE = { release: '11', productName: 'lineage_crown', productDevice: 'crown', uid: '0',
                  lineageVersion: '18.1-20261017-UNOFFICIAL-crown', libaspSha1: OUR_LIBASP };

check("our LineageOS build, rooted, passes as the LineageOS Show",
  refusal(LINEAGE) === null && classifyDevice(LINEAGE) === 'lineage18_crown');
check("the LineageOS Show is recognised by ro.product.device alone",
  classifyDevice({ ...LINEAGE, productName: 'something' }) === WIZARD_PLATFORM.CROWN_LINEAGE);
check("ro.lineage.version decides between the two Show images",
  classifyDevice({ ...SHOW, lineageVersion: '' }) === WIZARD_PLATFORM.CROWN);
check("our libasp.so is recognised from sha1sum's line", _isOurLibasp(OUR_LIBASP));
check("our libasp.so is recognised from the bare hash, in either case",
  _isOurLibasp('E3816EB3EFD4654C708D3891A5F907B79F3EBCE3'));
for (const out of ['e3816eb3', 'e3816eb3  /system/lib/libasp.so',
                   'sha1sum: /system/lib/libasp.so: No such file or directory', '', undefined]) {
  check(`not our libasp.so: ${out === undefined ? '(undefined)' : out || '(empty)'}`, !_isOurLibasp(out));
}

{
  // Upstream v0.6 ships another libasp.so, with no metadata encoder.
  const upstream = { ...LINEAGE, libaspSha1: '0123456789abcdef0123456789abcdef01234567  /system/lib/libasp.so' };
  const threw = refusal(upstream);
  check("an upstream LineageOS build is refused",
    threw && threw.message.includes('0123456789abcdef'), threw && threw.message);
  check("the upstream refusal names our fork's branches",
    threw && threw.message.includes('crown-ns6571-asp')
    && threw.message.includes('kdkavanagh/android_device_amazon_crown')
    && threw.message.includes('kdkavanagh/android_vendor_amazon_crown'), threw && threw.message);
  const missing = refusal({ ...LINEAGE, libaspSha1: 'sha1sum: /system/lib/libasp.so: No such file or directory' });
  check("a LineageOS build without libasp.so is refused the same way",
    missing && missing.message.includes('crown-ns6571-asp'), missing && missing.message);
  const both = refusal({ ...upstream, uid: '2000' });
  check("the ROM is named before root: root on the wrong build would not help",
    both && both.message.includes('crown-ns6571-asp') && !both.message.includes('Rooted debugging'),
    both && both.message);
}

{
  // LineageOS 18.1's adbd (system/core adb/daemon/restart_service.cpp) while
  // Rooted debugging is off.
  const reply = 'ADB Root access is disabled by system setting - enable in Settings -> System -> Developer options';
  const threw = refusal({ ...LINEAGE, uid: '2000', rootReply: reply });
  check("a LineageOS Show without root adb is refused",
    threw && threw.message.includes('id -u = "2000"'), threw && threw.message);
  check("the refusal names the Developer options to switch on",
    threw && threw.message.includes('Developer options') && threw.message.includes('"USB debugging"')
    && threw.message.includes('"Rooted debugging"'), threw && threw.message);
  check("the refusal quotes adbd's answer to adb root",
    threw && threw.message.includes(reply), threw && threw.message);
  check("the LineageOS refusal is not the Fire OS one",
    threw && !/boot-root/.test(threw.message), threw && threw.message);
}

{
  const threw = refusal({ ...LINEAGE, release: '12' });
  check("LineageOS on another Android than 11 is refused",
    threw && threw.message.includes('LineageOS 18.1 (Android 11)'), threw && threw.message);
}

// ─── Tested-build pin ───────────────────────────────────────────────────────
//
// A warning, never a refusal, per platform: the Dot's NS6569/6009 and the
// Show's NS65741/8146 (ro.build.version.name or ro.build.display.id).

const { untestedBuild } = await lift(
  [...WIZARD_PLATFORM_SRC, liftConst("_TESTED_FIREOS6_BUILD"), liftConst("_TESTED_FIREOS6_NAME"),
   liftConst("_TESTED_CROWN_BUILD"), liftConst("_TESTED_CROWN_NAME"),
   liftFunctionDecl("untestedBuild"), "export { untestedBuild };"]);

// LineageOS has no Fire OS build id; its pin is the libasp.so hash.
check("the LineageOS Show is never flagged against a Fire OS pin",
  untestedBuild('lineage18_crown', ['', 'lineage_crown-userdebug 11 RQ3A.211001.001 eng.kyle']) === null);

check("the Dot on NS6569/6009 is not flagged",
  untestedBuild('fireos6', ['Fire OS 6.5.6.9 (NS6569/6009)', '']) === null);
check("the Show on NS65741/8146 is not flagged",
  untestedBuild('fireos6_crown', ['Fire OS 6574.1 (NS65741/8146)', 'NS65741/8146']) === null);
check("the Show's pin is found in the display id alone",
  untestedBuild('fireos6_crown', ['', 'NS65741/8146']) === null);
{
  const warn = untestedBuild('fireos6_crown', ['Fire OS 6.5.7.3 (NS6573/9999)', '']);
  check("a Show on another build is flagged against the Show's pin",
    warn && warn.includes('NS65741/8146') && warn.includes('Echo Show 8'), warn);
  check("the Dot's build is not the Show's pin",
    untestedBuild('fireos6_crown', ['Fire OS 6.5.6.9 (NS6569/6009)']) !== null);
}
check("a Dot on the Show's build is flagged against the Dot's pin",
  (untestedBuild('fireos6', ['Fire OS 6574.1 (NS65741/8146)']) || '').includes('NS6569/6009'));
check("a device that names no build is not flagged", untestedBuild('fireos6', ['', '']) === null);

// ─── Show prerequisites: Wi-Fi joined on the device, a kiosk HOME ──────────

const { _wlanAddress } = await lift(
  [liftFunctionDecl("_wlanAddress"), "export { _wlanAddress };"]);

const IP_UP = `5: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP group default qlen 1000
    inet 192.168.3.159/24 brd 192.168.3.255 scope global wlan0
       valid_lft forever preferred_lft forever`;
check("a joined wlan0 yields its address", _wlanAddress(IP_UP) === '192.168.3.159');
check("wlan0 without an address is not on Wi-Fi",
  _wlanAddress('5: wlan0: <BROADCAST,MULTICAST> mtu 1500 qdisc mq state DOWN') === null);
check("a missing wlan0 is not on Wi-Fi",
  _wlanAddress('Device "wlan0" does not exist.') === null);

const { _homeActivities, _kioskHomes } = await lift(
  [liftFunctionDecl("_homeActivities"), liftFunctionDecl("_kioskHomes"),
   "export { _homeActivities, _kioskHomes };"]);

// Verbatim from the Show in OOBE (G6G16D12143203LD, 2026-10-17), stock.
const HOME_STOCK = `3 activities found:
  Activity #0:
    priority=101 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true
    com.amazon.ds2.oobe.efd/com.amazon.ds2.oobe.MainActivity
  Activity #1:
    priority=50 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true
    com.amazon.paladin/.app.MMLauncherActivity
  Activity #2:
    priority=-1000 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true
    com.android.settings/.FallbackHome`;
const homes = _homeActivities(HOME_STOCK);
check("every HOME activity is read", homes.length === 3, JSON.stringify(homes));
check("the short .Class form is kept as printed",
  homes[1]?.pkg === 'com.amazon.paladin' && homes[1]?.cls === '.app.MMLauncherActivity');
check("stock offers no kiosk: Amazon's and FallbackHome do not count",
  _kioskHomes(homes).length === 0);

const HOME_KIOSK = HOME_STOCK.replace('3 activities', '4 activities') + `
  Activity #3:
    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true
    de.ozerov.fully/.LauncherReplacement`;
{
  const k = _kioskHomes(_homeActivities(HOME_KIOSK));
  check("an installed kiosk is the one candidate",
    k.length === 1 && k[0].pkg === 'de.ozerov.fully', JSON.stringify(k));
}
check("resolve-activity's winner is read from its two lines",
  _homeActivities('priority=101 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\n'
                + 'com.amazon.ds2.oobe.efd/com.amazon.ds2.oobe.MainActivity').pop()?.pkg
    === 'com.amazon.ds2.oobe.efd');
check("no output, no activities", _homeActivities('').length === 0
  && _homeActivities('No activity found').length === 0);

// Fully Kiosk 1.61.4 ships `.LauncherReplacement` disabled (measured on the
// Show), so after a plain install it is not a HOME candidate at all.
const { _kioskToEnable, _chooseKiosk, _sameComponent, _imePackages, _imeKeep } = await lift(
  [liftFunctionDecl("_homeActivities"), liftFunctionDecl("_kioskHomes"),
   liftConst("_fullClass"), liftConst("_sameComponent"), liftConst("_KNOWN_KIOSK_HOMES"),
   liftFunctionDecl("_kioskToEnable"), liftFunctionDecl("_chooseKiosk"),
   liftFunctionDecl("_imePackages"), liftFunctionDecl("_imeKeep"),
   "export { _kioskToEnable, _chooseKiosk, _sameComponent, _imePackages, _imeKeep };"]);

{
  const fullyInstalled = new Set(['com.amazon.paladin', 'de.ozerov.fully']);
  const k = _kioskToEnable(homes, fullyInstalled);
  check("Fully installed with its home switched off is switched on",
    k && k.pkg === 'de.ozerov.fully' && k.cls === '.LauncherReplacement', JSON.stringify(k));
  check("nothing to switch on without a known kiosk installed",
    _kioskToEnable(homes, new Set(['com.amazon.paladin'])) === null);
  check("nothing to switch on once a kiosk home is offered",
    _kioskToEnable(_homeActivities(HOME_KIOSK), fullyInstalled) === null);
}

check("the short and full forms of a component are the same component",
  _sameComponent({ pkg: 'de.ozerov.fully', cls: '.LauncherReplacement' },
                 { pkg: 'de.ozerov.fully', cls: 'de.ozerov.fully.LauncherReplacement' }));
check("another package's activity is not the kiosk",
  !_sameComponent({ pkg: 'com.amazon.ds2.oobe.efd', cls: 'com.amazon.ds2.oobe.MainActivity' },
                  { pkg: 'de.ozerov.fully', cls: '.LauncherReplacement' }));
check("nothing resolved is not the kiosk",
  !_sameComponent(undefined, { pkg: 'de.ozerov.fully', cls: '.LauncherReplacement' }));
{
  const other = { pkg: 'com.example.launcher', cls: '.Home' };
  const fully = { pkg: 'de.ozerov.fully', cls: 'de.ozerov.fully.LauncherReplacement' };
  check("a known kiosk is chosen over another home app", _chooseKiosk([other, fully]) === fully);
  check("the only candidate is chosen", _chooseKiosk([other]) === other);
  check("no candidate, no choice", _chooseKiosk([]) === null);
}

// `ime list -s` on the stock Show: Amazon's dictation keyboard is the only IME.
const IME_STOCK = 'com.amazon.bluestone.keyboard/.DictationIME';
check("enabled keyboards are read by package",
  _imePackages(IME_STOCK).has('com.amazon.bluestone.keyboard'));
check("the only keyboard is never hidden",
  _imeKeep(['amazon.speech.sim', 'com.amazon.bluestone.keyboard'], _imePackages(IME_STOCK))
    .join() === 'com.amazon.bluestone.keyboard');
check("Amazon's keyboard may go once another is enabled",
  _imeKeep(['com.amazon.bluestone.keyboard'],
           _imePackages(IME_STOCK + '\ncom.example.kbd/.Ime')).length === 0);
check("a list without keyboards keeps nothing",
  _imeKeep(['amazon.speech.sim'], _imePackages(IME_STOCK)).length === 0);

// ─── Never hidden, microphones denied, Settings repaired (neutralize_alexa) ─
//
// Measured on the Show (2026-10-17): hiding amazon.speech.sim crash-loops
// KnightSystemUI, hiding kindle.otter.oobe takes every Settings page down.

const { _keepVisible, _CROWN_KEEP_VISIBLE, _micDenied } = await lift(
  [liftConst("_CROWN_KEEP_VISIBLE"), liftFunctionDecl("_keepVisible"), liftConst("_micDenied"),
   "export { _keepVisible, _CROWN_KEEP_VISIBLE, _micDenied };"]);

check("Alexa's wake engine is never hidden",
  _keepVisible(['com.amazon.paladin', 'amazon.speech.sim']).join() === 'amazon.speech.sim');
check("kindle.otter.oobe is never hidden",
  _keepVisible(['com.amazon.kindle.otter.oobe']).join() === 'com.amazon.kindle.otter.oobe');
check("an ordinary Tier 1 package is not protected",
  _keepVisible(['com.amazon.paladin', 'com.amazon.ds2.oobe.efd']).length === 0);
check("every protected package carries the reason the wizard logs",
  Object.values(_CROWN_KEEP_VISIBLE).every(r => typeof r === 'string' && r.length > 20));

check("a denied microphone is recognised",
  _micDenied('RECORD_AUDIO: ignore; time=+2m3s ago'));
check("a denied microphone without a time is recognised", _micDenied('RECORD_AUDIO: ignore'));
for (const out of ['RECORD_AUDIO: allow; time=+1s ago', 'RECORD_AUDIO: default', 'No operations.',
                   'Error: Unknown package: amazon.speech.sim', '', undefined]) {
  check(`not denied: ${out === undefined ? '(undefined)' : out || '(empty)'}`, !_micDenied(out));
}

const { _hasSettingsBundle, _SETTINGS_BUNDLE_BROADCAST, _CROWN_SETTINGS_PAGES } = await lift(
  [liftConst("_hasSettingsBundle"), liftConst("_SETTINGS_BUNDLE_BROADCAST"),
   liftConst("_CROWN_SETTINGS_PAGES"),
   "export { _hasSettingsBundle, _SETTINGS_BUNDLE_BROADCAST, _CROWN_SETTINGS_PAGES };"]);

check("the config bundle is found once Settings wrote it",
  _hasSettingsBundle('<map>\n    <string name="bundle_location">/data/user/0/com.android.settings/files/bundle_a</string>\n</map>'));
check("a store without bundle_location has no bundle",
  !_hasSettingsBundle('<?xml version=\'1.0\'?>\n<map />') && !_hasSettingsBundle(''));

// The receiver this broadcast reaches also handles the account-removed
// action, which FACTORY-RESETS a provisioned Show. The wizard sends exactly
// one broadcast: this action, to this explicit component.
check("the Settings broadcast is the measured action to the explicit receiver",
  _SETTINGS_BUNDLE_BROADCAST === 'am broadcast -a com.amazon.dcp.sso.broadcast.CORPFMHasChanged '
    + '-n com.android.settings/com.amazon.settings.AccountPropertyChangeReceiver');
check("nothing but a comment in dashboard.jsx names the account-removed action",
  src.split('\n').filter(l => l.includes('account.removed')).every(l => l.trim().startsWith('//')));
check("outside comments, dashboard.jsx names no Amazon sso action but CORPFMHasChanged",
  src.split('\n').filter(l => l.includes('com.amazon.dcp.sso') && !l.trim().startsWith('//'))
    .every(l => (l.match(/com\.amazon\.dcp\.sso\.[\w.]+/g) || [])
      .every(a => a === 'com.amazon.dcp.sso.broadcast.CORPFMHasChanged')));

check("the Show's Settings pages are explicit components",
  _CROWN_SETTINGS_PAGES['Wi-Fi'] === 'com.android.settings/.Settings$WifiSettingsActivity'
  && _CROWN_SETTINGS_PAGES.Bluetooth === 'com.android.settings/.Settings$BluetoothSettingsActivity');

// ─── pm replies and package lists (neutralize_alexa) ────────────────────────

const { _pmVerdict, _listedPackages } = await lift(
  [liftConst("PM_VERDICT"), liftArrow("_pmVerdict"), liftFunctionDecl("_listedPackages"),
   "export { _pmVerdict, _listedPackages };"]);

check("a hidden package is done", _pmVerdict("Package com.amazon.whad new hidden state: true") === 'done');
check("an unhidden package is done", _pmVerdict("Package com.amazon.whad new hidden state: false") === 'done');
check("an unknown package is absent, not a rejection",
  _pmVerdict("Error: java.lang.IllegalArgumentException: Unknown package: com.amazon.x") === 'absent');
for (const out of ["", "Error: Could not access the Package Manager. Is the system running?",
                   "Killed", "java.lang.SecurityException: Permission Denial"]) {
  check(`unrecognised pm output is a rejection: ${out || "(empty)"}`, _pmVerdict(out) === 'rejected');
}
{
  const pkgs = _listedPackages('package:com.amazon.paladin\npackage:de.ozerov.fully\r\n\nnoise');
  check("pm list packages is read line by line",
    pkgs.size === 2 && pkgs.has('com.amazon.paladin') && pkgs.has('de.ozerov.fully'));
}

// ─── Step list ──────────────────────────────────────────────────────────────

const { _WIZARD_STEPS, _stepsFor, _nextStep, _lastStep, _stepApplies, _stepDesc, _serverLog } = await lift(
  [...WIZARD_PLATFORM_SRC, liftConst("_ALL_PLATFORMS"), liftConst("_WIZARD_STEPS"),
   liftFunctionDecl("_stepApplies"), liftFunctionDecl("_stepsFor"),
   liftFunctionDecl("_nextStep"), liftFunctionDecl("_lastStep"),
   liftFunctionDecl("_stepDesc"), liftFunctionDecl("_serverLog"),
   "export { _WIZARD_STEPS, _stepsFor, _nextStep, _lastStep, _stepApplies, _stepDesc, _serverLog };"]);

check('connect_android is the first step', _WIZARD_STEPS[0]?.id === 'connect_android');

// The Dot's steps are exactly what they were before the Show.
const DOT_STEPS = ['connect_android', 'install_boot_hook', 'install_em', 'wifi', 'reboot',
                   'reconnect', 'verify_service', 'confirm_link', 'mirror_boot_hook'];
check("the Dot's step list is unchanged",
  _stepsFor('fireos6').join() === DOT_STEPS.join(), _stepsFor('fireos6').join());

// The other system slot is written only after a boot from the first one has
// reached a controller: a bad install must leave a stock fallback slot.
const dotAt = id => _stepsFor('fireos6').indexOf(id);
check('mirror_boot_hook is the Dot\'s last step', dotAt('mirror_boot_hook') === DOT_STEPS.length - 1);

// docs/echo-show-port-plan.md Phase 4.
const SHOW_STEPS = ['connect_android', 'check_home', 'install_boot_hook', 'install_em',
                    'neutralize_alexa', 'reboot', 'reconnect', 'verify_service', 'confirm_link'];
check("the Show's step list is Phase 4's",
  _stepsFor('fireos6_crown').join() === SHOW_STEPS.join(), _stepsFor('fireos6_crown').join());
for (const id of ['wifi', 'mirror_boot_hook']) {
  check(`${id} is not applicable on the Show`, !_stepsFor('fireos6_crown').includes(id));
}
for (const id of ['check_home', 'neutralize_alexa']) {
  check(`${id} is not applicable on the Dot`, !_stepsFor('fireos6').includes(id));
}
// docs/echo-show-lineage-plan.md Phase L4: no Amazon apps, so no kiosk
// check and no neutralisation; Wi-Fi is Android's, one system partition.
// docs/echo-show-companion-plan.md Phase C5: the firmware's app is verified last.
const LINEAGE_STEPS = ['connect_android', 'install_boot_hook', 'install_em', 'reboot',
                       'reconnect', 'verify_service', 'confirm_link', 'verify_companion'];
check("the LineageOS Show's step list is Phase L4's plus verify_companion",
  _stepsFor('lineage18_crown').join() === LINEAGE_STEPS.join(), _stepsFor('lineage18_crown').join());
for (const id of ['check_home', 'neutralize_alexa', 'wifi', 'mirror_boot_hook']) {
  check(`${id} is not applicable on the LineageOS Show`, !_stepsFor('lineage18_crown').includes(id));
}
for (const platform of ['fireos6', 'fireos6_crown']) {
  check(`verify_companion is not applicable on ${platform}`, !_stepsFor(platform).includes('verify_companion'));
}
check("before connect_android every step shows", _stepsFor(null).length === _WIZARD_STEPS.length);

// Advancing skips the other device's steps, in both directions of the list.
const idx = id => _WIZARD_STEPS.findIndex(s => s.id === id);
check("the Dot goes from install_em to wifi",
  _nextStep(idx('install_em') + 1, 'fireos6') === idx('wifi'));
check("the Dot skips neutralize_alexa after wifi",
  _nextStep(idx('wifi') + 1, 'fireos6') === idx('reboot'));
check("the Show skips wifi after install_em",
  _nextStep(idx('install_em') + 1, 'fireos6_crown') === idx('neutralize_alexa'));
check("the Show goes from connect_android to check_home",
  _nextStep(idx('connect_android') + 1, 'fireos6_crown') === idx('check_home'));
check("the Dot skips check_home after connect_android",
  _nextStep(idx('connect_android') + 1, 'fireos6') === idx('install_boot_hook'));
check("the Show ends at confirm_link", _lastStep('fireos6_crown') === idx('confirm_link'));
check("the Dot ends at mirror_boot_hook", _lastStep('fireos6') === idx('mirror_boot_hook'));
check("nothing runs past the Show's last step",
  _nextStep(idx('confirm_link') + 1, 'fireos6_crown') === _WIZARD_STEPS.length);
check("the LineageOS Show goes from connect_android to install_boot_hook",
  _nextStep(idx('connect_android') + 1, 'lineage18_crown') === idx('install_boot_hook'));
check("the LineageOS Show goes from install_em to reboot",
  _nextStep(idx('install_em') + 1, 'lineage18_crown') === idx('reboot'));
check("the LineageOS Show ends at verify_companion", _lastStep('lineage18_crown') === idx('verify_companion'));
check("the LineageOS Show goes from confirm_link to verify_companion",
  _nextStep(idx('confirm_link') + 1, 'lineage18_crown') === idx('verify_companion'));
check("the Dot ends at mirror_boot_hook, past verify_companion",
  _nextStep(idx('mirror_boot_hook') + 1, 'fireos6') === _WIZARD_STEPS.length);

// Every step needs a label a human reads, and names where it runs.
_WIZARD_STEPS.forEach(s => check(`${s.id} has a label`, !!s.label));
_WIZARD_STEPS.forEach(s => check(`${s.id} names its platforms`,
  Array.isArray(s.on) && s.on.length > 0
  && s.on.every(p => Object.values(WIZARD_PLATFORM).includes(p)), JSON.stringify(s.on)));
check("_stepApplies treats an unknown platform as all", _stepApplies(_WIZARD_STEPS[1], null));

// Each image reads its own words where they differ, the Fire OS Show's where not.
{
  const step = id => _WIZARD_STEPS[idx(id)];
  check("the LineageOS boot hook step mentions its addon.d script",
    _stepDesc(step('install_boot_hook'), 'lineage18_crown').includes('addon.d'));
  check("the Fire OS Show's boot hook step does not",
    !_stepDesc(step('install_boot_hook'), 'fireos6_crown').includes('addon.d'));
  check("both Shows wait for audioserver",
    _stepDesc(step('verify_service'), 'lineage18_crown').includes('audioserver')
    && _stepDesc(step('verify_service'), 'fireos6_crown').includes('audioserver'));
  check("the Dot waits for the mixer", _stepDesc(step('verify_service'), 'fireos6').includes('mixer'));
}
check("both Shows log under /data/local/tmp/echomuse, the Dot to /tmp",
  _serverLog('lineage18_crown') === '/data/local/tmp/echomuse/server.log'
  && _serverLog('fireos6_crown') === '/data/local/tmp/echomuse/server.log'
  && _serverLog('fireos6') === '/tmp/server.log');

// ─── verify_companion's evidence ────────────────────────────────────────────

const { _companionReady, _companionOnDevice } = await lift(
  [liftConst("_COMPANION_PORT"), liftFunctionDecl("_companionReady"), liftFunctionDecl("_companionOnDevice"),
   "export { _companionReady, _companionOnDevice };"]);

check("stats.companion linked with the overlay allowed passes",
  _companionReady({ version: 'cmp-0123456789ab', linked: true, overlay: true, error: null }));
for (const companion of [null, undefined, { linked: true, overlay: null }, { linked: false, overlay: true },
                         { linked: null, overlay: null }]) {
  check(`stats.companion ${JSON.stringify(companion)} does not pass`, !_companionReady(companion));
}
{
  // /proc/net/tcp6 with the app (uid 10123) connected from ::ffff:127.0.0.1
  // to the firmware's 127.0.0.1:8771 (0x2243), whose own row is uid 0.
  const HEAD = '  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode';
  const APP  = '   0: 0000000000000000FFFF00000100007F:A1B2 0000000000000000FFFF00000100007F:2243 01 00000000:00000000 00:00000000 00000000 10123        0 4242 1';
  const FW   = '   1: 0100007F:2243 0100007F:A1B2 01 00000000:00000000 00:00000000 00000000     0        0 4243 1';
  const ALLOW = 'SYSTEM_ALERT_WINDOW: allow; time=+1m2s ago';
  const on = (tcp, uid, ops) => JSON.stringify(_companionOnDevice(tcp, uid, ops));
  check("the app's established connection and the allowed op read as linked with the overlay",
    on([HEAD, APP, FW].join('\n'), '10123', ALLOW) === '{"linked":true,"overlay":true}');
  check("the firmware's own row is not the app's",
    on([HEAD, FW].join('\n'), '10123', ALLOW) === '{"linked":false,"overlay":true}');
  check("another uid's connection to the port is not the app's",
    on([HEAD, APP].join('\n'), '10124', ALLOW) === '{"linked":false,"overlay":true}');
  check("a connection not yet established is not linked",
    on(APP.replace(':2243 01 ', ':2243 02 '), '10123', ALLOW) === '{"linked":false,"overlay":true}');
  check("no app installed (no uid) is not linked",
    on(APP, '', ALLOW) === '{"linked":false,"overlay":true}');
  check("a denied or default op is no overlay",
    on(APP, '10123', 'SYSTEM_ALERT_WINDOW: ignore') === '{"linked":true,"overlay":false}'
    && on(APP, '10123', 'No operations.') === '{"linked":true,"overlay":false}');
}

// ─── Boot hook content + idempotency ───────────────────────────────────────

const { _ECHOMUSE_RC, _ECHOMUSE_RC_CROWN, _ECHOMUSE_RC_LINEAGE, _ECHOMUSE_ADDOND_LINEAGE, _ADDOND_PATH,
        _rcInstalled, _rcFor } = await lift(
  [...WIZARD_PLATFORM_SRC, liftConst("_ECHOMUSE_RC"), liftConst("_ECHOMUSE_RC_CROWN"),
   liftConst("_ECHOMUSE_RC_LINEAGE"), liftConst("_ADDOND_PATH"), liftConst("_ECHOMUSE_ADDOND_LINEAGE"),
   liftFunctionDecl("_rcFor"), liftFunctionDecl("_rcInstalled"),
   "export { _ECHOMUSE_RC, _ECHOMUSE_RC_CROWN, _ECHOMUSE_RC_LINEAGE, _ECHOMUSE_ADDOND_LINEAGE, "
   + "_ADDOND_PATH, _rcInstalled, _rcFor };"]);

for (const [name, rc] of [['Dot', _ECHOMUSE_RC], ['Show', _ECHOMUSE_RC_CROWN]]) {
  check(`the ${name} rc declares the echomuse service against start_server.sh`,
    rc.includes('service echomuse /system/bin/sh /data/local/bin/start_server.sh'));
  check(`the ${name} rc's seclabel is u:r:adbd:s0, not u:r:su:s0 (init refuses that transition)`,
    rc.includes('seclabel u:r:adbd:s0') && !rc.includes('u:r:su:s0'));
  check(`the ${name} rc starts the service on sys.boot_completed`,
    rc.includes('on property:sys.boot_completed=1') && rc.includes('start echomuse'));
  check(`the ${name} rc ends in a newline (init drops an unterminated last line)`,
    rc.endsWith('start echomuse\n'));
}
check("the Dot rc keeps aipc first (the mixer's requirement)",
  /^\s*group aipc /m.test(_ECHOMUSE_RC));
// docs/echo-show-port-plan.md decisions 7 and 10: uid 0, egid audio, and
// none of the groups for Wi-Fi, Bluetooth or keys, which are Android's there.
{
  const groups = /^\s*group (.*)$/m.exec(_ECHOMUSE_RC_CROWN)?.[1].split(/\s+/) || [];
  check("the Show rc runs as root with primary group audio",
    /^\s*user root$/m.test(_ECHOMUSE_RC_CROWN) && groups[0] === 'audio', groups.join(' '));
  for (const g of ['aipc', 'wifi', 'bluetooth', 'net_bt_stack', 'net_admin', 'input']) {
    check(`the Show rc does not take group ${g}`, !groups.includes(g), groups.join(' '));
  }
}
check("each platform gets its own rc",
  _rcFor('fireos6') === _ECHOMUSE_RC && _rcFor('fireos6_crown') === _ECHOMUSE_RC_CROWN
  && _rcFor('lineage18_crown') === _ECHOMUSE_RC_LINEAGE);

// docs/echo-show-lineage-plan.md decision 9: init starts the service in
// `su` itself, measured with a test service of this shape
// (docs/echo-show-crown.md §8.5); no adbd way in, no move. Byte for byte.
check("the LineageOS rc is decision 9's, byte for byte",
  _ECHOMUSE_RC_LINEAGE === 'service echomuse /system/bin/sh /data/local/bin/start_server.sh\n'
    + '    class late_start\n'
    + '    user root\n'
    + '    group root audio inet\n'
    + '    seclabel u:r:su:s0\n'
    + '    disabled\n'
    + '\n'
    + 'on property:sys.boot_completed=1\n'
    + '    start echomuse\n', JSON.stringify(_ECHOMUSE_RC_LINEAGE));
check("a LineageOS Show carrying a Fire OS rc is not up to date",
  !_rcInstalled(_ECHOMUSE_RC_CROWN, _ECHOMUSE_RC_LINEAGE));

// The addon.d v2 script that keeps the rc across a LineageOS update: Lineage
// 18.1's own 50-lineage.sh shape, the rc and nothing else.
{
  const s = _ECHOMUSE_ADDOND_LINEAGE;
  check("the addon.d script lives in /system/addon.d", _ADDOND_PATH === '/system/addon.d/50-echomuse.sh');
  check("the addon.d script is v2 and sources backuptool's functions",
    s.startsWith('#!/sbin/sh\n') && /^# ADDOND_VERSION=2$/m.test(s)
    && /^\. \/tmp\/backuptool\.functions$/m.test(s), s);
  const listed = /^list_files\(\) \{\ncat <<EOF\n([\s\S]*?)\nEOF\n\}$/m.exec(s)?.[1];
  check("list_files names the rc and nothing else", listed === 'etc/init/echomuse.rc', listed);
  for (const stage of ['backup', 'restore', 'pre-backup', 'post-backup', 'pre-restore', 'post-restore']) {
    check(`the addon.d script handles ${stage}`, new RegExp(`^  ${stage}\\)$`, 'm').test(s));
  }
  check("the addon.d script ends in a newline", s.endsWith('esac\n'));

  // Run it the way backuptool.sh does, with S and C exported and stand-ins
  // for backup_file and restore_file that say what they were asked.
  // backuptool.sh ignores a script's exit status, and 50-lineage.sh's own
  // restore ends non-zero when nothing was backed up, so only the output
  // counts.
  const dir = mkdtempSync(join(tmpdir(), 'em-addond-'));
  try {
    writeFileSync(join(dir, 'backuptool.functions'),
      'backup_file() { echo "backup_file $1"; }\nrestore_file() { echo "restore_file $1 [$2]"; }\n');
    const script = join(dir, '50-echomuse.sh');
    writeFileSync(script, s.replace('/tmp/backuptool.functions', join(dir, 'backuptool.functions')));
    const backupdir = join(dir, 'backup');
    const run = stage => spawnSync('/bin/sh', [script, stage], {
      encoding: 'utf8', env: { PATH: process.env.PATH, S: '/system', C: backupdir } });
    {
      const r = run('backup');
      check("backup saves the rc", r.status === 0
        && r.stdout === 'backup_file /system/etc/init/echomuse.rc\n', JSON.stringify(r));
    }
    check("restore without a backup restores nothing", run('restore').stdout === '');
    mkdirSync(join(backupdir, 'system', 'etc', 'init'), { recursive: true });
    writeFileSync(join(backupdir, 'system', 'etc', 'init', 'echomuse.rc'), _ECHOMUSE_RC_LINEAGE);
    {
      const r = run('restore');
      check("restore puts the backed-up rc back", r.status === 0
        && r.stdout === 'restore_file /system/etc/init/echomuse.rc []\n', JSON.stringify(r));
    }
    for (const stage of ['pre-backup', 'post-backup', 'pre-restore', 'post-restore']) {
      const r = run(stage);
      check(`${stage} does nothing`, r.status === 0 && r.stdout === '' && r.stderr === '',
        JSON.stringify(r));
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

check("a device already carrying the exact rc is up to date", _rcInstalled(_ECHOMUSE_RC));
check("a Show already carrying its rc is up to date",
  _rcInstalled(_ECHOMUSE_RC_CROWN, _ECHOMUSE_RC_CROWN));
check("the Dot's rc on a Show is not up to date",
  !_rcInstalled(_ECHOMUSE_RC, _ECHOMUSE_RC_CROWN));
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
