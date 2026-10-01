"""ESPHome reply runs (§16.7): the reply audio URL is released only once HA
has decided what the reply is — at INTENT_PROGRESS `tts_start_streaming` for
a streamed reply, else at TTS_END. A fetch of the RUN_START URL before then
parks forever when HA overrides the result with its acknowledge sound."""

import asyncio

import em_esphome
from em_ha_client import IntentEnded, RunEnded, TtsReady
from esphome import message_registry as registry
from esphome.vendor import api_pb2

_ET = api_pb2.VoiceAssistantEvent
START_URL = "http://ha:8123/api/tts_proxy/Qp2Lx7.flac"
END_URL = "http://ha:8123/api/tts_proxy/Qp2Lx7.flac?final"


class _Transport(asyncio.Transport):
    def __init__(self) -> None:
        super().__init__()
        self.writes: list[bytes] = []

    def write(self, data: bytes | bytearray | memoryview) -> None:
        self.writes.append(bytes(data))

    def is_closing(self) -> bool:
        return False

    def get_extra_info(self, name: str, default: object = None) -> object:
        return default


def _event(typ: int, **data: str) -> api_pb2.VoiceAssistantEventResponse:
    return api_pb2.VoiceAssistantEventResponse(
        event_type=typ,
        data=[api_pb2.VoiceAssistantEventData(name=k, value=v) for k, v in data.items()])


async def _reply(satellite: em_esphome.EchoMuseSatellite,
                 events: list[api_pb2.VoiceAssistantEventResponse]) -> list[object]:
    async def collect() -> list[object]:
        return [e async for e in satellite.esphome_reply(b"\x00\x00" * 320)]

    task = asyncio.create_task(collect())
    while satellite._run_queue is None:
        await asyncio.sleep(0)
    for msg in events:
        satellite.packet_received(*registry.encode(msg))
    return await asyncio.wait_for(task, 1.0)


def _satellite() -> em_esphome.EchoMuseSatellite:
    server = em_esphome.DeviceESPhomeServer("0123456789abcdef", "Office", "02:00:00:00:00:01", 0)
    satellite = server._protocol_factory()
    assert isinstance(satellite, em_esphome.EchoMuseSatellite)
    satellite.connection_made(_Transport())
    return satellite


def test_run_start_url_is_released_only_by_tts_start_streaming_and_resets_per_run():
    intent_end = _event(_ET.VOICE_ASSISTANT_INTENT_END, speech="Once upon a time.",
                        conversation_id="c1", continue_conversation="0")

    async def main() -> None:
        satellite = _satellite()
        streamed = await _reply(satellite, [
            _event(_ET.VOICE_ASSISTANT_RUN_START, url=START_URL),
            _event(_ET.VOICE_ASSISTANT_INTENT_PROGRESS, tts_start_streaming="1"),
            intent_end,
            _event(_ET.VOICE_ASSISTANT_TTS_END, url=END_URL),
            _event(_ET.VOICE_ASSISTANT_RUN_END),
        ])
        assert streamed[0] == TtsReady(START_URL, True)
        assert [type(e) for e in streamed] == [TtsReady, IntentEnded, RunEnded]

        # The next reply run announces a URL but never streams (HA may override the
        # result): nothing is fetchable before TTS_END, and the previous run's
        # released URL does not carry over.
        acknowledged = await _reply(satellite, [
            _event(_ET.VOICE_ASSISTANT_RUN_START, url=START_URL),
            _event(_ET.VOICE_ASSISTANT_INTENT_PROGRESS, tts_start_streaming="0"),
            intent_end,
            _event(_ET.VOICE_ASSISTANT_TTS_END, url=END_URL),
            _event(_ET.VOICE_ASSISTANT_RUN_END),
        ])
        assert [type(e) for e in acknowledged] == [IntentEnded, TtsReady, RunEnded]
        assert acknowledged[1] == TtsReady(END_URL, False)

    asyncio.run(main())
