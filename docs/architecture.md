# 架构

本文件是 weflow-server 的**架构事实源**：模块职责、数据流与跨模块陷阱。
接口的逐字段说明在 [`weflow-server-api.md`](weflow-server-api.md)。两者分工是
**「接口文档说有哪些字段，本文件说为什么是这样」**。

## 目录

- [定位与不变量](#定位与不变量)
- [总览与数据流](#总览与数据流)
- [核心模块](#核心模块)
- [库面与 feature](#库面与-feature)
- [服务层](#服务层)
- [工程与工具链](#工程与工具链)
- [测试与夹具](#测试与夹具)
- [设计要点与陷阱](#设计要点与陷阱)

> 完整骨架共九节，现为**七节**：「数据源与解析」「同步机制」「配置」三节随后续
> 改动增量补（回写规则见 `AGENTS.md` 的「文档同步义务」）。

## 定位与不变量

微信 4.x 消息库的**只读** HTTP / SSE 服务。下面五条是**不变量**——破坏其中任何一条
都是设计层面的变更，不是实现细节：

1. **只读**：从不写用户的数据库。连接以只读方式打开并带 `query_only`，写入会在 SQLite
   层被拒绝，而不是「我们小心不去写」。
2. **内存索引**：会话、联系人、群名片等全部解析进内存，**不落任何中间库**。
   代价是重启要重建索引；收益是没有第二份数据副本、也不必处理索引与源库的不一致。
3. **单账号**：一个进程绑定一个账号。注册之前不提供查询——这条不是策略，是数据前提：
   密钥与账号身份共同决定能打开哪些库。
4. **密钥不落盘**：数据库密钥由外部提供，只存在于进程内存；本服务**不做密钥提取**，
   那是另一个工具的事。
5. **无中间件**：没有数据库、没有消息队列，全部状态都在进程内。

## 总览与数据流

```text
微信数据目录
  ├─ db_storage/session/session.db         会话与联系人（WCDB 加密页）
  └─ message/*.db                          各会话的消息库
        │
        │  ① 页解密：key + 每页 IV/HMAC（db/wcdb.rs）
        ▼
   只读连接（db/live.rs）
        │
        │  ② 值驱动探测：先看有哪些表与列，再决定读什么（db/scan.rs）
        ▼
   解析（parser/mod.rs）——平台类型码 → 内部记录
        │
        │  ③ 建索引（store/index.rs 的 build_all_live）
        ▼
   内存索引 Store（store/mod.rs）
        │
        ├─ ④ 查询：handlers/* 读 Store → 信封 → JSON
        │
        └─ ⑤ 增量：sync/mod.rs 轮询 + sync/watch.rs 指纹
                │  新消息 / 撤回 → 事件
                ▼
           SSE 总线（handlers/push_events.rs）→ 订阅者
```

两条**容易看错**的路径：

- **`sync` 不是「监听文件变化」**：`sync/watch.rs` 只是用文件指纹决定「要不要再轮询一次」，
  真正的增量仍来自数据库水位线（见「同步机制」一节，待补）。
- **HTTP 与 SSE 读的是同一份 `Store`**：SSE 不另存一份队列，只在总线上广播事件；
  迟到的订阅者靠重放缓冲补齐（容量与 TTL 见「服务层」，待补）。

## 核心模块

按**目录 + 顶层文件**列出（单文件细节见各文件头部的 `//!` 注释）。

| 模块 | 职责 | 对外接口 | 依赖方向 |
|---|---|---|---|
| `main.rs` | 进程入口：解析配置、装日志、起服务 | — | → `server` / `config` |
| `lib.rs` | 库入口：承诺面 `api` ＋ 实现面（默认 `pub(crate)`，见「库面与 feature」） | `api` | — |
| `config.rs` | 命令行与数据目录解析、token 凭据库读写 | `Config` | ← 无 |
| `logging.rs` | 日志初始化 | `init()` | ← 无 |
| `pathsafe.rs` | **纯守卫**：路径分量与导出根目录的边界检查 | `slugify` / 校验函数 | ← 无（被 `media`、`server` 调用） |
| `db/wcdb.rs` | WCDB 页密码：页布局、密钥派生、每页校验 | 解密原语 | ← 无 |
| `db/live.rs` | 活库只读连接与密钥校验 | `open_read_only` / 连接池 | → `wcdb` |
| `db/open.rs` | 快照式打开（与 `live` / `scan` 分层，仅在需要稳定视图时用） | `open_snapshot` | → `wcdb` |
| `db/scan.rs` | **值驱动探测**：库 → 表 → 列，含账号发现 | `scan_accounts` 等 | → `live` |
| `parser/mod.rs` | 平台记录 → 内部类型；媒体类型归一 | `parse_message` | ← 无（纯函数为主） |
| `store/mod.rs` | 内存索引的数据结构与查询原语 | `Store` | → 无外部 IO |
| `store/index.rs` | 从库里**建**索引；消息表 → 会话归属 | `build_all_live` | → `db` / `parser` |
| `sync/mod.rs` | 增量轮询、水位线、事件构造 | `AccountSync` | → `store` / `parser` |
| `sync/watch.rs` | 文件指纹轮询（**决定何时再轮询**，不解析内容） | `WatchConfig` | → 文件系统 |
| `media/mod.rs` | 媒体元数据与路径解析 | — | → `pathsafe` |
| `media/export.rs` | 媒体导出（含外部工具调用） | `export_*` | → `pathsafe` / 子进程 |
| `keystore/mod.rs` | 密钥解析与镜像密钥（图片等） | `KeyMap` / `parse_db_key` | ← 无 |
| `server/mod.rs` | 路由装配、鉴权、共享状态、SSE 总线 | `serve_with_shutdown` | → 全部 |
| `server/handlers/mod.rs` | **handler 之间的共享件**：类型码映射、参数解析、信封 | `chatlab_type` / `parse_limit` / `extract_params`（`server::merge_params` 的薄包装） | — |
| `server/handlers/*` | 各端点的实现（account / session / message / contact / media / push / sns） | — | → `store` / `sync` |

## 库面与 feature

这个 crate **既是服务、也是库**，两条路径共用同一份实现。

### 两条使用路径

| 用途 | 怎么用 |
|---|---|
| 起服务 | `cargo run`（或 `cargo install <包名>`）—— 默认 feature 就是它 |
| 当库用 | `default-features = false`，再按需开 feature。**必须显式关掉默认 feature**，否则会连带拉进 axum 与 tokio |

嵌入者从 **`api`** 入手，它始终可用（不随任何 feature 开关）—— 「读自己的聊天记录」是最小可用面。
`examples/embed.rs` 是它的活文档：**不起 HTTP**，直接把一个账号读出来。

### 承诺面只有一处：`api`

其余模块在默认构建下是 **`pub(crate)`** —— 外部不可达，**边界由编译器强制**。这样做的理由：
`store::Store` 的字段是 `pub`（内部模块要写它），直接放出去等于把**每一个字段**都变成对外契约，
而它们本来是内部布局。

| 面 | 内容 | 稳定性 |
|---|---|---|
| `api` | 只读索引、同步句柄、事件类型、密钥类型、数据本身 | 有 semver 承诺；`#![deny(missing_docs)]` 只作用在这里 |
| 其余模块 | 解析、存储、同步、服务层的实现 | 随时可变；仅 `--features testing` 下对集成测试可见 |

`testing` 只改**可见性**，不改功能：开它则实现面转 `pub`（集成测试在独立 crate 里，只能看见
`pub`）。它同时是**给嵌入者的造库工具**面 —— 写自己的测试时同样要造库、造密钥。

### feature 矩阵

| feature | 内容 | 关掉的影响 |
|---|---|---|
| `server`（默认）| HTTP/SSE：axum ＋ tokio 运行时 ＋ OpenAPI 描述 | 没有服务，只剩库 |
| `sync` | watcher 与水位线增量：tokio ＋ notify | 没有增量同步；`api::Sync` 随之消失 |
| `media` | 媒体导出（外部 ffmpeg 在**运行时**探测，缺失则降级） | 没有导出与媒体代理 |
| `testing` | 把实现面转成 `pub`（见上） | 集成测试够不着实现面 |

**依赖面的实际约束**（可测，不是口号）：`--no-default-features` 的依赖树里**不含 axum 与 tokio**。
核心面（解析、存储）因此不得依赖可选面 —— 这条边界由 CI 上的一条检查守着。

### `clients/`：类型化 SDK（workspace 成员）

两个成员，与 src 的模块边界不同 —— 它们**消费** HTTP 面，不属于 crate 本体：

| 成员 | 内容 | 维护方式 |
|---|---|---|
| `clients/rust`（`weflow-client`） | 类型与操作客户端 ＋ 手写行为层（就绪轮询 / 游标排空 / SSE 重连 / 媒体重试 / 检索） | `generated/` 只许生成器改（`clients/regen`，CI 断言重生成无 diff）；行为层手写并测 |
| `clients/regen`（`weflow-regen`） | 生成工具：取 `server::openapi::document()`，做确定性规范化（3.1 → 3.0）后交给生成器 | 改规范化规则 = 改语义，需评审 |

分层的理由：描述文档只声明「形状」，不声明「翻页到什么时候停、断线后从哪续」——后者是行为，
生成不出来；而类型若靠手写，必然与描述静默分叉。所以形状交给生成器（入库 + no-diff 门禁），
行为交给手写层（对 mock 夹具测试）。SDK 不反依赖服务端 crate：它只依赖 `reqwest`，
对 API 的耦合全部发生在「描述 → 生成」这一步。

## 服务层

### 路由、方法与鉴权

路由与方法的**唯一事实源**是 `server::routes::ROUTES`（`build_router` 由它构建，对等性怎么强制
见「测试与夹具」）。方法口径是「**读端点只有 GET，动作端点保留 GET+POST**」：

| 面 | 路由 | 方法 |
|---|---|---|
| 免鉴权 | `/health`、`/api/v1/health`、`/openapi.json` | 前两条 GET+POST、后者 GET |
| 账号 | `/api/v1/accounts`（列表 / 注册）、`/api/v1/accounts/{wxid}`（注销） | GET+POST / DELETE |
| 原生读面 | `/api/v1/messages`、`/api/v1/sessions`、`/api/v1/sessions/{id}/messages`、`/api/v1/contacts`、`/api/v1/group-members`、`/api/v1/media/{id}`、`/api/v1/push/messages` | GET |
| ChatLab 面 | `/chatlab/sessions`、`/chatlab/messages`、`/chatlab/sessions/{id}/messages`、`/chatlab/push/messages` | GET |
| 动作 | `/api/v1/sync`（触发一次增量对账，**不返回消息体**） | GET+POST |
| 朋友圈 | `/api/v1/sns/*`（六条，**不进 `/openapi.json`**，见 `NOT_DOCUMENTED`） | GET+POST |

读端点砍掉 POST 的理由：两个方法完全等价（POST 读请求与 GET 行为一致），多一个方法只多一份
「两者不一致」的可能，换不来任何能力；而 `/api/v1/sync` 是**动作**不是读，两个方法都留。

鉴权**只有两条通道**：`Authorization: Bearer <token>` 与 `?access_token=<token>`
（`server/auth.rs::authorized`，常时比较）。`X-Api-Key`、`?token=` 与 POST JSON body 都**不是**
通道——每多一条就多一处凭据会被复制到的地方（请求体、代理日志、客户端抓包），而 body 连
「这是谁的凭据」都区分不出来：`handlers::extract_params` 从不把凭据键从 body 带进参数表。

`/chatlab/*` 的四条与老面**共用同一份实现与同一条事件总线**，差别只在形状：老面只输出原生/
富数据形状（`format=chatlab` / `chatlab=1` 开关已删除），ChatLab 形状一律走这四条——调用方不必
知道还有另一种形状，也不会因为漏传一个开关而拿到另一种。

### 响应形状只有一个事实源：`server/dto.rs`

每个端点的响应都由 `dto.rs` 里的 struct 定义，不再用 `json!` 字面量拼。三条纪律都写在
该模块的头部注释里，这里只说**为什么**值得多这一层：键名写错从「运行时才知道」变成
「编译不过」，而「哪些键在什么条件下出现」从「只存在于代码路径里」变成类型。

其中两条是踩过才知道的：

- **字段按字母序声明**。`json!` 走 `serde_json::Map`（默认 BTreeMap），所以历史响应的键
  **本来就是字母序**；而 struct 按**声明序**输出。不按字母序声明，「换 DTO」会顺带改动每个
  响应的键序 —— 语义上无害，但会让 review 淹没在无意义的 diff 里。
- **`null` 与「省略」是两件事**。要 `null` 就写 `Option<T>` 且**不加** `skip_serializing_if`；
  要省略才加。客户端常靠「键在不在」判断（媒体导出与否、有没有引用），顺手统一风格会让
  这个判据失效。

多形状端点**各建 struct**，不堆可选字段：`accounts`（POST）与 `accounts/{wxid}`（DELETE）的响应
形状由状态决定，硬塞进一个「所有字段都可选」的类型会让它**看起来**合法而实际没有任何取值组合
是对的。同一个概念在两个面上类型不同时同理 —— `sessions` 的 `type` 在原生面是数字、在 ChatLab 面
是字符串，因此是两个 struct，而不是一个字段带两种类型。

### 响应防线有三层，各管一件事

| 层 | 管什么 | 在哪 |
|---|---|---|
| **golden 快照** | 「**你改了**」：整个响应（含**状态码**与**键序**）逐字节比对 | `tests/golden/*.json` ＋ `tests/api_smoke.rs` 的 `mod golden` |
| **schema 校验** | 「**改成什么是合法的**」：引用能否解析、operationId 唯一、多形状确实用 `oneOf` | `tests/openapi.rs` |
| **契约套件** | 「**两个仓库是否一致**」：同一份用例跑两边 | `tests/conformance_runner.rs`（`#[ignore]`，CI 独立步骤） |

契约套件在 CI 里是**独立一步**（不是靠 `cargo test` 顺带跑的）：克隆 `conformance.pin` 所指的
tag，再驱动上面的执行入口。另有一步只跑 `nails-*`（四条数据不变量）。**停用某一步时要在
注释里写清原因** —— 一个不会红的门禁等于没有门禁。


快照那份有两个设计点值得知道：**易变值掩码值而不是删键**（删键会把形状一起丢掉），以及
**时钟哨兵** —— 快照里出现接近「现在」的时间戳即失败。后者的理由是：否则快照天天漂移，
下一个人会习惯性地点「更新快照」，护栏名存实亡。哨兵**不假设时间单位**（秒与毫秒各比一次）
—— 第一版只比秒级，于是毫秒级的 `updatedAt` 从它底下漏了过去。

### `/openapi.json` 由类型生成，且**免鉴权**

描述由 `#[derive(ToSchema)]` 从 DTO 生成，因此改 DTO 就改了它，不需要手工同步两处；副作用
是 DTO 上的文档注释**直接成了接口描述**。

免鉴权是刻意的：它描述的是形状，不含账号、路径或密钥，而且正是给**尚未拿到 token 的接入方**
看的。

`paths` 部分是按 OpenAPI 规范形状拼 JSON 再反序列化的 —— 它是**文档数据**，不是契约响应；
用 builder 逐层构造只会让那段代码变成对其 builder API 的考古。拼错了在加载时就会失败，
而不是等到有人打开文档。

### SSE 总线

`GET /api/v1/push/messages` 是长连接：订阅 `AppState.events` 这条 broadcast 总线，迟到者靠
重放缓冲补齐。`/chatlab/push/messages` 挂在**同一条总线**上，只换了序列化器：它发的是通知帧
（只带标识与时间，不带正文），连接机制（鉴权、重放、保活、基线）与老面完全一致。三点必须知道：

- **载体是类型不是 `json!`**：`sync::events::Event` 是带 `skip_serializing_if` 的 struct，
  `PushMedia` 在**类型层面就没有** `aes_key` —— 密钥不会因为某次改动「忘了过滤」而泄露。
- **推送载荷没有快照护栏**（快照的模型是一次请求一次响应），所以它的键集由
  `sse_payload_keys_are_pinned` 单独钉住。
- 那些「时间」「路径」类的易变字段在快照里由掩码与哨兵处理，不在这里重复。

## 工程与工具链

### 一条命令：`scripts/build.ps1`（或 `.sh`）

构建、测试、clippy **都必须走包装脚本**。原因不是偏好：rusqlite 捆绑 SQLCipher + vendored
OpenSSL，后者的 perl `Configure` 会直接调 `cl.exe` / `link.exe`，**绕过 cc crate 的自动 MSVC 探测**
——没有 `vcvars64.bat` 注入的 `INCLUDE`/`LIB`/`PATH` 就编译不过。脚本同时把 **Strawberry Perl**
放到 `PATH` 前面：Git 自带的 MSYS perl 会把 Windows 路径写坏。工具链版本钉在 `rust-toolchain.toml`。

### 提交钩子

`scripts/install-hooks.*` 装两个钩子：`pre-commit`（隐私扫描 + 编号引用扫描，**累积退出码**，
任一项失败即拒绝提交）与 `commit-msg`（把提交信息交给同一个扫描器）。**bash 与 Python 是提交
路径的硬依赖**：缺失时钩子报错并阻止提交，而不是跳过——失败开放的检查等于没有检查。

### CI 的门

Linux 与 Windows 双平台跑 `clippy --all-targets -D warnings` ＋ 全量测试。除此之外还有几道
与「接口化」直接相关的门，它们各自防一种特定的漂移：

| 门 | 防的是什么 |
|---|---|
| 公共段哈希比对（比对**所 pin tag** 的内容，不是本仓副本） | 三个仓库的协作规则悄悄分叉 |
| 编号引用扫描（`--tree` 与 `--ref`） | 注释里出现仓库外材料的编号，读者无从还原上下文 |
| `--no-default-features` 编译 `examples/embed.rs` | 承诺面被实现面「借用」，或依赖树里混进 axum/tokio |
| 一致性套件（见下节） | 契约与实现分叉 |

Python 只用于钩子与套件执行器，**纯标准库**——CI 与开发机都不需要装第三方包。

## 测试与夹具

### 三类测试，三种诚实

- **单元测试**（`src/**` 内嵌）：纯函数与解析逻辑，不碰库文件；
- **集成测试**（`tests/`）：用 tower 的 `oneshot` 直接打 router，**不起网络**；夹具造库；
- **真库探针**（`real_db_*`）：`#[ignore]` ＋ 环境变量门控，**CI 不跑**，只打印统计、不打印任何
  真实标识或正文。上游文档与本地库形态不一致时，只有它能给出正确答案。

### 夹具只能**造库**，不能造索引

`tests/common/mod.rs` 负责建一个假的微信数据目录。纪律是硬的：**禁止手工注入 store 字段**
——那会让断言在一个真实运行时不存在的状态上通过。群名片那次就是这么漏掉的：测试手工往
`group_cards` 里塞了一条，于是「生产代码从来没写过这张表」这件事被四条断言一起盖住。

### 快照与键序

`tests/golden/*.json` 钉住**状态码 ＋ 键序 ＋ 掩码后的内容**。键序单列是因为 `serde_json` 的
`Value` 往返会按 BTreeMap 排序，只存 body 会把原始顺序抹掉，「逐字节」就名不副实。
**快照文件缺失即失败**（缺了不再静默重建）：缺失与漂移是两种失败，但都必须看得见。

### 路由与接口描述的对等

路由的唯一事实源是 `server::routes::ROUTES`（`build_router` 由它构建，因此不存在「没进表的
真实路由」）；`/openapi.json` 的端点表与它的对等由 `documented_routes_match_the_openapi_table`
强制：集合必须等于「路由 − 豁免」，且**未声明的方法必须 405**（405 同时证明这条路径确实注册了）。
这一条是补出来的：此前两份清单各自手写、无人比对，「收口」之后仍漏了两条真实操作。

### 一致性套件

`tests/conformance_runner.rs` 起真服务、造夹具，跑契约仓库（版本记在 `conformance.pin`）里的
34 条用例。两条硬规矩：**带 `--fail-on-skip`**（有用例被跳过即失败，避免「夹具少声明一个端点」
让用例静默变成不跑），**缺 `FLOW_CONTRACT_DIR` 即失败**（不是跳过）。夹具里的
`contractVersion` 与 pin 由 `pinned_contract_version_matches_the_fixture` 钉在一起。

### 文档锚点

`tests/docs_anchors.rs` 把「文档同步义务」变成可执行：本文件里的每个同文档锚点链接都必须命中真实标题，
目录必须覆盖全部顶层章节。删掉目录项、改标题忘了改链接，都会红。

## 设计要点与陷阱

每一条都是**不知道就会踩**的那类。写新代码前扫一眼这里。

### `localType` 是打包字段，不掩码就读错类型

平台把「大类」与「子类」压在同一个整数里：高位是子类、低位是大类。直接当枚举用，
`appmsg` 一族会全部落到「其他」，**实测占全部消息的 28.6%**。掩码后才能取到真实类型
（回归见 `parser/mod.rs` 的类型映射测试）。

### WAL 预分配 4 MiB ⇒ 文件大小恒定，指纹只能看 mtime

库是 WAL 模式且预分配固定大小的日志，**写入不会改变文件大小**。所以「靠 size 判断有没有新消息」
永远为假；`sync/watch.rs` 因此只看 mtime。反过来说：**mtime 变了不代表真有新消息**，
水位线才是权威。

### 群名片的来源在另一个库，且两库的 id 空间互相独立

群名片不在 `contact.db`，而在 **`contact/contact_fts.db`** 的全文索引表里；
两个库各自有自己的 `name2id`，**同一个 username 在两库里的 rowid 不同**。
跨库复用 rowid 会**静默错配**——把 A 的群名片挂到 B 头上，不报错。
因此加载群名片必须**用那个库自己的 id 表**解析。

### 消息表名是 `Msg_<md5>`：哈希匹配不上时，哈希本身会成为会话键

消息表按 `md5(username)` 命名。建索引时拿表名后缀去已发现的会话里反查 username；
**匹配不上时，代码会把哈希串本身当作会话 ID**。表现是「凭空多出一个会话」——
不报错、不崩溃，只是多一条。排查会话数量异常时先看这里。

### `/health` 免鉴权，且账号状态枚举**刻意**没有「等待密钥」这一档

未鉴权方可以访问 `/health`，因此它返回的信息量是被设计约束过的：账号状态枚举只有
「未注册 / 建索引中 / 就绪 / 出错」四档，**没有「已配置但缺密钥」**——否则未鉴权方就能
**数出本机配置了几个账号**。加状态时请保持这个约束。

### ChatLab 类型码与平台类型码是**两套独立的空间**

同一张图在平台类型码里是 `3`、在 ChatLab 空间里是 `1`；两者不可互推。
另外 ChatLab 空间里的 **`6` 永不发射**——它是保留位，出现即 bug。

### 媒体导出**每请求上限 200 项**，超出部分保持未导出

这是延迟保护：一次请求导出过多会让响应时间不可控。下游若需要全量，必须自行翻页——
**上限不会以错误形式告知，只会「剩下的没导出」**。

### 媒体句柄只对「内容摘要派生」的名字下发

按名取字节（`GET /api/v1/media/{id}`）要遍历**所有**会话的导出目录，所以它只服务**内容摘要派生**
的名字：同名即同内容，遍历结果才是确定的。平台给的名字（视频按 `video_hardlink_info_v4.file_name`
回落）与原始文件名回落只作**元数据**——把它们当句柄，会在别的会话里躺着一个同名异内容的文件时
变成随机 404，或者更糟：服务了错的那一份。生成侧同理：只有本次请求确实写出了摘要派生的本地文件
时才通告句柄（原生面的 `mediaId`、消息面回填的 `media.fileName`），非摘要派生的一律不给。

### 没有 `page` 块的响应会被读成「完整一页」

会话列表默认只给 100 条，而 ChatLab 的约定是：**响应里没有 `page` 块，就表示「这就是全部」**。
两者相乘的结果是「第 101 个会话凭空消失」——不报错、也没有任何提示。

所以 ChatLab 形状**必须**带 `page.hasMore` 与 `page.nextCursor`。加新端点时同理：
**分页信息不是可选项**，缺了它，截断就从「有损」变成了「静默丢数据」。
