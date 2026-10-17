//! Tracing init（level 来自 `--log`，format 来自 `--log-format`）。
//!
//! # 为什么 subscriber 必须装在 `dispatch` 里、而不是只装在 serve 分支
//!
//! 子命令（export／sync／mcp…）把会话级失败降级成 `skipped` 时写的是 `tracing::warn!`。
//! 没有 subscriber 的进程里 tracing 宏是 no-op —— 用户只剩退出码 1 与一行「跳过: <会话>」，
//! **原因永久不可见**。所以 CLI 分流在动手执行任何子命令之前先装日志。
//! 回归位置（两仓同形）：
//! - `tests/cli_e2e.rs::subcommand_failures_are_logged_on_stderr`（warn 级默认，失败原因进 stderr）
//! - `tests/cli_e2e.rs::log_json_format_emits_parseable_lines`（`--log-format json` 逐行可解析）
//! - `tests/cli.rs::serve_log_flag_still_goes_through_the_old_parser`（serve 的 `--log` 归老解析器）
//! - `tests/cli.rs::serve_logs_stay_on_stdout`（serve 维持 stdout）
//!
//! # 谁装、何时装（两条互斥路径）
//!
//! - **子命令**：`cli::dispatch` 在执行任何动作前装 `init_with_format`（warn 级、stderr）。
//! - **serve**（含裸跑与老旗标）：`dispatch` 在构造 clap **之前**就把 `serve` 短路进老解析器，
//!   级别与格式因此完全由 `Config.log`/`Config.log_format` 决定，`lib.rs` 的 serve 分支装
//!   `init_for_serve`。默认 info 级别与 stdout 去向不变（行内自本批起多了模块 target 段，
//!   那是「恢复 target」条的效果，不属于入口分叉）。
//!
//! 两个入口都返回 `bool`（是否真的装了）：装法是 `try_init`——**已有 subscriber 时装不上、
//! 但不 panic**（`init` 会 panic）。同进程内嵌场景（宿主程序自己装了 subscriber）里，本模块
//! 的安装退化为 no-op、宿主的 subscriber 原样保留，这个事实由返回值可查而不是靠运气。
//!
//! # 输出去向：子命令 stderr，serve stdout（刻意的分叉）
//!
//! `fmt()` 的**默认 writer 是 stdout**。两个入口的 stdout 契约不同，去向因此不同：
//!
//! - **子命令的 stdout 是命令输出本身**（`--json` 的机器可读形状，或人类摘要行）。日志必须
//!   走 stderr，否则 `sessions --json | jq` 拿到的是半份 JSON 流。判据：
//!   `tests/cli_e2e.rs::log_json_format_emits_parseable_lines` 断言 stderr 逐行可解析、
//!   **且 stdout 没有日志行**。
//! - **serve 的 stdout 没有输出契约**，而既有部署就在用 `weflow-server > 日志文件` 收服务日志
//!   ——改通道会让那些脚本拿到空文件，这属于破坏性变更，不在本批范围。serve 维持 stdout。
//!
//! 已知代价（如实登记）：同一次运行里 `[fatal] {e:#}` 走 stderr、serve 普通日志走 stdout，
//! 「只看 `2>` 收错误」的采集侧拿不到 `[init]` 上下文。两通道各自的语义写在上面；合并成
//! 一条通道要等一个愿意接受破坏性的版本窗口。
//!
//! # 语言口径（红线）
//!
//! 日志行分两类，归类固定：
//! - **面向运维的用户句**（人在终端直接读的 info/warn，带 `[init]`/`[export]` 这类前缀的
//!   输出行同族）：**中文**。
//! - **模块级诊断**（给读代码的人定位内部机制的 warn/error/debug）：**英文**，如
//!   `watcher exited`。
//!
//! 新增日志拿不准时看同模块邻居的既有语言。存量只在明显违例时改，不为统一而全量重写。
//!
//! # 脱敏红线
//!
//! 日志行**禁止**出现：完整数据库密钥、完整 API token、消息正文。
//! 例外只有两处，都在 serve 路径，共同点是「不打印就没有第二次拿到的机会」：
//! - `[init] 生成新 API token:`（info 级）——只在**首次生成**时打印一次，值随后可从本机
//!   凭据库取回（`keystore` 与 `config::show_token` 的注释声明了同一例外）；
//! - `[init] 凭据库不可用 …: {t}`（warn 级）——此时 token 不落库、重启即变，不打印就没有
//!   这个值了（`config::load_token` 的注释声明了它）。
//!
//! 子命令的日志同样不得出现 token；`token` 子命令把值写到 stdout，那是命令输出而不是日志。
//! 透传上游错误 Display 的行（媒体下载失败等）允许点名状态码与 URL，禁止带出请求体。

/// 子命令形态：指定格式，输出写 **stderr**（stdout 留给命令输出，见模块头）。
///
/// json 形态是**逐行一个 JSON 对象**（tracing 的 JsonFormatter，含 level/target/fields），
/// 给日志采集器消费。text 形态是 tracing 默认行：`时间 级别 target: 消息`（实测
/// `2026-10-17T09:36:17Z DEBUG reqwest::connect: …`）——本批起多了 target 段；行首的
/// `[init]`/`[fatal]` 是**消息自带的前缀**，不是 formatter 的产物。除 `"json"` 外一律按 text 处理。
///
/// 门控：调用点在 `cli` feature 里；不开 `cli` 时本函数没有调用者，
/// 而 `--no-default-features` 的嵌入构建下 `logging` 是 `pub(crate)` —— 不加这条豁免
/// 会撞上 dead_code。
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub fn init_with_format(level: &str, format: &str) -> bool {
    install(level, format, true)
}

/// serve 形态：指定格式，输出写 **stdout**（既有 `> 日志文件` 部署的重定向习惯，维持不动）。
///
/// 为什么 serve 不用 stderr：普通部署在用 `weflow-server > 日志文件` 收服务日志，把日志挪到
/// stderr 会让那些脚本静默拿到**空文件**——这是破坏性变更，不在本批（子命令的 stdout 有输出
/// 契约、serve 的没有，所以两者去向不同；已知代价与判据见模块头）。
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub fn init_for_serve(level: &str, format: &str) -> bool {
    install(level, format, false)
}

fn install(level: &str, format: &str, to_stderr: bool) -> bool {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    // target 必须开着：模块很多（store/sync/server/media/watch…），关掉后日志行没有
    // 「谁说的」，排障只能靠猜。text 与 json 两种格式都带。
    //
    // 为什么两支各自成链而不是在一条 builder 上条件设 writer：`with_writer` 会把 builder 的
    // 泛型参数换成对应 writer（`Stderr` 与 `Stdout` 是两个不同类型），if/else 交出不同类型
    // 编译不过（这个错由门禁实测）。stdout 那支**显式写出**而不依赖 `fmt()` 的默认 ——
    // 默认值是库的实现细节，而「serve 的日志在 stdout」是本批要钉住的行为
    // （判据：`tests/cli.rs::serve_logs_stay_on_stdout`）。
    let json = format == "json";
    if to_stderr {
        let builder = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_writer(std::io::stderr);
        if json {
            builder.json().try_init().is_ok()
        } else {
            builder.try_init().is_ok()
        }
    } else {
        let builder = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_writer(std::io::stdout);
        if json {
            builder.json().try_init().is_ok()
        } else {
            builder.try_init().is_ok()
        }
    }
}
