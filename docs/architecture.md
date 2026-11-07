# 架构

本文件是 weflow-server 的架构事实源：模块职责、数据流与跨模块陷阱。逐字段说明在 [`weflow-server-api.md`](weflow-server-api.md)——**接口文档说有哪些字段，本文件说为什么是这样**。

## 目录

- [定位与不变量](#定位与不变量)
- [总览与数据流](#总览与数据流)
- [核心模块](#核心模块)
- [库面与 feature](#库面与-feature)
- [服务层](#服务层)
- [工程与工具链](#工程与工具链)
- [测试与夹具](#测试与夹具)
- [设计要点与陷阱](#设计要点与陷阱)

## 定位与不变量

微信 4.x 消息库的只读 HTTP / SSE 服务。下面五条是**不变量**——破坏任何一条都是设计层面的变更，不是实现细节。

1. **只读**：从不写用户的数据库。连接以只读方式打开并带 `query_only`，写入会在 SQLite 层被拒绝，而不是「我们小心不去写」。
2. **内存索引**：会话、联系人、群名片等全部解析进内存，不落任何中间库。代价是重启要重建索引；收益是没有第二份数据副本、也不必处理索引与源库的不一致。
3. **单账号**：一个进程绑定一个账号。注册之前不提供查询——这不是策略，是数据前提：密钥与账号身份共同决定能打开哪些库。
4. **密钥不落盘**：数据库密钥由外部提供，只存在于进程内存；本服务不做密钥提取。
5. **无中间件**：没有数据库、没有消息队列，全部状态都在进程内。

## 总览与数据流

```text
微信数据目录
  ├─ db_storage/session/session.db         会话与联系人（WCDB 加密页）
  └─ message/*.db                          各会话的消息库
        │  ① 页解密：key + 每页 IV/HMAC（db/wcdb.rs）
        ▼
   只读连接（db/live.rs）
        │  ② 值驱动探测：先看有哪些表与列，再决定读什么（db/scan.rs）
        ▼
   解析（parser/mod.rs）——平台类型码 → 内部记录
        │  ③ 建索引（store/index.rs 的 build_all_live）
        ▼
   内存索引 Store（store/mod.rs）
        ├─ ④ 查询：handlers/* 读 Store → 信封 → JSON
        └─ ⑤ 增量：sync/mod.rs 轮询 + sync/watch.rs 指纹
                │  新消息 / 撤回 → 事件
                ▼
           SSE 总线（handlers/push_events.rs）→ 订阅者
```

两条容易看错的路径：

- **`sync` 不是「监听文件变化」**：`sync/watch.rs` 只用文件指纹决定「要不要再轮询一次」，真正的增量仍来自数据库水位线。
- **HTTP 与 SSE 读的是同一份 `Store`**：SSE 不另存一份队列，只在总线上广播事件；迟到的订阅者靠重放缓冲补齐。

## 核心模块

按目录与顶层文件列出（单文件细节见各文件头部的 `//!` 注释）。

| 模块 | 职责 | 对外接口 |
|---|---|---|
| `main.rs` / `lib.rs` | 进程入口与库入口（承诺面 `api` ＋ 实现面） | `api` |
| `config.rs` / `logging.rs` | 命令行与数据目录、token 凭据库；日志初始化 | `Config` / `init_*` |
| `pathsafe.rs` | 纯守卫：路径分量与导出根目录的边界检查 | `slugify` / 校验函数 |
| `db/wcdb.rs` | WCDB 页密码：页布局、密钥派生、每页校验 | 解密原语 |
| `db/live.rs` / `db/open.rs` | 活库只读连接（含密钥校验）与快照式打开 | `open_read_only` / `open_snapshot` |
| `db/scan.rs` | 值驱动探测：库 → 表 → 列，含账号发现 | `scan_all` / `scan_root` |
| `parser/mod.rs` | 平台记录 → 内部类型；媒体类型归一 | `parse_message` |
| `store/mod.rs` / `store/index.rs` | 内存索引的数据结构、查询原语与建索引 | `Store` / `build_all_live` |
| `sync/mod.rs` / `sync/watch.rs` | 增量轮询、水位线、事件构造；文件指纹轮询 | `AccountSync` / `WatchConfig` |
| `media/mod.rs` / `media/export.rs` | 媒体元数据与路径解析；媒体导出（含外部工具调用） | `export_*` |
| `keystore/mod.rs` | 密钥解析与镜像密钥（图片等） | `KeyMap` / `parse_db_key` |
| `server/mod.rs` | 路由装配、鉴权、共享状态、SSE 总线 | `build_router` / `serve_with_shutdown` |
| `server/handlers/*` | 共享件（类型码映射、参数解析、信封）与各端点实现 | `chatlab_type` / `parse_limit` / `extract_params` |

```text
src/
├─ config.rs / logging.rs      # 命令行与数据目录、日志
├─ keystore/                   # 密钥形状校验（64 hex）、img_code 派生；仅内存
├─ db/                         # wcdb 页密码 / 活库连接 / 值驱动探测
├─ parser/                     # 消息内容解析（XML / zstd / 类型占位符 / 引用 / 撤回）
├─ store/                      # 内存索引（会话/联系人/消息/水位）＋查询
├─ sync/                       # 增量同步引擎（poll + 事件）＋ watch
└─ server/                     # axum：鉴权、账号注册、端点、SSE、媒体
tests/                         # 假库夹具、HTTP 契约、e2e、真库探针（#[ignore]）
clients/                       # 类型化 SDK（rust / python / ts；ts 未发布）
```

## 库面与 feature

这个 crate 既是服务、也是库，两条路径共用同一份实现。

### 承诺面只有一处：`api`

其余模块在默认构建下是 `pub(crate)`——外部不可达，边界由编译器强制。理由是 `store::Store` 的字段是 `pub`（内部模块要写它），直接放出去等于把每一个字段都变成对外契约，而它们本来是内部布局。

| 面 | 内容 | 稳定性 |
|---|---|---|
| `api` | 只读索引、同步句柄、事件类型、密钥类型、数据本身 | 有 semver 承诺；`#![deny(missing_docs)]` 只作用在这里 |
| 其余模块 | 解析、存储、同步、服务层的实现 | 随时可变；仅 `--features testing` 下对集成测试可见 |

「只有 `api`」是空 feature 树下的口径：打开 `cli`／`server` 之后根上还会多出几个入口函数（`run_cli`、`run`／`serve_with_shutdown`）。它们是二进制的入口，不是给嵌入者的承诺面。

`testing` 只改可见性、不改功能：开它则实现面转 `pub`（集成测试在独立 crate 里只能看见 `pub`），同时它也是给嵌入者的造库工具面。

### feature 矩阵

| feature | 默认 | 内容 | 关掉的影响 |
|---|---|---|---|
| `server` | 是 | HTTP/SSE：axum ＋ tokio 运行时 ＋ OpenAPI 描述 | 没有服务，只剩库 |
| `sync` | 否 | watcher 与水位线增量：tokio ＋ notify | 没有增量同步；`api::Sync` 随之消失 |
| `media` | 否 | 媒体导出（外部 ffmpeg 运行时探测，缺失则降级） | 没有导出与媒体代理 |
| `testing` | 否 | 把实现面转成 `pub` | 集成测试够不着实现面 |
| `cli` | 是 | 命令行子命令面：`clap` ＋ 复用 `weflow-client`（隐含 `server`） | 只剩「旗标」这一条老路 |

依赖面的实际约束（可测，不是口号）：不开任何可选 feature 时，`--no-default-features` 的依赖树里不含 axum 与 tokio；开了 `cli` 带进 `clap`、`reqwest` 与 tokio，开了 `mcp` 再加 `rmcp`。

核心面（解析、存储）因此不得依赖可选面，这条边界由 CI 的一条检查守着。

### `clients/`：类型化 SDK（workspace 成员）

三个成员，与 src 的模块边界不同——它们消费 HTTP 面，不属于 crate 本体。

- **`clients/rust`（`weflow-client`）**：类型与操作客户端 ＋ 手写行为层（就绪轮询 / 游标排空 / SSE 重连 / 媒体重试 / 检索）。`generated/` 只许生成器改（`clients/regen`，CI 断言重生成无 diff）。
- **`clients/regen`（`weflow-regen`）**：生成工具。取 `server::openapi::document()`，做确定性规范化（3.1 → 3.0）后交给生成器；`--dump-spec` 同时供 Python 侧取规范化 spec。
- **`clients/python`（`weflow-sdk`）**：模型由 openapi-generator 从同一份规范化 spec 生成（`scripts/regen.py`）；行为层是 `httpx.AsyncClient` 异步实现，与 Rust 侧逐方法同构。
  - `wait_ready` 是 wait-only 就绪轮询；`ensure_ready` 把注册应答里的 200 拒绝态映射成 `StatusError` 快速失败。
  - `watch` 单连接连续产出多帧：字节级 LF 分帧、1 MiB 缓冲上限、单帧解码失败跳过不杀流、EOF 冲刷未终结残行、退避仅在干净结束时复位。
- **`clients/ts`**：TypeScript 示例客户端（仅示例，不发布 npm）。演示级子集；配套 `stub-server.mjs` 可无真实服务跑 smoke。不进 CI 产物矩阵。

分层理由：描述文档只声明「形状」，不声明「翻页到什么时候停、断线后从哪续」——后者是行为，生成不出来。所以形状交给生成器，行为交给手写层。SDK 不反依赖服务端 crate：只依赖 `reqwest`，对 API 的耦合全部在「描述 → 生成」这一步。

## 服务层

服务层是「HTTP 请求 → 路由 → 处理器 → 存储」的全部实现面。

### 路由、方法与鉴权

路由与方法的唯一事实源是 `server::routes::ROUTES`（`build_router` 由它构建）。方法口径是「读端点只有 GET，动作端点保留 GET+POST」：

- **免鉴权**：`/health`、`/api/v1/health`（GET+POST）、`/openapi.json`（GET）。
- **账号**：`/api/v1/accounts`（GET+POST）、`/api/v1/accounts/{wxid}`（DELETE）。
- **原生读面**：`messages`、`sessions`、`sessions/{id}/messages`、`contacts`、`group-members`、`media/{id}`、`push/messages`（均 GET）。
- **ChatLab 面**：`/chatlab/sessions`、`/chatlab/messages`、`/chatlab/sessions/{id}/messages`、`/chatlab/push/messages`（均 GET）。
- **动作**：`/api/v1/sync`（触发一次增量对账，不返回消息体；GET+POST）。
- **朋友圈**：`/api/v1/sns/*`（六条，不进 `/openapi.json`，见 `NOT_DOCUMENTED`；GET+POST）。

读端点砍掉 POST 的理由：两个方法完全等价，多一个只多一份「不一致」的可能；`/api/v1/sync` 是动作不是读，两个方法都留。

鉴权只有两条通道：`Authorization: Bearer <token>` 与 `?access_token=<token>`（`server/auth.rs::authorized`，常时比较）。

`X-Api-Key`、`?token=` 与 POST JSON body 都不是通道——每多一条就多一处凭据会被复制到的地方，而 body 连「这是谁的凭据」都区分不出来（`handlers::extract_params` 从不把凭据键从 body 带进参数表）。

`/chatlab/*` 的四条与老面共用同一份实现与同一条事件总线，差别只在形状（`format=chatlab` 开关已删除）。

### 响应形状只有一个事实源：`server/dto.rs`

每个端点的响应都由 `dto.rs` 里的 struct 定义，不再用 `json!` 字面量拼。键名写错从「运行时才知道」变成「编译不过」，而「哪些键在什么条件下出现」从「只存在于代码路径里」变成类型。

两条踩过才知道的纪律：

- **字段按字母序声明**。`json!` 走 `serde_json::Map`（默认 BTreeMap），历史响应的键本来就是字母序；而 struct 按声明序输出。不按字母序声明，「换 DTO」会顺带改动每个响应的键序。
- **`null` 与「省略」是两件事**。要 `null` 就写 `Option<T>` 且不加 `skip_serializing_if`；要省略才加。客户端常靠「键在不在」判断，顺手统一风格会让这个判据失效。

多形状端点各建 struct，不堆可选字段：`accounts`（POST）与 `accounts/{wxid}`（DELETE）的响应形状由状态决定，硬塞进一个「所有字段都可选」的类型会让它看起来合法而实际没有任何取值组合是对的。

`sessions` 的 `type` 在原生面是数字、在 ChatLab 面是字符串，因此是两个 struct。

### 响应防线有三层，各管一件事

- **golden 快照**：管「你改了」——整个响应（含状态码与键序）逐字节比对；在 `tests/golden/*.json` ＋ `tests/api_smoke.rs` 的 `mod golden`。
- **schema 校验**：管「改成什么是合法的」——引用能否解析、operationId 唯一、多形状确实用 `oneOf`；在 `tests/openapi.rs`。
- **契约套件**：管「两个仓库是否一致」——同一份用例跑两边；在 `tests/conformance_runner.rs`（`#[ignore]`，CI 独立步骤）。

契约套件在 CI 里是独立一步：克隆 `conformance.pin` 所指的 tag，再驱动执行入口。另有一步只跑 `nails-*`（五条数据不变量；`FLOW_CONTRACT_CASE` 是子串过滤，改名别改漏）。

第五条 `no_mediaPath_in_pull` 防的是拉取面复活 `mediaPath`。

停用某一步时要在注释里写清原因——一个不会红的门禁等于没有门禁。

快照那份有两个设计点：**易变值掩码值而不是删键**（删键会把形状一起丢掉），以及**时钟哨兵**——快照里出现接近「现在」的时间戳即失败，否则快照天天漂移，下一个人会习惯性地点「更新快照」。

哨兵不假设时间单位（秒与毫秒各比一次）——第一版只比秒级，于是毫秒级的 `updatedAt` 漏了过去。

### `/openapi.json` 由类型生成，且免鉴权

描述由 `#[derive(ToSchema)]` 从 DTO 生成，因此改 DTO 就改了它，不需要手工同步两处；副作用是 DTO 上的文档注释直接成了接口描述。

免鉴权是刻意的：它描述的是形状，不含账号、路径或密钥，而且正是给尚未拿到 token 的接入方看的。

`paths` 部分是按 OpenAPI 规范形状拼 JSON 再反序列化的——它是文档数据，不是契约响应；拼错了在加载时就会失败，而不是等到有人打开文档。

### SSE 总线

`GET /api/v1/push/messages` 是长连接：订阅 `AppState.bus`（`sync::history::EventBus`，把重放历史与 broadcast 通道绑成一件），迟到者靠重放缓冲补齐。

`/chatlab/push/messages` 挂在同一条总线上，只换了序列化器。

- **历史由生产者单点写入**：`AccountSync` 与服务层都经 `EventBus::publish` 先写重放历史、再广播带 id 的载荷，订阅端只读快照、不再各自 append。否则零订阅者时那段事件根本不进历史，之后带旧 `Last-Event-ID` 重连的客户端漏收却无从得知；而 N 个在线订阅者各写一份会让事件 id 随连接数跳号、1000 条窗口被重复条目稀释。
- **载体是类型不是 `json!`**：`sync::events::Event` 是带 `skip_serializing_if` 的 struct，`PushMedia` 在类型层面就没有 `aes_key`——密钥不会因为某次改动「忘了过滤」而泄露。
- **推送载荷没有快照护栏**（快照的模型是一次请求一次响应），所以它的键集由 `sse_payload_keys_are_pinned` 单独钉住。

## 工程与工具链

构建、测试、clippy 都走一条包装脚本；工具链版本与构建 profile 钉死，钩子与 CI 门禁在这里落地。

### 一条命令：`scripts/build.ps1`（或 `.sh`）

构建、测试、clippy 都必须走包装脚本。原因不是偏好：rusqlite 捆绑 SQLCipher + vendored OpenSSL，后者的 perl `Configure` 会直接调 `cl.exe` / `link.exe`，绕过 cc crate 的自动 MSVC 探测。

没有 `vcvars64.bat` 注入的 `INCLUDE`/`LIB`/`PATH` 就编译不过。脚本同时把 Strawberry Perl 放到 `PATH` 前面：Git 自带的 MSYS perl 会把 Windows 路径写坏。

工具链版本钉在 `rust-toolchain.toml`。

### 构建 profile 的两个刻意取舍

`[profile.dev]` 里有两项非默认设置，目的都是让开发机的构建产物不再无限膨胀：

- **`incremental = false`**：增量编译只对「同一个编译单元被反复小改」有收益。本仓的 `clippy` 与 `test` 走两套不同的 rustc 调用，各自的产物与增量目录互不复用；而增量目录里的条目绝大多数来自本仓自己的测试目标，并随依赖图变动持续累积，Cargo 不会回收。关掉它意味着每次改动都要完整重编本仓 crate——这是有意接受的代价。
- **`debug = "line-tables-only"`**：依赖侧的调试信息已由 `[profile.dev.package."*"]` 关掉，剩下的全部来自本仓 crate。降到行号表仍保留 backtrace 的文件名与行号。

`[profile.dev.package."*"] debug = 0` 是上面两条成立的前提，不要删：它把非成员单元的调试信息钉成关闭，使 Cargo 对宿主单元（构建脚本、proc-macro）的调试信息推断根本不参与；

一旦删掉，构建脚本所在目录的哈希随之改变，会触发 vendored OpenSSL 的完整重建。

**不要用 `CARGO_INCREMENTAL` 在这两个值之间来回切**——该环境变量优先级高于清单，`1` 会静默压过 `incremental = false`。

### 日志：装在哪、写去哪、说什么话

`src/logging.rs` 的模块头是这三条口径的事实源，这里只记架构层面的取舍。

- **subscriber 的安装位置**：CLI 分流（`cli::dispatch`）在执行任何子命令之前装 warn 级 subscriber。原因是没装 subscriber 时 `tracing::warn!` 展开成 `if enabled { … }`，整体是 no-op——子命令里那些「会话级失败原因」的日志会静默消失，用户只剩退出码与一行摘要。
- serve 分支不在这里装：`dispatch` 在构造 clap 之前就把 `serve` 短路进老解析器，级别与格式仍由 `Config.log`／`Config.log_format` 决定。
- **输出去向统一 stderr**（0.10.0 破坏性变更）：子命令日志走 stderr——`fmt()` 的默认 writer 是 stdout，而子命令的 stdout 是命令输出本身（`--json` 形状），日志混进去会毁掉管道消费。
- serve 日志自 0.10.0 起也走 stderr：此前 serve 走 stdout 而 `[fatal]` 在 stderr，「只看 `2>` 收错误」拿不到启动上下文；合并后 `2>` 一处收齐。迁移方式见 CHANGELOG。
- **`--log-format json`**：逐行一个 JSON 对象（`level`/`target`/`fields`），给日志采集器消费。依赖上只多一个 `tracing-serde`，不拉 axum/tokio/clap。
- **target 必须开着**：模块多，关掉后日志行没有「谁说的」。
- **子命令日志不带 ANSI 颜色码**：`tracing-subscriber` 不做 TTY 检测，重定向时照样写转义序列；stderr 是脚本与采集器的消费面，所以子命令那支显式 `with_ansi(false)`。靠 `NO_COLOR` 不行——开发机设了它、CI 没设，判据就变成「本地绿 CI 红」。
- **语言口径与脱敏红线**：见 `src/logging.rs` 模块头（用户句中文、模块级诊断英文；日志禁入完整密钥／完整 token／消息正文，例外只有 serve 路径那两条「不打印就拿不到」的 token 行）。

### 提交钩子

`scripts/install-hooks.*` 装两个钩子：`pre-commit`（隐私扫描 + 编号引用扫描 + 文档折点与结构，累积退出码，任一项失败即拒绝提交）与 `commit-msg`（把提交信息交给同一个扫描器）。

bash 与 Python 是提交路径的硬依赖：缺失时钩子报错并阻止提交，而不是跳过——失败开放的检查等于没有检查。

### CI 的门

Linux 与 Windows 双平台跑 `clippy --all-targets -D warnings` ＋ 全量测试。除此之外还有几道与「接口化」直接相关的门：

- 公共段哈希比对（比对所 pin tag 的内容，不是本仓副本）：三个仓库的协作规则悄悄分叉。
- 编号引用扫描（`--tree` 与 `--ref`）：注释里出现仓库外材料的编号，读者无从还原上下文。
- 隐私形态层扫描（`PRIVACY_SHAPE_ONLY=1`，只跑 64-hex 层）：直推 master 且本机没装钩子时，密钥/token 形状的值漏进跟踪文件。
- `--no-default-features` 编译 `examples/embed.rs`：承诺面被实现面「借用」，或依赖树里混进 axum/tokio。
- 一致性套件：契约与实现分叉。

Python 只用于钩子与套件执行器，纯标准库——CI 与开发机都不需要装第三方包。

### 已登记的两类构建告警（预期内，处置＝维持现状）

门禁是 `-D warnings`，所以这里登记的是不会让门禁变红、但直接 `cargo build` 时会出现的两类噪声。两者都被诊断过，结论是无功能影响。

1. **默认 feature 组合下的 `dead_code`**：一批内部模块里的辅助函数（如 `db/wcdb.rs` 的 `decrypt_page`、`db/open.rs` 的 `open_snapshot`／`quick_check`）在默认构建里看似未用——它们只被 `testing` 夹具或 `#[cfg(test)]` 单测调用。
   - CI 的 `clippy --all-targets` 由包装脚本补了 `--features testing`，所以看不到。判据是两条路径都能编译，不是「日志里没有 warning」。
2. **链接期 `LNK4099`（找不到 `ossl_static.pdb`）**：vendored OpenSSL 的静态库引用了自己的调试信息，而发布物里没有那份 PDB。只影响调试信息，不影响代码。不消掉的理由：加 `/ignore:4099` 会一并隐藏将来真实的 LNK4099。

## 测试与夹具

测试分三类，各有各的诚实边界；夹具只能造库，不能造索引。

### 三类测试，三种诚实

- **单元测试**（`src/**` 内嵌）：纯函数与解析逻辑，不碰库文件。
- **集成测试**（`tests/`）：用 tower 的 `oneshot` 直接打 router，不起网络；夹具造库。
- **真库探针**（`real_db_*`）：`#[ignore]` ＋ 环境变量门控，CI 不跑，只打印统计、不打印任何真实标识或正文。上游文档与本地库形态不一致时，只有它能给出正确答案。

### 夹具只能造库，不能造索引

夹具造库器住在 `src/testing/mod.rs`（随 `testing` feature 编译，默认构建里不存在），`tests/common/mod.rs` 只是它的转发层。

为什么在库里：需要它的有两个调用方，而它们互相看不见对方的代码——集成测试是独立 crate，根包二进制同样是独立 crate，批量导出的夹具生成入口（CLI 的 `--rows`）够不着只在 `tests/` 下存在的模块。

纪律是硬的：禁止手工注入 store 字段。那会让断言在一个真实运行时不存在的状态上通过。群名片那次就是这么漏掉的：测试手工往 `group_cards` 里塞了一条，于是「生产代码从来没写过这张表」这件事被四条断言一起盖住。

### 大语料与内存恒定

`export` 的 JSONL 形态承诺「峰值常驻集与条数无关」。这条承诺在小语料上测不出来：把整个会话读进内存的实现，在一百条的夹具上一样表现为常数内存。

因此有一个隐藏的 `--rows N` 参数（`hide = true` ＋ 仅 `testing` feature 编译）按需造库并逐会话采样峰值 RSS。它不在 CI 里跑：用例标了 `#[ignore]`（百万级造库太慢）。

通常取 `N=200000`，机器允许时可上 `10^6`；断言取大群采样的首个与末个点，要求增量小于起始值的一成。发布二进制里没有这个参数。

同一 `testing` feature 还开放 `WEFLOW_TOKEN_SERVICE`：集成测试可把 API token 的凭据库服务名指到专用的空服务，避免并行用例互踩真实凭据库。

### 快照与键序

`tests/golden/*.json` 钉住状态码 ＋ 键序 ＋ 掩码后的内容。键序单列是因为 `serde_json` 的 `Value` 往返会按 BTreeMap 排序，只存 body 会把原始顺序抹掉。

快照文件缺失即失败（缺了不再静默重建）：缺失与漂移是两种失败，但都必须看得见。

### 路由与接口描述的对等

路由的唯一事实源是 `server::routes::ROUTES`（`build_router` 由它构建，因此不存在「没进表的真实路由」）。

`/openapi.json` 的端点表与它的对等由 `documented_routes_match_the_openapi_table` 强制：集合必须等于「路由 − 豁免」，且未声明的方法必须 405（405 同时证明这条路径确实注册了）。

这一条是补出来的：此前两份清单各自手写、无人比对，仍漏了两条真实操作。

### 一致性套件

`tests/conformance_runner.rs` 起真服务、造夹具，跑契约仓库（版本记在 `conformance.pin`）里的 36 条用例（`v0.8.0`：auth 1／cursor 13／discovery 3／envelope 6／lifecycle 8／nails 5）。

v0.6.1 至 v0.6.6 都是纯文档 tag，v0.7.0 新增跨仓文档模板；v0.8.0 移除该模板层、用例集未增删。换 pin 时这里要跟着改，它没有机器判据。

两条硬规矩：带 `--fail-on-skip`（有用例被跳过即失败），缺 `FLOW_CONTRACT_DIR` 即失败（不是跳过）。

夹具里的 `contractVersion` 与 pin 由 `pinned_contract_version_matches_the_fixture` 钉在一起。

### 文档锚点

`tests/docs_anchors.rs` 把「文档同步义务」变成可执行：本文件里的每个同文档锚点链接都必须命中真实标题，目录必须覆盖全部顶层章节。删掉目录项、改标题忘了改链接，都会红。

## 设计要点与陷阱

每一条都是不知道就会踩的那类。写新代码前扫一眼这里。对外承诺的字段形状另有落点：三种「没有值」的表示与规范偏离登记在 [`weflow-server-api.md`](weflow-server-api.md) 的「ChatLab 字段语义」一节。

### 索引构建与密钥

- **建索引进度用 `Arc` 原子计数，不碰 `Store` 的锁**：`full_sync` 期间占着账号同步锁数分钟，而 HTTP 侧要回答「现在到哪了」。进度挂在账号句柄上（`Arc<IndexProgress>`），建索引路径逐行/逐表更新原子量，HTTP 侧无锁读快照。分母只数消息表；读失败的表不推进 `tables_done`，结束时少一格是「确实有一条表没读成」的诚实表达。
- **图片键与库密钥是两套**：注册时页 1 HMAC 只校验 `session.db`，不覆盖图片。`img_aes_key`/`img_xor_key`（或 `img_code` 派生）缺失或过期时文本正常、图片解码失败——很容易被误判成解析 bug。遇到按「密钥已轮换、参数过期或 key 集不全」处理，重新注册并带全 key 集。
- **`message_resource.db` 的 HMAC 失配会在同步路径上周期性重试**，错误不一定上浮成一次显式失败；表现是资源/媒体长期缺件，而不是进程报错。
- **`message/` 目录的伴随库**（全文索引、资源引用等）密钥同样由 `keys` 表提供；未提供某个伴随库的密钥时索引按「该库跳过」继续。缺密钥不是静默的：索引日志以 `no key for` 前缀点名。主库密钥缺失后果最重，伴随库缺失只损失对应检索面。

### 类型码与字段

- **`localType` 是打包字段，不掩码就读错类型**：平台把「大类」与「子类」压在同一个整数里（高位子类、低位大类）。直接当枚举用，`appmsg` 一族会全部落到「其他」——它是消息里最大的一个族，漏掉它等于丢掉近三成消息。回归见 `parser/mod.rs` 的类型映射测试。
- **ChatLab 类型码与平台类型码是两套独立的空间**：同一张图在平台类型码里是 `3`、在 ChatLab 空间里是 `1`，不可互推。ChatLab 空间里的 `6` 永不发射——它是保留位，出现即 bug。
- **群名片的来源在另一个库，且两库的 id 空间互相独立**：群名片在 `contact/contact_fts.db` 的全文索引表里，两个库各有自己的 `name2id`，同一个 username 在两库里的 rowid 不同。跨库复用 rowid 会静默错配——把 A 的群名片挂到 B 头上，不报错。
- **消息表名是 `Msg_<md5>`**：建索引时拿表名后缀反查 username，匹配不上时代码会把哈希串本身当作会话 ID。表现是「凭空多出一个会话」，不报错、不崩溃。

### 增量同步

- **WAL 预分配 4 MiB ⇒ 文件大小恒定，指纹只能看 mtime**：库是 WAL 模式且预分配固定大小的日志，写入不会改变文件大小，所以「靠 size 判断有没有新消息」永远为假。反过来说 mtime 变了不代表真有新消息，水位线才是权威。
- **增量检测是两级**：已打开的库用 SQLite `PRAGMA data_version` 探测外部提交——它是连接本地属性，只在同一连接的两个时点之间比较才有意义，且别的连接提交后必然不同但不保证递增（比较用不等，不用大小）。文件 mtime/size 戳退为未打开文件的前置门。Windows 高负载下元数据回读可能滞后，只靠文件戳会把一次写入误判成「未变」。
- **记账规则**：只有真正读到数据的文件才记账（记文件戳 ＋ 确认 data_version 基线）——无密钥、打开失败的文件不记账，下一轮才不会把它判成「未变」而把变更永久吞掉。qqflow 侧语义天然如此，无需同构。
- **确认的是读取前采样值，且只有排空才确认**：消息库在 SELECT 之前采样 `data_version`，本轮完全排空（未触顶）才把采样值写回基线。SELECT 结束到确认之间落库的提交版本更大、留在基线之后，下一轮必读；确认时刻重读「当前值」会把那批提交错误记成已读。回归见 `unread_commit_between_read_and_ack_survives`。
- **元库重载与消息库同一确认纪律**：session/contact/fts/sns 成功重载后按**装载前采样值**确认，一次提交只产生一次有效重载。回归见 `metadata_reload_confirms_once`。
- **updatedAt 只随索引内容变化推进**：判据是本轮压入会话的行数或任一元库完成重载，不是事件数——只含自身发送消息、或只刷了名册的轮次同样改变索引内容；空轮询不推进。
- 回归见 `stamp_unchanged_but_committed_row_is_still_polled` 与 `failed_open_does_not_swallow_the_next_poll`。
- **增量读遇到「没有时间列」的表：报错，不能返回空增量**：空 Vec 与「真的没有新行」在调用方完全不可区分，于是这张表的增量永久静默为空。同函数里「无 `local_id` 列」本来就是 `Err`，两者行为不一致本身就是线索。
- 影响面：该错误经 `?` 上抛会中止本轮整账号增量（水位不推进，下一轮重来），而初始建索引那边是逐表 `warn + skip`——两条路径并不对称。回归位置：`no_time_column_is_an_error_not_an_empty_increment`。

### 导出与媒体

- **子集导出的文件名是按本轮输入算的，撞名要按归属拒绝**：`export --session` 非续跑轮的去重集合从空开始，本轮某个会话可以算出与一个未在本轮的会话已交付产物同名的文件名，`.part` 收尾 rename 直接把它盖掉；而新 `index.json` 没有那个条目，交付物被换掉、下一轮也无从自愈。
- 判据必须是归属（既有产物登记在别的会话名下，或没被任何清单认领），不是「本轮有没有写过这个路径」——后者会把同会话有意重导一并拒掉。处置是会话级：记入 `skipped`（起手前就拒，不发请求、不留 `.part`），`skipped` 非空则 CLI 以 1 退出。
- 拒绝轮必须把属主那一行带进本轮新清单，否则属主下次重导时归属变成「无主」，被同一个拒绝逻辑挡住。
- **这套防护是降低覆盖事故的概率，不是消除**：续跑轮把上一轮的名字先播种进 `taken`，别的会话的产物名压根算不出来（只会拿到 `-2` 后缀），那条路径本就不需要拒绝。清单丢失／被截断时「本轮无完成记录 ⇒ 重写」是既有且被需要的自愈语义。
- 回归位置：`refusing_a_collision_keeps_the_owner_exportable`、`subset_export_refuses_to_clobber_another_sessions_artifact`、`intentional_rerun_of_same_session_overwrites_itself`。
- 另两条：`unowned_leftover_file_is_not_clobbered`、`resume_with_intact_index_avoids_the_collision_entirely`。
- **媒体导出每请求上限 200 项，超出部分保持未导出**：这是延迟保护。下游若需要全量必须自行翻页——上限不会以错误形式告知，只会「剩下的没导出」。
- **媒体句柄只对「内容摘要派生」的名字下发**：按名取字节要遍历所有会话的导出目录，所以只服务内容摘要派生的名字（同名即同内容，遍历结果才确定）。平台给的名字与原始文件名回落只作元数据——当句柄用会在别的会话里躺着一个同名异内容文件时变成随机 404，或更糟：服务了错的那一份。
- **拉取面也通告 `messages[].mediaId`，但判据要绕一个弯**：它不执行导出，只能报告「已经导出过的那些」，而**不能拿元数据名去 stat**——图片落盘名的扩展名是解码后由内容嗅探得出的（`sniff_image_ext`；wxgf 还可能被 ffmpeg 转成 png），与 XML 里那个属性名不必相同，按全名 stat 会静默漏报。
- 所以按摘要干查（`media::export::digest_handles` 列出本会话导出目录里所有摘要派生名），命中才把实际落盘名当句柄给出。语音的 `voice_<svr>.silk` 与视频的平台名都不是摘要派生，拉取面同样不给句柄。
- **目录扫描是阻塞 IO 且按请求算一次**（逐条 stat 会让一页变成几十次目录打开），因此走 `spawn_blocking`，并且放在取 store 读锁之前——`RwLockReadGuard` 不是 `Send`，跨 `.await` 会让 axum 的 `Handler` bound 直接编译失败。
- 回归位置：`api_smoke::pull_advertises_media_id_only_for_exported_digest_files`；CLI 侧快路径钉在 `cli_e2e::media_id_from_pull_row_skips_the_export_round`。

### 分页与免鉴权面

- **没有 `page` 块的响应会被读成「完整一页」**：会话列表默认只给 100 条，而 ChatLab 的约定是「没有 `page` 块就表示这就是全部」。两者相乘的结果是「第 101 个会话凭空消失」，不报错也无提示。所以 ChatLab 形状必须带 `page.hasMore` 与 `page.nextCursor`。
- **`/health` 免鉴权，且账号状态枚举刻意没有「等待密钥」这一档**：未鉴权方可以访问它，因此返回的信息量被设计约束过——状态只有「未注册 / 建索引中 / 就绪 / 出错」四档。否则未鉴权方就能数出本机配置了几个账号。加状态时请保持这个约束。
