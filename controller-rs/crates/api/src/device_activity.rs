//! `GET /api/devices/{id}/activity?days=N` (signed in): a device's hourly rollups over the
//! last `days` days (1 to 90, default 7), the hourly device metrics and wake counters of
//! spec §4.11.
//!
//! The shipped dashboard does not call this route, so its shape is this controller's,
//! built from the stored columns (records crate, `rollups`):
//!
//! ```text
//! {device_id, days, since, hours: [{hour_ts, metrics, wake}], totals: {metrics, wake}}
//! ```
//!
//! `since` is the first hour covered (Unix seconds; the current hour and the `days × 24 −
//! 1` before it). `hours` lists, oldest first, every hour with a stored row; `metrics`
//! (from `device_metrics`) or `wake` (from `wake_counters`) is `null` for an hour without
//! that row. An average is `null` when no report fed it, never 0: `cpu_avg` and the other
//! per-report averages need `reports` > 0, `rssi_avg_dbm` needs `rssi_samples` > 0 and
//! `rtt_avg_ms` needs `rtt_samples` > 0. A wake hour's `max_score` and `max_infer_ms` are
//! `null` when no hop was scored, and `near_miss_max` when no near miss was counted.
//! Columns the firmware no longer reports (`underruns`, `dev_frames`, `dev_max_gap_ms`)
//! are not shown. `totals` sums or combines the same values over the whole range, `null`
//! where no hour has any.

use axum::{Json, Router, extract::State, routing::get};
use echomuse_core::types::DeviceId;
use echomuse_db::ReadQuery;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};

use crate::{
    activity::known_device,
    auth::AuthenticatedUser,
    error::ApiError,
    params::{DevicePath, QueryParams},
    state::ApiState,
};

/// Days shown when the query names none.
const DEFAULT_DAYS: u32 = 7;
/// The most days one request covers.
const MAX_DAYS: u32 = 90;

pub(crate) fn routes() -> Router<ApiState> {
    Router::new().route("/api/devices/{id}/activity", get(activity))
}

#[derive(Deserialize)]
struct ActivityQuery {
    days: Option<u32>,
}

/// One hour's `device_metrics` row, as the route shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
struct Metrics {
    reports: i64,
    cpu_avg: Option<f64>,
    cpu_max: Option<f64>,
    mem_used_avg_mb: Option<f64>,
    mem_total_mb: Option<f64>,
    storage_used_mb: Option<f64>,
    storage_total_mb: Option<f64>,
    rssi_samples: i64,
    rssi_avg_dbm: Option<f64>,
    rssi_min_dbm: Option<f64>,
    link_speed_last_mbps: Option<i64>,
    link_speed_min_mbps: Option<i64>,
    wifi_freq_last_mhz: Option<i64>,
    wifi_bssid_last: Option<String>,
    tx_bytes: i64,
    rx_bytes: i64,
    tx_errors: i64,
    tx_dropped: i64,
    rx_crc: i64,
    rtt_samples: i64,
    rtt_avg_ms: Option<f64>,
    rtt_min_ms: Option<i64>,
    rtt_max_ms: Option<i64>,
    rtt_excursions: i64,
    rtt_samples_idle: i64,
    rtt_excursions_idle: i64,
    cpu_temp_samples: i64,
    cpu_temp_avg_c: Option<f64>,
    cpu_temp_max_c: Option<f64>,
    max_temp_max_c: Option<f64>,
    cores_online_last: Option<i64>,
    cores_online_min: Option<i64>,
    cores_total: Option<i64>,
    thermal_limit_min: Option<i64>,
}

/// One hour's `wake_counters` row, as the route shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
struct Wake {
    near_misses: i64,
    near_miss_max: Option<f64>,
    hops: i64,
    drops: i64,
    crossings: i64,
    max_score: Option<f64>,
    max_infer_ms: Option<i64>,
}

/// The sums behind a [`Metrics`], kept so the range totals average the right way.
#[derive(Clone, Debug, Default)]
struct MetricSums {
    samples: i64,
    cpu_sum: f64,
    mem_used_sum: f64,
    rssi_sum: f64,
    rtt_sum_ms: i64,
    cpu_temp_sum: f64,
}

#[derive(Clone, Debug, Serialize)]
struct Hour {
    hour_ts: i64,
    metrics: Option<Metrics>,
    wake: Option<Wake>,
}

#[derive(Clone, Debug, Serialize)]
struct Totals {
    metrics: Option<Metrics>,
    wake: Option<Wake>,
}

#[derive(Clone, Debug, Serialize)]
struct Activity {
    device_id: DeviceId,
    days: u32,
    since: i64,
    hours: Vec<Hour>,
    totals: Totals,
}

fn ratio(sum: f64, count: i64) -> Option<f64> {
    (count > 0).then(|| sum / count as f64)
}

fn metrics_row(row: &Row<'_>) -> rusqlite::Result<(i64, Metrics, MetricSums)> {
    let samples: i64 = row.get("samples")?;
    let sums = MetricSums {
        samples,
        cpu_sum: row.get("cpu_sum")?,
        mem_used_sum: row.get("mem_used_sum")?,
        rssi_sum: row.get("rssi_sum")?,
        rtt_sum_ms: row.get("rtt_sum_ms")?,
        cpu_temp_sum: row.get("cpu_temp_sum")?,
    };
    let rssi_samples: i64 = row.get("rssi_samples")?;
    let rtt_samples: i64 = row.get("rtt_samples")?;
    let cpu_temp_samples: i64 = row.get("cpu_temp_samples")?;
    let reported = samples > 0;
    let metrics = Metrics {
        reports: samples,
        cpu_avg: ratio(sums.cpu_sum, samples),
        cpu_max: if reported {
            Some(row.get("cpu_max")?)
        } else {
            None
        },
        mem_used_avg_mb: ratio(sums.mem_used_sum, samples),
        mem_total_mb: row.get("mem_total_mb")?,
        storage_used_mb: row.get("storage_used_mb")?,
        storage_total_mb: row.get("storage_total_mb")?,
        rssi_samples,
        rssi_avg_dbm: ratio(sums.rssi_sum, rssi_samples),
        rssi_min_dbm: row.get("rssi_min")?,
        link_speed_last_mbps: row.get("link_speed_last")?,
        link_speed_min_mbps: row.get("link_speed_min")?,
        wifi_freq_last_mhz: row.get("wifi_freq_last")?,
        wifi_bssid_last: row.get("wifi_bssid_last")?,
        tx_bytes: row.get("tx_bytes_sum")?,
        rx_bytes: row.get("rx_bytes_sum")?,
        tx_errors: row.get("tx_errors_sum")?,
        tx_dropped: row.get("tx_dropped_sum")?,
        rx_crc: row.get("rx_crc_sum")?,
        rtt_samples,
        rtt_avg_ms: ratio(sums.rtt_sum_ms as f64, rtt_samples),
        rtt_min_ms: row.get("rtt_min_ms")?,
        rtt_max_ms: row.get("rtt_max_ms")?,
        rtt_excursions: row.get("rtt_excursions")?,
        rtt_samples_idle: row.get("rtt_samples_idle")?,
        rtt_excursions_idle: row.get("rtt_excursions_idle")?,
        cpu_temp_samples,
        cpu_temp_avg_c: ratio(sums.cpu_temp_sum, cpu_temp_samples),
        cpu_temp_max_c: row.get("cpu_temp_max")?,
        max_temp_max_c: row.get("max_temp_max")?,
        cores_online_last: row.get("cores_online_last")?,
        cores_online_min: row.get("cores_online_min")?,
        cores_total: row.get("cores_total")?,
        thermal_limit_min: row.get("thermal_limit_min")?,
    };
    Ok((row.get("hour_ts")?, metrics, sums))
}

fn wake_row(row: &Row<'_>) -> rusqlite::Result<(i64, Wake)> {
    let near_misses: i64 = row.get("near_misses")?;
    let hops: i64 = row.get("dev_hops")?;
    Ok((
        row.get("hour_ts")?,
        Wake {
            near_misses,
            near_miss_max: if near_misses > 0 {
                Some(row.get("near_miss_max")?)
            } else {
                None
            },
            hops,
            drops: row.get("dev_drops")?,
            crossings: row.get("dev_crossings")?,
            max_score: if hops > 0 {
                Some(row.get("dev_max_score")?)
            } else {
                None
            },
            max_infer_ms: if hops > 0 {
                Some(row.get("dev_max_infer_ms")?)
            } else {
                None
            },
        },
    ))
}

/// The device's rollup rows from `since_hour` on, oldest first.
struct Rollups {
    device: DeviceId,
    since_hour: i64,
}

type RollupRows = (Vec<(i64, Metrics, MetricSums)>, Vec<(i64, Wake)>);

impl ReadQuery for Rollups {
    type Output = RollupRows;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<RollupRows> {
        let metrics = conn
            .prepare_cached(
                "SELECT * FROM device_metrics WHERE device_id = ?1 AND hour_ts >= ?2 \
                 ORDER BY hour_ts",
            )?
            .query_map(rusqlite::params![self.device.0, self.since_hour], |row| {
                metrics_row(row)
            })?
            .collect::<rusqlite::Result<_>>()?;
        let wake = conn
            .prepare_cached(
                "SELECT * FROM wake_counters WHERE device_id = ?1 AND hour_ts >= ?2 \
                 ORDER BY hour_ts",
            )?
            .query_map(rusqlite::params![self.device.0, self.since_hour], |row| {
                wake_row(row)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((metrics, wake))
    }
}

fn min_of<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b < a { b } else { a }),
        (a, b) => a.or(b),
    }
}

fn max_of<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b > a { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// The range's metrics: counts summed, extremes combined, averages over every report,
/// and each `…_last` value from the latest hour that has one.
fn total_metrics(rows: &[(i64, Metrics, MetricSums)]) -> Option<Metrics> {
    if rows.is_empty() {
        return None;
    }
    let mut total = Metrics::default();
    let mut sums = MetricSums::default();
    for (_, m, s) in rows {
        total.reports += m.reports;
        total.cpu_max = max_of(total.cpu_max, m.cpu_max);
        total.mem_total_mb = m.mem_total_mb.or(total.mem_total_mb);
        total.storage_used_mb = m.storage_used_mb.or(total.storage_used_mb);
        total.storage_total_mb = m.storage_total_mb.or(total.storage_total_mb);
        total.rssi_samples += m.rssi_samples;
        total.rssi_min_dbm = min_of(total.rssi_min_dbm, m.rssi_min_dbm);
        total.link_speed_last_mbps = m.link_speed_last_mbps.or(total.link_speed_last_mbps);
        total.link_speed_min_mbps = min_of(total.link_speed_min_mbps, m.link_speed_min_mbps);
        total.wifi_freq_last_mhz = m.wifi_freq_last_mhz.or(total.wifi_freq_last_mhz);
        total.wifi_bssid_last = m.wifi_bssid_last.clone().or(total.wifi_bssid_last.take());
        total.tx_bytes += m.tx_bytes;
        total.rx_bytes += m.rx_bytes;
        total.tx_errors += m.tx_errors;
        total.tx_dropped += m.tx_dropped;
        total.rx_crc += m.rx_crc;
        total.rtt_samples += m.rtt_samples;
        total.rtt_min_ms = min_of(total.rtt_min_ms, m.rtt_min_ms);
        total.rtt_max_ms = max_of(total.rtt_max_ms, m.rtt_max_ms);
        total.rtt_excursions += m.rtt_excursions;
        total.rtt_samples_idle += m.rtt_samples_idle;
        total.rtt_excursions_idle += m.rtt_excursions_idle;
        total.cpu_temp_samples += m.cpu_temp_samples;
        total.cpu_temp_max_c = max_of(total.cpu_temp_max_c, m.cpu_temp_max_c);
        total.max_temp_max_c = max_of(total.max_temp_max_c, m.max_temp_max_c);
        total.cores_online_last = m.cores_online_last.or(total.cores_online_last);
        total.cores_online_min = min_of(total.cores_online_min, m.cores_online_min);
        total.cores_total = m.cores_total.or(total.cores_total);
        total.thermal_limit_min = min_of(total.thermal_limit_min, m.thermal_limit_min);
        sums.samples += s.samples;
        sums.cpu_sum += s.cpu_sum;
        sums.mem_used_sum += s.mem_used_sum;
        sums.rssi_sum += s.rssi_sum;
        sums.rtt_sum_ms += s.rtt_sum_ms;
        sums.cpu_temp_sum += s.cpu_temp_sum;
    }
    total.cpu_avg = ratio(sums.cpu_sum, sums.samples);
    total.mem_used_avg_mb = ratio(sums.mem_used_sum, sums.samples);
    total.rssi_avg_dbm = ratio(sums.rssi_sum, total.rssi_samples);
    total.rtt_avg_ms = ratio(sums.rtt_sum_ms as f64, total.rtt_samples);
    total.cpu_temp_avg_c = ratio(sums.cpu_temp_sum, total.cpu_temp_samples);
    Some(total)
}

fn total_wake(rows: &[(i64, Wake)]) -> Option<Wake> {
    if rows.is_empty() {
        return None;
    }
    let mut total = Wake::default();
    for (_, w) in rows {
        total.near_misses += w.near_misses;
        total.near_miss_max = max_of(total.near_miss_max, w.near_miss_max);
        total.hops += w.hops;
        total.drops += w.drops;
        total.crossings += w.crossings;
        total.max_score = max_of(total.max_score, w.max_score);
        total.max_infer_ms = max_of(total.max_infer_ms, w.max_infer_ms);
    }
    Some(total)
}

fn merge(metrics: &[(i64, Metrics, MetricSums)], wake: &[(i64, Wake)]) -> Vec<Hour> {
    let mut hours: std::collections::BTreeMap<i64, Hour> = std::collections::BTreeMap::new();
    for (hour_ts, m, _) in metrics {
        hours
            .entry(*hour_ts)
            .or_insert_with(|| Hour {
                hour_ts: *hour_ts,
                metrics: None,
                wake: None,
            })
            .metrics = Some(m.clone());
    }
    for (hour_ts, w) in wake {
        hours
            .entry(*hour_ts)
            .or_insert_with(|| Hour {
                hour_ts: *hour_ts,
                metrics: None,
                wake: None,
            })
            .wake = Some(w.clone());
    }
    hours.into_values().collect()
}

async fn activity(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
    QueryParams(query): QueryParams<ActivityQuery>,
) -> Result<Json<Activity>, ApiError> {
    let days = query.days.unwrap_or(DEFAULT_DAYS);
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(ApiError::bad_request(format!(
            "days must be 1 to {MAX_DAYS}"
        )));
    }
    known_device(&state, &device).await?;
    let now = crate::clock::unix_now(state.clock())?.0;
    let this_hour = now.div_euclid(3_600) * 3_600;
    let since = this_hour - (i64::from(days) * 24 - 1) * 3_600;
    let (metrics, wake) = state
        .db()
        .read(Rollups {
            device: device.clone(),
            since_hour: since,
        })
        .await?;
    Ok(Json(Activity {
        device_id: device,
        days,
        since,
        hours: merge(&metrics, &wake),
        totals: Totals {
            metrics: total_metrics(&metrics),
            wake: total_wake(&wake),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extremes_keep_unknown_unless_one_side_knows() {
        assert_eq!(min_of::<i64>(None, None), None);
        assert_eq!(min_of(Some(3), None), Some(3));
        assert_eq!(min_of(Some(3), Some(2)), Some(2));
        assert_eq!(max_of(None, Some(1.5)), Some(1.5));
        assert_eq!(ratio(10.0, 0), None);
        assert_eq!(ratio(10.0, 4), Some(2.5));
    }
}
