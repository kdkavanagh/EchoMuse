//! A ZIP archive written front to back, for archives streamed to the dashboard while they
//! are built.
//!
//! Every entry is stored (method 0, not compressed: WAV audio barely compresses) with its
//! CRC-32 and sizes in the local header, so any reader can extract it without the central
//! directory, and no data descriptor is needed. Each file is read twice through one open
//! handle: once for its CRC, once to copy it. A file renamed or removed after it was opened
//! is still read whole. Memory is one copy buffer, whatever the file sizes.
//!
//! Archives are plain ZIP, not ZIP64: at most [`MAX_ENTRIES`] entries and [`MAX_BYTES`]
//! bytes. [`ZipPlan`] checks both before anything is written.

use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use axum::{
    body::{Body, Bytes},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use tokio::sync::mpsc;

/// The most entries a plain ZIP holds.
pub(crate) const MAX_ENTRIES: usize = u16::MAX as usize;
/// The most bytes a plain ZIP holds: every offset and size fits 32 bits.
pub(crate) const MAX_BYTES: u64 = u32::MAX as u64;

const LOCAL_HEADER: u64 = 30;
const CENTRAL_HEADER: u64 = 46;
const END_RECORD: u64 = 22;
const COPY_BUFFER: usize = 64 * 1024;

/// Version 2.0, the version stored entries need.
const VERSION_NEEDED: u16 = 20;
/// Made by Unix (3), version 2.0, so the external attributes carry Unix permissions.
const VERSION_MADE_BY: u16 = (3 << 8) | 20;
/// Bit 11: the name is UTF-8.
const FLAG_UTF8: u16 = 1 << 11;
/// A regular file, `rw-r--r--`, in the high half of the external attributes.
const EXTERNAL_ATTRIBUTES: u32 = 0o100_644 << 16;

/// The size an archive of these entries will have, checked against the plain ZIP limits
/// before it is written.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ZipPlan {
    entries: usize,
    bytes: u64,
}

impl ZipPlan {
    /// Count one entry of `name` holding `size` bytes.
    pub(crate) fn add(&mut self, name: &str, size: u64) {
        let name = name.len() as u64;
        self.entries += 1;
        self.bytes = self
            .bytes
            .saturating_add(LOCAL_HEADER + name + size + CENTRAL_HEADER + name);
    }

    /// Whether the archive fits a plain ZIP.
    pub(crate) fn fits(&self) -> bool {
        self.entries <= MAX_ENTRIES && self.bytes.saturating_add(END_RECORD) <= MAX_BYTES
    }
}

/// A modification time as a ZIP entry stores it (MS-DOS date and time, UTC here).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DosTime {
    time: u16,
    date: u16,
}

impl DosTime {
    /// The time of whole Unix second `seconds`; before 1980, which DOS time cannot say, it
    /// is 1980-01-01 00:00:00, and after 2107 the last day DOS time can say.
    pub(crate) fn from_unix(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        if year < 1980 {
            return Self {
                time: 0,
                date: (1 << 5) | 1,
            };
        }
        if year > 2107 {
            return Self {
                time: (23 << 11) | (59 << 5) | 29,
                date: (127 << 9) | (12 << 5) | 31,
            };
        }
        let (hour, minute, second) = (of_day / 3600, of_day / 60 % 60, of_day % 60);
        Self {
            time: ((hour << 11) | (minute << 5) | (second / 2)) as u16,
            date: (((year - 1980) << 9) | (month << 5) | day) as u16,
        }
    }
}

/// The proleptic Gregorian (year, month, day) of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

struct CentralEntry {
    name: String,
    crc: u32,
    size: u32,
    time: DosTime,
    offset: u32,
}

fn too_large() -> io::Error {
    io::Error::other("the archive outgrew a plain ZIP")
}

/// Writes a ZIP archive to `out` front to back.
pub(crate) struct ZipWriter<W: Write> {
    out: W,
    offset: u64,
    entries: Vec<CentralEntry>,
}

impl<W: Write> ZipWriter<W> {
    pub(crate) fn new(out: W) -> Self {
        Self {
            out,
            offset: 0,
            entries: Vec::new(),
        }
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    /// Adds `file`, read from its start, as entry `name`.
    pub(crate) fn add_file<R: Read + Seek>(
        &mut self,
        name: &str,
        file: &mut R,
        time: DosTime,
    ) -> io::Result<()> {
        if self.entries.len() >= MAX_ENTRIES {
            return Err(too_large());
        }
        let mut buffer = vec![0; COPY_BUFFER];
        // First pass: the CRC and the size the header states.
        file.seek(SeekFrom::Start(0))?;
        let mut crc = crc32fast::Hasher::new();
        let mut size: u64 = 0;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            crc.update(&buffer[..read]);
            size += read as u64;
        }
        let crc = crc.finalize();
        let offset = u32::try_from(self.offset).map_err(|_| too_large())?;
        let size32 = u32::try_from(size).map_err(|_| too_large())?;
        let name_len = u16::try_from(name.len()).map_err(|_| too_large())?;
        if self.offset + LOCAL_HEADER + u64::from(name_len) + size > MAX_BYTES {
            return Err(too_large());
        }

        let mut header = Vec::with_capacity(LOCAL_HEADER as usize + name.len());
        header.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        header.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        header.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes());
        header.extend_from_slice(&time.time.to_le_bytes());
        header.extend_from_slice(&time.date.to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&size32.to_le_bytes());
        header.extend_from_slice(&size32.to_le_bytes());
        header.extend_from_slice(&name_len.to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes());
        header.extend_from_slice(name.as_bytes());
        self.put(&header)?;

        // Second pass: the bytes, which must be the ones the CRC covered.
        file.seek(SeekFrom::Start(0))?;
        let mut copied: u64 = 0;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            copied += read as u64;
            if copied > size {
                break;
            }
            self.put(&buffer[..read])?;
        }
        if copied != size {
            return Err(io::Error::other(format!(
                "{name} changed while it was archived"
            )));
        }
        self.entries.push(CentralEntry {
            name: name.to_owned(),
            crc,
            size: size32,
            time,
            offset,
        });
        Ok(())
    }

    /// Writes the central directory and the end record, and returns the output.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        let start = self.offset;
        let entries = std::mem::take(&mut self.entries);
        for entry in &entries {
            let mut header = Vec::with_capacity(CENTRAL_HEADER as usize + entry.name.len());
            header.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
            header.extend_from_slice(&VERSION_MADE_BY.to_le_bytes());
            header.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
            header.extend_from_slice(&FLAG_UTF8.to_le_bytes());
            header.extend_from_slice(&0_u16.to_le_bytes());
            header.extend_from_slice(&entry.time.time.to_le_bytes());
            header.extend_from_slice(&entry.time.date.to_le_bytes());
            header.extend_from_slice(&entry.crc.to_le_bytes());
            header.extend_from_slice(&entry.size.to_le_bytes());
            header.extend_from_slice(&entry.size.to_le_bytes());
            header.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            header.extend_from_slice(&0_u16.to_le_bytes()); // extra field
            header.extend_from_slice(&0_u16.to_le_bytes()); // comment
            header.extend_from_slice(&0_u16.to_le_bytes()); // disk
            header.extend_from_slice(&0_u16.to_le_bytes()); // internal attributes
            header.extend_from_slice(&EXTERNAL_ATTRIBUTES.to_le_bytes());
            header.extend_from_slice(&entry.offset.to_le_bytes());
            header.extend_from_slice(entry.name.as_bytes());
            self.put(&header)?;
        }
        let size = u32::try_from(self.offset - start).map_err(|_| too_large())?;
        let start = u32::try_from(start).map_err(|_| too_large())?;
        if self.offset + END_RECORD > MAX_BYTES {
            return Err(too_large());
        }
        let count = entries.len() as u16;
        let mut end = Vec::with_capacity(END_RECORD as usize);
        end.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        end.extend_from_slice(&0_u16.to_le_bytes()); // this disk
        end.extend_from_slice(&0_u16.to_le_bytes()); // the directory's disk
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&size.to_le_bytes());
        end.extend_from_slice(&start.to_le_bytes());
        end.extend_from_slice(&0_u16.to_le_bytes()); // comment
        self.put(&end)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Chunks of a streamed archive waiting for the client, at most [`COPY_BUFFER`] each. With
/// the writer's own buffer this bounds an archive's memory, whatever its size.
const STREAM_CHUNKS: usize = 4;

/// One file of a streamed archive.
#[derive(Clone, Debug)]
pub(crate) struct ZipEntry {
    /// The name inside the archive.
    pub(crate) name: String,
    /// The file.
    pub(crate) path: PathBuf,
    /// Its time inside the archive.
    pub(crate) time: DosTime,
}

/// A stretch of a 16 kHz mono PCM16 WAV, in samples.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SampleSpan {
    /// The first sample.
    pub(crate) start: u64,
    /// How many samples.
    pub(crate) len: u64,
}

impl SampleSpan {
    /// The size of the WAV file this span becomes.
    pub(crate) fn wav_size(&self) -> u64 {
        echomuse_records::recordings::WAV_HEADER_BYTES as u64 + self.len * 2
    }
}

/// One entry of a streamed archive: a file whole, or a span of a WAV file as a WAV of
/// its own.
#[derive(Clone, Debug)]
pub(crate) struct ZipPart {
    /// The file, its name and time.
    pub(crate) entry: ZipEntry,
    /// The span to take; `None` for the whole file.
    pub(crate) span: Option<SampleSpan>,
}

/// The PCM data of a RIFF WAV holding 16-bit mono PCM; `None` when it is not one.
fn pcm16_mono(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut format_ok = false;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = bytes.get(at + 8..)?;
        let body = &body[..size.min(body.len())];
        match id {
            b"fmt " => {
                let field = |i: usize| -> Option<u16> {
                    Some(u16::from_le_bytes(body.get(i..i + 2)?.try_into().ok()?))
                };
                format_ok = field(0)? == 1 && field(2)? == 1 && field(14)? == 16;
            }
            b"data" => return format_ok.then_some(body),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    None
}

/// The WAV of `span` of the WAV in `bytes`, cut short where the audio ends; `None` when
/// `bytes` is not 16-bit mono PCM.
pub(crate) fn crop_wav(bytes: &[u8], span: SampleSpan) -> Option<Vec<u8>> {
    let data = pcm16_mono(bytes)?;
    let samples = (data.len() / 2) as u64;
    let start = span.start.min(samples) as usize;
    let end = span.start.saturating_add(span.len).min(samples) as usize;
    let pcm: Vec<i16> = data[start * 2..end * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    Some(echomuse_records::recordings::wav_bytes(&pcm))
}

/// `name`, or the first of `name (1)`, `name (2)`, … (before the extension) not yet in
/// `taken`, as a browser numbers a repeated download; the result is added to `taken`.
pub(crate) fn unique_name(taken: &mut HashSet<String>, name: &str) -> String {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut candidate = name.to_owned();
    let mut n = 1;
    while taken.contains(&candidate) {
        candidate = format!("{stem} ({n}){extension}");
        n += 1;
    }
    taken.insert(candidate.clone());
    candidate
}

/// Writes into the channel in [`COPY_BUFFER`] chunks; fails once the client has gone.
struct ChannelSink {
    chunks: mpsc::Sender<io::Result<Bytes>>,
    pending: Vec<u8>,
}

impl ChannelSink {
    fn send(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(
            &mut self.pending,
            Vec::with_capacity(COPY_BUFFER),
        ));
        self.chunks
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the client went away"))
    }
}

impl Write for ChannelSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = COPY_BUFFER - self.pending.len();
        let taken = bytes.len().min(room);
        self.pending.extend_from_slice(&bytes[..taken]);
        if self.pending.len() >= COPY_BUFFER {
            self.send()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// The download of the archive of `parts`, in order, named `filename`: whole files and spans
/// of WAV files, written on a blocking thread as the client reads it. An entry whose file is
/// gone by the time it is reached, or a span whose file is not a 16-bit mono PCM WAV, is
/// left out. Any other failure ends the body with an error, so the client sees a broken
/// transfer rather than a short archive. Check the size with [`ZipPlan`] first.
pub(crate) fn zip_download(parts: Vec<ZipPart>, filename: &str) -> Response {
    let mut response = stream_parts(parts).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

fn stream_parts(parts: Vec<ZipPart>) -> Body {
    let (chunks, receiver) = mpsc::channel(STREAM_CHUNKS);
    tokio::task::spawn_blocking(move || {
        let failure = chunks.clone();
        let mut zip = ZipWriter::new(ChannelSink {
            chunks,
            pending: Vec::with_capacity(COPY_BUFFER),
        });
        let written = (|| {
            for part in &parts {
                let entry = &part.entry;
                let mut file = match File::open(&entry.path) {
                    Ok(file) => file,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                match part.span {
                    None => zip.add_file(&entry.name, &mut file, entry.time)?,
                    Some(span) => {
                        let mut bytes = Vec::new();
                        file.read_to_end(&mut bytes)?;
                        let Some(cropped) = crop_wav(&bytes, span) else {
                            tracing::warn!(
                                "{} is not a 16-bit mono WAV; left out of the archive",
                                entry.path.display()
                            );
                            continue;
                        };
                        zip.add_file(&entry.name, &mut io::Cursor::new(cropped), entry.time)?;
                    }
                }
            }
            zip.finish().map(drop)
        })();
        if let Err(error) = written
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            tracing::error!(%error, "an archive could not be written");
            let _ = failure.blocking_send(Err(error));
        }
    });
    Body::from_stream(futures_util::stream::unfold(
        receiver,
        |mut receiver| async { receiver.recv().await.map(|chunk| (chunk, receiver)) },
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn dos_time_of_a_known_instant() {
        // 2026-10-26T23:25:03Z: seconds round down to even.
        let t = DosTime::from_unix(1_793_057_103);
        assert_eq!(civil_from_days(1_793_057_103 / 86_400), (2026, 10, 26));
        assert_eq!(t.date, ((2026 - 1980) << 9) | (10 << 5) | 26);
        assert_eq!(t.time, (23 << 11) | (25 << 5) | 1);
        assert_eq!(DosTime::from_unix(0).date, (1 << 5) | 1);
    }

    #[test]
    fn a_standard_reader_reads_every_entry_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut written = Vec::new();
        let mut zip = ZipWriter::new(Vec::new());
        for (i, bytes) in [
            vec![],
            vec![7_u8; 3],
            (0..200_000_u32).map(|v| v as u8).collect(),
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("{i}.wav"));
            std::fs::write(&path, &bytes).unwrap();
            let name = format!("clip-{i}.wav");
            zip.add_file(
                &name,
                &mut File::open(&path).unwrap(),
                DosTime::from_unix(1_700_000_000),
            )
            .unwrap();
            written.push((name, bytes));
        }
        let archive = zip.finish().unwrap();
        let mut plan = ZipPlan::default();
        for (name, bytes) in &written {
            plan.add(name, bytes.len() as u64);
        }
        assert_eq!(plan.bytes + END_RECORD, archive.len() as u64);

        let mut reader = ::zip::ZipArchive::new(Cursor::new(archive)).unwrap();
        assert_eq!(reader.len(), written.len());
        for (index, (name, bytes)) in written.iter().enumerate() {
            let mut entry = reader.by_index(index).unwrap();
            assert_eq!(entry.name(), name);
            assert_eq!(entry.compression(), ::zip::CompressionMethod::Stored);
            let mut read = Vec::new();
            entry.read_to_end(&mut read).unwrap();
            assert_eq!(&read, bytes);
        }
    }

    #[test]
    fn plain_zip_limits_are_checked_before_writing() {
        let mut plan = ZipPlan::default();
        // Headers (30 + 46 + two names) and the end record (22) push it over.
        plan.add("a.wav", MAX_BYTES - 100);
        assert!(!plan.fits());
        let mut plan = ZipPlan::default();
        plan.add("a.wav", MAX_BYTES - 120);
        assert!(plan.fits());
        let mut plan = ZipPlan::default();
        for _ in 0..=MAX_ENTRIES {
            plan.add("a.wav", 0);
        }
        assert!(!plan.fits());
    }

    #[test]
    fn repeated_names_are_numbered_as_a_browser_does() {
        let mut taken = HashSet::new();
        assert_eq!(unique_name(&mut taken, "k-wake.wav"), "k-wake.wav");
        assert_eq!(unique_name(&mut taken, "k-wake.wav"), "k-wake (1).wav");
        assert_eq!(unique_name(&mut taken, "k-wake.wav"), "k-wake (2).wav");
        assert_eq!(unique_name(&mut taken, "noext"), "noext");
        assert_eq!(unique_name(&mut taken, "noext"), "noext (1)");
    }
}
