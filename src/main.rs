use clap::Parser;
use metaxy::config::{Args, Config};
use metaxy::Server;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let mut cfg = match &args.config {
        Some(path) => match Config::from_file(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("metaxy: {e}");
                std::process::exit(2);
            }
        },
        None => Config::default(),
    };
    args.apply(&mut cfg);
    // The world store: --world-store (or the file), then METAXY_WORLD_STORE,
    // then a temporary folder. A folder that cannot be used falls back to
    // the temporary one when the server starts; the relay always starts.
    if cfg.world_store.is_empty() {
        cfg.world_store = std::env::var("METAXY_WORLD_STORE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "temp".into());
    }
    if let Err(e) = metaxy::logging::init(&cfg.log_level, cfg.log_json) {
        eprintln!("metaxy: {e}");
        std::process::exit(2);
    }
    let server = match Server::start(cfg).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("metaxy: {e}");
            std::process::exit(1);
        }
    };
    if let Some(a) = server.addr {
        tracing::info!(addr = %a, mode = "plain", "listening");
    }
    if let Some(a) = server.tls_addr {
        tracing::info!(addr = %a, mode = "tls", "listening");
    }
    wait_for_stop().await;
    tracing::info!("stopping");
    server.shutdown().await;
}

/// Wait for Ctrl+C, or SIGTERM where there is one.
async fn wait_for_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
