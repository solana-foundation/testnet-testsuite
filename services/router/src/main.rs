use router::config::AppConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // two rustls providers are compiled in (ring via ws, aws-lc-rs via reqwest)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = dotenvy::dotenv();
    let cfg: AppConfig = mtm_common::config::load()?;
    mtm_telemetry::init("router", cfg.router.metrics_addr)?;
    router::run(cfg).await
}
