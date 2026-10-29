//! Placing the reference on the microphone's timeline (post-AFE architecture §4.3, §16.1).
//!
//! The microphone and the reference are different streams with different epochs: microphone
//! sample `m` is a capture sample, reference sample `r` is the final mix's render sample `3r`
//! decimated to 16 kHz. Every packet of either stream carries the device `CLOCK_MONOTONIC` time
//! of its first sample, taken from the device's own clock fits (capture callbacks for the
//! microphone, render buffer completions for the reference). Two samples with the same device
//! time are the same instant, so the controller maps a microphone sample to the reference sample
//! played at that moment through those stamps: the "clock estimate" of §16.6. What is left is
//! the acoustic and processing delay from the loudspeaker through the native AFE to the
//! microphone stream, which the lag search measures.
//!
//! Each stream's stamps are fitted again here by least squares over its most recent packets, so
//! a single packet's stamp cannot move the mapping by itself.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::lag::Lag;
use echomuse_turn::Epoch;

/// Device nanoseconds per 16 kHz sample.
const NOMINAL_NS_PER_SAMPLE: f64 = 62_500.0;
/// Packet stamps kept per stream: 64 packets, 5 s of 80 ms live packets.
const MAX_ANCHORS: usize = 64;
/// A fitted rate is used only once the stamps span this much device time (1 s); before that the
/// nominal 16 kHz rate is assumed, since a rate fitted over a few packets is mostly stamp noise.
const MIN_RATE_SPAN_NS: f64 = 1e9;
/// A fitted rate further than this from nominal (0.1%, ten times a poor crystal) is not a clock
/// and is replaced by the nominal rate.
const MAX_RATE_ERROR: f64 = 1e-3;

/// A straight line from device time to stream position.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClockFit {
    /// The device time the line is centred on, in ns.
    pub ns_ref: u64,
    /// The stream position at `ns_ref`, in samples (fractional).
    pub position_at_ref: f64,
    /// Stream samples per device nanosecond.
    pub samples_per_ns: f64,
}

impl ClockFit {
    /// The stream position at device time `ns`.
    pub fn position_at(&self, ns: f64) -> f64 {
        self.position_at_ref + self.samples_per_ns * (ns - self.ns_ref as f64)
    }

    /// The device time of stream position `position`.
    pub fn ns_at(&self, position: f64) -> f64 {
        self.ns_ref as f64 + (position - self.position_at_ref) / self.samples_per_ns
    }
}

/// The packet stamps of one stream epoch.
#[derive(Clone, Debug, Default)]
pub(crate) struct StreamClock {
    anchors: VecDeque<(u64, u64)>,
}

impl StreamClock {
    /// Forgets every stamp.
    pub(crate) fn clear(&mut self) {
        self.anchors.clear();
    }

    /// Records that stream position `position` was at device time `mono_ns`. A stamp for a
    /// position already stamped (a duplicate packet) is ignored.
    pub(crate) fn add(&mut self, position: u64, mono_ns: u64) {
        if self.anchors.iter().any(|&(p, _)| p == position) {
            return;
        }
        if self.anchors.len() == MAX_ANCHORS {
            self.anchors.pop_front();
        }
        self.anchors.push_back((position, mono_ns));
    }

    /// The least-squares line through the stamps, or `None` before the first one.
    pub(crate) fn fit(&self) -> Option<ClockFit> {
        let &(p0, t0) = self.anchors.back()?;
        let n = self.anchors.len() as f64;
        // Work in differences from the newest stamp so the sums stay small and exact.
        let mut sx = 0.0;
        let mut sy = 0.0;
        let mut lo = f64::MAX;
        let mut hi = f64::MIN;
        for &(p, t) in &self.anchors {
            let x = t as f64 - t0 as f64;
            sx += x;
            sy += p as f64 - p0 as f64;
            lo = lo.min(x);
            hi = hi.max(x);
        }
        let (mx, my) = (sx / n, sy / n);
        let nominal = 1.0 / NOMINAL_NS_PER_SAMPLE;
        let mut slope = nominal;
        if hi - lo >= MIN_RATE_SPAN_NS {
            let mut sxx = 0.0;
            let mut sxy = 0.0;
            for &(p, t) in &self.anchors {
                let x = t as f64 - t0 as f64 - mx;
                let y = p as f64 - p0 as f64 - my;
                sxx += x * x;
                sxy += x * y;
            }
            let fitted = sxy / sxx;
            if fitted.is_finite() && (fitted / nominal - 1.0).abs() <= MAX_RATE_ERROR {
                slope = fitted;
            }
        }
        // The line passes through the centroid of the stamps.
        Some(ClockFit {
            ns_ref: t0,
            position_at_ref: p0 as f64 + my - slope * mx,
            samples_per_ns: slope,
        })
    }
}

/// Where the reference sits on the microphone's timeline: both clock fits, and the acoustic lag
/// when one is established. The microphone analysis worker publishes it
/// ([`crate::EchoAnalyzer::alignment`]); the reference wake judgment reads it
/// ([`crate::ReferenceWakeScorer::judge`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alignment {
    /// The microphone's capture epoch.
    pub mic_epoch: Epoch,
    /// The reference epoch the fits belong to.
    pub reference_epoch: Epoch,
    /// Device time to microphone sample.
    pub mic: ClockFit,
    /// Device time to reference sample.
    pub reference: ClockFit,
    /// The acoustic lag, when established: microphone sample `m` hears reference sample
    /// `mic_to_reference(m) − lag`.
    pub lag: Option<Lag>,
}

impl Alignment {
    /// The reference sample played at the same device time as microphone sample `m`.
    pub fn mic_to_reference(&self, m: f64) -> f64 {
        self.reference.position_at(self.mic.ns_at(m))
    }

    /// The microphone sample captured at the same device time as reference sample `r` was
    /// played.
    pub fn reference_to_mic(&self, r: f64) -> f64 {
        self.mic.position_at(self.reference.ns_at(r))
    }

    /// The same alignment with another lag, for example the one a candidate comparison found.
    pub fn with_lag(self, lag: Option<Lag>) -> Self {
        Self { lag, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_stamp_uses_the_nominal_rate() {
        let mut c = StreamClock::default();
        c.add(16_000, 5_000_000_000);
        let f = c.fit().unwrap();
        assert!((f.position_at(6e9) - 32_000.0).abs() < 1e-6);
        assert!((f.ns_at(0.0) - 4e9).abs() < 1e-3);
    }

    #[test]
    fn a_drifting_clock_is_fitted_once_the_stamps_span_a_second() {
        // A clock 50 ppm fast: 16,000.8 samples per device second.
        let rate = 16_000.8 / 1e9;
        let mut c = StreamClock::default();
        for i in 0..40u64 {
            let p = i * 1_280;
            c.add(p, 7_000_000_000 + (p as f64 / rate).round() as u64);
        }
        let f = c.fit().unwrap();
        assert!((f.samples_per_ns / rate - 1.0).abs() < 1e-6);
        // Back-projection is within a hundredth of a sample over the fitted span.
        let p = 39.0 * 1_280.0;
        assert!((f.position_at(7e9 + p / rate) - p).abs() < 0.01);
    }

    #[test]
    fn a_wild_rate_falls_back_to_nominal() {
        let mut c = StreamClock::default();
        for i in 0..20u64 {
            // Stamps that claim 17 kHz: not a clock.
            c.add(i * 1_700, 1_000_000_000 + i * 100_000_000);
        }
        let f = c.fit().unwrap();
        assert_eq!(f.samples_per_ns, 1.0 / NOMINAL_NS_PER_SAMPLE);
    }

    #[test]
    fn the_alignment_maps_both_ways_through_device_time() {
        let mut mic = StreamClock::default();
        let mut reference = StreamClock::default();
        // The reference epoch started 2.5 s of device time after the capture epoch.
        mic.add(0, 10_000_000_000);
        reference.add(0, 12_500_000_000);
        let a = Alignment {
            mic_epoch: Epoch::new(1).unwrap(),
            reference_epoch: Epoch::new(2).unwrap(),
            mic: mic.fit().unwrap(),
            reference: reference.fit().unwrap(),
            lag: None,
        };
        assert!((a.mic_to_reference(40_000.0) - 0.0).abs() < 1e-6);
        assert!((a.reference_to_mic(16_000.0) - 56_000.0).abs() < 1e-6);
    }
}
