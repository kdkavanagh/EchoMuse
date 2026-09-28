"""The STT copy: what HA's speech-to-text hears (SPEC §16.7 step 1).

Built from the committed canonical span only. The per-turn ASR gain and the
optional DTLN denoiser touch this copy and nothing else: wake, endpoint,
attribution, and reference evidence all keep canonical PCM (§8.1).

Why a gain at all: native-AFE speech sits near −40 dBFS, where the deployed
faster-whisper silently drops quiet leading words ("How many ounces are in a
cup?" arrives as "ounces are in a cup."). +20 dB fixed that on every measured
turn; +25 dB made two of ten clips worse, so the nominal value stays at 20.
"""

from __future__ import annotations

import logging

import numpy as np

import em_ns

log = logging.getLogger("echomuse.stt_copy")

ASR_GAIN_NOMINAL_DB = 20.0

# The wake word is spoken at the command's level on average (mean offset
# −0.3 dB over eight measured turns) but with ±5 dB scatter uncorrelated with
# the command, so normalising 1:1 against it widens the post-gain spread. It is
# used as a guard band instead: hold the nominal gain while the predicted
# post-gain level lands where transcripts were verified good (−34…−22 dBFS),
# and only follow the wake word outside it.
ASR_TARGET_MAX_DBFS = -22.0   # louder than this: back off (clipping risk)
ASR_TARGET_MIN_DBFS = -34.0   # quieter than this: push, up to the ceiling
ASR_GAIN_CEIL_DB = 30.0


def asr_gain_for(wake_db: float | None) -> float:
    """This turn's ASR gain in dB from the wake word's loudness.

    `wake_db` is the peak cell level `E` (dBFS RMS) over the accepted
    candidate's support, or None for a turn without a wake word of its own
    (button, reply), which gets the nominal gain.
    """
    if wake_db is None:
        return ASR_GAIN_NOMINAL_DB
    predicted = wake_db + ASR_GAIN_NOMINAL_DB
    if predicted > ASR_TARGET_MAX_DBFS:
        return max(0.0, ASR_TARGET_MAX_DBFS - wake_db)
    if predicted < ASR_TARGET_MIN_DBFS:
        return min(ASR_GAIN_CEIL_DB, ASR_TARGET_MIN_DBFS - wake_db)
    return ASR_GAIN_NOMINAL_DB


def apply_asr_gain(pcm: bytes, gain_db: float = ASR_GAIN_NOMINAL_DB) -> tuple[bytes, int]:
    """Scale S16_LE mono PCM by `gain_db`, hard-clamped to the int16 rail.

    Returns (pcm, clipped_sample_count). Clamping, never wrapping: a wrapped
    sample is a full-scale impulse. `gain_db == 0` returns the input object
    unchanged.
    """
    if not gain_db or not pcm:
        return pcm, 0
    samples = np.frombuffer(pcm, dtype=np.int16).astype(np.float32)
    samples *= 10.0 ** (gain_db / 20.0)
    clipped = int(np.count_nonzero((samples > 32767.0) | (samples < -32768.0)))
    np.clip(samples, -32768.0, 32767.0, out=samples)
    return samples.astype(np.int16).tobytes(), clipped


def stt_copy(canonical_pcm16: np.ndarray, *, gain: float, ns: bool) -> bytes:
    """The committed span as HA STT input: optional DTLN (`nsAsr`), then the gain.

    CPU-bound; call from an executor. When `ns` is set but the DTLN models are
    missing or fail, the span is sent undenoised and a warning is logged —
    denoising is an optional enhancement on top of the native AFE's own.
    """
    pcm = np.asarray(canonical_pcm16)
    if pcm.dtype != np.int16 or pcm.ndim != 1:
        raise ValueError("canonical PCM must be 1-D int16")
    data = pcm.tobytes()
    if ns:
        if not em_ns.available():
            log.warning("nsAsr is on but the DTLN models are missing (%s); STT copy is undenoised",
                        em_ns.MODEL_DIR)
        else:
            try:
                raw = data
                data = em_ns.StreamingDenoiser().process(raw)
                # DTLN holds back a sub-hop remainder; the tail of the span is
                # kept raw rather than cut, so STT hears the whole span.
                data += raw[len(data):]
                em_ns.dump_debug_pair("stt_copy", raw, data)
            except Exception as exc:
                log.warning("DTLN failed (%s); STT copy is undenoised", exc)
                data = pcm.tobytes()
    out, clipped = apply_asr_gain(data, gain)
    if clipped:
        log.warning("ASR gain clamped %d samples at %+.1f dB; lower ASR_TARGET_MAX_DBFS", clipped, gain)
    return out
