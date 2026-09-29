"""BLE proxy advert forwarding, observed on the ESPHome wire."""

import asyncio

import pytest

pytest.importorskip("google.protobuf")
pytest.importorskip("zeroconf")

import em_ble_proxy  # noqa: E402
from esphome import message_registry  # noqa: E402
from esphome.frame_protocol import PlaintextFrameProtocol  # noqa: E402
from esphome.vendor import api_pb2  # noqa: E402

DEVICE = "G090XX0012345678"


class _Transport(asyncio.Transport):
    def __init__(self) -> None:
        super().__init__()
        self.sent = b""

    def write(self, data: bytes | bytearray | memoryview) -> None:
        self.sent += bytes(data)

    def is_closing(self) -> bool:
        return False


class _Peer(PlaintextFrameProtocol):
    """Decodes what the proxy wrote, as HA would."""

    def __init__(self) -> None:
        super().__init__()
        self.messages: list[object] = []

    def packet_received(self, msg_type: int, payload: bytes) -> None:
        self.messages.append(message_registry.decode(msg_type, payload))


@pytest.fixture
def proxy():
    server = em_ble_proxy.DeviceBleProxyServer(DEVICE, "Kitchen", "02:00:12:34:56:78", 16101)
    em_ble_proxy._proxies[DEVICE] = server
    yield server
    em_ble_proxy._proxies.pop(DEVICE, None)


def test_malformed_adverts_are_dropped_not_forwarded_as_blank_entries(proxy):
    satellite = proxy._protocol_factory()
    transport = _Transport()
    satellite.connection_made(transport)
    satellite.subscribed = True

    em_ble_proxy.forward_adverts(DEVICE, [
        {"addr": "zz:not:hex", "addrType": 0, "rssi": -70, "data": ""},
        {"addr": "AA:BB:CC:DD:EE:FF", "addrType": 1, "rssi": -62, "data": "AgEG"},
        {"addr": "11:22:33:44:55:66", "addrType": 0, "rssi": -80, "data": "abc"},  # bad padding
    ])

    peer = _Peer()
    peer.data_received(transport.sent)
    [resp] = peer.messages
    assert isinstance(resp, api_pb2.BluetoothLERawAdvertisementsResponse)
    assert [(a.address, a.address_type, a.rssi, a.data) for a in resp.advertisements] == [
        (0xAABBCCDDEEFF, 1, -62, b"\x02\x01\x06"),
    ]
    assert (proxy.adverts_received, proxy.adverts_forwarded) == (3, 1)
