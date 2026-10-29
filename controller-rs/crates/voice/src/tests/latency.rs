//! `turns.response_latency_ms` and the decision trace's pause view: the end of the last
//! word to the answer's first frame on the device's clock, and the stable prefix, pause
//! decodes and commit source the dashboard reads.

use echomuse_core::services::{MediaUrl, RunEvent, Transcript};
use echomuse_protocol::MessageType;
use serde_json::{Value, json};

use super::rig::{Options, Rig};
use super::{command, intent_end, wake};
use crate::{DecodeCommand, HaReply, HaRequest};

/// Device nanoseconds per capture sample, as the simulated device stamps its mic.
const NS_PER_SAMPLE: u64 = 62_500;

/// A wake turn answered by HA, whose answer the device reports first playing
/// `first_frame_after_ns` after the last captured sample (or never, with `None`).
/// Returns the device time of the answer's first frame, when reported.
fn answered(rig: &mut Rig, first_frame_after_ns: Option<u64>) -> Option<u64> {
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
    let call = rig
        .decodes
        .iter()
        .rev()
        .find_map(|c| match c {
            DecodeCommand::Start { call, .. } => Some(*call),
            _ => None,
        })
        .expect("a decode");
    rig.handle(crate::Input::Decode {
        call,
        event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Opened {
            seekable: echomuse_playback::Seekability::Seekable,
        }),
    });
    rig.handle(crate::Input::Decode {
        call,
        event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Audio(vec![800; 9_600])),
    });
    let mut first_frame = None;
    if let Some(after) = first_frame_after_ns {
        // 4,800 frames (100 ms at 48 kHz) completed by the reported time.
        let (playback_id, generation) = rig.last_render_start();
        let mono = rig.device.captured() * NS_PER_SAMPLE + after;
        first_frame = Some(mono - 100_000_000);
        let progress = json!({
            "playback_id": playback_id, "event": "progress", "submitted_frames": "9600",
            "completed_frames": "4800", "mono_ns": mono.to_string(), "uncertainty_us": 200,
            "timing_quality": "measured", "reference_coverage": "full",
        });
        rig.control(
            MessageType::RenderProgress,
            progress.as_object().unwrap().clone(),
            generation,
        );
    }
    rig.finish_render(9_600);
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
    first_frame
}

fn trace(rig: &Rig) -> Value {
    let row = rig.turn_rows().pop().expect("a turn row").clone();
    serde_json::from_str(row.decision_trace.as_ref().expect("a trace").as_str()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn response_latency_runs_from_the_last_word_to_the_first_frame_on_the_device_clock() {
    let mut rig = Rig::new(Options::default());
    let first_frame = answered(&mut rig, Some(700_000_000)).unwrap();
    let row = rig.turn_rows().pop().expect("a turn row").clone();
    let trace = trace(&rig);
    let commit = trace["utterance"]["endpoint"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "commit")
        .expect("a commit")
        .clone();
    let boundary_ns = commit["boundary"].as_u64().unwrap() * NS_PER_SAMPLE;
    // The commit boundary on the mic stream's clock to the progress time less the 100 ms
    // of frames it reported played.
    assert_eq!(
        row.response_latency.expect("measured"),
        std::time::Duration::from_nanos(first_frame - boundary_ns)
    );
}

#[tokio::test(start_paused = true)]
async fn response_latency_is_null_without_device_timing() {
    let mut rig = Rig::new(Options::default());
    answered(&mut rig, None);
    let row = rig.turn_rows().pop().expect("a turn row").clone();
    assert_eq!(row.response_latency, None, "not measured, not zero");
}

#[tokio::test(start_paused = true)]
async fn the_trace_carries_the_pause_view_the_dashboard_reads() {
    let mut rig = Rig::new(Options::default());
    answered(&mut rig, Some(500_000_000));
    let trace = trace(&rig);
    let utterance = &trace["utterance"];
    assert!(utterance["stable_prefix"].is_string(), "{utterance}");
    let pauses = utterance["pauses"].as_array().expect("pauses");
    assert!(
        !pauses.is_empty(),
        "the pause before the commit was decoded: {utterance}"
    );
    for pause in pauses {
        assert!(pause["through"].is_u64(), "{pause}");
        assert!(pause["kroko"]["text"].is_string(), "{pause}");
        assert!(pause["kroko"]["ms"].is_null(), "{pause}");
    }
    let commit = utterance["endpoint"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "commit")
        .expect("a commit");
    let source = commit["source"].as_str().expect("a source");
    assert!(["kroko", "streaming"].contains(&source), "{commit}");
}

#[tokio::test(start_paused = true)]
async fn a_wake_heard_while_playing_has_the_verification_view_at_the_top_of_its_trace() {
    let mut rig = Rig::new(Options::default());
    rig.silence(3_000);
    let word = rig.words(&["ophelia"]);
    rig.play(&word);
    rig.wake("c1", 48_000 - 6_400, 48_000 + 10_400, true);
    rig.silence(1_000);
    let trace = trace(&rig);
    let verification = trace["verification"]
        .as_object()
        .unwrap_or_else(|| panic!("a verification view: {trace}"));
    for key in [
        "deadline_ms",
        "input",
        "input_ready_ms",
        "mic_past_open_ms",
        "answered_ms",
        "decided_ms",
        "result",
        "queued_ms",
        "decode_ms",
    ] {
        assert!(verification.contains_key(key), "{key}: {trace}");
    }
    assert!(verification["mic_past_open_ms"].is_u64(), "{trace}");
    // The worker does not split its wait from its decode: unknown, not zero.
    assert!(verification["queued_ms"].is_null());
    assert!(verification["decode_ms"].is_null());
}
