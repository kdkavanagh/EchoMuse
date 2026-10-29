//! A client for the Wyoming protocol (rhasspy/wyoming), the TCP protocol Home
//! Assistant's own speech services speak: one JSON header line per event, then the
//! event's extra data and binary payload, if any.
//!
//! ```text
//! { "type": "...", "data": { ... }, "data_length": ..., "payload_length": ... }\n
//! <data_length bytes of JSON, merged over "data" (optional)>
//! <payload_length bytes, usually PCM audio (optional)>
//! ```
//!
//! The controller uses two exchanges, each on a fresh connection:
//!
//! - [`describe`]: `describe` → `info`, to list the speech-to-text programs a server
//!   offers and their models (the dashboard's "Check" beside `pauseAsr`).
//! - [`transcribe`]: `transcribe {name?, language?}`, `audio-start`, `audio-chunk`s of
//!   16 kHz mono 16-bit PCM, `audio-stop` → `transcript {text}`, for the remote pause
//!   re-decode (spec §4.7).
//!
//! Both take a deadline that covers connecting, sending and the answer; a server that
//! has not answered by then is reported as [`WyomingError::TimedOut`] and the connection
//! is dropped. Events a request does not wait for (`transcript-start`,
//! `transcript-chunk`, anything unknown) are skipped, as the protocol asks of every peer.
//! Every read is bounded ([`MAX_HEADER_BYTES`], [`MAX_DATA_BYTES`],
//! [`MAX_PAYLOAD_BYTES`]), so a misbehaving server cannot make the controller buffer
//! without limit.
//!
//! [`read_event`] and [`write_event`] are public so a test can stand up a simulated
//! server that speaks the same framing.

use std::{fmt, io, time::Duration};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
};

/// The longest header line read, newline included.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// The most extra data one event may carry.
pub const MAX_DATA_BYTES: usize = 1024 * 1024;
/// The largest payload one event may carry.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// The only audio format sent: the capture rate of every device.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per `audio-chunk` sent (64 ms).
pub const CHUNK_SAMPLES: usize = 1_024;

/// Why an exchange with a Wyoming server failed. The text is a sentence the dashboard
/// can show.
#[derive(Debug)]
pub enum WyomingError {
    /// The host or port cannot name a server.
    InvalidAddress(String),
    /// No connection could be made.
    Connect(io::Error),
    /// The server did not answer within the deadline.
    TimedOut(Duration),
    /// The connection failed after it was made.
    Io(io::Error),
    /// The server closed the connection before it answered.
    Closed,
    /// The server sent something that is not a Wyoming event, or not the answer asked
    /// for; what.
    Protocol(String),
}

impl fmt::Display for WyomingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddress(why) => write!(f, "{why}"),
            Self::Connect(error) => write!(f, "could not connect: {error}"),
            Self::TimedOut(after) => {
                write!(f, "no answer within {:.1} s", after.as_secs_f64())
            }
            Self::Io(error) => write!(f, "the connection failed: {error}"),
            Self::Closed => f.write_str("the server closed the connection without answering"),
            Self::Protocol(what) => write!(f, "the server does not speak Wyoming: {what}"),
        }
    }
}

impl std::error::Error for WyomingError {}

/// Where a Wyoming server listens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerAddress {
    host: String,
    port: u16,
}

impl ServerAddress {
    /// A host name or address and a port. The host must be non-empty, at most 253
    /// characters, with no spaces or control characters; the port must not be 0.
    pub fn new(host: &str, port: u16) -> Result<Self, WyomingError> {
        let host = host.trim();
        if host.is_empty() {
            return Err(WyomingError::InvalidAddress("no host was given".to_owned()));
        }
        if host.len() > 253 || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(WyomingError::InvalidAddress(
                "the host is not a host name or address".to_owned(),
            ));
        }
        if port == 0 {
            return Err(WyomingError::InvalidAddress(
                "the port must be 1 to 65535".to_owned(),
            ));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// The host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for ServerAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

// ─── Events ─────────────────────────────────────────────────────────────────────────

/// One Wyoming event: its type, its data (the header's `data` with any extra data merged
/// over it) and its payload.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    /// `type`.
    pub kind: String,
    /// `data`; empty when the event has none.
    pub data: Map<String, Value>,
    /// The binary payload; empty when the event has none.
    pub payload: Vec<u8>,
}

impl Event {
    /// An event of this type with no data and no payload.
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            ..Self::default()
        }
    }

    /// This event with `data`. Anything but a JSON object leaves the data empty.
    pub fn with_data(mut self, data: Value) -> Self {
        if let Value::Object(map) = data {
            self.data = map;
        }
        self
    }

    /// This event with `payload`.
    pub fn with_payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = payload;
        self
    }
}

fn protocol(what: impl Into<String>) -> WyomingError {
    WyomingError::Protocol(what.into())
}

/// The length field `name` of a header, if present.
fn length(header: &Map<String, Value>, name: &str, max: usize) -> Result<usize, WyomingError> {
    match header.get(name) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => {
            let n = value
                .as_u64()
                .ok_or_else(|| protocol(format!("{name} is not a byte count")))?;
            let n = usize::try_from(n)
                .ok()
                .filter(|&n| n <= max)
                .ok_or_else(|| protocol(format!("{name} {n} is more than {max} bytes")))?;
            Ok(n)
        }
    }
}

/// Reads the next event. `None` when the peer closed the connection between events; a
/// close inside an event is an error.
pub async fn read_event<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Event>, WyomingError> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_HEADER_BYTES as u64)
        .read_until(b'\n', &mut line)
        .await
        .map_err(WyomingError::Io)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if read >= MAX_HEADER_BYTES {
            protocol(format!(
                "a header line is longer than {MAX_HEADER_BYTES} bytes"
            ))
        } else {
            WyomingError::Closed
        });
    }
    let header: Value = serde_json::from_slice(&line)
        .map_err(|error| protocol(format!("a header is not JSON: {error}")))?;
    let Value::Object(header) = header else {
        return Err(protocol("a header is not a JSON object"));
    };
    let kind = header
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("a header has no type"))?
        .to_owned();
    let mut data = match header.get("data") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(data)) => data.clone(),
        Some(_) => return Err(protocol(format!("the data of {kind} is not an object"))),
    };
    let data_length = length(&header, "data_length", MAX_DATA_BYTES)?;
    let payload_length = length(&header, "payload_length", MAX_PAYLOAD_BYTES)?;
    if data_length > 0 {
        let mut extra = vec![0; data_length];
        read_exact(reader, &mut extra).await?;
        match serde_json::from_slice(&extra) {
            Ok(Value::Object(extra)) => data.extend(extra),
            _ => {
                return Err(protocol(format!(
                    "the extra data of {kind} is not an object"
                )));
            }
        }
    }
    let mut payload = vec![0; payload_length];
    if payload_length > 0 {
        read_exact(reader, &mut payload).await?;
    }
    Ok(Some(Event {
        kind,
        data,
        payload,
    }))
}

async fn read_exact<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    into: &mut [u8],
) -> Result<(), WyomingError> {
    reader
        .read_exact(into)
        .await
        .map(drop)
        .map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => WyomingError::Closed,
            _ => WyomingError::Io(error),
        })
}

/// Writes one event: its header line (with `data` inline), then its payload.
pub async fn write_event<W: AsyncWrite + Unpin>(writer: &mut W, event: &Event) -> io::Result<()> {
    let mut header = Map::new();
    header.insert("type".to_owned(), Value::String(event.kind.clone()));
    if !event.data.is_empty() {
        header.insert("data".to_owned(), Value::Object(event.data.clone()));
    }
    if !event.payload.is_empty() {
        header.insert("payload_length".to_owned(), json!(event.payload.len()));
    }
    let mut bytes = serde_json::to_vec(&Value::Object(header)).map_err(io::Error::other)?;
    bytes.push(b'\n');
    bytes.extend_from_slice(&event.payload);
    writer.write_all(&bytes).await
}

// ─── Describe ───────────────────────────────────────────────────────────────────────

/// One speech-to-text program a server offers, as its `info` lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AsrProgram {
    /// Its name.
    pub name: String,
    /// Its version; `None` when not given.
    pub version: Option<String>,
    /// Its description; `None` when not given.
    pub description: Option<String>,
    /// Whether it is installed; `None` when not given.
    pub installed: Option<bool>,
    /// Its models.
    pub models: Vec<AsrModel>,
}

/// One model of a speech-to-text program.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AsrModel {
    /// Its name, as `transcribe` names it.
    pub name: String,
    /// The languages it supports.
    pub languages: Vec<String>,
    /// Its version; `None` when not given.
    pub version: Option<String>,
    /// Its description; `None` when not given.
    pub description: Option<String>,
    /// Whether it is installed; `None` when not given.
    pub installed: Option<bool>,
}

/// What a server's `info` says about speech to text.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Info {
    /// Its speech-to-text programs; empty when it offers none.
    pub asr: Vec<AsrProgram>,
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn parse_model(value: &Value) -> Result<AsrModel, WyomingError> {
    let object = value
        .as_object()
        .ok_or_else(|| protocol("an ASR model is not an object"))?;
    Ok(AsrModel {
        name: text(object, "name").ok_or_else(|| protocol("an ASR model has no name"))?,
        languages: object
            .get("languages")
            .and_then(Value::as_array)
            .map(|languages| {
                languages
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        version: text(object, "version"),
        description: text(object, "description"),
        installed: object.get("installed").and_then(Value::as_bool),
    })
}

/// Reads the speech-to-text part of an `info` event's data.
pub fn parse_info(data: &Map<String, Value>) -> Result<Info, WyomingError> {
    let programs = match data.get("asr") {
        None | Some(Value::Null) => return Ok(Info::default()),
        Some(Value::Array(programs)) => programs,
        Some(_) => return Err(protocol("info's asr is not a list")),
    };
    let asr = programs
        .iter()
        .map(|program| {
            let object = program
                .as_object()
                .ok_or_else(|| protocol("an ASR program is not an object"))?;
            let models = match object.get("models") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(models)) => {
                    models.iter().map(parse_model).collect::<Result<_, _>>()?
                }
                Some(_) => return Err(protocol("an ASR program's models are not a list")),
            };
            Ok(AsrProgram {
                name: text(object, "name").ok_or_else(|| protocol("an ASR program has no name"))?,
                version: text(object, "version"),
                description: text(object, "description"),
                installed: object.get("installed").and_then(Value::as_bool),
                models,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(Info { asr })
}

async fn within<T>(
    deadline: Duration,
    work: impl Future<Output = Result<T, WyomingError>>,
) -> Result<T, WyomingError> {
    tokio::time::timeout(deadline, work)
        .await
        .unwrap_or(Err(WyomingError::TimedOut(deadline)))
}

async fn connect(server: &ServerAddress) -> Result<BufReader<TcpStream>, WyomingError> {
    let stream = TcpStream::connect((server.host.as_str(), server.port))
        .await
        .map_err(WyomingError::Connect)?;
    // Small events are written whole; there is nothing to gain from waiting to fill
    // packets.
    stream.set_nodelay(true).map_err(WyomingError::Io)?;
    Ok(BufReader::new(stream))
}

/// Reads events until one of type `kind`, skipping every other.
async fn wait_for(stream: &mut BufReader<TcpStream>, kind: &str) -> Result<Event, WyomingError> {
    loop {
        match read_event(stream).await? {
            None => return Err(WyomingError::Closed),
            Some(event) if event.kind == kind => return Ok(event),
            Some(_) => {}
        }
    }
}

/// Asks the server at `server` what it offers, within `deadline`.
pub async fn describe(server: &ServerAddress, deadline: Duration) -> Result<Info, WyomingError> {
    within(deadline, async {
        let mut stream = connect(server).await?;
        write_event(stream.get_mut(), &Event::new("describe"))
            .await
            .map_err(WyomingError::Io)?;
        let info = wait_for(&mut stream, "info").await?;
        parse_info(&info.data)
    })
    .await
}

// ─── Transcribe ─────────────────────────────────────────────────────────────────────

/// What to transcribe.
#[derive(Clone, Copy, Debug)]
pub struct TranscribeRequest<'a> {
    /// The model to use; `None` for the server's default.
    pub model: Option<&'a str>,
    /// The spoken language; `None` for the server's default.
    pub language: Option<&'a str>,
    /// The audio: 16 kHz mono 16-bit PCM.
    pub pcm: &'a [i16],
}

/// A server's transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transcript {
    /// The text, as the server gave it.
    pub text: String,
    /// The language it reports; `None` when not given.
    pub language: Option<String>,
}

fn audio_format() -> Value {
    json!({ "rate": SAMPLE_RATE, "width": 2, "channels": 1 })
}

/// Has the server at `server` transcribe `request`'s audio, within `deadline`.
pub async fn transcribe(
    server: &ServerAddress,
    request: TranscribeRequest<'_>,
    deadline: Duration,
) -> Result<Transcript, WyomingError> {
    within(deadline, async {
        let mut stream = connect(server).await?;
        let mut asked = Map::new();
        if let Some(model) = request.model.filter(|m| !m.is_empty()) {
            asked.insert("name".to_owned(), json!(model));
        }
        if let Some(language) = request.language.filter(|l| !l.is_empty()) {
            asked.insert("language".to_owned(), json!(language));
        }
        let writer = stream.get_mut();
        let write = WyomingError::Io;
        write_event(
            writer,
            &Event::new("transcribe").with_data(Value::Object(asked)),
        )
        .await
        .map_err(write)?;
        write_event(writer, &Event::new("audio-start").with_data(audio_format()))
            .await
            .map_err(write)?;
        let mut bytes = Vec::with_capacity(CHUNK_SAMPLES * 2);
        for chunk in request.pcm.chunks(CHUNK_SAMPLES) {
            bytes.clear();
            for sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            write_event(
                writer,
                &Event::new("audio-chunk")
                    .with_data(audio_format())
                    .with_payload(bytes.clone()),
            )
            .await
            .map_err(write)?;
        }
        write_event(writer, &Event::new("audio-stop"))
            .await
            .map_err(write)?;
        let transcript = wait_for(&mut stream, "transcript").await?;
        let text =
            text(&transcript.data, "text").ok_or_else(|| protocol("the transcript has no text"))?;
        Ok(Transcript {
            text,
            language: self::text(&transcript.data, "language"),
        })
    })
    .await
}
#[cfg(any(test, feature = "testing"))]
pub mod sim;

#[cfg(test)]
mod tests;
