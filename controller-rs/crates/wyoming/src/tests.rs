use std::{io::Cursor, time::Duration};

use serde_json::json;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::TcpListener,
};

use super::*;
use crate::sim::{Script, SimServer};

const DEADLINE: Duration = Duration::from_millis(1_500);

fn address(server: &SimServer) -> ServerAddress {
    ServerAddress::new("127.0.0.1", server.address().port()).unwrap()
}

async fn decode(bytes: &[u8]) -> Result<Option<Event>, WyomingError> {
    read_event(&mut BufReader::new(Cursor::new(bytes.to_vec()))).await
}

#[tokio::test]
async fn events_round_trip_with_data_and_payload() {
    let event = Event::new("audio-chunk")
        .with_data(json!({"rate": 16000, "width": 2, "channels": 1}))
        .with_payload(vec![1, 2, 3, 4]);
    let mut bytes = Vec::new();
    write_event(&mut bytes, &event).await.unwrap();
    let header_end = bytes.iter().position(|&b| b == b'\n').unwrap();
    let header: Value = serde_json::from_slice(&bytes[..header_end]).unwrap();
    assert_eq!(header["type"], "audio-chunk");
    assert_eq!(header["payload_length"], 4);
    assert_eq!(&bytes[header_end + 1..], &[1, 2, 3, 4]);
    assert_eq!(decode(&bytes).await.unwrap(), Some(event));
}

#[tokio::test]
async fn extra_data_is_merged_over_the_header_data() {
    let extra = br#"{"text":"turn on the light","language":"en"}"#;
    let mut bytes = format!(
        "{{\"type\":\"transcript\",\"data\":{{\"text\":\"old\",\"keep\":1}},\"data_length\":{}}}\n",
        extra.len()
    )
    .into_bytes();
    bytes.extend_from_slice(extra);
    let event = decode(&bytes).await.unwrap().unwrap();
    assert_eq!(event.kind, "transcript");
    assert_eq!(event.data["text"], "turn on the light");
    assert_eq!(event.data["keep"], 1);
    assert!(event.payload.is_empty());
}

#[tokio::test]
async fn malformed_and_cut_events_are_errors() {
    assert!(decode(b"").await.unwrap().is_none());
    assert!(matches!(
        decode(b"not json\n").await,
        Err(WyomingError::Protocol(_))
    ));
    assert!(matches!(
        decode(b"{\"data\":{}}\n").await,
        Err(WyomingError::Protocol(_))
    ));
    assert!(matches!(
        decode(b"{\"type\":\"describe\"").await,
        Err(WyomingError::Closed)
    ));
    assert!(matches!(
        decode(b"{\"type\":\"audio-chunk\",\"payload_length\":10}\nabc").await,
        Err(WyomingError::Closed)
    ));
    let huge = format!(
        "{{\"type\":\"audio-chunk\",\"payload_length\":{}}}\n",
        MAX_PAYLOAD_BYTES + 1
    );
    assert!(matches!(
        decode(huge.as_bytes()).await,
        Err(WyomingError::Protocol(_))
    ));
    let long = vec![b' '; MAX_HEADER_BYTES + 10];
    assert!(matches!(
        decode(&long).await,
        Err(WyomingError::Protocol(_))
    ));
}

#[test]
fn addresses_are_checked() {
    assert!(ServerAddress::new("whisper.local", 10300).is_ok());
    assert_eq!(
        ServerAddress::new(" 10.0.0.2 ", 10300).unwrap().to_string(),
        "10.0.0.2:10300"
    );
    assert_eq!(
        ServerAddress::new("::1", 10300).unwrap().to_string(),
        "[::1]:10300"
    );
    assert!(ServerAddress::new("", 10300).is_err());
    assert!(ServerAddress::new("two words", 10300).is_err());
    assert!(ServerAddress::new("host", 0).is_err());
}

#[tokio::test]
async fn describe_lists_speech_to_text_programs_and_models() {
    let server = SimServer::start(Script::whisper(&["tiny-int8", "base-int8"], &[]))
        .await
        .unwrap();
    let info = describe(&address(&server), DEADLINE).await.unwrap();
    assert_eq!(info.asr.len(), 1);
    let program = &info.asr[0];
    assert_eq!(program.name, "faster-whisper");
    assert_eq!(program.version.as_deref(), Some("2.5.0"));
    assert_eq!(program.installed, Some(true));
    let models: Vec<&str> = program.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(models, ["tiny-int8", "base-int8"]);
    assert_eq!(program.models[0].languages, ["en"]);
}

#[test]
fn info_without_speech_to_text_is_empty_and_bad_shapes_are_refused() {
    let data = |v: Value| v.as_object().unwrap().clone();
    assert_eq!(
        parse_info(&data(json!({"tts": []}))).unwrap(),
        Info::default()
    );
    // Optional fields stay unknown rather than defaulting.
    let info = parse_info(&data(
        json!({"asr": [{"name": "vosk", "models": [{"name": "small"}]}]}),
    ))
    .unwrap();
    assert_eq!(info.asr[0].version, None);
    assert_eq!(info.asr[0].installed, None);
    assert!(info.asr[0].models[0].languages.is_empty());
    assert!(parse_info(&data(json!({"asr": {}}))).is_err());
    assert!(parse_info(&data(json!({"asr": [{"models": []}]}))).is_err());
}

#[tokio::test]
async fn transcribe_sends_the_audio_and_waits_for_the_final_transcript() {
    let server = SimServer::start(Script::whisper(
        &["tiny-int8"],
        &["Turn on the kitchen light."],
    ))
    .await
    .unwrap();
    let pcm: Vec<i16> = (0..5_000).map(|i| (i % 300) as i16 - 150).collect();
    let transcript = transcribe(
        &address(&server),
        TranscribeRequest {
            model: Some("tiny-int8"),
            language: Some("en"),
            pcm: &pcm,
        },
        DEADLINE,
    )
    .await
    .unwrap();
    assert_eq!(transcript.text, "Turn on the kitchen light.");
    let heard = server.heard();
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].model.as_deref(), Some("tiny-int8"));
    assert_eq!(heard[0].language.as_deref(), Some("en"));
    assert_eq!(heard[0].format, Some((16_000, 2, 1)));
    assert_eq!(heard[0].pcm, pcm);
}

#[tokio::test]
async fn empty_model_and_language_ask_for_the_server_defaults() {
    let server = SimServer::start(Script::whisper(&["tiny-int8"], &["hello"]))
        .await
        .unwrap();
    transcribe(
        &address(&server),
        TranscribeRequest {
            model: Some(""),
            language: None,
            pcm: &[0; 100],
        },
        DEADLINE,
    )
    .await
    .unwrap();
    let heard = server.heard();
    assert_eq!(heard[0].model, None);
    assert_eq!(heard[0].language, None);
}

#[tokio::test(start_paused = true)]
async fn a_slow_server_times_out_at_the_deadline() {
    let mut script = Script::whisper(&["tiny-int8"], &["late"]);
    script.delay = Duration::from_secs(5);
    let server = SimServer::start(script).await.unwrap();
    let outcome = transcribe(
        &address(&server),
        TranscribeRequest {
            model: None,
            language: None,
            pcm: &[0; 100],
        },
        DEADLINE,
    )
    .await;
    match outcome {
        Err(WyomingError::TimedOut(after)) => assert_eq!(after, DEADLINE),
        other => panic!("expected a timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn a_server_that_hangs_up_is_reported_as_closed() {
    let server = SimServer::start(Script::whisper(&["tiny-int8"], &[]))
        .await
        .unwrap();
    let outcome = transcribe(
        &address(&server),
        TranscribeRequest {
            model: None,
            language: None,
            pcm: &[0; 100],
        },
        DEADLINE,
    )
    .await;
    assert!(matches!(outcome, Err(WyomingError::Closed)), "{outcome:?}");
}

#[tokio::test]
async fn a_server_speaking_something_else_is_a_protocol_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await
            .unwrap();
    });
    let outcome = describe(&ServerAddress::new("127.0.0.1", port).unwrap(), DEADLINE).await;
    assert!(
        matches!(outcome, Err(WyomingError::Protocol(_))),
        "{outcome:?}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn nothing_listening_is_a_connect_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let outcome = describe(&ServerAddress::new("127.0.0.1", port).unwrap(), DEADLINE).await;
    assert!(
        matches!(outcome, Err(WyomingError::Connect(_))),
        "{outcome:?}"
    );
}
