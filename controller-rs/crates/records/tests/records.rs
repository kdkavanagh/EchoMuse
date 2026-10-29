//! Records against temporary databases built from the contract schema, with synthetic rows
//! and audio only.

use std::{collections::HashMap, fs, path::Path, sync::Arc, time::Duration};

use echomuse_core::{services::ConversationId, types::DeviceId};
use echomuse_db::{DatabaseService, DbHandle, OpenedDatabase, ServiceConfig};
use echomuse_protocol::{
    session::{OpenRule, RuleCombine, RuleProfile},
    vocab::Sha256,
};
use echomuse_records::{
    AfeEvidence, AfeSeries, ClipWindows, ContinuationUpdate, DeviceLogEntry, GetNearMiss, GetTurn,
    InsertTurn, IntentColumns, JsonObject, NearMissAudio, NearMissRecord, NearMissUuid, PolicyHash,
    Publication, RecordingKind, RecordingsLayout, RecordsError, RecordsService, ResponseType,
    RetentionLimits, RttSample, Score, ShadowEvent, ShadowStats, StatsReport, SttFix, TurnAudio,
    TurnKey, TurnRecord, UnixMillis, VerdictUpdate, WakeContext, WakeModelName, WakeStatsReport,
    WallSeconds,
    near_misses::InsertNearMiss,
    recordings::{wav_bytes, write_atomically},
    rollups::NetDelta,
    vocab::{
        Continuation, ContinuationEnd, FollowUp, LogLevel, LogSource, NearMissEnd,
        NearMissRecording, NearMissVerdict, PlaybackReason, ShadowEventKind, Stored,
        TerminalReason, TriggerType, TurnOutcome, WakeAttribution,
    },
};
use echomuse_turn::{
    CloseCause, CloseReason, Route, Sample, SampleSpan, TextClass, TextSource, UtteranceTrace,
    VerifierResult, WakeCandidate, WakeDecider, WakeHop, WakePolicy, WakeProfile,
    endpoint::{EndBound, TurnConfig},
    time::MonoInstant,
    trace::{EndpointEvent, EvidenceCounts, TraceKind},
    wake::ArrivalConditions,
};
use rusqlite::{Connection, types::Value};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

const DEV: &str = "G0SYNTH0001";
const OTHER: &str = "G0SYNTH0002";
const GRAPH: &str = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f";

struct Harness {
    dir: TempDir,
    service: Option<DatabaseService>,
    db: DbHandle,
    records: RecordsService,
}

impl Harness {
    fn new(limits: RetentionLimits) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("echomuse.db");
        let opened = OpenedDatabase::open(&path).unwrap();
        let service = DatabaseService::start(opened, ServiceConfig::default()).unwrap();
        let db = service.handle();
        let conn = Connection::open(&path).unwrap();
        for d in [DEV, OTHER] {
            conn.execute("INSERT INTO devices (device_id) VALUES (?1)", [d])
                .unwrap();
        }
        let records =
            RecordsService::new(db.clone(), RecordingsLayout::beside_database(&path), limits);
        Self {
            dir,
            service: Some(service),
            db,
            records,
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(self.dir.path().join("echomuse.db")).unwrap()
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn file(&self, kind: RecordingKind, device: &str, id: i64) -> std::path::PathBuf {
        self.records.layout().path(kind, &dev(device), id)
    }

    async fn close(mut self) {
        self.records.shutdown().await;
        self.service
            .take()
            .unwrap()
            .shutdown(Duration::from_secs(5))
            .await
            .unwrap();
    }
}

fn dev(id: &str) -> DeviceId {
    DeviceId(id.to_owned())
}

fn pcm(samples: usize) -> Arc<[i16]> {
    (0..samples).map(|i| (i % 200) as i16 - 100).collect()
}

fn ts(seconds: f64) -> WallSeconds {
    WallSeconds::new(seconds).unwrap()
}

fn policy() -> PolicyHash {
    PolicyHash::new("post_afe_10+wyoming:default").unwrap()
}

/// Every column of a row: its SQLite type and value.
fn row(conn: &Connection, table: &str, id: i64) -> HashMap<String, Value> {
    let mut statement = conn
        .prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))
        .unwrap();
    let names: Vec<String> = statement.column_names().map(str::to_owned).collect();
    statement
        .query_row([id], |r| {
            Ok(names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), r.get::<_, Value>(i).unwrap()))
                .collect())
        })
        .unwrap()
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn real(v: &Value) -> f64 {
    match v {
        Value::Real(r) => *r,
        other => panic!("expected real, got {other:?}"),
    }
}

fn hop(end: u64, raw: f64, smoothed: f64) -> WakeHop {
    WakeHop {
        end: Sample(end),
        raw: Some(raw),
        smoothed: Some(smoothed),
    }
}

fn candidate(id: &str, producing_sound: bool) -> WakeCandidate {
    WakeCandidate {
        candidate_id: id.to_owned(),
        profile: if producing_sound {
            WakeProfile::Playback
        } else {
            WakeProfile::Idle
        },
        threshold: if producing_sound { 0.65 } else { 0.9 },
        producing_sound,
        first_crossing_end: Sample(1_456_061_440),
        support_start: Sample(1_456_036_480),
        hops: vec![
            hop(1_456_058_880, 0.97, 0.48),
            hop(1_456_061_440, 0.998, 0.81),
            hop(1_456_064_000, 0.999, 0.99),
        ],
    }
}

fn ms(n: u64) -> MonoInstant {
    MonoInstant(n * 1_000_000)
}

/// An idle candidate, accepted on arrival.
fn accepted_idle(id: &str) -> WakeDecider {
    let d = WakeDecider::arrive(
        candidate(id, false),
        ArrivalConditions::default(),
        WakePolicy::default(),
        ms(0),
    );
    assert!(d.decision().is_some());
    d
}

/// A candidate heard during playback whose verifier heard something else.
fn unverified(id: &str) -> WakeDecider {
    let mut d = WakeDecider::arrive(
        candidate(id, true),
        ArrivalConditions::default(),
        WakePolicy::default(),
        ms(0),
    );
    d.reference_scored(echomuse_turn::ReferenceScore::NoOverlap, ms(100));
    d.echo_compared(echomuse_turn::CandidateEcho::NearEndPresent, ms(150));
    d.verified(VerifierResult::Text("bananas".to_owned()), ms(400));
    assert!(d.decision().is_some());
    d
}

fn context(peak: Option<f64>) -> WakeContext {
    WakeContext {
        model: WakeModelName::new("ophelia"),
        graph_sha256: Sha256::parse(GRAPH).unwrap(),
        candidate_end_peak: peak,
    }
}

/// A committed wake utterance: 77,376 samples (4,836 ms) of audio, committed 32,768 samples
/// (2,048 ms) after its boundary, 223 cells of which 42 lacked full reference coverage.
fn committed_utterance() -> UtteranceTrace {
    let start = 1_456_031_680;
    let boundary = 1_456_105_984;
    UtteranceTrace {
        config: TurnConfig::default(),
        kind: TraceKind::Wake,
        epoch: 7,
        start: Sample(start),
        trigger: Sample(1_456_064_000),
        guard_until: None,
        command_start: Some(Sample(1_456_064_512)),
        answer_start: None,
        foreground_db: Some(-51.8),
        foreground_seeded: true,
        command_cells: 40,
        evidence: EvidenceCounts {
            cells: 223,
            reference_incomplete: 42,
            ..EvidenceCounts::default()
        },
        segments: Vec::new(),
        streaming_text: "Ophelia. What's the score of the game?".to_owned(),
        heard: "Ophelia. What's the score of the game?".to_owned(),
        text: Vec::new(),
        finalized_at: Vec::new(),
        pauses: Vec::new(),
        recognizer: Vec::new(),
        endpoint: vec![EndpointEvent::Commit {
            route: Route::A,
            boundary: Sample(boundary),
            span: SampleSpan::new(Sample(start), Sample(start + 77_376)),
            end_bound: EndBound::Cell,
            last_word: None,
            at: Sample(boundary + 32_768),
            text: "what's the score of the game".to_owned(),
            class: Some(TextClass::Unknown),
            source: TextSource::Server,
        }],
        frontier: Some(Sample(boundary + 32_768)),
    }
}

/// A button utterance that heard nothing.
fn silent_button() -> UtteranceTrace {
    let mut u = committed_utterance();
    u.kind = TraceKind::Button;
    u.streaming_text.clear();
    u.evidence = EvidenceCounts::default();
    u.endpoint = vec![EndpointEvent::Close {
        reason: CloseReason::NoInput,
        cause: CloseCause::Limit,
        at: None,
    }];
    u
}

fn object(value: serde_json::Value) -> JsonObject {
    JsonObject::parse(&value.to_string()).unwrap()
}

fn full_turn(uuid: Uuid, commit_id: Uuid) -> TurnRecord {
    let wake = accepted_idle("cand-accepted");
    let utterance = committed_utterance();
    let mut r = TurnRecord::turn(
        dev(DEV),
        ts(1_793_235_185.803_92),
        uuid,
        TriggerType::Wake,
        Some((wake.trace(), context(Some(0.9995)))),
        Some((&utterance, Some(commit_id))),
        policy(),
        TerminalReason::Completed,
        Duration::from_millis(13_292),
    )
    .unwrap();
    r.outcome = Some(TurnOutcome::Ha);
    r.stt_raw = Some("Ophelia. What's the score of the game?".to_owned());
    r.stt_text = Some("What's the score of the game?".to_owned());
    r.stt = Some(Duration::from_millis(300));
    r.stt_fix = Some(SttFix(object(json!({"mode": "shadow", "applied": false}))));
    r.intent = Some(IntentColumns {
        elapsed: Duration::from_millis(3_379),
        local: Some(true),
    });
    r.response_type = ResponseType::new("query_answer");
    r.response_text = Some("They lead 1 to 0.".to_owned());
    r.conversation_id = Some(ConversationId("01M62JB14533KNVQD4WS77S7SW".to_owned()));
    r.tts_url = Some(Duration::from_millis(3_379));
    r.playback = Some(Duration::from_millis(3_745));
    r.playback_reason = Some(PlaybackReason::Drained);
    r.first_audio = Some(Duration::from_micros(4_815_900));
    r.response_latency = Some(Duration::from_millis(7_363));
    r.reply_to = Some(Uuid::from_u128(7));
    r.continuation = Some(Continuation::Pending);
    r.afe_evidence = Some(AfeEvidence(object(
        json!({"support_start": 1, "pre": null}),
    )));
    r.afe_series = Some(AfeSeries(object(json!({"start": 0, "period": 1280}))));
    r.wake_windows = Some(ClipWindows {
        graph_sha256: Sha256::parse(GRAPH).unwrap(),
        samples: 42_560,
        window: 22_400,
        scores: vec![(0, 0.0141), (2_560, 0.4789)],
    });
    r
}

const LEGACY_TURN_COLUMNS: [&str; 20] = [
    "noise_floor",
    "vad_end_ms",
    "tts_fetch_ms",
    "tts_bytes",
    "underruns",
    "playback_periods",
    "min_depth",
    "prime_wait_ms",
    "recv_span_ms",
    "max_gap_ms",
    "bytes_recv",
    "send_ms",
    "delivery_ms",
    "eq_ms",
    "dev_wake_score",
    "dev_wake_delta_ms",
    "dev_shadow",
    "dev_threshold",
    "ctrl_wake_score",
    "ctrl_wake_delta_ms",
];

#[tokio::test]
async fn a_turn_writes_every_column_in_its_stored_shape() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    let commit_id = Uuid::new_v4();
    let persisted = h
        .records
        .persist_turn(
            full_turn(uuid, commit_id),
            TurnAudio {
                utterance: Some(pcm(77_376)),
                wake_clip: Some(pcm(42_560)),
                turn_recording: None,
            },
        )
        .await
        .unwrap();
    assert!(persisted.insert.inserted);
    assert_eq!(persisted.utterance, Some(Publication::Saved));
    assert_eq!(persisted.wake_clip, Some(Publication::Saved));
    let id = persisted.insert.id.0;
    let r = row(&h.conn(), "turns", id);
    assert_eq!(r.len(), 63, "the contract's turns columns");

    assert_eq!(text(&r["device_id"]), DEV);
    assert!((real(&r["ts"]) - 1_793_235_185.803_92).abs() < 1e-6);
    assert_eq!(text(&r["trigger_type"]), "wake");
    assert_eq!(text(&r["wake_model"]), "ophelia");
    assert_eq!(
        real(&r["wake_score"]),
        0.9995,
        "the candidate end's peak wins"
    );
    assert_eq!(real(&r["wake_threshold"]), 0.9);
    assert_eq!(text(&r["wake_model_sha256"]), GRAPH);
    assert_eq!(text(&r["wake_attribution"]), "idle");
    assert_eq!(text(&r["outcome"]), "ha");
    assert_eq!(
        text(&r["stt_raw"]),
        "Ophelia. What's the score of the game?"
    );
    assert_eq!(text(&r["stt_text"]), "What's the score of the game?");
    assert_eq!(int(&r["total_ms"]), 13_292);
    assert_eq!(int(&r["stt_ms"]), 300);
    assert_eq!(int(&r["tts_url_ms"]), 3_379);
    assert_eq!(int(&r["playback_ms"]), 3_745);
    assert_eq!(int(&r["audio_ms"]), 4_836);
    assert_eq!(text(&r["policy_hash"]), "post_afe_10+wyoming:default");
    assert!((real(&r["reference_coverage"]) - 181.0 / 223.0).abs() < 1e-12);
    assert_eq!(text(&r["commit_route"]), "A");
    assert_eq!(text(&r["terminal_reason"]), "completed");
    assert_eq!(text(&r["commit_id"]), commit_id.to_string());
    assert_eq!(
        text(&r["asr_text"]),
        "Ophelia. What's the score of the game?"
    );
    assert_eq!(text(&r["endpoint_class"]), "unknown");
    assert_eq!(int(&r["endpoint_ms"]), 2_048);
    assert_eq!(int(&r["intent_ms"]), 3_379);
    assert_eq!(int(&r["intent_local"]), 1);
    assert_eq!(text(&r["response_type"]), "query_answer");
    assert_eq!(text(&r["response_text"]), "They lead 1 to 0.");
    assert_eq!(text(&r["turn_uuid"]), uuid.to_string());
    assert_eq!(text(&r["conversation_id"]), "01M62JB14533KNVQD4WS77S7SW");
    assert_eq!(text(&r["reply_to"]), Uuid::from_u128(7).to_string());
    assert_eq!(text(&r["continuation"]), "pending");
    assert_eq!(text(&r["playback_reason"]), "drained");
    assert_eq!(int(&r["first_audio_ms"]), 4_815, "whole ms, rounded down");
    assert_eq!(int(&r["response_latency_ms"]), 7_363);
    assert_eq!(
        text(&r["afe_evidence"]),
        r#"{"pre":null,"support_start":1}"#
    );
    assert_eq!(text(&r["afe_series"]), r#"{"period":1280,"start":0}"#);
    assert_eq!(text(&r["stt_fix"]), r#"{"applied":false,"mode":"shadow"}"#);
    assert_eq!(
        text(&r["wake_windows"]),
        format!(
            r#"{{"graph_sha256":"{GRAPH}","samples":42560,"window":22400,"scores":[[0,0.0141],[2560,0.4789]]}}"#
        )
    );
    assert_eq!(text(&r["audio_file"]), format!("{DEV}_{id}.wav"));
    assert_eq!(text(&r["wake_file"]), format!("{id}.wav"));
    for column in LEGACY_TURN_COLUMNS {
        assert_eq!(r[column], Value::Null, "{column} is no longer written");
    }

    let trace: serde_json::Value = serde_json::from_str(text(&r["decision_trace"])).unwrap();
    assert_eq!(trace["turn_id"], uuid.to_string());
    assert_eq!(trace["trigger"], "wake");
    assert_eq!(trace["terminal_reason"], "completed");
    assert_eq!(trace["candidate_id"], "cand-accepted");
    assert_eq!(trace["profile"], "idle");
    assert_eq!(trace["hops"][2]["end_sample"], 1_456_064_000u64);
    assert!(
        trace.get("verification").is_none(),
        "an idle wake is not verified"
    );
    assert_eq!(trace["utterance"]["start"], 1_456_031_680u64);
    assert_eq!(trace["utterance"]["endpoint"][0]["event"], "commit");

    assert_eq!(
        fs::read(h.file(RecordingKind::Utterance, DEV, id)).unwrap(),
        wav_bytes(&pcm(77_376))
    );
    assert!(h.file(RecordingKind::WakeClip, DEV, id).exists());

    let stored =
        h.db.read(GetTurn(persisted.insert.id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(stored.trigger_type, Some(Stored::Known(TriggerType::Wake)));
    assert_eq!(
        stored.wake_attribution,
        Some(Stored::Known(WakeAttribution::Idle))
    );
    assert_eq!(
        stored.continuation,
        Some(Stored::Known(Continuation::Pending))
    );
    assert_eq!(stored.intent_local, Some(true));
    h.close().await;
}

#[tokio::test]
async fn unknown_values_stay_null() {
    let h = Harness::new(RetentionLimits::default());
    let utterance = silent_button();
    let record = TurnRecord::turn(
        dev(DEV),
        ts(1_000.5),
        Uuid::new_v4(),
        TriggerType::Button,
        None,
        Some((&utterance, None)),
        policy(),
        TerminalReason::NoInput,
        Duration::from_millis(5_000),
    )
    .unwrap();
    let id = h
        .records
        .persist_turn(record, TurnAudio::default())
        .await
        .unwrap()
        .insert
        .id;
    let r = row(&h.conn(), "turns", id.0);
    assert_eq!(text(&r["trigger_type"]), "button");
    assert_eq!(text(&r["terminal_reason"]), "no_input");
    assert_eq!(int(&r["total_ms"]), 5_000);
    for column in [
        "wake_model",
        "wake_score",
        "wake_threshold",
        "wake_model_sha256",
        "wake_attribution",
        "outcome",
        "asr_text",
        "reference_coverage",
        "commit_route",
        "commit_id",
        "endpoint_class",
        "endpoint_ms",
        "audio_ms",
        "stt_raw",
        "stt_text",
        "stt_ms",
        "stt_fix",
        "intent_ms",
        "intent_local",
        "response_type",
        "response_text",
        "conversation_id",
        "tts_url_ms",
        "playback_ms",
        "playback_reason",
        "first_audio_ms",
        "response_latency_ms",
        "reply_to",
        "continuation",
        "afe_evidence",
        "afe_series",
        "wake_windows",
        "audio_file",
        "wake_file",
    ] {
        assert_eq!(r[column], Value::Null, "{column}");
    }
    h.close().await;
}

#[tokio::test]
async fn a_refused_candidate_is_written_once() {
    let h = Harness::new(RetentionLimits::default());
    let decider = unverified("cand-refused");
    let record = TurnRecord::candidate(
        dev(DEV),
        ts(2_000.0),
        decider.trace(),
        context(None),
        false,
        policy(),
        Duration::from_millis(400),
    )
    .unwrap();
    assert!(matches!(record.key, TurnKey::Candidate(_)));
    let first = h
        .records
        .persist_turn(record.clone(), TurnAudio::default())
        .await
        .unwrap();
    let again = h
        .records
        .persist_turn(record, TurnAudio::default())
        .await
        .unwrap();
    assert!(first.insert.inserted);
    assert!(!again.insert.inserted);
    assert_eq!(first.insert.id, again.insert.id);
    let count: i64 = h
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let r = row(&h.conn(), "turns", first.insert.id.0);
    assert_eq!(r["turn_uuid"], Value::Null);
    assert_eq!(text(&r["wake_attribution"]), "unverified_wake");
    assert_eq!(text(&r["terminal_reason"]), "unverified_wake");
    assert_eq!(text(&r["trigger_type"]), "wake");
    assert_eq!(
        real(&r["wake_score"]),
        0.99,
        "the hops' peak smoothed score"
    );
    assert_eq!(real(&r["wake_threshold"]), 0.65);
    let trace: serde_json::Value = serde_json::from_str(text(&r["decision_trace"])).unwrap();
    assert_eq!(trace["turn_id"], serde_json::Value::Null);
    assert_eq!(trace["candidate_id"], "cand-refused");
    assert_eq!(trace["verification"]["result"], "fail");
    assert_eq!(trace["verification"]["text"], "bananas");
    assert_eq!(trace["verification"]["text_passes"], false);
    assert_eq!(trace["verification"]["deadline_ms"], 700);
    assert_eq!(trace["verification"]["answered_ms"], 400);

    // An accepted candidate that lost arbitration.
    let lost = TurnRecord::candidate(
        dev(DEV),
        ts(2_001.0),
        accepted_idle("cand-lost").trace(),
        context(None),
        true,
        policy(),
        Duration::ZERO,
    )
    .unwrap();
    let id = h
        .records
        .persist_turn(lost, TurnAudio::default())
        .await
        .unwrap()
        .insert
        .id;
    let r = row(&h.conn(), "turns", id.0);
    assert_eq!(text(&r["wake_attribution"]), "arbitration_lost");
    assert_eq!(text(&r["terminal_reason"]), "arbitration_lost");
    assert_eq!(int(&r["total_ms"]), 0);
    h.close().await;
}

#[tokio::test]
async fn a_repeated_turn_completion_inserts_once_and_publishes_once() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    let record = full_turn(uuid, Uuid::new_v4());
    let mut stored = h.records.subscribe_turns();
    let first = h
        .records
        .persist_turn(
            record.clone(),
            TurnAudio {
                utterance: Some(pcm(1_600)),
                wake_clip: None,
                turn_recording: None,
            },
        )
        .await
        .unwrap();
    let again = h
        .records
        .persist_turn(
            record.clone(),
            TurnAudio {
                utterance: Some(pcm(3_200)),
                wake_clip: None,
                turn_recording: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(first.insert.id, again.insert.id);
    assert!(!again.insert.inserted);
    assert_eq!(again.utterance, None, "nothing is published for a repeat");
    // The dashboard hears of the new row once, after its recording is published.
    assert_eq!(
        stored.try_recv().unwrap(),
        echomuse_records::TurnStored {
            device: dev(DEV),
            id: first.insert.id,
        }
    );
    assert!(stored.try_recv().is_err(), "a repeat is not a new turn");
    let path = h.file(RecordingKind::Utterance, DEV, first.insert.id.0);
    assert_eq!(fs::read(path).unwrap(), wav_bytes(&pcm(1_600)));

    let mut elsewhere = record;
    elsewhere.device = dev(OTHER);
    let conflict = h
        .records
        .persist_turn(elsewhere, TurnAudio::default())
        .await;
    assert!(matches!(conflict, Err(RecordsError::Db(_))));
    h.close().await;
}

#[tokio::test]
async fn a_continuation_moves_from_pending_to_one_final_value() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    h.records
        .persist_turn(full_turn(uuid, Uuid::new_v4()), TurnAudio::default())
        .await
        .unwrap();
    let device = dev(DEV);
    let answered = ContinuationEnd::FollowUp(FollowUp::Answered);
    assert_eq!(
        h.records
            .record_continuation(&device, uuid, answered)
            .await
            .unwrap(),
        ContinuationUpdate::Resolved
    );
    assert_eq!(
        h.records
            .record_continuation(
                &device,
                uuid,
                ContinuationEnd::Ended(TerminalReason::ReplyTimeout)
            )
            .await
            .unwrap(),
        ContinuationUpdate::AlreadyFinal(answered)
    );
    assert_eq!(
        h.records
            .record_continuation(&dev(OTHER), uuid, answered)
            .await
            .unwrap(),
        ContinuationUpdate::NoSuchTurn
    );
    let stored: String = h
        .conn()
        .query_row(
            "SELECT continuation FROM turns WHERE turn_uuid = ?1",
            [uuid.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "answered");

    let mut quiet = full_turn(Uuid::new_v4(), Uuid::new_v4());
    quiet.continuation = None;
    let TurnKey::Turn(quiet_uuid) = quiet.key else {
        unreachable!()
    };
    h.records
        .persist_turn(quiet, TurnAudio::default())
        .await
        .unwrap();
    assert_eq!(
        h.records
            .record_continuation(&device, quiet_uuid, answered)
            .await
            .unwrap(),
        ContinuationUpdate::NotAsked
    );

    // A pending question left by a stopped controller ends as session_lost at startup.
    let stale = Uuid::new_v4();
    h.records
        .persist_turn(full_turn(stale, Uuid::new_v4()), TurnAudio::default())
        .await
        .unwrap();
    let report = h.records.start_up().await.unwrap();
    assert_eq!(report.stale_continuations, 1);
    let stored: String = h
        .conn()
        .query_row(
            "SELECT continuation FROM turns WHERE turn_uuid = ?1",
            [stale.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "session_lost");
    h.close().await;
}

fn stored_turn(h: &Harness, uuid: Uuid) -> Option<(Option<String>, Option<String>)> {
    use rusqlite::OptionalExtension;
    h.conn()
        .query_row(
            "SELECT stt_fix, continuation FROM turns WHERE turn_uuid = ?1",
            [uuid.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

#[test]
fn the_pause_view_is_stored_in_the_shapes_the_dashboard_reads() {
    use echomuse_turn::{PauseError, PauseRecord, PauseWords, trace::TextChange};
    let mut utterance = committed_utterance();
    let boundary = 1_456_105_984;
    let first = Sample(boundary - 40_000);
    let pause = Sample(boundary + 16_000);
    let judged = "what's the score of the game";
    utterance.text = vec![
        TextChange {
            text: "what's the".to_owned(),
            grammar: None,
            at: first,
            source: TextSource::Streaming,
        },
        TextChange {
            text: judged.to_owned(),
            grammar: None,
            at: Sample(pause.0 + 5_120),
            source: TextSource::Server,
        },
    ];
    utterance.finalized_at = vec![first, pause];
    utterance.pauses = vec![
        PauseRecord {
            through: first,
            kroko: PauseWords {
                text: Some("Ophilia what's".to_owned()),
                judged: Some("what's".to_owned()),
                at: Some(first),
                ..PauseWords::default()
            },
            server: Some(PauseWords {
                ms: Some(160),
                error: Some(PauseError::Superseded),
                ..PauseWords::default()
            }),
        },
        PauseRecord {
            through: pause,
            kroko: PauseWords {
                text: Some("Ophilia what's the score of the gay".to_owned()),
                judged: Some("what's the score of the gay".to_owned()),
                at: Some(pause),
                ..PauseWords::default()
            },
            server: Some(PauseWords {
                text: Some("Ophelia. What's the score of the game?".to_owned()),
                ms: Some(282),
                judged: Some(judged.to_owned()),
                at: Some(Sample(pause.0 + 5_120)),
                error: None,
            }),
        },
    ];
    let wake = accepted_idle("cand-accepted");
    let mut record = TurnRecord::turn(
        dev(DEV),
        ts(1_793_235_185.803_92),
        Uuid::new_v4(),
        TriggerType::Wake,
        Some((wake.trace(), context(Some(0.9995)))),
        Some((&utterance, Some(Uuid::new_v4()))),
        policy(),
        TerminalReason::Completed,
        Duration::from_millis(13_292),
    )
    .unwrap();
    record.set_trace_details(
        Some(&utterance),
        &echomuse_records::TraceDetails {
            verification: echomuse_records::VerificationTiming::default(),
        },
    );
    let trace: serde_json::Value =
        serde_json::from_str(record.decision_trace.as_ref().unwrap().as_str()).unwrap();
    let u = &trace["utterance"];
    assert_eq!(u["stable_prefix"], judged);
    assert_eq!(u["heard"], "Ophelia. What's the score of the game?");
    assert_eq!(
        u["pauses"][1],
        json!({"through": pause.0,
               "kroko": {"text": "Ophilia what's the score of the gay", "ms": null,
                         "judged": "what's the score of the gay", "at": pause.0, "error": null},
               "server": {"text": "Ophelia. What's the score of the game?", "ms": 282,
                          "judged": judged, "at": pause.0 + 5_120, "error": null}})
    );
    assert_eq!(
        u["pauses"][0]["server"],
        json!({"text": null, "ms": 160, "judged": null, "at": null, "error": "superseded"})
    );
    let commit = u["endpoint"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "commit")
        .unwrap();
    assert_eq!(commit["source"], "server");
    // A wake the verifier never heard has no verification, as stored rows show.
    assert!(trace.get("verification").is_none());
}

#[tokio::test]
async fn answered_requests_are_read_as_sent_newest_first() {
    let h = Harness::new(RetentionLimits::default());
    let turn = |text: &str, fix: Option<serde_json::Value>, outcome: TurnOutcome| {
        let mut r = full_turn(Uuid::new_v4(), Uuid::new_v4());
        r.stt_text = Some(text.to_owned());
        r.stt_fix = fix.map(|f| SttFix(object(f)));
        r.outcome = Some(outcome);
        r
    };
    for record in [
        turn("turn on the lite", None, TurnOutcome::Ha),
        turn(
            "plate cherub rock",
            Some(json!({"applied": true, "sent": "play cherub rock"})),
            TurnOutcome::Ha,
        ),
        turn(
            "watch flex",
            Some(json!({"applied": false, "sent": "watch plex"})),
            TurnOutcome::Ha,
        ),
        turn("stop", None, TurnOutcome::LocalCommand),
        turn("Turn on the lite", None, TurnOutcome::Ha),
    ] {
        h.records
            .persist_turn(record, TurnAudio::default())
            .await
            .unwrap();
    }
    let texts =
        h.db.read(echomuse_records::AnsweredRequests { limit: 10 })
            .await
            .unwrap();
    // A shadow proposal is never used; a local command is not HA's answer; each text once.
    assert_eq!(
        texts,
        ["Turn on the lite", "watch flex", "play cherub rock"]
    );
    h.close().await;
}

#[tokio::test]
async fn a_held_row_waits_for_its_correction_record_and_takes_a_reply_meanwhile() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    let mut record = full_turn(uuid, Uuid::new_v4());
    record.stt_fix = None;
    let (fix, follows) = tokio::sync::oneshot::channel();
    let written = h
        .records
        .submit_turn(record, TurnAudio::default(), Some(follows));
    // The question's reply arrives while its row waits for the record.
    let answered = ContinuationEnd::FollowUp(FollowUp::Answered);
    let resolved = h.records.submit_continuation(&dev(DEV), uuid, answered);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(stored_turn(&h, uuid), None, "the row waits for its record");
    fix.send(Some(SttFix(object(
        json!({"mode": "shadow", "reason": "would_apply"}),
    ))))
    .unwrap();
    let persisted = written.await.unwrap().unwrap();
    assert!(persisted.insert.inserted);
    assert_eq!(
        resolved.await.unwrap().unwrap(),
        ContinuationUpdate::Resolved
    );
    let (stt_fix, continuation) = stored_turn(&h, uuid).expect("written");
    let stt_fix: serde_json::Value = serde_json::from_str(&stt_fix.unwrap()).unwrap();
    assert_eq!(stt_fix["reason"], "would_apply");
    assert_eq!(continuation.as_deref(), Some("answered"));
    // A later reply finds the written row and changes nothing.
    assert_eq!(
        h.records
            .record_continuation(
                &dev(DEV),
                uuid,
                ContinuationEnd::Ended(TerminalReason::ReplyTimeout)
            )
            .await
            .unwrap(),
        ContinuationUpdate::AlreadyFinal(answered)
    );
    h.close().await;
}

#[tokio::test]
async fn a_held_row_whose_check_went_away_is_written_without_a_record() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    let mut record = full_turn(uuid, Uuid::new_v4());
    record.stt_fix = None;
    let (fix, follows) = tokio::sync::oneshot::channel::<Option<SttFix>>();
    let written = h
        .records
        .submit_turn(record, TurnAudio::default(), Some(follows));
    drop(fix);
    written.await.unwrap().unwrap();
    let (stt_fix, continuation) = stored_turn(&h, uuid).expect("written");
    assert_eq!(stt_fix, None);
    assert_eq!(continuation.as_deref(), Some("pending"));
    h.close().await;
}

#[tokio::test]
async fn a_submitted_turn_outlives_the_runtime_that_submitted_it() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    let (fix, follows) = tokio::sync::oneshot::channel::<Option<SttFix>>();
    // A device runtime: its own thread and runtime, gone right after submitting, with
    // the row still waiting for its record and a turn recording to publish.
    let records = h.records.clone();
    let record = full_turn(uuid, Uuid::new_v4());
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            drop(records.submit_turn(
                record,
                TurnAudio {
                    turn_recording: Some(pcm(1_280)),
                    ..TurnAudio::default()
                },
                Some(follows),
            ));
        });
    })
    .join()
    .unwrap();
    fix.send(Some(SttFix(object(json!({"mode": "shadow"})))))
        .unwrap();
    // The shutdown drain waits for it.
    assert!(h.records.drain(Duration::from_secs(5)).await);
    let (stt_fix, _) = stored_turn(&h, uuid).expect("written after its runtime ended");
    assert!(stt_fix.is_some());
    let id: i64 = h
        .conn()
        .query_row(
            "SELECT id FROM turns WHERE turn_uuid = ?1",
            [uuid.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(h.file(RecordingKind::TurnRecording, DEV, id).exists());
    h.close().await;
}

#[tokio::test]
async fn the_shutdown_drain_writes_a_row_whose_record_never_comes() {
    let h = Harness::new(RetentionLimits::default());
    let uuid = Uuid::new_v4();
    let mut record = full_turn(uuid, Uuid::new_v4());
    record.stt_fix = None;
    let (_fix, follows) = tokio::sync::oneshot::channel::<Option<SttFix>>();
    let written = h
        .records
        .submit_turn(record, TurnAudio::default(), Some(follows));
    assert!(
        h.records.drain(Duration::from_millis(100)).await,
        "flushed within the second wait"
    );
    written.await.unwrap().unwrap();
    assert_eq!(stored_turn(&h, uuid).expect("written").0, None);
    h.close().await;
}

fn near_miss(uuid: &str, at: f64) -> NearMissRecord {
    NearMissRecord {
        device: dev(DEV),
        ts: ts(at),
        uuid: NearMissUuid::new(uuid).unwrap(),
        profile: WakeProfile::Idle,
        floor: Score::new(0.17).unwrap(),
        peak: Score::new(0.477_947_872_564_294).unwrap(),
        wake_threshold: Score::new(0.9),
        wake_model_sha256: Sha256::parse(GRAPH),
        duration: echomuse_records::near_misses::support_duration(1_484_915_840, 1_484_956_160),
        end_reason: NearMissEnd::Below,
        afe_series: None,
    }
}

#[tokio::test]
async fn a_near_miss_writes_every_column_once() {
    let h = Harness::new(RetentionLimits::default());
    let mut record = near_miss("nm-1", 1_793_236_994.413_71);
    record.afe_series = Some(AfeSeries(object(json!({"start": 1, "utterance": null}))));
    let saved = h
        .records
        .persist_near_miss(record.clone(), NearMissAudio::Recorded(pcm(50_368)))
        .await
        .unwrap();
    assert_eq!(saved.recording, Some(Publication::Saved));
    let id = saved.insert.id;
    let r = row(&h.conn(), "near_misses", id.0);
    assert_eq!(r.len(), 16, "the contract's near_misses columns");
    assert_eq!(text(&r["device_id"]), DEV);
    assert!((real(&r["ts"]) - 1_793_236_994.413_71).abs() < 1e-6);
    assert_eq!(text(&r["near_miss_uuid"]), "nm-1");
    assert_eq!(text(&r["profile"]), "idle");
    assert_eq!(real(&r["floor"]), 0.17);
    assert_eq!(real(&r["peak"]), 0.477_947_872_564_294);
    assert_eq!(real(&r["wake_threshold"]), 0.9);
    assert_eq!(text(&r["wake_model_sha256"]), GRAPH);
    assert_eq!(int(&r["duration_ms"]), 2_520);
    assert_eq!(text(&r["end_reason"]), "below");
    assert_eq!(text(&r["recording"]), "saved");
    assert_eq!(int(&r["audio_ms"]), 3_148);
    assert_eq!(text(&r["afe_series"]), r#"{"start":1,"utterance":null}"#);
    assert_eq!(r["verdict"], Value::Null);
    assert_eq!(r["windows"], Value::Null);
    assert!(h.file(RecordingKind::NearMiss, DEV, id.0).exists());

    let again = h
        .records
        .persist_near_miss(record, NearMissAudio::Recorded(pcm(10)))
        .await
        .unwrap();
    assert_eq!(again.insert.id, id);
    assert!(!again.insert.inserted);
    assert_eq!(again.recording, None);

    let windows = ClipWindows {
        graph_sha256: Sha256::parse(GRAPH).unwrap(),
        samples: 50_368,
        window: 22_400,
        scores: vec![(0, 0.0039)],
    };
    assert!(
        h.records
            .set_near_miss_windows(&dev(DEV), id, windows.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        h.records
            .set_near_miss_verdict(&dev(DEV), id, Some(NearMissVerdict::MissedWake))
            .await
            .unwrap(),
        VerdictUpdate::Stored
    );
    let stored = h.db.read(GetNearMiss(id)).await.unwrap().unwrap();
    assert_eq!(
        stored.verdict,
        Some(Stored::Known(NearMissVerdict::MissedWake))
    );
    assert_eq!(
        ClipWindows::parse(stored.windows.as_deref().unwrap()),
        Some(windows)
    );

    // Without a recording: why, and no length.
    let off = h
        .records
        .persist_near_miss(
            near_miss("nm-2", 1_793_237_000.0),
            NearMissAudio::NotRecorded(NearMissRecording::Off),
        )
        .await
        .unwrap();
    let r = row(&h.conn(), "near_misses", off.insert.id.0);
    assert_eq!(text(&r["recording"]), "off");
    assert_eq!(r["audio_ms"], Value::Null);
    assert_eq!(r["afe_series"], Value::Null);
    assert_eq!(
        h.records
            .set_near_miss_verdict(
                &dev(DEV),
                off.insert.id,
                Some(NearMissVerdict::CorrectlyIgnored)
            )
            .await
            .unwrap(),
        VerdictUpdate::NotRecorded
    );
    h.close().await;
}

#[tokio::test]
async fn startup_reconciles_every_publication_crash_point() {
    let h = Harness::new(RetentionLimits::default());
    let device = dev(DEV);

    // The row committed and the controller stopped before writing its file.
    let unsaved =
        h.db.write(InsertTurn::new(full_turn(Uuid::new_v4(), Uuid::new_v4())))
            .await
            .unwrap()
            .id
            .0;
    let failed_nm =
        h.db.write(InsertNearMiss::new(
            near_miss("nm-crash", 3_000.0),
            NearMissRecording::Failed,
        ))
        .await
        .unwrap()
        .id
        .0;

    // The file was renamed into place but the row was never marked.
    let adoptable =
        h.db.write(InsertTurn::new(full_turn(Uuid::new_v4(), Uuid::new_v4())))
            .await
            .unwrap()
            .id
            .0;
    let wake_dir = h
        .records
        .layout()
        .directory(RecordingKind::WakeClip, &device);
    write_atomically(
        &wake_dir,
        &format!("{adoptable}.wav"),
        &wav_bytes(&pcm(16_000)),
    )
    .unwrap();
    let adoptable_nm =
        h.db.write(InsertNearMiss::new(
            near_miss("nm-adopt", 3_001.0),
            NearMissRecording::Failed,
        ))
        .await
        .unwrap()
        .id
        .0;
    let nm_dir = h.root().join("nearmiss_recordings");
    write_atomically(
        &nm_dir,
        &format!("{DEV}_{adoptable_nm}.wav"),
        &wav_bytes(&pcm(8_000)),
    )
    .unwrap();

    // A cut-short file of an unsaved row.
    let rec_dir = h.root().join("recordings");
    let mut short = wav_bytes(&pcm(100));
    short.truncate(60);
    fs::create_dir_all(&rec_dir).unwrap();
    fs::write(rec_dir.join(format!("{DEV}_{unsaved}.wav")), &short).unwrap();

    // Files without rows, a row that says it was never recorded, an interrupted write and a
    // stranger's file.
    fs::write(rec_dir.join(format!("{DEV}_999.wav")), wav_bytes(&pcm(10))).unwrap();
    let off_nm =
        h.db.write(InsertNearMiss::new(
            near_miss("nm-off", 3_002.0),
            NearMissRecording::Off,
        ))
        .await
        .unwrap()
        .id
        .0;
    fs::write(
        nm_dir.join(format!("{DEV}_{off_nm}.wav")),
        wav_bytes(&pcm(10)),
    )
    .unwrap();
    let turn_dir = h.root().join("turn_recordings");
    fs::create_dir_all(&turn_dir).unwrap();
    fs::write(
        turn_dir.join(format!("{OTHER}_{unsaved}.wav")),
        wav_bytes(&pcm(10)),
    )
    .unwrap();
    fs::write(
        turn_dir.join(format!(".{DEV}_{unsaved}.wav.1-0.tmp")),
        b"partial",
    )
    .unwrap();
    fs::write(rec_dir.join("notes.txt"), b"keep me").unwrap();

    let report = h.records.start_up().await.unwrap();

    let conn = h.conn();
    let r = row(&conn, "turns", unsaved);
    assert_eq!(
        r["audio_file"],
        Value::Null,
        "a row without its file stays unsaved"
    );
    assert_eq!(r["wake_file"], Value::Null);
    let r = row(&conn, "near_misses", failed_nm);
    assert_eq!(text(&r["recording"]), "failed");
    assert_eq!(r["audio_ms"], Value::Null);

    assert_eq!(
        text(&row(&conn, "turns", adoptable)["wake_file"]),
        format!("{adoptable}.wav")
    );
    let r = row(&conn, "near_misses", adoptable_nm);
    assert_eq!(text(&r["recording"]), "saved");
    assert_eq!(int(&r["audio_ms"]), 500);
    assert_eq!(report.adopted.len(), 2);

    assert_eq!(
        report.incomplete_removed,
        vec![rec_dir.join(format!("{DEV}_{unsaved}.wav"))]
    );
    assert_eq!(report.orphans_removed.len(), 3, "{report:?}");
    assert!(!rec_dir.join(format!("{DEV}_999.wav")).exists());
    assert!(!nm_dir.join(format!("{DEV}_{off_nm}.wav")).exists());
    assert!(!turn_dir.join(format!("{OTHER}_{unsaved}.wav")).exists());
    assert_eq!(report.temporary_removed.len(), 1);
    assert_eq!(report.unrecognized, vec![rec_dir.join("notes.txt")]);
    assert!(rec_dir.join("notes.txt").exists());
    h.close().await;
}

#[tokio::test]
async fn retention_keeps_the_newest_files_and_every_marked_near_miss() {
    let limits = RetentionLimits {
        utterances: 3,
        turn_recordings: 2,
        wake_clips: 4,
        near_miss_recordings: 2,
        near_miss_rows: 3,
        ..RetentionLimits::default()
    };
    let h = Harness::new(limits);
    let device = dev(DEV);
    let mut ids = Vec::new();
    for _ in 0..5 {
        let p = h
            .records
            .persist_turn(
                full_turn(Uuid::new_v4(), Uuid::new_v4()),
                TurnAudio {
                    utterance: Some(pcm(160)),
                    wake_clip: Some(pcm(160)),
                    turn_recording: Some(pcm(160)),
                },
            )
            .await
            .unwrap();
        let id = p.insert.id;
        assert_eq!(p.turn_recording, Some(Publication::Saved));
        ids.push(id.0);
    }
    let exists = |kind, id| h.file(kind, DEV, id).exists();
    let kept = |kind| {
        ids.iter()
            .filter(|id| exists(kind, **id))
            .copied()
            .collect::<Vec<_>>()
    };
    assert_eq!(kept(RecordingKind::Utterance), ids[2..]);
    assert_eq!(kept(RecordingKind::TurnRecording), ids[3..]);
    assert_eq!(kept(RecordingKind::WakeClip), ids[1..]);
    let named: i64 = h
        .conn()
        .query_row(
            "SELECT count(*) FROM turns WHERE audio_file IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(named, 5, "rows keep their file names after the files go");

    // Near misses: the first is marked; then four more arrive.
    let mut nms = Vec::new();
    for i in 0..5 {
        let p = h
            .records
            .persist_near_miss(
                near_miss(&format!("nm-{i}"), 4_000.0 + f64::from(i)),
                NearMissAudio::Recorded(pcm(160)),
            )
            .await
            .unwrap();
        nms.push(p.insert.id);
        if i == 0 {
            assert_eq!(
                h.records
                    .set_near_miss_verdict(&device, p.insert.id, Some(NearMissVerdict::MissedWake))
                    .await
                    .unwrap(),
                VerdictUpdate::Stored
            );
        }
    }
    let rows: Vec<i64> = {
        let conn = h.conn();
        let mut s = conn
            .prepare("SELECT id FROM near_misses ORDER BY id")
            .unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        rows,
        vec![nms[0].0, nms[2].0, nms[3].0, nms[4].0],
        "3 unmarked + marked"
    );
    let files: Vec<i64> = nms
        .iter()
        .map(|n| n.0)
        .filter(|id| exists(RecordingKind::NearMiss, *id))
        .collect();
    assert_eq!(
        files,
        vec![nms[0].0, nms[3].0, nms[4].0],
        "2 unmarked + marked"
    );

    // Clearing the mark puts it back under the limits.
    h.records
        .set_near_miss_verdict(&device, nms[0], None)
        .await
        .unwrap();
    assert!(h.db.read(GetNearMiss(nms[0])).await.unwrap().is_none());
    assert!(!exists(RecordingKind::NearMiss, nms[0].0));
    h.close().await;
}

#[tokio::test]
async fn turn_traces_are_kept_for_the_newest_rows_only() {
    let h = Harness::new(RetentionLimits::default());
    let mut ids = Vec::new();
    for _ in 0..3 {
        let mut insert = InsertTurn::new(full_turn(Uuid::new_v4(), Uuid::new_v4()));
        insert.trace_retention = 2;
        ids.push(h.db.write(insert).await.unwrap().id.0);
    }
    let conn = h.conn();
    let r = row(&conn, "turns", ids[0]);
    assert_eq!(r["decision_trace"], Value::Null);
    assert_eq!(r["afe_series"], Value::Null);
    assert_ne!(
        r["afe_evidence"],
        Value::Null,
        "only the trace and series go"
    );
    assert_ne!(row(&conn, "turns", ids[1])["decision_trace"], Value::Null);
    assert_ne!(row(&conn, "turns", ids[2])["afe_series"], Value::Null);
    h.close().await;
}

#[tokio::test]
async fn deleting_a_device_stops_late_writers() {
    let h = Harness::new(RetentionLimits::default());
    let device = dev(DEV);
    let p = h
        .records
        .persist_turn(
            full_turn(Uuid::new_v4(), Uuid::new_v4()),
            TurnAudio {
                utterance: Some(pcm(160)),
                wake_clip: Some(pcm(160)),
                turn_recording: None,
            },
        )
        .await
        .unwrap();
    h.records
        .persist_near_miss(
            near_miss("nm-x", 5_000.0),
            NearMissAudio::Recorded(pcm(160)),
        )
        .await
        .unwrap();
    h.records
        .log(DeviceLogEntry {
            device: device.clone(),
            ts: UnixMillis(1_793_239_028_889),
            level: LogLevel::Info,
            source: LogSource::Device,
            message: "hello".to_owned(),
        })
        .await
        .unwrap();
    let deleted = h.records.delete_device(&device).await.unwrap();
    assert_eq!(deleted.rows.turns, 1);
    assert_eq!(deleted.rows.near_misses, 1);
    assert_eq!(deleted.rows.device_logs, 1);
    assert_eq!(deleted.files, 3);
    assert!(
        !h.file(RecordingKind::Utterance, DEV, p.insert.id.0)
            .exists()
    );
    assert!(!h.root().join("wakes").join(DEV).exists());

    let late = h
        .records
        .persist_turn(
            full_turn(Uuid::new_v4(), Uuid::new_v4()),
            TurnAudio {
                utterance: Some(pcm(160)),
                wake_clip: None,
                turn_recording: Some(pcm(160)),
            },
        )
        .await;
    assert!(matches!(late, Err(RecordsError::Closed(_))));
    assert!(!h.root().join("turn_recordings").exists());
    let count: i64 = h
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "nothing was recreated");

    // Another device is unaffected, and an approved-again device writes again.
    let mut other = full_turn(Uuid::new_v4(), Uuid::new_v4());
    other.device = dev(OTHER);
    h.records
        .persist_turn(other, TurnAudio::default())
        .await
        .unwrap();
    h.records.reopen_device(&device).await;
    h.records
        .persist_turn(
            full_turn(Uuid::new_v4(), Uuid::new_v4()),
            TurnAudio::default(),
        )
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn device_logs_store_milliseconds_and_keep_the_newest_lines() {
    let h = Harness::new(RetentionLimits {
        device_logs: 3,
        ..RetentionLimits::default()
    });
    let device = dev(DEV);
    for i in 0..5 {
        h.records
            .log(DeviceLogEntry {
                device: device.clone(),
                ts: UnixMillis(1_793_239_000_000 + i),
                level: if i == 4 {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                },
                source: if i == 4 {
                    LogSource::Controller
                } else {
                    LogSource::Device
                },
                message: format!("line {i}"),
            })
            .await
            .unwrap();
    }
    h.records
        .log(DeviceLogEntry {
            device: dev(OTHER),
            ts: UnixMillis(1),
            level: LogLevel::Warn,
            source: LogSource::Device,
            message: "other".to_owned(),
        })
        .await
        .unwrap();
    let conn = h.conn();
    let mut s = conn
        .prepare(
            "SELECT typeof(ts), ts, level, source, message FROM device_logs \
             WHERE device_id = ?1 ORDER BY ts",
        )
        .unwrap();
    let lines: Vec<(String, i64, String, String, String)> = s
        .query_map([DEV], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].0, "integer");
    assert_eq!(lines[0].1, 1_793_239_000_002);
    assert_eq!(lines[0].4, "line 2");
    assert_eq!(
        (lines[2].2.as_str(), lines[2].3.as_str()),
        ("error", "controller")
    );
    let other: i64 = conn
        .query_row(
            "SELECT count(*) FROM device_logs WHERE device_id = ?1",
            [OTHER],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other, 1);
    h.close().await;
}

#[tokio::test]
async fn device_metrics_roll_up_per_hour() {
    let h = Harness::new(RetentionLimits::default());
    let device = dev(DEV);
    let hour = 1_793_235_600.0;
    let first = StatsReport {
        cpu_pct: 31.5,
        mem_used_mb: 90.0,
        mem_total_mb: Some(477.0),
        storage_used_mb: Some(293.0),
        storage_total_mb: Some(1_231.0),
        wifi_rssi: Some(-58.0),
        link_speed_mbps: Some(135),
        wifi_freq_mhz: Some(5_805),
        wifi_bssid: Some("02:00:00:00:00:01".to_owned()),
        net: Some(NetDelta {
            tx_bytes: 1_000,
            rx_bytes: 5_000,
            tx_errors: 1,
            tx_dropped: 0,
            rx_crc: 2,
        }),
        cpu_temp_c: Some(49.5),
        max_temp_c: Some(50.0),
        cores_online: Some(4),
        cores_total: Some(4),
        thermal_core_limit: Some(4),
    };
    let second = StatsReport {
        cpu_pct: 40.0,
        mem_used_mb: 100.0,
        mem_total_mb: Some(477.0),
        storage_used_mb: Some(294.0),
        storage_total_mb: Some(1_231.0),
        wifi_rssi: None,
        link_speed_mbps: Some(65),
        wifi_freq_mhz: None,
        wifi_bssid: None,
        net: None,
        cpu_temp_c: Some(52.0),
        max_temp_c: None,
        cores_online: Some(2),
        cores_total: Some(4),
        thermal_core_limit: None,
    };
    h.records
        .record_rtt(
            &device,
            ts(hour + 1.0),
            RttSample {
                rtt: Duration::from_millis(18),
                idle: true,
            },
        )
        .await
        .unwrap();
    h.records
        .record_stats(&device, ts(hour + 30.0), first)
        .await
        .unwrap();
    h.records
        .record_stats(&device, ts(hour + 60.0), second)
        .await
        .unwrap();
    for (rtt, idle) in [(3, true), (250, false), (200, true)] {
        h.records
            .record_rtt(
                &device,
                ts(hour + 90.0),
                RttSample {
                    rtt: Duration::from_millis(rtt),
                    idle,
                },
            )
            .await
            .unwrap();
    }
    // A Show reports no Wi-Fi details, in the next hour.
    h.records
        .record_stats(
            &device,
            ts(hour + 3_600.0),
            StatsReport {
                cpu_pct: 10.0,
                mem_used_mb: 50.0,
                ..StatsReport::default()
            },
        )
        .await
        .unwrap();

    let conn = h.conn();
    let get = |hour_ts: i64| -> HashMap<String, Value> {
        let mut s = conn
            .prepare("SELECT * FROM device_metrics WHERE device_id = ?1 AND hour_ts = ?2")
            .unwrap();
        let names: Vec<String> = s.column_names().map(str::to_owned).collect();
        s.query_row(rusqlite::params![DEV, hour_ts], |r| {
            Ok(names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), r.get::<_, Value>(i).unwrap()))
                .collect())
        })
        .unwrap()
    };
    let m = get(1_793_235_600);
    assert_eq!(int(&m["samples"]), 2);
    assert_eq!(real(&m["cpu_sum"]), 71.5);
    assert_eq!(real(&m["cpu_max"]), 40.0);
    assert_eq!(real(&m["mem_used_sum"]), 190.0);
    assert_eq!(real(&m["mem_total_mb"]), 477.0);
    assert_eq!(real(&m["storage_used_mb"]), 294.0, "the latest");
    assert_eq!(real(&m["storage_total_mb"]), 1_231.0);
    assert_eq!(real(&m["rssi_sum"]), -58.0);
    assert_eq!(int(&m["rssi_samples"]), 1);
    assert_eq!(real(&m["rssi_min"]), -58.0);
    assert_eq!(int(&m["link_speed_last"]), 65);
    assert_eq!(int(&m["link_speed_min"]), 65);
    assert_eq!(
        int(&m["wifi_freq_last"]),
        5_805,
        "an unreported value keeps the last"
    );
    assert_eq!(text(&m["wifi_bssid_last"]), "02:00:00:00:00:01");
    assert_eq!(int(&m["tx_bytes_sum"]), 1_000);
    assert_eq!(int(&m["rx_bytes_sum"]), 5_000);
    assert_eq!(int(&m["tx_errors_sum"]), 1);
    assert_eq!(int(&m["tx_dropped_sum"]), 0);
    assert_eq!(int(&m["rx_crc_sum"]), 2);
    assert_eq!(int(&m["rtt_sum_ms"]), 471);
    assert_eq!(int(&m["rtt_samples"]), 4);
    assert_eq!(int(&m["rtt_min_ms"]), 3);
    assert_eq!(int(&m["rtt_max_ms"]), 250);
    assert_eq!(int(&m["rtt_excursions"]), 2);
    assert_eq!(int(&m["rtt_excursions_idle"]), 1);
    assert_eq!(int(&m["rtt_samples_idle"]), 3);
    assert_eq!(real(&m["cpu_temp_sum"]), 101.5);
    assert_eq!(int(&m["cpu_temp_samples"]), 2);
    assert_eq!(real(&m["cpu_temp_max"]), 52.0);
    assert_eq!(real(&m["max_temp_max"]), 50.0);
    assert_eq!(int(&m["cores_online_last"]), 2);
    assert_eq!(int(&m["cores_online_min"]), 2);
    assert_eq!(int(&m["cores_total"]), 4);
    assert_eq!(int(&m["thermal_limit_min"]), 4);

    let next = get(1_793_239_200);
    assert_eq!(int(&next["samples"]), 1);
    for column in [
        "mem_total_mb",
        "rssi_min",
        "link_speed_last",
        "wifi_bssid_last",
        "rtt_min_ms",
        "rtt_max_ms",
        "cpu_temp_max",
        "max_temp_max",
        "cores_online_last",
        "cores_total",
        "thermal_limit_min",
    ] {
        assert_eq!(next[column], Value::Null, "{column} is unknown, not 0");
    }
    assert_eq!(int(&next["rssi_samples"]), 0);
    assert_eq!(int(&next["rtt_samples"]), 0);
    h.close().await;
}

#[tokio::test]
async fn wake_stats_roll_up_counters_and_shadow_rules() {
    let h = Harness::new(RetentionLimits::default());
    let device = dev(DEV);
    let rule = OpenRule::new(RuleProfile::Idle, 2, RuleCombine::Mean, 0.9).unwrap();
    let shadow = |lead: [u64; 7], events: Vec<ShadowEvent>| ShadowStats {
        rule,
        hops: 187,
        opens: 1,
        matched: 1,
        lead_hist: lead,
        unmatched: u64::from(!events.is_empty()),
        retried: 0,
        live_only: 0,
        events,
        events_dropped: 2,
    };
    let event = ShadowEvent {
        kind: ShadowEventKind::Unmatched,
        ts: ts(1_793_105_139.315_22),
        peak_raw: 0.938_549_356_289_295_5,
        raws: vec![
            0.178_704_750_197_772_43,
            0.864_493_719_241_965_5,
            0.938_549_356_289_295_5,
        ],
    };
    let at = ts(1_793_105_100.0);
    h.records
        .record_wake_stats(
            &device,
            at,
            WakeStatsReport {
                hops_scored: 187,
                hops_dropped: 1,
                near_miss_peaks: vec![0.31, 0.2],
                candidates_opened: 1,
                peak_smoothed: Some(0.93),
                infer_max_ms: Some(96.6),
                shadow: Some(vec![shadow([0, 0, 0, 0, 1, 0, 0], vec![event])]),
            },
        )
        .await
        .unwrap();
    h.records
        .record_wake_stats(
            &device,
            at,
            WakeStatsReport {
                hops_scored: 188,
                hops_dropped: 0,
                near_miss_peaks: Vec::new(),
                candidates_opened: 0,
                peak_smoothed: None,
                infer_max_ms: None,
                shadow: Some(vec![shadow([0, 0, 0, 1, 1, 0, 0], Vec::new())]),
            },
        )
        .await
        .unwrap();
    // Older firmware: no shadow data at all.
    h.records
        .record_wake_stats(
            &device,
            at,
            WakeStatsReport {
                hops_scored: 5,
                ..WakeStatsReport::default()
            },
        )
        .await
        .unwrap();

    let conn = h.conn();
    let (near, near_max, underruns, frames, drops, crossings, max_score, infer, gap, hops): (
        i64,
        f64,
        i64,
        i64,
        i64,
        i64,
        f64,
        i64,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT near_misses, near_miss_max, underruns, dev_frames, dev_drops, \
             dev_crossings, dev_max_score, dev_max_infer_ms, dev_max_gap_ms, dev_hops \
             FROM wake_counters WHERE device_id = ?1 AND hour_ts = 1793102400",
            [DEV],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .unwrap();
    assert_eq!((near, near_max), (2, 0.31));
    assert_eq!((underruns, frames, gap), (0, 0, 0), "no longer written");
    assert_eq!(
        (drops, crossings, max_score, infer, hops),
        (1, 1, 0.93, 97, 380)
    );

    let shadow_row: (String, i64, i64, i64, String, i64, i64) = conn
        .query_row(
            "SELECT rule_key, hops, opens, matched, lead_hist, unmatched, events_dropped \
             FROM wake_shadow WHERE device_id = ?1",
            [DEV],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        shadow_row,
        (
            "idle:2:mean:0.90".to_owned(),
            374,
            2,
            2,
            "[0, 0, 0, 1, 2, 0, 0]".to_owned(),
            1,
            4
        )
    );
    let ev: (String, f64, String, String, f64, String) = conn
        .query_row(
            "SELECT typeof(ts), ts, rule_key, kind, peak_raw, raws FROM wake_shadow_events",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(ev.0, "real");
    assert!((ev.1 - 1_793_105_139.315_22).abs() < 1e-6);
    assert_eq!(
        (ev.2.as_str(), ev.3.as_str()),
        ("idle:2:mean:0.90", "unmatched")
    );
    assert_eq!(ev.4, 0.938_549_356_289_295_5);
    assert_eq!(
        ev.5,
        "[0.17870475019777243, 0.8644937192419655, 0.9385493562892955]"
    );
    h.close().await;
}

#[tokio::test]
async fn sample_clips_are_published_with_their_sidecar() {
    let h = Harness::new(RetentionLimits::default());
    let device = dev(DEV);
    let at = UnixMillis(1_786_757_114_826);
    let sidecar = AfeSeries(object(json!({"start": 0, "period": 1280, "support": null})));
    let path = h
        .records
        .publish_sample_clip(&device, at, pcm(320), Some(sidecar))
        .await
        .unwrap();
    assert_eq!(
        path,
        h.root().join("samples").join(DEV).join("1786757114826.wav")
    );
    assert_eq!(fs::read(&path).unwrap(), wav_bytes(&pcm(320)));
    assert_eq!(
        fs::read_to_string(
            h.root()
                .join("samples")
                .join(DEV)
                .join("1786757114826.afe.json")
        )
        .unwrap(),
        r#"{"period":1280,"start":0,"support":null}"#
    );
    let again = h
        .records
        .publish_sample_clip(&device, at, pcm(320), None)
        .await;
    assert!(matches!(again, Err(RecordsError::Io(_))));
    let unsafe_id = h
        .records
        .publish_sample_clip(&dev("../x"), at, pcm(1), None)
        .await;
    assert!(matches!(unsafe_id, Err(RecordsError::UnsafeDeviceId(_))));
    h.close().await;
}

#[test]
fn the_default_limits_are_the_documented_ones() {
    let l = RetentionLimits::default();
    assert_eq!(
        (
            l.utterances,
            l.turn_recordings,
            l.wake_clips,
            l.near_miss_recordings,
            l.near_miss_rows,
            l.turn_traces,
            l.near_miss_series,
            l.device_logs
        ),
        (300, 100, 1_000, 1_000, 2_000, 1_000, 1_000, 10_000)
    );
}
