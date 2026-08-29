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

use anyhow::{Context, Result};

use crate::config::Config;

/// Parse CLI and run the service.
pub fn run(cfg: Config) -> Result<()> {
    if cfg.show_token {
        return match config::show_token()? {
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

/// How long a graceful shutdown may take before the process exits anyway.
///
/// `with_graceful_shutdown` waits for every in-flight connection to finish,
/// but an SSE stream never ends on its own — without an upper bound, Ctrl+C
/// would hang for as long as a client stays subscribed. The SSE handler also
/// watches the shutdown channel and closes its own stream, so this is the
/// safety net rather than the normal path.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

async fn serve(cfg: Config) -> Result<()> {
    serve_with_shutdown(cfg, async {
        tokio::signal::ctrl_c().await.ok();
    })
    .await
}

/// `serve`, with the shutdown trigger injected.
///
/// Exists so the shutdown path is testable: a real `CTRL_C_EVENT` cannot be
/// delivered to another process from a test on Windows, and the original bug
/// here was precisely that no signal handler was installed at all — the
/// process died before it could log or release the watcher handles. Tests
/// drive this with a channel instead of a signal.
pub async fn serve_with_shutdown(
    cfg: Config,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // Create the data dir up front so a permission problem surfaces here
    // rather than as a silently swallowed failure inside media export.
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| {
        format!("创建数据目录失败: {}", cfg.data_dir.display())
    })?;
    let token = config::load_token()?;
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState::new(
        cfg.clone(),
        token.clone(),
        shutdown_tx,
    ));
    // Platform scan for discovery only: zero accounts is a valid start state,
    // and a client registers them with keys via POST /api/v1/accounts.
    //
    // Only the COUNT is logged, never the wxids. `/health` and `account_views`
    // pay a type-level price to avoid enumerating accounts without a token
    // (`AccountPhase` has no `AwaitingKey` variant precisely so a discovered
    // account cannot leak through the unauthenticated endpoint) — printing the
    // full list here would route around that for anyone who can read the log.
    // The list itself stays available to authenticated callers via
    // `GET /api/v1/accounts`, which merges `discovered` into its response.
    let found = db::scan::scan_all(&db::scan::default_roots());
    if found.is_empty() {
        tracing::info!("[init] 未发现本机微信账号目录（客户端可显式传 db_path 注册）");
    } else {
        tracing::info!(
            "[init] 发现 {} 个账号目录，等待注册（清单见 GET /api/v1/accounts，需鉴权）",
            found.len()
        );
    }
    state.set_discovered(found);

    let app = server::build_router(state.clone());
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "[init] 服务启动: http://{addr}  (API token 存于系统凭据库; 仅首次生成时打印; --show-token 获取)"
    );
    tracing::info!("[init] 等待客户端注册账号: POST /api/v1/accounts {{\"wxid\", \"key\", \"db_path\"}}");

    // Signal the watchers (and the SSE streams) the moment Ctrl+C lands, then
    // let axum drain. `drain_tx` tells the grace timer when to start counting.
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let signal_state = state.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal.await;
        tracing::info!("收到退出信号，清理中…");
        // Stops the per-account watch tasks (releasing their directory
        // handles) and ends every live SSE stream.
        let _ = signal_state.shutdown.send(true);
        let _ = drain_tx.send(());
    });

    tokio::select! {
        result = server => result?,
        _ = async {
            // Only start the clock once shutdown was actually requested;
            // if the sender is dropped without a signal (server ended on its
            // own) this branch must never win the select.
            match drain_rx.await {
                Ok(()) => tokio::time::sleep(SHUTDOWN_GRACE).await,
                Err(_) => std::future::pending::<()>().await,
            }
        } => {
            tracing::warn!(
                "退出宽限期 {:?} 已到，仍有连接未结束，强制退出",
                SHUTDOWN_GRACE
            );
        }
    }
    Ok(())
}
