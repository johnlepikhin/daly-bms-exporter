//! HTTP surface: device ingest endpoints, `/metrics`, `/healthz`, and the
//! router assembly with body-limit / timeout / catch-panic layers.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::{debug, trace, warn};

use crate::config::Config;
use crate::decode::{Block, ConfigData, RealtimeData};
use crate::metrics::Metrics;
use crate::modbus;
use crate::payload::TelemetryBody;

#[derive(Clone)]
pub struct AppState {
    pub metrics: Arc<Metrics>,
    pub config: Arc<Config>,
}

/// Build the router with all routes and middleware layers.
pub fn router(state: AppState) -> Router {
    let max_body = state.config.max_body_bytes;
    let timeout = Duration::from_secs(state.config.request_timeout_secs);
    let metrics_path = state.config.metrics_path.clone();
    let mw_state = state.clone();

    Router::new()
        .route("/api/v2/http2/SaveThingInfo1", post(telemetry))
        .route("/api/v2/http2/SaveThing", post(registration))
        .route(&metrics_path, get(metrics))
        .route("/healthz", get(healthz))
        .with_state(state)
        // Untrusted device input: cap body size, bound request time, and never
        // let a decode panic take down the worker.
        .layer(DefaultBodyLimit::max(max_body))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .layer(CatchPanicLayer::new())
        // Outermost layer: record the final status of every request, including
        // the statuses produced by the layers above (413 body-limit, 408
        // timeout, 500 catch-panic), which never reach a handler.
        .layer(middleware::from_fn_with_state(mw_state, record_metrics))
}

/// Middleware recording `http_requests_total{endpoint,status}` for every
/// request. The endpoint label is mapped to a fixed set of known routes so an
/// attacker probing random paths cannot explode label cardinality.
async fn record_metrics(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let endpoint = if path == "/api/v2/http2/SaveThingInfo1" {
        "telemetry"
    } else if path == "/api/v2/http2/SaveThing" {
        "registration"
    } else if path == st.config.metrics_path {
        "metrics"
    } else if path == "/healthz" {
        "healthz"
    } else {
        "other"
    };
    let resp = next.run(req).await;
    st.metrics.record_request(endpoint, resp.status().as_u16());
    resp
}

/// Telemetry ingest. Always returns 200 so the device firmware (which expects a
/// 200 from databms.com) does not spin on retries.
async fn telemetry(State(st): State<AppState>, body: Bytes) -> impl IntoResponse {
    trace!(bytes = body.len(), "telemetry body received");
    let body: TelemetryBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = ?e, bytes = body.len(), "unparseable telemetry body");
            st.metrics.record_dropped("bad_json");
            return StatusCode::OK;
        }
    };

    if !st.config.accept_serial(&body.sn) {
        warn!(sn = ?body.sn, "rejected serial (not accepted / implausible)");
        for _ in 0..body.data.len().max(1) {
            st.metrics.record_dropped("serial_rejected");
        }
        return StatusCode::OK;
    }

    // Bound metric cardinality: refuse new serials once max_devices is reached.
    if !st.metrics.admit(&body.sn) {
        warn!(sn = ?body.sn, "device limit reached, dropping frame");
        for _ in 0..body.data.len().max(1) {
            st.metrics.record_dropped("device_limit");
        }
        return StatusCode::OK;
    }

    debug!(
        sn = ?body.sn,
        device = ?body.device_name,
        frames = body.data.len(),
        "telemetry accepted"
    );
    for (index, entry) in body.data.iter().enumerate() {
        if let Err(reason) = handle_entry(&st, &body.sn, &entry.command, &entry.data) {
            warn!(
                sn = ?body.sn,
                index,
                reason,
                command = ?entry.command,
                data_len = entry.data.len(),
                "frame dropped"
            );
            st.metrics.record_dropped(reason);
        }
    }
    st.metrics.mark_seen(&body.sn);
    StatusCode::OK
}

/// Decode one Modbus pair and update metrics. Returns a drop-reason on failure.
fn handle_entry(st: &AppState, sn: &str, command: &str, data: &str) -> Result<(), &'static str> {
    let start = modbus::request_start_register(command).map_err(|e| {
        tracing::debug!(sn = ?sn, error = ?e, "bad command frame");
        "bad_command"
    })?;
    let block = Block::from_start_register(start).map_err(|e| {
        tracing::debug!(sn = ?sn, error = ?e, "unknown block");
        "unknown_block"
    })?;
    let regs = modbus::parse_response(data).map_err(|e| {
        tracing::debug!(sn = ?sn, error = ?e, "modbus parse error");
        drop_reason(e)
    })?;
    trace!(sn = ?sn, ?block, registers = regs.len(), "frame parsed");

    match block {
        Block::Realtime => {
            let d = RealtimeData::from_registers(&regs);
            // The realtime block carries the pack serial; warn if it disagrees
            // with the body's Sn (the label source of truth).
            if let Some(decoded) = &d.serial {
                if decoded != sn {
                    warn!(body_sn = ?sn, decoded_sn = ?decoded, "serial mismatch");
                }
            }
            debug!(
                sn = ?sn,
                pack_v = ?d.pack_v,
                current_a = ?d.current_a,
                soc_pct = ?d.soc_pct,
                cells = d.cells_v.len(),
                temps = d.temps_c.len(),
                alarm_bits = ?d.alarm_bits,
                "realtime frame decoded"
            );
            st.metrics.update_realtime(sn, &d);
            st.metrics
                .accumulate_coulombs(sn, d.current_a, crate::metrics::now_unix_secs());
            st.metrics.record_decoded("realtime");
        }
        Block::Config => {
            let c = ConfigData::from_registers(&regs);
            debug!(
                sn = ?sn,
                rated_capacity_ah = ?c.rated_capacity_ah,
                balance_enable = ?c.balance_enable,
                machine_code = ?c.machine_code,
                "config frame decoded"
            );
            st.metrics.update_config(sn, &c);
            st.metrics.record_decoded("config");
        }
    }
    Ok(())
}

/// Map a decode error to a metric drop-reason label.
fn drop_reason(e: crate::error::DecodeError) -> &'static str {
    use crate::error::DecodeError::*;
    match e {
        BadHex(_) => "bad_hex",
        TooShort { .. } => "too_short",
        BadAddress(_) | BadFunction(_) | BadByteCount { .. } => "bad_frame",
        CrcMismatch { .. } => "crc",
        UnknownBlock(_) => "unknown_block",
    }
}

/// Device registration metadata (form-urlencoded). We only acknowledge it, but
/// log the payload since it carries device identity (versions, machine code).
async fn registration(body: Bytes) -> impl IntoResponse {
    debug!(metadata = ?String::from_utf8_lossy(&body), "device registration");
    StatusCode::OK
}

async fn metrics(State(st): State<AppState>) -> impl IntoResponse {
    trace!("metrics scrape");
    let (content_type, body) = st.metrics.render();
    let mut headers = HeaderMap::new();
    if let Ok(v) = content_type.parse() {
        headers.insert(header::CONTENT_TYPE, v);
    }
    (StatusCode::OK, headers, body)
}

async fn healthz() -> impl IntoResponse {
    StatusCode::OK
}
