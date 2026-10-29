//! The hourly rollups: `device_metrics` from the device's `stats` reports and the
//! controller's link round trips, `wake_counters` and `wake_shadow` from `wake.stats`, and
//! `wake_shadow_events` for each resolved shadow episode.
//!
//! Every rollup row is keyed by device and `hour_ts`, the UTC hour's first second
//! ([`HourStart`]), and each report adds into its hour's row. Counters and sums start at 0;
//! a nullable column stays NULL until a report carries its value, and a report without a
//! value (the wire's null or an omitted field) leaves it as it was, never adding a 0.
//!
//! # `device_metrics`
//!
//! | Column | From |
//! |---|---|
//! | `samples` | count of `stats` reports |
//! | `cpu_sum`, `cpu_max` | `cpuPct` |
//! | `mem_used_sum` | `memUsedMb` |
//! | `mem_total_mb`, `storage_used_mb`, `storage_total_mb` | the latest `memTotalMb`, `storageUsedMb`, `storageTotalMb` |
//! | `rssi_sum`, `rssi_samples`, `rssi_min` | `wifiRssi` (dBm) when reported |
//! | `link_speed_last`, `link_speed_min` | `linkSpeedMbps` when reported |
//! | `wifi_freq_last`, `wifi_bssid_last` | `wifiFreqMhz`, `wifiBssid` when reported |
//! | `tx_bytes_sum`, `rx_bytes_sum`, `tx_errors_sum`, `tx_dropped_sum`, `rx_crc_sum` | increases of the device's cumulative counters since its previous report ([`NetCounters`]) |
//! | `cpu_temp_sum`, `cpu_temp_samples`, `cpu_temp_max` | `cpuTempC` when reported |
//! | `max_temp_max` | `maxTempC` when reported |
//! | `cores_online_last`, `cores_online_min`, `cores_total` | `coresOnline`, `coresTotal` when reported |
//! | `thermal_limit_min` | `thermalCoreLimit` when reported |
//! | `rtt_sum_ms`, `rtt_samples`, `rtt_min_ms`, `rtt_max_ms` | each link round trip, whole ms |
//! | `rtt_excursions` | round trips of 200 ms or more |
//! | `rtt_samples_idle`, `rtt_excursions_idle` | the same, counted only while the device was idle |
//!
//! The 200 ms excursion bound is read from the deployed rows: every hour with an excursion
//! has `rtt_max_ms` ≥ 200, and every hour without one has it ≤ 199.
//!
//! # `wake_counters`
//!
//! Filled from `wake.stats` (post-AFE architecture §18.4 step 4): `near_misses` and
//! `near_miss_max` from the reported near-miss episodes, `dev_hops` from `hops_scored`,
//! `dev_drops` from `hops_dropped` (hops lost to capture overrun), `dev_crossings` from
//! `candidates_opened`, `dev_max_score` from `peak_smoothed` and `dev_max_infer_ms` from
//! `infer_max_ms` rounded to a whole millisecond. `underruns`, `dev_frames` and
//! `dev_max_gap_ms` are no longer written and stay 0 in new rows.
//!
//! # `wake_shadow` and `wake_shadow_events`
//!
//! One row per hour and shadow rule, keyed by [`rule_key`]: the counters of each report's
//! entry summed, and `lead_hist` summed bucket by bucket, stored as JSON with a space after
//! each comma (`[0, 0, 0, 1, 0, 0, 0]`) as every existing row is. Each event is one
//! `wake_shadow_events` row with `raws` in the same JSON style. A report without `shadow`
//! (older firmware) means no data and writes nothing.

use std::time::Duration;

use echomuse_core::types::DeviceId;
use echomuse_db::WriteCommand;
use echomuse_protocol::session::OpenRule;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::{
    error::RecordError,
    values::{HourStart, WallSeconds, whole_ms},
    vocab::ShadowEventKind,
};

/// A round trip of at least this long is an excursion.
pub const RTT_EXCURSION: Duration = Duration::from_millis(200);
/// Buckets of a shadow rule's lead histogram (lead −3..+3 hops).
pub const LEAD_BUCKETS: usize = 7;

/// The increases of the device's interface counters over one report interval.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetDelta {
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Bytes received.
    pub rx_bytes: u64,
    /// Send errors.
    pub tx_errors: u64,
    /// Sends dropped.
    pub tx_dropped: u64,
    /// Received CRC errors.
    pub rx_crc: u64,
}

/// The cumulative counters a `stats` report carries (`txBytes`, `rxBytes`, `txErrors`,
/// `txDropped`, `rxCrcErrors`).
pub type NetTotals = NetDelta;

/// Turns the device's cumulative counters into per-interval increases. The device runtime
/// owns one per device. The first report, and any report in which a counter went backwards
/// (the device restarted or the interface was reset), has no known increase.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetCounters {
    last: Option<NetTotals>,
}

impl NetCounters {
    /// Takes a report's totals and returns the increase since the previous report.
    pub fn observe(&mut self, now: NetTotals) -> Option<NetDelta> {
        let previous = self.last.replace(now)?;
        Some(NetDelta {
            tx_bytes: now.tx_bytes.checked_sub(previous.tx_bytes)?,
            rx_bytes: now.rx_bytes.checked_sub(previous.rx_bytes)?,
            tx_errors: now.tx_errors.checked_sub(previous.tx_errors)?,
            tx_dropped: now.tx_dropped.checked_sub(previous.tx_dropped)?,
            rx_crc: now.rx_crc.checked_sub(previous.rx_crc)?,
        })
    }
}

/// One `stats` report, with each field the wire may leave out or null as an Option.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatsReport {
    /// `cpuPct`.
    pub cpu_pct: f64,
    /// `memUsedMb`.
    pub mem_used_mb: f64,
    /// `memTotalMb`.
    pub mem_total_mb: Option<f64>,
    /// `storageUsedMb`.
    pub storage_used_mb: Option<f64>,
    /// `storageTotalMb`.
    pub storage_total_mb: Option<f64>,
    /// `wifiRssi`.
    pub wifi_rssi: Option<f64>,
    /// `linkSpeedMbps`.
    pub link_speed_mbps: Option<i64>,
    /// `wifiFreqMhz`.
    pub wifi_freq_mhz: Option<i64>,
    /// `wifiBssid`.
    pub wifi_bssid: Option<String>,
    /// The counters' increase since the previous report.
    pub net: Option<NetDelta>,
    /// `cpuTempC`.
    pub cpu_temp_c: Option<f64>,
    /// `maxTempC`.
    pub max_temp_c: Option<f64>,
    /// `coresOnline`.
    pub cores_online: Option<i64>,
    /// `coresTotal`.
    pub cores_total: Option<i64>,
    /// `thermalCoreLimit`.
    pub thermal_core_limit: Option<i64>,
}

fn sum(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Adds one `stats` report to its hour.
#[derive(Clone, Debug)]
pub struct RecordStats {
    /// The device.
    pub device: DeviceId,
    /// When the report arrived.
    pub at: WallSeconds,
    /// The report.
    pub report: StatsReport,
}

impl WriteCommand for RecordStats {
    type Output = HourStart;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<HourStart, RecordError> {
        let hour = self.at.hour();
        let r = &self.report;
        let net = r.net.unwrap_or_default();
        tx.execute(
            "INSERT INTO device_metrics (device_id, hour_ts, samples, cpu_sum, cpu_max, \
             mem_used_sum, mem_total_mb, storage_used_mb, storage_total_mb, rssi_sum, \
             rssi_samples, rssi_min, link_speed_last, link_speed_min, wifi_freq_last, \
             wifi_bssid_last, tx_bytes_sum, rx_bytes_sum, tx_errors_sum, tx_dropped_sum, \
             rx_crc_sum, cpu_temp_sum, cpu_temp_samples, cpu_temp_max, max_temp_max, \
             cores_online_last, cores_online_min, cores_total, thermal_limit_min) \
             VALUES (?1, ?2, 1, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13, ?14, \
             ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?23, ?24, ?25) \
             ON CONFLICT (device_id, hour_ts) DO UPDATE SET \
             samples = samples + 1, \
             cpu_sum = cpu_sum + excluded.cpu_sum, \
             cpu_max = max(cpu_max, excluded.cpu_max), \
             mem_used_sum = mem_used_sum + excluded.mem_used_sum, \
             mem_total_mb = coalesce(excluded.mem_total_mb, mem_total_mb), \
             storage_used_mb = coalesce(excluded.storage_used_mb, storage_used_mb), \
             storage_total_mb = coalesce(excluded.storage_total_mb, storage_total_mb), \
             rssi_sum = rssi_sum + excluded.rssi_sum, \
             rssi_samples = rssi_samples + excluded.rssi_samples, \
             rssi_min = min(coalesce(rssi_min, excluded.rssi_min), \
                            coalesce(excluded.rssi_min, rssi_min)), \
             link_speed_last = coalesce(excluded.link_speed_last, link_speed_last), \
             link_speed_min = min(coalesce(link_speed_min, excluded.link_speed_min), \
                                  coalesce(excluded.link_speed_min, link_speed_min)), \
             wifi_freq_last = coalesce(excluded.wifi_freq_last, wifi_freq_last), \
             wifi_bssid_last = coalesce(excluded.wifi_bssid_last, wifi_bssid_last), \
             tx_bytes_sum = tx_bytes_sum + excluded.tx_bytes_sum, \
             rx_bytes_sum = rx_bytes_sum + excluded.rx_bytes_sum, \
             tx_errors_sum = tx_errors_sum + excluded.tx_errors_sum, \
             tx_dropped_sum = tx_dropped_sum + excluded.tx_dropped_sum, \
             rx_crc_sum = rx_crc_sum + excluded.rx_crc_sum, \
             cpu_temp_sum = cpu_temp_sum + excluded.cpu_temp_sum, \
             cpu_temp_samples = cpu_temp_samples + excluded.cpu_temp_samples, \
             cpu_temp_max = max(coalesce(cpu_temp_max, excluded.cpu_temp_max), \
                                coalesce(excluded.cpu_temp_max, cpu_temp_max)), \
             max_temp_max = max(coalesce(max_temp_max, excluded.max_temp_max), \
                                coalesce(excluded.max_temp_max, max_temp_max)), \
             cores_online_last = coalesce(excluded.cores_online_last, cores_online_last), \
             cores_online_min = min(coalesce(cores_online_min, excluded.cores_online_min), \
                                    coalesce(excluded.cores_online_min, cores_online_min)), \
             cores_total = coalesce(excluded.cores_total, cores_total), \
             thermal_limit_min = min(coalesce(thermal_limit_min, excluded.thermal_limit_min), \
                                     coalesce(excluded.thermal_limit_min, thermal_limit_min))",
            params![
                self.device.0,
                hour.get(),
                r.cpu_pct,
                r.mem_used_mb,
                r.mem_total_mb,
                r.storage_used_mb,
                r.storage_total_mb,
                r.wifi_rssi.unwrap_or(0.0),
                i64::from(r.wifi_rssi.is_some()),
                r.wifi_rssi,
                r.link_speed_mbps,
                r.wifi_freq_mhz,
                r.wifi_bssid,
                sum(net.tx_bytes),
                sum(net.rx_bytes),
                sum(net.tx_errors),
                sum(net.tx_dropped),
                sum(net.rx_crc),
                r.cpu_temp_c.unwrap_or(0.0),
                i64::from(r.cpu_temp_c.is_some()),
                r.cpu_temp_c,
                r.max_temp_c,
                r.cores_online,
                r.cores_total,
                r.thermal_core_limit,
            ],
        )?;
        Ok(hour)
    }
}

/// One round trip over the device link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RttSample {
    /// The round trip.
    pub rtt: Duration,
    /// Whether the device was idle (no turn, playback or diagnostic mode) at the time.
    pub idle: bool,
}

/// Adds one round trip to its hour of `device_metrics`.
#[derive(Clone, Debug)]
pub struct RecordRtt {
    /// The device.
    pub device: DeviceId,
    /// When the reply arrived.
    pub at: WallSeconds,
    /// The round trip.
    pub sample: RttSample,
}

impl WriteCommand for RecordRtt {
    type Output = HourStart;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<HourStart, RecordError> {
        let hour = self.at.hour();
        let ms = whole_ms(self.sample.rtt);
        let excursion = i64::from(self.sample.rtt >= RTT_EXCURSION);
        let idle = i64::from(self.sample.idle);
        tx.execute(
            "INSERT INTO device_metrics (device_id, hour_ts, rtt_sum_ms, rtt_samples, \
             rtt_min_ms, rtt_max_ms, rtt_excursions, rtt_excursions_idle, rtt_samples_idle) \
             VALUES (?1, ?2, ?3, 1, ?3, ?3, ?4, ?4 * ?5, ?5) \
             ON CONFLICT (device_id, hour_ts) DO UPDATE SET \
             rtt_sum_ms = rtt_sum_ms + excluded.rtt_sum_ms, \
             rtt_samples = rtt_samples + 1, \
             rtt_min_ms = min(coalesce(rtt_min_ms, excluded.rtt_min_ms), excluded.rtt_min_ms), \
             rtt_max_ms = max(coalesce(rtt_max_ms, excluded.rtt_max_ms), excluded.rtt_max_ms), \
             rtt_excursions = rtt_excursions + excluded.rtt_excursions, \
             rtt_excursions_idle = rtt_excursions_idle + excluded.rtt_excursions_idle, \
             rtt_samples_idle = rtt_samples_idle + excluded.rtt_samples_idle",
            params![self.device.0, hour.get(), ms, excursion, idle],
        )?;
        Ok(hour)
    }
}

/// One resolved shadow episode (`wake.stats.shadow[].events[]`).
#[derive(Clone, Debug, PartialEq)]
pub struct ShadowEvent {
    /// `kind`.
    pub kind: ShadowEventKind,
    /// The episode's wall time (from its `mono_ns`), stored as `ts`.
    pub ts: WallSeconds,
    /// `peak_raw`.
    pub peak_raw: f64,
    /// `raws`: the last raw scores (up to three) at the open.
    pub raws: Vec<f64>,
}

/// One `wake.stats.shadow` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ShadowStats {
    /// The rule.
    pub rule: OpenRule,
    /// `hops`.
    pub hops: u64,
    /// `opens`.
    pub opens: u64,
    /// `matched`.
    pub matched: u64,
    /// `lead_hist`.
    pub lead_hist: [u64; LEAD_BUCKETS],
    /// `unmatched`.
    pub unmatched: u64,
    /// `retried`.
    pub retried: u64,
    /// `live_only`.
    pub live_only: u64,
    /// `events`.
    pub events: Vec<ShadowEvent>,
    /// `events_dropped`.
    pub events_dropped: u64,
}

/// One `wake.stats` report, as far as the rollups use it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WakeStatsReport {
    /// `hops_scored`.
    pub hops_scored: u64,
    /// `hops_dropped`.
    pub hops_dropped: u64,
    /// The `peak` of each `near_misses` entry.
    pub near_miss_peaks: Vec<f64>,
    /// `candidates_opened`.
    pub candidates_opened: u64,
    /// `peak_smoothed`; None when nothing was scored.
    pub peak_smoothed: Option<f64>,
    /// `infer_max_ms`; None when nothing was inferred.
    pub infer_max_ms: Option<f64>,
    /// `shadow`; None when the firmware sent none.
    pub shadow: Option<Vec<ShadowStats>>,
}

/// `wake_shadow.rule_key`: `profile:windows:combine:threshold`, the threshold with two
/// decimals as every existing key has it (`idle:2:mean:0.90`), or more where two would not
/// give the threshold back exactly.
pub fn rule_key(rule: &OpenRule) -> String {
    let threshold = rule.threshold();
    let two = format!("{threshold:.2}");
    let text = if two.parse::<f64>() == Ok(threshold) {
        two
    } else {
        format!("{threshold}")
    };
    format!(
        "{}:{}:{}:{text}",
        rule.profile().as_str(),
        rule.windows(),
        rule.combine().as_str()
    )
}

/// A JSON array with a space after each comma, the style of `lead_hist` and `raws`.
fn spaced_array<T: ToString>(values: impl IntoIterator<Item = T>) -> String {
    let items: Vec<String> = values.into_iter().map(|v| v.to_string()).collect();
    format!("[{}]", items.join(", "))
}

fn json_number(v: f64) -> String {
    serde_json::Number::from_f64(v).map_or_else(|| "null".to_owned(), |n| n.to_string())
}

/// Adds one `wake.stats` report to its hour of `wake_counters` and `wake_shadow`, and writes
/// its shadow events. Returns the number of events written.
#[derive(Clone, Debug)]
pub struct RecordWakeStats {
    /// The device.
    pub device: DeviceId,
    /// When the report arrived.
    pub at: WallSeconds,
    /// The report.
    pub report: WakeStatsReport,
}

impl WriteCommand for RecordWakeStats {
    type Output = usize;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<usize, RecordError> {
        let hour = self.at.hour().get();
        let r = &self.report;
        let near_max = r
            .near_miss_peaks
            .iter()
            .copied()
            .filter(|p| p.is_finite())
            .reduce(f64::max);
        let infer_ms = r
            .infer_max_ms
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v.round() as i64);
        tx.execute(
            "INSERT INTO wake_counters (device_id, hour_ts, near_misses, near_miss_max, \
             dev_drops, dev_crossings, dev_max_score, dev_max_infer_ms, dev_hops) \
             VALUES (?1, ?2, ?3, coalesce(?4, 0), ?5, ?6, coalesce(?7, 0), coalesce(?8, 0), ?9) \
             ON CONFLICT (device_id, hour_ts) DO UPDATE SET \
             near_misses = near_misses + excluded.near_misses, \
             near_miss_max = max(near_miss_max, coalesce(?4, near_miss_max)), \
             dev_drops = dev_drops + excluded.dev_drops, \
             dev_crossings = dev_crossings + excluded.dev_crossings, \
             dev_max_score = max(dev_max_score, coalesce(?7, dev_max_score)), \
             dev_max_infer_ms = max(dev_max_infer_ms, coalesce(?8, dev_max_infer_ms)), \
             dev_hops = dev_hops + excluded.dev_hops",
            params![
                self.device.0,
                hour,
                sum(r.near_miss_peaks.len() as u64),
                near_max,
                sum(r.hops_dropped),
                sum(r.candidates_opened),
                r.peak_smoothed.filter(|v| v.is_finite()),
                infer_ms,
                sum(r.hops_scored),
            ],
        )?;
        let mut events = 0;
        for entry in r.shadow.iter().flatten() {
            let key = rule_key(&entry.rule);
            let stored: Option<String> = tx
                .query_row(
                    "SELECT lead_hist FROM wake_shadow \
                     WHERE device_id = ?1 AND hour_ts = ?2 AND rule_key = ?3",
                    params![self.device.0, hour, key],
                    |row| row.get(0),
                )
                .optional()?;
            let mut lead = entry.lead_hist;
            if let Some(text) = stored {
                let previous: Vec<u64> =
                    serde_json::from_str(&text).map_err(|_| RecordError::BadStoredValue {
                        table: "wake_shadow",
                        column: "lead_hist",
                        id: hour,
                    })?;
                if previous.len() != LEAD_BUCKETS {
                    return Err(RecordError::BadStoredValue {
                        table: "wake_shadow",
                        column: "lead_hist",
                        id: hour,
                    });
                }
                for (bucket, old) in lead.iter_mut().zip(previous) {
                    *bucket = bucket.saturating_add(old);
                }
            }
            tx.execute(
                "INSERT INTO wake_shadow (device_id, hour_ts, rule_key, hops, opens, matched, \
                 lead_hist, unmatched, retried, live_only, events_dropped) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                 ON CONFLICT (device_id, hour_ts, rule_key) DO UPDATE SET \
                 hops = hops + excluded.hops, opens = opens + excluded.opens, \
                 matched = matched + excluded.matched, lead_hist = excluded.lead_hist, \
                 unmatched = unmatched + excluded.unmatched, \
                 retried = retried + excluded.retried, \
                 live_only = live_only + excluded.live_only, \
                 events_dropped = events_dropped + excluded.events_dropped",
                params![
                    self.device.0,
                    hour,
                    key,
                    sum(entry.hops),
                    sum(entry.opens),
                    sum(entry.matched),
                    spaced_array(lead),
                    sum(entry.unmatched),
                    sum(entry.retried),
                    sum(entry.live_only),
                    sum(entry.events_dropped),
                ],
            )?;
            for event in &entry.events {
                tx.execute(
                    "INSERT INTO wake_shadow_events (device_id, ts, rule_key, kind, peak_raw, \
                     raws) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        self.device.0,
                        event.ts.get(),
                        key,
                        event.kind.as_str(),
                        event.peak_raw,
                        spaced_array(event.raws.iter().map(|v| json_number(*v))),
                    ],
                )?;
                events += 1;
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use echomuse_protocol::session::{RuleCombine, RuleProfile};

    use super::*;

    #[test]
    fn rule_keys_match_the_stored_spelling() {
        let rule = OpenRule::new(RuleProfile::Idle, 2, RuleCombine::Mean, 0.9).unwrap();
        assert_eq!(rule_key(&rule), "idle:2:mean:0.90");
        let rule = OpenRule::new(RuleProfile::Playback, 3, RuleCombine::All, 0.925).unwrap();
        assert_eq!(rule_key(&rule), "playback:3:all:0.925");
    }

    #[test]
    fn counters_skip_unknown_intervals() {
        let mut c = NetCounters::default();
        let t = |b| NetTotals {
            tx_bytes: b,
            rx_bytes: b * 2,
            ..NetTotals::default()
        };
        assert_eq!(c.observe(t(100)), None);
        assert_eq!(
            c.observe(t(150)).map(|d| (d.tx_bytes, d.rx_bytes)),
            Some((50, 100))
        );
        assert_eq!(c.observe(t(10)), None, "a reset has no known increase");
        assert_eq!(c.observe(t(15)).map(|d| d.tx_bytes), Some(5));
    }

    #[test]
    fn arrays_use_the_stored_style() {
        assert_eq!(spaced_array([0u64, 0, 1]), "[0, 0, 1]");
        assert_eq!(
            spaced_array([0.0425, 0.97].iter().map(|v| json_number(*v))),
            "[0.0425, 0.97]"
        );
    }
}
