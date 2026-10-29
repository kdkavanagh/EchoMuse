//! Turning an upload into an alert asset: `ffmpeg` decodes it to 48 kHz mono PCM16, and
//! the result is cut to [`MAX_SAMPLES`], faded out over its last [`FADE_SAMPLES`], and
//! written as a plain 44-byte-header WAV named by its SHA-256. Everything here blocks; it
//! runs on the sound service's thread, never on a device runtime or the I/O runtime.

use std::fmt;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use echomuse_protocol::Sha256;

/// The asset sample rate.
pub const ASSET_RATE: u32 = 48_000;
/// The longest asset: 10 s.
pub const MAX_SAMPLES: usize = 10 * ASSET_RATE as usize;
/// The fade-out at the end of every asset: 50 ms.
pub const FADE_SAMPLES: usize = ASSET_RATE as usize / 20;
/// How long one decode may run.
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(60);
/// How much more than [`MAX_SAMPLES`] is decoded, to tell whether the upload was longer.
const DECODE_SECONDS: &str = "10.1";
const STDERR_TAIL_BYTES: u64 = 512;
const POLL: Duration = Duration::from_millis(10);

/// Why an export failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExportError {
    /// `ffmpeg` could not be started.
    Spawn(String),
    /// `ffmpeg` failed; its last error output.
    Decode(String),
    /// The upload holds no audio.
    NoAudio,
    /// Decoding took longer than [`DECODE_TIMEOUT`].
    TimedOut,
    /// The service is stopping.
    Stopped,
    /// A file could not be read or written.
    Io(String),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "ffmpeg could not be started: {e}"),
            Self::Decode(e) => write!(f, "ffmpeg could not decode the sound: {e}"),
            Self::NoAudio => f.write_str("the file holds no audio"),
            Self::TimedOut => f.write_str("decoding took too long"),
            Self::Stopped => f.write_str("the controller is stopping"),
            Self::Io(e) => write!(f, "file error: {e}"),
        }
    }
}

impl ExportError {
    /// The upload itself cannot be made into a sound, so trying again will not help.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Decode(_) | Self::NoAudio)
    }
}

impl std::error::Error for ExportError {}

/// A finished asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Asset {
    /// The whole WAV file.
    pub wav: Vec<u8>,
    /// Its SHA-256, which names it.
    pub sha256: Sha256,
    /// Its length in samples.
    pub samples: usize,
    /// The decoded audio was longer than [`MAX_SAMPLES`].
    pub shortened: bool,
}

impl Asset {
    /// Its length in seconds, to 0.01 s.
    pub fn seconds(&self) -> f64 {
        (self.samples as f64 * 100.0 / f64::from(ASSET_RATE)).round() / 100.0
    }
}

/// Makes the asset from decoded 48 kHz mono samples.
pub fn finish(mut samples: Vec<i16>) -> Result<Asset, ExportError> {
    if samples.is_empty() {
        return Err(ExportError::NoAudio);
    }
    let shortened = samples.len() > MAX_SAMPLES;
    samples.truncate(MAX_SAMPLES);
    fade_out(&mut samples);
    let wav = wav_bytes(&samples);
    Ok(Asset {
        sha256: Sha256::of(&wav),
        samples: samples.len(),
        shortened,
        wav,
    })
}

/// A linear fade to silence over the last [`FADE_SAMPLES`] (or the whole sound when it is
/// shorter): the last sample is 0.
fn fade_out(samples: &mut [i16]) {
    let n = samples.len().min(FADE_SAMPLES);
    let start = samples.len() - n;
    let steps = (n.max(2) - 1) as f64;
    for (k, sample) in samples[start..].iter_mut().enumerate() {
        let gain = (n - 1 - k) as f64 / steps;
        *sample = (f64::from(*sample) * gain).round() as i16;
    }
}

/// A 48 kHz mono PCM16 WAV file with the plain 44-byte header.
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = u32::try_from(samples.len() * 2).expect("assets are at most 10 s");
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&ASSET_RATE.to_le_bytes());
    wav.extend_from_slice(&(ASSET_RATE * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Decodes the first 10.1 s of `input` to 48 kHz mono samples with `ffmpeg`, through
/// scratch files in `scratch_dir`. Setting `stop` kills the child and returns
/// [`ExportError::Stopped`]; the child is always reaped.
pub fn decode(
    ffmpeg: &Path,
    input: &Path,
    scratch_dir: &Path,
    stop: &AtomicBool,
) -> Result<Vec<i16>, ExportError> {
    let n = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let raw = scratch_dir.join(format!(".decode.{pid}.{n}.raw"));
    let log = scratch_dir.join(format!(".decode.{pid}.{n}.log"));
    let result = run_ffmpeg(ffmpeg, input, &raw, &log, stop);
    let _ = fs::remove_file(&raw);
    let _ = fs::remove_file(&log);
    result
}

fn run_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    raw: &Path,
    log: &Path,
    stop: &AtomicBool,
) -> Result<Vec<i16>, ExportError> {
    let io = |e: std::io::Error| ExportError::Io(e.to_string());
    let stderr = File::create(log).map_err(io)?;
    let mut child = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-nostats",
            "-nostdin",
            "-loglevel",
            "error",
            "-y",
        ])
        .arg("-i")
        .arg(input)
        .args(["-map", "0:a:0", "-vn", "-ac", "1"])
        .args(["-ar", &ASSET_RATE.to_string(), "-t", DECODE_SECONDS])
        .args(["-f", "s16le", "-c:a", "pcm_s16le"])
        .arg(raw)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|e| ExportError::Spawn(e.to_string()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            break status;
        }
        let stopping = stop.load(Ordering::Relaxed);
        if stopping || started.elapsed() > DECODE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if stopping {
                ExportError::Stopped
            } else {
                ExportError::TimedOut
            });
        }
        thread::sleep(POLL);
    };
    if !status.success() {
        return Err(ExportError::Decode(stderr_tail(log, status)));
    }
    let bytes = fs::read(raw).map_err(io)?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect())
}

fn stderr_tail(log: &Path, status: std::process::ExitStatus) -> String {
    let text = fs::read(log).unwrap_or_default();
    let tail = &text[text.len().saturating_sub(STDERR_TAIL_BYTES as usize)..];
    let tail = String::from_utf8_lossy(tail).trim().to_owned();
    if tail.is_empty() {
        format!("ffmpeg exited with {status}")
    } else {
        tail
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A sine tone at `rate`, `channels` interleaved, `seconds` long.
    pub(crate) fn tone(rate: u32, channels: u16, seconds: f64, hz: f64) -> Vec<i16> {
        let frames = (f64::from(rate) * seconds).round() as usize;
        let mut out = Vec::with_capacity(frames * usize::from(channels));
        for i in 0..frames {
            let v = (f64::sin(2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate))
                * 12_000.0)
                .round() as i16;
            for _ in 0..channels {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn a_short_tone_becomes_a_faded_asset_named_by_its_hash() {
        let samples = tone(ASSET_RATE, 1, 2.0, 1_000.0);
        let asset = finish(samples.clone()).unwrap();
        assert_eq!(asset.samples, 96_000);
        assert!(!asset.shortened);
        assert_eq!(asset.seconds(), 2.0);
        assert_eq!(asset.sha256, Sha256::of(&asset.wav));
        assert_eq!(asset.wav.len(), 44 + 192_000);
        // The header the existing assets carry.
        assert_eq!(
            asset.wav[..44],
            [
                b'R', b'I', b'F', b'F', 0x24, 0xee, 0x02, 0x00, b'W', b'A', b'V', b'E', b'f', b'm',
                b't', b' ', 16, 0, 0, 0, 1, 0, 1, 0, 0x80, 0xbb, 0, 0, 0x00, 0x77, 0x01, 0x00, 2,
                0, 16, 0, b'd', b'a', b't', b'a', 0x00, 0xee, 0x02, 0x00
            ]
        );
        let pcm: Vec<i16> = asset.wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| i16::from_le_bytes(*p))
            .collect();
        let fade_start = 96_000 - FADE_SAMPLES;
        assert_eq!(pcm[..fade_start], samples[..fade_start]);
        assert_eq!(pcm[fade_start], samples[fade_start]);
        assert_eq!(*pcm.last().unwrap(), 0);
        let mid = fade_start + FADE_SAMPLES / 2;
        assert!(pcm[mid].unsigned_abs() <= samples[mid].unsigned_abs() / 2 + 6);
    }

    #[test]
    fn a_long_sound_is_cut_to_ten_seconds() {
        let asset = finish(tone(ASSET_RATE, 1, 12.0, 440.0)).unwrap();
        assert!(asset.shortened);
        assert_eq!(asset.samples, MAX_SAMPLES);
        assert_eq!(asset.seconds(), 10.0);
        let exact = finish(tone(ASSET_RATE, 1, 10.0, 440.0)).unwrap();
        assert!(!exact.shortened);
        assert_eq!(finish(Vec::new()), Err(ExportError::NoAudio));
        // A sound shorter than the fade fades over its whole length.
        let blip = finish(vec![1_000; 10]).unwrap();
        assert_eq!(blip.samples, 10);
        assert_eq!(blip.seconds(), 0.0);
    }
}
