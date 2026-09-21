//! End-to-end handler tests: POST a synthetic telemetry body, then scrape
//! `/metrics` and assert the decoded series appear.
//!
//! NOTE: the frame is synthetic (built from known register values), so this
//! verifies wiring, serde renames, dispatch and formulas — but NOT that the
//! register offsets match a real device. Capture a real POST into
//! `tests/fixtures/` for a true golden test (see the plan / README risks).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use daly_bms_exporter::config::Config;
use daly_bms_exporter::metrics::Metrics;
use daly_bms_exporter::server::{AppState, router};
use http_body_util::BodyExt;
use tower::ServiceExt; // oneshot

/// Build a hex response ADU from registers. The `modbus` builder is
/// `pub(crate)` and unreachable from an integration test, so the framing is
/// local — but the CRC uses the crate's `pub` `crc16` rather than a copy.
fn build_frame(regs: &[u16]) -> String {
    let mut body = vec![0xD2, 0x03, (regs.len() * 2) as u8];
    for r in regs {
        body.extend_from_slice(&r.to_be_bytes());
    }
    body.extend_from_slice(&daly_bms_exporter::modbus::crc16(&body).to_le_bytes());
    hex::encode(body)
}

/// A 126-register realtime block with a couple of known values.
///
/// `current_raw` must be set explicitly: the encoding is `(raw - 30000) * 0.1`,
/// so leaving the register at zero would mean -3000 A — a value the
/// plausibility gate rejects, which would make current-related assertions pass
/// or fail for the wrong reason.
fn realtime_frame_with_current(current_raw: u16) -> String {
    let mut regs = vec![0u16; 126];
    regs[0] = 2195; // cell 1 = 2.195 V
    regs[0x28] = 0x010D; // pack voltage -> 26.9 V
    regs[0x29] = current_raw;
    regs[0x2A] = 0x034A; // SOC -> 84.2 %
    regs[0x31] = 1; // cell count
    build_frame(&regs)
}

/// The same block at a steady 10 A charge (raw 30100).
fn realtime_frame() -> String {
    realtime_frame_with_current(30_100)
}

fn app(config: Config) -> axum::Router {
    let state = AppState {
        metrics: Arc::new(Metrics::new((&config).into())),
        config: Arc::new(config),
    };
    router(state)
}

async fn post_json(app: &axum::Router, path: &str, body: String) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

async fn scrape(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn telemetry_updates_metrics() {
    let app = app(Config::default());
    let body = format!(
        r#"{{"DeviceName":"dev","Sn":"SN1","Data":[{{"Command":"D2030000007ED649","Data":"{}"}}]}}"#,
        realtime_frame()
    );
    assert_eq!(
        post_json(&app, "/api/v2/http2/SaveThingInfo1", body).await,
        StatusCode::OK
    );
    let metrics = scrape(&app).await;
    assert!(metrics.contains("daly_bms_pack_voltage_volts{sn=\"SN1\"} 26.9"));
    assert!(metrics.contains("daly_bms_soc_percent{sn=\"SN1\"} 84.2"));
    // Prometheus sorts labels alphabetically, so match components order-agnostically.
    let cell_line = metrics
        .lines()
        .find(|l| l.starts_with("daly_bms_cell_voltage_volts{"))
        .expect("cell voltage series present");
    assert!(cell_line.contains("sn=\"SN1\""));
    assert!(cell_line.contains("cell=\"01\""));
    assert!(cell_line.ends_with(" 2.195"));
}

#[tokio::test]
async fn corrupt_frame_still_returns_200_and_keeps_good_frame() {
    let app = app(Config::default());
    let body = format!(
        r#"{{"Sn":"SN2","Data":[
            {{"Command":"D2030000007ED649","Data":"D203FCDEADBEEF"}},
            {{"Command":"D2030000007ED649","Data":"{}"}}
        ]}}"#,
        realtime_frame()
    );
    assert_eq!(
        post_json(&app, "/api/v2/http2/SaveThingInfo1", body).await,
        StatusCode::OK
    );
    let metrics = scrape(&app).await;
    assert!(metrics.contains("daly_bms_pack_voltage_volts{sn=\"SN2\"} 26.9"));
    assert!(metrics.contains("daly_bms_frames_dropped_total"));
}

#[tokio::test]
async fn rejected_serial_creates_no_series() {
    let config = Config {
        allowed_serials: Some(vec!["ONLY_THIS".to_string()]),
        ..Config::default()
    };
    let app = app(config);
    let body = format!(
        r#"{{"Sn":"INTRUDER","Data":[{{"Command":"D2030000007ED649","Data":"{}"}}]}}"#,
        realtime_frame()
    );
    assert_eq!(
        post_json(&app, "/api/v2/http2/SaveThingInfo1", body).await,
        StatusCode::OK
    );
    let metrics = scrape(&app).await;
    assert!(!metrics.contains("sn=\"INTRUDER\""));
    assert!(metrics.contains("daly_bms_frames_dropped_total{reason=\"serial_rejected\"}"));
}

/// Value of a `{sn="SN1"}` series in the exposition text, if present.
fn series_value(metrics: &str, name: &str, sn: &str) -> Option<f64> {
    let prefix = format!("{name}{{sn=\"{sn}\"}}");
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .map(|v| v.trim().parse().unwrap())
}

async fn post_realtime(app: &axum::Router, sn: &str, frame: &str) {
    let body =
        format!(r#"{{"Sn":"{sn}","Data":[{{"Command":"D2030000007ED649","Data":"{frame}"}}]}}"#);
    assert_eq!(
        post_json(app, "/api/v2/http2/SaveThingInfo1", body).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn implausible_current_frame_is_dropped_whole() {
    // Regression for the 2026-09-21 production spike: one CRC-valid frame with
    // current register 0x863E (436.6 A) in an otherwise sane block. The gauge
    // showed it for a whole post interval and every derived power panel spiked.
    let app = app(Config::default());
    post_realtime(&app, "SN1", &realtime_frame_with_current(30_100)).await; // +10 A
    let before = scrape(&app).await;
    assert_eq!(
        series_value(&before, "daly_bms_current_amperes", "SN1"),
        Some(10.0)
    );

    post_realtime(&app, "SN1", &realtime_frame_with_current(0x863E)).await;
    let metrics = scrape(&app).await;
    assert_eq!(
        series_value(&metrics, "daly_bms_current_amperes", "SN1"),
        Some(10.0),
        "gauge took the garbage current: {metrics}"
    );
    assert!(
        metrics.contains("daly_bms_frames_dropped_total{reason=\"implausible_current\"} 1"),
        "gate did not count the dropped frame: {metrics}"
    );
    assert!(
        !metrics.contains("daly_bms_coulomb_samples_rejected_total{reason=\"implausible"),
        "implausible frames are dropped, not counted as rejected samples: {metrics}"
    );
    // Dropped whole: nothing from that frame is a "decoded" frame either.
    assert!(
        metrics.contains("daly_bms_frames_decoded_total{block=\"realtime\"} 1"),
        "dropped frame was counted as decoded: {metrics}"
    );
    // The POST itself still counts as contact with the device.
    assert!(series_value(&metrics, "daly_bms_last_frame_timestamp_seconds", "SN1").is_some());
}

#[tokio::test]
async fn implausible_current_frame_does_not_reach_the_energy_counters() {
    let app = app(Config::default());
    // Good, garbage, good. A single garbage frame would prove nothing on its
    // own: the first frame of a device only sets the integration baseline, so
    // "no energy accumulated" would hold with the gate switched off too.
    let frames = [
        realtime_frame_with_current(30_100), // +10 A
        realtime_frame_with_current(0xFFFF), // (65535-30000)*0.1 = 3553.5 A
        realtime_frame_with_current(30_100), // +10 A again
    ];
    for f in frames {
        post_realtime(&app, "SN1", &f).await;
    }

    let metrics = scrape(&app).await;
    assert!(
        metrics.contains("daly_bms_frames_dropped_total{reason=\"implausible_current\"} 1"),
        "gate did not report the garbage frame: {metrics}"
    );
    // The three POSTs happen within milliseconds, so the integrated amount is
    // negligible — what matters is that 3553 A never entered the integral.
    let charged = series_value(&metrics, "daly_bms_charge_amp_hours_total", "SN1").unwrap_or(0.0);
    assert!(
        charged < 0.01,
        "garbage current was integrated: {charged} Ah"
    );
}

#[tokio::test]
async fn implausible_voltage_frame_is_dropped_whole() {
    let app = app(Config::default());
    let mut regs = vec![0u16; 126];
    regs[0x28] = 0xFFFF; // 6553.5 V
    regs[0x29] = 30_100; // +10 A, fine on its own
    regs[0x2A] = 0x034A;
    post_realtime(&app, "SN1", &build_frame(&regs)).await;
    let metrics = scrape(&app).await;
    assert!(
        series_value(&metrics, "daly_bms_current_amperes", "SN1").is_none(),
        "a frame with a garbage voltage must not set any gauge: {metrics}"
    );
    assert!(series_value(&metrics, "daly_bms_soc_percent", "SN1").is_none());
    assert!(metrics.contains("daly_bms_frames_dropped_total{reason=\"implausible_voltage\"} 1"));
}

#[tokio::test]
async fn drop_reasons_are_exported_before_any_drop() {
    // A reason series born at its first increment is invisible to
    // `increase()`; the alert on implausible frames relies on the zero baseline.
    let app = app(Config::default());
    let metrics = scrape(&app).await;
    for reason in daly_bms_exporter::metrics::DROP_REASONS {
        assert!(
            metrics.contains(&format!(
                "daly_bms_frames_dropped_total{{reason=\"{reason}\"}} 0"
            )),
            "missing zero baseline for {reason}: {metrics}"
        );
    }
}

#[tokio::test]
async fn healthz_ok() {
    let app = app(Config::default());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn http_requests_recorded_by_endpoint_and_status() {
    let app = app(Config::default());
    // A telemetry POST (recorded as endpoint="telemetry", status=200).
    let body = format!(
        r#"{{"Sn":"SN1","Data":[{{"Command":"D2030000007ED649","Data":"{}"}}]}}"#,
        realtime_frame()
    );
    post_json(&app, "/api/v2/http2/SaveThingInfo1", body).await;
    // An unmatched path (404) maps to endpoint="other" — bounded label.
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let metrics = scrape(&app).await;
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("daly_bms_http_requests_total{")
                && l.contains("endpoint=\"telemetry\"")
                && l.contains("status=\"200\""))
    );
    assert!(
        metrics
            .lines()
            .any(|l| l.contains("endpoint=\"other\"") && l.contains("status=\"404\"")),
        "unmatched path should record endpoint=other status=404"
    );
}
