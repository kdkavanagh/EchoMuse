//! Reading a session file back.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use bytes::{Buf, Bytes};
use serde::{Deserialize, Serialize};

use crate::format::{
    FORMAT_VERSION, MAX_RECORD_BYTES, PREAMBLE_BYTES, PreambleError, RecordType, TailError,
    check_preamble, next_record,
};
use crate::record::{
    End, EndReason, Entry, Event, FrameEntry, Header, IngressOrdinal, Marker, RelTime, StreamName,
    Tally,
};

/// Why a file could not be read at all.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The file could not be read.
    #[error("could not read the recording: {0}")]
    Io(#[from] io::Error),
    /// The file does not start with the session-recording preamble.
    #[error("not a session recording")]
    NotARecording,
    /// The file is a session recording of another format version.
    #[error("format version {found} is not supported (this reader reads version {supported})")]
    UnsupportedVersion {
        /// The file's version.
        found: u16,
        /// The version this reader reads.
        supported: u16,
    },
    /// The first record is missing, damaged or not a header.
    #[error("the recording has no readable header: {0}")]
    BadHeader(String),
}

impl From<PreambleError> for ReadError {
    fn from(e: PreambleError) -> Self {
        match e {
            PreambleError::NotARecording => Self::NotARecording,
            PreambleError::UnsupportedVersion(found) => Self::UnsupportedVersion {
                found,
                supported: FORMAT_VERSION,
            },
        }
    }
}

/// One item after the header, in file order.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordedItem {
    /// An observed event.
    Event(Box<Entry>),
    /// An EMA1 frame.
    Frame(FrameEntry),
    /// The writer's note of missing entries.
    Marker(Marker),
}

impl RecordedItem {
    /// The runtime's sequence number, for events and frames.
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Event(e) => Some(e.seq),
            Self::Frame(f) => Some(f.seq),
            Self::Marker(_) => None,
        }
    }
}

/// How the file ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tail {
    /// With its end record and nothing after it.
    Ended,
    /// Without an end record: still being written, or cut short by a crash or a failed write.
    Unended,
    /// With a damaged record; nothing after it was read.
    Corrupt,
}

/// Something a recording does not hold that a full recording would.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "limitation", rename_all = "snake_case")]
pub enum Limitation {
    /// The file has no end record.
    NotEnded,
    /// The file has a damaged record.
    CorruptTail,
    /// Entries were dropped because the write queue was full.
    EntriesLost {
        /// How many.
        entries: u64,
    },
    /// Entries other than audio were discarded unwritten when the grant was withdrawn.
    EventsDiscarded {
        /// How many.
        events: u64,
    },
    /// Records of a kind this reader does not know.
    UnreadableEntries {
        /// How many.
        entries: u64,
    },
    /// The grant did not cover transcripts.
    TranscriptsWithheld,
    /// The grant did not cover these streams.
    StreamsWithheld {
        /// Which.
        streams: Vec<StreamName>,
    },
    /// Audio frames taken but discarded unwritten on mute or withdrawal.
    FramesDiscarded {
        /// How many.
        frames: u64,
    },
    /// Audio frames not taken while the device was muted or capture not permitted.
    AudioWithheld {
        /// How many.
        frames: u64,
    },
    /// Credential values replaced in HA bodies.
    CredentialsRedacted {
        /// How many.
        values: u64,
    },
    /// The replay had to use a recorded echo result where it was asked to recompute one.
    EchoNotRecomputed {
        /// Cells and candidate comparisons that fell back to the recorded value.
        results: u64,
    },
}

impl Limitation {
    /// Whether this can change what a decision replay concludes. Decision replay reads the
    /// recorded decision evidence, transcripts and loudness cells; it never reads HA bodies,
    /// and reads audio frames only to recompute echo labels.
    pub fn affects_decision_replay(&self) -> bool {
        match self {
            Self::NotEnded
            | Self::CorruptTail
            | Self::EntriesLost { .. }
            | Self::EventsDiscarded { .. }
            | Self::UnreadableEntries { .. }
            | Self::TranscriptsWithheld
            | Self::EchoNotRecomputed { .. } => true,
            Self::StreamsWithheld { streams } => streams.contains(&StreamName::Cells),
            Self::FramesDiscarded { .. }
            | Self::AudioWithheld { .. }
            | Self::CredentialsRedacted { .. } => false,
        }
    }
}

/// What the file says about its own completeness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integrity {
    /// How the file ends.
    pub tail: Tail,
    /// Why the recording ended, when it ended in order.
    pub end: Option<EndReason>,
    /// The totals: from the end record, or summed from the markers when there is none.
    pub tally: Tally,
    /// Sequence numbers assigned but absent from the file.
    pub missing_seqs: u64,
    /// Records this reader could not interpret.
    pub unreadable: u64,
    /// The streams the grant left out.
    pub streams_withheld: Vec<StreamName>,
    /// Whether the grant left out transcripts.
    pub transcripts_withheld: bool,
}

impl Integrity {
    /// Everything the recording lacks, in a fixed order.
    pub fn limitations(&self) -> Vec<Limitation> {
        let mut out = Vec::new();
        match self.tail {
            Tail::Ended => {}
            Tail::Unended => out.push(Limitation::NotEnded),
            Tail::Corrupt => out.push(Limitation::CorruptTail),
        }
        let lost = self.tally.lost_entries.max(
            self.missing_seqs
                .saturating_sub(self.tally.discarded_frames + self.tally.discarded_events),
        );
        if lost > 0 {
            out.push(Limitation::EntriesLost { entries: lost });
        }
        if self.tally.discarded_events > 0 {
            out.push(Limitation::EventsDiscarded {
                events: self.tally.discarded_events,
            });
        }
        if self.unreadable > 0 {
            out.push(Limitation::UnreadableEntries {
                entries: self.unreadable,
            });
        }
        if self.transcripts_withheld {
            out.push(Limitation::TranscriptsWithheld);
        }
        if !self.streams_withheld.is_empty() {
            out.push(Limitation::StreamsWithheld {
                streams: self.streams_withheld.clone(),
            });
        }
        if self.tally.discarded_frames > 0 {
            out.push(Limitation::FramesDiscarded {
                frames: self.tally.discarded_frames,
            });
        }
        if self.tally.withheld_frames > 0 {
            out.push(Limitation::AudioWithheld {
                frames: self.tally.withheld_frames,
            });
        }
        if self.tally.credential_redactions > 0 {
            out.push(Limitation::CredentialsRedacted {
                values: self.tally.credential_redactions,
            });
        }
        out
    }

    /// Whether the file holds everything the session produced under its grant: it ended in
    /// order and nothing was lost, discarded or unreadable. Scope choices and credential
    /// redaction are reported by [`Integrity::is_redacted`] instead.
    pub fn is_complete(&self) -> bool {
        self.tail == Tail::Ended
            && self.tally.lost_entries == 0
            && self.tally.discarded_events == 0
            && self.tally.discarded_frames == 0
            && self.missing_seqs == 0
            && self.unreadable == 0
    }

    /// Whether anything was withheld or redacted: streams or transcripts outside the grant,
    /// audio withheld while muted, or credentials.
    pub fn is_redacted(&self) -> bool {
        self.transcripts_withheld
            || !self.streams_withheld.is_empty()
            || self.tally.withheld_frames > 0
            || self.tally.credential_redactions > 0
    }
}

/// A whole session file in memory. Frames are slices of the file's bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    /// The header.
    pub header: Header,
    /// Everything after it, in file order.
    pub items: Vec<RecordedItem>,
    /// The end record, when the file has one.
    pub end: Option<End>,
    /// What the file lacks.
    pub integrity: Integrity,
}

impl Recording {
    /// Reads a file.
    pub fn read(path: &Path) -> Result<Self, ReadError> {
        Self::from_bytes(Bytes::from(std::fs::read(path)?))
    }

    /// Reads a file's bytes.
    pub fn from_bytes(bytes: Bytes) -> Result<Self, ReadError> {
        check_preamble(&bytes)?;
        let mut rest = bytes.slice(PREAMBLE_BYTES..);
        let header = match next_record(&mut rest) {
            Some(Ok(r)) if r.ty == RecordType::Header => {
                serde_json::from_slice::<Header>(&r.payload)
                    .map_err(|e| ReadError::BadHeader(e.to_string()))?
            }
            Some(Ok(_)) => return Err(ReadError::BadHeader("first record is not a header".into())),
            Some(Err(e)) => return Err(ReadError::BadHeader(format!("{e:?}"))),
            None => return Err(ReadError::BadHeader("no records".into())),
        };
        let mut items = Vec::new();
        let mut end = None;
        let mut tail = Tail::Unended;
        let mut unreadable = 0;
        let mut marked = Tally::default();
        while let Some(record) = next_record(&mut rest) {
            let record = match record {
                Ok(r) => r,
                Err(TailError::Truncated) => {
                    tail = Tail::Unended;
                    break;
                }
                Err(TailError::Corrupt) => {
                    tail = Tail::Corrupt;
                    break;
                }
            };
            if end.is_some() {
                // Nothing may follow the end record.
                tail = Tail::Corrupt;
                break;
            }
            match record.ty {
                RecordType::Event => match serde_json::from_slice::<Entry>(&record.payload) {
                    Ok(e) => items.push(RecordedItem::Event(Box::new(e))),
                    Err(_) => unreadable += 1,
                },
                RecordType::Frame => {
                    let mut p = record.payload;
                    if p.len() < 24 {
                        unreadable += 1;
                        continue;
                    }
                    let seq = p.get_u64_le();
                    let t = p.get_u64_le();
                    let ingress = p.get_u64_le();
                    items.push(RecordedItem::Frame(FrameEntry {
                        seq,
                        t: RelTime(t),
                        ingress: IngressOrdinal(ingress),
                        frame: p,
                    }));
                }
                RecordType::Marker => match serde_json::from_slice::<Marker>(&record.payload) {
                    Ok(m) => {
                        match &m {
                            Marker::Lost { entries } => marked.lost_entries += entries,
                            Marker::Discarded { frames, events } => {
                                marked.discarded_frames += frames;
                                marked.discarded_events += events;
                            }
                        }
                        items.push(RecordedItem::Marker(m));
                    }
                    Err(_) => unreadable += 1,
                },
                RecordType::End => match serde_json::from_slice::<End>(&record.payload) {
                    Ok(e) => {
                        end = Some(e);
                        tail = Tail::Ended;
                    }
                    Err(_) => {
                        tail = Tail::Corrupt;
                        break;
                    }
                },
                RecordType::Header => {
                    tail = Tail::Corrupt;
                    break;
                }
            }
        }
        let tally = match &end {
            Some(e) => e.tally,
            None => {
                let mut t = marked;
                for i in &items {
                    let RecordedItem::Event(e) = i else { continue };
                    match &e.event {
                        Event::AudioWithheld { frames, .. } => t.withheld_frames += frames,
                        Event::Ha(h) => t.credential_redactions += u64::from(h.redactions()),
                        _ => {}
                    }
                }
                t
            }
        };
        let missing_seqs = missing_seqs(&items, end.as_ref().and_then(|e| e.last_seq));
        let integrity = Integrity {
            tail,
            end: end.as_ref().map(|e| e.reason),
            tally,
            missing_seqs,
            unreadable,
            streams_withheld: header.grant.streams.withheld_streams(),
            transcripts_withheld: !header.grant.streams.transcripts,
        };
        Ok(Self {
            header,
            items,
            end,
            integrity,
        })
    }

    /// The events, in order.
    pub fn events(&self) -> impl Iterator<Item = &Entry> {
        self.items.iter().filter_map(|i| match i {
            RecordedItem::Event(e) => Some(&**e),
            _ => None,
        })
    }

    /// The frames, in order.
    pub fn frames(&self) -> impl Iterator<Item = &FrameEntry> {
        self.items.iter().filter_map(|i| match i {
            RecordedItem::Frame(f) => Some(f),
            _ => None,
        })
    }
}

fn missing_seqs(items: &[RecordedItem], last: Option<u64>) -> u64 {
    let mut seqs: Vec<u64> = items.iter().filter_map(RecordedItem::seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    let top = match (seqs.last().copied(), last) {
        (Some(a), Some(b)) => a.max(b),
        (a, b) => match a.or(b) {
            Some(t) => t,
            None => return 0,
        },
    };
    (top + 1).saturating_sub(seqs.len() as u64)
}

/// Reads only a file's header, for listing.
pub fn read_header(path: &Path) -> Result<Header, ReadError> {
    let mut f = File::open(path)?;
    let mut pre = [0u8; PREAMBLE_BYTES + 4];
    f.read_exact(&mut pre)?;
    check_preamble(&pre)?;
    let len = u32::from_le_bytes([pre[16], pre[17], pre[18], pre[19]]);
    if len == 0 || len > MAX_RECORD_BYTES {
        return Err(ReadError::BadHeader("bad record length".into()));
    }
    let mut rest = vec![0u8; len as usize + 4];
    f.read_exact(&mut rest)?;
    let mut all = pre.to_vec();
    all.extend_from_slice(&rest);
    let mut bytes = Bytes::from(all).slice(PREAMBLE_BYTES..);
    match next_record(&mut bytes) {
        Some(Ok(r)) if r.ty == RecordType::Header => {
            serde_json::from_slice(&r.payload).map_err(|e| ReadError::BadHeader(e.to_string()))
        }
        _ => Err(ReadError::BadHeader("first record is not a header".into())),
    }
}
