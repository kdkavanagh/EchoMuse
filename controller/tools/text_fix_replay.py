"""Replay transcript correction (em_text_fix, SPEC §16.8) over the household's own turns.

    text_fix_replay.py record SNAPSHOT [--db /app/data/echomuse.db]
    text_fix_replay.py replay SNAPSHOT [--held-out 0.3] [--labels LABELS.json] [--show N]

`record` runs inside the controller container (it needs HA_URL/HA_TOKEN or the
Supervisor token, and the database). It freezes HA's trigger templates and vocabulary,
every HA-routed turn with its HA device and history provenance, and HA's verdict on each
original and on every candidate the policy proposes, asked of HA's non-executing debug
matcher (`conversation/agent/homeassistant/debug`) in the pipeline's language. Nothing is
executed. The snapshot holds household data: keep it out of the repository.

`replay` needs neither HA nor models: it re-runs the policy on the snapshot, each turn
with only the history of earlier turns, and reports by reason, rule, source and device,
separately for the earlier (tuning) and later (held-out) turns. A candidate the snapshot
has no verdict for is reported as unrecorded: record again after changing the policy.
`--labels` is a JSON object {turn id: intended command}; an applied correction is then
counted right (its letters equal the label's) or wrong.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sqlite3
import sys
from collections import Counter
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import em_ha_client as ha_mod  # noqa: E402
import em_text_fix as tf  # noqa: E402

TURNS_SQL = ("SELECT id, ts, device_id, trigger_type, stt_text, outcome, intent_local, response_type, {fix} "
             "FROM turns WHERE stt_text IS NOT NULL AND stt_text != '' ORDER BY id")
WAKE_PHRASE = "ophelia"
Snapshot = dict[str, Any]


def _vocabulary(raw: Mapping[str, Any]) -> ha_mod.Vocabulary:
    return ha_mod.Vocabulary(
        entities=tuple(ha_mod.VocabEntity(e, d, tuple(n), a) for e, d, n, a in raw["entities"]),
        areas=tuple(ha_mod.VocabArea(a, tuple(n), f) for a, n, f in raw["areas"]),
        floors=tuple(ha_mod.VocabFloor(f, tuple(n)) for f, n in raw["floors"]))


def _history(turns: Sequence[Mapping[str, Any]], before: int) -> list[tf.HistoryEntry]:
    return [tf.HistoryEntry(tf.sent_text(t["stt_text"], t["stt_fix"]), t["id"], t["ts"]) for t in turns
            if t["id"] < before and tf.history_eligible(t["outcome"], t["intent_local"], t["response_type"])]


def _request(turn: Mapping[str, Any]) -> tf.Request:
    return tf.Request(tf.fix_key(turn["stt_text"]), WAKE_PHRASE if turn["trigger_type"] == "wake" else None)


async def record(path: Path, db_path: str) -> None:
    client = ha_mod.HaClient(ha_mod.HaEndpoint.from_env() or sys.exit("HA is not configured"))
    await client.start()
    await client.wait_connected(15)
    vocab = await client.refresh_vocabulary()
    templates = await client.trigger_sentences()
    pipelines = await client.list_pipelines()
    language = await client.pipeline_language(pipelines.preferred_id) if pipelines.preferred_id else None
    by_mac: dict[str, str] = {}
    for entry in await client.command({"type": "config/device_registry/list"}):
        for kind, value in entry.get("connections") or []:
            if kind == "mac":
                by_mac[ha_mod.format_mac(value)] = entry["id"]
    from em_esphome import serialno_to_mac
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    columns = {r[1] for r in con.execute("PRAGMA table_info(turns)")}
    fix = "stt_fix" if "stt_fix" in columns else "NULL AS stt_fix"   # a database from before schema 34
    turns = [dict(r) for r in con.execute(TURNS_SQL.format(fix=fix))]
    devices = {t["device_id"]: by_mac.get(ha_mod.format_mac(serialno_to_mac(t["device_id"]))) for t in turns}
    base = tf.build_tables(vocab, templates, [], 0)
    user = tf.user_vocabulary((), ())
    verdicts: dict[str, dict[str, Any]] = {}

    async def ask(sentences: Sequence[str], device: str | None) -> None:
        todo = sorted({s for s in sentences if s not in verdicts.setdefault(device or "", {})})
        for i in range(0, len(todo), 25):
            batch = todo[i:i + 25]
            msg: dict[str, object] = {"type": "conversation/agent/homeassistant/debug", "sentences": batch,
                                      "device_id": device}
            if language:
                msg["language"] = language
            result = await client.command(msg, timeout=60)
            verdicts[device or ""].update(zip(batch, result["results"]))

    for turn in turns:
        if turn["outcome"] != "ha":
            continue
        device = devices.get(turn["device_id"])
        request = _request(turn)
        await ask([request.text], device)
        original = ha_mod._sentence_recognition(verdicts[device or ""][request.text])
        if original is not None and original.match:
            continue
        tables = tf.Tables(**{**base.__dict__, "history": tf.build_history(_history(turns, turn["id"]))})
        proposal = tf.propose(request, original, tables, user)
        await ask([c.text for c in proposal.validate], device)
    await client.close()
    snapshot = {"language": language, "templates": templates,
                "vocabulary": {"entities": [[e.entity_id, e.domain, list(e.names), e.area_id] for e in vocab.entities],
                               "areas": [[a.area_id, list(a.names), a.floor_id] for a in vocab.areas],
                               "floors": [[f.floor_id, list(f.names)] for f in vocab.floors]},
                "devices": devices, "turns": turns, "verdicts": verdicts}
    path.write_text(json.dumps(snapshot))
    print(f"recorded {len(turns)} turns, {sum(len(v) for v in verdicts.values())} verdicts → {path}")


class Unrecorded(Exception):
    pass


def replay(path: Path, held_out: float, labels: Mapping[str, str], show: int) -> None:
    snap: Snapshot = json.loads(path.read_text())
    turns = [t for t in snap["turns"] if t["outcome"] == "ha"]
    base = tf.build_tables(_vocabulary(snap["vocabulary"]), snap["templates"], [], 0)
    user = tf.user_vocabulary((), ())
    split = turns[int(len(turns) * (1 - held_out))]["id"] if turns and held_out > 0 else None
    groups: dict[str, list[dict[str, Any]]] = {"tuning": [], "held-out": []}
    for turn in turns:
        device = snap["devices"].get(turn["device_id"]) or ""
        recorded = snap["verdicts"].get(device, {})

        def verdict(text: str) -> ha_mod.SentenceRecognition | None:
            if text not in recorded:
                raise Unrecorded(text)
            return ha_mod._sentence_recognition(recorded[text])

        request = _request(turn)
        row: dict[str, Any] = {"id": turn["id"], "device": turn["device_id"][-4:], "text": request.text}
        try:
            original = verdict(request.text)
            tables = tf.Tables(**{**base.__dict__, "history": tf.build_history(_history(snap["turns"], turn["id"]))})
            proposal = tf.propose(request, original, tables, user)
            decision = tf.decide(proposal, [verdict(c.text) for c in proposal.validate], original, tables)
        except Unrecorded as err:
            row["reason"] = f"unrecorded: {err}"
        else:
            row["reason"] = decision.reason
            shown = decision.selected or decision.proposed
            if shown is not None:
                c = shown.candidate
                row.update(proposed=c.text, rule=c.rule, source=c.source, evidence=round(c.evidence, 3),
                           status=shown.status)
            label = labels.get(str(turn["id"]))
            if decision.selected is not None and label is not None:
                row["right"] = tf.letters(label) == tf.letters(decision.selected.candidate.text)
        groups["held-out" if split is not None and turn["id"] >= split else "tuning"].append(row)
    for name, rows in groups.items():
        if not rows:
            continue
        print(f"\n== {name}: turns {rows[0]['id']}–{rows[-1]['id']} ({len(rows)})")
        print("  reasons:", dict(Counter(str(r["reason"]) for r in rows).most_common()))
        applied = [r for r in rows if r["reason"] == tf.Reason.APPLIED]
        print("  applied by rule:", dict(Counter(str(r["rule"]) for r in applied)),
              "by source:", dict(Counter(str(r["source"]) for r in applied)),
              "by device:", dict(Counter(r["device"] for r in applied)))
        judged = [r for r in applied if "right" in r]
        if judged:
            print(f"  labelled: {sum(r['right'] for r in judged)} right, {sum(not r['right'] for r in judged)} wrong"
                  f" of {len(judged)}")
        interesting = [r for r in rows if r["reason"] not in (tf.Reason.ORIGINAL_MATCHED, tf.Reason.NO_CANDIDATE)]
        for r in interesting[:show]:
            print(f"  {r['id']:>5} {r['device']} {r['reason']:<20} {r['text']!r} → {r.get('proposed')!r}"
                  f" ({r.get('rule')}/{r.get('source')} {r.get('evidence')} {r.get('status')})"
                  + ("" if "right" not in r else (" RIGHT" if r["right"] else " WRONG")))


def main(argv: Sequence[str]) -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    rec = sub.add_parser("record")
    rec.add_argument("snapshot", type=Path)
    rec.add_argument("--db", default="/app/data/echomuse.db")
    rep = sub.add_parser("replay")
    rep.add_argument("snapshot", type=Path)
    rep.add_argument("--held-out", type=float, default=0.3)
    rep.add_argument("--labels", type=Path)
    rep.add_argument("--show", type=int, default=50)
    args = parser.parse_args(argv)
    if args.command == "record":
        asyncio.run(record(args.snapshot, args.db))
    else:
        labels = json.loads(args.labels.read_text()) if args.labels else {}
        replay(args.snapshot, args.held_out, labels, args.show)


if __name__ == "__main__":
    main(sys.argv[1:])
