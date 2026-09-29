import asyncio

import numpy as np

import em_media


def test_closing_the_stream_releases_the_decoder_before_aclose_returns(monkeypatch):
    """Pause/stop close the stream and move on at once; ffmpeg and the HTTP
    request must be gone by then, not whenever the loop's async-generator
    finalizer gets round to them."""
    released = []

    async def fake_once(url, rate, start_s):
        try:
            while True:
                yield np.zeros(4, dtype=np.int16)
        finally:
            released.append(url)    # where _stream_once kills ffmpeg and drops the request

    monkeypatch.setattr(em_media, "_stream_once", fake_once)

    async def main():
        stream = em_media.stream_url("http://tts/a")
        await anext(stream)
        await stream.aclose()
        assert released == ["http://tts/a"]

    asyncio.run(main())
