use oracle::config::AppConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Two rustls crypto providers are compiled in (ring via tungstenite,
    // aws-lc-rs via reqwest); without an explicit process default, websocket
    // TLS panics at connect time.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = dotenvy::dotenv();
    let cfg: AppConfig = mtm_common::config::load()?;
    mtm_telemetry::init("oracle", cfg.oracle.metrics_addr)?;
    oracle::run(cfg).await
}
