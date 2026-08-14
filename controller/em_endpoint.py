"""End-of-utterance detection that survives a talking television.

Split out of `em_esphome._stream_mic_audio` for the reason `em_turnclock`,
`em_button.decide` and `em_linkauth.decide` were: this is a state machine with
two trackers, a voting window and three time limits, and it decides when to
stop listening to somebody. It needs tests, and the suite does not import
em_esphome.

## The problem this exists for

For a wake-word turn EchoMuse has no endpointer of its own. The device's VAD
gate applies only to `lock_mic` (button) turns — the always-on wake stream is
ungated by design — so no device sentinel is ever sent, and end-of-turn is
decided entirely by Home Assistant's `STT_VAD_END`.

HA's VAD asks "is there speech?". With a television on there always is, so it
never ends, and the turn runs to the 20s hard cap: twenty seconds of dead air
followed by a transcript with a TV mixed into it.

## Why the answer is a relative threshold and not a better VAD

From Amazon's own stack on this hardware (docs/alexa-endpointing.md): Alexa
does not endpoint with a VAD at all. Its strongest mode predicts the pause
duration expected at this point in the sentence from the decoder's active
hypotheses, so background speech — which contributes no hypotheses — cannot
delay it. That needs a decoder we do not have.

But Amazon's *fallback* energy VAD (`fe.energy_vad.*`) is still not a
threshold on RMS. It tracks a running maximum **and** minimum dB energy with
hunt and hysteresis on each, and sets its start/stop thresholds as per-mille
fractions of the range between them. That part needs no model, and it is what
this module implements.

The consequence is the whole point: the stop threshold is a fraction of the
distance from the room's floor to **the loudest voice in this utterance**.
A television 12-15 dB below a near-field speaker sits under it and reads as
non-speech, however continuously it talks. An absolute detector cannot express
that at all.

## What is anchored, and why it is the wake word

`seed()` takes the wake word's own level. That is a known-good sample of the
target speaker, at their real distance, available at frame zero — so the
threshold is correctly placed from the first frame of the command rather than
after the maximum tracker has had a second or two to converge on it. On the
native-AFE path this also absorbs Amazon's +7.2 dB output gain and its PGA
staging for free: every number here is relative, so none of them care what the
absolute level is.

## This is a ceiling, not a competitor

`_stream_mic_audio` ends the stream on whichever fires first, and the defaults
here are deliberately slower than HA's VAD (STOP_WINDOW is 1.2s of
below-threshold audio against HA's ~0.5-1.0s of silence). In a quiet room HA
still wins every turn and nothing changes. This only ever bites when HA is
stuck — which is exactly the reported failure.

## The one thing it cannot do

Level cannot separate a quiet talker from a loud television at the same level
at the microphone. Amazon separates them with a device-directedness classifier
over a per-frame acoustic embedding — speaker identity, not energy. That is
P5 in docs/alexa-endpointing.md and is not attempted here. `MIN_SPEECH_MS` and
the backporch bound the damage when this module is wrong.
"""

from __future__ import annotations

import math
from collections import deque
from dataclasses import dataclass

# Sub-frame the 80ms voice_queue frames for the decision. Amazon runs its
# trackers at the 10ms feature rate; 80ms granularity would make the voting
# window (below) a 15-vote affair where a single frame is 7% of the decision.
# 20ms keeps the window resolution useful at negligible cost — the RMS of a
# 320-sample slice is one numpy reduction.
SUBFRAME_MS = 20

# Floor for the dB conversion. A digitally-silent sub-frame is -inf, which
# poisons every tracker it touches; -100 dBFS is far below anything the ADC
# can produce and behaves like a number.
DB_FLOOR = -100.0


@dataclass(frozen=True)
class EndpointConfig:
    """Tuning for the relative endpointer.

    Named after Amazon's `fe.energy_vad.*` parameters where they correspond,
    so the two can be read side by side. Their shipped values are NOT
    recoverable — they arrive per-locale as cloud-vended JSON artifacts
    through `amazon.speech.davs.davcservice`, which is disabled on a debloated
    unit and was never account-authenticated anyway. These are EchoMuse's own,
    reasoned about in the module docstring and meant to be tuned by ear.
    """

    # `low_per_mil` — the stop threshold as thousandths of the way from the
    # tracked minimum to the tracked maximum. 400 puts it 40% of the way up:
    # comfortably above a television 12+ dB down, comfortably below a speaker
    # who is still talking at anything near their own established level.
    low_per_mil: int = 400

    # `min_range` — floor on (max - min) before it is used for thresholding.
    # Amazon: "Roughly, should approximate a lower bound of the signal-to-noise
    # ratio." Without it a quiet room (where max and min nearly meet during a
    # pause) computes a threshold a hair above the floor and never stops.
    min_range_db: float = 12.0

    # `max_hunt` / `min_hunt` — the trackers decay toward each other so that
    # neither a single loud event nor a single quiet one is remembered
    # forever. Expressed per second here and converted per sub-frame; Amazon
    # expresses them per frame.
    max_hunt_db_per_s: float = -2.0
    min_hunt_db_per_s: float = 1.0

    # `max_hysteresis` / `min_hysteresis` — consecutive sub-frames beyond a
    # tracker needed to move it. Asymmetric on purpose: the maximum should
    # follow a real voice quickly (2 = 40ms), the minimum should not be
    # dragged down by one clipped sample (5 = 100ms).
    max_hysteresis: int = 2
    min_hysteresis: int = 5

    # `stop_window` / `stop_count` — N-of-M voting, not K-consecutive.
    # A voting window tolerates the stray speech-scoring sub-frame inside a
    # genuine pause, which is precisely what a television produces, without
    # needing the window to be longer. 60 sub-frames is 1.2s; 48 is 80%.
    stop_window: int = 60
    stop_count: int = 48

    # No stop decision before this much audio has been consumed. Amazon's
    # `recognizer.endpoint.min_speech_seconds`, which it implements as a
    # decision object whose entire job is to refuse (`UnderMinEndpointDecision`).
    min_speech_ms: int = 1000

    def __post_init__(self) -> None:
        # Mirrors the assertions libpryon makes on its own config. A
        # nonsensical combination here does not crash, it silently never
        # endpoints — the failure this module exists to fix.
        if not 0 <= self.low_per_mil <= 1000:
            raise ValueError("low_per_mil must be in [0, 1000]")
        if self.stop_window <= 0:
            raise ValueError("stop_window must be > 0")
        if not 0 < self.stop_count <= self.stop_window:
            raise ValueError("stop_count must be in (0, stop_window]")
        if self.min_range_db <= 0:
            raise ValueError("min_range_db must be > 0")
        if self.max_hunt_db_per_s >= 0:
            raise ValueError("max_hunt_db_per_s must be < 0")
        if self.min_hunt_db_per_s <= 0:
            raise ValueError("min_hunt_db_per_s must be > 0")


DEFAULT_CONFIG = EndpointConfig()

# Fraction of the voting window that must agree before the turn ends. Not
# exposed: it is the shape of the decision rather than a room's property, and
# a user with both this and the window length has two knobs that trade against
# each other with no way to tell which one they moved.
STOP_RATIO = 0.8


def config_for(silence_ms: int, low_per_mil: int) -> EndpointConfig:
    """Build a config from the two settings a room actually needs tuning for.

    `silence_ms` is how much below-threshold audio ends a turn — the knob
    behind "it cut me off mid-sentence" and "it kept listening". It is the
    same question `vadSilenceMs` answers for the device's own button-turn
    gate, and it is exposed for the same reason.

    Note it is NOT the same quantity as HA's silence timer even though both
    are milliseconds: HA counts frames its VAD calls non-speech, this counts
    frames below the turn's own relative threshold. In a quiet room they
    coincide; with a television only the second one ever elapses.
    """
    window = max(1, round(silence_ms / SUBFRAME_MS))
    count = max(1, min(window, round(window * STOP_RATIO)))
    return EndpointConfig(
        low_per_mil=low_per_mil, stop_window=window, stop_count=count,
    )

# Audio kept flowing to HA after the stop decision, before end=True.
#
# Amazon calls this the backporch and does not use a constant — it estimates
# it from the phone alignment or the VAD queue (four separate estimators) and
# reports the error as a metric. We have no alignment, so a fixed tail it is.
#
# It is not optional garnish: the whole point of this module is to endpoint
# while something is still making noise, and a voting window that needs 80% of
# 1.2s to agree has, by construction, already spent some of that window on
# audio the user was still producing. Without a tail the last word is clipped
# on exactly the turns this feature is meant to rescue.
BACKPORCH_MS = 250


def rms_dbfs(samples) -> float:
    """dBFS of a numpy int16 array. Silence reads DB_FLOOR, never -inf."""
    if samples.size == 0:
        return DB_FLOOR
    mean_sq = float(((samples.astype("float64") / 32768.0) ** 2).mean())
    if mean_sq <= 0.0:
        return DB_FLOOR
    # 10*log10 of mean-square is 20*log10 of RMS, without the sqrt.
    return max(DB_FLOOR, 10.0 * math.log10(mean_sq))


class Endpointer:
    """Range-normalised end-of-utterance detector.

    Feed it sub-frame dB values in order; it returns a stop reason once, and
    then keeps returning it. Deliberately has no clock of its own: `elapsed_ms`
    is audio time (sub-frames consumed), which is the right clock for
    "have we heard enough speech to be allowed to stop". Wall-clock limits —
    `maxSpeechMs`, the backporch — belong to the caller, because they are about
    how long a person has been waiting and must keep running when the link
    stalls and no audio arrives at all.
    """

    def __init__(self, cfg: EndpointConfig = DEFAULT_CONFIG):
        self.cfg = cfg
        self.max_db: float | None = None
        self.min_db: float | None = None
        self.elapsed_ms: int = 0
        self.stop_reason: str | None = None

        self._max_above = 0
        self._min_below = 0
        self._votes: deque[bool] = deque(maxlen=cfg.stop_window)
        self._below = 0

        self._max_step = cfg.max_hunt_db_per_s * SUBFRAME_MS / 1000.0
        self._min_step = cfg.min_hunt_db_per_s * SUBFRAME_MS / 1000.0

    # ── anchoring ──────────────────────────────────────────────────────────

    def seed(self, db: float) -> None:
        """Anchor the maximum tracker on the wake word's own level.

        Only ever raises it. A wake word quieter than what has already been
        heard is not evidence that the speaker is quiet — it is the tail of a
        detection, and lowering the anchor to it would place the threshold
        below the person's real level for the rest of the turn.
        """
        db = max(DB_FLOOR, db)
        if self.max_db is None or db > self.max_db:
            self.max_db = db

    # ── the decision ───────────────────────────────────────────────────────

    @property
    def low_threshold(self) -> float | None:
        """Current stop threshold in dBFS, or None before the first sample.

        Measured DOWN from the maximum, not up from the minimum. The two are
        identical whenever the observed range exceeds `min_range_db` — which
        is the ordinary case — and differ only when the floor clamps it, where
        anchoring from the minimum is actively wrong: a steady tone converges
        both trackers onto its own level, and `min + 0.4 x 12dB` then sits
        4.8 dB ABOVE the signal, so a speaker holding a constant level
        endpoints themselves. Found by
        `test_a_talker_who_stays_at_their_own_level_is_never_cut`.

        The maximum is the speaker; the threshold belongs a fixed distance
        below them, never a fixed distance above the room.
        """
        if self.max_db is None or self.min_db is None:
            return None
        span = max(self.max_db - self.min_db, self.cfg.min_range_db)
        return self.max_db - (1.0 - self.cfg.low_per_mil / 1000.0) * span

    def push_db(self, db: float) -> str | None:
        """Consume one sub-frame. Returns a stop reason, or None."""
        if self.stop_reason is not None:
            return self.stop_reason

        db = min(0.0, max(DB_FLOOR, db))
        cfg = self.cfg

        if self.max_db is None:
            self.max_db = db
        if self.min_db is None:
            self.min_db = db

        # Maximum tracker: rises on `max_hysteresis` consecutive sub-frames
        # above it, otherwise hunts down.
        if db > self.max_db:
            self._max_above += 1
            if self._max_above >= cfg.max_hysteresis:
                self.max_db = db
                self._max_above = 0
        else:
            self._max_above = 0
            self.max_db += self._max_step

        # Minimum tracker: falls on `min_hysteresis` consecutive sub-frames
        # below it, otherwise hunts up.
        if db < self.min_db:
            self._min_below += 1
            if self._min_below >= cfg.min_hysteresis:
                self.min_db = db
                self._min_below = 0
        else:
            self._min_below = 0
            self.min_db += self._min_step

        # The hunts move independently and can cross. A minimum above the
        # maximum inverts the threshold and would stop the turn instantly, so
        # the ordering is restored rather than left to `min_range_db` to
        # paper over.
        if self.min_db > self.max_db:
            self.min_db = self.max_db

        low = self.low_threshold
        below = db < low
        if len(self._votes) == self._votes.maxlen and self._votes[0]:
            self._below -= 1
        self._votes.append(below)
        if below:
            self._below += 1

        self.elapsed_ms += SUBFRAME_MS

        # min_speech is a veto, checked after the trackers have been updated:
        # the window must still be filling during the protected period, or a
        # user who pauses at 1.0s restarts the vote count from zero and the
        # guard costs a second of latency on every turn instead of none.
        if self.elapsed_ms < cfg.min_speech_ms:
            return None

        if len(self._votes) == self._votes.maxlen and self._below >= cfg.stop_count:
            self.stop_reason = "relative_silence"
        return self.stop_reason

    def push_pcm(self, pcm: bytes, np) -> str | None:
        """Consume one 80ms S16LE frame as sub-frames. `np` is numpy.

        numpy is passed in rather than imported so this module stays free of
        heavyweight imports for the tests that only exercise `push_db`.
        """
        samples = np.frombuffer(pcm, dtype=np.int16)
        step = int(16000 * SUBFRAME_MS / 1000)  # 320 samples at 16kHz
        for i in range(0, len(samples) - step + 1, step):
            self.push_db(rms_dbfs(samples[i:i + step]))
        return self.stop_reason
