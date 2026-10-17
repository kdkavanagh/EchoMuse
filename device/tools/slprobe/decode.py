#!/usr/bin/env python3
"""Decode an slprobe capture: PREFIX.raw (S16LE mono) + PREFIX.ts.

Recovers Amazon's v3.3 AFE metadata from bit 0 with the firmware decoder's
rule (device/internal/audio/afe/afe.go): one 128-bit frame per 128 samples,
MSB first; valid when sync is 0xA5, version 0x63, PAYLOAD_SIZE 104 and the
checksum byte equals the popcount of bits 127..8. The frame grid is the
phase (sample offset mod 128) with the most valid frames.

Reports capture timing (buffer gaps, first buffer, stalls, record events)
and, per segment, the PCM level (bit 0 cleared) and the metadata: PA, ERLE,
DTD (/31), DNN_VAD_PROB (x0.25), UED and UED_GR (/32), MIC_CLIPPED,
OUTPUT_CLIPPED, AEC_DIVERGED, DEVICE_MUTE, VOLUME, and the offset of each
frame's buffer completion (CLOCK_MONOTONIC ms mod 65536) after its
AFE_TIMESTAMP.

Segments: fixed windows (--window), explicit --seg NAME:T0:T1 in capture
seconds, or --stim-seg FILE (stimulus.py's .seg) mapped through the player's
enqueue stamps plus --latency, trimmed by --trim at each edge. The mapping
takes a capture sample's time as its buffer's completion, so the latency is
the onset's delay in that convention: about 0.226 s for GENERIC and 0.285 s
for VOICE_RECOGNITION (the ASP path adds about 60 ms), measured on
G6G16D12143203LD from 1 kHz burst onsets; the default follows the preset.

  decode.py /tmp/cap                     # 1 s windows
  decode.py /tmp/cap --stim-seg loud.seg # stimulus segments
  decode.py /tmp/cap --csv frames.csv    # every decoded frame
"""
import argparse
import sys

import numpy as np

FIELDS = ("counter", "ts", "erle", "rms", "aec_div", "mic_clip", "out_clip", "pa",
          "volume", "mute", "vss", "dtd", "ued_gr", "ued", "vad")


def read_ts(path):
    hdr, t0, rec, play, events, marks = {}, None, [], [], [], []
    ps = pe = None
    with open(path) as f:
        for line in f:
            p = line.split()
            if not p:
                continue
            if p[0] == "#":
                for kv in p[1:]:
                    if "=" in kv:
                        k, v = kv.split("=", 1)
                        hdr.setdefault(k, v)
            elif p[0] == "T0":
                t0 = int(p[1])
            elif p[0] == "R":
                rec.append((int(p[1]), int(p[2])))
            elif p[0] == "P":
                play.append((int(p[1]), int(p[2]), int(p[3])))
            elif p[0] == "PS":
                ps = int(p[1])
            elif p[0] == "PE":
                pe = int(p[1])
            elif p[0] == "E":
                events.append((p[1], int(p[2])))
            elif p[0] == "X":
                marks.append((p[1], int(p[-1])))
    return hdr, t0, np.array(rec, dtype=np.int64).reshape(-1, 2), play, ps, pe, events, marks


def frames_at(bits, phase):
    k = (len(bits) - phase) // 128
    b = bits[phase:phase + 128 * k].reshape(k, 128)
    packed = np.packbits(b, axis=1)
    hi = packed[:, :8].copy().view(">u8").ravel().astype(np.uint64)
    lo = packed[:, 8:].copy().view(">u8").ravel().astype(np.uint64)
    ok = ((hi >> np.uint64(48)) == 0xA563) & (((hi >> np.uint64(41)) & np.uint64(0x7F)) == 104)
    ok &= b[:, :120].sum(axis=1).astype(np.uint64) == (lo & np.uint64(0xFF))
    return hi, lo, ok


def decode_fields(hi, lo):
    def f(x, shift, mask):
        return ((x >> np.uint64(shift)) & np.uint64(mask)).astype(np.int64)
    return {
        "counter": f(hi, 35, 0x3F), "ts": f(hi, 19, 0xFFFF), "erle": f(hi, 11, 0xFF),
        "rms": f(hi, 2, 0xFF) - 256, "aec_div": f(lo, 63, 1), "mic_clip": f(lo, 62, 1),
        "out_clip": f(lo, 61, 1), "pa": f(lo, 60, 1), "volume": f(lo, 53, 0x7F),
        "mute": f(lo, 52, 1), "vss": f(lo, 44, 0xFF), "dtd": f(lo, 39, 0x1F),
        "ued_gr": f(lo, 34, 0x1F), "ued": f(lo, 29, 0x1F), "vad": f(lo, 13, 0x3),
    }


def db(x):
    return 20 * np.log10(x) if x > 0 else float("-inf")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("prefix")
    ap.add_argument("--window", type=float, default=1.0, help="window length, s (default 1)")
    ap.add_argument("--seg", action="append", default=[], help="NAME:T0:T1 in capture seconds")
    ap.add_argument("--stim-seg", help="stimulus segment file (stimulus.py .seg)")
    ap.add_argument("--latency", type=float, help="enqueue-to-onset latency, s (default by preset)")
    ap.add_argument("--trim", type=float, default=0.15, help="trim at each stimulus segment edge, s")
    ap.add_argument("--csv", help="write every decoded frame to this CSV")
    a = ap.parse_args()

    hdr, t0, rec, play, ps, pe, events, marks = read_ts(a.prefix + ".ts")
    rate = int(hdr.get("rate", 16000))
    per = int(hdr.get("frames", 256))
    pcm = np.fromfile(a.prefix + ".raw", dtype="<i2")
    bits = (pcm & 1).astype(np.uint8)
    lvl = (pcm & ~1).astype(np.float64) / 32768.0
    print(f"{a.prefix}: {len(pcm)} samples ({len(pcm) / rate:.2f} s), preset {hdr.get('preset')}, "
          f"{per}-frame buffers x {hdr.get('nbuf')}, context {hdr.get('context')}")

    # Capture timing.
    if len(rec):
        ns = rec[:, 1]
        gaps = np.diff(ns) / 1e6
        first = (ns[0] - t0) / 1e6 if t0 else float("nan")
        print(f"  buffers {len(rec)}, first completion {first:.1f} ms after RECORDING; gaps ms "
              f"p50 {np.median(gaps):.1f} p99 {np.percentile(gaps, 99):.1f} max {gaps.max():.1f}"
              if len(gaps) else f"  buffers {len(rec)}")
        span = (ns[-1] - ns[0]) / 1e9
        if span > 0:
            print(f"  delivered {(len(rec) - 1) * per / span:.1f} samples/s over {span:.1f} s "
                  f"(nominal {rate})")
        big = np.nonzero(gaps > 200)[0] if len(gaps) else []
        for i in big[:20]:
            print(f"  gap {gaps[i]:.0f} ms after buffer {rec[i, 0]} "
                  f"(t={(ns[i] - t0) / 1e9:.2f} s)")
    for ev, t in events:
        print(f"  record event {ev} at t={(t - t0) / 1e9:.2f} s")
    for m, t in marks:
        print(f"  {m} at t={(t - t0) / 1e9:.2f} s")
    if ps:
        print(f"  player start t={(ps - t0) / 1e9:.3f} s" + (f", stimulus end t={(pe - t0) / 1e9:.3f} s" if pe else ""))

    # Frame grid.
    best = None
    for phase in range(128):
        hi, lo, ok = frames_at(bits, phase)
        if best is None or ok.sum() > best[3].sum():
            best = (phase, hi, lo, ok)
    phase, hi, lo, ok = best
    nvalid = int(ok.sum())
    print(f"  metadata: phase {phase}, {nvalid}/{len(ok)} frames valid")
    if nvalid == 0:
        F = None
    else:
        F = decode_fields(hi, lo)
        idx = np.nonzero(ok)[0]
        steps = np.diff(F["counter"][idx]) % 64
        gaps_k = np.diff(idx)
        bad = int(np.sum((steps != gaps_k % 64)))
        print(f"  FRAME_COUNTER: {bad} discontinuities among valid frames")
    end_sample = phase + 128 * np.arange(len(ok)) + 127

    # AFE_TIMESTAMP vs completion of the buffer that holds the frame's last bit.
    off = None
    if F is not None and len(rec):
        seq = end_sample // per
        have = seq < len(rec)
        comp_ms = np.full(len(ok), -1, dtype=np.int64)
        comp_ms[have] = (rec[seq[have], 1] // 1000000) % 65536
        off = (comp_ms - F["ts"] + 32768) % 65536 - 32768
        off = np.where(have & ok, off, np.iinfo(np.int64).min)

    if a.csv and F is not None:
        with open(a.csv, "w") as f:
            f.write("frame,end_sample,valid," + ",".join(FIELDS) + ",completion_minus_ts_ms\n")
            for i in range(len(ok)):
                vals = ",".join(str(F[k][i]) for k in FIELDS)
                o = "" if off is None or off[i] == np.iinfo(np.int64).min else str(off[i])
                f.write(f"{i},{end_sample[i]},{int(ok[i])},{vals},{o}\n")

    # Segments, in capture samples.
    segs = []
    for s in a.seg:
        name, s0, s1 = s.split(":")
        segs.append((name, float(s0), float(s1)))
    if a.stim_seg:
        if not play or not len(rec):
            sys.exit("--stim-seg needs player stamps and capture buffers in the .ts")
        stim_rate = int(hdr.get("stim_rate", 48000))
        pf = np.array([p[0] for p in play], dtype=np.int64)
        pn = np.array([p[2] for p in play], dtype=np.int64)
        first_pass = np.concatenate([[True], np.diff(pf) > 0]).cumprod().astype(bool)
        pf, pn = pf[first_pass], pn[first_pass]
        # completion clock -> capture sample: least squares over all buffers,
        # on clock offsets from the first completion (raw ns is ill-conditioned).
        ns0 = rec[0, 1]
        k, c = np.polyfit((rec[:, 1] - ns0).astype(np.float64), ((rec[:, 0] + 1) * per).astype(np.float64), 1)

        latency = a.latency if a.latency is not None else (0.285 if hdr.get("preset") == "3" else 0.226)

        def stim_to_cap(t):
            fr = t * stim_rate
            i = max(0, np.searchsorted(pf, fr, side="right") - 1)
            mono = pn[i] - ns0 + (fr - pf[i]) / stim_rate * 1e9 + latency * 1e9
            return (k * mono + c) / rate
        with open(a.stim_seg) as f:
            for line in f:
                p = line.split()
                if len(p) == 3:
                    s0, s1 = stim_to_cap(float(p[1])) + a.trim, stim_to_cap(float(p[2])) - a.trim
                    if s1 > s0:
                        segs.append((p[0], s0, s1))
    if not segs:
        n = int(np.ceil(len(pcm) / rate / a.window))
        segs = [(f"w{i}", i * a.window, min((i + 1) * a.window, len(pcm) / rate)) for i in range(n)]

    print()
    print(f"{'segment':10} {'t0':>6} {'t1':>6} {'dBFS':>6} {'valid':>9} {'PA':>4} {'ERLE':>9} "
          f"{'DTD max/mean/nz%':>17} {'VAD max/mean/nz%':>17} {'UED max/mean':>12} {'GR':>4} "
          f"{'MICC':>4} {'OUTC':>4} {'DIV':>3} {'MUTE':>4} {'VOL':>7} {'cmp-ts ms':>13}")
    for name, s0, s1 in segs:
        i0, i1 = int(s0 * rate), min(int(s1 * rate), len(pcm))
        if i1 <= i0:
            continue
        level = db(np.sqrt(np.mean(lvl[i0:i1] ** 2)))
        row = f"{name:10} {s0:6.2f} {s1:6.2f} {level:6.1f}"
        if F is None:
            print(row + "   no metadata")
            continue
        sel = (end_sample >= i0) & (end_sample < i1)
        v = sel & ok
        nv = int(v.sum())
        if nv == 0:
            print(row + f" {0:>4}/{int(sel.sum()):<4} no valid frames")
            continue
        g = {k: F[k][v] for k in FIELDS}
        dtd, vad, ued = g["dtd"] / 31, g["vad"] * 0.25, g["ued"] / 32
        vols = sorted(set(g["volume"].tolist()))
        vol = ",".join(map(str, vols[:3])) + ("…" if len(vols) > 3 else "")
        o = off[v] if off is not None else np.array([])
        o = o[o != np.iinfo(np.int64).min]
        ostr = f"{int(np.median(o))} [{int(o.min())},{int(o.max())}]" if len(o) else "-"
        erle_nz = g["erle"][g["erle"] > 0]
        erle = f"{erle_nz.mean():4.1f}({g['erle'].max():2d})" if len(erle_nz) else "   0( 0)"
        row += (f" {nv:>4}/{int(sel.sum()):<4} {g['pa'].mean():4.2f} {erle:>9} "
                f"{dtd.max():5.2f}/{dtd.mean():5.3f}/{100 * np.mean(dtd > 0):4.0f} "
                f"{vad.max():5.2f}/{vad.mean():5.3f}/{100 * np.mean(vad > 0):4.0f} "
                f"{ued.max():5.2f}/{ued.mean():5.3f} {g['ued_gr'].mean() / 32:4.2f} "
                f"{int(g['mic_clip'].sum()):4d} {int(g['out_clip'].sum()):4d} {int(g['aec_div'].sum()):3d} "
                f"{int(g['mute'].sum()):4d} {vol:>7} {ostr:>13}")
        print(row)


if __name__ == "__main__":
    main()
