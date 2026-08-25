"""
em_wake_scorer — the buffering, not the model.

BcresnetScorer takes an injected `infer`, so everything interesting about it is
testable with no onnxruntime and no .onnx file: when a score appears, when it
does not, what gets normalized, and what a reset actually discards. That is the
whole reason the seam has that shape.
"""

import json

import numpy as np
import pytest

import em_wake_scorer as ws


SPEC = ws.BcresnetSpec(
    window=22400,
    sample_rate=16000,
    labels=("noise", "ohphelia", "unknown"),
    wake_index=1,
    norm_peak=0.8,
    n_mels=40,
    clip_seconds=1.4,
)

CHUNK = ws.CHUNK_SAMPLES
CHUNKS_PER_WINDOW = SPEC.window // CHUNK          # 17.5 -> not integral, see below


def chunk(level=3000, n=CHUNK):
    """A chunk of steady-amplitude audio, loud enough to clear the RMS floor."""
    return np.full(n, level, dtype=np.int16)


class RecordingInfer:
    """Captures every window it is asked to score, and returns fixed logits."""

    def __init__(self, logits=(0.0, 5.0, 0.0)):
        self.logits = np.array([logits], dtype=np.float32)
        self.windows = []

    def __call__(self, x):
        self.windows.append(np.array(x, copy=True))
        return self.logits


def feed(scorer, n_chunks, level=3000):
    return [scorer.push(chunk(level)) for _ in range(n_chunks)]


# -- sidecar parsing -------------------------------------------------------

def test_parse_spec_accepts_the_exporters_output():
    raw = {
        "type": "bcresnet", "sampleRate": 16000, "window": 22400,
        "labels": ["noise", "ohphelia", "unknown"], "wakeIndex": 1,
        "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8,
    }
    spec = ws.parse_spec(raw)
    assert spec.wake_index == 1
    assert spec.wake_label == "ohphelia"
    assert spec.window == 22400


def test_wake_index_is_read_not_assumed():
    """The whole reason the sidecar exists.

    Alphabetical class ordering puts the wake word at 1 for this model and at 0
    for "hey tara". A scorer that defaulted to 0 would score `noise` and look
    like a detector that simply never fires.
    """
    spec = ws.parse_spec({
        "type": "bcresnet", "window": 22400,
        "labels": ["noise", "ohphelia", "unknown"], "wakeIndex": 1,
    })
    assert spec.wake_index == 1 and spec.labels[spec.wake_index] == "ohphelia"


@pytest.mark.parametrize("raw, needle", [
    ({"type": "oww", "window": 22400, "labels": ["a"], "wakeIndex": 0}, "type"),
    ({"type": "bcresnet", "window": 22400, "labels": [], "wakeIndex": 0}, "labels"),
    ({"type": "bcresnet", "window": 22400, "labels": ["a"], "wakeIndex": 3}, "wakeIndex"),
    ({"type": "bcresnet", "window": 22400, "labels": ["a"], "wakeIndex": True}, "wakeIndex"),
    ({"type": "bcresnet", "window": 0, "labels": ["a"], "wakeIndex": 0}, "window"),
    ({"type": "bcresnet", "window": 22400, "labels": ["a"], "wakeIndex": 0,
      "sampleRate": 48000}, "sampleRate"),
    ({"type": "bcresnet", "window": 22400, "labels": ["a"], "wakeIndex": 0,
      "normPeak": 0}, "normPeak"),
])
def test_parse_spec_refuses_rather_than_guesses(raw, needle):
    with pytest.raises(ValueError, match=needle):
        ws.parse_spec(raw)


def test_load_spec_names_the_missing_file(tmp_path):
    model = tmp_path / "wake.onnx"
    model.write_bytes(b"not really onnx")
    with pytest.raises(FileNotFoundError, match="wake.json"):
        ws.load_spec(model)


def test_load_spec_reads_the_sidecar_beside_the_model(tmp_path):
    model = tmp_path / "wake.onnx"
    model.write_bytes(b"")
    (tmp_path / "wake.json").write_text(json.dumps({
        "type": "bcresnet", "window": 22400,
        "labels": ["noise", "w", "unknown"], "wakeIndex": 1,
    }))
    assert ws.load_spec(model).wake_label == "w"


def test_is_bcresnet_model_decides_on_the_sidecar_not_the_name(tmp_path):
    """A model with no sidecar is openWakeWord — which is also the right answer
    for every model that predates this feature."""
    plain = tmp_path / "bcresnet.onnx"      # named like one, and is not one
    plain.write_bytes(b"")
    assert not ws.is_bcresnet_model(str(plain))
    (tmp_path / "bcresnet.json").write_text("{}")
    assert ws.is_bcresnet_model(str(plain))
    assert not ws.is_bcresnet_model("alexa_v0.1")   # a bundled oWW name
    assert not ws.is_bcresnet_model("")


# -- hop and readiness -----------------------------------------------------

def test_no_score_until_a_full_window_exists():
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    # 22400 / 1280 = 17.5, so the 18th chunk is the first that fills the ring.
    out = feed(s, 17)
    assert out == [None] * 17
    assert not s.ready
    assert inf.windows == []
    # softmax((0, 5, 0))[1] = e^5 / (2 + e^5)
    assert s.push(chunk()) == pytest.approx(0.98670, abs=1e-4)
    assert s.ready


def test_hop_scores_every_nth_chunk_after_the_first_window():
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=3)
    feed(s, 17)                       # ring not yet full
    assert s.push(chunk()) is not None     # scores the moment it can
    assert s.push(chunk()) is None
    assert s.push(chunk()) is None
    assert s.push(chunk()) is not None     # ...then every 3rd
    assert len(inf.windows) == 2


def test_hop_chunks_1_scores_every_chunk():
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    feed(s, 18)
    assert all(v is not None for v in feed(s, 5))


@pytest.mark.parametrize("bad", [0, -1])
def test_bad_hop_is_refused_at_construction(bad):
    with pytest.raises(ValueError, match="hop_chunks"):
        ws.BcresnetScorer(RecordingInfer(), SPEC, hop_chunks=bad)


# -- what the model actually sees -----------------------------------------

def test_window_is_the_most_recent_audio_in_order():
    """The ring must slide, not shuffle: the newest chunk ends up at the end."""
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    for i in range(1, 19):
        s.push(np.full(CHUNK, i * 100, dtype=np.int16))
    w = inf.windows[0][0]
    assert w[-1] > w[0]                       # newest at the end
    assert np.all(np.diff(w[::CHUNK]) >= 0)   # monotonically newer


def test_window_is_peak_normalized_to_norm_peak():
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    feed(s, 18, level=1000)
    assert np.abs(inf.windows[0]).max() == pytest.approx(SPEC.norm_peak, abs=1e-4)


def test_normalization_is_level_invariant():
    """Quiet and loud audio of the same shape must reach the model identically —
    this is what the model's level invariance depends on."""
    quiet, loud = RecordingInfer(), RecordingInfer()
    a = ws.BcresnetScorer(quiet, SPEC, hop_chunks=1)
    b = ws.BcresnetScorer(loud, SPEC, hop_chunks=1)
    for i in range(18):
        a.push(np.full(CHUNK, 300, dtype=np.int16))
        b.push(np.full(CHUNK, 12000, dtype=np.int16))
    assert np.allclose(quiet.windows[0], loud.windows[0], atol=1e-3)


def test_near_silence_is_not_scored_at_all():
    """A window peaking just above PEAK_EPS would be rescaled ~8000x into
    full-scale noise. Not scoring it is the point; returning 0.0 would be a
    claim about audio nobody judged."""
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    out = feed(s, 25, level=1)
    assert out == [None] * 25
    assert inf.windows == []


def test_a_muted_device_streams_zeros_and_is_never_scored():
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    assert feed(s, 25, level=0) == [None] * 25
    assert inf.windows == []


# -- softmax and label selection ------------------------------------------

def test_score_is_the_softmax_of_the_wake_logit():
    logits = (1.0, 2.0, 3.0)
    s = ws.BcresnetScorer(RecordingInfer(logits), SPEC, hop_chunks=1, smoothing=1)
    feed(s, 17)
    expected = ws.softmax(np.array(logits))[SPEC.wake_index]
    assert s.push(chunk()) == pytest.approx(expected)


def test_scoring_the_wrong_index_is_a_different_answer():
    """Guards the sidecar's whole purpose: with these logits, index 0 and
    index 1 differ by a factor of ~150."""
    logits = (0.0, 5.0, 0.0)
    other = ws.BcresnetSpec(**{**SPEC.__dict__, "wake_index": 0})
    a = ws.BcresnetScorer(RecordingInfer(logits), SPEC, hop_chunks=1, smoothing=1)
    b = ws.BcresnetScorer(RecordingInfer(logits), other, hop_chunks=1, smoothing=1)
    feed(a, 17); feed(b, 17)
    assert a.push(chunk()) > 0.9
    assert b.push(chunk()) < 0.01


def test_logit_count_must_match_the_sidecars_labels():
    """A mismatched .onnx/.json pair is caught rather than scored."""
    s = ws.BcresnetScorer(RecordingInfer((1.0, 2.0)), SPEC, hop_chunks=1)
    feed(s, 17)
    with pytest.raises(ValueError, match="2 logits"):
        s.push(chunk())


def test_softmax_is_stable_on_large_logits():
    assert ws.softmax(np.array([1000.0, 1001.0])).sum() == pytest.approx(1.0)
    assert np.all(np.isfinite(ws.softmax(np.array([1e4, -1e4]))))


# -- smoothing -------------------------------------------------------------

def test_smoothing_averages_the_last_n_window_scores():
    class Ramp:
        def __init__(self): self.n = 0
        def __call__(self, x):
            self.n += 1
            # p_wake climbs 0 -> ~1 as the wake logit rises
            return np.array([[0.0, float(self.n), 0.0]], dtype=np.float32)

    s = ws.BcresnetScorer(Ramp(), SPEC, hop_chunks=1, smoothing=3)
    feed(s, 17)
    scores = [s.push(chunk()) for _ in range(4)]
    assert scores[0] < scores[-1]                 # follows the ramp
    assert all(x is not None for x in scores)


def test_smoothing_1_is_the_raw_score():
    logits = (0.0, 5.0, 0.0)
    s = ws.BcresnetScorer(RecordingInfer(logits), SPEC, hop_chunks=1, smoothing=1)
    feed(s, 17)
    expected = ws.softmax(np.array(logits))[1]
    assert s.push(chunk()) == pytest.approx(expected)
    assert s.push(chunk()) == pytest.approx(expected)


# -- reset -----------------------------------------------------------------

def test_reset_discards_the_ring_so_a_full_window_must_be_rebuilt():
    """This is also the refractory.

    Every controller path that acts on a wake calls reset(), and a reset means
    the next score cannot arrive until 1.4s of fresh audio has been collected —
    so one utterance cannot fire eight overlapping windows. Weakening this
    silently removes the refractory.
    """
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1)
    feed(s, 18)
    assert s.ready and len(inf.windows) == 1
    s.reset()
    assert not s.ready
    assert feed(s, 17) == [None] * 17
    assert len(inf.windows) == 1                 # nothing scored while refilling
    assert s.push(chunk()) is not None


def test_reset_clears_the_smoothing_history():
    """Otherwise a post-turn score is an average with pre-turn audio in it."""
    high = ws.BcresnetScorer(RecordingInfer((0.0, 9.0, 0.0)), SPEC,
                             hop_chunks=1, smoothing=3)
    feed(high, 20)                                # smoothing full of ~1.0
    high.reset()
    high._infer = RecordingInfer((9.0, 0.0, 0.0))  # now scores ~0
    feed(high, 17)
    assert high.push(chunk()) < 0.01              # no memory of the old scores


def test_reset_before_any_audio_is_harmless():
    s = ws.BcresnetScorer(RecordingInfer(), SPEC, hop_chunks=1)
    s.reset(); s.reset()
    assert not s.ready


# -- OwwScorer -------------------------------------------------------------

class FakeOww:
    def __init__(self, scores): self.scores = scores; self.resets = 0
    def predict(self, samples): return self.scores
    def reset(self): self.resets += 1


def test_oww_scorer_reads_the_prediction_key():
    s = ws.OwwScorer(FakeOww({"hey_jarvis": 0.42}), "hey_jarvis")
    assert s.push(chunk()) == pytest.approx(0.42)


def test_oww_scorer_missing_key_is_zero_not_none():
    """openWakeWord scores every chunk, so a float is always the honest answer —
    None means 'not judged' and would skip the near-miss counters."""
    assert ws.OwwScorer(FakeOww({"other": 0.9}), "mine").push(chunk()) == 0.0


def test_oww_scorer_forwards_reset():
    m = FakeOww({"k": 0.0})
    ws.OwwScorer(m, "k").reset()
    assert m.resets == 1


# -- upload classification -------------------------------------------------

@pytest.mark.parametrize("shape, expect", [
    ([1, 16, 96],              ws.TYPE_OWW),
    (["batch", 22400],         ws.TYPE_BCRESNET),
    ([1, 22400],               ws.TYPE_BCRESNET),
    (["batch", 1, 40, 141],    ws.TYPE_BCRESNET_FEATURES),
    ([1, 1, 40, 141],          ws.TYPE_BCRESNET_FEATURES),
    ([1, 76, 32, 1],           ""),          # oWW embedding model, not a head
    ([],                       ""),
    (None,                     ""),
])
def test_classify_model_shape(shape, expect):
    assert ws.classify_model_shape(shape) == expect


def test_features_in_export_is_named_not_lumped_with_unknown():
    """train_qc.py's DEFAULT mode writes this graph, to a file called
    bcresnet.onnx. Reporting it as 'unrecognised' sends someone looking at the
    controller when the fix is --onnx_input audio in the training repo."""
    assert ws.classify_model_shape(["batch", 1, 40, 141]) == ws.TYPE_BCRESNET_FEATURES
    assert ws.TYPE_BCRESNET_FEATURES != ws.TYPE_BCRESNET


def test_an_empty_chunk_changes_nothing():
    """No audio in, no state change.

    Without the guard an empty chunk still advances the hop counter, so the
    next real chunk scores early — the hop would depend on how often push()
    was called rather than on how much audio arrived. Pinned at both ends:
    the device's bcresnet.Detector carries the same guard, and the two
    scorers must not disagree about what counts as a frame.
    """
    inf = RecordingInfer()
    s = ws.BcresnetScorer(inf, SPEC, hop_chunks=1, smoothing=1)
    feed(s, CHUNKS_PER_WINDOW + 1)
    n = len(inf.windows)
    assert n > 0

    for empty in (np.zeros(0, dtype=np.int16), np.array([], dtype=np.int16)):
        assert s.push(empty) is None
    assert len(inf.windows) == n, "an empty chunk ran inference"
