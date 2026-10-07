"""Native AFE metadata (`afe_metadata_v1`): span summaries and decoder stats.

Fire OS 6's AFE writes a metadata frame into every 8 ms of the mixer capture;
the device decodes it and uplinks one record per 80 ms capture period (EMA1
kind 5, `em_audio_timeline.AfeRecord`) and its decoder counters in
`wake.stats.afe` (docs/protocol-v1.md). This module turns both into typed
evidence for turn rows, the decision trace and the dashboard.

Evidence only: no wake acceptance, attribution verdict, endpoint or threshold
reads anything here — nothing has yet related these fields to outcomes.
Absent data is "no data", never zeros: a span without a record that carries
frames has no summary (None), stats the device did not send are None, and a
device without the capability has no evidence at all (`unavailable`, SQL NULL).

Timing (measured): RMS describes the 128 samples its frame rides on;
PLAYBACK_ACTIVE, ERLE_RAW, DTD and DNN_VAD_PROB lead the audio they ride on by
≈48 ms, less than one period, so a span summarises the periods it overlaps
without a shift.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import TypedDict

from em_audio_timeline import AFE_PERIOD_FRAMES, AFE_PERIOD_SAMPLES, AfeRecord, AfeTimeline

DTD_SCALE = 31.0        # DTD value = raw / 31
VAD_STEP = 0.25         # DNN_VAD_PROB value = raw × 0.25
RMS_OFFSET = 256        # RMS dB = raw − 256
FRAME_SAMPLES = AFE_PERIOD_SAMPLES // AFE_PERIOD_FRAMES   # one AFE frame: 128 capture samples (8 ms)
PRE_SAMPLES = 16_000    # `pre`: the second before a candidate's support window


# --- span summaries --------------------------------------------------------------------


class AfeSummaryJson(TypedDict):
    """`AfeSummary` on the wire (turn rows, decision trace, turns API)."""

    start: int
    end: int
    periods_expected: int
    periods_received: int
    periods_with_frames: int
    frames: int
    frames_expected: int
    playback_frames: int
    erle_max: int
    erle_mean: float | None
    erle_frames: int
    dtd_max: float
    dtd_frames: int
    rms_max_db: int | None
    rms_mean_db: float | None
    vad_max: float
    vad_frames: int
    volume: int
    output_clipped: int
    mic_clipped: int
    aec_diverged: int
    device_mute: int
    gaps: int
    syncs: int
    lost_frames: int


@dataclass(frozen=True, slots=True)
class AfeSummary:
    """The native AFE records of capture samples `[start, end)`: every period
    overlapping the span. Raw device units except where named: ERLE stays
    ERLE_RAW (max over valid frames; `erle_mean` the mean of the non-zero
    values weighted by their frame counts, None without one); DTD and VAD are
    scaled to their values; RMS is dB (None when no period computed RMS; the
    mean is over the periods' means). Flag fields count periods with the flag."""

    start: int
    end: int
    periods_expected: int
    periods_received: int
    periods_with_frames: int
    frames: int
    frames_expected: int
    playback_frames: int
    erle_max: int
    erle_mean: float | None
    erle_frames: int
    dtd_max: float
    dtd_frames: int
    rms_max_db: int | None
    rms_mean_db: float | None
    vad_max: float
    vad_frames: int
    volume: int                 # VOLUME of the last period with frames
    output_clipped: int
    mic_clipped: int
    aec_diverged: int
    device_mute: int
    gaps: int
    syncs: int
    lost_frames: int

    def wire(self) -> AfeSummaryJson:
        return AfeSummaryJson(
            start=self.start, end=self.end, periods_expected=self.periods_expected,
            periods_received=self.periods_received, periods_with_frames=self.periods_with_frames,
            frames=self.frames, frames_expected=self.frames_expected, playback_frames=self.playback_frames,
            erle_max=self.erle_max, erle_mean=self.erle_mean, erle_frames=self.erle_frames,
            dtd_max=self.dtd_max, dtd_frames=self.dtd_frames,
            rms_max_db=self.rms_max_db, rms_mean_db=self.rms_mean_db,
            vad_max=self.vad_max, vad_frames=self.vad_frames, volume=self.volume,
            output_clipped=self.output_clipped, mic_clipped=self.mic_clipped,
            aec_diverged=self.aec_diverged, device_mute=self.device_mute,
            gaps=self.gaps, syncs=self.syncs, lost_frames=self.lost_frames,
        )

    @classmethod
    def from_wire(cls, raw: object) -> AfeSummary:
        """Parse `wire()` output (a stored turn row). Raises ValueError."""
        if not isinstance(raw, Mapping):
            raise ValueError(f"afe summary must be an object, got {raw!r}")
        return cls(
            start=_count(raw, "start"), end=_count(raw, "end"),
            periods_expected=_count(raw, "periods_expected"), periods_received=_count(raw, "periods_received"),
            periods_with_frames=_count(raw, "periods_with_frames"),
            frames=_count(raw, "frames"), frames_expected=_count(raw, "frames_expected"),
            playback_frames=_count(raw, "playback_frames"),
            erle_max=_count(raw, "erle_max"), erle_mean=_optional_float(raw, "erle_mean"),
            erle_frames=_count(raw, "erle_frames"),
            dtd_max=_float(raw, "dtd_max"), dtd_frames=_count(raw, "dtd_frames"),
            rms_max_db=_optional_int(raw, "rms_max_db"), rms_mean_db=_optional_float(raw, "rms_mean_db"),
            vad_max=_float(raw, "vad_max"), vad_frames=_count(raw, "vad_frames"), volume=_count(raw, "volume"),
            output_clipped=_count(raw, "output_clipped"), mic_clipped=_count(raw, "mic_clipped"),
            aec_diverged=_count(raw, "aec_diverged"), device_mute=_count(raw, "device_mute"),
            gaps=_count(raw, "gaps"), syncs=_count(raw, "syncs"), lost_frames=_count(raw, "lost_frames"),
        )


def summarize(records: Sequence[AfeRecord], start: int, end: int) -> AfeSummary | None:
    """Summary of the records whose periods overlap capture samples
    `[start, end)`; None ("no data") unless one of them carries frames."""
    if end <= start:
        return None
    span = [r for r in records if r.start < end and r.start + AFE_PERIOD_SAMPLES > start]
    valid = [r for r in span if r.frames > 0]
    if not valid:
        return None
    periods = -(-end // AFE_PERIOD_SAMPLES) - start // AFE_PERIOD_SAMPLES
    erle_frames = sum(r.erle_frames for r in valid)
    rms = [r for r in valid if r.rms_max > 0]
    rms_means = [r.rms_mean - RMS_OFFSET for r in valid if r.rms_mean > 0]
    return AfeSummary(
        start=start, end=end,
        periods_expected=periods,
        periods_received=len(span),
        periods_with_frames=len(valid),
        frames=sum(r.frames for r in valid),
        frames_expected=periods * AFE_PERIOD_FRAMES,
        playback_frames=sum(r.playback for r in valid),
        erle_max=max(r.erle_max for r in valid),
        erle_mean=(round(sum(r.erle_mean * r.erle_frames for r in valid) / erle_frames, 2)
                   if erle_frames else None),
        erle_frames=erle_frames,
        dtd_max=round(max(r.dtd_max for r in valid) / DTD_SCALE, 3),
        dtd_frames=sum(r.dtd_frames for r in valid),
        rms_max_db=max(r.rms_max for r in rms) - RMS_OFFSET if rms else None,
        rms_mean_db=round(sum(rms_means) / len(rms_means), 2) if rms_means else None,
        vad_max=max(r.vad_max for r in valid) * VAD_STEP,
        vad_frames=sum(r.vad_frames for r in valid),
        volume=max(valid, key=lambda r: r.start).volume,
        output_clipped=sum(r.output_clipped for r in span),
        mic_clipped=sum(r.mic_clipped for r in span),
        aec_diverged=sum(r.aec_diverged for r in span),
        device_mute=sum(r.device_mute for r in span),
        gaps=sum(r.gap for r in span),
        syncs=sum(r.sync for r in span),
        lost_frames=sum(r.lost for r in span),
    )


# --- per-turn evidence -----------------------------------------------------------------


class AfeEvidenceJson(TypedDict):
    """`turns.afe_evidence` and the decision trace's `afe_evidence`."""

    support_start: int | None
    pre: AfeSummaryJson | None
    wake: AfeSummaryJson | None
    playback_onset: int | None
    utterance: AfeSummaryJson | None


@dataclass(frozen=True, slots=True)
class AfeEvidence:
    """A turn's native AFE evidence from a session that carried the `afe`
    stream. For a wake turn or rejected candidate, anchored on the
    candidate's `support_start`: `pre`, the second before it (the AEC state
    while only the answer played — inside the support window a real talker
    and unconverged echo both read ERLE-low/DTD-high); `wake`, the support
    window; `playback_onset`, the capture sample of the last PLAYBACK_ACTIVE
    rise at or before it. `utterance` is the committed or closed utterance's
    audio. A None field is "no data" (all candidate fields are None for a
    button or reply turn); a turn with no AfeEvidence is "unavailable"."""

    support_start: int | None
    pre: AfeSummary | None
    wake: AfeSummary | None
    playback_onset: int | None
    utterance: AfeSummary | None

    def wire(self) -> AfeEvidenceJson:
        return AfeEvidenceJson(
            support_start=self.support_start,
            pre=self.pre.wire() if self.pre is not None else None,
            wake=self.wake.wire() if self.wake is not None else None,
            playback_onset=self.playback_onset,
            utterance=self.utterance.wire() if self.utterance is not None else None,
        )

    def dumps(self) -> str:
        return json.dumps(self.wire(), separators=(",", ":"))

    @classmethod
    def loads(cls, text: str) -> AfeEvidence:
        """Parse a stored `turns.afe_evidence`; an absent field is no data. Raises ValueError."""
        raw = json.loads(text)
        if not isinstance(raw, Mapping):
            raise ValueError("afe evidence must be an object")

        def summary(key: str) -> AfeSummary | None:
            value = raw.get(key)
            return None if value is None else AfeSummary.from_wire(value)

        return cls(
            support_start=None if raw.get("support_start") is None else _count(raw, "support_start"),
            pre=summary("pre"),
            wake=summary("wake"),
            playback_onset=None if raw.get("playback_onset") is None else _count(raw, "playback_onset"),
            utterance=summary("utterance"),
        )


def playback_onset(records: Sequence[AfeRecord], at: int) -> int | None:
    """Capture sample of the last PLAYBACK_ACTIVE 0→1 rise at or before `at`,
    or None (no data). A rise is the first period with playback right after a
    period with frames and none, placed where the period's playback frames
    begin if they end it: start + (10 − playback) × 128. A playback run whose
    start was not received (it began before the first record, or after a
    missing or frameless period) has no known rise."""
    onset: int | None = None
    prev: AfeRecord | None = None
    for r in sorted(records, key=lambda rec: rec.start):
        if r.start > at:
            break
        if r.frames == 0:
            prev = None
            continue
        p = prev if prev is not None and prev.start + AFE_PERIOD_SAMPLES == r.start else None
        if r.playback > 0:
            if p is not None and p.playback == 0:
                rise = r.start + (AFE_PERIOD_FRAMES - r.playback) * FRAME_SAMPLES
                if rise > at:
                    break
                onset = rise
            elif p is None:
                onset = None
        prev = r
    return onset


def turn_evidence(store: AfeTimeline, support: tuple[int, int] | None,
                  utterance: tuple[int, int] | None) -> AfeEvidence:
    """Evidence from a lease's AFE records: around the candidate's support
    window `[support_start, support_end)` when there is a candidate, and over
    `utterance` when there is one."""
    start: int | None = None
    pre: AfeSummary | None = None
    wake: AfeSummary | None = None
    onset: int | None = None
    if support is not None:
        support_start, support_end = support
        start = support_start
        a = max(0, support_start - PRE_SAMPLES)
        pre = summarize(store.read(a, support_start), a, support_start)
        wake = summarize(store.read(support_start, support_end), support_start, support_end)
        onset = playback_onset(store.read(0, support_start + 1), support_start)
    span = None if utterance is None else summarize(store.read(*utterance), *utterance)
    return AfeEvidence(start, pre, wake, onset, span)


# --- wake.stats.afe ----------------------------------------------------------------------


class AfeStatsJson(TypedDict):
    """`AfeStats` as the dashboard receives it."""

    periods: int
    frames: int
    frames_expected: int
    invalid: int
    syncs: int
    gaps: int
    lost_frames: int


@dataclass(frozen=True, slots=True)
class AfeStats:
    """The device's AFE decoder counters for one `wake.stats` window
    (counted whether or not capture was privacy-muted)."""

    periods: int            # capture periods decoded
    frames: int             # valid AFE frames
    invalid: int            # frames that failed validation where the locked decoder expected one
    syncs: int              # lock acquisitions
    gaps: int               # FRAME_COUNTER / AFE_TIMESTAMP discontinuities
    lost_frames: int        # AFE frames the counters show missing

    @property
    def frames_expected(self) -> int:
        return self.periods * AFE_PERIOD_FRAMES

    def wire(self) -> AfeStatsJson:
        return AfeStatsJson(periods=self.periods, frames=self.frames, frames_expected=self.frames_expected,
                            invalid=self.invalid, syncs=self.syncs, gaps=self.gaps,
                            lost_frames=self.lost_frames)


def parse_stats(body: Mapping[str, object]) -> AfeStats | None:
    """`wake.stats.afe`: None when absent (no data, never zeros). Raises
    ValueError when present but malformed; the caller logs and keeps None."""
    raw = body.get("afe")
    if raw is None:
        return None
    if not isinstance(raw, Mapping):
        raise ValueError(f"wake.stats afe must be an object, got {raw!r}")
    return AfeStats(periods=_count(raw, "periods"), frames=_count(raw, "frames"), invalid=_count(raw, "invalid"),
                    syncs=_count(raw, "syncs"), gaps=_count(raw, "gaps"), lost_frames=_count(raw, "lost_frames"))


# --- JSON field readers ------------------------------------------------------------------


def _count(raw: Mapping[str, object], key: str) -> int:
    value = raw.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"{key} must be a non-negative integer, got {value!r}")
    return value


def _optional_int(raw: Mapping[str, object], key: str) -> int | None:
    value = raw.get(key)
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{key} must be an integer or null, got {value!r}")
    return value


def _float(raw: Mapping[str, object], key: str) -> float:
    value = raw.get(key)
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise ValueError(f"{key} must be a number, got {value!r}")
    return float(value)


def _optional_float(raw: Mapping[str, object], key: str) -> float | None:
    return None if raw.get(key) is None else _float(raw, key)
