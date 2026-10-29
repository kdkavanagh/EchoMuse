//! A simulated Wyoming speech-to-text server for tests: it answers `describe` with the
//! `info` it was given and each transcription with the next scripted transcript, after an
//! optional delay, and keeps what every transcription request sent.

use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::BufReader,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use crate::{Event, read_event, write_event};

/// One transcription a client asked for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heard {
    /// `transcribe`'s `name`.
    pub model: Option<String>,
    /// `transcribe`'s `language`.
    pub language: Option<String>,
    /// The audio format `audio-start` named: rate, width, channels.
    pub format: Option<(u64, u64, u64)>,
    /// Every sample of every `audio-chunk`.
    pub pcm: Vec<i16>,
}

/// How the server answers.
#[derive(Clone, Debug)]
pub struct Script {
    /// `info`'s data.
    pub info: Value,
    /// The transcripts, one per transcription, in order; once used up, the server closes
    /// the connection without answering.
    pub transcripts: VecDeque<String>,
    /// How long each answer waits.
    pub delay: Duration,
}

impl Script {
    /// A server offering one program, faster-whisper 2.5.0, with `models`, that answers
    /// every transcription with `transcripts` in turn.
    pub fn whisper(models: &[&str], transcripts: &[&str]) -> Self {
        Self {
            info: json!({
                "asr": [{
                    "name": "faster-whisper",
                    "version": "2.5.0",
                    "description": "Faster Whisper transcription with CTranslate2",
                    "installed": true,
                    "attribution": {"name": "Guillaume Klein", "url": "https://github.com/guillaumekln/faster-whisper/"},
                    "supports_transcript_streaming": false,
                    "models": models.iter().map(|m| json!({
                        "name": m,
                        "languages": ["en"],
                        "attribution": {"name": "Systran", "url": "https://huggingface.co/Systran"},
                        "installed": true,
                        "description": m,
                        "version": "1.0",
                    })).collect::<Vec<_>>(),
                }],
                "tts": [],
            }),
            transcripts: transcripts.iter().map(|t| (*t).to_owned()).collect(),
            delay: Duration::ZERO,
        }
    }
}

struct State {
    script: Script,
    heard: Vec<Heard>,
    connections: usize,
}

/// A running simulated server on a loopback port. It stops when dropped.
pub struct SimServer {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Drop for SimServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SimServer {
    /// Starts a server with `script` on a free loopback port.
    pub async fn start(script: Script) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State {
            script,
            heard: Vec::new(),
            connections: 0,
        }));
        let serving = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                serving.lock().expect("sim lock").connections += 1;
                tokio::spawn(serve(stream, Arc::clone(&serving)));
            }
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    /// Where it listens.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Every transcription received so far.
    pub fn heard(&self) -> Vec<Heard> {
        self.state.lock().expect("sim lock").heard.clone()
    }

    /// How many connections were made.
    pub fn connections(&self) -> usize {
        self.state.lock().expect("sim lock").connections
    }
}

fn text(event: &Event, key: &str) -> Option<String> {
    event
        .data
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

async fn serve(stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut stream = BufReader::new(stream);
    let mut heard: Option<Heard> = None;
    while let Ok(Some(event)) = read_event(&mut stream).await {
        match event.kind.as_str() {
            "describe" => {
                let (info, delay) = {
                    let state = state.lock().expect("sim lock");
                    (state.script.info.clone(), state.script.delay)
                };
                tokio::time::sleep(delay).await;
                if write_event(stream.get_mut(), &Event::new("info").with_data(info))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            "transcribe" => {
                heard = Some(Heard {
                    model: text(&event, "name"),
                    language: text(&event, "language"),
                    ..Heard::default()
                });
            }
            "audio-start" => {
                if let Some(heard) = &mut heard {
                    let field = |key: &str| event.data.get(key).and_then(Value::as_u64);
                    heard.format = field("rate")
                        .zip(field("width"))
                        .zip(field("channels"))
                        .map(|((rate, width), channels)| (rate, width, channels));
                }
            }
            "audio-chunk" => {
                if let Some(heard) = &mut heard {
                    heard.pcm.extend(
                        event
                            .payload
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|b| i16::from_le_bytes(*b)),
                    );
                }
            }
            "audio-stop" => {
                let Some(done) = heard.take() else { continue };
                let (answer, delay) = {
                    let mut state = state.lock().expect("sim lock");
                    state.heard.push(done);
                    (state.script.transcripts.pop_front(), state.script.delay)
                };
                tokio::time::sleep(delay).await;
                let Some(answer) = answer else { return };
                // A streaming server sends partial chunks first; a client waits for the
                // final transcript.
                let partial = Event::new("transcript-chunk").with_data(json!({"text": "…"}));
                let last = Event::new("transcript").with_data(json!({"text": answer}));
                if write_event(stream.get_mut(), &partial).await.is_err()
                    || write_event(stream.get_mut(), &last).await.is_err()
                {
                    return;
                }
            }
            _ => {}
        }
    }
}
