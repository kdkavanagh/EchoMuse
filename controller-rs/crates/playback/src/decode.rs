//! Fetching and decoding network audio (spec §4.8, §4.9, §5.1).
//!
//! A [`Decoder`] fetches an HTTP or HTTPS URL, such as a Home Assistant TTS
//! proxy URL or a media URL, and pipes the body through an `ffmpeg` child
//! process that writes 48 kHz mono PCM16. The work runs on the runtime handle
//! the decoder was given, normally the shared I/O runtime, never on a device
//! runtime's thread; the decoding itself happens in the child process.
//!
//! Audio streams out as it decodes, in chunks of at most
//! [`DECODE_CHUNK_FRAMES`], through a bounded channel. When the reader stops
//! taking chunks, the channel fills, `ffmpeg` blocks on its output, stops
//! reading its input, and the fetch stops reading the socket, so memory stays
//! bounded however long the stream is.
//!
//! Cancelling, or dropping the [`DecodeStream`], kills the child and waits
//! for it, so no `ffmpeg` process is left running or unreaped.
//!
//! A resource that announces its length is [`Seekability::Seekable`]: a
//! later decode of the same URL can skip to a bookmark by discarding decoded
//! frames. One without a length (a live stream) is [`Seekability::Live`] and
//! resumes at its live edge.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use reqwest::Url;
use reqwest::header::CONTENT_LENGTH;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::eq::{Equalizer, RENDER_SAMPLE_RATE};

/// The most frames one decoded chunk carries: one render packet.
pub const DECODE_CHUNK_FRAMES: usize = 3_840;

/// How many decoded chunks wait for the reader before decoding pauses:
/// about 0.6 s of audio.
pub const DECODE_QUEUE_CHUNKS: usize = 8;

/// How much of `ffmpeg`'s error output is kept for a failure report.
const STDERR_TAIL_BYTES: usize = 2_048;

/// How long the server may take to answer before the fetch fails.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether a later decode can resume the resource from a bookmark.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Seekability {
    /// It announced its length: the same bytes can be fetched again.
    Seekable,
    /// It announced no length: a live stream, resumed at its live edge.
    Live,
}

/// Why fetching failed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FetchError {
    /// Only `http` and `https` URLs are fetched.
    #[error("unsupported URL scheme {0}")]
    Scheme(String),
    /// No response within the response timeout.
    #[error("no response within the timeout")]
    TimedOut,
    /// The request failed: name resolution, connection or TLS.
    #[error("request failed: {0}")]
    Request(String),
    /// The server answered with an error status.
    #[error("HTTP status {0}")]
    Status(u16),
    /// The body broke off.
    #[error("body failed: {0}")]
    Body(String),
}

/// Why decoding failed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DecodeError {
    /// The audio could not be fetched.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// `ffmpeg` could not be started.
    #[error("ffmpeg could not start: {0}")]
    Spawn(String),
    /// `ffmpeg` exited unsuccessfully, for example on data it cannot decode.
    #[error("ffmpeg exited with {exit_code:?}: {detail}")]
    Ffmpeg {
        /// Its exit code; `None` when a signal ended it.
        exit_code: Option<i32>,
        /// The end of its error output.
        detail: String,
    },
    /// `ffmpeg` succeeded but produced no audio.
    #[error("no audio in the stream")]
    NoAudio,
    /// Reading `ffmpeg`'s output failed.
    #[error("reading decoded audio failed: {0}")]
    Pipe(String),
}

/// What a completed decode produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeSummary {
    /// Frames decoded, including skipped ones.
    pub decoded_frames: u64,
    /// Frames delivered after the skip.
    pub delivered_frames: u64,
}

/// One message from a decode, in order: `Opened`, any number of `Audio`,
/// then `Finished`. A cancelled decode sends nothing more.
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeEvent {
    /// The server answered and decoding started.
    Opened {
        /// Whether the resource can be resumed from a bookmark.
        seekable: Seekability,
    },
    /// Decoded 48 kHz mono audio, at most [`DECODE_CHUNK_FRAMES`] frames.
    Audio(Vec<i16>),
    /// The decode is over.
    Finished(Result<DecodeSummary, DecodeError>),
}

/// How a decode task ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeEnd {
    /// Every frame was decoded.
    Completed(DecodeSummary),
    /// It failed.
    Failed(DecodeError),
    /// It was cancelled, or its reader went away; the child was killed and
    /// reaped.
    Cancelled,
}

/// What to decode.
#[derive(Clone, Debug)]
pub struct DecodeRequest {
    /// An absolute `http` or `https` URL.
    pub url: Url,
    /// Decoded frames to discard before delivering any: a bookmark into a
    /// seekable resource.
    pub skip_frames: u64,
    /// The equalizer to apply to the delivered audio, if any.
    pub equalizer: Option<Equalizer>,
}

/// Resolves a URL Home Assistant gave, which may be a path such as
/// `/api/tts_proxy/…`, against its base URL.
pub fn resolve_media_url(base: &Url, reference: &str) -> Result<Url, FetchError> {
    let url = base
        .join(reference)
        .map_err(|err| FetchError::Request(format!("invalid URL {reference:?}: {err}")))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(FetchError::Scheme(other.to_owned())),
    }
}

/// Starts decodes. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Decoder {
    runtime: Handle,
    http: reqwest::Client,
    ffmpeg: PathBuf,
    response_timeout: Duration,
}

impl Decoder {
    /// Decodes on `runtime` with `http` and the `ffmpeg` executable at
    /// `ffmpeg`. The integrator builds `http` with the controller's TLS
    /// configuration.
    pub fn new(runtime: Handle, http: reqwest::Client, ffmpeg: PathBuf) -> Self {
        Self {
            runtime,
            http,
            ffmpeg,
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }

    /// Changes how long the server may take to answer.
    pub fn with_response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = timeout;
        self
    }

    /// Starts decoding `request`. The returned stream yields its events.
    pub fn start(&self, request: DecodeRequest) -> DecodeStream {
        let (events, receiver) = mpsc::channel(DECODE_QUEUE_CHUNKS);
        let cancel = CancellationToken::new();
        let pid = Arc::new(AtomicU32::new(0));
        let job = Job {
            http: self.http.clone(),
            ffmpeg: self.ffmpeg.clone(),
            response_timeout: self.response_timeout,
            request,
            events,
            cancel: cancel.clone(),
            pid: Arc::clone(&pid),
        };
        let task = self.runtime.spawn(job.run());
        DecodeStream {
            events: receiver,
            cancel,
            task: Some(task),
            pid,
        }
    }
}

/// One decode in progress. Dropping it cancels the decode.
#[derive(Debug)]
pub struct DecodeStream {
    events: mpsc::Receiver<DecodeEvent>,
    cancel: CancellationToken,
    task: Option<JoinHandle<DecodeEnd>>,
    pid: Arc<AtomicU32>,
}

impl DecodeStream {
    /// The next event; `None` once the decode has ended and every event has
    /// been taken, or after cancellation.
    pub async fn recv(&mut self) -> Option<DecodeEvent> {
        self.events.recv().await
    }

    /// The next event if one is waiting.
    pub fn try_recv(&mut self) -> Option<DecodeEvent> {
        self.events.try_recv().ok()
    }

    /// Stops the decode: the child is killed and reaped.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Waits for the decode task to end and reports how. After
    /// [`cancel`](Self::cancel), this returns once the child has been reaped.
    pub async fn stopped(mut self) -> DecodeEnd {
        // Events still queued are dropped with the receiver so a decode
        // waiting to send cannot block its own end.
        self.events.close();
        let task = self.task.take().expect("the task is taken only here");
        match task.await {
            Ok(end) => end,
            Err(err) => DecodeEnd::Failed(DecodeError::Pipe(format!("decode task: {err}"))),
        }
    }

    /// The `ffmpeg` child's process id once it has started, for diagnostics.
    pub fn ffmpeg_pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::Acquire) {
            0 => None,
            pid => Some(pid),
        }
    }
}

impl Drop for DecodeStream {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancel.cancel();
        }
    }
}

struct Job {
    http: reqwest::Client,
    ffmpeg: PathBuf,
    response_timeout: Duration,
    request: DecodeRequest,
    events: mpsc::Sender<DecodeEvent>,
    cancel: CancellationToken,
    pid: Arc<AtomicU32>,
}

/// Why piping stopped early.
enum PumpError {
    Fetch(FetchError),
    Pipe(String),
    /// The reader went away.
    Abandoned,
}

impl Job {
    async fn run(self) -> DecodeEnd {
        let end = self.decode().await;
        let event = match &end {
            DecodeEnd::Completed(summary) => Some(DecodeEvent::Finished(Ok(*summary))),
            DecodeEnd::Failed(error) => Some(DecodeEvent::Finished(Err(error.clone()))),
            DecodeEnd::Cancelled => None,
        };
        if let Some(event) = event {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {}
                _ = self.events.send(event) => {}
            }
        }
        end
    }

    async fn decode(&self) -> DecodeEnd {
        let url = &self.request.url;
        if !matches!(url.scheme(), "http" | "https") {
            return DecodeEnd::Failed(FetchError::Scheme(url.scheme().to_owned()).into());
        }
        let response = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return DecodeEnd::Cancelled,
            response = tokio::time::timeout(self.response_timeout, self.http.get(url.clone()).send()) => response,
        };
        let response = match response {
            Err(_) => return DecodeEnd::Failed(FetchError::TimedOut.into()),
            Ok(Err(err)) => return DecodeEnd::Failed(FetchError::Request(err.to_string()).into()),
            Ok(Ok(response)) if !response.status().is_success() => {
                return DecodeEnd::Failed(FetchError::Status(response.status().as_u16()).into());
            }
            Ok(Ok(response)) => response,
        };
        let seekable = if response.headers().contains_key(CONTENT_LENGTH) {
            Seekability::Seekable
        } else {
            Seekability::Live
        };
        if !self.send(DecodeEvent::Opened { seekable }).await {
            return DecodeEnd::Cancelled;
        }

        let mut child = match self.spawn() {
            Ok(child) => child,
            Err(err) => return DecodeEnd::Failed(DecodeError::Spawn(err.to_string())),
        };
        if let Some(pid) = child.id() {
            self.pid.store(pid, Ordering::Release);
        }
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            reap(&mut child).await;
            return DecodeEnd::Failed(DecodeError::Spawn("ffmpeg pipes missing".to_owned()));
        };
        let stderr = tokio::spawn(stderr_tail(stderr));

        let pumped = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => None,
            result = async { tokio::try_join!(feed(response, stdin), self.read(stdout)) } => Some(result),
        };
        let summary = match pumped {
            None | Some(Err(PumpError::Abandoned)) => {
                reap(&mut child).await;
                stderr.abort();
                return DecodeEnd::Cancelled;
            }
            Some(Err(PumpError::Fetch(error))) => {
                reap(&mut child).await;
                stderr.abort();
                return DecodeEnd::Failed(error.into());
            }
            Some(Err(PumpError::Pipe(detail))) => {
                reap(&mut child).await;
                stderr.abort();
                return DecodeEnd::Failed(DecodeError::Pipe(detail));
            }
            Some(Ok(((), summary))) => summary,
        };
        let status = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                reap(&mut child).await;
                stderr.abort();
                return DecodeEnd::Cancelled;
            }
            status = child.wait() => status,
        };
        let detail = stderr.await.unwrap_or_default();
        match status {
            Ok(status) if status.success() && summary.decoded_frames > 0 => {
                DecodeEnd::Completed(summary)
            }
            Ok(status) if status.success() => DecodeEnd::Failed(DecodeError::NoAudio),
            Ok(status) => DecodeEnd::Failed(DecodeError::Ffmpeg {
                exit_code: status.code(),
                detail,
            }),
            Err(err) => DecodeEnd::Failed(DecodeError::Pipe(err.to_string())),
        }
    }

    fn spawn(&self) -> std::io::Result<Child> {
        Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-nostats", "-loglevel", "error"])
            // A small probe: with the default 5 MB, ffmpeg holds back output
            // until that much of a slow stream (streamed TTS, a live radio
            // stream) has arrived. 32 KiB was checked to identify WAV, MP3,
            // FLAC and ADTS AAC arriving slowly.
            .args(["-probesize", "32768"])
            .args(["-i", "pipe:0", "-map", "0:a:0", "-vn"])
            .args(["-ac", "1", "-ar", &RENDER_SAMPLE_RATE.to_string()])
            .args(["-f", "s16le", "-c:a", "pcm_s16le", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
    }

    async fn send(&self, event: DecodeEvent) -> bool {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            sent = self.events.send(event) => sent.is_ok(),
        }
    }

    /// Reads decoded PCM in whole chunks, skips, equalizes and delivers it.
    async fn read(&self, mut stdout: impl AsyncRead + Unpin) -> Result<DecodeSummary, PumpError> {
        let mut equalizer = self.request.equalizer.clone();
        let mut skip = self.request.skip_frames;
        let mut summary = DecodeSummary {
            decoded_frames: 0,
            delivered_frames: 0,
        };
        let mut buffer = vec![0u8; DECODE_CHUNK_FRAMES * 2];
        loop {
            let mut filled = 0;
            while filled < buffer.len() {
                let read = stdout
                    .read(&mut buffer[filled..])
                    .await
                    .map_err(|err| PumpError::Pipe(err.to_string()))?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            // ffmpeg writes whole samples; a stray odd byte at the end is not
            // audio.
            let frames = filled / 2;
            if frames == 0 {
                return Ok(summary);
            }
            summary.decoded_frames += frames as u64;
            let skipped = skip.min(frames as u64) as usize;
            skip -= skipped as u64;
            let mut pcm: Vec<i16> = buffer[skipped * 2..frames * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair))
                .collect();
            if !pcm.is_empty() {
                if let Some(equalizer) = equalizer.as_mut() {
                    equalizer.process(&mut pcm);
                }
                summary.delivered_frames += pcm.len() as u64;
                if self.events.send(DecodeEvent::Audio(pcm)).await.is_err() {
                    return Err(PumpError::Abandoned);
                }
            }
            if filled < buffer.len() {
                return Ok(summary);
            }
        }
    }
}

/// Copies the body into `ffmpeg`'s input, then closes it so `ffmpeg` sees the
/// end. A write error means `ffmpeg` stopped reading; its exit status says
/// why.
async fn feed(mut response: reqwest::Response, mut stdin: ChildStdin) -> Result<(), PumpError> {
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|err| PumpError::Fetch(FetchError::Body(err.to_string())))?;
        let Some(chunk) = chunk else {
            break;
        };
        if stdin.write_all(&chunk).await.is_err() {
            break;
        }
    }
    drop(stdin);
    Ok(())
}

/// The last [`STDERR_TAIL_BYTES`] of `ffmpeg`'s error output.
async fn stderr_tail(mut stderr: impl AsyncRead + Unpin) -> String {
    let mut tail = Vec::new();
    let mut buffer = [0u8; 512];
    while let Ok(read) = stderr.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        tail.extend_from_slice(&buffer[..read]);
        if tail.len() > STDERR_TAIL_BYTES {
            tail.drain(..tail.len() - STDERR_TAIL_BYTES);
        }
    }
    String::from_utf8_lossy(&tail).trim().to_owned()
}

/// Kills the child and waits for it, so it leaves no zombie.
async fn reap(child: &mut Child) {
    // An error means it has already exited; waiting still reaps it.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests;
