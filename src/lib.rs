//! weflow-server: headless WeChat 4.x database monitor + decrypt/extract service.
//!
//! # 两条使用路径
//!
//! - **起服务**：`cargo run`（或 `cargo install weflow-server`）——默认 feature 就是它。
//! - **当库用**：`default-features = false`，然后按需开 feature。**必须显式关掉默认 feature**，
//!   否则会连带拉进 axum 与 tokio。嵌入者从 [`api`] 入手。
//!
//! # 文档门
//!
//! `#![deny(missing_docs)]` 开着。它**天然只作用在承诺面**：[`api`] —— 因为其余模块在默认构建下
//! 是 `pub(crate)`，而 missing_docs 只看得到 `pub`。于是「补 rustdoc」从愿望变成了可验证的门。

#![deny(missing_docs)]
//!
//! Module layout mirrors qqflow-server (reference architecture) with the
//! WeChat-specific pieces rewritten (WCDB/SQLCipher-4 page cipher, `db_storage`
//! layout, `Msg_<md5>` message tables, XML/zstd content parsing).

// 实现面：默认 **`pub(crate)`**（边界由编译器强制，外部不可达）；
// `--features testing` 下转 `pub` —— 集成测试在独立 crate 里，只能看见 `pub`。
//
// 承诺面是 [`api`]，见它的模块文档。
macro_rules! internal {
    ($($m:ident),* $(,)?) => {
        $(
            // `testing` 那一支**豁免文档门**：它把实现面转成 `pub` 只是为了让集成测试够得着，
            // 不是对外承诺 —— 要求它逐项写 rustdoc 会把 370 处内部项变成文档债，而它们随时会变。
            // 文档门（`#![deny(missing_docs)]`）因此**始终只作用在承诺面 [`api`] 上**。
            #[cfg(feature = "testing")]
            #[allow(missing_docs)]
            pub mod $m;
            #[cfg(not(feature = "testing"))]
            pub(crate) mod $m;
        )*
    };
}

/// 嵌入者承诺面 —— 本 crate **唯一**的对外承诺。
///
/// 它始终可用（不随任何 feature 开关），因为「读自己的聊天记录」是最小可用面。
pub mod api;

// 核心：只读数据访问与解析。不依赖 tokio，也不依赖 axum。
internal!(config, db, keystore, logging, parser, pathsafe, store);

// 可选面：关掉即从依赖树里消失。
#[cfg(feature = "media")]
internal!(media);
#[cfg(feature = "sync")]
internal!(sync);
#[cfg(feature = "server")]
internal!(server);

/// 命令行子命令面（`cli` feature）。
///
/// `pub(crate)`：CLI 是**二进制的面**，不是嵌入者的承诺面——把它做成 `pub` 会让只服务于
/// `run_cli` 分流的类型进入 semver 契约。
#[cfg(feature = "cli")]
pub(crate) mod cli;

/// 造库/造密钥夹具 —— **不是承诺面**，只随 `testing` feature 编译。
///
/// 落点为什么在库里而不是 `tests/common`：需要它的有两个调用方，而它们互相
/// 看不见对方的代码 —— 集成测试是独立 crate（链库），**根包二进制同样是独立
/// crate**，批量导出的夹具生成入口（CLI 的 `--rows`）够不着只在 `tests/` 下
/// 存在的模块。一份造库器、两个调用方、一个 feature 门。
///
/// 豁免文档门：它随 `testing` 编译，语义等同上面 `internal!` 的 testing 分支
/// （把实现面转 `pub` 只为让本仓自己的二进制与集成测试够得着），不是对外承诺。
#[cfg(feature = "testing")]
#[allow(missing_docs)]
pub mod testing;

#[cfg(feature = "server")]
use std::sync::Arc;

#[cfg(feature = "server")]
use anyhow::{Context, Result};

#[cfg(feature = "server")]
use crate::config::Config;

/// CLI 入口：分流子命令、初始化日志、起服务。
///
/// **二进制走这里，而不是直接用 `config`/`logging`。** 原因是一个容易被忽略的事实：
/// `src/main.rs` 是**独立 crate**，只能看见 `pub` —— 而实现面默认是 `pub(crate)`（边界由编译器
/// 强制）。所以「连自家二进制也得走承诺面」不是麻烦，正是这条边界在起作用：它证明承诺面**够用**，
/// 嵌入者能做的事，二进制没有多一分。
///
/// 需要 `server` feature —— 它建 tokio 运行时并起 HTTP 服务。
#[cfg(all(feature = "server", feature = "cli"))]
pub fn run_cli() -> Result<()> {
    match cli::dispatch()? {
        cli::Entry::Serve(cfg) => {
            logging::init(&cfg.log);
            run(cfg)
        }
        // 子命令已经把活干完（token / 查询 / sync），或 --help/--version 已经打印过。
        cli::Entry::Done => Ok(()),
    }
}

/// 同上，但 `cli` 关掉时只剩「旗标」这一条老路。
///
/// 保留这个分支是有意的：`--no-default-features` 的依赖树必须不含 clap 与 SDK（CI 的 embed
/// 钉子守着），所以那条路上不能引用 `cli` 模块，只能直接走 `config::load()`。
#[cfg(all(feature = "server", not(feature = "cli")))]
pub fn run_cli() -> Result<()> {
    let Some(cfg) = config::load()? else {
        return Ok(()); // --help / --version 已经打印过了
    };
    logging::init(&cfg.log);
    run(cfg)
}

/// Parse CLI and run the service.
///
/// 需要 `server` feature —— 它建 tokio 运行时并起 HTTP 服务。
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
