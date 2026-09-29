"""Speech worker with the real pinned bundle (§8.1, §16.6).

The bundle comes from `SPEECH_BUNDLE_DIR` when it points at a verified bundle;
otherwise it is assembled with `tools/fetch_speech_bundle.py` from local copies
named by `EM_TEST_SHERPA_WHEEL`, `EM_TEST_KROKO_ARCHIVE`, `EM_TEST_SILERO_V5`
(defaults: the evidence copies under /tmp). Tests skip when neither exists.
"""

import asyncio
import importlib.util
import os
import sys
import time
from pathlib import Path

import numpy as np
import pytest

import em_speech_worker as sw
from em_audio_timeline import ReferenceTimeline, SampleTimeline, StreamId

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
    assert speech_cells.max() > 0.8                                   # evidence copy reaches Silero
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
                return ("echo_only",), 12
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
