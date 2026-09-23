//! Replay recorded production telemetry through [`Calibrator`] and print what it
//! would have concluded.
//!
//! This is the only way to check the estimator against reality rather than
//! against a simulation: the unit tests feed it clean synthetic ramps, while a
//! real bank brings quantisation, gaps, a saturated `Q` and a balancer that may
//! or may not be telling the truth. Re-run it after any change to the estimator.
//!
//! Input is a JSON dump of three Prometheus range queries, shaped
//! `{"<metric>": {"<series>": [[unix_ts, value], ...]}}`, holding
//! `daly_bms_current_amperes`, `daly_bms_balance_current_amperes` and
//! `daly_bms_remaining_capacity_amp_hours`. Produce one with:
//!
//! ```sh
//! curl -s --data-urlencode 'query=daly_bms_current_amperes' \
//!      --data-urlencode "start=$(date -d '-7 days' +%s)" \
//!      --data-urlencode "end=$(date +%s)" --data-urlencode 'step=60' \
//!      http://127.0.0.1:9090/api/v1/query_range
//! ```
//!
//! then merge the three responses' `data.result` into the shape above (Prometheus
//! caps a single range query at 11000 points, so long windows need chunking).
//!
//! ```sh
//! cargo run --example replay_history -- history.json
//! ```

use std::collections::BTreeMap;

use daly_bms_exporter::calibration::{Calibrator, Options, Sample};

type Series = BTreeMap<String, Vec<(f64, f64)>>;
/// Per-device lookup of the two metrics a current sample needs to join against.
type Companions = BTreeMap<u64, f64>;
/// One `(timestamp, applied, estimate)` row per device at one trace point.
type TracePoint = (f64, Vec<(String, f64, f64)>);

/// One metric's worth of `{series: [(ts, value)]}`, as dumped from Prometheus.
fn take(doc: &serde_json::Value, metric: &str) -> Series {
    doc.get(metric)
        .and_then(serde_json::Value::as_object)
        .map(|series| {
            series
                .iter()
                .map(|(name, points)| {
                    let pts = points
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|p| {
                                    let p = p.as_array()?;
                                    Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (name.clone(), pts)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: replay_history <history.json>");
        std::process::exit(2);
    });
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read history"))
            .expect("parse history");

    let current = take(&doc, "daly_bms_current_amperes");
    let balance = take(&doc, "daly_bms_balance_current_amperes");
    let remaining = take(&doc, "daly_bms_remaining_capacity_amp_hours");

    // Index the secondary metrics by timestamp so each current sample can pick
    // up the balancer reading and remaining capacity of the same instant.
    let lookup: BTreeMap<&String, (Companions, Companions)> = current
        .keys()
        .map(|sn| {
            let idx = |s: &Series| -> Companions {
                s.get(sn)
                    .map(|v| v.iter().map(|(t, x)| (*t as u64, *x)).collect())
                    .unwrap_or_default()
            };
            (sn, (idx(&balance), idx(&remaining)))
        })
        .collect();

    // Interleave every device's samples in timestamp order, the way the ingest
    // handler sees them — the peer regression depends on that interleaving.
    let mut stream: Vec<(f64, &String, f64)> = current
        .iter()
        .flat_map(|(sn, pts)| pts.iter().map(move |(t, v)| (*t, sn, *v)))
        .collect();
    stream.sort_by(|a, b| a.0.total_cmp(&b.0));

    // Print the running estimate on this cadence, so a convergence that only
    // looks stable at the end cannot hide a swing in the middle.
    const TRACE_INTERVAL_SECS: f64 = 3.0 * 86_400.0;

    let mut cal = Calibrator::new(Options::default());
    let mut next_trace = stream.first().map_or(f64::MAX, |s| s.0) + TRACE_INTERVAL_SECS;
    let mut trace: Vec<TracePoint> = Vec::new();
    for (ts, sn, current_a) in &stream {
        let (bal, rem) = &lookup[sn];
        cal.observe(
            sn,
            Sample {
                current_a: *current_a,
                balance_a: bal.get(&(*ts as u64)).copied(),
                remaining_ah: rem.get(&(*ts as u64)).copied(),
                capacity_ah: None,
                now_secs: *ts,
            },
        );
        if *ts >= next_trace {
            next_trace = *ts + TRACE_INTERVAL_SECS;
            let row = cal
                .devices()
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|sn| {
                    let r = cal.report(&sn)?;
                    Some((sn, r.applied, r.estimate.unwrap_or(f64::NAN)))
                })
                .collect();
            trace.push((*ts, row));
        }
    }

    println!("== running estimate (applied / estimate, amperes) ==");
    for (ts, row) in &trace {
        let cells: Vec<String> = row
            .iter()
            .map(|(sn, applied, estimate)| format!("{sn} {applied:>+7.4}/{estimate:>+7.4}"))
            .collect();
        println!(
            "  day {:>5.1}  {}",
            (ts - stream[0].0) / 86_400.0,
            cells.join("  ")
        );
    }
    println!();

    println!(
        "{:<12} {:>9} {:>10} {:>8} {:>7} {:>9} {:>6} {:>8}  hold",
        "device", "applied", "estimate", "anchor", "R2", "span_h", "peer", "disagree"
    );
    let names: Vec<String> = cal.devices().cloned().collect();
    for sn in names {
        let r = cal.report(&sn).expect("device just replayed");
        let peer = r.peers.first();
        println!(
            "{:<12} {:>+9.4} {:>+10.4} {:>8.4} {:>7.4} {:>9.1} {:>+6.3} {:>8.3}  {:?}",
            sn,
            r.applied,
            r.estimate.unwrap_or(f64::NAN),
            r.anchor_error,
            r.r2.unwrap_or(f64::NAN),
            r.span_hours,
            peer.map_or(f64::NAN, |p| p.relative_offset),
            r.peer_disagreement.unwrap_or(f64::NAN),
            r.hold,
        );
    }
}
