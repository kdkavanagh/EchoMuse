#!/usr/bin/env python3
"""Write slprobe playback stimuli: a 48 kHz stereo S16 WAV plus a segment
file (name, start s, end s in stimulus time) that decode.py --stim-seg reads.

  stimulus.py echo  out.wav   # docs/echo-show-crown.md §4.4's stimulus
  stimulus.py loud  out.wav   # Phase 0 item 1: loud programme, clipping

Levels are dBFS (RMS for noise, peak for tones). Needs numpy.
"""
import argparse
import sys
import wave

import numpy as np

RATE = 48000


def silence(s):
    return np.zeros(int(s * RATE))


def white(s, dbfs, rng):
    x = rng.standard_normal(int(s * RATE))
    return x / np.sqrt(np.mean(x**2)) * 10 ** (dbfs / 20)


def pink(s, dbfs, rng):
    n = int(s * RATE)
    spec = np.fft.rfft(rng.standard_normal(n))
    f = np.fft.rfftfreq(n, 1 / RATE)
    f[0] = f[1]
    x = np.fft.irfft(spec / np.sqrt(f), n)
    return x / np.sqrt(np.mean(x**2)) * 10 ** (dbfs / 20)


def tone(s, hz, dbfs):
    t = np.arange(int(s * RATE)) / RATE
    return np.sin(2 * np.pi * hz * t) * 10 ** (dbfs / 20)


def sweep(s, f0, f1, dbfs):
    t = np.arange(int(s * RATE)) / RATE
    k = np.log(f1 / f0)
    return np.sin(2 * np.pi * f0 * s / k * (np.exp(t / s * k) - 1)) * 10 ** (dbfs / 20)


def syllabic(s, dbfs, rng):
    """Pink noise with 4 Hz on/off modulation: a speech-rhythm programme."""
    x = pink(s, 0, rng)
    t = np.arange(len(x)) / RATE
    env = np.clip(np.sin(2 * np.pi * 4 * t) * 1.5, 0, 1)
    x = x * env
    return x / np.sqrt(np.mean(x**2)) * 10 ** (dbfs / 20)


def fade(x, ms=5):
    n = min(int(ms * RATE / 1000), len(x) // 2)
    if n:
        r = np.linspace(0, 1, n)
        x[:n] *= r
        x[-n:] *= r[::-1]
    return x


def bursts(count, dbfs, spacing=1.0, ms=20):
    out = []
    for _ in range(count):
        out.append(fade(tone(ms / 1000, 1000, dbfs)))
        out.append(silence(spacing - ms / 1000))
    return out


def build(kind, rng):
    if kind == "echo":
        parts = [("lead", silence(1)), ("noise", white(4, -18, rng)), ("gap1", silence(1)),
                 ("tone", fade(tone(3, 1000, -12))), ("gap2", silence(1)),
                 ("sweep", fade(sweep(4, 100, 7000, -12))), ("gap3", silence(1)),
                 ("bursts", np.concatenate(bursts(3, -6))), ("tail", silence(1.5))]
    else:
        parts = [("lead", silence(1)), ("pink", pink(4, -12, rng)), ("gap1", silence(1)),
                 ("tone", fade(tone(3, 1000, -3))), ("gap2", silence(1)),
                 ("sweep", fade(sweep(4, 100, 7000, -6))), ("gap3", silence(1)),
                 ("syllabic", syllabic(4, -12, rng)), ("gap4", silence(1)),
                 ("bursts", np.concatenate(bursts(3, 0))), ("tail", silence(1.5))]
    return parts


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("kind", choices=["echo", "loud"])
    ap.add_argument("out", help="output WAV; the segment file is OUT with .seg")
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    parts = build(a.kind, np.random.default_rng(a.seed))
    x = np.concatenate([p for _, p in parts])
    pcm = np.clip(np.round(x * 32767), -32768, 32767).astype("<i2")
    with wave.open(a.out, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(np.repeat(pcm[:, None], 2, axis=1).tobytes())
    seg = a.out.rsplit(".", 1)[0] + ".seg"
    t = 0.0
    with open(seg, "w") as f:
        for name, p in parts:
            d = len(p) / RATE
            f.write(f"{name} {t:.3f} {t + d:.3f}\n")
            t += d
    print(f"{a.out}: {t:.2f} s; segments in {seg}", file=sys.stderr)


if __name__ == "__main__":
    main()
