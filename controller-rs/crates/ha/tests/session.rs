//! The Home Assistant session against a simulated HA speaking HA 2026.8.1's message shapes.

mod fake_ha;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use echomuse_core::{
    services::{
        ConversationId, DirectIntent, DirectIntentRequest, EffectOutcome, HaConnectionGeneration,
        HaDeviceId, HaError, HaTimer, HomeAssistant, IntentEnd, IntentRequest, MediaUrl,
        NotDispatched, PipelineId, RecognizeRequest, RunEvent, SatelliteTarget,
        SentenceRecognition, SlotValue, SttRequest, TimerSelector, TimerStatus, Transcript,
        TtsRequest, UnknownCause,
    },
    types::{DeviceId, RuntimeId, SessionId, TurnId, WorkIdentity, WorkScope},
};
use echomuse_ha::{ConnectionFault, HaClient, HaEndpoint, HaOptions, SessionEnd};
use fake_ha::{FakeConnection, FakeHa, HA_VERSION, Inbound, TOKEN, within};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const DEVICE: &str = "b285ec5e94b98f4c1cbeabf926be4b77";
const PIPELINE: &str = "01hnpec9vt52ec4dkkxqa89z46";

fn options() -> HaOptions {
    HaOptions {
        command_timeout: Duration::from_secs(2),
        reconnect_initial: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(200),
        heartbeat: Duration::from_secs(60),
        stt_reconnect_wait: Duration::from_secs(2),
        subscription_capacity: 4,
    }
}

fn scope() -> WorkScope {
    WorkScope::new(
        WorkIdentity {
            device: DeviceId("kitchen".to_owned()),
            runtime: RuntimeId(1),
            session: SessionId(1),
            turn: Some(TurnId(1)),
        },
        CancellationToken::new(),
    )
}

fn cancelled_scope() -> WorkScope {
    let scope = scope();
    scope.cancel();
    scope
}

fn device() -> HaDeviceId {
    HaDeviceId(DEVICE.to_owned())
}

fn target() -> SatelliteTarget {
    SatelliteTarget {
        pipeline: PipelineId(PIPELINE.to_owned()),
        ha_device: Some(device()),
    }
}

struct Session {
    client: HaClient,
    task: JoinHandle<SessionEnd>,
    ha: FakeHa,
    connection: FakeConnection,
}

async fn session_with(options: HaOptions) -> Session {
    let mut ha = FakeHa::start().await;
    let endpoint = HaEndpoint::direct(&ha.base_url(), TOKEN).unwrap();
    let (client, task) = HaClient::start(endpoint, options).unwrap();
    let connection = ha.connection().await;
    wait_for_generation(&client, 1).await;
    Session {
        client,
        task,
        ha,
        connection,
    }
}

async fn session() -> Session {
    session_with(options()).await
}

async fn wait_for_generation(client: &HaClient, generation: u64) {
    let mut status = client.status();
    within(status.wait_for(|status| status.generation == Some(HaConnectionGeneration(generation))))
        .await
        .unwrap();
}

fn recognize_request(scope: WorkScope, sentences: &[&str]) -> RecognizeRequest {
    RecognizeRequest {
        scope,
        ha_device: Some(device()),
        sentences: sentences.iter().map(|s| (*s).to_owned()).collect(),
        language: None,
    }
}

fn intent_request(scope: WorkScope, text: &str) -> IntentRequest {
    IntentRequest {
        scope,
        target: target(),
        text: text.to_owned(),
        conversation: None,
    }
}

/// HA's `IntentResponse.as_dict` for a conversation agent's reply.
fn intent_output(speech: &str, conversation: &str, continue_conversation: bool) -> Value {
    json!({
        "response": {
            "speech": {"plain": {"speech": speech, "extra_data": null}},
            "card": {},
            "language": "en",
            "response_type": "action_done",
            "data": {"targets": [], "success": [], "failed": []}
        },
        "conversation_id": conversation,
        "continue_conversation": continue_conversation
    })
}

fn run_start(tts_url: Option<(&str, bool)>, handler: Option<u8>) -> Value {
    let mut data = json!({
        "pipeline": PIPELINE,
        "language": "en",
        "conversation_id": "01K8CONV",
        "runner_data": {"stt_binary_handler_id": handler, "timeout": 30},
    });
    if let Some((url, stream_response)) = tts_url {
        data["tts_output"] = json!({
            "token": "abc.flac", "url": url, "mime_type": "audio/flac",
            "stream_response": stream_response
        });
    }
    data
}

fn tts_end(url: &str) -> Value {
    json!({"tts_output": {
        "media_id": "media-source://tts/-stream-/abc.flac", "token": "abc.flac",
        "url": url, "mime_type": "audio/flac"
    }})
}

async fn next_event(events: &mut mpsc::Receiver<RunEvent>) -> Option<RunEvent> {
    within(events.recv()).await
}

async fn all_events(events: &mut mpsc::Receiver<RunEvent>) -> Vec<RunEvent> {
    let mut collected = Vec::new();
    while let Some(event) = next_event(events).await {
        collected.push(event);
    }
    collected
}

/// One recognition round trip: HA answers in order, so every message sent before this
/// reply has been handled once it returns.
async fn barrier(session: &mut Session) {
    let client = session.client.clone();
    let call = tokio::spawn(async move {
        client
            .recognize(recognize_request(scope(), &["barrier"]))
            .await
    });
    let request = session.connection.request().await;
    assert_eq!(request["type"], "conversation/agent/homeassistant/debug");
    session
        .connection
        .result(&request["id"], json!({"results": [null]}));
    assert_eq!(within(call).await.unwrap(), Ok(vec![None]));
}

// ── Connection ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn authentication_sends_the_token_and_reports_the_connection() {
    let session = session().await;
    assert_eq!(
        session.connection.auth,
        json!({"type": "auth", "access_token": TOKEN})
    );
    let status = session.client.status().borrow().clone();
    assert_eq!(status.generation, Some(HaConnectionGeneration(1)));
    assert_eq!(status.ha_version.as_deref(), Some(HA_VERSION));
    assert_eq!(status.last_fault, None);

    session.client.stop();
    assert_eq!(within(session.task).await.unwrap(), SessionEnd::Stopped);
    assert_eq!(session.client.status().borrow().generation, None);
}

#[tokio::test]
async fn a_refused_token_stops_the_session_without_retrying() {
    let ha = FakeHa::start().await;
    let endpoint = HaEndpoint::direct(&ha.base_url(), "wrong-token").unwrap();
    let (client, task) = HaClient::start(endpoint, options()).unwrap();
    assert_eq!(
        within(task).await.unwrap(),
        SessionEnd::AuthRejected("Invalid access token or password".to_owned())
    );
    let status = client.status().borrow().clone();
    assert_eq!(status.generation, None);
    assert_eq!(
        status.last_fault,
        Some(ConnectionFault::AuthRejected(
            "Invalid access token or password".to_owned()
        ))
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ha.websocket_attempts(), 1);
    assert_eq!(
        client
            .recognize(recognize_request(scope(), &["what time is it"]))
            .await,
        Err(HaError::NotDispatched(NotDispatched::Unavailable))
    );
}

#[tokio::test]
async fn concurrent_commands_share_the_connection_and_take_their_own_replies() {
    let mut session = session().await;
    let first_client = session.client.clone();
    let first = tokio::spawn(async move {
        first_client
            .recognize(recognize_request(scope(), &["first"]))
            .await
    });
    let one = session.connection.request().await;
    let second_client = session.client.clone();
    let second = tokio::spawn(async move {
        second_client
            .recognize(recognize_request(scope(), &["second"]))
            .await
    });
    let two = session.connection.request().await;
    assert_eq!(one["sentences"], json!(["first"]));
    assert_eq!(two["sentences"], json!(["second"]));
    assert!(two["id"].as_u64().unwrap() > one["id"].as_u64().unwrap());

    // HA answers the later request first.
    let trigger = |template: &str| json!({"results": [{"match": true, "source": "trigger", "sentence_template": template}]});
    session.connection.result(&two["id"], trigger("second"));
    session.connection.result(&one["id"], trigger("first"));
    let template = |result: Result<Vec<Option<SentenceRecognition>>, HaError>| {
        result.unwrap()[0].as_ref().unwrap().template.clone()
    };
    assert_eq!(template(within(second).await.unwrap()), "second");
    assert_eq!(template(within(first).await.unwrap()), "first");
}

#[tokio::test]
async fn an_unanswered_command_times_out_and_its_late_reply_is_ignored() {
    let mut session = session_with(HaOptions {
        command_timeout: Duration::from_millis(200),
        ..options()
    })
    .await;
    let client = session.client.clone();
    let call = tokio::spawn(async move {
        client
            .recognize(recognize_request(scope(), &["slow"]))
            .await
    });
    let request = session.connection.request().await;
    assert_eq!(within(call).await.unwrap(), Err(HaError::Timeout));

    session
        .connection
        .result(&request["id"], json!({"results": [null]}));
    barrier(&mut session).await;
    assert_eq!(
        session.client.status().borrow().generation,
        Some(HaConnectionGeneration(1))
    );
}

#[tokio::test]
async fn a_lost_connection_fails_open_work_at_once_and_the_next_one_is_a_new_generation() {
    let mut session = session().await;
    let client = session.client.clone();
    let pending = tokio::spawn(async move {
        client
            .recognize(recognize_request(scope(), &["pending"]))
            .await
    });
    session.connection.request().await;
    let mut run = session
        .client
        .run_intent_tts(intent_request(scope(), "turn on the lights"))
        .await
        .unwrap();
    assert_eq!(run.generation, HaConnectionGeneration(1));
    let dispatched = session.connection.request().await;
    assert_eq!(dispatched["type"], "assist_pipeline/run");
    session.connection.result(&dispatched["id"], Value::Null);

    session.connection.close();
    assert_eq!(within(pending).await.unwrap(), Err(HaError::ConnectionLost));
    assert_eq!(all_events(&mut run.events).await, vec![RunEvent::Lost]);

    wait_for_generation(&session.client, 2).await;
    let mut second = session.ha.connection().await;
    let client = session.client.clone();
    let call = tokio::spawn(async move {
        client
            .recognize(recognize_request(scope(), &["again"]))
            .await
    });
    // The intent run is never resent: the first request on the new connection is this one.
    let request = second.request().await;
    assert_eq!(request["type"], "conversation/agent/homeassistant/debug");
    second.result(&request["id"], json!({"results": [null]}));
    assert_eq!(within(call).await.unwrap(), Ok(vec![None]));
}

#[tokio::test]
async fn subscriptions_deliver_events_and_end_when_the_subscriber_falls_behind() {
    let mut session = session().await;
    let client = session.client.clone();
    let subscribe =
        tokio::spawn(async move { client.subscribe_events("echomuse_alert_request").await });
    let request = session.connection.request().await;
    assert_eq!(request["type"], "subscribe_events");
    assert_eq!(request["event_type"], "echomuse_alert_request");
    session.connection.result(&request["id"], Value::Null);
    let mut subscription = within(subscribe).await.unwrap().unwrap();
    assert_eq!(subscription.generation, HaConnectionGeneration(1));

    let event = |n: u64| {
        json!({
            "event_type": "echomuse_alert_request",
            "data": {"request_id": n},
            "origin": "LOCAL",
            "time_fired": "2026-10-28T09:00:00.000000+00:00",
            "context": {"id": "01K8", "parent_id": null, "user_id": null}
        })
    };
    // Capacity is 4 in these options: the fifth unread event ends the subscription.
    for n in 0..5 {
        session.connection.event(&request["id"], event(n));
    }
    barrier(&mut session).await;
    let mut seen = Vec::new();
    while let Some(event) = within(subscription.events.recv()).await {
        assert_eq!(event.event_type, "echomuse_alert_request");
        seen.push(event.data["request_id"].as_u64().unwrap());
    }
    assert_eq!(seen, vec![0, 1, 2, 3]);

    let client = session.client.clone();
    let unsubscribe = tokio::spawn(async move { client.unsubscribe(subscription).await });
    let request_two = session.connection.request().await;
    assert_eq!(
        request_two,
        json!({"type": "unsubscribe_events", "subscription": request["id"], "id": request_two["id"]})
    );
    session.connection.result(&request_two["id"], Value::Null);
    assert_eq!(within(unsubscribe).await.unwrap(), Ok(()));
}

// ── STT ──────────────────────────────────────────────────────────────────────

fn stt_request(audio: Vec<i16>) -> SttRequest {
    SttRequest {
        scope: scope(),
        target: target(),
        audio: Arc::from(audio),
    }
}

/// Plays HA's side of an STT run that has been requested: run-start with `handler`, then
/// the audio, then the transcript. Returns the binary frames received.
async fn answer_stt(connection: &mut FakeConnection, id: &Value, handler: u8) -> Vec<Vec<u8>> {
    connection.result(id, Value::Null);
    connection.pipeline_event(id, "run-start", run_start(None, Some(handler)));
    connection.pipeline_event(
        id,
        "stt-start",
        json!({"engine": "stt.faster_whisper", "metadata": {"language": "en"}}),
    );
    let mut frames = Vec::new();
    loop {
        let frame = connection.binary().await;
        let last = frame.len() == 1;
        frames.push(frame);
        if last {
            break;
        }
    }
    connection.pipeline_event(
        id,
        "stt-end",
        json!({"stt_output": {"text": " Turn on the kitchen lights. "}}),
    );
    connection.pipeline_event(id, "run-end", Value::Null);
    frames
}

#[tokio::test]
async fn stt_sends_the_audio_as_handler_prefixed_frames_ended_by_the_bare_handler() {
    let mut session = session().await;
    // Two full half-second frames and three samples more.
    let audio: Vec<i16> = (0..16_003).map(|n| (n * 7 - 20_000) as i16).collect();
    let client = session.client.clone();
    let request = stt_request(audio.clone());
    let call = tokio::spawn(async move { client.run_stt(request).await });

    let run = session.connection.request().await;
    assert_eq!(
        run,
        json!({
            "id": run["id"],
            "type": "assist_pipeline/run",
            "start_stage": "stt",
            "end_stage": "stt",
            "input": {"sample_rate": 16000, "no_vad": true},
            "pipeline": PIPELINE,
            "device_id": DEVICE,
            "timeout": 30.0
        })
    );
    let frames = answer_stt(&mut session.connection, &run["id"], 3).await;
    assert_eq!(
        within(call).await.unwrap(),
        Ok(Transcript {
            text: "Turn on the kitchen lights.".to_owned()
        })
    );

    assert_eq!(frames.len(), 4);
    assert!(frames.iter().all(|frame| frame[0] == 3));
    assert!(frames.iter().all(|frame| frame.len() <= 16_001));
    assert_eq!(frames.last().unwrap(), &vec![3]);
    let payload: Vec<u8> = frames
        .iter()
        .flat_map(|frame| frame[1..].to_vec())
        .collect();
    let expected: Vec<u8> = audio
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    assert_eq!(payload, expected);
}

#[tokio::test]
async fn stt_is_retried_once_on_a_new_connection_after_the_transport_fails() {
    let mut session = session().await;
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.run_stt(stt_request(vec![1, -1, 2, -2])).await });

    let first = session.connection.request().await;
    assert_eq!(first["type"], "assist_pipeline/run");
    session.connection.result(&first["id"], Value::Null);
    session.connection.close();

    let mut second = session.ha.connection().await;
    let retry = second.request().await;
    assert_eq!(retry["type"], "assist_pipeline/run");
    assert_eq!(retry["start_stage"], "stt");
    answer_stt(&mut second, &retry["id"], 1).await;
    assert_eq!(
        within(call).await.unwrap(),
        Ok(Transcript {
            text: "Turn on the kitchen lights.".to_owned()
        })
    );
    second.assert_quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn a_second_transport_failure_ends_stt_as_lost() {
    let mut session = session().await;
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.run_stt(stt_request(vec![5; 10])).await });
    let first = session.connection.request().await;
    session.connection.close();
    assert_eq!(first["type"], "assist_pipeline/run");
    let mut second = session.ha.connection().await;
    second.request().await;
    second.close();
    assert_eq!(within(call).await.unwrap(), Err(HaError::ConnectionLost));
    let mut third = session.ha.connection().await;
    third.assert_quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn ha_errors_in_stt_are_not_retried() {
    let mut session = session().await;

    // An error event after the audio.
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.run_stt(stt_request(vec![0; 100])).await });
    let run = session.connection.request().await;
    session.connection.result(&run["id"], Value::Null);
    session
        .connection
        .pipeline_event(&run["id"], "run-start", run_start(None, Some(1)));
    assert_eq!(session.connection.binary().await.len(), 201);
    assert_eq!(session.connection.binary().await, vec![1]);
    session.connection.pipeline_event(
        &run["id"],
        "error",
        json!({"code": "stt-no-text-recognized", "message": "No text recognized"}),
    );
    session
        .connection
        .pipeline_event(&run["id"], "run-end", Value::Null);
    assert_eq!(
        within(call).await.unwrap(),
        Err(HaError::Rejected {
            code: "stt-no-text-recognized".to_owned(),
            message: "No text recognized".to_owned()
        })
    );

    // HA refusing to start the run.
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.run_stt(stt_request(vec![0; 10])).await });
    let run = session.connection.request().await;
    session.connection.error(
        &run["id"],
        "pipeline-not-found",
        "Pipeline not found: id=missing",
    );
    assert_eq!(
        within(call).await.unwrap(),
        Err(HaError::Rejected {
            code: "pipeline-not-found".to_owned(),
            message: "Pipeline not found: id=missing".to_owned()
        })
    );
    session
        .connection
        .assert_quiet(Duration::from_millis(100))
        .await;
}

// ── Intent to TTS ────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_intent_run_reports_the_answer_and_its_audio_in_order() {
    let mut session = session().await;
    let base = session.ha.base_url();
    let mut request = intent_request(scope(), "turn on the kitchen lights");
    request.conversation = Some(ConversationId("01K8EARLIER".to_owned()));
    let mut run = session.client.run_intent_tts(request).await.unwrap();
    let sent = session.connection.request().await;
    assert_eq!(
        sent,
        json!({
            "id": sent["id"],
            "type": "assist_pipeline/run",
            "start_stage": "intent",
            "end_stage": "tts",
            "input": {"text": "turn on the kitchen lights"},
            "pipeline": PIPELINE,
            "device_id": DEVICE,
            "conversation_id": "01K8EARLIER",
            "timeout": 30.0
        })
    );
    let id = &sent["id"];
    let c = &session.connection;
    c.result(id, Value::Null);
    c.pipeline_event(
        id,
        "run-start",
        run_start(Some(("/api/tts_proxy/abc.flac", false)), None),
    );
    c.pipeline_event(
        id,
        "intent-start",
        json!({"engine": "conversation.home_assistant"}),
    );
    c.pipeline_event(
        id,
        "intent-end",
        json!({
            "processed_locally": true,
            "intent_output": intent_output("Turned on the kitchen lights.", "01K8EARLIER", false)
        }),
    );
    c.pipeline_event(
        id,
        "tts-start",
        json!({"engine": "tts.piper", "tts_input": "Turned on"}),
    );
    c.pipeline_event(id, "tts-end", tts_end("/api/tts_proxy/abc.flac"));
    c.pipeline_event(id, "run-end", Value::Null);

    assert_eq!(
        all_events(&mut run.events).await,
        vec![
            RunEvent::IntentEnded(IntentEnd {
                speech: "Turned on the kitchen lights.".to_owned(),
                conversation: Some(ConversationId("01K8EARLIER".to_owned())),
                continue_conversation: false,
                response_type: Some("action_done".to_owned()),
                processed_locally: Some(true),
            }),
            RunEvent::TtsReady {
                url: MediaUrl(format!("{base}/api/tts_proxy/abc.flac")),
                streamed: false
            },
            RunEvent::Ended,
        ]
    );
}

#[tokio::test]
async fn a_streamed_answer_releases_its_audio_only_when_ha_starts_streaming() {
    let mut session = session().await;
    let base = session.ha.base_url();

    let mut run = session
        .client
        .run_intent_tts(intent_request(scope(), "tell me a story"))
        .await
        .unwrap();
    let id = session.connection.request().await["id"].clone();
    let c = &session.connection;
    c.result(&id, Value::Null);
    c.pipeline_event(
        &id,
        "run-start",
        run_start(Some(("/api/tts_proxy/abc.flac", true)), None),
    );
    c.pipeline_event(
        &id,
        "intent-progress",
        json!({"chat_log_delta": {"role": "assistant"}}),
    );
    c.pipeline_event(
        &id,
        "intent-progress",
        json!({"chat_log_delta": {"content": "Once upon a time, "}}),
    );
    c.pipeline_event(&id, "intent-progress", json!({"tts_start_streaming": true}));
    c.pipeline_event(
        &id,
        "intent-end",
        json!({
            "processed_locally": false,
            "intent_output": intent_output("Once upon a time, a fox. More?", "01K8STORY", true)
        }),
    );
    c.pipeline_event(&id, "tts-end", tts_end("/api/tts_proxy/abc.flac"));
    c.pipeline_event(&id, "run-end", Value::Null);
    let events = all_events(&mut run.events).await;
    assert_eq!(
        events[0],
        RunEvent::AnswerDelta("Once upon a time, ".to_owned())
    );
    assert_eq!(
        events[1],
        RunEvent::TtsReady {
            url: MediaUrl(format!("{base}/api/tts_proxy/abc.flac")),
            streamed: true
        }
    );
    match &events[2] {
        RunEvent::IntentEnded(end) => {
            assert!(end.continue_conversation);
            assert_eq!(end.processed_locally, Some(false));
        }
        other => panic!("expected intent-end, got {other:?}"),
    }
    assert_eq!(events[3..], [RunEvent::Ended]);

    // HA announces a streamable reply but never streams it (it may replace the reply
    // with its acknowledge sound): the audio comes from tts-end.
    let mut run = session
        .client
        .run_intent_tts(intent_request(scope(), "turn on the lights"))
        .await
        .unwrap();
    let id = session.connection.request().await["id"].clone();
    let c = &session.connection;
    c.result(&id, Value::Null);
    c.pipeline_event(
        &id,
        "run-start",
        run_start(Some(("/api/tts_proxy/ack.flac", true)), None),
    );
    c.pipeline_event(
        &id,
        "intent-end",
        json!({"processed_locally": true,
               "intent_output": intent_output("Turned on the lights.", "01K8ACK", false)}),
    );
    c.pipeline_event(&id, "tts-end", tts_end("/api/tts_proxy/ack2.flac"));
    c.pipeline_event(&id, "run-end", Value::Null);
    let events = all_events(&mut run.events).await;
    assert_eq!(
        events[1],
        RunEvent::TtsReady {
            url: MediaUrl(format!("{base}/api/tts_proxy/ack2.flac")),
            streamed: false
        }
    );
    assert_eq!(events.len(), 3);
}

#[tokio::test]
async fn failed_and_refused_runs_end_with_one_terminal_event() {
    let mut session = session().await;

    let mut run = session
        .client
        .run_intent_tts(intent_request(scope(), "make this fail"))
        .await
        .unwrap();
    let id = session.connection.request().await["id"].clone();
    let c = &session.connection;
    c.result(&id, Value::Null);
    c.pipeline_event(
        &id,
        "run-start",
        run_start(Some(("/api/tts_proxy/x.flac", false)), None),
    );
    c.pipeline_event(
        &id,
        "error",
        json!({"code": "intent-failed", "message": "Unexpected error during intent recognition"}),
    );
    c.pipeline_event(&id, "run-end", Value::Null);
    assert_eq!(
        all_events(&mut run.events).await,
        vec![RunEvent::Failed {
            code: "intent-failed".to_owned(),
            message: "Unexpected error during intent recognition".to_owned()
        }]
    );

    let mut run = session
        .client
        .run_intent_tts(intent_request(scope(), "anything"))
        .await
        .unwrap();
    let id = session.connection.request().await["id"].clone();
    session
        .connection
        .error(&id, "pipeline-not-found", "Pipeline not found: id=missing");
    assert_eq!(
        all_events(&mut run.events).await,
        vec![RunEvent::Rejected {
            code: "pipeline-not-found".to_owned(),
            message: "Pipeline not found: id=missing".to_owned()
        }]
    );

    // An event this controller cannot read ends delivery without a terminal event, which
    // the caller reads as an unknown outcome.
    let mut run = session
        .client
        .run_intent_tts(intent_request(scope(), "anything"))
        .await
        .unwrap();
    let id = session.connection.request().await["id"].clone();
    let c = &session.connection;
    c.result(&id, Value::Null);
    c.pipeline_event(&id, "intent-end", json!({"processed_locally": true}));
    c.pipeline_event(&id, "run-end", Value::Null);
    assert_eq!(all_events(&mut run.events).await, vec![]);
}
#[tokio::test]
async fn a_connection_that_falls_silent_is_noticed_by_the_heartbeat() {
    let mut session = session_with(HaOptions {
        heartbeat: Duration::from_millis(100),
        ..options()
    })
    .await;
    let client = session.client.clone();
    let pending = tokio::spawn(async move {
        client
            .recognize(recognize_request(scope(), &["pending"]))
            .await
    });
    session.connection.request().await;
    session.connection.freeze();
    // Well before the 2 s command deadline.
    assert_eq!(within(pending).await.unwrap(), Err(HaError::ConnectionLost));
    wait_for_generation(&session.client, 2).await;
}

#[tokio::test]
async fn events_arriving_after_cancellation_are_not_delivered() {
    let mut session = session().await;
    let turn = scope();
    let mut run = session
        .client
        .run_intent_tts(intent_request(turn.clone(), "tell me a story"))
        .await
        .unwrap();
    let id = session.connection.request().await["id"].clone();
    session.connection.result(&id, Value::Null);
    session.connection.pipeline_event(
        &id,
        "intent-progress",
        json!({"chat_log_delta": {"content": "Once"}}),
    );
    assert_eq!(
        next_event(&mut run.events).await,
        Some(RunEvent::AnswerDelta("Once".to_owned()))
    );

    turn.cancel();
    let c = &session.connection;
    c.pipeline_event(
        &id,
        "intent-progress",
        json!({"chat_log_delta": {"content": " upon"}}),
    );
    c.pipeline_event(
        &id,
        "intent-end",
        json!({"processed_locally": false,
               "intent_output": intent_output("Once upon a time.", "01K8LATE", false)}),
    );
    c.pipeline_event(&id, "run-end", Value::Null);
    barrier(&mut session).await;
    assert_eq!(next_event(&mut run.events).await, None);
}

#[tokio::test]
async fn a_cancelled_scope_sends_nothing() {
    let mut session = session().await;
    let client = &session.client;
    assert_eq!(
        client
            .run_intent_tts(intent_request(cancelled_scope(), "turn on the lights"))
            .await
            .map(|_| ()),
        Err(NotDispatched::Cancelled)
    );
    let mut stt = stt_request(vec![0; 10]);
    stt.scope = cancelled_scope();
    assert_eq!(
        client.run_stt(stt).await,
        Err(HaError::NotDispatched(NotDispatched::Cancelled))
    );
    assert_eq!(
        client
            .recognize(recognize_request(cancelled_scope(), &["what time is it"]))
            .await,
        Err(HaError::NotDispatched(NotDispatched::Cancelled))
    );
    assert_eq!(
        client
            .run_tts(TtsRequest {
                scope: cancelled_scope(),
                target: target(),
                text: "Done.".to_owned()
            })
            .await,
        Err(HaError::NotDispatched(NotDispatched::Cancelled))
    );
    assert_eq!(
        client.pipeline_for(cancelled_scope(), Some(device())).await,
        Err(HaError::NotDispatched(NotDispatched::Cancelled))
    );
    assert_eq!(
        client.timer_status(cancelled_scope(), Some(device())).await,
        Err(HaError::NotDispatched(NotDispatched::Cancelled))
    );
    assert_eq!(
        client
            .call_intent(DirectIntentRequest {
                scope: cancelled_scope(),
                ha_device: Some(device()),
                intent: DirectIntent::CancelAllTimers,
            })
            .await,
        EffectOutcome::NotDispatched(NotDispatched::Cancelled)
    );
    session
        .connection
        .assert_quiet(Duration::from_millis(100))
        .await;
    assert!(session.ha.no_rest_call_waiting());
}

// ── TTS, recognition and the satellite's pipeline ────────────────────────────

#[tokio::test]
async fn a_tts_run_resolves_to_the_absolute_audio_url() {
    let mut session = session().await;
    let base = session.ha.base_url();
    let client = session.client.clone();
    let call = tokio::spawn(async move {
        client
            .run_tts(TtsRequest {
                scope: scope(),
                target: target(),
                text: "Alarm set for 7 AM.".to_owned(),
            })
            .await
    });
    let sent = session.connection.request().await;
    assert_eq!(
        sent,
        json!({
            "id": sent["id"],
            "type": "assist_pipeline/run",
            "start_stage": "tts",
            "end_stage": "tts",
            "input": {"text": "Alarm set for 7 AM."},
            "pipeline": PIPELINE,
            "device_id": DEVICE,
            "timeout": 10.0
        })
    );
    let id = &sent["id"];
    let c = &session.connection;
    c.result(id, Value::Null);
    c.pipeline_event(
        id,
        "run-start",
        run_start(Some(("/api/tts_proxy/abc.flac", false)), None),
    );
    c.pipeline_event(
        id,
        "tts-start",
        json!({"engine": "tts.piper", "tts_input": "Alarm"}),
    );
    c.pipeline_event(id, "tts-end", tts_end("/api/tts_proxy/abc.flac"));
    c.pipeline_event(id, "run-end", Value::Null);
    assert_eq!(
        within(call).await.unwrap(),
        Ok(MediaUrl(format!("{base}/api/tts_proxy/abc.flac")))
    );
}

#[tokio::test]
async fn recognition_reads_triggers_intents_and_unrecognized_sentences() {
    let mut session = session().await;
    let client = session.client.clone();
    let mut request = recognize_request(
        scope(),
        &[
            "what time is it",
            "set a timer for five",
            "dim the living room",
            "zqxj",
        ],
    );
    request.language = Some("en".to_owned());
    let call = tokio::spawn(async move { client.recognize(request).await });
    let sent = session.connection.request().await;
    assert_eq!(
        sent,
        json!({
            "id": sent["id"],
            "type": "conversation/agent/homeassistant/debug",
            "sentences": ["what time is it", "set a timer for five", "dim the living room", "zqxj"],
            "device_id": DEVICE,
            "language": "en"
        })
    );
    // The shapes of `DefaultAgent.async_debug_recognize`.
    session.connection.result(
        &sent["id"],
        json!({"results": [
            {"match": true, "source": "trigger", "sentence_template": "what time is it"},
            {
                "intent": {"name": "HassStartTimer"},
                "slots": {},
                "details": {},
                "targets": {},
                "match": false,
                "sentence_template": "set [a|an] timer for <timer_duration>",
                "unmatched_slots": {"minutes": "five"},
                "source": "custom",
                "file": "en/timers.yaml"
            },
            {
                "intent": {"name": "HassLightSet"},
                "slots": {"area": "Living Room", "brightness": 50},
                "details": {
                    "area": {"name": "area", "value": "Living Room", "text": "living room"},
                    "brightness": {"name": "brightness", "value": 50, "text": "50%"}
                },
                "targets": {"light.sofa": {"matched": true}, "light.arc": {"matched": true}},
                "match": true,
                "sentence_template": "dim [the] {area}",
                "unmatched_slots": {},
                "source": "builtin"
            },
            null
        ]}),
    );
    let results = within(call).await.unwrap().unwrap();
    assert_eq!(
        results,
        vec![
            Some(SentenceRecognition {
                matched: true,
                unfilled_slots: false,
                source: Some("trigger".to_owned()),
                template: "what time is it".to_owned(),
                intent: None,
                details: BTreeMap::new(),
                unmatched: BTreeMap::new(),
                targets: vec![],
            }),
            Some(SentenceRecognition {
                matched: false,
                unfilled_slots: true,
                source: Some("custom".to_owned()),
                template: "set [a|an] timer for <timer_duration>".to_owned(),
                intent: Some("HassStartTimer".to_owned()),
                details: BTreeMap::new(),
                unmatched: BTreeMap::from([("minutes".to_owned(), "five".to_owned())]),
                targets: vec![],
            }),
            Some(SentenceRecognition {
                matched: true,
                unfilled_slots: false,
                source: Some("builtin".to_owned()),
                template: "dim [the] {area}".to_owned(),
                intent: Some("HassLightSet".to_owned()),
                details: BTreeMap::from([
                    (
                        "area".to_owned(),
                        SlotValue {
                            value: "Living Room".to_owned(),
                            text: "living room".to_owned()
                        }
                    ),
                    (
                        "brightness".to_owned(),
                        SlotValue {
                            value: "50".to_owned(),
                            text: "50%".to_owned()
                        }
                    ),
                ]),
                unmatched: BTreeMap::new(),
                targets: vec!["light.arc".to_owned(), "light.sofa".to_owned()],
            }),
            None,
        ]
    );

    // Without a language HA uses its own; a result count that does not match is refused.
    let client = session.client.clone();
    let call = tokio::spawn(async move {
        client
            .recognize(recognize_request(scope(), &["one", "two"]))
            .await
    });
    let sent = session.connection.request().await;
    assert!(sent.get("language").is_none());
    session
        .connection
        .result(&sent["id"], json!({"results": [null]}));
    assert!(matches!(
        within(call).await.unwrap(),
        Err(HaError::BadResponse(_))
    ));
}

/// `config/entity_registry/list` entries (`RegistryEntry.as_partial_dict`) of an ESPHome
/// satellite with two pipeline selects and its VAD select.
fn satellite_registry() -> Value {
    let entry = |entity: &str, unique: &str| {
        json!({
            "area_id": null, "categories": {}, "config_entry_id": "01K8ENTRY",
            "config_subentry_id": null, "created_at": 1_761_000_000.0, "device_id": DEVICE,
            "disabled_by": null, "entity_category": "config", "entity_id": entity,
            "has_entity_name": true, "hidden_by": null, "icon": null, "id": "reg",
            "labels": [], "modified_at": 1_761_000_000.0, "name": null, "options": {},
            "original_name": "Assistant", "platform": "esphome", "translation_key": "pipeline",
            "unique_id": unique
        })
    };
    json!([
        entry("select.office_assistant_2", "09:0F:10:72:84:26-pipeline_2"),
        entry(
            "select.office_finished_speaking_detection",
            "09:0F:10:72:84:26-vad_sensitivity"
        ),
        entry("select.office_assistant", "09:0F:10:72:84:26-pipeline"),
    ])
}

fn pipeline_list() -> Value {
    json!({
        "pipelines": [
            {"conversation_engine": "conversation.home_assistant", "conversation_language": "en",
             "id": "01hmd0v9ffh9ze4dbrz9mw0gks", "language": "en", "name": "Home Assistant",
             "stt_engine": null, "stt_language": null, "tts_engine": null, "tts_language": null,
             "tts_voice": null, "wake_word_entity": null, "wake_word_id": null,
             "prefer_local_intents": false},
            {"conversation_engine": "conversation.claude", "conversation_language": "en",
             "id": PIPELINE, "language": "en", "name": "Cloud",
             "stt_engine": "stt.faster_whisper", "stt_language": "en",
             "tts_engine": "tts.piper", "tts_language": "en", "tts_voice": "amy",
             "wake_word_entity": null, "wake_word_id": null, "prefer_local_intents": true}
        ],
        "preferred_pipeline": "01hmd0v9ffh9ze4dbrz9mw0gks"
    })
}

#[tokio::test]
async fn the_satellites_select_chooses_the_pipeline_by_name() {
    let mut session = session().await;

    for (state_status, state, expected) in [
        (200, Some("Cloud"), PIPELINE),
        (200, Some("preferred"), "01hmd0v9ffh9ze4dbrz9mw0gks"),
        (404, None, "01hmd0v9ffh9ze4dbrz9mw0gks"),
    ] {
        let client = session.client.clone();
        let call = tokio::spawn(async move { client.pipeline_for(scope(), Some(device())).await });
        let registry = session.connection.request().await;
        assert_eq!(registry["type"], "config/entity_registry/list");
        session
            .connection
            .result(&registry["id"], satellite_registry());

        let rest = session.ha.rest_call().await;
        assert_eq!(rest.method, "GET");
        assert_eq!(rest.path, "/api/states/select.office_assistant");
        assert_eq!(rest.authorization.as_deref(), Some("Bearer test-token"));
        match state {
            Some(state) => rest.respond(
                state_status,
                json!({
                    "entity_id": "select.office_assistant", "state": state,
                    "attributes": {"options": ["preferred", "Cloud", "Home Assistant"]},
                    "last_changed": "2026-10-28T09:00:00+00:00",
                    "context": {"id": "01K8", "parent_id": null, "user_id": null}
                }),
            ),
            None => rest.respond(state_status, json!({"message": "Entity not found."})),
        }

        let list = session.connection.request().await;
        assert_eq!(list["type"], "assist_pipeline/pipeline/list");
        session.connection.result(&list["id"], pipeline_list());
        assert_eq!(
            within(call).await.unwrap(),
            Ok(PipelineId(expected.to_owned()))
        );
    }

    // A satellite HA does not know gets the preferred pipeline.
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.pipeline_for(scope(), None).await });
    let list = session.connection.request().await;
    assert_eq!(list["type"], "assist_pipeline/pipeline/list");
    session.connection.result(&list["id"], pipeline_list());
    assert_eq!(
        within(call).await.unwrap(),
        Ok(PipelineId("01hmd0v9ffh9ze4dbrz9mw0gks".to_owned()))
    );
}

// ── Direct intents ───────────────────────────────────────────────────────────

/// `IntentResponse.as_dict` of an intent that ran.
fn action_done() -> Value {
    json!({"speech": {}, "card": {}, "language": "en", "response_type": "action_done",
           "data": {"success": [], "failed": []}})
}

#[tokio::test]
async fn timer_status_reads_every_timer_ha_reports() {
    let mut session = session().await;
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.timer_status(scope(), Some(device())).await });
    let rest = session.ha.rest_call().await;
    assert_eq!(
        (rest.method.as_str(), rest.path.as_str()),
        ("POST", "/api/intent/handle")
    );
    assert_eq!(rest.authorization.as_deref(), Some("Bearer test-token"));
    assert_eq!(
        rest.body,
        json!({"name": "HassTimerStatus", "data": {}, "device_id": DEVICE})
    );
    let timer = |id: &str, name: &str, device: &str, minutes: u32, active: bool| {
        json!({
            "id": id, "name": name, "device_id": device, "language": "en",
            "start_hours": 0, "start_minutes": minutes, "start_seconds": 0,
            "is_active": active, "hours_left": 0, "minutes_left": minutes, "seconds_left": 0,
            "rounded_hours_left": 0, "rounded_minutes_left": minutes,
            "rounded_seconds_left": 0, "total_seconds_left": minutes * 60
        })
    };
    let mut response = action_done();
    response["speech_slots"] = json!({"timers": [
        timer("01K8T1", "pasta", DEVICE, 10, true),
        timer("01K8T2", "", "", 5, false),
    ]});
    rest.respond(200, response);
    assert_eq!(
        within(call).await.unwrap(),
        Ok(TimerStatus {
            response_type: "action_done".to_owned(),
            timers: vec![
                HaTimer {
                    id: "01K8T1".to_owned(),
                    name: Some("pasta".to_owned()),
                    ha_device: Some(device()),
                    start_hours: 0,
                    start_minutes: 10,
                    start_seconds: 0,
                    is_active: true,
                },
                HaTimer {
                    id: "01K8T2".to_owned(),
                    name: None,
                    ha_device: None,
                    start_hours: 0,
                    start_minutes: 5,
                    start_seconds: 0,
                    is_active: false,
                },
            ],
        })
    );
}

fn cancel_pasta(scope: WorkScope) -> DirectIntentRequest {
    DirectIntentRequest {
        scope,
        ha_device: Some(device()),
        intent: DirectIntent::CancelTimer(TimerSelector {
            name: Some("pasta".to_owned()),
            start_hours: None,
            start_minutes: Some(10),
            start_seconds: None,
        }),
    }
}

async fn call_intent_answered(
    session: &mut Session,
    request: DirectIntentRequest,
    answer: impl FnOnce(fake_ha::RestCall),
) -> EffectOutcome<()> {
    let client = session.client.clone();
    let call = tokio::spawn(async move { client.call_intent(request).await });
    answer(session.ha.rest_call().await);
    within(call).await.unwrap()
}

#[tokio::test]
async fn a_direct_intent_distinguishes_applied_refused_and_unknown() {
    let mut session = session_with(HaOptions {
        command_timeout: Duration::from_millis(300),
        ..options()
    })
    .await;

    let applied = call_intent_answered(&mut session, cancel_pasta(scope()), |rest| {
        assert_eq!(
            rest.body,
            json!({"name": "HassCancelTimer",
                   "data": {"name": "pasta", "start_minutes": 10},
                   "device_id": DEVICE})
        );
        rest.respond(200, action_done());
    })
    .await;
    assert_eq!(applied, EffectOutcome::Applied(()));

    let all = DirectIntentRequest {
        scope: scope(),
        ha_device: Some(device()),
        intent: DirectIntent::CancelAllTimers,
    };
    let applied = call_intent_answered(&mut session, all, |rest| {
        assert_eq!(
            rest.body,
            json!({"name": "HassCancelAllTimers", "data": {}, "device_id": DEVICE})
        );
        rest.respond(200, action_done());
    })
    .await;
    assert_eq!(applied, EffectOutcome::Applied(()));

    // HA answers a cancel that matches no timer with HTTP 200 and an error response.
    let refused = call_intent_answered(&mut session, cancel_pasta(scope()), |rest| {
        rest.respond(
            200,
            json!({"speech": {"plain": {"speech": "Timer not found", "extra_data": null}},
                   "card": {}, "language": "en", "response_type": "error",
                   "data": {"code": "failed_to_handle"}}),
        );
    })
    .await;
    assert_eq!(
        refused,
        EffectOutcome::Rejected {
            code: "failed_to_handle".to_owned(),
            message: "Timer not found".to_owned()
        }
    );

    let crashed = call_intent_answered(&mut session, cancel_pasta(scope()), |rest| {
        rest.respond_text(
            500,
            "500 Internal Server Error\n\nServer got itself in trouble",
        );
    })
    .await;
    assert_eq!(crashed, EffectOutcome::Unknown(UnknownCause::Inconclusive));

    let hung_up = call_intent_answered(&mut session, cancel_pasta(scope()), drop).await;
    assert_eq!(
        hung_up,
        EffectOutcome::Unknown(UnknownCause::ConnectionLost)
    );

    let mut held = None;
    let silent = call_intent_answered(&mut session, cancel_pasta(scope()), |rest| {
        held = Some(rest);
    })
    .await;
    assert_eq!(silent, EffectOutcome::Unknown(UnknownCause::Timeout));
    drop(held);

    let turn = scope();
    let client = session.client.clone();
    let request = cancel_pasta(turn.clone());
    let call = tokio::spawn(async move { client.call_intent(request).await });
    let _held = session.ha.rest_call().await;
    turn.cancel();
    assert_eq!(
        within(call).await.unwrap(),
        EffectOutcome::Unknown(UnknownCause::Withdrawn)
    );
}

#[tokio::test]
async fn a_direct_intent_to_an_unreachable_ha_was_never_sent() {
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = unused.local_addr().unwrap();
    drop(unused);
    let endpoint = HaEndpoint::direct(&format!("http://{address}"), TOKEN).unwrap();
    let (client, _task) = HaClient::start(endpoint, options()).unwrap();
    assert_eq!(
        client.call_intent(cancel_pasta(scope())).await,
        EffectOutcome::NotDispatched(NotDispatched::Unavailable)
    );
    assert_eq!(
        client.timer_status(scope(), None).await,
        Err(HaError::NotDispatched(NotDispatched::Unavailable))
    );
    client.stop();
}

#[tokio::test]
async fn stopping_the_session_ends_the_connection() {
    let mut session = session().await;
    session.client.stop();
    assert_eq!(within(session.task).await.unwrap(), SessionEnd::Stopped);
    assert!(matches!(
        session.connection.receive().await,
        Inbound::Closed
    ));
}
