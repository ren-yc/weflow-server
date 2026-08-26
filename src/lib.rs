//! weflow-server: headless WeChat 4.x database monitor + decrypt/extract service.
//!
//! Module layout mirrors qqflow-server (reference architecture) with the
//! WeChat-specific pieces rewritten (WCDB/SQLCipher-4 page cipher, `db_storage`
//! layout, `Msg_<md5>` message tables, XML/zstd content parsing).

pub mod config;
pub mod db;
pub mod keystore;
pub mod logging;
pub mod media;
pub mod parser;
pub mod server;
pub mod store;
pub mod sync;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;

use crate::config::Config;

/// Parse CLI and run the service.
pub fn run(cfg: Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(cfg))
}

async fn serve(cfg: Config) -> Result<()> {
    let token = server::load_token(&cfg.data_dir)?;
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState {
        cfg: cfg.clone(),
        token,
        accounts: Mutex::new(HashMap::new()),
        shutdown: shutdown_tx,
    });
    let app = server::build_router(state);
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "weflow-server {} listening on http://{addr}  (token: {})",
        env!("CARGO_PKG_VERSION"),
        cfg.data_dir.join("token.txt").display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
