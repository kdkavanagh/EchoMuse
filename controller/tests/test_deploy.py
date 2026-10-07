"""
Deployment-shape guards: the image, add-on and env files must agree with the
modules and SPEC §18.4/§18.5 they package. Source-level because the suite
does not build images or start aiohttp.
"""

import ast
import json
import re
from pathlib import Path

CONTROLLER = Path(__file__).resolve().parents[1]
ROOT = CONTROLLER.parent
DOCKERFILE = CONTROLLER / "Dockerfile"


def _copy_sources() -> list[str]:
    """Every COPY source in the Dockerfile (root build context, §18.5)."""
    out = []
    for line in DOCKERFILE.read_text().splitlines():
        m = re.match(r"^COPY\s+(.+)\s+\S+$", line)
        if m:
            out.extend(m.group(1).split())
    return out


def test_every_copy_source_exists_in_the_root_build_context():
    """A COPY of a deleted or moved file fails the release build."""
    for src in _copy_sources():
        matches = list(ROOT.glob(src.rstrip("/")))
        assert matches, f"Dockerfile COPY {src} matches nothing under the repo root"


def test_image_carries_every_runtime_module_and_speech_input():
    sources = set(_copy_sources())
    assert "controller/em_*.py" in sources, "every controller module must be copied"
    for required in (
        "controller/version.py",
        "controller/echomuse_grammar/",
        "controller/speech_bundle.json",
        "controller/tools/fetch_speech_bundle.py",
        "bcresnet_audio.onnx",
        "bcresnet_audio.json",
    ):
        assert required in sources, f"image is missing {required}"


def test_compose_builds_from_the_root_context():
    compose = (CONTROLLER / "docker-compose.yml").read_text()
    assert re.search(r"^\s+context:\s*\.\.\s*$", compose, re.M)
    assert re.search(r"^\s+dockerfile:\s*controller/Dockerfile\s*$", compose, re.M)


def test_image_drops_openwakeword_and_keeps_hash_checked_assets():
    """§18.5: no openWakeWord; the device ORT runtime stays hash-checked; the
    speech bundle is fetched and verified with its Kroko attribution."""
    text = DOCKERFILE.read_text()
    assert "openwakeword" not in text and "download_models" not in text
    ort = text[text.index("onnxruntime-android-1.19.2.aar"):]
    assert "sha256sum -c" in ort[:400]
    assert "jni/armeabi-v7a/libonnxruntime.so" in ort
    assert "fetch_speech_bundle.py /app/speech" in text
    assert "huggingface.co/Banafo/Kroko-ASR" in text
    manifest = json.loads((CONTROLLER / "speech_bundle.json").read_text())
    assert manifest["attribution"]["url"] == "https://huggingface.co/Banafo/Kroko-ASR"


def test_requirements_pin_the_speech_runtime_and_drop_removed_packages():
    reqs = {
        re.split(r"[=<>!~]", line, maxsplit=1)[0].strip().lower(): line.strip()
        for line in (CONTROLLER / "requirements.txt").read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    manifest = json.loads((CONTROLLER / "speech_bundle.json").read_text())
    assert reqs["sherpa-onnx"] == f"sherpa-onnx=={manifest['runtime']['version']}"
    for removed in ("openwakeword", "speexdsp-ns", "tqdm", "scikit-learn", "requests"):
        assert removed not in reqs, f"{removed} is removed by SPEC §18.5"


def _addon_options() -> set[str]:
    config = (CONTROLLER / "config.yaml").read_text()
    block = re.search(r"^options:\n((?:[ ]+.*\n)+)", config, re.M)
    assert block, "config.yaml has no options block"
    return set(re.findall(r"^\s+([a-z_]+):", block.group(1), re.M))


def _start_option_map() -> dict[str, str]:
    tree = ast.parse((CONTROLLER / "em_start.py").read_text())
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
                isinstance(t, ast.Name) and t.id == "OPTION_ENV_VARS" for t in node.targets):
            return ast.literal_eval(node.value)
    raise AssertionError("em_start.OPTION_ENV_VARS not found")


def test_addon_options_and_entrypoint_map_agree():
    """A drifted option either never reaches the controller or trips the
    entrypoint's drift warning on every boot."""
    options = _addon_options()
    mapping = _start_option_map()
    assert options == set(mapping)
    assert not any("oww" in k for k in options)
    config = (CONTROLLER / "config.yaml").read_text()
    assert re.search(r"^homeassistant_api:\s*true\s*$", config, re.M), \
        "the add-on reaches HA with SUPERVISOR_TOKEN (§16.7)"


def test_env_example_names_ha_credentials_not_wake_settings():
    env = (CONTROLLER / ".env.example").read_text()
    keys = set(re.findall(r"^([A-Z_]+)=", env, re.M))
    assert {"HA_URL", "HA_TOKEN"} <= keys
    assert not {"OWW_MODEL", "OWW_THRESHOLD"} & keys


def test_dashboard_bundle_is_cache_busted():
    """
    /dashboard must not hand the browser a bare /static/dashboard.js URL.

    aiohttp's add_static sends Last-Modified and ETag but no Cache-Control, so
    browsers apply heuristic freshness and serve a stale bundle without
    revalidating. That failure is invisible server-side — deploy correct, file
    correct, compiled bundle correct, browser showing the previous UI — so it
    reads as "my change didn't work" and sends you hunting in the wrong place.
    Asserted at the source level because the alternative is starting an aiohttp
    app, which this suite deliberately does not do.
    """
    src = (Path(__file__).resolve().parent.parent / "em_api.py").read_text()
    handler = src[src.index("async def _serve_dashboard"):]
    handler = handler[:handler.index("\nasync def ", 1)]

    assert "dashboard.js?v=" in handler, \
        "the bundle URL must carry a cache-busting token"
    assert "no-cache" in handler, \
        "dashboard.html itself must be revalidated, or the new URL is never seen"
    # A version-string token would not change between two local "dev" builds;
    # mtime changes on every rebuild.
    assert "st_mtime" in handler, \
        "cache-bust on the bundle's mtime, not on a version string"


def test_dashboard_paths_are_ingress_safe():
    """
    As a Home Assistant add-on, the dashboard is mounted below a generated
    path (Ingress), not at the site root — an absolute "/static/..." or
    "/api/..." reference bypasses that path straight to the root and 404s.
    Every asset/API reference must therefore be relative, resolved against
    the <base href> em_api.py injects for the add-on case.

    Source-level because starting an aiohttp app + browser is outside what
    this suite does elsewhere (see test_dashboard_bundle_is_cache_busted).
    """
    static = CONTROLLER / "static"
    index = (static / "index.html").read_text()
    dashboard = (static / "dashboard.html").read_text()
    jsx = (static / "dashboard.jsx").read_text()
    api = (CONTROLLER / "em_api.py").read_text()
    config = (CONTROLLER / "config.yaml").read_text()

    for name, html in (("index.html", index), ("dashboard.html", dashboard)):
        assert 'href="/static/' not in html, f"{name} has an absolute /static href"
        assert 'src="/static/' not in html, f"{name} has an absolute /static src"
        assert "url('/static/" not in html, f"{name} has an absolute /static url()"

    assert "'/api/" not in index, \
        "index.html must fetch relative api/... paths, not /api/..."
    assert "location.replace('/dashboard')" not in index, \
        "index.html must redirect to a relative dashboard path"

    assert "function ingressPath(path)" in jsx, \
        "dashboard.jsx must relativize absolute paths for ingress"
    assert "function ingressWebSocketUrl(path)" in jsx, \
        "dashboard.jsx must build ingress-relative WebSocket URLs"
    assert "document.baseURI" in jsx, \
        "the WebSocket URL must resolve against the injected <base href>"
    assert "fetch(ingressPath(path)" in jsx, \
        "the shared API helpers must route through ingressPath"
    assert "location.replace('/')" not in jsx, \
        "an absolute root redirect bypasses the ingress path"

    assert '"static/dashboard.js"' in api, \
        "the cache-bust replace must target the relative asset URL"
    assert 'web.HTTPFound(".")' in api, \
        "/setup must redirect relatively, or it bounces out of the ingress path"
    assert "_with_ingress_base" in api, \
        "index.html and dashboard.html must get a <base href> injected"
    assert 'request.headers.get("X-Ingress-Path"' in api, \
        "the base path must come from Home Assistant's ingress header"
    assert "ECHOMUSE_HOME_ASSISTANT_INGRESS" in config, \
        "config.yaml must set the env var that gates ingress-only mode"


def test_addon_image_is_published_not_built_on_the_user_machine():
    """
    Without an `image:` key Supervisor builds the Dockerfile on whatever
    the user runs Home Assistant on — an onnxruntime/ffmpeg build on a Pi —
    and cannot pass EM_CONTROLLER_VERSION, so the controller reports "dev"
    and the update notice goes quiet. The arch list must also stay within
    what controller-release.yml actually publishes.
    """
    config = (CONTROLLER / "config.yaml").read_text()
    assert re.search(r"^image:\s*\S+", config, re.M), \
        "config.yaml must pull the published image, not build on the user's machine"

    workflow = (CONTROLLER.parent / ".github/workflows/controller-release.yml").read_text()
    platforms = re.search(r"platforms:\s*(\S+)", workflow)
    assert platforms, "controller-release.yml no longer declares platforms"
    published = platforms.group(1)
    arch_block = re.search(r"^arch:\n((?:\s+-\s*\w+\n)+)", config, re.M)
    assert arch_block, "config.yaml has no arch list"
    declared = set(re.findall(r"-\s*(\w+)", arch_block.group(1)))
    # Home Assistant's arch names vs Docker's platform names.
    equivalent = {"aarch64": "linux/arm64", "amd64": "linux/amd64"}
    for arch in declared:
        assert equivalent.get(arch) in published, (
            f"config.yaml offers {arch} but controller-release.yml only "
            f"publishes {published} — that install would find no image"
        )


def test_release_workflow_publishes_the_tag_annotation():
    """
    The notes shown in the dashboard come from the annotated tag, so the
    workflow must publish that rather than only GitHub's generated commit
    list. If this drifts, every future release silently shows a commit dump to
    whoever is deciding whether to update.
    """
    from pathlib import Path
    wf = (Path(__file__).resolve().parent.parent.parent
          / ".github" / "workflows" / "release.yml").read_text()
    assert "body_path:" in wf, "the release must publish notes from a file"
    assert "%(contents)" in wf, "notes must come from the tag annotation"


def test_every_device_payload_has_an_update_path():
    """
    A payload installed only at provisioning drifts forever.

    This has now bitten twice: start_server.sh (Lounge was a revision behind
    Office, 2026-07-11) and the debloat pair (round 2 added a package and every
    fielded device needed a manual push, 2026-07-30). Both fixes were the same
    shape — an md5-compared sync riding the OTA — so this asserts that every
    file in device_payloads/ is named by a sync function, and fails when a
    fourth payload is added without one.
    """
    from pathlib import Path
    root = Path(__file__).resolve().parent.parent
    api = (root / "em_api.py").read_text()
    payloads = sorted(p.name for p in (root / "device_payloads").iterdir() if p.is_file())
    assert payloads, "device_payloads/ is empty — has it moved?"

    for name in payloads:
        assert name in api, (
            f"{name} has no update path: nothing in em_api.py references it. "
            f"A payload installed only by the provisioning wizard drifts on every "
            f"device already in the field."
        )


def test_debloat_sync_reconciles_both_halves():
    """
    The Fire OS 5 debloat is a boot script AND a pm-hide list. Round 2 added a
    *package*, so a sync that only refreshed the script would have looked like
    it worked and changed nothing on any device.
    """
    from pathlib import Path
    api = (Path(__file__).resolve().parent.parent / "em_api.py").read_text()
    fn = api[api.index("async def _sync_debloat_fireos5"):]
    fn = fn[:fn.index("\nasync def ", 1)] if "\nasync def " in fn[1:] else fn

    assert "echomuse-debloat.sh" in fn, "the boot script half must be synced"
    assert "_debloat_packages()" in fn, "the pm-hide half must be reconciled"
    assert "pm hide" in fn, "drifted packages must actually be hidden"
    # Rename-based replacement: the running shell keeps the old inode.
    assert "mv " in fn and ".new" in fn, \
        "the script must be replaced by rename, not written in place"
    # md5 both before (skip when in sync) and after (verify the transfer).
    assert fn.count("md5") >= 2, "sync must md5-compare and md5-verify"


def test_debloat_reachable_without_an_ota():
    """
    The OTA-time sync cannot reach a device already on the latest firmware —
    which is the exact case that exposed the gap. A manual trigger is required,
    not a nicety.
    """
    from pathlib import Path
    root = Path(__file__).resolve().parent.parent
    api = (root / "em_api.py").read_text()
    assert '"/api/devices/{id}/debloat"' in api, "no manual debloat endpoint registered"
    jsx = (root / "static" / "dashboard.jsx").read_text()
    assert "/debloat`" in jsx, "the dashboard must be able to trigger it"


def test_stale_release_cache_is_not_returned_when_it_has_aged_out():
    """
    _get_cached_release used to fire a refresh into the background and return
    the STALE value. Two consequences, both seen on 2026-07-30: the dashboard
    reported "there's an update" only after someone pressed Check now, and an
    OTA pushed v2.9.9 while v2.9.10 was the current release.

    The refresh is now awaited when the DB cache has aged past the check
    interval, falling back to the stale value only if the fetch fails.
    """
    from pathlib import Path
    api = (Path(__file__).resolve().parent.parent / "em_api.py").read_text()
    fn = api[api.index("async def _get_cached_release"):]
    fn = fn[:fn.index("\nasync def ", 1)]

    assert "await _fetch_latest_release()" in fn, \
        "an aged-out cache must be refreshed synchronously, not in the background"
    assert "asyncio.create_task(_fetch_latest_release())" not in fn, \
        "fire-and-forget refresh returns the stale value to this caller"


def test_controller_update_is_advisory_only():
    """
    The dashboard may TELL you a newer controller exists; it must never offer
    to apply it.

    The controller is a container the user owns and updates with their own
    docker tooling. An in-app update would have to restart the process serving
    the page, mid-request, with no way to report the outcome — and it is an
    explicit product decision (Wil, 2026-07-31) that this stays out of the
    interface. The notice is information for a decision the user takes
    elsewhere.
    """
    from pathlib import Path
    root = Path(__file__).resolve().parent.parent

    jsx = (root / "static" / "dashboard.jsx").read_text()
    start = jsx.index("{/* Controller update notice.")
    banner = jsx[start:jsx.index("{/* Summary */}", start)]
    for forbidden in ("API.post", "API.put", "API.delete", "onClick={doUpdate"):
        assert forbidden not in banner, (
            f"the controller update notice must not perform actions, found "
            f"{forbidden!r} — updating is the user's docker command to run"
        )

    api = (root / "em_api.py").read_text()
    assert 'add_get("/api/releases/controller"' in api, \
        "the controller release endpoint must be read-only (GET)"
    for verb in ("add_post", "add_put", "add_delete"):
        assert f'{verb}("/api/releases/controller' not in api, \
            f"{verb} on /api/releases/controller would make the update actionable"


def test_controller_notes_come_from_the_tag_annotation():
    """
    controller-v* tags ship a GHCR image and no GitHub Release (CLAUDE.md,
    "Versioning / releases"), so the notes must be read from the annotated
    tag object. Reading them from the releases list would return the newest
    DEVICE firmware release instead — right shape, wrong product.
    """
    from pathlib import Path
    api = (Path(__file__).resolve().parent.parent / "em_api.py").read_text()
    fn = api[api.index("async def _fetch_controller_release"):]
    fn = fn[:fn.index("\nasync def ", 1)]
    assert "GITHUB_TAGS_URL" in fn and "GITHUB_TAG_OBJECT_URL" in fn, \
        "controller notes must come from the tag annotation, not /releases"
    assert "GITHUB_API_URL" not in fn, \
        "that is the device firmware release feed, not the controller's"


def test_every_db_call_in_em_api_exists():
    """
    A typo'd db.<name> is invisible to pyflakes (it is a valid attribute
    expression) and raises AttributeError only on the request that uses it.
    That is the same shape as the NameError which stopped wake word
    fleet-wide on 2026-07-30: green CI, clean logs, broken at runtime.

    Written after db.get_global_config() — a function that has never existed —
    reached a live endpoint.
    """
    import ast
    from pathlib import Path
    root = Path(__file__).resolve().parent.parent

    tree = ast.parse((root / "em_api.py").read_text())
    used = {
        n.attr for n in ast.walk(tree)
        if isinstance(n, ast.Attribute)
        and isinstance(n.value, ast.Name) and n.value.id == "db"
    }
    db_tree = ast.parse((root / "em_db.py").read_text())
    defined = {
        n.name for n in db_tree.body
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
    } | {
        t.id for n in db_tree.body if isinstance(n, ast.Assign)
        for t in n.targets if isinstance(t, ast.Name)
    } | {
        # Annotated module constants, e.g. `MIGRATIONS: list[str] = [...]`.
        # Missing these produced a false positive on db.MIGRATIONS, which is
        # the failure mode that gets a guard disabled rather than fixed.
        n.target.id for n in db_tree.body
        if isinstance(n, ast.AnnAssign) and isinstance(n.target, ast.Name)
    }
    missing = sorted(used - defined)
    assert not missing, f"em_api.py calls db.{{{', '.join(missing)}}} which em_db.py does not define"


def test_no_unjustified_hardcoded_media_state():
    """
    Forbid the SHAPE, not just the instances.

    A hardcoded MediaPlayerState sent to HA asserts what the player is doing
    without asking em_player, so it is wrong whenever the guess is wrong — and
    it wins, because the feed announces PLAYING exactly once, at the start.

    This has now been the same bug twice: the turn-end IDLE (#53, "reports
    idle even though the music continues to play"), and then IDLE on every
    device volume report, which told Music Assistant the music had stopped
    while it was audibly playing. Fixing instances one at a time is how the
    second one survived the first fix, so this pins the rule.

    Two remain legitimate and are named explicitly:
      - PLAYING for play_media, documented as optimistic — the feed pushes
        the authoritative state moments later.
      - ANNOUNCING, a genuine transition with no em_player equivalent.

    Anything else must go through _media_state_msg(), which reads em_player
    truth.
    """
    from pathlib import Path
    src = (Path(__file__).resolve().parent.parent / "em_esphome.py").read_text()

    allowed = {"MediaPlayerState.PLAYING", "MediaPlayerState.ANNOUNCING"}
    found = [
        line.strip()
        for line in src.splitlines()
        if "state=MediaPlayerState." in line
    ]
    offenders = [
        ln for ln in found
        if not any(a in ln for a in allowed)
    ]
    assert not offenders, (
        f"hardcoded media state(s) {offenders} — use _media_state_msg() so the "
        f"entity reflects what em_player is actually doing"
    )


def test_supervisor_log_path_matches_between_script_and_controller():
    """
    The supervisor writes its decisions to a persistent path and the
    controller reads them back from it. Two languages, one path — if they
    drift, the fetch silently returns nothing and a failed update stays
    unexplained, which is the exact failure this feature exists to remove.
    """
    from pathlib import Path
    root = Path(__file__).resolve().parent.parent

    script = (root / "device_payloads" / "start_server.sh").read_text()
    api = (root / "em_api.py").read_text()

    import re
    m = re.search(r"^SUP_LOG=(\S+)", script, re.M)
    assert m, "start_server.sh no longer defines SUP_LOG"
    script_path = m.group(1)

    m = re.search(r'^SUPERVISOR_LOG = "([^"]+)"', api, re.M)
    assert m, "em_api.py no longer defines SUPERVISOR_LOG"
    assert m.group(1) == script_path, (
        f"supervisor log path drifted: script writes {script_path}, "
        f"controller reads {m.group(1)}"
    )


def test_supervisor_log_is_persistent_and_bounded():
    """
    Two properties it cannot lose.

    PERSISTENT: under /data. /tmp is RAM-backed, so a log there is wiped by
    the reboot used to recover from the very failure it would explain.

    BOUNDED: these devices have ~350MB free and no operator. A crash-loop
    writing every few seconds must never be able to fill /data — so the trim
    happens BEFORE the append, not after.
    """
    from pathlib import Path
    import re
    script = (Path(__file__).resolve().parent.parent
              / "device_payloads" / "start_server.sh").read_text()

    m = re.search(r"^SUP_LOG=(\S+)", script, re.M)
    assert m.group(1).startswith("/data/"), \
        "the supervisor log must live on persistent storage, not /tmp"

    fn = script[script.index("sup_log() {"):]
    fn = fn[:fn.index("\n}")]
    trim = fn.index("SUP_MAX")
    append = fn.index('>> "$SUP_LOG"')
    assert trim < append, \
        "the size check must run before the append, or a crash-loop outruns it"


def test_a_failed_update_asks_for_the_supervisor_log():
    """
    The fetch cannot happen at failure time — the device is gone, which IS the
    problem. So every failure path must record that an explanation is owed,
    and the connect path must collect it.
    """
    from pathlib import Path
    api = (Path(__file__).resolve().parent.parent / "em_api.py").read_text()

    # The failure paths live in _run_update, which is what awaits
    # _monitor_reconnect and decides what its result means.
    monitor = api[api.index("async def _run_update"):]
    monitor = monitor[:monitor.index("\nasync def ", 1)]
    assert monitor.count("_supervisor_log_wanted.add") >= 2, (
        "both update-failure paths (auto-rollback and timeout) must request "
        "the supervisor log"
    )

    connect = api[api.index("async def notify_device_connected"):]
    connect = connect[:connect.index("\nasync def ", 1)]
    assert "_collect_supervisor_log" in connect, \
        "nothing collects the supervisor log when the device comes back"


def test_support_bundle_attributes_metrics_to_a_device():
    """
    `db.get_device_metrics` builds its own result dicts and does NOT include
    the device, so the support handler must attach it. Without that, every
    device's hourly CPU, memory and RTT pool into one flat anonymous list —
    present in the bundle, useless for diagnosis, and wrong in the quiet way
    where the file still looks full of data. Shipped like that in #63.
    """
    src = (CONTROLLER / "em_api.py").read_text()
    body = src.split("_get_support_bundle")[-1]
    call = re.search(r"metrics \+= \[(.*?)\]\n", body, re.S)
    assert call, "could not find where the support bundle collects metrics"
    assert "device_id" in call.group(1), (
        "support bundle metrics rows must carry device_id — "
        "get_device_metrics does not return it"
    )


def _fn_body(src: str, name: str) -> str:
    """Slice one async def out of a module's source, up to the next top-level def."""
    start = src.index(f"async def {name}")
    rest  = src[start + 1:]
    end   = rest.index("\nasync def ") if "\nasync def " in rest else len(rest)
    return src[start:start + 1 + end]


def test_firmware_transfer_is_verified_by_md5_not_by_an_exit_status():
    """
    TRANSFER_OK only ever proved that the decode pipeline and chmod exited 0 —
    not that the bytes on the device match the bytes we sent (#76).

    That matters because a corrupt binary and a genuinely broken one produce
    the SAME observable: three fast exits, a symlink flip, and a device back on
    its old version. Shipping an unverified binary therefore costs a reboot and
    a rollback to learn nothing at all.
    """
    src = (CONTROLLER / "em_api.py").read_text()
    fn  = _fn_body(src, "_stream_file_to_device")

    assert "hashlib.md5(data).hexdigest()" in fn, (
        "the transfer must hash what it actually sent — hashing anything else "
        "verifies the wrong thing"
    )
    assert ".part" in fn and "mv " in fn, (
        "bytes must land in .part and be renamed only once verified, so a bad "
        "transfer leaves the destination as it was"
    )
    # The rename must be conditional on the comparison, not merely nearby.
    assert re.search(r"case .*GOT.*in .*want.*mv ", fn, re.S), (
        "the rename must be guarded by the md5 comparison"
    )
    assert "rm -f" in fn, "a failed verification must remove the .part"

    # Anchored on the CALL, not the function body: the docstring explains why
    # require_verify is set, so a body-wide search passes on the prose alone
    # while the argument is gone (caught by reintroducing exactly that).
    slot = _fn_body(src, "_stream_binary_to_slot")
    call = slot[slot.index("return await _stream_file_to_device"):]
    assert "require_verify=True" in call, (
        "firmware is the payload where an unverifiable transfer must fail "
        "rather than proceed — it is the one we are about to boot"
    )


def test_a_corrupt_binary_never_reaches_the_symlink_flip():
    """
    The real win of verifying is not the error message, it is the ordering:
    a mismatch must be caught while the device is still running fine, not
    after it has taken a reboot and a rollback to tell us the same thing.
    """
    src = (CONTROLLER / "em_api.py").read_text()
    ota = src[src.index("await _stream_binary_to_slot("):]
    ota = ota[:ota.index("_monitor_reconnect")]

    guard = ota.index("if not ok:")
    flip  = ota.index("ln -sf")
    assert guard < flip, (
        "the transfer result must be checked BEFORE the symlink flip"
    )
    assert "return" in ota[guard:flip], (
        "a failed transfer must return, not fall through to the flip — "
        "otherwise verification changes the log message and nothing else"
    )


def test_ota_checks_free_space_before_writing_anything():
    """
    The OTA path had no space check at all, unlike the asset path.

    Two traps, both already paid for elsewhere: read the figure with
    parse_free_mb rather than an awk field index (busybox wraps a long
    filesystem name onto its own line, so $4 is the PERCENTAGE on these
    devices), and treat an unreadable df as "carry on" rather than as a full
    disk — refusing on an unparsed reading blocks updates on any device whose
    df we have not seen.
    """
    src = (CONTROLLER / "em_api.py").read_text()
    ota = src[src.index("inactive_slot = "):src.index("await _stream_binary_to_slot(")]

    assert "parse_free_mb" in ota, (
        "free space must be read with parse_free_mb, never an awk field index"
    )
    # Comments are stripped first: the trap is worth explaining in a comment,
    # and a test that reads its own warning as the bug is a test that can only
    # be silenced by deleting the explanation.
    code = "\n".join(l for l in ota.splitlines() if not l.lstrip().startswith("#"))
    assert "awk" not in code, "an awk field index reads the percentage on these devices"
    assert "free_mb is not None and free_mb <" in ota, (
        "an unknown reading must not be compared as if it were a number"
    )


def test_a_transfer_never_deletes_the_destination_before_sending():
    """
    For firmware the destination IS the rollback slot, so deleting it up front
    means a transfer that fails early leaves the device with a good active
    slot and an empty partner — and a later crash-loop flips the symlink onto
    nothing. Three Dots hit exactly that in #121, while being told the slot
    had been left untouched.

    The `.part` discipline only protects `dest` from a CORRUPT transfer. It
    cannot protect it from being removed before the transfer starts.
    """
    src = (CONTROLLER / "em_api.py").read_text()
    fn  = _fn_body(src, "_stream_file_to_device")
    code = "\n".join(l for l in fn.splitlines() if not l.lstrip().startswith("#"))

    assert "rm -f {dest}" not in code, (
        "deleting dest before sending destroys the rollback slot on any "
        "transfer that fails early"
    )
    # The .part cleanup on a bad md5 must survive — that one is load-bearing.
    assert "rm -f {landing}" in code or "rm -f {landing};" in code, (
        "a failed verification must still remove the .part"
    )


def test_tested_firmware_build_matches_the_docs():
    """
    The wizard warns when a device is on a FireOS build other than the one
    EchoMuse is developed against, and docs/rooting.md tells people which to
    flash. Those two have to name the same build: a warning pointing at a
    version the docs do not mention is worse than no warning, because the
    person reading it has nowhere to go.

    Verified against the fleet 2026-08-07 — all three connected devices report
    ro.build.version.incremental = 272.6.8.0_user_680767620.
    """
    jsx = (CONTROLLER / "static" / "dashboard.jsx").read_text()
    m = re.search(r"_TESTED_FIREOS_BUILD\s*=\s*'([^']+)'", jsx)
    assert m, "dashboard.jsx no longer declares _TESTED_FIREOS_BUILD"
    build = m.group(1)

    rooting = (CONTROLLER.parent / "docs" / "rooting.md").read_text()
    assert build in rooting, (
        f"the wizard warns against build {build} but docs/rooting.md never "
        f"names it — a reader has nowhere to go"
    )


def test_tested_firmware_build_matches_the_docs_fireos6():
    """
    Same contract as test_tested_firmware_build_matches_the_docs, for the
    Fire OS 6 pin (_TESTED_FIREOS6_BUILD) added alongside it. docs/rooting.md
    names the Fire OS 6 build EchoMuse is tested against in its own section;
    the wizard's warning has to name the same one.
    """
    jsx = (CONTROLLER / "static" / "dashboard.jsx").read_text()
    m = re.search(r"_TESTED_FIREOS6_BUILD\s*=\s*'([^']+)'", jsx)
    assert m, "dashboard.jsx no longer declares _TESTED_FIREOS6_BUILD"
    build = m.group(1)

    rooting = (CONTROLLER.parent / "docs" / "rooting.md").read_text()
    assert build in rooting, (
        f"the wizard warns against Fire OS 6 build {build} but docs/rooting.md "
        f"never names it — a reader has nowhere to go"
    )


def test_push_log_event_callers_do_not_also_persist():
    """
    `push_log_event` persists AND pushes. A caller that also calls
    `db.log_device` writes the line twice.

    That is not hypothetical: the device `log` handler did both, so every
    device log line landed twice about 6ms apart, and roughly half of
    `device_logs` was duplicates. It stayed invisible because a doubled log
    line looks like a device that logged twice.

    Two things it cost beyond the wasted rows. `em_support.thin_noise` keeps
    the newest three `[mem]` lines per device, so duplication halved the
    distinct readings a leak hunt gets from a bundle. And the redundant call
    was a synchronous SQLite write on the event loop, which
    `event_loop_lag_monitor` exists to catch.

    Checked by source shape because the alternative is importing
    em_controller, which the suite deliberately does not do.
    """
    root = Path(__file__).resolve().parent.parent
    for name in ("em_controller.py", "em_api.py"):
        lines = (root / name).read_text().splitlines()
        for i, line in enumerate(lines):
            if "push_log_event(" not in line or "async def" in line:
                continue
            # The persist would sit just above the push, in the same block.
            window = lines[max(0, i - 6):i]
            offenders = [w.strip() for w in window if "db.log_device(" in w]
            assert not offenders, (
                f"{name}:{i+1} calls push_log_event, which already persists, "
                f"but is preceded by {offenders[0]!r}. That writes the log "
                f"line twice. Drop the db.log_device call."
            )


# ── Ambient light status reaches a support bundle (#90) ──────────────────────
#
# Two users reported no light sensor and the bundle could not say why: the
# firmware reports whether the chip is absent or the driver has not bound
# (`session.hello.ambient_light_status`); the bundle must carry it.

def test_bundle_live_state_carries_ambient_light_status():
    """And the support bundle must actually carry it.

    em_support takes live_state wholesale rather than through an allowlist, so
    the field has to be put there by em_api. A reason that never reaches a
    bundle leaves us exactly where #90 started.
    """
    root = Path(__file__).resolve().parent.parent
    api = (root / "em_api.py").read_text()
    assert '"ambient_light_status":' in api, (
        "em_api's live_state must include ambient_light_status so support "
        "bundles can answer why a device reports no light sensor"
    )

