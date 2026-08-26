//! One-call tracing + metrics setup, identical across all binaries.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::EnvFilter;

/// Initialize tracing and (optionally) a Prometheus scrape endpoint.
/// Call once at the top of every binary, inside the tokio runtime.
/// Log level comes from RUST_LOG (default "info").
pub fn init(service: &'static str, metrics_addr: Option<SocketAddr>) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Some(addr) = metrics_addr {
        PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()?;
        tracing::info!(%addr, "prometheus scrape endpoint listening");
    }

    tracing::info!(service, "telemetry initialized");
    Ok(())
}
