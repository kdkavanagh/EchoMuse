use std::f64::consts::PI;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::eq::EqSettings;

const FFMPEG: &str = "/usr/bin/ffmpeg";

/// A 16-bit PCM WAV file of a sine at `hz`, `seconds` long, at `rate` with
/// `channels` identical channels.
fn wav(hz: f64, rate: u32, channels: u16, seconds: f64) -> Vec<u8> {
    let frames = (f64::from(rate) * seconds).round() as u32;
    let data_bytes = frames * u32::from(channels) * 2;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for n in 0..frames {
        let sample =
            (8_000.0 * (2.0 * PI * hz * f64::from(n) / f64::from(rate)).sin()).round() as i16;
        for _ in 0..channels {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
    out
}

/// What the test server answers for a path.
#[derive(Clone)]
enum Reply {
    /// The whole body with its length.
    Whole(Vec<u8>),
    /// A status with an empty body.
    Status(u16),
    /// The first bytes of a body without a length, then nothing more while
    /// the connection stays open.
    Stalled(Vec<u8>),
}

/// Serves `routes` on a local port until the test ends.
async fn serve(routes: Vec<(&'static str, Reply)>) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let reply = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map_or(Reply::Status(404), |(_, reply)| reply.clone());
                match reply {
                    Reply::Whole(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    }
                    Reply::Status(code) => {
                        let head = format!(
                            "HTTP/1.1 {code} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                    }
                    Reply::Stalled(body) => {
                        let head = "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n";
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                        std::future::pending::<()>().await;
                    }
                }
            });
        }
    });
    base
}

/// A plain-HTTP client whose TLS configuration names its crypto provider, as
/// the controller's does, so it does not depend on a process default.
fn http_client() -> reqwest::Client {
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    reqwest::Client::builder()
        .no_proxy()
        .tls_backend_preconfigured(tls)
        .build()
        .unwrap()
}

fn decoder() -> Decoder {
    Decoder::new(Handle::current(), http_client(), PathBuf::from(FFMPEG))
}

fn request(url: Url) -> DecodeRequest {
    DecodeRequest {
        url,
        skip_frames: 0,
        equalizer: None,
    }
}

/// Collects every event of a decode.
async fn collect(
    mut stream: DecodeStream,
) -> (
    Option<Seekability>,
    Vec<i16>,
    Result<DecodeSummary, DecodeError>,
) {
    let mut seekable = None;
    let mut audio = Vec::new();
    loop {
        match stream.recv().await.expect("a finished event") {
            DecodeEvent::Opened { seekable: s } => seekable = Some(s),
            DecodeEvent::Audio(chunk) => {
                assert!(chunk.len() <= DECODE_CHUNK_FRAMES);
                audio.extend(chunk);
            }
            DecodeEvent::Finished(result) => return (seekable, audio, result),
        }
    }
}

/// Upward zero crossings per second of `pcm` at 48 kHz.
fn frequency(pcm: &[i16]) -> f64 {
    let crossings = pcm.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
    crossings as f64 * 48_000.0 / pcm.len() as f64
}

fn gone(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generated_tone_decodes_to_48_khz_mono_of_the_right_length() {
    let base = serve(vec![(
        "/tone.wav",
        Reply::Whole(wav(440.0, 44_100, 2, 1.5)),
    )])
    .await;
    let stream = decoder().start(request(base.join("tone.wav").unwrap()));
    let (seekable, audio, result) = collect(stream).await;
    assert_eq!(seekable, Some(Seekability::Seekable));
    let summary = result.unwrap();
    // 1.5 s of 44.1 kHz stereo is 72,000 frames of 48 kHz mono.
    assert_eq!(audio.len(), 72_000);
    assert_eq!(summary.decoded_frames, 72_000);
    assert_eq!(summary.delivered_frames, 72_000);
    let hz = frequency(&audio[4_800..67_200]);
    assert!((hz - 440.0).abs() < 2.0, "{hz} Hz");
    let peak = audio.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(
        (7_500..=8_500).contains(&peak),
        "the two channels average, peak {peak}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skip_and_equalizer_apply_to_the_delivered_audio() {
    let base = serve(vec![(
        "/tone.wav",
        Reply::Whole(wav(1_000.0, 48_000, 1, 1.0)),
    )])
    .await;
    let settings = EqSettings::new([0, 0, 0, 6, 0, 0, 0, 0], false).unwrap();
    let mut req = request(base.join("tone.wav").unwrap());
    req.skip_frames = 30_000;
    req.equalizer = Some(Equalizer::new(&settings, RENDER_SAMPLE_RATE));
    let (_, audio, result) = collect(decoder().start(req)).await;
    let summary = result.unwrap();
    assert_eq!(summary.decoded_frames, 48_000);
    assert_eq!(summary.delivered_frames, 18_000);
    assert_eq!(audio.len(), 18_000);
    // +6 dB at 1 kHz roughly doubles the 8,000 peak.
    let peak = audio[4_800..]
        .iter()
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap();
    assert!((15_500..=16_500).contains(&peak), "peak {peak}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_errors_and_undecodable_data_are_typed() {
    let garbage: Vec<u8> = (0..20_000u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let base = serve(vec![("/garbage", Reply::Whole(garbage))]).await;

    let (seekable, _, result) =
        collect(decoder().start(request(base.join("missing").unwrap()))).await;
    assert_eq!(seekable, None);
    assert_eq!(result, Err(DecodeError::Fetch(FetchError::Status(404))));

    let (_, audio, result) = collect(decoder().start(request(base.join("garbage").unwrap()))).await;
    assert!(audio.is_empty());
    assert!(
        matches!(result, Err(DecodeError::Ffmpeg { exit_code: Some(code), .. }) if code != 0),
        "{result:?}"
    );

    let refused = Url::parse("http://127.0.0.1:1/x.wav").unwrap();
    let (_, _, result) = collect(decoder().start(request(refused))).await;
    assert!(
        matches!(result, Err(DecodeError::Fetch(FetchError::Request(_)))),
        "{result:?}"
    );

    let ftp = Url::parse("ftp://example.invalid/x.wav").unwrap();
    let (_, _, result) = collect(decoder().start(request(ftp))).await;
    assert_eq!(
        result,
        Err(DecodeError::Fetch(FetchError::Scheme("ftp".into())))
    );

    let missing_ffmpeg = Decoder::new(
        Handle::current(),
        http_client(),
        PathBuf::from("/nonexistent/ffmpeg"),
    );
    let tone = serve(vec![("/t.wav", Reply::Whole(wav(440.0, 48_000, 1, 0.1)))]).await;
    let (_, _, result) = collect(missing_ffmpeg.start(request(tone.join("t.wav").unwrap()))).await;
    assert!(matches!(result, Err(DecodeError::Spawn(_))), "{result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_stream_plays_before_it_ends_and_cancelling_reaps_ffmpeg() {
    let tone = wav(440.0, 48_000, 1, 10.0);
    // Only the first two seconds arrive; the connection then stays open.
    let base = serve(vec![(
        "/live.wav",
        Reply::Stalled(tone[..44 + 2 * 96_000].to_vec()),
    )])
    .await;
    let mut stream = decoder().start(request(base.join("live.wav").unwrap()));
    assert_eq!(
        stream.recv().await,
        Some(DecodeEvent::Opened {
            seekable: Seekability::Live
        })
    );
    let mut received = 0;
    while received < 48_000 {
        // The stream never ends, so audio must arrive while it is open.
        let event = tokio::time::timeout(Duration::from_secs(10), stream.recv())
            .await
            .expect("audio while the stream is still open");
        match event {
            Some(DecodeEvent::Audio(chunk)) => received += chunk.len(),
            other => panic!("expected audio, got {other:?}"),
        }
    }
    let pid = stream.ffmpeg_pid().expect("ffmpeg started");
    assert!(!gone(pid), "ffmpeg is still decoding the open stream");

    stream.cancel();
    assert_eq!(stream.stopped().await, DecodeEnd::Cancelled);
    assert!(gone(pid), "ffmpeg {pid} was killed and reaped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_stream_reaps_ffmpeg_too() {
    let tone = wav(440.0, 48_000, 1, 10.0);
    let base = serve(vec![(
        "/live.wav",
        Reply::Stalled(tone[..44 + 96_000].to_vec()),
    )])
    .await;
    let mut stream = decoder().start(request(base.join("live.wav").unwrap()));
    let first_audio =
        async { while !matches!(stream.recv().await, Some(DecodeEvent::Audio(_))) {} };
    tokio::time::timeout(Duration::from_secs(10), first_audio)
        .await
        .expect("audio while the stream is still open");
    let pid = stream.ffmpeg_pid().expect("ffmpeg started");
    drop(stream);
    for _ in 0..200 {
        if gone(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("ffmpeg {pid} still exists two seconds after the stream was dropped");
}

#[test]
fn relative_media_urls_resolve_against_home_assistant() {
    let base = Url::parse("http://homeassistant.local:8123/").unwrap();
    assert_eq!(
        resolve_media_url(&base, "/api/tts_proxy/abc.mp3")
            .unwrap()
            .as_str(),
        "http://homeassistant.local:8123/api/tts_proxy/abc.mp3"
    );
    assert_eq!(
        resolve_media_url(&base, "https://cdn.example/x.flac")
            .unwrap()
            .as_str(),
        "https://cdn.example/x.flac"
    );
    assert_eq!(
        resolve_media_url(&base, "media-source://x"),
        Err(FetchError::Scheme("media-source".into()))
    );
}
