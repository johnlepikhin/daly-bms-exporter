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
fn realtime_frame() -> String {
    let mut regs = vec![0u16; 126];
    regs[0] = 2195; // cell 1 = 2.195 V
    regs[0x28] = 0x010D; // pack voltage -> 26.9 V
    regs[0x2A] = 0x034A; // SOC -> 84.2 %
    regs[0x31] = 1; // cell count
    build_frame(&regs)
}

fn app(config: Config) -> axum::Router {
    let state = AppState {
        metrics: Arc::new(Metrics::new(
            config.coulomb_max_gap_secs as f64,
            config.max_devices,
            config.coulomb_state_path.clone(),
        )),
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
