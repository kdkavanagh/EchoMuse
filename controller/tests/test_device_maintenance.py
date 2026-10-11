"""Device maintenance over the shell plane: the transfer's tool probe, the
OTA free-space probe, the start-script sync and the debloat. Each must work
with toybox alone (Fire OS 6: no busybox, no python).

The shell text runs through the host's /bin/sh against fake tools laid out
like the image's PATH; the debloat runs against a recording shell.
"""

import asyncio
import contextlib
import hashlib
import os
import re
import shutil
import signal
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest

pytest.importorskip("aiohttp")
pytest.importorskip("websockets")

import em_api  # noqa: E402

PAYLOADS = Path(__file__).resolve().parents[1] / "device_payloads"
START_SERVER = PAYLOADS / "start_server.sh"
SH = "/bin/sh"

# toybox `df /data` on the Fire OS 6 Dot (G090LF0965260F1J), 1K blocks; the
# row is the one captured there.
TOYBOX_DF = (
    "Filesystem               1K-blocks    Used Available Use% Mounted on\n"
    "/dev/block/mmcblk0p16   1253196 130352   1122844  11% /data\n"
)


def _host(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        pytest.skip(f"host has no {name}")
    return path


def _tool(bindir: Path, name: str, body: str) -> None:
    path = bindir / name
    path.write_text("#!/bin/sh\n" + body + "\n")
    path.chmod(0o755)


def _link(bindir: Path, *names: str) -> None:
    """The host's own tools, standing in for toybox's."""
    for name in names:
        (bindir / name).symlink_to(_host(name))


def _sh(cmd: str, bindir: Path) -> str:
    """`cmd` through /bin/sh with `bindir` as the whole PATH."""
    return subprocess.run([SH, "-c", cmd], env={"PATH": str(bindir)},
                          capture_output=True, text=True, timeout=30).stdout


@pytest.fixture
def bindir(tmp_path: Path) -> Path:
    path = tmp_path / "bin"
    path.mkdir()
    return path


# ── transfer tool probe ──────────────────────────────────────────────────────

def _fireos6(bindir: Path) -> None:
    _link(bindir, "base64", "md5sum")      # toybox's, and nothing else


def _bogus_base64(bindir: Path) -> None:
    _tool(bindir, "base64", "exit 0")       # exits 0, decodes nothing
    _link(bindir, "md5sum")


@pytest.mark.parametrize("image, decoder, md5", [
    (_fireos6, "base64 -d", "md5sum"),
    (_bogus_base64, None, "md5sum"),
    (lambda _: None, None, None),
])
def test_tool_probe_names_the_decoder(bindir, image, decoder, md5):
    image(bindir)
    out = _sh(em_api.TOOL_PROBE, bindir)
    assert em_api.TOOL_PROBE_DONE in out, "the probe must answer even with no tools"
    assert em_api._probe_decoder(out) == decoder
    assert em_api._probe_md5(out) == md5


def test_md5_of_reads_the_hash(bindir, tmp_path):
    _fireos6(bindir)
    target = tmp_path / "payload"
    target.write_bytes(b"echomuse\n")
    out = _sh(em_api._md5_of(str(target)), bindir)
    assert out.split()[0] == hashlib.md5(b"echomuse\n").hexdigest()


# ── OTA free space ───────────────────────────────────────────────────────────

@pytest.mark.parametrize("probe_out, expected", [
    # toybox: header and row folded onto one line, 1K blocks.
    ("FREE_KB Filesystem 1K-blocks Used Available Use% Mounted on "
     "/dev/block/mmcblk0p16 1253196 130352 1122844 11% /data", 1122844 // 1024),
    # Unreadable is None ("carry on"), never a number.
    ("FREE_KB", None),
    ("FREE_KB Filesystem Size Used Free Blksize /data 1.2G 127.3M 1.1G 4096", None),
    ("", None),
])
def test_free_space_parses_toybox_kib(probe_out, expected):
    assert em_api._parse_free_mb(probe_out) == expected


def test_free_space_probe_reads_toybox_df(bindir):
    _tool(bindir, "df", f"""[ "$1" = -m ] && {{ echo "df: Unknown option 'm'" >&2; exit 1; }}
printf '%s' '{TOYBOX_DF}'""")
    assert em_api._parse_free_mb(_sh(em_api.FREE_SPACE_PROBE, bindir)) == 1122844 // 1024


def test_free_space_without_any_df_is_unknown(bindir):
    assert em_api._parse_free_mb(_sh(em_api.FREE_SPACE_PROBE, bindir)) is None


# ── debloat ──────────────────────────────────────────────────────────────────

class Shell:
    """Records what the controller sends; answers by the first matching needle."""

    def __init__(self) -> None:
        self.replies: list[tuple[str, str]] = []
        self.commands: list[str] = []
        self.transfers: list[tuple[str, str]] = []
        self.transfer_ok = True
        self.logs: list[tuple[str, str]] = []

    async def run(self, shell, live, cmd, timeout=30.0):
        self.commands.append(cmd)
        return next((reply for needle, reply in self.replies if needle in cmd), "")

    async def stream(self, shell, live, data, dest, mode="755", require_verify=False):
        self.transfers.append((dest, mode))
        if self.transfer_ok:
            return em_api.TransferResult(True)
        return em_api._transfer_failed(em_api.TransferStage.DECODER)

    async def log(self, device_id, level, source, message):
        self.logs.append((level, message))


@pytest.fixture
def dev_shell(monkeypatch) -> Shell:
    rec = Shell()
    monkeypatch.setattr(em_api, "_shell_run", rec.run)
    monkeypatch.setattr(em_api, "_stream_file_to_device", rec.stream)
    monkeypatch.setattr(em_api, "push_log_event", rec.log)

    async def no_wait(_seconds):
        return None
    monkeypatch.setattr(em_api.asyncio, "sleep", no_wait)
    return rec


def _debloat():
    live = SimpleNamespace(device_id="DEV1")
    asyncio.run(em_api._sync_debloat(None, live, "DEV1"))


START = "/data/local/bin/start_server.sh"
DEBLOAT = f"sh {START} debloat 2>&1"
START_MD5 = hashlib.md5(START_SERVER.read_bytes()).hexdigest()


def test_debloat_runs_the_start_scripts_denylist(dev_shell):
    dev_shell.replies = [("md5sum", f"{START_MD5}  {START}"),
                         (" debloat", f"DEBLOAT_STOPPED:{len(DENYLIST)} STILL_RUNNING:\n")]
    _debloat()
    assert dev_shell.commands == [em_api._md5_of(START), DEBLOAT]
    assert dev_shell.transfers == []
    assert dev_shell.logs[-1][0] == em_api.db.LogLevel.INFO
    assert f"{len(DENYLIST)} listed, none running" in dev_shell.logs[-1][1]


def test_debloat_syncs_a_stale_script_first(dev_shell, bindir, tmp_path):
    dev_shell.replies = [("NEW=$(", "SCRIPT_SYNCED"), (" debloat", "DEBLOAT_WAITING:puffin\n"
                         f"DEBLOAT_STOPPED:{len(DENYLIST) - 1} STILL_RUNNING:puffin\n")]
    _debloat()
    assert dev_shell.transfers == [(f"{START}.new", "755")]
    assert dev_shell.commands[-1] == DEBLOAT
    level, message = dev_shell.logs[-1]
    assert level == em_api.db.LogLevel.WARN and "still running: puffin" in message

    # The post-transfer check must work with toybox alone: no cut.
    verify = next(c for c in dev_shell.commands if c.startswith("NEW=$("))
    _link(bindir, "md5sum", "mv", "chmod", "rm")
    landed = tmp_path / "start_server.sh"
    (tmp_path / "start_server.sh.new").write_bytes(START_SERVER.read_bytes())
    assert "SCRIPT_SYNCED" in _sh(verify.replace(START, str(landed)), bindir)
    assert landed.read_bytes() == START_SERVER.read_bytes()


def test_debloat_never_runs_a_script_it_could_not_sync(dev_shell):
    """An older start_server.sh ignores `debloat` and runs in full: a second
    supervisor beside the live one."""
    dev_shell.transfer_ok = False
    _debloat()
    assert dev_shell.commands == [em_api._md5_of(START)]
    assert DEBLOAT not in dev_shell.commands
    assert "debloat not applied" in dev_shell.logs[-1][1]


# ── start_server.sh debloat ──────────────────────────────────────────────────

DENYLIST = re.search(r'^FOS6_DENYLIST="([^"]+)"', START_SERVER.read_text(), re.M).group(1).split()


@pytest.fixture
def device(tmp_path: Path, bindir: Path) -> Path:
    """A fake image: init's service states, and tools that record what ran.
    `ifconfig` is the first command past the debloat mode, so it records the
    leak and stops the script before it reaches the supervisor."""
    state = tmp_path / "state"
    state.mkdir()
    for svc in DENYLIST:
        (state / svc).write_text("running\n")
    _tool(bindir, "getprop", """case "$1" in
  init.svc.*) cat "$STATE/${1#init.svc.}" 2>/dev/null ;;
esac""")
    _tool(bindir, "stop", 'echo "$1" >> "$STATE/stopped"\n'
                          '[ -e "$STATE/$1.stubborn" ] || echo stopped > "$STATE/$1"')
    # Beyond pid_max, so the script's `kill` can never reach a host process.
    _tool(bindir, "pidof", '[ -e "$STATE/dnsmasq.alive" ] && { echo 999999999; exit 0; }; exit 1')
    _tool(bindir, "sleep", "exit 0")
    for leak in ("mount", "tinymix"):
        _tool(bindir, leak, 'echo "$0 $*" >> "$STATE/leaked"')
    _tool(bindir, "ifconfig", 'echo "$0 $*" >> "$STATE/leaked"; kill -9 $PPID')
    return state


def _start_server(bindir: Path, state: Path) -> tuple[int, str, str]:
    env = {"PATH": f"{bindir}:{os.environ.get('PATH', '/usr/bin:/bin')}",
           "STATE": str(state)}
    proc = subprocess.Popen([SH, str(START_SERVER), "debloat"], env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True)
    try:
        out, err = proc.communicate(timeout=30)
    finally:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)
    return proc.returncode, out, err


def _lines(path: Path) -> list[str]:
    return path.read_text().split() if path.exists() else []


def test_denylist_stops_sntpd_but_never_time_update():
    """sntpd waits on ACE NetMgr, which wifisvc (stopped) serves, so init
    restarts it every ~20 s forever; the firmware syncs the clock itself.
    time_update restores the saved time at boot and must keep running."""
    assert "sntpd" in DENYLIST and "time_update" not in DENYLIST


def test_debloat_mode_stops_the_denylist_and_nothing_else(bindir, device):
    rc, out, err = _start_server(bindir, device)
    assert _lines(device / "leaked") == [], "the mode must exit before the boot path"
    assert rc == 0, err
    assert _lines(device / "stopped") == DENYLIST
    assert out.splitlines()[-1] == f"DEBLOAT_STOPPED:{len(DENYLIST)} STILL_RUNNING:"
    assert "DEBLOAT_WAITING" not in out


def test_debloat_mode_names_what_is_still_running(bindir, device):
    (device / "puffin.stubborn").touch()
    (device / "dnsmasq.alive").touch()
    rc, out, err = _start_server(bindir, device)
    assert rc == 0 and _lines(device / "leaked") == [], err
    assert "DEBLOAT_WAITING:puffin,dnsmasq" in out
    assert out.splitlines()[-1] == f"DEBLOAT_STOPPED:{len(DENYLIST) - 1} STILL_RUNNING:puffin,dnsmasq"
    assert em_api._parse_denylist_result(out) == em_api.DenylistResult(
        len(DENYLIST) - 1, ("puffin", "dnsmasq"))


