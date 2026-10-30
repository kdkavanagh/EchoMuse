//! A simulated device's audio side, for tests: what the firmware would upload for a
//! scripted stretch of capture and playback, as real EMA1 frames and message bodies
//! (docs/protocol-v1.md §3, §4.2, §4.4).
//!
//! The device captures continuously from sample 0 of one capture epoch; the reference
//! (what the device itself plays) runs on its own epoch, sample for sample with capture
//! and stamped with the same device clock, so an echo is the reference heard in the
//! mic. Audio leaves the device only as live packets or as a subscription's backfill
//! run, with one sequence counter per stream epoch, exactly as the firmware sends it.

use bytes::Bytes;
use echomuse_protocol::ema::{self, Cell, CellFlags, Flags, Header, Kind, SourceMask};
use serde_json::{Map, Value, json};

/// Capture samples per mic packet, the firmware's 80 ms block.
pub const BLOCK: u64 = 1_280;
/// Samples per loudness cell.
pub const CELL: u64 = 512;
/// Cell records per packet: a live packet carries at most one capture block's (the
/// firmware's `liveMaxCells`), a backfill packet up to the kind's limit (`MaxCells`).
const LIVE_CELLS: u64 = 2;
const BACKFILL_CELLS: u64 = 320;
const NS_PER_SAMPLE: u64 = 62_500;

/// One simulated device's capture and reference timelines.
#[derive(Clone, Debug)]
pub struct SimDevice {
    pub capture_epoch: u64,
    pub reference_epoch: u64,
    mic: Vec<i16>,
    reference: Vec<i16>,
    /// How far each stream has been sent live.
    live_mic: u64,
    live_cells: u64,
    live_reference: u64,
    seq_mic: u64,
    seq_cells: u64,
    seq_reference: u64,
    /// The device serves the `afe` stream (`afe_metadata_v1` with `session.ready`
    /// `afe_metadata`): one record per 80 ms capture period.
    afe: bool,
    live_afe: u64,
    seq_afe: u64,
}

impl SimDevice {
    pub fn new(capture_epoch: u64, reference_epoch: u64) -> Self {
        Self {
            capture_epoch,
            reference_epoch,
            mic: Vec::new(),
            reference: Vec::new(),
            live_mic: 0,
            live_cells: 0,
            live_reference: 0,
            seq_mic: 0,
            seq_cells: 0,
            seq_reference: 0,
            afe: false,
            live_afe: 0,
            seq_afe: 0,
        }
    }

    /// The same device serving the `afe` stream.
    pub fn with_afe(mut self) -> Self {
        self.afe = true;
        self
    }

    /// Whether it serves the `afe` stream.
    pub fn serves_afe(&self) -> bool {
        self.afe
    }

    /// The native AFE record the device reports for capture period `[from, from + BLOCK)`:
    /// ten valid frames, its VAD up with speech, its RMS from the period's loudness.
    pub fn afe_record(&self, from: u64) -> ema::AfeRecord {
        let pcm = &self.mic[from as usize..(from + BLOCK) as usize];
        let db = f64::from(loudness_centi(pcm)) / 100.0;
        let speech = db > -60.0;
        let rms = (db.round() as i64 + 256).clamp(1, 255) as u8;
        let playing = self.reference[from as usize..(from + BLOCK) as usize]
            .iter()
            .any(|s| *s != 0);
        ema::AfeRecord {
            frames: 10,
            playback: if playing { 10 } else { 0 },
            vad_max: if speech { 3 } else { 0 },
            vad_frames: if speech { 10 } else { 0 },
            rms_max: rms,
            rms_mean: rms,
            volume: 64,
            ..ema::AfeRecord::default()
        }
    }

    /// The `stream.open` bodies for mic, reference and cells.
    pub fn stream_opens(&self) -> Vec<Map<String, Value>> {
        [
            ("mic", self.capture_epoch, 1, 1),
            ("reference", self.reference_epoch, 2, 1),
            ("cells", self.capture_epoch, 4, 2),
            ("afe", self.capture_epoch, 5, 3),
        ]
        .into_iter()
        .filter(|(stream, ..)| self.afe || *stream != "afe")
        .map(|(stream, epoch, kind, format)| {
            json!({"stream_id": stream, "epoch": epoch.to_string(), "kind": kind,
                   "sample_rate": 16000, "format": format, "reason": "start"})
            .as_object()
            .expect("an object")
            .clone()
        })
        .collect()
    }

    /// Captured samples so far.
    pub fn captured(&self) -> u64 {
        self.mic.len() as u64
    }

    /// The mic it captured, from its first sample.
    pub fn mic(&self) -> &[i16] {
        &self.mic
    }

    /// The room says `pcm` while the device plays nothing.
    pub fn hear(&mut self, pcm: &[i16]) {
        self.mic.extend_from_slice(pcm);
        self.reference.extend(std::iter::repeat_n(0, pcm.len()));
    }

    /// The device plays `pcm` and hears it back (echo only, no near-end speech).
    pub fn play(&mut self, pcm: &[i16]) {
        self.mic.extend_from_slice(pcm);
        self.reference.extend_from_slice(pcm);
    }

    /// Moves every stream's live position to what was captured without sending it, as
    /// the firmware does while no subscription wants a stream: nothing is uploaded then,
    /// and a later subscription's backfill covers what it asks for.
    pub fn skip_live(&mut self) {
        let end = self.captured();
        self.live_mic = end;
        self.live_cells = end / CELL * CELL;
        self.live_reference = end;
        self.live_afe = end / BLOCK * BLOCK;
    }

    /// The live packets of every stream up to what was captured, in sending order.
    pub fn live(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        let end = self.captured();
        while self.live_mic < end {
            let from = self.live_mic;
            let to = (from + BLOCK).min(end);
            out.push(self.mic_packet(from, to));
            self.live_mic = to;
            let cells_end = to / CELL * CELL;
            while self.live_cells < cells_end {
                let first = self.live_cells;
                let last = (first + LIVE_CELLS * CELL).min(cells_end);
                out.push(self.cells_packet(first, last));
                self.live_cells = last;
            }
            while self.live_reference < to {
                let first = self.live_reference;
                let end = to;
                out.push(self.reference_packet(first, end));
                self.live_reference = end;
            }
            while self.afe && self.live_afe + BLOCK <= to {
                let first = self.live_afe;
                out.push(self.afe_packet(first, first + BLOCK));
                self.live_afe = first + BLOCK;
            }
        }
        out
    }

    /// A subscription's backfill run of `stream` from `from` up to its live position.
    pub fn backfill(&mut self, stream: &str, from: u64) -> Vec<Bytes> {
        let mut out = Vec::new();
        match stream {
            "mic" => {
                let mut at = from;
                while at < self.live_mic {
                    let to = (at + BLOCK).min(self.live_mic);
                    out.push(self.mic_packet(at, to));
                    at = to;
                }
            }
            "cells" => {
                let mut at = from / CELL * CELL;
                while at < self.live_cells {
                    let to = (at + BACKFILL_CELLS * CELL).min(self.live_cells);
                    out.push(self.cells_packet(at, to));
                    at = to;
                }
            }
            "reference" => {
                let mut at = from;
                while at < self.live_reference {
                    let to = (at + BLOCK).min(self.live_reference);
                    out.push(self.reference_packet(at, to));
                    at = to;
                }
            }
            "afe" if self.afe => {
                let mut at = from / BLOCK * BLOCK;
                while at < self.live_afe {
                    let to = (at + 125 * BLOCK).min(self.live_afe);
                    out.push(self.afe_packet(at, to));
                    at = to;
                }
            }
            _ => {}
        }
        out
    }

    fn header(&self, kind: Kind, epoch: u64, sequence: u64, first: u64, frames: u64) -> Header {
        Header {
            kind,
            flags: Flags::NONE,
            epoch,
            sequence,
            first_sample: first,
            mono_ns: first * NS_PER_SAMPLE,
            uncertainty_us: None,
            frame_count: frames as u32,
            generation: 0,
            source_mask: SourceMask::NONE,
        }
    }

    fn mic_packet(&mut self, from: u64, to: u64) -> Bytes {
        let payload: Vec<u8> = self.mic[from as usize..to as usize]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let header = self.header(Kind::Mic, self.capture_epoch, self.seq_mic, from, to - from);
        self.seq_mic += 1;
        ema::encode(&header, &payload).expect("a valid mic frame")
    }

    fn reference_packet(&mut self, from: u64, to: u64) -> Bytes {
        let slice = &self.reference[from as usize..to as usize];
        let silent = slice.iter().all(|s| *s == 0);
        let mut header = self.header(
            Kind::Reference,
            self.reference_epoch,
            self.seq_reference,
            from,
            to - from,
        );
        self.seq_reference += 1;
        if silent {
            header.flags = Flags::DIGITAL_SILENCE;
            return ema::encode(&header, &[]).expect("a valid silent reference frame");
        }
        header.source_mask = SourceMask::DIALOG;
        let payload: Vec<u8> = slice.iter().flat_map(|s| s.to_le_bytes()).collect();
        ema::encode(&header, &payload).expect("a valid reference frame")
    }

    /// One packet of the cell records of `[from, to)`, whole cells.
    fn cells_packet(&mut self, from: u64, to: u64) -> Bytes {
        let mut payload = Vec::new();
        let mut at = from;
        while at < to {
            let span = at as usize..(at + CELL) as usize;
            let mask = if self.reference[span.clone()].iter().any(|s| *s != 0) {
                SourceMask::DIALOG
            } else {
                SourceMask::NONE
            };
            let cell = Cell {
                e_centi: loudness_centi(&self.mic[span]),
                flags: CellFlags::NONE,
                mask,
            };
            payload.extend_from_slice(&cell.to_bytes());
            at += CELL;
        }
        let cells = (to - from) / CELL;
        let header = self.header(Kind::Cells, self.capture_epoch, self.seq_cells, from, cells);
        self.seq_cells += 1;
        ema::encode(&header, &payload).expect("a valid cell frame")
    }

    /// One packet of the AFE records of the whole periods `[from, to)`.
    fn afe_packet(&mut self, from: u64, to: u64) -> Bytes {
        let mut payload = Vec::new();
        let mut at = from;
        while at < to {
            payload.extend_from_slice(&self.afe_record(at).to_bytes());
            at += BLOCK;
        }
        let periods = (to - from) / BLOCK;
        let header = self.header(Kind::Afe, self.capture_epoch, self.seq_afe, from, periods);
        self.seq_afe += 1;
        ema::encode(&header, &payload).expect("a valid afe frame")
    }

    /// A `wake.candidate` body for a wake word supported from `support_start` whose last
    /// qualifying hop ends at `open`.
    pub fn candidate(
        &self,
        candidate_id: &str,
        lease_id: &str,
        support_start: u64,
        open: u64,
        producing_sound: bool,
    ) -> Map<String, Value> {
        json!({
            "candidate_id": candidate_id, "lease_id": lease_id,
            "capture_epoch": self.capture_epoch.to_string(),
            "graph_sha256": null, "scorer_revision": 3,
            "profile": if producing_sound { "playback" } else { "idle" },
            "threshold": 0.9, "producing_sound": producing_sound, "chimed": false,
            "first_crossing_end": open.to_string(), "support_start": support_start.to_string(),
            "mono_ns": (open * NS_PER_SAMPLE).to_string(), "active_alert": null,
            "hops": [{"end_sample": open.to_string(), "raw": 0.95, "smoothed": 0.93,
                      "profile": if producing_sound { "playback" } else { "idle" }}],
        })
        .as_object()
        .expect("an object")
        .clone()
    }

    /// A `button.action` body for a tap released at capture sample `press`.
    pub fn tap(&self, press: u64, physical_seq: u64) -> Map<String, Value> {
        json!({
            "click_type": 138, "button": "Dot", "down": false, "held_ms": 120, "muted": false,
            "mono_ns": (press * NS_PER_SAMPLE).to_string(),
            "capture_epoch": self.capture_epoch.to_string(),
            "capture_sample": press.to_string(), "physical_seq": physical_seq,
            "occurrence_id": null, "handled": null,
        })
        .as_object()
        .expect("an object")
        .clone()
    }
}

/// The firmware's cell loudness: 10·log10(mean square) in hundredths of a dB, floored.
fn loudness_centi(pcm: &[i16]) -> i16 {
    echomuse_turn::DeviceCell::loudness_centi(pcm)
}
