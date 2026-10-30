# 命令行子命令（`cli` feature）

本页是任务导向的操作指南（从 [API 参考](../weflow-server-api.md) 拆出）；端点、字段与错误形状见该参考。

---

同一个二进制带一个子命令面。**默认 feature 是 `["server", "cli", "mcp"]`**，所以 `cargo install weflow-server` 装出来即有；`--no-default-features` 时整面消失（连 `clap` 与 SDK 都不进依赖树）。

**兼容口径（重要）**：

- **裸跑仍等于 `serve`**：`weflow-server --port 5033` 一个字符都不用改；
- 以旗标开头的写法、以及 `serve` 后面的旗标，**原样交给既有参数解析器**——因此 `--help`／ `--version`／`--show-token`／配置文件加载的行为逐字不变，`serve --port 6002` 与 `--port 6002` 等价；
- `token` 子命令 = 既有的 `--show-token`；
- 第一个参数既不是旗标也不是已知子命令时，由 clap 报「未知子命令」并以 **2** 退出。老解析器遇到这种情况只会说「参数 bogus 缺少值」（退出 1），那是把用法错误伪装成取值错误。

退出码约定（回归位置：`tests/cli.rs`）：

| 码 | 含义 | 例子 |
| --- | --- | --- |
| `0` | 成功 | `weflow-server sessions` |
| `1` | 运行期错误：连不上、被拒、缺密钥、缺配置文件 | 未设 `WEFLOW_TOKEN` 且本机凭据库无记录就跑查询 |
| `2` | 用法错误：未知子命令、缺必需参数 | `weflow-server bogus`、`search` 不带 `--keyword` |

- **`serve`** — 打哪个面：—；要点：起服务；等价裸跑
- **`token`** — 打哪个面：系统凭据库；要点：打印 API token 并退出
- **`sessions`** — 打哪个面：SDK `list_all_sessions`；要点：`page_size=10000`（服务端硬上限），一次取尽
- **`messages`** — 打哪个面：SDK `list_messages`；要点：HTTP 形态必须给 `--talker`（服务端按会话查询）；`--since` 接受 unix 秒或 `YYYYMMDD`
- **`search`** — 打哪个面：SDK `list_messages` 带 `keyword`；要点：`--keyword` 必填；搜不到不是错误（退出 0）
- **`contacts`** — 打哪个面：SDK `contacts`；要点：单页；要全量请自己带 `--limit`/`--offset` 翻页
- **`accounts`** — 打哪个面：SDK `accounts`；要点：**只有 HTTP 形态**（没有 `--embedded`）：这一面问的是「服务端此刻实际绑定了什么」，进程内索引给的是另一个答案
- **`sync`** — 打哪个面：SDK `sync_now`；要点：**写动作**：让服务端立刻跑一次增量同步；同样没有 `--embedded`
- **`export`** — 打哪个面：SDK `list_all_sessions` ＋ `pull_page` ＋ `chatlab_messages`；要点：**只走 HTTP**（不提供 `--embedded`）：批量导出到 ChatLab Format 文件；`--with-media` 时用同一页的时间窗调 `chatlab_messages(media=1)` 触发导出并下载字节，见[批量导出](export.md)
- **`mcp`** — 打哪个面：经 SDK 打 HTTP 面；要点：stdio 上的 MCP 服务端：进程不碰数据库密钥，token 走同一来源链；**必须先起服务端**。工具清单与安全提示见 [mcp 文档](../mcp.md)

环境变量：`WEFLOW_BASE_URL`（默认 `http://127.0.0.1:5033`）、`WEFLOW_TOKEN`（API token）。**token 来源链**：`WEFLOW_TOKEN`（非空）→ 本机凭据库（与 `weflow-server token` 同一份）。同机运行不必每次 export；跨机仍走环境变量。环境变量未设且凭据库无记录（或后端不可读）时，错误会分别点名这两种成因并指路 `weflow-server token`。**token 一律不经命令行传递**——命令行会落进 shell history 与进程列表，而这个值能读出整份聊天记录。

`--embedded`（进程内直读本地库，不起也不打 HTTP）**只开放给只读查询类**（`sessions`／`messages`／ `search`／`contacts`）。它读与 `examples/embed.rs` **同一个**配置文件（`{"wxid":…, "db_path":…, "keys":{…}}`），路径由 `WEFLOW_EMBED_CONFIG` 给出；密钥仍只从环境变量／磁盘配置来。

输出：默认是人类可读的紧凑行（每类只挑最常看的几列），`--json` 给机器可读形状。

### 日志旗标（`--log` / `--log-format`）

子命令面带两个 **global 旗标**（在任何子命令的位置都可用）：

- **`--log`** — 取值：`error\|warn\|info\|debug`；默认：子命令 **warn**；serve **info**；说明：日志详细度。子命令默认只播失败原因，正常路径不刷屏
- **`--log-format`** — 取值：`text\|json`；默认：`text`；说明：`json` 为**逐行一个对象**（含 `level`/`target`/`fields`），给日志采集器消费；两入口日志统一走 stderr

关键口径（回归位置：`tests/cli_e2e.rs::subcommand_failures_are_logged_on_stderr` 与 `tests/cli_e2e.rs::log_json_format_emits_parseable_lines`、`tests/cli.rs::invalid_log_flags_are_usage_errors_exit_2`）：

- **子命令的日志只进 stderr，stdout 永远是命令输出**。`--json` 的 stdout 可被管道直接解析，加 `--log-format json` 也不会让 JSON 日志混进 stdout（serve 的日志去向见下下条）。
- **子命令在动手执行前就装好日志**——所以 `export` 把某个会话标记为 `skipped` 时，「**为什么**跳过」会以 `warn` 行出现在 stderr（改造前那行 `tracing::warn!` 落在没装 subscriber 的进程里，是 no-op：只剩退出码 1 与一行「跳过」，原因永远看不到）。
- **`serve` 的 `--log` 仍走老解析器**：`serve --log debug` 与裸跑 `--log debug` 逐字等价，info 默认也不被子命令预装的 warn 劫走（dispatch 在构造 clap 之前就短路进 legacy；回归位置：`tests/cli.rs::serve_log_flag_still_goes_through_the_old_parser`）。
- **输出去向统一 stderr**（0.10.0 破坏性变更）：子命令的日志走 stderr（stdout 留给命令输出），**serve 的日志也走 stderr**——`2>` 一处收齐服务日志与 `[init]`/`[fatal]`；此前 serve 走 stdout 的 `weflow-server > 日志文件` 类部署会拿到空 stdout，**迁移方式：改用 `2>` 收日志**。回归位置：`tests/cli.rs::serve_logs_go_to_stderr`。理由与历史代价写在 `docs/architecture.md` 的「日志：装在哪、写去哪、说什么话」。
- 非法取值（`--log verbose`、`--log-format yaml`）是**用法错误（退出码 2）**，与 `--since` 同一契约。
- 环境变量 `RUST_LOG` **优先于 `--log`**（EnvFilter 的既有语义）：设了它时命令行旗标被忽略。脚本与测试基座里一律 `env_remove` 它，避免级别随环境漂移。
- 子命令日志**不带 ANSI 颜色码**（`--log-format json` 与 text 同）：stderr 是脚本与采集器的消费面，`2> 日志文件` 里出现 `\x1b[33m` 会毁掉按行解析。这条是产品保证、不依赖 `NO_COLOR` 环境变量。serve 自 0.10.0 起并入同一通道，观感一致（同为无颜色码的行）。
