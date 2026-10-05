//! 命令行子命令面（`cli` feature）。
//!
//! # 这层为什么存在
//!
//! 服务跑起来之后，用户第二类要做的事是「问一句」：有哪些会话、某个会话最近的消息、服务端现在
//! 绑的是哪个账号、要不要立刻同步一次。以前这些只能手打 HTTP 或另写脚本；本面把它们收进同一个
//! 二进制。
//!
//! # 默认走 HTTP，复用 SDK
//!
//! 查询一律经 `weflow_client::client::Client`，**不在本 crate 里再写一份 HTTP**。失败模式很具体：
//! 自己拼请求的那一份不会被 SDK 的契约测试钉住——服务端改了字段名或错误语义时只有 SDK 的调用方
//! 会变红，而 CLI 会安静地读出错了的东西（同一仓库里两份实现各自漂移）。
//!
//! `--embedded`（进程内直读本地库，不起也不打 HTTP）**只开放给只读查询类**；`accounts` 与 `sync`
//! 刻意没有这个开关：前者要回答的是「服务端此刻实际绑定了什么」，后者是**写动作**——进程内路径
//! 给出的不是同一个问题的答案。
//!
//! # 兼容口径（裸跑仍＝serve）
//!
//! - 不带任何参数、或以旗标（`-` 开头）开头的写法**原样交给既有解析器**：`weflow-server
//!   --port 6002` 一个字符都不用改，`--help`／`--version`／`--show-token`／配置文件加载的行为
//!   全部逐字不变。
//! - `serve` 子命令 = 裸跑；`serve` 之后的旗标同样交给老解析器，因此 `serve --port 6002` 与
//!   `--port 6002` 等价。
//! - `token` 子命令 = 既有的 `--show-token`。
//! - 第一个参数既不是旗标也不是已知子命令时，**让 clap 报**「未知子命令」并以 2 退出：老解析器
//!   在这种情况下只会说「参数 bogus 缺少值」，那是把用法错误伪装成取值错误。
//!
//! # 退出码
//!
//! `0` 成功；`1` 运行期错误（连不上、被拒、缺密钥）；`2` 用法错误（未知子命令、缺参数）。`2` 由
//! clap 自己给出，本面不吞——降级成 1 会让脚本分不出「我参数写错了」和「服务没起来」。
//!
//! # 密钥边界
//!
//! API token 与数据库密钥**只从环境变量或磁盘配置**取，绝不接受命令行明文参数：命令行会进 shell
//! history 与进程列表，而这两个值合起来能读出整份聊天记录。

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use weflow_client::client::{Client, ContactsQuery, MessageQuery};
use crate::export;

use crate::api;
use crate::config::Config;

/// 顶层：一个子命令。`name` 固定成二进制名，因为 `dispatch` 用 `parse_from` 手工喂 argv
/// （只有第一个参数不是旗标时才走到这里）。
#[derive(Parser)]
#[command(name = "weflow-server", version, about = "本地微信 4.x 消息库只读服务与查询命令行")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 起服务（与不带子命令时的行为完全一致）
    Serve,
    /// 打印已存的 API token 并退出（等价于 `--show-token`）
    Token,
    /// 会话列表
    Sessions(ReadArgs),
    /// 某个会话的消息（时间倒序）
    Messages(MessageArgs),
    /// 按关键词搜消息（`--keyword` 必填）
    Search(MessageArgs),
    /// 联系人列表
    Contacts(ReadArgs),
    /// 账号明细：绑定方、状态机值、消息数、`error` 根因
    Accounts(HttpArgs),
    /// 让服务端立刻跑一次增量同步（写动作）
    Sync(HttpArgs),
    /// 批量导出：把会话写成 ChatLab Format 的 JSONL / JSON 落盘
    Export(ExportArgs),
    /// 以 MCP（stdio）方式暴露只读查询工具，供 agent 客户端调用
    #[cfg(feature = "mcp")]
    Mcp(McpArgs),
}

/// `mcp` 的参数。刻意只有服务地址：token 只从环境变量取（与其它子命令同一口径），
/// 而 MCP 进程不碰数据库密钥 —— 那是服务端的事。
#[cfg(feature = "mcp")]
#[derive(clap::Args)]
struct McpArgs {
    /// 服务地址；默认取环境变量 WEFLOW_BASE_URL，再默认 http://127.0.0.1:5033
    #[arg(long, env = "WEFLOW_BASE_URL")]
    base_url: Option<String>,
}

/// 只读查询类子命令的共用参数（带 `--embedded`）。
#[derive(clap::Args)]
struct ReadArgs {
    #[command(flatten)]
    common: Common,
    /// 进程内直读本地库，不经过 HTTP（仅只读查询类可用）
    #[arg(long)]
    embedded: bool,
}

#[derive(clap::Args)]
struct MessageArgs {
    #[command(flatten)]
    common: Common,
    /// 会话 ID（`…@chatroom` 为群）
    #[arg(long)]
    talker: Option<String>,
    /// 起始时间：unix 秒或 `YYYYMMDD`（含边界）
    #[arg(long)]
    since: Option<String>,
    /// 关键词子串
    #[arg(long)]
    keyword: Option<String>,
    /// 单页条数（服务端上限 10000）
    #[arg(long)]
    limit: Option<u32>,
    /// 进程内直读本地库，不经过 HTTP（仅只读查询类可用）
    #[arg(long)]
    embedded: bool,
}

/// 只有 HTTP 形态的子命令：刻意没有 `--embedded`（见模块头的边界说明）。
#[derive(clap::Args)]
struct HttpArgs {
    #[command(flatten)]
    common: Common,
}

/// `export` 的参数。
///
/// 两条硬约束的落点：`--embedded` **不提供**——批量导出只走 HTTP（服务端已经把密钥握在
/// 内存里，CLI 只做编排与落盘；否则一个长任务会长时间持有密钥，还得把密钥带上命令行）。
/// `--with-media` 把字节下载到导出目录下的 `media/`，而**导出物里不写任何 URL**。
#[derive(clap::Args)]
struct ExportArgs {
    /// 输出目录（必需：落盘是有意的动作，不给默认路径）
    #[arg(long)]
    out: PathBuf,
    /// 格式：jsonl（流式、内存与条数无关）或 json（每会话一个完整信封）
    #[arg(long, default_value = "jsonl", value_parser = ["jsonl", "json"])]
    format: String,
    /// 只导这些会话（可重复）
    #[arg(long)]
    session: Vec<String>,
    /// 起始时间：unix 秒或 YYYYMMDD
    #[arg(long)]
    since: Option<String>,
    /// 已存在的会话文件跳过（幂等续跑）
    #[arg(long)]
    resume: bool,
    /// 下载本会话用到的媒体字节到 <out>/media/，并把导出物里的 fileName 限定为确实落盘的句柄
    #[arg(long)]
    with_media: bool,
    #[command(flatten)]
    common: Common,

    /// 测试专用的语料生成入口：**不进 --help，且只在 testing feature 下编译**。
    ///
    /// 为什么是隐藏参数而不是文档化能力：它存在的唯一理由，是让「大语料下内存恒定」那条
    /// 断言能在 CI 里造出足够大的语料——小语料上「把整会话读进内存」的实现一样看不出来。
    /// 把它暴露给用户会诱导人拿它当真库用，而它造的是假账号；发布二进制里没有这个参数。
    #[cfg(feature = "testing")]
    #[arg(long, hide = true)]
    rows: Option<usize>,
}

#[derive(clap::Args)]
struct Common {
    /// 服务地址；默认取环境变量 `WEFLOW_BASE_URL`，再默认 `http://127.0.0.1:5033`
    #[arg(long, env = "WEFLOW_BASE_URL")]
    base_url: Option<String>,
    /// 输出机器可读 JSON（默认是人类可读的紧凑行）
    #[arg(long)]
    json: bool,
}

/// `run_cli` 需要的结果：要么按配置起服务，要么本次命令行已经办完、直接退出。
///
/// `pub(crate)` 而不是 `pub`：CLI 面不属于承诺面（嵌入者用不到 `Entry`），而承诺面每多
/// 一个类型就多一份 semver 债。
pub(crate) enum Entry {
    Serve(Config),
    Done,
}

/// 分流入口。返回 `Entry::Done` 表示不该起服务（含 `--help`／`--version`）。
pub(crate) fn dispatch() -> Result<Entry> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(first) = argv.first() else {
        // 裸跑：走 `config::load()` 这条**今天就在用**的入口，连读取环境变量与配置文件的
        // 顺序都不动 —— 「行为逐字不变」不能靠「看起来一样」来保证。
        return legacy_load();
    };
    if first.starts_with('-') {
        // 老写法：行为逐字不变。
        return legacy(&argv);
    }
    if first == "serve" {
        // serve 之后的旗标**原样**交给老解析器：`serve --port 6002` 必须与 `--port 6002`
        // 等价。让它先进 clap 的话，`--port` 会被当成未知子命令的参数而报错——那是把既有
        // 写法弄坏，而不是新增能力。
        return legacy(&argv[1..]);
    }
    let cli = Cli::parse_from(std::iter::once("weflow-server".to_string()).chain(argv.clone()));
    match cli.command {
        // serve 之后的旗标交给老解析器：`serve --port 6002` 与 `--port 6002` 必须等价。
        Command::Serve => legacy(&argv[1..]),
        Command::Token => match crate::config::show_token()? {
            Some(t) => {
                println!("{t}");
                Ok(Entry::Done)
            }
            None => anyhow::bail!("尚未生成 API token（先启动一次服务以生成）"),
        },
        Command::Sessions(q) => {
            let rows = if q.embedded {
                embedded_sessions()?
            } else {
                let client = http_client(&q.common)?;
                block(client.list_all_sessions(Some(10_000), None))?
                    .iter()
                    .map(|s| {
                        json!({
                            "username": s.username,
                            "displayName": s.display_name,
                            "sessionType": s.session_type,
                            "lastTimestamp": s.last_timestamp,
                            "messageCount": s.message_count,
                            "unreadCount": s.unread_count,
                        })
                    })
                    .collect()
            };
            emit(&Value::Array(rows), q.common.json, "sessions");
            Ok(Entry::Done)
        }
        Command::Contacts(q) => {
            let rows = if q.embedded {
                embedded_contacts()?
            } else {
                let client = http_client(&q.common)?;
                let page = block(client.contacts(&ContactsQuery::default()))?;
                page.contacts
                    .iter()
                    .map(|c| {
                        json!({
                            "username": c.username,
                            "displayName": c.display_name,
                            "remark": c.remark,
                            "nickname": c.nickname,
                            "alias": c.alias,
                        })
                    })
                    .collect()
            };
            emit(&Value::Array(rows), q.common.json, "contacts");
            Ok(Entry::Done)
        }
        Command::Messages(m) => {
            let rows = run_messages(&m)?;
            emit(&Value::Array(rows), m.common.json, "messages");
            Ok(Entry::Done)
        }
        Command::Search(m) => {
            if m.keyword.as_deref().unwrap_or("").is_empty() {
                usage_error("search 需要 --keyword（要列全部消息请用 messages）");
            }
            let rows = run_messages(&m)?;
            emit(&Value::Array(rows), m.common.json, "messages");
            Ok(Entry::Done)
        }
        Command::Export(a) => {
            #[cfg(feature = "testing")]
            if let Some(rows) = a.rows {
                return export_corpus(&a.out, rows).map(|_| Entry::Done);
            }
            run_export(&a).map(|_| Entry::Done)
        }
        Command::Accounts(q) => {
            let client = http_client(&q.common)?;
            let rows = block(client.accounts())?
                .iter()
                .map(|a| {
                    json!({
                        "wxid": a.wxid,
                        "state": a.state.to_string(),
                        "messageCount": a.message_count,
                        "dbStorage": a.db_storage,
                        "error": a.error,
                    })
                })
                .collect();
            emit(&Value::Array(rows), q.common.json, "accounts");
            Ok(Entry::Done)
        }
        Command::Sync(q) => {
            let client = http_client(&q.common)?;
            let r = block(client.sync_now())?;
            emit(&json!({"success": r.success, "newMessages": r.new_messages, "revokeMessages": r.revoke_messages}), q.common.json, "sync");
            Ok(Entry::Done)
        }
        #[cfg(feature = "mcp")]
        Command::Mcp(a) => {
            let base = a.base_url.unwrap_or_else(|| "http://127.0.0.1:5033".to_string());
            crate::mcp::run(base, env_token()?)?;
            Ok(Entry::Done)
        }
    }
}

/// 裸跑（没有任何参数）的老入口。
fn legacy_load() -> Result<Entry> {
    match crate::config::load()? {
        Some(cfg) => Ok(Entry::Serve(cfg)),
        // --help / --version：既有解析器已经打印过，这里只需退出 0。
        None => Ok(Entry::Done),
    }
}

/// 带旗标的老写法：把参数原样交给既有解析器，行为逐字不变。
fn legacy(argv: &[String]) -> Result<Entry> {
    match crate::config::parse_args(argv.to_vec())? {
        Some(cfg) => Ok(Entry::Serve(cfg)),
        // --help / --version：既有解析器已经打印过，这里只需退出 0。
        None => Ok(Entry::Done),
    }
}

/// 用法错误：交给 clap 打印并以 2 退出（见模块头的退出码约定）。
fn usage_error(msg: &str) -> ! {
    clap::Error::raw(clap::error::ErrorKind::MissingRequiredArgument, msg).exit()
}

/// `messages` 与 `search` 共用。HTTP 形态必须给 `--talker`（服务端按会话查询），
/// 进程内形态可以省略（索引里所有会话都能翻）。
fn run_messages(m: &MessageArgs) -> Result<Vec<Value>> {
    if m.embedded {
        return embedded_messages(m.talker.as_deref(), m.since.as_deref(), m.keyword.as_deref(), m.limit);
    }
    let Some(talker) = m.talker.clone() else {
        usage_error("HTTP 形态的 messages/search 需要 --talker（服务端按会话查询；要跨会话请配 --embedded）")
    };
    let client = http_client(&m.common)?;
    let q = MessageQuery {
        talker,
        keyword: m.keyword.clone(),
        start: m.since.clone(),
        limit: m.limit,
        ..Default::default()
    };
    let page = block(client.list_messages(&q))?;
    Ok(page.messages.iter().map(|r| json!({"serverId": r.server_id, "createTime": r.create_time, "senderName": r.sender_name, "senderUsername": r.sender_username, "content": r.content})).collect())
}

fn http_client(c: &Common) -> Result<Client> {
    let base = c.base_url.clone().unwrap_or_else(|| "http://127.0.0.1:5033".to_string());
    Ok(Client::new(base, env_token()?))
}

/// API token 只从环境变量取：它不经命令行传递，以免落进 shell history 与进程列表。
fn env_token() -> Result<String> {
    std::env::var("WEFLOW_TOKEN").map_err(|_| {
        anyhow::anyhow!("缺少 API token：请设环境变量 WEFLOW_TOKEN（值可用 `weflow-server token` 取）；token 不经命令行传递，以免落进 shell history 与进程列表")
    })
}

/// 与 [`block`] 同形，但面向返回 `anyhow::Result` 的 future：`anyhow::Error` 不实现
/// `std::error::Error`，塞不进 `block` 的约束里（那条约束是为了把 SDK 的错误类型接过来）。
fn block_anyhow<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("建 tokio 运行时失败")?;
    rt.block_on(f)
}

/// SDK 的方法都是 async；子命令是「跑一次就退出」，所以用一个当前线程运行时把 future 拉完。
/// 刻意不建多线程运行时：这里没有并发需求，多花的线程只会拖慢冷启动。
fn block<T, E: std::error::Error + Send + Sync + 'static>(
    f: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("建 tokio 运行时失败")?;
    rt.block_on(f).map_err(anyhow::Error::new)
}

// ---- --embedded：进程内直读 ------------------------------------------------
//
// 密钥来源与 examples/embed.rs 用同一个配置文件（db_path / wxid / keys 那张表）：
// 嵌入者手里通常就是它，不该为 CLI 再发明一种格式。路径由环境变量给出而不是命令行参数——
// 路径本身不是秘密，但把它做成命令行参数会诱导人们顺手把 keys 也做成参数。

const EMBED_CONFIG_ENV: &str = "WEFLOW_EMBED_CONFIG";

fn embedded_index() -> Result<api::Index> {
    let path = std::env::var(EMBED_CONFIG_ENV)
        .map(PathBuf::from)
        .with_context(|| format!("--embedded 需要环境变量 {EMBED_CONFIG_ENV} 指向配置文件"))?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读配置文件失败: {}", path.display()))?;
    let cfg: Value = serde_json::from_str(&raw)
        .with_context(|| format!("配置文件不是合法 JSON: {}", path.display()))?;
    let account_root = PathBuf::from(cfg.get("db_path").and_then(Value::as_str).unwrap_or_default());
    let wxid = cfg.get("wxid").and_then(Value::as_str).unwrap_or("unknown");
    let keys: std::collections::HashMap<String, String> = cfg
        .get("keys")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    if keys.is_empty() {
        anyhow::bail!("配置里没有 keys —— 没有密钥就读不了任何库");
    }
    let keymap = api::KeyMap::from_parts(None, Some(keys))?;
    let storage = account_root.join("db_storage");
    eprintln!("[embedded] 索引 {} …", storage.display());
    let index = api::open(&storage, &keymap, wxid)?;
    eprintln!("[embedded] 建好：{} 个会话", index.sessions().len());
    Ok(index)
}

fn embedded_sessions() -> Result<Vec<Value>> {
    let index = embedded_index()?;
    Ok(index
        .sessions()
        .iter()
        .map(|s| json!({
            "username": s.username,
            "displayName": index.session_display(&s.username),
            "sessionType": kind_name(&s.kind),
            "lastTimestamp": s.last_timestamp,
            "messageCount": s.message_count,
            "unreadCount": s.unread_count,
        }))
        .collect())
}

fn embedded_contacts() -> Result<Vec<Value>> {
    let index = embedded_index()?;
    // 承诺面的 Contact 里备注与昵称是 Option：这里保留「没有就是 null」而不是压成空串——
    // 「有键但值为空」与「没有这个键」的区分在本服务的其它面是契约的一部分，这里不自创例外。
    Ok(index
        .contacts()
        .iter()
        .map(|c| json!({
            "username": c.username,
            "displayName": c.display_name(),
            "remark": c.remark,
            "nickname": c.nickname,
            "alias": c.alias,
        }))
        .collect())
}

fn embedded_messages(
    talker: Option<&str>,
    since: Option<&str>,
    keyword: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<Value>> {
    let index = embedded_index()?;
    let start = match since {
        // 进程内形态只接受 unix 秒：YYYYMMDD 的换算规则（当天零点起、含整天）是服务端解析器定的，
        // 在 CLI 里另写一份就是第二套规则——两份实现迟早给出不同的答案。
        Some(s) => s.parse::<i64>().with_context(|| format!("--since 需为 unix 秒: {s}"))?,
        None => 0,
    };
    let cap = limit.unwrap_or(200) as usize;
    let targets: Vec<String> = match talker {
        Some(t) => vec![t.to_string()],
        None => index.sessions().into_iter().map(|s| s.username).collect(),
    };
    let mut out = Vec::new();
    for t in targets {
        // 与会话/联系人两处同规：HTTP 形态给的是服务端索引的倒序，进程内这里也按时间倒序——
        // 同一个命令换形态不该给出不同的东西。
        let mut msgs = index.messages(&t);
        msgs.sort_by_key(|m| std::cmp::Reverse(m.create_time));
        for msg in msgs {
            if msg.create_time < start {
                continue;
            }
            let text = if msg.parsed.display.is_empty() {
                msg.parsed.parsed_text.as_str()
            } else {
                msg.parsed.display.as_str()
            };
            // 关键词同时命中展示文本与原始 XML：后者里常有前者被剥掉的字段（标题、文件名）。
            if keyword
                .is_some_and(|kw| !text.contains(kw) && !msg.parsed.raw_content.contains(kw))
            {
                continue;
            }
            out.push(json!({
                "serverId": msg.server_id.to_string(),
                "createTime": msg.create_time,
                "senderName": msg.sender_name,
                "senderUsername": msg.sender_username,
                "content": text,
            }));
            if out.len() >= cap {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

fn kind_name(kind: &api::SessionKind) -> String {
    match kind {
        api::SessionKind::Group => "group".to_string(),
        api::SessionKind::Official => "official".to_string(),
        api::SessionKind::Private => "private".to_string(),
        api::SessionKind::Other => "other".to_string(),
    }
}

// ---- 输出 ------------------------------------------------------------------

/// 人类可读输出按「哪一类数据」挑列：把所有字段都印出来一屏放不下，而人要的只是
/// 「有哪些会话、最近谁说了什么」。机器面永远是 --json。
fn emit(value: &Value, as_json: bool, kind: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if as_json {
        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
        let _ = writeln!(out, "{text}");
        return;
    }
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                let _ = writeln!(out, "（无结果）");
            }
            for it in items {
                let _ = writeln!(out, "{}", human_row(it, kind));
            }
        }
        other => {
            let _ = writeln!(out, "{}", human_row(other, kind));
        }
    }
}

fn human_row(v: &Value, kind: &str) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let n = |k: &str| v.get(k).map(|x| x.to_string()).unwrap_or_default();
    match kind {
        "sessions" => format!(
            "{}\t{}\t{} 条\t{}",
            s("username"),
            s("displayName"),
            n("messageCount"),
            n("lastTimestamp")
        ),
        "contacts" => format!("{}\t{}", s("username"), s("displayName")),
        "accounts" => {
            let err = v
                .get("error")
                .and_then(Value::as_str)
                .map(|e| format!("\terror: {e}"))
                .unwrap_or_default();
            format!("{}\t{}\t{} 条\t{}{err}", s("wxid"), s("state"), n("messageCount"), s("dbStorage"))
        }
        "sync" => format!(
            "新增 {} 条，撤回 {} 条（success={}）",
            n("newMessages"),
            n("revokeMessages"),
            n("success")
        ),
        _ => format!("{}\t{}\t{}", n("createTime"), s("senderName"), s("content")),
    }
}

// ---- export ---------------------------------------------------------------

/// Pull 面的消息项 → 导出的中立 Row。单独一个函数是因为取数面的类型属于 SDK，
/// 而「怎么写盘」只认 Row —— 这样导出模块能在不起服务、不装 SDK 类型的情况下被测全。
fn row_from_pull(m: &weflow_client::generated::r#gen::types::PullMessage) -> export::Row {
    export::Row {
        platform_message_id: m.platform_message_id.clone(),
        sender: m.sender.clone(),
        account_name: m.account_name.clone(),
        group_nickname: m.group_nickname.clone(),
        timestamp: m.timestamp,
        msg_type: m.type_,
        content: m.content.clone(),
        reply_to_message_id: m.reply_to_message_id.clone(),
        // 媒体只写元数据（type + fileName）：服务的媒体链接带 access_token，
        // 而导出文件会被拷进聊天工具、传上网盘。媒体字节的下载尚未实现：届时应把它下载到
        // 导出目录下的 media/ 并把 fileName 换成实际落盘的句柄，让交付包自成一体。
        media_file_name: m.media.as_ref().map(|x| x.file_name.clone()).filter(|s| !s.is_empty()),
        media_type: m.media.as_ref().map(|x| x.type_.clone()),
    }
}

/// 把一个会话的媒体导出并下载到 `<out>/media/`，返回**确实落盘**的句柄集合。
///
/// 为什么先走 `/chatlab/messages?media=1`：服务端**只有在真的写出了本地副本之后**才把
/// `fileName` 回填成可取句柄（外链与平台名给不出跨会话唯一的句柄），所以「触发导出」与
/// 「拿到句柄」是同一次请求的两面。该面每请求最多导出 200 项，超出部分靠翻页续传。
///
/// 单个媒体拿不到（404）只跳过，不升级成会话级失败：外链媒体本来就没有可取句柄，把它
/// 升格会让「这个群里有一个表情包不是本地文件」变成「这个群一条都没导出」。
async fn download_session_media(
    client: &Client,
    talker: &str,
    media_dir: &std::path::Path,
) -> Result<std::collections::BTreeSet<String>> {
    let mut downloaded: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut offset = 0u64;
    loop {
        let mut q = MessageQuery::new(talker.to_string());
        q.media = true;
        q.limit = Some(200);
        q.offset = Some(offset);
        let page = client.chatlab_messages(&q).await?;
        let count = page.messages.len();
        for m in &page.messages {
            let Some(media) = &m.media else { continue };
            let name = media.file_name.clone();
            if name.is_empty() || downloaded.contains(&name) {
                continue;
            }
            match client.media_bytes_by_id(&name).await {
                Ok(bytes) => {
                    std::fs::create_dir_all(media_dir)
                        .with_context(|| format!("创建媒体目录失败: {}", media_dir.display()))?;
                    let path = media_dir.join(&name);
                    std::fs::write(&path, &bytes)
                        .with_context(|| format!("写媒体失败: {}", path.display()))?;
                    downloaded.insert(name);
                }
                // 不是可取句柄（外链、或服务端没写出副本）：跳过该条，继续。
                Err(e) => tracing::debug!("跳过不可取句柄 {name}: {e}"),
            }
        }
        if !page.page.has_more || count == 0 {
            return Ok(downloaded);
        }
        offset += count as u64;
    }
}

/// 把 --since 转成 unix 秒。接受 unix 秒或 YYYYMMDD（后者取当天 00:00，与服务端对
/// **下界**的口径一致；上界才取整天）。
fn to_unix(s: &str) -> Result<i64> {
    let s = s.trim();
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s[0..4].parse().unwrap_or(0);
        let m: u32 = s[4..6]
            .parse()
            .with_context(|| format!("--since 月份非法: {}", s))?;
        let d: u32 = s[6..8]
            .parse()
            .with_context(|| format!("--since 日非法: {}", s))?;
        let naive = chrono::NaiveDate::from_ymd_opt(y as i32, m, d)
            .with_context(|| format!("--since 不是合法日期: {}", s))?;
        return Ok(naive.and_time(chrono::NaiveTime::MIN).and_utc().timestamp());
    }
    s.parse::<i64>()
        .with_context(|| format!("--since 需为 unix 秒或 YYYYMMDD: {}", s))
}

fn run_export(a: &ExportArgs) -> Result<()> {
    let format = match a.format.as_str() {
        "jsonl" => export::Format::Jsonl,
        "json" => export::Format::Json,
        other => anyhow::bail!("--format 只支持 jsonl|json: {}", other),
    };
    let client = http_client(&a.common)?;
    // 令牌就是导出物里绝不允许出现的那串（见 export 模块头的硬约束一）。
    let secret = std::env::var("WEFLOW_TOKEN").unwrap_or_default();
    let all = block(client.list_all_sessions(Some(10_000), None))?;
    let name_of = |t: &str| {
        all.iter()
            .find(|s| s.username == t)
            .map(|s| s.display_name.clone())
            .unwrap_or_default()
    };
    let targets: Vec<export::SessionTarget> = if a.session.is_empty() {
        all.iter()
            .map(|s| export::SessionTarget {
                talker: s.username.clone(),
                display_name: s.display_name.clone(),
            })
            .collect()
    } else {
        a.session
            .iter()
            .map(|t| export::SessionTarget {
                talker: t.clone(),
                display_name: name_of(t),
            })
            .collect()
    };
    let opts = export::Options {
        out_dir: a.out.clone(),
        format,
        resume: a.resume,
        secret,
    };
    let since = a.since.as_deref().map(to_unix).transpose()?;
    let start = std::time::Instant::now();
    let media_dir = a.out.join("media");
    let mut media_total = 0usize;
    let outcome = export::run(&targets, &opts, |target, on_page| {
        // Pull 面的 since 是**排他**下界，而 --since 对用户是含边界的：差一秒就会让
        // 「起点那一条」凭空消失，故这里减一。
        let talker = target.talker.clone();
        let since_pull = since.map(|s| s - 1);
        // --with-media：**先**触发导出并把字节落盘，**再**拉行。顺序不能反 —— 服务端只有
        // 在真的写出了本地副本之后，才把 Pull 面的 fileName 回填成可取句柄。
        let downloaded = if a.with_media {
            let got = block_anyhow(download_session_media(&client, &talker, &media_dir))?;
            media_total += got.len();
            got
        } else {
            std::collections::BTreeSet::new()
        };
        // SDK 的回调要求它自己的错误类型，而这里真正会失败的是**写盘**。
        // 把写失败硬塞成 ClientError 会丢信息，所以错误先存起来、循环后立即上抛，
        // 后续页只跳过不再写（半途而废的会话由 run 删掉，交给 --resume 重来）。
        let mut write_err: Option<anyhow::Error> = None;
        block(client.drain_session(&talker, since_pull, |msgs| {
            if write_err.is_none() {
                let mut rows: Vec<export::Row> = msgs.iter().map(row_from_pull).collect();
                if a.with_media {
                    for r in rows.iter_mut() {
                        export::retain_downloaded_media(r, &downloaded);
                    }
                }
                if let Err(e) = on_page(&rows) {
                    write_err = Some(e);
                }
            }
            Ok(())
        }))?;

        match write_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })?;
    println!(
        "[export] {} 个会话、{} 条消息、{} 个媒体 → {}（跳过 {}，索引 {}），用时 {:?}",
        outcome.written.len(),
        outcome.messages,
        media_total,
        opts.out_dir.display(),
        outcome.skipped.len(),
        outcome.index.display(),
        start.elapsed()
    );
    if !outcome.skipped.is_empty() {
        for s in &outcome.skipped {
            println!("[export] 跳过: {}", s);
        }
        // 少导了东西必须以非零码说话：静默的部分成功是这类工具最坏的失败方式。
        anyhow::bail!("{} 个会话未能导出（见上面的 跳过: 行）", outcome.skipped.len());
    }
    Ok(())
}

/// 测试专用的语料生成＋导出入口（`--rows N`，隐藏且只在 `testing` 下编译）。
///
/// **它为什么存在**：「大语料下内存恒定」这条断言在小语料上根本看不出来——把整个会话读进
/// 内存的实现，在一百条的夹具上一样表现为常数内存。要让它显形，语料必须大到「整会话驻留」
/// 与「流式写」差出量级，所以这里按需造库（CI 跑 200000 条，本机可跑 10^6 条）。
///
/// 与用户面的 `export` 唯一的区别是取数来源：这里进程内直读夹具库（`api::open`），
/// 因为测试环境里没有服务可打；写盘路径、行形状、`--resume` 与令牌检查用的是同一套代码
/// （`crate::export`），所以这条路径测到的东西对用户面同样成立。
///
/// 输出：每个会话一个采样点，形如 `[rss] session=<idx> peak_kb=<n>`；调用方（测试）
/// 比较**首个与末个**采样点，断言增量小于起始值的一成。
#[cfg(feature = "testing")]
fn export_corpus(out: &std::path::Path, rows: usize) -> Result<()> {
    use crate::testing::{self, BULK_SESSIONS};
    let dir = testing::tmp_dir("export-corpus");
    let key_hex = testing::FAKE_KEY_HEX.to_string();
    // Key 是 [u8; 32] 的别名，而 DbKey 包住同一个数组：这里借承诺面的解析器拿字节，
    // 不在 CLI 里另写一份 hex 解码。
    let key: crate::db::wcdb::Key = api::parse_db_key(&key_hex)?.0;
    // 造库器返回的就是 db_storage 目录本身（与 build_wechat_account 同规）。
    let storage = testing::build_account_with_rows(&dir, &key, rows);
    let keys = api::KeyMap::from_parts(
        Some(api::parse_db_key(&key_hex)?),
        None,
    )?;
    let index = api::open(&storage, &keys, testing::FAKE_WXID)?;
    let sessions: Vec<export::SessionTarget> = index
        .sessions()
        .iter()
        .map(|s| export::SessionTarget {
            talker: s.username.clone(),
            display_name: index.session_display(&s.username),
        })
        .collect();
    let opts = export::Options {
        out_dir: out.to_path_buf(),
        format: export::Format::Jsonl,
        resume: false,
        // 夹具里没有真令牌；留空表示「不检查」，而检查逻辑本身由 tests/cli.rs 直接测。
        secret: String::new(),
    };
    let started = std::time::Instant::now();
    let mut sample_idx = 0usize;
    let outcome = export::run(&sessions, &opts, |target, on_page| {
        let msgs = index.messages(&target.talker);
        let rowsv: Vec<export::Row> = msgs
            .iter()
            .map(|m| export::Row {
                platform_message_id: m.server_id.to_string(),
                sender: m.sender_username.clone(),
                account_name: m.sender_name.clone(),
                group_nickname: String::new(),
                timestamp: m.create_time,
                msg_type: m.local_type,
                content: m.parsed.display.clone(),
                reply_to_message_id: m.parsed.reply_to.clone(),
                media_file_name: None,
                media_type: None,
            })
            .collect();
        let n = rowsv.len();
        on_page(&rowsv)?;
        // 采样点：每导完一个会话取一次峰值 RSS。
        println!("[rss] session={sample_idx} rows={n} peak_kb={:?}", peak_rss_kb());
        sample_idx += 1;
        Ok(())
    })?;
    println!(
        "[corpus] rows={rows} sessions={} written={} messages={} 用时 {:?}",
        BULK_SESSIONS,
        outcome.written.len(),
        outcome.messages,
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// 进程峰值 RSS（KB）。取不到时返回 `None`——调用方据此跳过断言，而不是拿 0 当成
/// 「内存恒定」的证据。
#[cfg(feature = "testing")]
fn peak_rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // /proc/self/status 的 VmHWM 就是峰值常驻集，无需外部依赖。
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                return rest.trim().trim_end_matches(" kB").trim().parse().ok();
            }
        }
        None
    }
    #[cfg(windows)]
    {
        // 没有依赖可拿 PeakWorkingSet64，因此起一次 powershell 查询自身进程。
        // 每次采样一个子进程：只在测试路径里跑，代价可以接受。
        let pid = std::process::id();
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).PeakWorkingSet64"),
            ])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        s.parse::<u64>().ok().map(|bytes| bytes / 1024)
    }
    #[cfg(target_os = "macos")]
    {
        // macOS 的峰值要 getrusage（需要 libc 依赖）。这里退回 `ps -o rss` 的**当前值**，
        // 并在断言里当作下限使用；这一平台差异已登记在计划文件里。
        let pid = std::process::id();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        None
    }
}
