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

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;

/// Parse CLI and run the service.
pub fn run(cfg: Config) -> Result<()> {
    if cfg.show_token {
        return match server::show_token()? {
            Some(t) => {
                println!("{t}");
                Ok(())
            }
            None => anyhow::bail!(
                "尚未生成 API token（先启动一次服务以生成）"
            ),
        };
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(cfg))
}

async fn serve(cfg: Config) -> Result<()> {
    let token = server::load_token()?;
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState::new(
        cfg.clone(),
        token.clone(),
        shutdown_tx,
    ));
    let app = server::build_router(state);
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "[init] 服务启动: http://{addr}  (API token 存于系统凭据库; 仅首次生成时打印; --show-token 获取)"
    );
    tracing::info!("[init] 等待客户端注册账号: POST /api/v1/accounts {{\"wxid\", \"key\", \"db_path\"}}");
    let _ = token;
    axum::serve(listener, app).await?;
    Ok(())
}
