"""Speech worker with the real pinned bundle (§8.1, §16.6).

The bundle comes from `SPEECH_BUNDLE_DIR` when it points at a verified bundle;
otherwise it is assembled with `tools/fetch_speech_bundle.py` from local copies
named by `EM_TEST_SHERPA_WHEEL`, `EM_TEST_KROKO_ARCHIVE`, `EM_TEST_SILERO_V5`
(defaults: the evidence copies under /tmp). Tests skip when neither exists.
"""

import asyncio
import importlib.util
import logging
import os
import sys
import time
from pathlib import Path

import numpy as np
import pytest

import em_speech_worker as sw
from _wyoming_fake import FakeWyoming
from em_attribution import EchoResult
from em_audio_timeline import ReferenceTimeline, SampleTimeline, StreamId
from em_pause_asr import WyomingServer

ROOT = Path(__file__).resolve().parents[2]
CONTROLLER = ROOT / "controller"
DEFAULTS = {
    "EM_TEST_SHERPA_WHEEL": "/tmp/speech-bundle-probe/wheels/"
                            "sherpa_onnx-1.13.8-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.whl",
    "EM_TEST_KROKO_ARCHIVE": "/tmp/asr-bakeoff/downloads/sherpa-onnx-streaming-zipformer-en-kroko-2025-08-06.tar.bz2",
    "EM_TEST_SILERO_V5": "/tmp/asr-bakeoff/vad/silero_vad_v5.onnx",
}


def test_evidence_copy_is_times_ten_clamped_to_int16():
    pcm = np.array([0, 1, -1, 3276, 3277, -3277, 32767, -32768], dtype=np.int16)
    out = sw.evidence_copy(pcm)
    assert out.dtype == np.int16
    assert out.tolist() == [0, 10, -10, 32760, 32767, -32768, 32767, -32768]


def test_vad_input_is_times_ten_to_the_one_and_a_half_saturated_as_float():
    pcm = np.array([0, 100, -100, 1036, 1037, -1037, 32767, -32768], dtype=np.int16)
    out = sw.vad_input(pcm)
    assert out.dtype == np.float32
    assert (out * 32768).tolist() == [0, 3162, -3162, 32761, 32767, -32768, 32767, -32768]


class _GainProbe:
    """Stands in for both models and keeps what each was given."""

    revision = "fake"

    def __init__(self):
        self.cells, self.decoded = [], []
        self.recognizer = self

    def create_stream(self):
        return object()

    def vad(self, state, cell):
        self.cells.append(cell.copy())
        return 0.0

    def decode(self, stream, evidence):
        self.decoded.append(evidence.copy())

    def result(self, stream):
        return sw.AsrResult("", (), (), 0)


def test_vad_reads_its_own_input_and_asr_the_evidence_copy():
    """§16.6: Silero gets the +30 dB VAD input on the 512 cell grid; the utterance's
    stream gets the +20 dB evidence copy of the same blocks."""
    probe = _GainProbe()
    pcm = (np.arange(2_560) % 2_000 - 1_000).astype(np.int16)

    async def main():
        w = sw.SpeechWorker(None, on_observation=lambda o: None)
        w._executor = sw.ThreadPoolExecutor(max_workers=1)
        w.models = probe
        w._wake["g"] = object()
        w.open_lease("dev", 7, "L", "g")
        w.open_utterance("L", "U", 0, 0, SampleTimeline())
        w.submit_mic_block("L", 0, pcm[:1_280])
        w.submit_mic_block("L", 1_280, pcm[1_280:])
        await w.close()

    asyncio.run(main())
    assert [c.size for c in probe.cells] == [512] * 5
    assert np.array_equal(np.concatenate(probe.cells), sw.vad_input(pcm))
    assert np.array_equal(np.concatenate(probe.decoded), sw.evidence_copy(pcm))


def test_observations_with_a_callback_are_not_also_queued():
    """The controller consumes observations through `on_observation` and never reads the
    queue; buffering them there as well kept every VAD/ASR/echo result for the process lifetime."""
    got = []

    async def main():
        w = sw.SpeechWorker(None, on_observation=got.append)
        await w._emit(sw.Observation("dev", 7, StreamId.MIC, sw.ObservationSource.CONTROLLER,
                                     sw.ObservationKind.VAD, 512, None, None, None,
                                     sw.VadPayload(0, (0.1,)), 0, "L"))
        return w.observations.qsize()

    assert asyncio.run(main()) == 0 and len(got) == 1


class _StateVad:
    """Stands in for Silero: a cell's probability is the state it ran from plus its context's
    first sample, and each cell adds 1 to the state."""

    revision = "fake"

    def vad(self, state, cell):
        p = float(state.state[0, 0, 0] + state.window[0])
        state.state = state.state + 1
        state.window[:sw.VAD_CONTEXT_SAMPLES] = cell[-sw.VAD_CONTEXT_SAMPLES:]
        return p


def test_a_primed_lease_restores_the_voice_from_the_first_cell_at_its_sample():
    """§16.6 reply window: cells before the prime keep the carried state, the first cell at
    or after its sample runs from the snapshot (state and context), a later prime without a
    snapshot starts a fresh state."""
    got = []

    async def main():
        w = sw.SpeechWorker(None, on_observation=got.append)
        w._executor = sw.ThreadPoolExecutor(max_workers=1)
        w.models = _StateVad()
        w._wake["g"] = object()                             # the lease's wake graph: unused here
        w.open_lease("dev", 7, "L", "g")
        silence = np.zeros(1_280, dtype=np.int16)
        w.submit_mic_block("L", 0, silence)                 # cells 0, 512 (256 samples pending)
        voice = sw.VoiceSnapshot(np.full((2, 1, 128), 100, dtype=np.float32),
                                 np.full(64, 0.5, dtype=np.float32), 999)
        w.prime_vad("L", 1_280, voice)
        w.submit_mic_block("L", 1_280, silence)             # cells 1,024 (carried), 1,536, 2,048
        w.prime_vad("L", 3_000, None)
        w.submit_mic_block("L", 2_560, silence)             # cells 2,560 (carried), 3,072 (fresh)
        await w.close()

    asyncio.run(main())
    probs = [p for o in got if isinstance(o.payload, sw.VadPayload) for p in o.payload.probabilities]
    assert probs == [0.0, 1.0, 2.0, 100.5, 101.0, 102.0, 0.0]


class _Clock:
    """The worker's injected `now`: moves only when a test (or a fake job) moves it."""

    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


class _ProbeModels:
    revision = "fake"

    def probe(self):
        pass


ECHO = ((EchoResult.ECHO_ONLY,), None)


def _fake_worker(clock, got, **kw):
    w = sw.SpeechWorker(None, on_observation=got.append, now=clock, **kw)
    w._executor = sw.ThreadPoolExecutor(max_workers=1)
    w.models = _ProbeModels()
    w._wake["g"] = object()
    return w


async def _until(condition, timeout=5.0):
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        await asyncio.sleep(0.005)


def _errors(got):
    return [o for o in got if o.kind == "error"]


def _worker_logs(caplog, text):
    return [r for r in caplog.records if r.name == "echomuse.speech" and text in r.getMessage()]


def test_one_stall_is_one_error_however_much_backlog_it_leaves_in_every_lane(caplog):
    """§8.1: one stall that makes every queued job miss its deadline, in every lane, reports
    each job's error but counts once: alone it cannot make the worker unavailable, and two
    later independent errors are needed to do so."""
    caplog.set_level(logging.DEBUG, logger="echomuse.speech")
    clock, got, changes = _Clock(), [], []

    def stall():
        if clock.t == 0.0:
            clock.t = 1.0                    # the first job to run is held past the deadline
        return ECHO

    def boom():
        raise RuntimeError("comparator failed")

    async def main():
        w = _fake_worker(clock, got, on_availability=changes.append, probe_interval_s=3600)
        try:
            for device, lease in (("office", "L1"), ("kitchen", "L2")):
                w.open_lease(device, 7, lease, "g")
                for i in range(4):
                    w.submit_echo(lease, i * 512, (i + 1) * 512, stall)
            await _until(lambda: len(_errors(got)) == 8)
            stalled = w.available
            w.submit_echo("L1", 2_048, 2_560, boom)
            await _until(lambda: len(_errors(got)) == 9)
            one_more = w.available
            w.submit_echo("L2", 2_048, 2_560, boom)
            await _until(lambda: changes == [False])
            return stalled, one_more
        finally:
            await w.close()

    stalled, one_more = asyncio.run(main())
    assert stalled and one_more
    deadline = [o for o in _errors(got) if "deadline" in o.payload.reason]
    assert len(deadline) == 8 and {o.device_id for o in deadline} == {"office", "kitchen"}
    counted = _worker_logs(caplog, "speech worker error")
    assert len(counted) == 3 and all(r.levelno == logging.WARNING for r in counted)
    assert "deadline" in counted[0].getMessage()
    assert len(_worker_logs(caplog, "not counted")) == 7


def test_sustained_overload_still_makes_the_worker_unavailable():
    """Each job that arrives after a counted stall and misses its deadline too is a new error
    (its own queued backlog is not), so a worker that stays too slow goes unavailable."""
    clock, got, changes = _Clock(), [], []

    def slow():
        clock.t += 1.0
        return ECHO

    async def main():
        w = _fake_worker(clock, got, on_availability=changes.append, probe_interval_s=3600)
        try:
            w.open_lease("office", 7, "L", "g")
            available = []
            for n in range(3):
                clock.t += 0.08                  # the next block arrives after the last failure
                w.submit_echo("L", 1_024 * n, 1_024 * n + 512, slow)
                w.submit_echo("L", 1_024 * n + 512, 1_024 * (n + 1), lambda: ECHO)   # its backlog
                await _until(lambda: len(_errors(got)) == 2 * (n + 1))
                available.append(w.available)
            return available
        finally:
            await w.close()

    assert asyncio.run(main()) == [True, True, False]
    assert changes == [False]


def test_independent_errors_make_the_worker_unavailable_and_the_transitions_are_logged(caplog):
    """Exceptions always count, and the worker itself logs each error (with its device) and both
    availability transitions, so they are visible after the turn that owned the lease is gone."""
    caplog.set_level(logging.INFO, logger="echomuse.speech")
    clock, got, changes = _Clock(), [], []

    def boom():
        raise RuntimeError("comparator failed")

    async def main():
        w = _fake_worker(clock, got, on_availability=changes.append, probe_interval_s=0.01)
        try:
            w.open_lease("office", 7, "L1", "g")
            w.open_lease("kitchen", 7, "L2", "g")
            for i, lease in enumerate(("L1", "L2", "L1")):
                clock.t += 20.0                  # three errors, all within 60 s
                w.submit_echo(lease, i * 512, (i + 1) * 512, boom)
                await _until(lambda: len(_errors(got)) == i + 1)
            await _until(lambda: changes == [False, True])
            return w.available
        finally:
            await w.close()

    assert asyncio.run(main())
    errors = _worker_logs(caplog, "speech worker error")
    assert [r.levelno for r in errors] == [logging.WARNING] * 3
    assert ["office" in r.getMessage() for r in errors] == [True, False, True]
    assert all("comparator failed" in r.getMessage() for r in errors)
    (down,) = _worker_logs(caplog, "unavailable")
    assert down.levelno == logging.WARNING
    assert all(s in down.getMessage() for s in ("3 errors", "office", "kitchen", "comparator failed"))
    (up,) = _worker_logs(caplog, "available again")
    assert up.levelno == logging.INFO
    assert caplog.records.index(down) < caplog.records.index(up)


def test_errors_further_apart_than_the_window_never_add_up():
    clock, got, changes = _Clock(), [], []

    def boom():
        raise RuntimeError("comparator failed")

    async def main():
        w = _fake_worker(clock, got, on_availability=changes.append, probe_interval_s=3600)
        try:
            w.open_lease("office", 7, "L", "g")
            for i in range(4):
                clock.t += 31.0                  # only two ever fall within 60 s
                w.submit_echo("L", i * 512, (i + 1) * 512, boom)
                await _until(lambda: len(_errors(got)) == i + 1)
            return w.available
        finally:
            await w.close()

    assert asyncio.run(main()) and changes == []



@pytest.fixture(scope="module")
def bundle_dir(tmp_path_factory):
    pytest.importorskip("onnxruntime")
    pytest.importorskip("sherpa_onnx")
    from em_speech_bundle import BundleError, verify_bundle
    configured = os.environ.get("SPEECH_BUNDLE_DIR")
    if configured:
        try:
            verify_bundle(configured)
            return Path(configured)
        except BundleError:
            pass
    sources = {k: Path(os.environ.get(k, v)) for k, v in DEFAULTS.items()}
    if not all(p.is_file() for p in sources.values()):
        pytest.skip("no speech bundle and no local artifacts to build one")
    spec = importlib.util.spec_from_file_location("fetch_speech_bundle", CONTROLLER / "tools/fetch_speech_bundle.py")
    fetch = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fetch)
    dest = tmp_path_factory.mktemp("speech")
    fetch.fetch(dest, wheel=str(sources["EM_TEST_SHERPA_WHEEL"]),
                archive=str(sources["EM_TEST_KROKO_ARCHIVE"]), vad=str(sources["EM_TEST_SILERO_V5"]))
    return dest


@pytest.fixture(scope="module")
def registry(tmp_path_factory, bundle_dir):
    import em_wake_registry as wr
    reg = wr.WakeRegistry(tmp_path_factory.mktemp("oww_models"), active_getter=lambda: wr.DEPLOYED_GRAPH_SHA256)
    reg.install_deployed(ROOT / "bcresnet_audio.onnx", ROOT / "bcresnet_audio.json")
    return reg


@pytest.fixture(scope="module")
def speech(bundle_dir):
    """The archive's test utterance as 16 kHz canonical int16 at native-AFE
    level (the evidence copy restores it to its recorded level)."""
    from scipy.signal import resample_poly
    from em_speech_bundle import read_qualification_wav, verify_bundle
    rate, x = read_qualification_wav(verify_bundle(bundle_dir, check_installed_package=False))
    assert rate == 24000
    y = resample_poly(x.astype(np.float64), 2, 3) / 10.0
    return np.clip(np.round(y * 32768), -32768, 32767).astype(np.int16)


def run(coro):
    return asyncio.run(coro)


async def drain(worker, until, timeout=30.0):
    """Collect observations until `until(list)` holds."""
    got = []
    deadline = time.monotonic() + timeout
    while not until(got):
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"timed out with {[(o.kind, o.through_sample) for o in got]}"
        got.append(await asyncio.wait_for(worker.observations.get(), remaining))
    return got


def test_bundle_verification_refuses_a_modified_file(bundle_dir, tmp_path):
    import shutil
    from em_speech_bundle import BundleError, verify_bundle
    copy = tmp_path / "b"
    shutil.copytree(bundle_dir, copy)
    (copy / "kroko/tokens.txt").write_text("tampered")
    with pytest.raises(BundleError, match="tokens"):
        verify_bundle(copy)
    (copy / "silero_vad_v5.onnx").unlink()
    with pytest.raises(BundleError):
        verify_bundle(copy)


def test_real_vad_and_streaming_asr_over_an_utterance(bundle_dir, registry, speech):
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            silence = np.zeros(16_000, np.int16)
            audio = np.concatenate([silence, speech, silence, silence])
            audio = audio[: audio.size // 1280 * 1280]
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            w.open_utterance("L", "U", 0, 0, SampleTimeline())
            for k in range(0, audio.size, 1280):
                w.submit_mic_block("L", k, audio[k:k + 1280])
                await asyncio.sleep(0)
            end = audio.size
            obs = await drain(w, lambda got: any(o.kind == "asr" and o.through_sample == end for o in got)
                              and any(o.kind == "vad" and o.through_sample >= end - 512 for o in got))
            return obs, end
        finally:
            await w.close()

    obs, end = run(main())
    assert not [o for o in obs if o.kind == "error"]
    vad = [o for o in obs if o.kind == "vad"]
    assert [o.payload.first_cell_sample for o in vad] == sorted(o.payload.first_cell_sample for o in vad)
    probs = np.concatenate([o.payload.probabilities for o in vad])
    assert probs.size == end // 512
    assert probs[: 16_000 // 512 - 2].max() < 0.2                     # leading digital silence
    speech_cells = probs[16_000 // 512 + 10: (16_000 + 80_000) // 512]
    assert speech_cells.max() > 0.8                                   # the VAD input reaches Silero
    asr = [o for o in obs if o.kind == "asr"]
    assert all(o.utterance_id == "U" for o in asr)
    assert [o.through_sample for o in asr] == sorted(o.through_sample for o in asr)
    final = asr[-1].payload
    assert "country" in final.text.lower()
    em = final.token_emission_sample
    assert list(em) == sorted(em) and em[0] >= 16_000 and em[-1] <= 16_000 + speech.size + 8_000
    # 2 s of trailing zeros: blank frames accumulate at ~25/s, 40 ms each
    assert final.trailing_blank_frames >= 20
    assert final.stable_prefix is not None and final.stable_prefix.split()[0] == "ask"


def test_finalize_at_each_pause_carries_the_last_word_before_the_live_chunk(bundle_dir, registry, speech):
    """[1 s zeros, clip, 1 s zeros, clip, 3 s zeros]: the clip's own sentence pause
    is shorter than 320 ms, so exactly the two real pauses are finalized."""
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            silence = np.zeros(16_000, np.int16)
            audio = np.concatenate([silence, speech, silence, speech, silence, silence, silence])
            audio = audio[: audio.size // 1280 * 1280]
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            w.open_utterance("L", "U", 0, 0, SampleTimeline())
            for k in range(0, audio.size, 1280):
                w.submit_mic_block("L", k, audio[k:k + 1280])
                await asyncio.sleep(0)
            return await drain(w, lambda got: any(o.kind == "asr" and o.through_sample == audio.size for o in got))
        finally:
            await w.close()

    obs = run(main())
    assert not [o for o in obs if o.kind == "error"]
    probs = np.concatenate([o.payload.probabilities for o in obs if o.kind == "vad"])
    positive = np.nonzero(probs >= 0.50)[0]
    pauses = [(i + 1) * 512 for i, j in zip(positive, [*positive[1:], 10**9]) if j - i > 10]
    asr = [o for o in obs if o.kind == "asr"]
    finals = [i for i, o in enumerate(asr) if o.payload.finalized]
    assert len(pauses) == 2 and len(finals) == 2
    for speech_end, i in zip(pauses, finals):
        assert 0 <= asr[i].through_sample - (speech_end + sw.FINALIZE_PAUSE) < sw.BLOCK_SAMPLES
        assert asr[i].payload.text.lower().endswith("your country")
        # Each finalize carries the lane's Silero state there: the utterance's voice snapshot.
        voice = asr[i].payload.voice
        assert voice.sample == asr[i].through_sample and voice.state.shape == sw.VAD_STATE_SHAPE
        assert voice.context.shape == (sw.VAD_CONTEXT_SAMPLES,) and np.any(voice.state)
    assert all(o.payload.voice is None for i, o in enumerate(asr) if i not in finals)

    first = asr[finals[0]]
    assert not asr[finals[0] - 1].payload.text.lower().endswith("your country")   # live still lacks it
    assert first.through_sample < 23_040 + 3 * 20_480        # before the live stream's next chunk edge
    # Held while the live tokens are a prefix of it; blanks count real audio since the
    # latest speech-positive cell (Kroko emits the last word late), never the flush.
    held = [first]
    for o in asr[finals[0] + 1:]:
        if o.payload.tokens != first.payload.tokens:
            break
        held.append(o)
    assert len(held) > 1 and not any(o.payload.finalized for o in held[1:])
    ends = (positive + 1) * 512
    for o in held:
        speech_end = ends[ends <= o.through_sample].max()
        assert o.payload.trailing_blank_frames == (o.through_sample - speech_end) // sw.BLANK_FRAME_SAMPLES
    assert first.payload.trailing_blank_frames >= sw.FINALIZE_PAUSE // sw.BLANK_FRAME_SAMPLES
    assert min(o.payload.trailing_blank_frames for o in held) < first.payload.trailing_blank_frames  # new speech
    # The live stream resumes once its own chunk adds tokens: the second clip's first words.
    resumed = asr[finals[0] + len(held)]
    assert resumed.through_sample < pauses[1]
    assert resumed.payload.tokens[:len(first.payload.tokens)] == first.payload.tokens
    assert len(resumed.payload.tokens) > len(first.payload.tokens)


def two_pauses(registry, bundle_dir, speech, server):
    """[1 s zeros, clip, 1 s zeros, clip, 3 s zeros] through a worker with a pause server,
    fed at twice real time so a server's answer lands blocks after the pause."""
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            silence = np.zeros(16_000, np.int16)
            audio = np.concatenate([silence, speech, silence, speech, silence, silence, silence])
            audio = audio[: audio.size // 1280 * 1280]
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            w.open_utterance("L", "U", 0, 0, SampleTimeline(), pause_server=server)
            for k in range(0, audio.size, 1280):
                w.submit_mic_block("L", k, audio[k:k + 1280])
                await asyncio.sleep(0.04)
            return await drain(w, lambda got: any(o.kind == "asr" and o.through_sample == audio.size for o in got))
        finally:
            await w.close()
    return run(main())


def test_the_pause_servers_words_follow_kroko_without_holding_it(bundle_dir, registry, speech):
    """Each pause sends the evidence copy so far to the server. Kroko's finalized result is
    reported without waiting for it (an answer already in by then rides it); the server's
    transcript rides the results from its answer on, while the tokens stay Kroko's, until
    new speech replaces the finalized result."""
    words = "Ask not what your country can do for you."
    with FakeWyoming(words, delay=0.2) as fake:
        obs = two_pauses(registry, bundle_dir, speech, WyomingServer(fake.host, fake.port, "", "en"))
    assert not [o for o in obs if o.kind == "error"]
    asr = [o for o in obs if o.kind == "asr"]
    finals = [i for i, o in enumerate(asr) if o.payload.finalized]
    answers = [i for i, o in enumerate(asr) if o.payload.pause_decode is not None]
    assert len(finals) == 2 and len(answers) == 2 and len(fake.requests) == 2
    for final, answer, request in zip(finals, answers, fake.requests):
        assert asr[final].payload.text.lower().endswith("your country")      # Kroko's own result
        decode = asr[answer].payload.pause_decode
        assert answer >= final and decode.through == asr[final].through_sample
        assert (decode.text, decode.error) == (words, None) and decode.ms >= 200
        sent = b"".join(payload for kind, _, payload in request if kind == "audio-chunk")
        assert len(sent) == 2 * decode.through                               # [0, through) of the utterance
        held = [o for o in asr[answer:] if o.payload.tokens == asr[final].payload.tokens]
        assert held and all(o.payload.pause_text == words for o in held)
    resumed = next(o for o in asr[answers[0]:] if o.payload.tokens != asr[finals[0]].payload.tokens)
    assert resumed.payload.pause_text is None                               # new speech: Kroko's text again


def test_a_silent_pause_server_never_holds_kroko(bundle_dir, registry, speech, monkeypatch):
    monkeypatch.setattr(sw, "PAUSE_REMOTE_TIMEOUT_S", 1.0)
    with FakeWyoming(hang=True) as fake:
        obs = two_pauses(registry, bundle_dir, speech, WyomingServer(fake.host, fake.port, "", "en"))
    assert not [o for o in obs if o.kind == "error"]
    asr = [o for o in obs if o.kind == "asr"]
    finals = [i for i, o in enumerate(asr) if o.payload.finalized]
    assert len(finals) == 2
    for i in finals:
        assert asr[i].payload.text.lower().endswith("your country")
        assert asr[i].computed_at_ms - asr[i - 1].computed_at_ms < 600        # not the 1 s timeout
    assert all(o.payload.pause_text is None for o in asr)
    decodes = [o.payload.pause_decode for o in asr if o.payload.pause_decode is not None]
    assert decodes and all(d.text is None and d.error in ("timeout", "superseded") for d in decodes)
    assert decodes[-1].error == "timeout" and decodes[-1].ms >= 1_000


def test_the_flush_reaches_speech_just_past_a_chunk_edge(bundle_dir, registry, speech):
    """A 1.43 s window ends 150 ms past Kroko's first 1.28 s chunk: a 0.5 s flush
    left it at "Ask not what your"; the flush must decode through "country"."""
    window = speech[:22_880]

    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            tokens, _ = await w.decode_span(window)
            mic = SampleTimeline()
            mic.write(0, np.concatenate([np.zeros(16_000, np.int16), speech]))
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            # Verification input [support_start − 300 ms, open + 480 ms) is exactly the window.
            end = 16_000 + window.size
            w.submit_verification("L", "C", mic, 16_000 + sw.VERIFICATION_PREROLL, end - sw.VERIFICATION_LOOKAHEAD,
                                  "country")
            got = await drain(w, lambda got: any(o.kind == "verification" for o in got))
            return tokens, next(o for o in got if o.kind == "verification")
        finally:
            await w.close()

    tokens, verification = run(main())
    assert "country" in "".join(tokens).lower()
    assert verification.payload.result == "pass" and "country" in verification.payload.text.lower()


def test_verification_decodes_candidate_window_and_matches_the_core(bundle_dir, registry, speech):
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            mic = SampleTimeline()
            mic.write(0, np.concatenate([np.zeros(16_000, np.int16), speech]))
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            # "ask not what your country ..." begins ~0.5 s into the clip.
            support_start, open_sample = 16_000 + 4_800, 16_000 + 40_000
            w.submit_verification("L", "C1", mic, support_start, open_sample, "country")
            w.submit_verification("L", "C2", mic, support_start, open_sample, "ophel")
            return await drain(w, lambda got: sum(o.kind == "verification" for o in got) == 2)
        finally:
            await w.close()

    got = {o.payload.candidate_id: o for o in run(main()) if o.kind == "verification"}
    assert got["C1"].payload.result == "pass" and got["C1"].payload.alias_distance <= 0.4
    assert got["C2"].payload.result == "fail"
    assert got["C1"].through_sample == 16_000 + 40_000 + 7_680     # flush zeros are not counted
    assert "country" in got["C1"].payload.text.lower()


def test_verification_needs_the_whole_lookahead(bundle_dir, registry):
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            mic = SampleTimeline()
            mic.write(0, np.zeros(20_000, np.int16))
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            with pytest.raises(sw.SpeechWorkerError, match="not all known"):
                w.submit_verification("L", "C", mic, 10_000, 15_000, "ophel")
        finally:
            await w.close()
    run(main())


def test_reference_scoring_on_its_own_hop_grid_skips_digital_silence(bundle_dir, registry):
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=60.0)
        await w.start()
        try:
            ref = ReferenceTimeline()
            rng = np.random.default_rng(3)
            ref.write(9, 1000, None, 30_000)                                   # digital silence
            noise = (rng.normal(0, 2000, 30_000)).astype(np.int16)
            ref.write(9, 31_000, noise, noise.size)
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)
            w.submit_reference("L", ref.segment(9))
            return await drain(w, lambda got: any(o.kind == "reference_score" for o in got))
        finally:
            await w.close()

    (obs,) = [o for o in run(main()) if o.kind == "reference_score"]
    p = obs.payload
    ends = [p.first_hop_end_sample + 2560 * i for i in range(len(p.raw))]
    assert p.first_hop_end_sample == 25_600                  # first multiple of 2,560 ≥ 1,000 + 22,400
    assert ends[-1] <= 61_000 and ends[-1] + 2560 > 61_000
    assert obs.through_sample == ends[-1] and obs.stream == "reference" and p.reference_epoch == 9
    silent = [e for e in ends if e <= 31_000]
    assert all(r is None for r, e in zip(p.raw, ends) if e in silent)       # unscored: unavailable, not 0
    scored = [r for r, e in zip(p.raw, ends) if e - 22_400 >= 31_000]
    assert scored and all(r is not None and r < 0.30 for r in scored)
    assert p.candidates == ()
    assert obs.model_revision == registry.active().graph_sha256


def test_errors_mark_unavailable_and_two_good_probes_recover(bundle_dir, registry):
    async def main():
        changes = []
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), probe_interval_s=0.05,
                            on_availability=changes.append)
        await w.start()
        try:
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)

            def boom():
                raise RuntimeError("comparator failed")
            for i in range(3):
                w.submit_echo("L", i * 512, (i + 1) * 512, boom, utterance_id="U")
            errors = await drain(w, lambda got: sum(o.kind == "error" for o in got) == 3)
            assert await asyncio.wait_for(w.availability.get(), 5) is False
            assert w.available is False
            assert await asyncio.wait_for(w.availability.get(), 10) is True
            return errors, changes, w.available
        finally:
            await w.close()

    errors, changes, available = run(main())
    e = [o for o in errors if o.kind == "error"]
    assert all(o.payload.component == "echo" and "comparator failed" in o.payload.reason for o in e)
    assert [o.through_sample for o in e] == [512, 1024, 1536] and all(o.utterance_id == "U" for o in e)
    assert changes == [False, True] and available


def test_job_older_than_its_deadline_is_an_error(bundle_dir, registry):
    async def main():
        w = sw.SpeechWorker(registry, bundle_dir=str(bundle_dir), job_deadline_s=0.05)
        await w.start()
        try:
            w.open_lease("dev", 7, "L", registry.active().graph_sha256)

            def slow():
                time.sleep(0.2)
                return (EchoResult.ECHO_ONLY,), 12
            w.submit_echo("L", 0, 512, slow)
            return await drain(w, lambda got: bool(got))
        finally:
            await w.close()

    (obs,) = run(main())
    assert obs.kind == "error" and "deadline" in obs.payload.reason


@pytest.mark.skipif(sys.platform != "linux", reason="host-only timing")
def test_bundle_qualification_trailing_blank_slope(bundle_dir):
    from em_speech_bundle import qualify_bundle, verify_bundle
    q = qualify_bundle(verify_bundle(bundle_dir, check_installed_package=False))
    assert 22 <= q.blank_slope_per_s <= 27
    assert "country" in q.text.lower()
