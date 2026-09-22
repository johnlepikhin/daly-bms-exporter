//! Self-calibration of the pack current sensor.
//!
//! # The problem
//!
//! The BMS current register has a zero-point error that differs per unit. In
//! production the three parallel packs sit at roughly -70, +150 and -60 mA at
//! true zero. Integrated around the clock, +150 mA is 3.6 Ah/day — on a 40 Ah
//! pack that is a phantom "charge in" of nearly a full cycle every eleven days,
//! and it is exactly what made `charge_watt_hours` outrun `discharge_watt_hours`
//! by 25% on one pack while its state of charge never moved.
//!
//! # The estimator
//!
//! Over any interval a pack obeys
//!
//! ```text
//! ∫I dt  =  ΔQ  +  ∫I_balance dt  +  offset·T  +  ε
//! ```
//!
//! where `Q` is the BMS-reported remaining capacity, `I_balance` the passive
//! balancer bleed and `ε` the unmodelled remainder. Rearranged, the running
//! quantity
//!
//! ```text
//! y(t) = ∫I dt − Q(t) − ∫I_balance dt
//! ```
//!
//! has slope `offset` in amperes when `t` is measured in hours. So the offset is
//! the slope of a straight line fitted through `y`, and [`EwFit`] fits exactly
//! that with exponential forgetting — no ring buffer, a dozen floats of state,
//! and a window that ages out on its own.
//!
//! **Why `Q` does not make this circular.** `Q` comes from the BMS's own coulomb
//! counter, which suffers the very offset being estimated. It cannot, however,
//! run away: the BMS clamps it to `0..=rated_capacity`. Whatever it does inside
//! those bounds, `|ΔQ| ≤ Cap`, so the error it can induce in the fitted slope is
//! at most `Cap/T` and shrinks as the window grows — ±55 mA over 30 days on a
//! 40 Ah pack, ±18 mA over 90. The estimate is anchored by a *bound* on `Q`, not
//! by trusting its value. This is also why a pack whose `Q` is pinned at 100%
//! (production: `ratzek-2`) still calibrates correctly; a pinned `Q` simply
//! contributes `ΔQ = 0`.
//!
//! **Integrated time, not wall time.** `x` advances by the same clamped `dt` the
//! coulomb counter integrates over, so an offline stretch that `max_gap_secs`
//! excluded from `∫I dt` is excluded from `T` as well. Using wall time here
//! would dilute the slope toward zero after every outage.
//!
//! # What the estimate actually contains
//!
//! The slope absorbs everything that makes charge disappear without the sensor
//! seeing it: the sensor's zero error, but also any real loss not captured by
//! `∫I_balance dt` — under-reported balancer bleed, cell self-discharge, BMS
//! self-consumption. That is the right thing to subtract if the goal is books
//! that balance, and the wrong thing if the goal is a sensor reading. The two
//! are told apart by the peer check below, which sees the sensor error *only*.
//!
//! # The peer check
//!
//! Packs wired in parallel carry near-identical current, so regressing one
//! pack's reading against another's gives a slope (≈1, their conductance ratio)
//! and an intercept that is the difference of their zero errors — a pure sensor
//! measurement, independent of any loss term. Peers are discovered from the data
//! (slope near 1, high R², enough samples); nothing declares the topology.
//!
//! A peer disagreement does **not** mean the calibration is wrong: the closure
//! estimate legitimately exceeds the peer estimate by whatever real loss
//! `∫I_balance dt` failed to account for. In production that difference is
//! ~90 mA. The check is therefore tuned to catch gross breakage — a sensor
//! failing, a pack leaving the bank, an estimator bug — and freezes the applied
//! offset when it fires, rather than policing tens of milliamps.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Exponentially-weighted least-squares fit of `y = slope·x + intercept`.
///
/// Weights decay by `exp(-Δ/τ)` applied to every accumulator before each push,
/// which makes the effective window `τ` wide without storing a single sample.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct EwFit {
    /// Sum of weights — the effective sample count.
    n: f64,
    sx: f64,
    sy: f64,
    sxx: f64,
    sxy: f64,
    syy: f64,
}

/// A fitted line together with the diagnostics needed to decide whether to
/// believe it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub slope: f64,
    pub intercept: f64,
    /// Coefficient of determination, `0..=1`. Low R² on a near-zero slope is
    /// normal and harmless (there is no ramp to explain), so this is a
    /// diagnostic, never a gate.
    pub r2: f64,
    /// Standard error of `slope`. Optimistic by construction: `y` is a running
    /// integral, so its residuals are strongly autocorrelated and the
    /// independent-sample formula understates the true spread. Treat it as a
    /// floor, which is why the span gate carries the real weight.
    pub stderr: f64,
}

impl EwFit {
    /// Age every accumulator by `factor` (`0..=1`), discounting past samples.
    fn decay(&mut self, factor: f64) {
        self.n *= factor;
        self.sx *= factor;
        self.sy *= factor;
        self.sxx *= factor;
        self.sxy *= factor;
        self.syy *= factor;
    }

    /// Add one unit-weight observation.
    fn push(&mut self, x: f64, y: f64) {
        self.n += 1.0;
        self.sx += x;
        self.sy += y;
        self.sxx += x * x;
        self.sxy += x * y;
        self.syy += y * y;
    }

    /// Move the `x` origin `d` to the right, exactly: every accumulator is
    /// rewritten in terms of `x' = x − d`. Keeps `sxx` small on a device that
    /// has been running for years, where `x²` would otherwise dominate the
    /// centred sums and eat precision.
    fn shift_x(&mut self, d: f64) {
        self.sxx = self.sxx - 2.0 * d * self.sx + d * d * self.n;
        self.sxy -= d * self.sy;
        self.sx -= d * self.n;
    }

    /// Fit the line, or `None` when the data cannot support one: fewer than
    /// three effective samples, or no spread along `x`.
    #[must_use]
    pub fn fit(&self) -> Option<Fit> {
        if self.n < 3.0 {
            return None;
        }
        let sxx = self.sxx - self.sx * self.sx / self.n;
        let sxy = self.sxy - self.sx * self.sy / self.n;
        let syy = self.syy - self.sy * self.sy / self.n;
        // `sxx` can go slightly negative through rounding when every sample
        // shares one `x`; treat that as no spread rather than dividing by it.
        if sxx <= 0.0 || !sxx.is_finite() {
            return None;
        }
        let slope = sxy / sxx;
        let intercept = (self.sy - slope * self.sx) / self.n;
        // Residual sum of squares, floored at zero: the algebraic form can go a
        // few ULP negative on a near-perfect fit.
        let sse = (syy - slope * sxy).max(0.0);
        let stderr = (sse / (self.n - 2.0) / sxx).sqrt();
        let r2 = if syy > 0.0 {
            (sxy * sxy / (sxx * syy)).clamp(0.0, 1.0)
        } else {
            1.0
        };
        if !slope.is_finite() || !intercept.is_finite() {
            return None;
        }
        Some(Fit {
            slope,
            intercept,
            r2,
            stderr: if stderr.is_finite() { stderr } else { f64::MAX },
        })
    }

    /// Effective sample count.
    #[must_use]
    pub fn weight(&self) -> f64 {
        self.n
    }
}

/// Tunables for [`Calibrator`]. Defaults are the production values; see
/// `config.example.yaml` for what each one buys.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Forgetting time constant of the closure fit, in **integrated** hours.
    pub tau_hours: f64,
    /// Integrated hours that must have accumulated before any offset is applied.
    /// A floor under [`Options::max_anchor_error_amperes`], which is the gate
    /// that normally binds.
    pub min_span_hours: f64,
    /// Refuse to apply an offset until the anchor bound `Cap / window` has
    /// fallen below this many amperes.
    ///
    /// This is the gate that matters. `Q` is bounded by the pack's capacity, so
    /// however wrong the BMS's state of charge is, it can shift the fitted slope
    /// by at most `Cap / window` — a number the estimator knows exactly. Gating
    /// on it means "do not correct until the estimate is provably good to within
    /// X", instead of guessing at a warmup period. On a 40 Ah pack, 0.05 A is
    /// reached after ~800 integrated hours.
    ///
    /// The alternative gate — the fitted slope's standard error — is useless
    /// here: `y` is a running integral, so its residuals are autocorrelated and
    /// the reported error is optimistic by orders of magnitude.
    pub max_anchor_error_amperes: f64,
    /// Hard clamp on the applied offset (amperes).
    pub max_offset_amperes: f64,
    /// Refuse to apply a slope whose standard error exceeds this (amperes).
    pub max_stderr_amperes: f64,
    /// Integration-interval cap, mirroring the coulomb counter's own.
    pub max_gap_secs: f64,
    /// Two devices' samples pair up for the peer fit when their timestamps are
    /// within this many seconds.
    pub peer_max_skew_secs: f64,
    /// Forgetting time constant of the peer fits, in wall-clock hours.
    pub peer_tau_hours: f64,
    /// Effective sample count a peer fit needs before it is consulted.
    pub peer_min_weight: f64,
    /// Peer slope must lie within `1 ± this` for the pair to count as parallel.
    pub peer_slope_tolerance: f64,
    /// Minimum R² for a peer fit to be consulted.
    pub peer_min_r2: f64,
    /// Freeze the applied offset when the closure and peer estimates of the same
    /// pairwise difference disagree by more than this (amperes).
    pub peer_max_disagreement_amperes: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // Two months. The window has to be long enough that the anchor
            // bound below is small, and the offset itself drifts far slower
            // than that: a month of production data moved it by under 40 mA.
            tau_hours: 1440.0,
            min_span_hours: 168.0,
            max_anchor_error_amperes: 0.05,
            max_offset_amperes: 0.5,
            max_stderr_amperes: 0.05,
            max_gap_secs: 900.0,
            peer_max_skew_secs: 120.0,
            peer_tau_hours: 168.0,
            peer_min_weight: 500.0,
            peer_slope_tolerance: 0.2,
            peer_min_r2: 0.8,
            peer_max_disagreement_amperes: 0.25,
        }
    }
}

/// One realtime frame's worth of input.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Raw pack current (A), as decoded — never pre-corrected.
    pub current_a: f64,
    /// Balancer bleed current (A), when the frame carries it.
    pub balance_a: Option<f64>,
    /// BMS-reported remaining capacity (Ah), when the frame carries it.
    pub remaining_ah: Option<f64>,
    /// Rated pack capacity (Ah), when known — it bounds how far `remaining_ah`
    /// can move and therefore how wrong the fitted slope can be. Absent, the
    /// largest `remaining_ah` ever seen stands in as a lower bound.
    pub capacity_ah: Option<f64>,
    /// Wall-clock arrival time, seconds since the epoch.
    pub now_secs: f64,
}

/// Why the estimate is not being applied. Exported as a gauge so a dashboard can
/// say which gate is holding rather than just "inactive".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Hold {
    /// Applied and trusted.
    None,
    /// The window is still too short for the anchor bound to be tight enough.
    /// Also the default: a device with no estimator state yet is warming up, not
    /// calibrated, so nothing can read as applied before the first frame.
    #[default]
    Warmup,
    /// No fit at all (too few samples, or no spread along `x`).
    NoFit,
    /// Slope standard error above `max_stderr_amperes`.
    Noisy,
    /// A peer pair disagrees beyond `peer_max_disagreement_amperes`.
    PeerDisagreement,
}

impl Hold {
    /// Stable numeric encoding for the `daly_bms_calibration_hold` gauge.
    #[must_use]
    pub fn code(self) -> f64 {
        match self {
            Self::None => 0.0,
            Self::Warmup => 1.0,
            Self::NoFit => 2.0,
            Self::Noisy => 3.0,
            Self::PeerDisagreement => 4.0,
        }
    }
}

/// What the peer check concluded for one device: the worst-disagreeing pair.
#[derive(Debug, Clone)]
pub struct PeerReport {
    pub peer_sn: String,
    /// `offset(this) − offset(peer)` as measured by the peer regression.
    pub relative_offset: f64,
    pub slope: f64,
    pub r2: f64,
    /// Absolute gap between the peer-measured difference and the closure-derived
    /// one. Expected to be non-zero — see the module docs.
    pub disagreement: f64,
}

/// Everything the metrics layer exports about one device's calibration.
#[derive(Debug, Clone)]
pub struct Report {
    /// Offset actually subtracted from the current (A).
    pub applied: f64,
    /// Latest closure estimate, whether or not it is applied (A).
    pub estimate: Option<f64>,
    pub stderr: Option<f64>,
    pub r2: Option<f64>,
    /// Integrated hours accumulated so far.
    pub span_hours: f64,
    /// How far the BMS's bounded state of charge could still be shifting the
    /// estimate, in amperes — the headline accuracy of `estimate`.
    pub anchor_error: f64,
    pub weight: f64,
    pub hold: Hold,
    pub peer: Option<PeerReport>,
}

/// Persisted per-device estimator state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceState {
    #[serde(default)]
    fit: EwFit,
    /// `x` (integrated hours) of the last pushed sample, relative to the fit's
    /// current origin.
    #[serde(default)]
    x: f64,
    /// Total integrated hours since this device was first seen, never re-origined.
    #[serde(default)]
    span_hours: f64,
    #[serde(default)]
    cum_net_ah: f64,
    #[serde(default)]
    cum_bal_ah: f64,
    /// Offset currently being applied; kept so a restart does not step the
    /// correction back to zero while the fit reloads.
    #[serde(default)]
    applied: f64,
    /// Largest capacity hint seen: the rated capacity when config frames carry
    /// it, otherwise the high-water mark of `remaining_ah`.
    #[serde(default)]
    capacity_ah: f64,
}

/// Persisted whole-calibrator state, embedded in the coulomb state file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    devices: BTreeMap<String, DeviceState>,
    /// Peer fits, keyed `"<a>|<b>"` with `a < b` lexicographically.
    #[serde(default)]
    pairs: BTreeMap<String, EwFit>,
}

/// Non-persisted per-device scratch: the previous frame, needed to integrate.
#[derive(Debug, Clone, Default)]
struct Live {
    last_ts: Option<f64>,
    last_current: Option<f64>,
    last_balance: Option<f64>,
    /// Last raw reading, for pairing against peers.
    last_raw: Option<(f64, f64)>,
    hold: Hold,
    peer: Option<PeerReport>,
}

/// Per-pair scratch: when the pair fit was last aged.
#[derive(Debug, Clone, Default)]
struct PairLive {
    last_ts: Option<f64>,
}

/// Rolling current-sensor calibration for every tracked device.
///
/// Not `Sync`; the metrics layer keeps it behind the coulomb mutex so its
/// updates stay serialized with the counters they feed.
#[derive(Debug)]
pub struct Calibrator {
    opts: Options,
    state: State,
    live: BTreeMap<String, Live>,
    pair_live: BTreeMap<String, PairLive>,
}

/// Worst-case error the BMS's state of charge can still induce in the fitted
/// slope: `capacity / window`, where the window is the shorter of the data
/// collected so far and the forgetting constant (older data is discounted away,
/// so it does not widen the window).
///
/// Returns infinity when nothing is known about the capacity yet, which holds
/// the correction rather than applying an unbounded estimate.
fn anchor_error(capacity_ah: f64, span_hours: f64, tau_hours: f64) -> f64 {
    let window = span_hours.min(tau_hours);
    if capacity_ah <= 0.0 || window <= 0.0 {
        return f64::INFINITY;
    }
    capacity_ah / window
}

/// Key for a device pair, ordered so `(a,b)` and `(b,a)` hash the same.
fn pair_key(a: &str, b: &str) -> (String, bool) {
    if a < b {
        (format!("{a}|{b}"), false)
    } else {
        (format!("{b}|{a}"), true)
    }
}

impl Calibrator {
    #[must_use]
    pub fn new(opts: Options) -> Self {
        Self {
            opts,
            state: State::default(),
            live: BTreeMap::new(),
            pair_live: BTreeMap::new(),
        }
    }

    /// Feed one realtime frame and get back the offset to subtract from its
    /// current before it reaches the calibrated counters.
    ///
    /// The caller must pass the **raw** current: the estimator's whole input is
    /// the uncorrected reading, and feeding it a corrected one closes a feedback
    /// loop that drives the offset to zero.
    pub fn observe(&mut self, sn: &str, s: Sample) -> f64 {
        self.integrate(sn, s);
        self.update_peers(sn, s);
        self.recompute(sn);
        self.state.devices.get(sn).map_or(0.0, |d| d.applied)
    }

    /// Advance the running integrals and push one point into the closure fit.
    fn integrate(&mut self, sn: &str, s: Sample) {
        let dev = self.state.devices.entry(sn.to_string()).or_default();
        let live = self.live.entry(sn.to_string()).or_default();

        if let (Some(last_ts), Some(last_cur)) = (live.last_ts, live.last_current) {
            let dt = (s.now_secs - last_ts).clamp(0.0, self.opts.max_gap_secs);
            if dt > 0.0 {
                let hours = dt / 3600.0;
                dev.cum_net_ah += (last_cur + s.current_a) / 2.0 * hours;
                if let (Some(lb), Some(b)) = (live.last_balance, s.balance_a) {
                    // The bleed is a magnitude; a negative reading is a decode
                    // artifact, not a reverse balancer.
                    dev.cum_bal_ah += (lb.max(0.0) + b.max(0.0)) / 2.0 * hours;
                }
                dev.x += hours;
                dev.span_hours += hours;

                if let Some(q) = s.remaining_ah {
                    dev.capacity_ah = dev.capacity_ah.max(s.capacity_ah.unwrap_or(0.0)).max(q);
                    let y = dev.cum_net_ah - q - dev.cum_bal_ah;
                    dev.fit.decay((-hours / self.opts.tau_hours).exp());
                    dev.fit.push(dev.x, y);
                    // Keep `x` within a few window widths of the origin so the
                    // centred sums stay well-conditioned on a long-lived device.
                    if dev.x > 50.0 * self.opts.tau_hours {
                        let d = dev.x - self.opts.tau_hours;
                        dev.fit.shift_x(d);
                        dev.x -= d;
                    }
                }
            }
        }

        live.last_ts = Some(s.now_secs);
        live.last_current = Some(s.current_a);
        live.last_balance = s.balance_a;
        live.last_raw = Some((s.now_secs, s.current_a));
    }

    /// Pair this sample against every peer that reported recently enough, and
    /// update the corresponding parallel-bank regressions.
    fn update_peers(&mut self, sn: &str, s: Sample) {
        let peers: Vec<(String, f64)> = self
            .live
            .iter()
            .filter(|(other, _)| other.as_str() != sn)
            .filter_map(|(other, l)| {
                l.last_raw.and_then(|(ts, cur)| {
                    ((s.now_secs - ts).abs() <= self.opts.peer_max_skew_secs)
                        .then(|| (other.clone(), cur))
                })
            })
            .collect();

        for (other, other_cur) in peers {
            let (key, flipped) = pair_key(sn, &other);
            // The fit is always `larger_sn = slope·smaller_sn + intercept`, so
            // the stored intercept has one fixed meaning regardless of which
            // device's frame triggered the update.
            let (x, y) = if flipped {
                (other_cur, s.current_a)
            } else {
                (s.current_a, other_cur)
            };
            let pl = self.pair_live.entry(key.clone()).or_default();
            let decay = pl.last_ts.map_or(1.0, |last| {
                let hours = ((s.now_secs - last).max(0.0)) / 3600.0;
                (-hours / self.opts.peer_tau_hours).exp()
            });
            pl.last_ts = Some(s.now_secs);
            let fit = self.state.pairs.entry(key).or_default();
            fit.decay(decay);
            fit.push(x, y);
        }
    }

    /// Re-derive the applied offset for one device from its fit and its peers.
    fn recompute(&mut self, sn: &str) {
        let (span, previous, fit, anchor) = {
            let Some(dev) = self.state.devices.get(sn) else {
                return;
            };
            (
                dev.span_hours,
                dev.applied,
                dev.fit.fit(),
                anchor_error(dev.capacity_ah, dev.span_hours, self.opts.tau_hours),
            )
        };

        let (mut offset, mut hold) = match fit {
            None => (previous, Hold::NoFit),
            Some(_)
                if span < self.opts.min_span_hours
                    || anchor > self.opts.max_anchor_error_amperes =>
            {
                (previous, Hold::Warmup)
            }
            Some(f) if f.stderr > self.opts.max_stderr_amperes => (previous, Hold::Noisy),
            Some(f) => (
                f.slope
                    .clamp(-self.opts.max_offset_amperes, self.opts.max_offset_amperes),
                Hold::None,
            ),
        };

        // The peer report is always published — the disagreement is a useful
        // diagnostic on its own — but it may only veto a correction that would
        // otherwise be applied.
        //
        // Comparing the two estimators before the closure estimate is eligible
        // is meaningless: during warmup the closure slope is dominated by the
        // state-of-charge excursion that `anchor_error` bounds (±1.6 A after a
        // day on a 40 Ah pack), while the peer regression is already accurate to
        // tens of milliamps. They will disagree by construction, every time. A
        // `PeerDisagreement` raised there also hides the real reason the
        // correction is held, and makes any alert on it fire on a healthy pack
        // that has simply not finished warming up — which is exactly what
        // happened on the first day in production.
        let peer = self.peer_report(sn, fit.map(|f| f.slope));
        if hold == Hold::None
            && let Some(p) = &peer
            && p.disagreement > self.opts.peer_max_disagreement_amperes
        {
            // Hold the last good correction rather than reverting to zero: a
            // pack that has been calibrated for weeks is better served by a
            // slightly stale offset than by a sudden step back to raw.
            offset = previous;
            hold = Hold::PeerDisagreement;
        }

        if let Some(dev) = self.state.devices.get_mut(sn) {
            dev.applied = offset;
        }
        let live = self.live.entry(sn.to_string()).or_default();
        live.hold = hold;
        live.peer = peer;
    }

    /// Worst-disagreeing usable peer pair for `sn`, if any.
    fn peer_report(&self, sn: &str, own_estimate: Option<f64>) -> Option<PeerReport> {
        let own = own_estimate?;
        let mut worst: Option<PeerReport> = None;
        for other in self.state.devices.keys() {
            if other == sn {
                continue;
            }
            let (key, flipped) = pair_key(sn, other);
            let Some(fit) = self.state.pairs.get(&key) else {
                continue;
            };
            if fit.weight() < self.opts.peer_min_weight {
                continue;
            }
            let Some(f) = fit.fit() else { continue };
            if (f.slope - 1.0).abs() > self.opts.peer_slope_tolerance
                || f.r2 < self.opts.peer_min_r2
            {
                continue;
            }
            // The stored fit reads `larger = slope·smaller + intercept`, so the
            // intercept is `offset(larger) − offset(smaller)`. Flip the sign when
            // `sn` is the smaller of the two.
            let relative = if flipped { f.intercept } else { -f.intercept };
            let Some(peer_estimate) = self
                .state
                .devices
                .get(other)
                .and_then(|d| d.fit.fit())
                .map(|pf| pf.slope)
            else {
                continue;
            };
            let disagreement = (relative - (own - peer_estimate)).abs();
            let report = PeerReport {
                peer_sn: other.clone(),
                relative_offset: relative,
                slope: f.slope,
                r2: f.r2,
                disagreement,
            };
            if worst.as_ref().is_none_or(|w| disagreement > w.disagreement) {
                worst = Some(report);
            }
        }
        worst
    }

    /// Everything the metrics layer needs to export for `sn`.
    #[must_use]
    pub fn report(&self, sn: &str) -> Option<Report> {
        let dev = self.state.devices.get(sn)?;
        let fit = dev.fit.fit();
        let live = self.live.get(sn);
        Some(Report {
            applied: dev.applied,
            estimate: fit.map(|f| f.slope),
            stderr: fit.map(|f| f.stderr),
            r2: fit.map(|f| f.r2),
            span_hours: dev.span_hours,
            anchor_error: anchor_error(dev.capacity_ah, dev.span_hours, self.opts.tau_hours),
            weight: dev.fit.weight(),
            hold: live.map_or(Hold::Warmup, |l| l.hold),
            peer: live.and_then(|l| l.peer.clone()),
        })
    }

    /// Snapshot for persistence.
    #[must_use]
    pub fn state(&self) -> State {
        self.state.clone()
    }

    /// Reload a snapshot, dropping any serial the caller no longer admits (the
    /// `max_devices` cap is enforced by the caller, which knows about it).
    pub fn restore(&mut self, state: State, admit: impl Fn(&str) -> bool) {
        self.state.devices = state
            .devices
            .into_iter()
            .filter(|(sn, _)| admit(sn.as_str()))
            .collect();
        self.state.pairs = state
            .pairs
            .into_iter()
            .filter(|(key, _)| key.split('|').all(&admit))
            .collect();
    }

    /// Serials the calibrator currently tracks.
    pub fn devices(&self) -> impl Iterator<Item = &String> {
        self.state.devices.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a synthetic pack: constant load profile, a true offset injected
    /// into the reported current, and a `Q` that tracks the true charge.
    struct Sim {
        cal: Calibrator,
        t: f64,
        q: f64,
    }

    impl Sim {
        fn new(opts: Options) -> Self {
            Self {
                cal: Calibrator::new(opts),
                t: 1_000_000.0,
                q: 20.0,
            }
        }

        /// One frame `dt` seconds later carrying `true_a` of real current, with
        /// `offset` added by the sensor and `balance_a` of bleed.
        fn step(&mut self, sn: &str, true_a: f64, offset: f64, balance_a: f64, dt: f64) -> f64 {
            self.t += dt;
            // Real charge moves with the true current minus what the balancer
            // burns; the BMS reports that as remaining capacity.
            self.q += (true_a - balance_a) * dt / 3600.0;
            self.cal.observe(
                sn,
                Sample {
                    current_a: true_a + offset,
                    balance_a: Some(balance_a),
                    remaining_ah: Some(self.q),
                    capacity_ah: Some(40.0),
                    now_secs: self.t,
                },
            )
        }
    }

    fn fast_opts() -> Options {
        Options {
            min_span_hours: 24.0,
            // The production gate needs ~800 integrated hours on a 40 Ah pack;
            // these tests run five simulated days, so they assert the estimator
            // rather than the warmup schedule (which has its own test).
            max_anchor_error_amperes: 10.0,
            peer_min_weight: 100.0,
            ..Options::default()
        }
    }

    #[test]
    fn ew_fit_recovers_a_known_line() {
        let mut f = EwFit::default();
        for i in 0..100 {
            let x = f64::from(i);
            f.push(x, 3.0 * x - 7.0);
        }
        let fit = f.fit().expect("100 points on a line must fit");
        assert!((fit.slope - 3.0).abs() < 1e-9, "slope {}", fit.slope);
        assert!((fit.intercept + 7.0).abs() < 1e-9);
        assert!(fit.r2 > 0.999_999);
    }

    #[test]
    fn ew_fit_shift_x_is_exact() {
        let mut a = EwFit::default();
        let mut b = EwFit::default();
        for i in 0..50 {
            let x = 1000.0 + f64::from(i);
            let y = 0.25 * x + 3.0;
            a.push(x, y);
            b.push(x, y);
        }
        b.shift_x(1000.0);
        let (fa, fb) = (a.fit().expect("a"), b.fit().expect("b"));
        // The slope is origin-invariant; the intercept moves by slope*d.
        assert!((fa.slope - fb.slope).abs() < 1e-9);
        assert!((fb.intercept - (fa.intercept + 0.25 * 1000.0)).abs() < 1e-6);
    }

    #[test]
    fn ew_fit_needs_spread_and_samples() {
        let mut f = EwFit::default();
        assert!(f.fit().is_none(), "empty fit must not produce a line");
        for _ in 0..10 {
            f.push(5.0, 1.0);
        }
        assert!(
            f.fit().is_none(),
            "no spread along x must not produce a line"
        );
    }

    #[test]
    fn recovers_the_injected_offset() {
        let mut sim = Sim::new(fast_opts());
        // Five days of 60 s frames, alternating charge and discharge so the true
        // net stays near zero, with a +0.2 A sensor error.
        let mut applied = 0.0;
        for i in 0..(5 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            applied = sim.step("SN1", true_a, 0.2, 0.0, 60.0);
        }
        assert!(
            (applied - 0.2).abs() < 0.02,
            "expected ~+0.2 A, got {applied}"
        );
    }

    #[test]
    fn balancer_bleed_is_not_mistaken_for_sensor_offset() {
        let mut sim = Sim::new(fast_opts());
        // No sensor error at all, but a 0.5 A balancer running constantly. The
        // bleed is reported, so the closure must attribute nothing to the sensor.
        let mut applied = 0.0;
        for i in 0..(5 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            applied = sim.step("SN1", true_a, 0.0, 0.5, 60.0);
        }
        assert!(
            applied.abs() < 0.02,
            "reported bleed must not land in the offset, got {applied}"
        );
    }

    #[test]
    fn unreported_loss_lands_in_the_offset() {
        // Same bleed, but the frame does not report it: the closure has no other
        // place to put it, and says so. This is the documented behaviour that
        // the peer check exists to flag.
        let mut cal = Calibrator::new(fast_opts());
        let (mut t, mut q) = (1_000_000.0, 20.0);
        let mut applied = 0.0;
        for i in 0..(5 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            t += 60.0;
            q += (true_a - 0.5) * 60.0 / 3600.0;
            applied = cal.observe(
                "SN1",
                Sample {
                    current_a: true_a,
                    balance_a: None,
                    remaining_ah: Some(q),
                    capacity_ah: Some(40.0),
                    now_secs: t,
                },
            );
        }
        assert!(
            (applied - 0.5).abs() < 0.05,
            "unreported loss must surface as offset, got {applied}"
        );
    }

    #[test]
    fn warmup_applies_nothing() {
        let mut sim = Sim::new(Options::default());
        let mut applied = 0.0;
        // One day, against the default 72 h span gate.
        for i in 0..(24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            applied = sim.step("SN1", true_a, 0.3, 0.0, 60.0);
        }
        assert_eq!(applied, 0.0, "must not correct before the span gate opens");
        let r = sim.cal.report("SN1").expect("report");
        assert_eq!(r.hold, Hold::Warmup);
        assert!(r.estimate.is_some(), "the estimate is published while held");
    }

    #[test]
    fn offset_is_clamped() {
        let opts = Options {
            max_offset_amperes: 0.1,
            ..fast_opts()
        };
        let mut sim = Sim::new(opts);
        let mut applied = 0.0;
        for i in 0..(5 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            applied = sim.step("SN1", true_a, 0.4, 0.0, 60.0);
        }
        assert!(
            (applied - 0.1).abs() < 1e-9,
            "must clamp to the configured ceiling, got {applied}"
        );
    }

    #[test]
    fn a_gap_longer_than_max_gap_is_not_integrated_as_time() {
        // A pack that goes offline for a day must not have that day counted as
        // integrated time: otherwise the slope is diluted toward zero.
        let mut sim = Sim::new(fast_opts());
        for i in 0..(3 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            sim.step("SN1", true_a, 0.2, 0.0, 60.0);
        }
        let before = sim.cal.report("SN1").expect("report").span_hours;
        // 24 h outage, then one frame.
        sim.step("SN1", 0.0, 0.2, 0.0, 86_400.0);
        let after = sim.cal.report("SN1").expect("report").span_hours;
        assert!(
            after - before <= 900.0 / 3600.0 + 1e-9,
            "outage added {} h of span",
            after - before
        );
    }

    #[test]
    fn peer_regression_measures_the_relative_offset() {
        let mut cal = Calibrator::new(fast_opts());
        let (mut t, mut q1, mut q2) = (1_000_000.0, 20.0, 20.0);
        for i in 0..(5 * 24 * 60) {
            // A smooth 6 h swing rather than a square wave: the two packs post
            // ~60 s apart, and pairing samples across a current step is
            // errors-in-variables noise that attenuates the fitted slope. Real
            // load profiles move continuously, so this is the honest shape.
            let true_a = 8.0 * (f64::from(i) * std::f64::consts::TAU / 360.0).sin();
            t += 60.0;
            q1 += true_a * 60.0 / 3600.0;
            q2 += true_a * 60.0 / 3600.0;
            // SN1 reads 0.1 A high, SN2 reads 0.1 A low: a 0.2 A difference.
            cal.observe(
                "SN1",
                Sample {
                    current_a: true_a + 0.1,
                    balance_a: Some(0.0),
                    remaining_ah: Some(q1),
                    capacity_ah: Some(40.0),
                    now_secs: t,
                },
            );
            cal.observe(
                "SN2",
                Sample {
                    current_a: true_a - 0.1,
                    balance_a: Some(0.0),
                    remaining_ah: Some(q2),
                    capacity_ah: Some(40.0),
                    now_secs: t + 1.0,
                },
            );
        }
        let r1 = cal.report("SN1").expect("SN1");
        let peer = r1.peer.expect("SN1 must have found SN2");
        assert_eq!(peer.peer_sn, "SN2");
        assert!((peer.slope - 1.0).abs() < 0.05, "slope {}", peer.slope);
        assert!(
            (peer.relative_offset - 0.2).abs() < 0.02,
            "relative offset {}",
            peer.relative_offset
        );
        // Both estimators agree here, so nothing is frozen.
        assert!(
            peer.disagreement < 0.05,
            "disagreement {}",
            peer.disagreement
        );
        assert_eq!(r1.hold, Hold::None);
    }

    #[test]
    fn peer_disagreement_freezes_the_correction() {
        let opts = Options {
            peer_max_disagreement_amperes: 0.05,
            ..fast_opts()
        };
        let mut cal = Calibrator::new(opts);
        let (mut t, mut q1, mut q2) = (1_000_000.0, 20.0, 20.0);
        let mut applied = 0.0;
        for i in 0..(6 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            t += 60.0;
            // SN1 additionally leaks 0.4 A that nothing reports: the closure
            // blames its sensor, the peer regression sees identical sensors.
            q1 += (true_a - 0.4) * 60.0 / 3600.0;
            q2 += true_a * 60.0 / 3600.0;
            applied = cal.observe(
                "SN1",
                Sample {
                    current_a: true_a,
                    balance_a: Some(0.0),
                    remaining_ah: Some(q1),
                    capacity_ah: Some(40.0),
                    now_secs: t,
                },
            );
            cal.observe(
                "SN2",
                Sample {
                    current_a: true_a,
                    balance_a: Some(0.0),
                    remaining_ah: Some(q2),
                    capacity_ah: Some(40.0),
                    now_secs: t + 1.0,
                },
            );
        }
        let r1 = cal.report("SN1").expect("SN1");
        assert_eq!(
            r1.hold,
            Hold::PeerDisagreement,
            "a loss the peers do not see must freeze, not calibrate away"
        );
        assert!(
            applied.abs() < 0.2,
            "the frozen value must be the pre-disagreement one, got {applied}"
        );
        assert!(
            r1.estimate.expect("estimate") > 0.3,
            "the estimate itself is still published"
        );
    }

    #[test]
    fn a_disagreeing_peer_does_not_mask_the_warmup_hold() {
        // Regression test for the first day in production: during warmup the
        // closure slope is noise bounded by `anchor_error` while the peer
        // regression is already sharp, so the two disagree by construction. The
        // veto must not fire there — it would report the wrong reason for a
        // correction that warmup is holding anyway, and page on a healthy pack.
        let opts = Options {
            peer_max_disagreement_amperes: 0.05,
            peer_min_weight: 100.0,
            // Everything else at production defaults, so the warmup gate is shut.
            ..Options::default()
        };
        let mut cal = Calibrator::new(opts);
        let (mut t, mut q1, mut q2) = (1_000_000.0, 20.0, 20.0);
        for i in 0..(6 * 24 * 60) {
            let true_a = 8.0 * (f64::from(i) * std::f64::consts::TAU / 360.0).sin();
            t += 60.0;
            // SN1 leaks 0.4 A that nothing reports: the closure blames its
            // sensor, the peer regression sees two identical sensors.
            q1 += (true_a - 0.4) * 60.0 / 3600.0;
            q2 += true_a * 60.0 / 3600.0;
            for (sn, q, skew) in [("SN1", q1, 0.0), ("SN2", q2, 1.0)] {
                cal.observe(
                    sn,
                    Sample {
                        current_a: true_a,
                        balance_a: Some(0.0),
                        remaining_ah: Some(q),
                        capacity_ah: Some(40.0),
                        now_secs: t + skew,
                    },
                );
            }
        }
        let r = cal.report("SN1").expect("SN1");
        assert_eq!(
            r.hold,
            Hold::Warmup,
            "warmup is the binding gate and must be the reported one"
        );
        let peer = r.peer.expect("the peer report is still published");
        assert!(
            peer.disagreement > 0.05,
            "the test is pointless unless the veto would otherwise have fired, got {}",
            peer.disagreement
        );
    }

    #[test]
    fn state_round_trips() {
        let mut sim = Sim::new(fast_opts());
        for i in 0..(5 * 24 * 60) {
            let true_a = if (i / 60) % 2 == 0 { 8.0 } else { -8.0 };
            sim.step("SN1", true_a, 0.2, 0.0, 60.0);
        }
        let before = sim.cal.report("SN1").expect("before");
        let json = serde_json::to_vec(&sim.cal.state()).expect("serialize");
        let state: State = serde_json::from_slice(&json).expect("deserialize");

        let mut restored = Calibrator::new(fast_opts());
        restored.restore(state, |_| true);
        let after = restored.report("SN1").expect("after");
        assert!((before.applied - after.applied).abs() < 1e-12);
        assert!(
            (before.estimate.expect("b") - after.estimate.expect("a")).abs() < 1e-12,
            "the fit must survive a restart"
        );
        assert!((before.span_hours - after.span_hours).abs() < 1e-12);
    }

    #[test]
    fn restore_honours_the_admission_filter() {
        let mut sim = Sim::new(fast_opts());
        for i in 0..200 {
            let true_a = if i % 2 == 0 { 8.0 } else { -8.0 };
            sim.step("SN1", true_a, 0.2, 0.0, 60.0);
            sim.step("SN2", true_a, 0.2, 0.0, 60.0);
        }
        let state = sim.cal.state();
        let mut restored = Calibrator::new(fast_opts());
        restored.restore(state, |sn| sn == "SN1");
        assert!(restored.report("SN1").is_some());
        assert!(
            restored.report("SN2").is_none(),
            "a rejected serial must not be restored"
        );
        assert!(
            restored.state().pairs.is_empty(),
            "a pair naming a rejected serial must not be restored"
        );
    }

    #[test]
    fn frames_without_remaining_capacity_still_integrate() {
        // `Q` is what anchors the fit; without it there is nothing to fit, but
        // the running integrals must keep up so the next anchored frame is
        // consistent.
        let mut cal = Calibrator::new(fast_opts());
        let mut t = 1_000_000.0;
        for _ in 0..100 {
            t += 60.0;
            cal.observe(
                "SN1",
                Sample {
                    current_a: 5.0,
                    balance_a: None,
                    remaining_ah: None,
                    capacity_ah: Some(40.0),
                    now_secs: t,
                },
            );
        }
        let r = cal.report("SN1").expect("report");
        assert_eq!(r.hold, Hold::NoFit);
        assert_eq!(r.applied, 0.0);
        assert!(r.span_hours > 1.0, "integration must still advance");
    }
}
