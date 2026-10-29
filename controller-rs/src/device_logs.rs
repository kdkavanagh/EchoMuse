//! The device's `log` lines (spec §4.10 "per-device logs"; protocol-v1.md §4.8):
//! each line is stamped with the controller's receive time and stored in `device_logs`
//! through the records service, which keeps the device's newest 10,000 lines.
//!
//! The firmware sends one `{level, message}` line about every five minutes, but nothing
//! on this side trusts that: a device runtime hands lines over without waiting
//! ([`LogSink::offer`]), each device may store at most [`BURST`] lines at once and then
//! one per [`REFILL`], a line longer than [`MAX_LINE_BYTES`] is cut, and the queue to the
//! writer is bounded. Lines refused by any of these are counted per device and the count
//! is written as one controller line once lines are accepted again, so the log says
//! what it is missing.
//!
//! The dashboard reads the log with `GET /api/devices/{id}/logs`; it does not read
//! live `device_log` events, so none are published.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use echomuse_core::types::DeviceId;
use echomuse_records::{
    DeviceLogEntry, RecordsService, UnixMillis,
    vocab::{LogLevel, LogSource},
};
use serde_json::{Map, Value};
use tokio::{sync::mpsc, time::Instant};

use crate::supervision::TaskSupervisor;

/// Lines one device may store at once.
pub const BURST: u32 = 20;
/// How often a device earns one more line.
pub const REFILL: Duration = Duration::from_secs(3);
/// The longest line kept, in bytes; longer lines are cut at a character boundary.
pub const MAX_LINE_BYTES: usize = 2_000;
/// Lines waiting for the writer, across devices.
pub const QUEUE: usize = 256;

/// One device's allowance.
struct Bucket {
    tokens: u32,
    refilled: Instant,
    refused: u64,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: BURST,
            refilled: now,
            refused: 0,
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        let earned = now.saturating_duration_since(self.refilled).as_millis() / REFILL.as_millis();
        if earned > 0 {
            self.tokens = BURST.min(self.tokens.saturating_add(earned as u32));
            self.refilled += REFILL * earned as u32;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// Hands device log lines to the records service. Cheap to clone.
#[derive(Clone)]
pub struct LogSink {
    queue: mpsc::Sender<DeviceLogEntry>,
    buckets: Arc<Mutex<HashMap<DeviceId, Bucket>>>,
}

impl LogSink {
    /// Starts the writer on the shared runtime.
    pub fn start(records: RecordsService, tasks: &TaskSupervisor) -> Self {
        let (queue, mut lines) = mpsc::channel::<DeviceLogEntry>(QUEUE);
        tasks.spawn("device logs", async move {
            while let Some(line) = lines.recv().await {
                let device = line.device.clone();
                if let Err(error) = records.log(line).await {
                    tracing::warn!(%device, %error, "a device log line was not stored");
                }
            }
        });
        Self {
            queue,
            buckets: Arc::default(),
        }
    }

    /// A `log` body from `device`. Never waits; a line it cannot take is counted.
    pub fn offer(&self, device: &DeviceId, body: &Map<String, Value>) {
        let level = match body.get("level").and_then(Value::as_str) {
            Some("warn") | Some("warning") => LogLevel::Warn,
            Some("error") => LogLevel::Error,
            _ => LogLevel::Info,
        };
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .map(cut)
            .unwrap_or_else(|| "(a log message without text)".to_owned());
        self.line(device, level, LogSource::Device, message);
    }

    /// A line the controller writes about `device`.
    pub fn controller_line(&self, device: &DeviceId, level: LogLevel, message: String) {
        self.line(device, level, LogSource::Controller, message);
    }

    fn line(&self, device: &DeviceId, level: LogLevel, source: LogSource, message: String) {
        let now = Instant::now();
        let Some(ts) = UnixMillis::from_system_time(SystemTime::now()) else {
            return;
        };
        let refused = {
            let mut buckets = self.buckets.lock().expect("log buckets lock");
            let bucket = buckets
                .entry(device.clone())
                .or_insert_with(|| Bucket::new(now));
            if !bucket.take(now) {
                bucket.refused += 1;
                return;
            }
            std::mem::take(&mut bucket.refused)
        };
        if refused > 0 {
            let note = DeviceLogEntry {
                device: device.clone(),
                ts,
                level: LogLevel::Warn,
                source: LogSource::Controller,
                message: format!("{refused} device log lines were not stored (rate limit)"),
            };
            let _ = self.queue.try_send(note);
        }
        let entry = DeviceLogEntry {
            device: device.clone(),
            ts,
            level,
            source,
            message,
        };
        if self.queue.try_send(entry).is_err() {
            let mut buckets = self.buckets.lock().expect("log buckets lock");
            if let Some(bucket) = buckets.get_mut(device) {
                bucket.refused += 1;
            }
        }
    }

    /// Forgets a deleted device's allowance.
    pub fn forget(&self, device: &DeviceId) {
        self.buckets
            .lock()
            .expect("log buckets lock")
            .remove(device);
    }
}

fn cut(text: &str) -> String {
    if text.len() <= MAX_LINE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_allowance_refills_over_time() {
        let mut bucket = Bucket::new(Instant::now());
        for _ in 0..BURST {
            assert!(bucket.take(Instant::now()));
        }
        assert!(!bucket.take(Instant::now()));
        tokio::time::advance(REFILL).await;
        assert!(bucket.take(Instant::now()));
        assert!(!bucket.take(Instant::now()));
    }

    #[test]
    fn long_lines_are_cut_at_a_character() {
        let line = "é".repeat(MAX_LINE_BYTES);
        let kept = cut(&line);
        assert!(kept.len() <= MAX_LINE_BYTES + "…".len());
        assert!(kept.ends_with('…'));
        assert_eq!(cut("short"), "short");
    }
}
