"""The firmware bundled with the controller: the only build it installs."""

import hashlib

import pytest

import em_firmware as fw

BINARY = b"\x7fELF echomuse server " * 4096


def bundle(directory, binary=BINARY, version="fw-3fa9c2e1b7d0\n"):
    if binary is not None:
        (directory / fw.BINARY_NAME).write_bytes(binary)
    if version is not None:
        (directory / fw.VERSION_NAME).write_text(version)
    return directory


def test_valid_bundle_reports_version_size_and_sha256(tmp_path):
    firmware = fw.load(bundle(tmp_path))
    assert firmware.version == "fw-3fa9c2e1b7d0"
    assert firmware.size == len(BINARY)
    assert firmware.sha256 == hashlib.sha256(BINARY).hexdigest()
    assert firmware.path == tmp_path / fw.BINARY_NAME
    assert firmware.read() == BINARY


@pytest.mark.parametrize("binary, version, message", [
    (None, "fw-1\n", "binary is missing"),
    (b"", "fw-1\n", "binary is empty"),
    (BINARY, None, "version file is missing"),
    (BINARY, "", "version file is blank"),
    (BINARY, " \n\n", "version file is blank"),
    (BINARY, "fw-1\nfw-2\n", "single line"),
])
def test_incomplete_bundle_is_refused(tmp_path, binary, version, message):
    with pytest.raises(fw.FirmwareError, match=message):
        fw.load(bundle(tmp_path, binary, version))


def test_a_binary_replaced_after_load_is_not_installed(tmp_path):
    """Rebuilding device/build under a running bare-metal controller must not
    install new bytes under the version read at startup."""
    firmware = fw.load(bundle(tmp_path))
    (tmp_path / fw.BINARY_NAME).write_bytes(BINARY + b"rebuilt")
    with pytest.raises(fw.FirmwareError, match="changed since the controller started"):
        firmware.read()
