//! The native AFE chart and the turn recording of a turn in an `afe_metadata` session.

use echomuse_core::services::{MediaUrl, RunEvent, Transcript};
use echomuse_uplink::UplinkCommand;
use serde_json::Value;

use super::rig::{Options, Rig, capabilities};
use super::{command, intent_end, wake};
use crate::{HaReply, HaRequest, PersistRequest};

fn afe_rig(save_utterances: bool) -> Rig {
    let mut settings = echomuse_settings::DeviceSettings::default();
    settings.microphones.save_utterances = save_utterances;
    Rig::new(Options {
        capabilities: capabilities(&["afe_metadata_v1"]),
        afe_metadata: true,
        settings,
        ..Options::default()
    })
}

fn answered_turn(rig: &mut Rig) {
    wake(rig, "c1");
    let stt = command(rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: "turn on the kitchen light".to_owned(),
        })),
    );
    let intent = rig.take_ha("intent", |r| matches!(r, HaRequest::IntentTts { .. }));
    rig.reply(intent, HaReply::IntentDispatched(Ok(())));
    rig.reply(intent, HaReply::IntentEvent(intent_end("Done.", false)));
    rig.reply(
        intent,
        HaReply::IntentEvent(RunEvent::TtsReady {
            url: MediaUrl("http://ha/api/tts_proxy/a.wav".to_owned()),
            streamed: false,
        }),
    );
    // The answer plays while the room keeps going: the response subscription hears it.
    rig.silence(400);
    rig.play_out(0.5);
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
}

fn response_closes(rig: &Rig) -> usize {
    rig.uplink_commands()
        .into_iter()
        .filter(|c| matches!(c, UplinkCommand::Close(_)))
        .count()
}

#[tokio::test(start_paused = true)]
async fn the_response_subscription_and_the_row_wait_a_second_after_the_turn() {
    let mut rig = afe_rig(true);
    answered_turn(&mut rig);
    let closes = response_closes(&rig);
    assert!(
        rig.turn_rows().is_empty(),
        "the row waits for the response subscription"
    );
    rig.silence(800);
    assert!(rig.turn_rows().is_empty(), "still within the second");
    assert_eq!(response_closes(&rig), closes, "the response stays open");
    rig.silence(400);
    assert_eq!(rig.turn_rows().len(), 1, "written once the second passed");
    assert_eq!(response_closes(&rig), closes + 1, "and the response closed");
}

#[tokio::test(start_paused = true)]
async fn a_mute_while_the_response_lingers_writes_the_row_without_the_responses_part() {
    let mut rig = afe_rig(true);
    answered_turn(&mut rig);
    rig.silence(600);
    assert!(rig.turn_rows().is_empty(), "still lingering");
    rig.handle(crate::Input::Privacy {
        muted: true,
        physical_seq: Some(1),
    });
    let row = rig.turn_rows().pop().expect("written at the mute").clone();
    let series: Value = serde_json::from_str(row.afe_series.unwrap().as_str()).unwrap();
    let recording = series["recording"].as_array().expect("the committed part");
    let committed_at = series["stages"]["endpoint"][1]
        .as_u64()
        .expect("an endpoint");
    // The recording stops where the committed subscription's mic did (the commit's
    // frontier), not through the answer the response subscription heard.
    assert!(
        recording[1].as_u64().unwrap() <= committed_at + 1_280,
        "{recording:?} vs {committed_at}"
    );
    // The response subscription heard on (the answer and the 600 ms after the turn).
    let mic_heard = rig.device.captured();
    assert!(recording[1].as_u64().unwrap() + 9_600 < mic_heard);
}

#[tokio::test(start_paused = true)]
async fn a_committed_turn_keeps_its_chart_through_the_answer_and_its_recording() {
    let mut rig = afe_rig(true);
    answered_turn(&mut rig);
    rig.silence(1_200);
    // The commit opened the response subscription: AFE records and, saving utterances,
    // the mic.
    let response = rig
        .uplink_commands()
        .into_iter()
        .find_map(|c| match c {
            UplinkCommand::Open(open) if open.owner.as_str().starts_with("response-") => {
                Some(open.clone())
            }
            _ => None,
        })
        .expect("a response subscription");
    assert!(
        response
            .streams
            .contains_key(&echomuse_protocol::StreamId::Afe)
    );
    assert!(
        response
            .streams
            .contains_key(&echomuse_protocol::StreamId::Mic)
    );
    let row = rig.turn_rows().pop().expect("persisted").clone();
    let series: Value =
        serde_json::from_str(row.afe_series.as_ref().expect("a chart").as_str()).unwrap();
    assert_eq!(series["period"], 1_280);
    let start = series["start"].as_u64().unwrap();
    // A second before the wake word's support, on the period grid.
    assert_eq!(
        start,
        (super::SUPPORT_START - 16_000.min(super::SUPPORT_START)) / 1_280 * 1_280
    );
    let frames = series["frames"].as_array().unwrap();
    let support = series["support"].as_array().unwrap();
    assert_eq!(support[0], super::SUPPORT_START);
    let recording = series["recording"]
        .as_array()
        .expect("a turn recording span");
    let (rec_start, rec_end) = (
        recording[0].as_u64().unwrap(),
        recording[1].as_u64().unwrap(),
    );
    assert!(rec_start >= start && rec_start <= super::SUPPORT_START);
    // The chart runs on past the commit, through the answer.
    let utterance_end = series["utterance"][1].as_u64().unwrap();
    assert!(
        rec_end > utterance_end + 12_800,
        "{rec_end} vs {utterance_end}"
    );
    assert!(frames.len() as u64 * 1_280 >= rec_end - start - 1_280);
    assert!(frames.iter().filter(|f| f.as_u64() == Some(10)).count() > 20);
    assert!(series["silero"].as_array().is_some_and(|s| !s.is_empty()));
    // The processing stages, in order on the capture timeline after the commit.
    let stages = series["stages"].as_object().expect("stages");
    let span = |name: &str| {
        let s = stages[name]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: {stages:?}"));
        (s[0].as_u64().unwrap(), s[1].as_u64().unwrap())
    };
    let endpoint = span("endpoint");
    let (stt, intent, tts, reply) = (span("stt"), span("intent"), span("tts"), span("reply"));
    assert!(endpoint.0 < endpoint.1 && endpoint.1 <= stt.0, "{stages:?}");
    assert!(stt.1 <= intent.0 && intent.0 <= intent.1, "{stages:?}");
    assert!(tts.1 == reply.0 && reply.0 <= reply.1, "{stages:?}");
    // The evidence summaries, in the column and the decision trace alike.
    let evidence: Value =
        serde_json::from_str(row.afe_evidence.as_ref().expect("evidence").as_str()).unwrap();
    assert_eq!(evidence["support_start"], super::SUPPORT_START);
    assert_eq!(evidence["wake"]["start"], super::SUPPORT_START);
    assert_eq!(
        evidence["wake"]["frames"].as_u64(),
        Some(evidence["wake"]["frames_expected"].as_u64().unwrap())
    );
    assert_eq!(evidence["utterance"]["start"], series["utterance"][0]);
    let trace: Value =
        serde_json::from_str(row.decision_trace.as_ref().expect("a trace").as_str()).unwrap();
    assert_eq!(trace["afe_evidence"], evidence);
    // The recording goes with the row; the records service publishes it under its id.
    let published = rig
        .persisted
        .iter()
        .find_map(|r| match r {
            PersistRequest::Turn { audio, .. } => audio.turn_recording.as_ref(),
            _ => None,
        })
        .map(|pcm| pcm.len() as u64)
        .expect("the turn recording was handed over");
    assert_eq!(published, rec_end - rec_start);
}

#[tokio::test(start_paused = true)]
async fn without_saved_utterances_the_chart_has_no_recording() {
    let mut rig = afe_rig(false);
    answered_turn(&mut rig);
    rig.silence(1_200);
    let row = rig.turn_rows().pop().expect("persisted").clone();
    let series: Value = serde_json::from_str(row.afe_series.unwrap().as_str()).unwrap();
    assert_eq!(series["recording"], Value::Null);
    assert!(!rig.persisted.iter().any(|r| matches!(r,
        PersistRequest::Turn { audio, .. } if audio.turn_recording.is_some())));
}

#[tokio::test(start_paused = true)]
async fn a_session_without_afe_metadata_stores_no_chart() {
    let mut rig = Rig::new(Options::default());
    answered_turn(&mut rig);
    let row = rig.turn_rows().pop().expect("persisted").clone();
    assert!(row.afe_series.is_none());
    assert!(row.afe_evidence.is_none(), "unavailable, not empty");
}

#[tokio::test(start_paused = true)]
async fn a_recorded_near_miss_gets_its_chart() {
    let mut settings = echomuse_settings::DeviceSettings::default();
    settings.wakeword.save_wake_clips = true;
    let mut rig = Rig::new(Options {
        capabilities: capabilities(&["afe_metadata_v1", "near_miss_v1"]),
        afe_metadata: true,
        settings,
        ..Options::default()
    });
    rig.silence(2_000);
    let (support_start, support_end) = (16_000u64, 24_000u64);
    let body = serde_json::json!({
        "near_miss_id": "nm-1",
        "capture_epoch": rig.device.capture_epoch.to_string(),
        "graph_sha256": null, "profile": "idle", "floor": 0.17, "peak": 0.48,
        "support_start": support_start.to_string(), "support_end": support_end.to_string(),
        "reason": "below",
    })
    .as_object()
    .expect("an object")
    .clone();
    rig.control(echomuse_protocol::MessageType::WakeNearMiss, body, 0);
    rig.silence(1_500);
    let (record, audio) = rig
        .persisted
        .iter()
        .find_map(|r| match r {
            PersistRequest::NearMiss { record, audio } => Some((record, audio)),
            _ => None,
        })
        .expect("the near miss was written");
    assert!(matches!(
        audio,
        echomuse_records::NearMissAudio::Recorded(_)
    ));
    let series: Value =
        serde_json::from_str(record.afe_series.as_ref().expect("a chart").as_str()).unwrap();
    assert_eq!(series["start"], 0, "a second before the support");
    assert_eq!(
        series["support"],
        serde_json::json!([support_start, support_end])
    );
    assert_eq!(series["recording"][1], support_end + 4_800);
    assert_eq!(series["utterance"], Value::Null);
    assert_eq!(series["stages"], serde_json::json!({}));
    let frames = series["frames"].as_array().unwrap();
    assert!(frames.iter().filter(|f| f.as_u64() == Some(10)).count() >= 20);
}

#[tokio::test(start_paused = true)]
async fn a_refused_candidate_carries_its_evidence() {
    let mut rig = afe_rig(false);
    rig.silence(500);
    // Muted: the candidate is refused on arrival, with no records behind it.
    rig.handle(crate::Input::Privacy {
        muted: true,
        physical_seq: Some(1),
    });
    rig.wake(
        "c1",
        super::SUPPORT_START,
        super::SUPPORT_START + 8_000,
        false,
    );
    let row = rig.turn_rows().pop().expect("a refused row").clone();
    let evidence: Value =
        serde_json::from_str(row.afe_evidence.as_ref().expect("evidence").as_str()).unwrap();
    assert_eq!(evidence["support_start"], super::SUPPORT_START);
    assert_eq!(evidence["wake"], Value::Null, "no data, not zeros");
    assert_eq!(evidence["utterance"], Value::Null);
}
