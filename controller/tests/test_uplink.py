"""EMA1 validation, stream epochs, uplink leases, per-lease timelines, and the
capture↔reference clock map (§4.2–4.4, §16.1, WIRE §3–§5)."""

import struct

import numpy as np
import pytest

import em_audio_timeline as at
from em_audio_timeline import Kind, ProtocolError

MIC_EPOCH = 111
REF_EPOCH = 222


def mic(first, n=1280, seq=0, value=None, flags=0, epoch=MIC_EPOCH, mono_ns=0, unc=None):
    pcm = np.arange(first, first + n, dtype=np.int64) % 30000 if value is None else np.full(n, value)
    return at.build_packet(Kind.MIC, epoch=epoch, sequence=seq, first_sample=first,
                           pcm=pcm.astype(np.int16), flags=flags, mono_ns=mono_ns, uncertainty_us=unc)


def ref(first, n=1280, seq=0, silence=False, value=7, epoch=REF_EPOCH, mono_ns=0, unc=None, mask=4):
    if silence:
        return at.build_packet(Kind.REFERENCE, epoch=epoch, sequence=seq, first_sample=first,
                               frame_count=n, flags=at.FLAG_DIGITAL_SILENCE, mono_ns=mono_ns,
                               uncertainty_us=unc, source_mask=0)
    return at.build_packet(Kind.REFERENCE, epoch=epoch, sequence=seq, first_sample=first,
                           pcm=np.full(n, value, np.int16), mono_ns=mono_ns, uncertainty_us=unc,
                           source_mask=mask)


def cells(first, e, seq=0, flags=None, mask=None):
    n = len(e)
    data = at.build_cells(e, flags or [0] * n, mask or [0] * n)
    return at.build_packet(Kind.CELLS, epoch=MIC_EPOCH, sequence=seq, first_sample=first, cells=data)


def afe_rec(frames=10, **kw):
    """One afe record v1 (the `start` is the packet's to imply)."""
    fields = dict(start=0, frames=frames, flags=0, playback=0, erle_max=0, erle_mean=0, erle_frames=0,
                  dtd_max=0, dtd_frames=0, rms_max=200, rms_mean=195, vad_max=0, vad_frames=0, volume=70, lost=0)
    fields.update(kw)
    return at.AfeRecord(**fields)


def afe(first, recs, seq=0, flags=0):
    return at.build_packet(Kind.AFE, epoch=MIC_EPOCH, sequence=seq, first_sample=first,
                           afe=at.build_afe(recs), flags=flags)


def opened_session(now, afe_metadata=False):
    s = at.UplinkSession(now=lambda: now[0], afe_metadata=afe_metadata)
    streams = [("mic", 1, MIC_EPOCH, 1), ("cells", 4, MIC_EPOCH, 2), ("reference", 2, REF_EPOCH, 1)]
    if afe_metadata:
        streams.append(("afe", 5, MIC_EPOCH, 3))
    for sid, kind, epoch, fmt in streams:
        s.stream_open({"stream_id": sid, "epoch": str(epoch), "kind": kind,
                       "sample_rate": 16000, "format": fmt, "reason": "start"})
    return s


# -- EMA1 frame ----------------------------------------------------------------

def test_mic_frame_round_trips_every_header_field():
    frame = mic(512, n=1280, seq=4, flags=at.FLAG_ESTIMATED, mono_ns=123456789, unc=42700)
    assert len(frame) == 64 + 2560 and frame[:4] == b"EMA1"
    p = at.parse_packet(frame)
    assert (p.kind, p.epoch, p.sequence, p.first_sample, p.frame_count) == (Kind.MIC, MIC_EPOCH, 4, 512, 1280)
    assert (p.mono_ns, p.uncertainty_us, p.sample_rate, p.estimated) == (123456789, 42700, 16000, True)
    assert p.end_sample == 1792 and p.pcm()[0] == 512


def test_unknown_uncertainty_is_none_not_zero():
    assert at.parse_packet(mic(0)).uncertainty_us is None


def test_digital_silence_reference_has_no_payload_and_reads_as_zeros():
    frame = ref(2560, silence=True)
    assert len(frame) == 64
    p = at.parse_packet(frame)
    assert p.digital_silence and p.frame_count == 1280 and not p.pcm().any()


def test_cell_records_parse_e_flags_and_mask():
    p = at.parse_packet(cells(1024, [-5000, -12000, 0], flags=[0, at.CELL_FLAG_GAP, 2], mask=[1, 0, 12]))
    c = p.cells()
    assert list(c.e_centi) == [-5000, -12000, 0] and list(c.flags) == [0, 1, 2] and list(c.mask) == [1, 0, 12]
    assert c.e_db[0] == pytest.approx(-50.0)
    assert p.end_sample == 1024 + 3 * 512


def patch(frame, offset, fmt, value):
    b = bytearray(frame)
    struct.pack_into(fmt, b, offset, value)
    return bytes(b)


@pytest.mark.parametrize("mutate, needle", [
    (lambda f: b"EMA2" + f[4:], "magic"),
    (lambda f: patch(f, 4, "<B", 9), "kind"),
    (lambda f: patch(f, 5, "<B", 1 << 5), "flags"),
    (lambda f: patch(f, 5, "<B", at.FLAG_DIGITAL_SILENCE), "digital silence"),
    (lambda f: patch(f, 6, "<B", 2), "channels"),
    (lambda f: patch(f, 7, "<B", 2), "format"),
    (lambda f: patch(f, 8, "<Q", 0), "epoch"),
    (lambda f: patch(f, 44, "<I", 48000), "rate"),
    (lambda f: patch(f, 48, "<I", 1281), "frame count"),
    (lambda f: patch(f, 48, "<I", 0), "frame count"),
    (lambda f: patch(f, 52, "<I", 3), "generation"),
    (lambda f: patch(f, 56, "<I", 1), "mask"),
    (lambda f: patch(f, 60, "<I", 2558), "payload"),
    (lambda f: f[:-2], "bytes"),
    (lambda f: f[:40], "shorter"),
])
def test_malformed_mic_frames_raise_protocol_error(mutate, needle):
    with pytest.raises(ProtocolError, match=needle):
        at.parse_packet(mutate(mic(0)))


def test_cell_packet_off_the_512_grid_is_rejected():
    frame = patch(cells(1024, [-100]), 24, "<Q", 1000)
    with pytest.raises(ProtocolError, match="cell boundary"):
        at.parse_packet(frame)


@pytest.mark.parametrize("e, flags, needle", [
    ([-12001], [0], "outside"), ([1], [0], "outside"),
    ([-100], [4], "undefined"), ([-100], [1], "gap cell"),
])
def test_invalid_cell_records_are_rejected(e, flags, needle):
    rec = np.zeros(1, dtype=at.CELL_RECORD)
    rec["e"], rec["flags"] = e, flags
    with pytest.raises(ProtocolError, match=needle):
        at.parse_cells(rec.tobytes())


def test_afe_records_parse_every_field_and_cover_1280_samples_each():
    recs = [afe_rec(10, flags=at.AFE_FLAG_SYNC | at.AFE_FLAG_MIC_CLIPPED, playback=7, erle_max=26, erle_mean=12,
                    erle_frames=5, dtd_max=31, dtd_frames=2, rms_max=230, rms_mean=220, vad_max=3,
                    vad_frames=4, volume=127, lost=255),
            afe_rec(0, rms_max=0, rms_mean=0, volume=0)]
    frame = afe(2560, recs, flags=at.FLAG_DISCONTINUITY)
    assert len(frame) == 64 + 2 * 14
    p = at.parse_packet(frame)
    assert p.kind is Kind.AFE and p.frame_count == 2 and p.end_sample == 2560 + 2 * 1280
    got = p.afe()
    assert [r.start for r in got] == [2560, 3840]
    assert got[0] == at.AfeRecord(2560, 10, 0b1010, 7, 26, 12, 5, 31, 2, 230, 220, 3, 4, 127, 255)
    assert got[0].sync and got[0].mic_clipped and not got[0].gap and not got[0].device_mute
    assert got[1].frames == 0                         # a period without data, carried as such
    with pytest.raises(TypeError):
        p.pcm()


def test_afe_packet_off_the_1280_grid_is_rejected():
    frame = patch(afe(2560, [afe_rec()]), 24, "<Q", 2048)
    with pytest.raises(ProtocolError, match="period boundary"):
        at.parse_packet(frame)


@pytest.mark.parametrize("field, value, needle", [
    ("frames", 11, "frames above"),
    ("flags", 1 << 6, "reserved"), ("flags", 1 << 7, "reserved"),
    ("playback", 11, "playback above"), ("erle_frames", 11, "erle_frames above"),
    ("dtd_frames", 11, "dtd_frames above"), ("vad_frames", 11, "vad_frames above"),
    ("dtd_max", 32, "dtd_max above"), ("vad_max", 4, "vad_max above"), ("volume", 128, "volume above"),
])
def test_invalid_afe_records_are_rejected(field, value, needle):
    rec = np.zeros(1, dtype=at.AFE_RECORD)
    rec["frames"] = 10
    rec[field] = value
    with pytest.raises(ProtocolError, match=needle):
        at.parse_afe(rec.tobytes())


def test_afe_counts_above_a_periods_frames_are_rejected_even_below_ten():
    rec = np.zeros(1, dtype=at.AFE_RECORD)
    rec["frames"], rec["vad_frames"] = 3, 4
    with pytest.raises(ProtocolError, match="vad_frames"):
        at.parse_afe(rec.tobytes())


@pytest.mark.parametrize("mutate, needle", [
    (lambda f: patch(f, 7, "<B", 2), "format"),
    (lambda f: patch(f, 48, "<I", 126), "frame count"),
    (lambda f: patch(f, 60, "<I", 13), "payload"),
    (lambda f: f[:-1], "bytes"),
])
def test_malformed_afe_frames_raise_protocol_error(mutate, needle):
    with pytest.raises(ProtocolError, match=needle):
        at.parse_packet(mutate(afe(0, [afe_rec()])))


def test_afe_stream_open_must_name_kind_5_format_3():
    s = at.UplinkSession(afe_metadata=True)
    assert s.stream_open({"stream_id": "afe", "epoch": "5", "kind": 5, "sample_rate": 16000,
                          "format": 3, "reason": "start"}) == ("afe", 5)
    with pytest.raises(ProtocolError, match="parameters"):
        s.stream_open({"stream_id": "afe", "epoch": "6", "kind": 4, "sample_rate": 16000, "format": 2})


# -- stream epochs ---------------------------------------------------------------

def test_packet_for_unopened_or_ended_epoch_is_a_protocol_error():
    now = [0.0]
    s = opened_session(now)
    with pytest.raises(ProtocolError, match="never opened"):
        s.ingest(mic(0, epoch=999))
    s.stream_end({"stream_id": "mic", "epoch": str(MIC_EPOCH), "final_sample": "1280", "reason": "privacy"})
    with pytest.raises(ProtocolError, match="ended"):
        s.ingest(mic(0))


def test_stream_open_must_match_the_stream_format():
    s = at.UplinkSession()
    with pytest.raises(ProtocolError):
        s.stream_open({"stream_id": "mic", "epoch": "5", "kind": 1, "sample_rate": 48000, "format": 1})
    with pytest.raises(ProtocolError, match="decimal string"):
        s.stream_open({"stream_id": "mic", "epoch": 5, "kind": 1, "sample_rate": 16000, "format": 1})


def test_sequence_must_increase_and_samples_may_not_go_back_without_a_backfill():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "diagnostic", "d", MIC_EPOCH, {"mic": None})
    s.ingest(mic(12800, seq=0))       # the lease's run starts here
    with pytest.raises(ProtocolError, match="sequence"):
        s.ingest(mic(14080, seq=0))
    s.ingest(mic(14080, seq=1))
    with pytest.raises(ProtocolError, match="backfill"):
        s.ingest(mic(0, seq=2))


def test_second_lease_backfill_may_interleave_with_live_packets():
    now = [0.0]
    s = opened_session(now)
    s.open("A", "diagnostic", "d", MIC_EPOCH, {"mic": None})
    s.ingest(mic(25600, seq=0))
    s.open("B", "turn", "t", MIC_EPOCH, {"mic": 12800})
    s.ingest(mic(12800, seq=1))       # B's backfill run starts
    s.ingest(mic(26880, seq=2))       # live, interleaved
    s.ingest(mic(14080, seq=3))       # backfill continues in order
    with pytest.raises(ProtocolError):
        s.ingest(mic(1280, seq=4))    # a second run nobody granted


# -- leases -----------------------------------------------------------------

def test_candidate_lease_starts_on_the_cell_and_wire_offsets():
    table = at.LeaseTable(now=lambda: 0.0)
    lease = table.open_candidate("L", "C", MIC_EPOCH, support_start=300_000)
    assert lease.streams == {"mic": 294_912, "cells": 134_656, "reference": 254_912}
    assert lease.generation == 1 and lease.ack is at.AckState.PENDING and not lease.active


def test_an_opted_in_candidate_lease_wants_afe_from_its_reference_start_on_the_period_grid():
    table = at.LeaseTable(now=lambda: 0.0)
    lease = table.open_candidate("L", "C", MIC_EPOCH, support_start=300_000, afe=True)
    # mic 294,912 − 40,000 = 254,912 → 254,720 on the 1280 grid
    assert lease.streams == {"mic": 294_912, "cells": 134_656, "reference": 254_912, "afe": 254_720}
    assert lease.streams["afe"] % 1280 == 0
    early = table.open_candidate("E", "C2", MIC_EPOCH, support_start=10_000, afe=True)
    assert early.streams["afe"] == 0
    s = opened_session([0.0], afe_metadata=True)
    assert s.candidate("S", "C3", MIC_EPOCH, 300_000).streams["afe"] == 254_720
    assert "afe" not in opened_session([0.0]).candidate("S", "C3", MIC_EPOCH, 300_000).streams


def test_controller_lease_rounds_the_afe_start_down_to_the_period_grid():
    t = at.LeaseTable(now=lambda: 0.0)
    lease, msg = t.open("L", "turn", "T", MIC_EPOCH, {"mic": 1000, "afe": 3000})
    assert lease.streams["afe"] == 2560 and msg.body["streams"]["afe"] == "2560"
    _, live = t.open("R", "reply", "R", MIC_EPOCH, {"mic": None, "afe": None})
    assert live.body["streams"]["afe"] == "live"


def test_afe_records_land_in_the_leases_of_their_capture_epoch_from_the_lease_start():
    s = opened_session([0.0], afe_metadata=True)
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": None, "afe": 3840})
    got = s.ingest(afe(1280, [afe_rec(10, volume=v) for v in (1, 2, 3, 4)], seq=0))
    assert [(d.lease_id, d.stream_id, d.ranges) for d in got] == [("L", "afe", ((3840, 6400),))]
    store = s.timelines["L"].afe
    assert [(r.start, r.volume) for r in store.read(0, 10_000)] == [(3840, 3), (5120, 4)]
    # A period nobody sent has no record — never a zero record.
    s.ingest(afe(8960, [afe_rec(10, volume=9)], seq=1, flags=at.FLAG_DISCONTINUITY))
    assert [r.start for r in store.read(0, 20_000)] == [3840, 5120, 8960]
    assert store.read(6400, 8960) == [] and store.read(7000, 7001) == []
    assert [r.start for r in store.read(6000, 9000)] == [5120, 8960]       # periods overlapping the span
    s.clear(at.LeaseEnd.MUTE)
    assert store.read(0, 20_000) == []                 # erased with the lease's audio


def test_afe_of_another_capture_epoch_is_not_delivered():
    s = opened_session([0.0], afe_metadata=True)
    s.stream_open({"stream_id": "afe", "epoch": "999", "kind": 5, "sample_rate": 16000, "format": 3})
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": None, "afe": None})
    frame = at.build_packet(Kind.AFE, epoch=999, sequence=0, first_sample=0, afe=at.build_afe([afe_rec()]))
    assert s.ingest(frame) == []


def test_audio_for_an_unacknowledged_candidate_lease_is_dropped():
    now = [0.0]
    s = opened_session(now)
    s.candidate("L", "C", MIC_EPOCH, 20_000)
    assert s.ingest(mic(15360, seq=0)) == []
    s.acknowledge("L", accepted=True)
    got = s.ingest(mic(16640, seq=1))
    assert [(d.lease_id, d.ranges) for d in got] == [("L", ((16640, 17920),))]


def test_unacknowledged_candidate_ends_by_ttl_after_one_second():
    now = [10.0]
    t = at.LeaseTable(now=lambda: now[0])
    t.open_candidate("L", "C", MIC_EPOCH, 50_000)
    now[0] = 10.9
    assert t.expire() == []
    now[0] = 11.0
    assert [l.ended for l in t.expire()] == ["ttl"]


def test_conversion_to_turn_bumps_generation_and_emits_renew():
    t = at.LeaseTable(now=lambda: 0.0)
    t.open_candidate("L", "C", MIC_EPOCH, 50_000)
    with pytest.raises(at.LeaseError):
        t.convert_to_turn("L", "T")                 # not acknowledged yet
    t.acknowledge("L", True)
    msg = t.convert_to_turn("L", "T")
    assert msg == at.LeaseMessage("uplink.renew", 2, {"lease_id": "L", "ttl_ms": 3000, "reason": "turn", "owner": "T"})
    assert t.get("L").reason is at.LeaseReason.TURN and t.get("L").owner == "T"


def test_controller_lease_open_message_uses_decimal_strings_and_live():
    t = at.LeaseTable(now=lambda: 0.0)
    lease, msg = t.open("L", "turn", "T", MIC_EPOCH, {"mic": 1000, "cells": 900, "reference": 1234})
    assert msg.type == "uplink.open" and msg.generation == 1
    assert msg.body["streams"] == {"mic": "512", "cells": "512", "reference": "1234"}
    _, msg = t.open("R", "reply", "E", MIC_EPOCH, {"mic": None, "reference": None, "cells": 0})
    assert msg.body["streams"] == {"mic": "live", "reference": "live", "cells": "0"}
    with pytest.raises(at.LeaseError):
        t.open("X", "candidate", "C", MIC_EPOCH, {"mic": None})


def test_renewals_are_due_each_second_and_ttl_is_three_seconds():
    now = [0.0]
    t = at.LeaseTable(now=lambda: now[0])
    t.open("L", "turn", "T", MIC_EPOCH, {"mic": None})
    now[0] = 0.99
    assert t.due_renewals() == []
    now[0] = 1.0
    assert [l.lease_id for l in t.due_renewals()] == ["L"]
    t.renew("L")
    now[0] = 3.99
    assert t.expire() == []
    now[0] = 4.0
    assert [l.ended for l in t.expire()] == ["ttl"]


def test_diagnostic_lease_is_not_renewed_past_thirty_minutes():
    now = [0.0]
    t = at.LeaseTable(now=lambda: now[0])
    t.open("D", "diagnostic", "rec", MIC_EPOCH, {"mic": None})
    now[0] = 30 * 60
    assert t.due_renewals() == []
    with pytest.raises(at.LeaseError):
        t.renew("D")


def test_close_and_device_end_stop_delivery():
    now = [0.0]
    s = opened_session(now)
    msg = s.open("L", "turn", "T", MIC_EPOCH, {"mic": None})
    assert msg.body["reason"] == "turn"
    assert s.leases.close("L", "committed").body == {"lease_id": "L", "reason": "committed"}
    assert s.ingest(mic(0)) == []
    s.open("M", "turn", "T", MIC_EPOCH, {"mic": None})
    lease = s.leases.device_ended({"lease_id": "M", "reason": "overrun", "last_sample": {"mic": "1280"},
                                   "clipped_start": {"mic": None}})
    assert lease.ended == "overrun" and lease.last_sample == {"mic": 1280} and lease.clipped_start == {"mic": None}
    assert s.ingest(mic(1280, seq=1)) == []


def test_device_end_of_a_lease_that_sent_nothing_has_null_last_sample():
    # The device reports null for a wanted stream it never sent (e.g. a
    # candidate lease that expired unacknowledged); that is not malformed.
    s = opened_session([0.0])
    s.open("N", "turn", "T", MIC_EPOCH, {"mic": None, "cells": None})
    lease = s.leases.device_ended({"lease_id": "N", "reason": "ttl",
                                   "last_sample": {"mic": None, "cells": None},
                                   "clipped_start": {"mic": None, "cells": None}})
    assert lease.ended == "ttl" and lease.last_sample == {"mic": None, "cells": None}


def test_packet_for_a_stream_no_lease_wants_is_dropped():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "diagnostic", "d", MIC_EPOCH, {"mic": None})
    assert s.ingest(ref(0)) == []
    assert s.ingest(cells(0, [-100])) == []


def test_session_loss_ends_every_lease_and_erases_audio():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "T", MIC_EPOCH, {"mic": None})
    s.ingest(mic(0))
    assert [l.ended for l in s.clear("session")] == ["session"]
    assert s.timelines == {}
    with pytest.raises(ProtocolError, match="never opened"):
        s.ingest(mic(1280, seq=1))


# -- timelines ----------------------------------------------------------------

def test_first_copy_of_each_sample_wins_across_overlapping_leases():
    now = [0.0]
    s = opened_session(now)
    s.open("A", "diagnostic", "d", MIC_EPOCH, {"mic": None})
    s.ingest(mic(0, value=1, seq=0))
    s.open("B", "turn", "t", MIC_EPOCH, {"mic": 0})
    got = s.ingest(mic(640, value=2, seq=1))
    by_lease = {d.lease_id: d.ranges for d in got}
    assert by_lease == {"A": ((1280, 1920),), "B": ((640, 1920),)}
    a = s.timelines["A"].mic
    assert (a.read(0, 1280) == 1).all() and (a.read(1280, 1920) == 2).all()


def test_gap_is_an_explicit_missing_range_never_silence():
    tl = at.SampleTimeline(capacity=10_000)
    tl.write(0, np.ones(1280, np.int16))
    tl.write(2560, np.ones(1280, np.int16))
    assert tl.missing(0, 3840) == [(1280, 2560)]
    assert not tl.covers(0, 3840)
    with pytest.raises(KeyError):
        tl.read(1000, 3000)


def test_backfill_start_before_the_lease_start_is_cut_at_the_lease_start():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": 1024, "cells": 1024})
    got = s.ingest(mic(0))
    assert got[0].ranges == ((1024, 1280),)
    got = s.ingest(cells(0, [-100, -200, -300, -400]))
    assert got[0].ranges == ((1024, 2048),)
    c = s.timelines["L"].cells.read(1024, 2048)
    assert list(c.e_centi) == [-300, -400]


def test_digital_silence_reference_is_known_zeros():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "t", MIC_EPOCH, {"reference": 0})
    s.ingest(ref(0, silence=True))
    s.ingest(ref(1280, seq=1, value=5))
    view = s.timelines["L"].reference.segment(REF_EPOCH)
    assert view.is_silent(0, 1280) and not view.is_silent(0, 2560)
    assert (view.read(0, 1280) == 0).all() and (view.read(1280, 2560) == 5).all()


def test_muted_mic_audio_is_not_stored_as_evidence():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": None})
    got = s.ingest(mic(0, flags=at.FLAG_MUTED))
    assert got[0].ranges == () and got[0].packet.muted
    tl = s.timelines["L"].mic
    assert not tl.covers(0, 1280) and tl.muted.covers(0, 1280)


def test_reference_epoch_restart_keeps_both_epochs_addressable_in_one_ring():
    r = at.ReferenceTimeline(capacity=10_000)
    r.write(1, 0, np.full(1280, 1, np.int16), 1280)
    r.write(2, 50, np.full(1280, 2, np.int16), 1280)
    assert r.epochs == (1, 2)
    assert (r.segment(1).read(0, 1280) == 1).all()
    assert (r.segment(2).read(50, 1330) == 2).all()
    assert r.segment(2).first_sample == 50 and r.segment(2).frontier == 1330
    # the old epoch is closed where the new one began: late audio is dropped
    assert r.write(1, 1280, np.full(1280, 9, np.int16), 1280) == []
    r.write(2, 1330, np.zeros(9000, np.int16), 9000)
    assert r.epochs == (2,) and r.nbytes == 20_000


def test_retention_keeps_rolling_ten_seconds_before_an_open_utterance():
    lease = at.Lease("L", at.LeaseReason.TURN, "t", 1, MIC_EPOCH, {"mic": None, "cells": None}, 3000, 0, 0)
    tl = at.LeaseTimeline(lease)
    block = np.zeros(1280, np.int16)
    for k in range(0, 200 * 1280, 1280):
        tl.mic.write(k, block)
        tl._retain()
    assert tl.mic.floor == 200 * 1280 - 160_000          # idle: rolling 10 s
    tl.set_utterance_start(190 * 1280)
    for k in range(200 * 1280, 500 * 1280, 1280):
        tl.mic.write(k, block)
        tl._retain()
    # utterance start − 10 s is older than what was kept: nothing more ages out
    assert tl.mic.floor == 200 * 1280 - 160_000
    assert tl.mic.covers(200 * 1280 - 160_000, 500 * 1280)
    tl.set_utterance_start(None)
    assert tl.mic.floor == 500 * 1280 - 160_000


def test_lease_memory_stays_under_the_2_6_mb_bound():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": None, "reference": None, "cells": None})
    seq = 0
    for k in range(0, 60 * 16000, 1280):
        s.ingest(mic(k, seq=seq))
        s.ingest(ref(k, seq=seq))
        seq += 1
    for k in range(0, 60 * 16000, 320 * 512):
        s.ingest(cells(k, [-100] * 320, seq=k))
    assert s.timelines["L"].nbytes <= at.LEASE_BYTES_CAP


# -- clocks -------------------------------------------------------------------

def test_clock_map_relates_capture_and_reference_through_monotonic_time():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": None, "reference": None})
    base_mic, base_ref = 5_000_000_000, 5_250_000_000       # reference epoch began 250 ms later
    for i in range(20):
        k = i * 1280
        s.ingest(mic(k, seq=i, mono_ns=base_mic + k * 62_500, unc=80_000))
        s.ingest(ref(k, seq=i, mono_ns=base_ref + k * 62_500, unc=1_000))
    cmap = s.clock_map(MIC_EPOCH, REF_EPOCH)
    m = cmap.capture_to_reference(16000 + 4000)
    assert m.sample == pytest.approx(16000, abs=1)
    assert m.uncertainty_samples == pytest.approx((80_000 + 1_000) * 1e3 / 62_500, rel=0.01)
    assert cmap.reference_to_capture(16000).sample == pytest.approx(20000, abs=1)


def test_clock_fit_rejects_outlier_anchor_and_refuses_off_nominal_rate():
    c = at.StreamClock()
    for i in range(10):
        k = i * 1280
        mono = 1_000_000_000 + k * 62_500 + (500_000_000 if i == 5 else 0)
        c.add(at.parse_packet(mic(k, seq=i, mono_ns=mono, unc=10)))
    fit = c.fit()
    assert fit.ns_per_sample == pytest.approx(62_500, rel=1e-6)
    fast = at.StreamClock()
    for i in range(10):
        k = i * 1280
        fast.add(at.parse_packet(mic(k, seq=i, mono_ns=1 + int(k * 62_500 * 1.002), unc=10)))
    assert fast.fit() is None


def test_timing_without_an_anchor_is_unknown():
    now = [0.0]
    s = opened_session(now)
    s.open("L", "turn", "t", MIC_EPOCH, {"mic": None})
    s.ingest(mic(0))
    assert s.clock_fit("mic", MIC_EPOCH) is None
    assert s.clock_map(MIC_EPOCH, REF_EPOCH) is None
