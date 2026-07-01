#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use daly_bms_exporter::config::Config;
use daly_bms_exporter::metrics::Metrics;
use daly_bms_exporter::server::{AppState, router};

/// Prometheus exporter for Daly BMS telemetry.
///
/// Listens for raw Modbus frames forwarded from the Hlktech WiFi module (see
/// `doc/daly-bms-protocol.md`) and exposes decoded metrics.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the YAML configuration file.
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    // RUST_LOG wins; otherwise fall back to the configured level.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::debug!(
        config_path = %cli.config.display(),
        listen = %config.listen,
        metrics_path = %config.metrics_path,
        max_body_bytes = config.max_body_bytes,
        request_timeout_secs = config.request_timeout_secs,
        allowlisted_serials = config.allowed_serials.as_ref().map_or(0, Vec::len),
        "configuration loaded"
    );

    let metrics = Arc::new(Metrics::new(
        config.coulomb_max_gap_secs as f64,
        config.max_devices,
        config.coulomb_state_path.clone(),
    ));
    // Restore persisted coulomb totals (if configured) before serving scrapes.
    metrics.restore_coulombs();

    let state = AppState {
        metrics: metrics.clone(),
        config: Arc::new(config.clone()),
    };
    let app = router(state);

    let listener = match tokio::net::TcpListener::bind(config.listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, listen = %config.listen, "failed to bind listen address");
            return Err(e.into());
        }
    };
    tracing::info!(listen = %config.listen, metrics = %config.metrics_path, "daly-bms-exporter started");

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(error = %e, "server error");
        metrics.persist_coulombs();
        return Err(e.into());
    }
    // Persist the latest coulomb totals on graceful shutdown.
    metrics.persist_coulombs();
    tracing::info!("daly-bms-exporter stopped");
    Ok(())
}

/// Resolve on Ctrl-C or SIGTERM (systemd stop).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
