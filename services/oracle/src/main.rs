//! `oracle` runs the service; `oracle ready [URL]` exits 0 only when the
//! service at URL (default `http://127.0.0.1:8080/ready`) answers `/ready`
//! with 200, for container healthchecks.

use oracle::config::AppConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(cmd) = args.next() {
        anyhow::ensure!(
            cmd == "ready",
            "unknown command {cmd:?} (usage: oracle [ready [URL]])"
        );
        let url = args.next();
        return oracle::api::probe_ready(url.as_deref().unwrap_or(oracle::api::DEFAULT_READY_URL))
            .await;
    }
    // Two rustls crypto providers are compiled in (ring via tungstenite,
    // aws-lc-rs via reqwest); without an explicit process default, websocket
    // TLS panics at connect time.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = dotenvy::dotenv();
    let cfg: AppConfig = mtm_common::config::load()?;
    mtm_telemetry::init("oracle", cfg.oracle.metrics_addr)?;
    oracle::run(cfg).await
}
