"""BCResNet preparation, smoothing, reference candidates (§5.1, §5.2), and the
deployed fixture's probe scores (§5.4)."""

import json
from pathlib import Path

import numpy as np
import pytest

import em_wake_scorer as ws

ROOT = Path(__file__).resolve().parents[2]
SIDECAR = {
    "type": "bcresnet", "sampleRate": 16000, "window": 22400,
    "labels": ["noise", "ohphelia", "unknown"], "wakeIndex": 1,
    "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8,
}
SPEC = ws.parse_spec(SIDECAR)
N = SPEC.window


class Fixed:
    """Infer stub returning queued logits and recording the windows it saw."""

    def __init__(self, *probs):
        self.probs = list(probs)
        self.windows = []

    def __call__(self, x):
        self.windows.append(x.copy())
        p = self.probs.pop(0) if len(self.probs) > 1 else self.probs[0]
        # logits whose softmax puts `p` on index 1 and splits the rest.
        rest = (1 - p) / 2
        return np.log(np.array([[rest, p, rest]], dtype=np.float64)).astype(np.float32)


def loud(level=3000):
    return np.full(N, level, dtype=np.int16)


# -- sidecar ----------------------------------------------------------------

def test_deployed_sidecar_parses_with_wake_index_one():
    spec = ws.load_spec(ROOT / "bcresnet_audio.onnx")
    assert spec.wake_label == "ohphelia" and spec.wake_index == 1 and spec.window == 22400


@pytest.mark.parametrize("field, value", [
    ("type", "oww"), ("labels", []), ("wakeIndex", 3), ("wakeIndex", True),
    ("window", 16000), ("sampleRate", 48000), ("normPeak", 0.9), ("clipSeconds", 1.0),
    ("nMels", 0),
])
def test_sidecar_violations_are_refused(field, value):
    with pytest.raises(ValueError):
        ws.parse_spec({**SIDECAR, field: value})


def test_missing_sidecar_is_named(tmp_path):
    with pytest.raises(FileNotFoundError, match="wake.json"):
        ws.load_spec(tmp_path / "wake.onnx")


# -- §5.1 preparation -----------------------------------------------------------

def test_below_rms_floor_is_not_scored_and_leaves_history():
    infer = Fixed(0.8)
    s = ws.BcresnetScorer(infer, SPEC)
    first = s.score_window(2560 * 9, loud())
    quiet = np.zeros(N, dtype=np.int16)
    quiet[0] = 3           # peak above 1e-4 but RMS far below it
    hop = s.score_window(2560 * 10, quiet)
    assert hop.raw is None and hop.smoothed is None
    assert s.history == ((2560 * 9, pytest.approx(0.8)),)
    assert len(infer.windows) == 1 and first.smoothed == pytest.approx(0.8)


def test_window_is_peak_normalized_to_0_8_from_int16_over_32768():
    infer = Fixed(0.1)
    pcm = np.zeros(N, dtype=np.int16)
    pcm[::7] = 1000
    pcm[5] = -2000
    ws.BcresnetScorer(infer, SPEC).score_window(25600, pcm)
    x = infer.windows[0][0]
    assert x.dtype == np.float32 and x.shape == (N,)
    assert np.abs(x).max() == pytest.approx(0.8)
    assert x[5] == pytest.approx(-0.8) and x[0] == pytest.approx(0.4)


def test_probability_is_softmax_of_the_wake_logit():
    logits = np.array([[1.0, 2.0, 3.0]], dtype=np.float32)
    p = ws.wake_probability(lambda x: logits, SPEC, loud())
    e = np.exp(logits[0] - logits[0].max())
    assert p == pytest.approx(e[1] / e.sum())


def test_logit_count_must_match_labels_and_be_finite():
    with pytest.raises(ValueError, match="expected"):
        ws.wake_probability(lambda x: np.zeros((1, 2), np.float32), SPEC, loud())
    with pytest.raises(ValueError, match="non-finite"):
        ws.wake_probability(lambda x: np.array([[0, np.nan, 0]], np.float32), SPEC, loud())


def test_window_must_be_int16_of_exact_length():
    with pytest.raises(ValueError):
        ws.prepare_window(np.zeros(N - 1, np.int16), SPEC)
    with pytest.raises(ValueError):
        ws.prepare_window(np.zeros(N, np.float32), SPEC)


# -- §5.2 smoothing ---------------------------------------------------------------

def test_smoothed_is_mean_of_up_to_three_scored_values():
    s = ws.BcresnetScorer(Fixed(0.3, 0.6, 0.9, 0.001), SPEC)
    got = [s.score_window(2560 * k, loud()).smoothed for k in range(9, 13)]
    assert got == pytest.approx([0.3, 0.45, 0.6, 0.5003333])


def test_six_unscored_hops_clear_history_five_do_not():
    s = ws.BcresnetScorer(Fixed(0.9), SPEC)
    s.score_window(2560 * 9, loud())
    for k in range(5):
        s.score_window(2560 * (10 + k), loud(), digital_silence=True)
    assert len(s.history) == 1
    s.score_window(2560 * 15, loud(), digital_silence=True)
    assert s.history == ()


def test_hop_end_must_lie_on_the_2560_grid():
    with pytest.raises(ValueError, match="multiple"):
        ws.BcresnetScorer(Fixed(0.1), SPEC).score_window(23000, loud())


# -- reference candidates (threshold 0.30) --------------------------------------------

def run_detector(probs):
    scorer = ws.BcresnetScorer(Fixed(*probs), SPEC)
    det = ws.ReferenceDetector(scorer, 0.30)
    events = []
    for k in range(len(probs)):
        events.extend(det.push(scorer.score_window(2560 * (9 + k), loud())))
    return events


def test_candidate_opens_at_first_smoothed_crossing_with_support_from_oldest_window():
    events = run_detector([0.1, 0.2, 0.7])     # smoothed 0.1, 0.15, 0.333
    assert len(events) == 1
    c = events[0]
    assert c.close_reason is None
    assert c.first_crossing_end == 2560 * 11
    assert c.support_start == 2560 * 9 - N
    assert c.support_end == 2560 * 11


def test_candidate_closes_after_two_values_below_half_threshold():
    events = run_detector([0.9, 0.9, 0.001, 0.001, 0.001, 0.001, 0.001])
    # smoothed: .9 .9 .6 .3 0 0 0 → below 0.15 twice at the 6th/7th hops
    assert [e.close_reason for e in events] == [None, "below"]
    assert events[1].support_end == 2560 * 10
    assert events[1].peak_smoothed == pytest.approx(0.9)


def test_gap_window_closes_an_open_candidate():
    scorer = ws.BcresnetScorer(Fixed(0.9), SPEC)
    det = ws.ReferenceDetector(scorer, 0.30)
    det.push(scorer.score_window(23040, loud()))
    events = det.push(scorer.invalid(25600), invalid=True)
    assert [e.close_reason for e in events] == ["gap"] and scorer.history == ()


# -- deployed graph (§5.4 probe) ---------------------------------------------------------

def test_deployed_fixture_scores_probe_signals_in_the_measured_range():
    pytest.importorskip("onnxruntime")
    graph = ROOT / "bcresnet_audio.onnx"
    infer, _ = ws.onnx_infer(graph)
    scores = ws.probe_model(infer, ws.load_spec(graph))
    assert set(scores) == {"silence", "noise", "tone"}
    for value in scores.values():
        assert 0.0033 - 5e-5 <= value <= 0.0106 + 5e-5
    ws.validate_probe(scores, 0.17)


def test_onnx_infer_refuses_sidecar_with_wrong_label_count(tmp_path):
    pytest.importorskip("onnxruntime")
    graph = tmp_path / "g.onnx"
    graph.write_bytes((ROOT / "bcresnet_audio.onnx").read_bytes())
    (tmp_path / "g.json").write_text(json.dumps({**SIDECAR, "labels": ["a", "b"], "wakeIndex": 1}))
    with pytest.raises(ValueError, match="labels"):
        ws.onnx_infer(graph)
