//! Pause transcription by a Wyoming server (`pauseAsr` with engine `wyoming`, §4.7): when
//! the machine asks the server, what audio it sends, where the answers go, and what the
//! turn row records. The precedence rules themselves are the turn policy's
//! (`echomuse-turn`'s endpoint tests).

use echomuse_core::services::{MediaUrl, PipelineId, RunEvent, Transcript};
use echomuse_protocol::StreamId;
use echomuse_records::vocab::TerminalReason;
use echomuse_settings::values::{PauseAsr, PauseAsrEngine};
use echomuse_turn::{PauseError, Sample, ServerFailure, TextSource};
use echomuse_uplink::buffer::Span;
use serde_json::Value;

use super::{
    intent_end,
    rig::{Options, Rig},
    wake,
};
use crate::{HaReply, HaRequest, Input};

fn wyoming_rig() -> Rig {
    let mut options = Options::default();
    options.settings.microphones.pause_asr = PauseAsr::new(
        PauseAsrEngine::Wyoming,
        "wyoming-faster-whisper".to_owned(),
        10_300,
        String::new(),
        "en".to_owned(),
    )
    .unwrap();
    let mut rig = Rig::new(options);
    rig.voice.set_recording(true);
    rig
}

/// A command of the rig's words the local grammar cannot judge.
fn unjudged() -> Vec<&'static str> {
    let grammar = echomuse_grammar::Grammar::new().unwrap();
    let candidates: [&[&'static str]; 5] = [
        &["kitchen", "light", "time"],
        &["it", "kitchen"],
        &["light", "kitchen", "on"],
        &["stop", "kitchen", "time"],
        &["the", "light", "it"],
    ];
    candidates
        .iter()
        .find(|words| grammar.classify(&words.join(" ")) == echomuse_grammar::Completeness::Unknown)
        .expect("one candidate is beyond the grammar")
        .to_vec()
}

/// Wakes, says a command the grammar cannot judge, and pauses long enough for a pause
/// decode; HA's sentence matcher is never answered.
fn pause_after_command(rig: &mut Rig) {
    wake(rig, "c1");
    let speech = rig.words(&unjudged());
    rig.hear(&speech);
    rig.silence(480);
}

/// Listens in 80 ms of silence until the turn commits (its pipeline is asked for);
/// returns how many blocks that took, or `None` within `blocks`.
fn blocks_to_commit(rig: &mut Rig, blocks: usize) -> Option<usize> {
    for k in 0..blocks {
        if rig
            .ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::Pipeline { .. }))
        {
            return Some(k);
        }
        rig.silence(80);
    }
    None
}

/// The pauses the policy recorded so far.
fn pauses(rig: &Rig) -> Vec<echomuse_turn::PauseRecord> {
    let turn = rig.voice.turn.as_ref().expect("a turn");
    match (&turn.policy, &turn.utterance) {
        (Some(policy), _) => policy.trace().pauses.clone(),
        (None, Some(utterance)) => utterance.pauses.clone(),
        (None, None) => Vec::new(),
    }
}

/// Answers the committed turn's HA requests with `heard` as HA's transcript, through to
/// the answer played and the row written; returns the row's decision trace.
fn answer_from_ha(rig: &mut Rig, heard: &str) -> Value {
    let pipeline = rig.take_ha("pipeline", |r| matches!(r, HaRequest::Pipeline { .. }));
    rig.reply(
        pipeline,
        HaReply::Pipeline(Ok(PipelineId("pipeline-1".to_owned()))),
    );
    let stt = rig.take_ha("stt", |r| matches!(r, HaRequest::Stt { .. }));
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: heard.to_owned(),
        })),
    );
    let intent = rig.take_ha("intent", |r| matches!(r, HaRequest::IntentTts { .. }));
    rig.reply(intent, HaReply::IntentDispatched(Ok(())));
    rig.reply(
        intent,
        HaReply::IntentEvent(intent_end("It is noon.", false)),
    );
    rig.reply(
        intent,
        HaReply::IntentEvent(RunEvent::TtsReady {
            url: MediaUrl("http://ha/api/tts_proxy/a.wav".to_owned()),
            streamed: false,
        }),
    );
    rig.play_out(1.0);
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
    let rows = rig.turn_rows();
    let row = rows.last().expect("the turn was persisted");
    serde_json::from_str(row.decision_trace.as_ref().expect("a trace").as_str()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_pause_goes_to_the_server_as_its_decode_starts() {
    let mut rig = wyoming_rig();
    pause_after_command(&mut rig);
    let pauses = pauses(&rig);
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    // One request for the pause, about exactly the pause decode's audio.
    assert_eq!(rig.server_requests.len(), 1);
    let (_, through, request) = &rig.server_requests[0];
    assert_eq!(*through, pauses[0].through);
    assert_eq!(request.limit, crate::PAUSE_SERVER_LIMIT);
    assert_eq!(
        (request.host.as_str(), request.port, request.model.as_str()),
        ("wyoming-faster-whisper", 10_300, "")
    );
    assert_eq!(request.language, "en");
    // The utterance from its start, at the recognizers' +20 dB.
    let turn = rig.voice.turn.as_ref().unwrap();
    let start = turn.policy.as_ref().unwrap().trace().start;
    let mut canonical = Vec::new();
    rig.voice
        .uplink
        .timeline()
        .read_pcm(
            StreamId::Mic,
            turn.epoch().unwrap(),
            Span {
                start: start.0,
                end: through.0,
            },
            &mut canonical,
        )
        .unwrap();
    let expected: Vec<i16> = canonical.iter().map(|s| s.saturating_mul(10)).collect();
    assert_eq!(&request.pcm[..], &expected[..]);
    assert!(expected.contains(&i16::MAX), "loud words saturate");
    // Asked when the worker started the pause decode: before its result reached the policy.
    let asked = rig.evidence.iter().position(|(_, e)| {
        matches!(e, crate::evidence::Evidence::PauseServerAsked { pause, .. } if pause == through)
    });
    let decoded = rig.evidence.iter().position(|(_, e)| {
        matches!(e, crate::evidence::Evidence::Asr { observation, .. }
            if observation.through == *through
                && observation.kind == echomuse_turn::AsrKind::Finalized)
    });
    assert!(
        asked.is_some() && asked < decoded,
        "{asked:?} before {decoded:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_servers_words_end_the_turn_and_are_recorded() {
    // Kroko alone: its words are unknown and HA's matcher never answers, so the turn
    // waits out the long pause.
    let mut alone = Rig::new(Options::default());
    pause_after_command(&mut alone);
    let kroko_blocks = blocks_to_commit(&mut alone, 60).expect("Kroko's turn commits");
    assert!(alone.server_requests.is_empty());

    let mut rig = wyoming_rig();
    rig.server_answers
        .push_back(Ok("Ophelia, set a timer for five minutes.".to_owned()));
    pause_after_command(&mut rig);
    let blocks = blocks_to_commit(&mut rig, 60).expect("the turn commits");
    assert!(
        blocks < kroko_blocks,
        "the server's complete words end the turn sooner: {blocks} vs {kroko_blocks}"
    );
    let commit = rig
        .voice
        .turn
        .as_ref()
        .and_then(|t| t.commit.as_ref())
        .map(|(c, _, _)| c.clone())
        .unwrap();
    assert_eq!(commit.text, "set a timer for five minutes");
    assert_eq!(commit.source, TextSource::Server);
    assert!(rig.evidence.iter().any(|(_, e)| matches!(e,
        crate::evidence::Evidence::PauseServerAnswer { answer, .. }
            if answer.outcome.as_deref() == Ok("Ophelia, set a timer for five minutes."))));

    // HA still transcribes the audio itself; the server's words decided only the end.
    let trace = answer_from_ha(&mut rig, "Ophelia, set a timer for five minutes.");
    let rows = rig.turn_rows();
    let row = rows.last().unwrap();
    assert_eq!(row.policy_hash.as_str(), "post_afe_10+wyoming:default");
    assert_eq!(
        row.stt_text.as_deref(),
        Some("set a timer for five minutes.")
    );
    let u = &trace["utterance"];
    assert_eq!(u["heard"], "Ophelia, set a timer for five minutes.");
    assert_eq!(u["stable_prefix"], "set a timer for five minutes");
    let pause = &u["pauses"][0];
    assert_eq!(
        pause["server"]["text"],
        "Ophelia, set a timer for five minutes."
    );
    assert_eq!(pause["server"]["judged"], "set a timer for five minutes");
    assert!(pause["server"]["at"].as_u64() > pause["through"].as_u64());
    assert!(pause["server"]["ms"].is_u64(), "{pause}");
    assert_eq!(pause["kroko"]["judged"], unjudged().join(" "));
    let commit = u["endpoint"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "commit")
        .unwrap();
    assert_eq!(commit["source"], "server");
}

#[tokio::test(start_paused = true)]
async fn answers_are_applied_only_to_the_call_and_audio_they_were_asked_about() {
    let mut rig = wyoming_rig();
    pause_after_command(&mut rig);
    let (call, through, _) = rig.server_requests[0].clone();
    let stale = rig.voice.counters.stale_results;
    // An answer naming other audio, and one under a call never made, change nothing.
    rig.handle(Input::ServerTranscript {
        call,
        through: Sample(through.0 - 1_280),
        result: Ok("Ophelia, what time is it?".to_owned()),
    });
    rig.handle(Input::ServerTranscript {
        call: crate::CallId(u64::MAX),
        through,
        result: Ok("Ophelia, what time is it?".to_owned()),
    });
    assert_eq!(rig.voice.counters.stale_results, stale + 1);
    rig.silence(160);
    let server = pauses(&rig)[0].server.clone().unwrap();
    assert!(server.is_open(), "{server:?}");
    // The call was spent on the mismatched answer: the real one is dropped too.
    rig.handle(Input::ServerTranscript {
        call,
        through,
        result: Ok("Ophelia, what time is it?".to_owned()),
    });
    rig.silence(160);
    assert!(pauses(&rig)[0].server.as_ref().unwrap().is_open());
}

#[tokio::test(start_paused = true)]
async fn an_unreachable_server_leaves_krokos_words_alone() {
    let mut rig = wyoming_rig();
    rig.server_answers.push_back(Err(ServerFailure::Failed(
        "wyoming-faster-whisper:10300: could not connect".to_owned(),
    )));
    pause_after_command(&mut rig);
    blocks_to_commit(&mut rig, 60).expect("the turn commits");
    let pause = pauses(&rig).remove(0);
    let server = pause.server.unwrap();
    assert_eq!(
        server.error,
        Some(PauseError::Failed(
            "wyoming-faster-whisper:10300: could not connect".to_owned()
        ))
    );
    assert_eq!((server.text, server.judged), (None, None));
    let commit = rig
        .voice
        .turn
        .as_ref()
        .and_then(|t| t.commit.as_ref())
        .map(|(c, _, _)| c.clone())
        .unwrap();
    assert_eq!(commit.text, unjudged().join(" "));
    assert_ne!(commit.source, TextSource::Server);
}

#[tokio::test(start_paused = true)]
async fn kroko_alone_asks_no_server_and_records_no_server_words() {
    let mut rig = Rig::new(Options::default());
    rig.voice.set_recording(true);
    pause_after_command(&mut rig);
    blocks_to_commit(&mut rig, 60).expect("the turn commits");
    assert!(rig.server_requests.is_empty());
    let pauses = pauses(&rig);
    assert!(!pauses.is_empty());
    assert!(pauses.iter().all(|p| p.server.is_none()));
    let trace = answer_from_ha(&mut rig, "Ophelia, kitchen light time.");
    assert_eq!(
        rig.turn_rows().last().unwrap().policy_hash.as_str(),
        "post_afe_10"
    );
    assert_eq!(trace["utterance"]["pauses"][0]["server"], Value::Null);
}

/// Wakes, says the unjudged command, pauses 400 ms, then a quieter sound the recognizer
/// has no word for (a TV across the room) runs on.
fn command_then_tv(rig: &mut Rig) {
    wake(rig, "c1");
    let speech = rig.words(&unjudged());
    rig.hear(&speech);
    rig.silence(150);
    let tv = crate::speech::testing::tone(super::rig::WORDS.len() + 5, 4_000, 600);
    for block in tv.chunks(1_280) {
        rig.hear(block);
        if rig
            .ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::Pipeline { .. }))
            || rig.voice.turn.as_ref().is_none_or(|t| t.policy.is_none())
        {
            break;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_route_b_commit_on_the_servers_words_is_rechecked_by_the_same_server() {
    for (recheck, terminal) in [
        (Ok("Ophelia, set a timer for five minutes."), None),
        (
            Ok("Ophelia, set a timer for fifteen minutes."),
            Some(TerminalReason::Retry),
        ),
        (
            Err(ServerFailure::Timeout),
            Some(TerminalReason::Interrupted),
        ),
    ] {
        let mut rig = wyoming_rig();
        rig.server_answers
            .push_back(Ok("Ophelia, set a timer for five minutes.".to_owned()));
        rig.server_answers
            .push_back(recheck.clone().map(str::to_owned));
        command_then_tv(&mut rig);
        let rows = rig.turn_rows();
        let committed = rig
            .voice
            .turn
            .as_ref()
            .and_then(|t| t.commit.as_ref())
            .map(|(c, _, _)| c.clone());
        let route = committed.as_ref().map(|c| c.route).or_else(|| {
            rows.last()
                .and_then(|r| r.commit.as_ref())
                .map(|c| match c.route {
                    echomuse_records::vocab::CommitRoute::B => echomuse_turn::Route::B,
                    _ => echomuse_turn::Route::A,
                })
        });
        assert_eq!(route, Some(echomuse_turn::Route::B), "{recheck:?}");
        // The committed span went to the server, not to Kroko's re-decode.
        assert_eq!(rig.server_requests.len(), 2, "{recheck:?}");
        assert_eq!(rig.server_requests[1].2.limit, crate::SPAN_SERVER_LIMIT);
        assert!(
            !rig.log.iter().any(|l| l.contains("Redecode")),
            "{recheck:?}"
        );
        match terminal {
            None => assert!(
                rig.ha
                    .iter()
                    .any(|(_, r)| matches!(r, HaRequest::Pipeline { .. })),
                "the re-check agreed: on to STT"
            ),
            Some(reason) => {
                assert_eq!(
                    rig.turn_rows().last().map(|r| r.terminal_reason),
                    Some(reason),
                    "{recheck:?}"
                );
            }
        }
    }
}
