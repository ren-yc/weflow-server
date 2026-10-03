# Changelog

本文件从 0.5.0 起维护。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [未发布]

### 变更

- **`clients/python`（`weflow-sdk`）`watch()` 的缓冲上限语义**：1 MiB 上限现在同时
  约束「单个完整帧」与「未成帧累计」——超限帧（无论格式是否合法）一律**不交付**，
  本次流结束并退避重连（原先超限的完整帧会先被交付）。这是行为变化；SDK 未发布、
  无迁移动作。回归位置：`clients/python/tests/test_behavior.py` 的
  `test_watch_rejects_oversized_single_frame_whole_and_split`（对照
  `test_watch_delivers_frame_just_under_the_cap`）。
- **`clients/python`（`weflow-sdk`）`watch()` 的重连退避**：退避只在「干净结束」
  （EOF、无超限、无传输错误）时复位到 0.5s，否则按 0.5→1→2→4… 升级到 30s 上限。
  原先每建一次连接即复位：畸形但能建连的流永远 0.5s 一轮（实测 3 秒 6 连），退避
  形同虚设。回归位置：`test_watch_backoff_escalates_on_persistent_overflow`（对照
  `test_watch_backoff_returns_to_floor_after_clean_stream_end`）。
- **`clients/python`（`weflow-sdk`）SSE 一帧多条 `data:` 行按规范以 LF 拼接**成单一
  载荷（原先逐行覆盖、只保留末行）。当前服务端每帧恰一行 `data:`，对现有形状逐字节
  中性；回归位置：`test_watch_joins_multiple_data_lines_per_frame`（三行边界对照
  `test_watch_joins_three_data_lines_boundary`）。
- **`clients/ts` 示例**：POST 显式携带 `Content-Type: application/json`（服务端 axum
  Json 提取器对非 JSON 内容类型回 415，示例此前必然踩中）；`smoke.ts` 第 3 步改为
  真断言——只接受「业务拒绝（StatusError 带 state）」或「受理后就绪等待超时
  （NotReadyError）」两种合法结局，其它结局（415/5xx/网络错误/未知异常）非零退出；
  新增可入库的 `stub-server.mjs`（无真实服务即可门禁：`STUB_STATUS=500` 必须让
  smoke 变红）；`watch` 补齐真实帧组装（`id:`/`event:`/`data:` 切分、空行成帧、
  `Last-Event-ID`、字节上限），与 qqflow 版同构（weflow 版为异步生成器、qqflow 版为
  回调），README 写明两版等价。
- **`clients/python`（`weflow-sdk`）`watch` 撤销从未生效的 `poll_interval` 形参
  （破坏性）**：该形参在 0.7.0 引入后从未被函数体读取（Rust 侧亦无对应参数），
  现已从签名移除。**迁移方式**：调用方删掉该实参即可，行为不变。SDK 未发布，
  已核实在役调用点为零。

### 新增

- **`clients/python` ruff 门禁**：`clients/python/pyproject.toml` 落显式的
  `[tool.ruff]` 与 `[tool.ruff.lint]` 表（规则集写死在配置里、不依赖默认集；
  生成层的排除只写在配置文件中——CLI 传相对排除按 cwd 解析、传 `--config` 会改变
  配置内相对路径的基准，两种写法都会让排除静默失效并把生成层整套扫进去）；
  CI `check.yml` 增加对 `src` 与 `tests` 的 ruff 步骤，并附「摘掉排除后命中必须
  涨到千量级」的区分力自检（本地实测 2253 条）；`ruff` 以精确版本钉进 dev extras
  （默认规则集随版本漂移，钉死才可复现）。手写层首批告警清零（pyupgrade 系与
  `RUF022` 自动修后逐处人工过 diff；`watch` EOF 支路的裸 `except Exception` 收窄为
  与主循环相同的 `(ShapeError, ValidationError, AttributeError)`）。
- **`clients/python/src/weflow_sdk/LICENSE`**：仓库根许可证复制进包树，随构建产物
  分发（消费方打包时由各自的 package-data 声明决定是否入 wheel）。
- **`clients/python/scripts/regen.py --check` 改为对生成树做摘要比对**（此前只比
  `spec.json`）：重生成到临时目录、与已提交生成树逐文件比摘要，模型层或空白层面的
  漂移不再可能「spec 没变就算绿」。生成器版本钉在 npx 调用里（wrapper 2.41.0 →
  openapi-generator 7.25.0），不再依赖可漂移的 latest。
- **`clients/rust`（`weflow-client`）**：类型化 Rust SDK（workspace 成员，随根包版本 0.7.0）。
  类型与操作客户端由 `/openapi.json` 描述生成（生成物入库，CI 断言「重生成无 diff」）；
  行为面手写六件套：`ensure_ready`（注册 + 就绪轮询，吸收 503 语义）、`drain_session`
  （Pull 游标原样回传排空）、`list_all_sessions`、`watch`（SSE 重连带 `Last-Event-ID`、
  心跳过滤、`generation` 变化上报）、`media_bytes`（404 后按「先 `media=1` 导出再取」重试一次）、
  `search`（`YYYYMMDD` 客户端校验，`end` 覆盖当天）。本轮**不发布** crates.io（本地可安装）。
- **`clients/python`（`weflow-sdk`）**：类型化 Python SDK（纯 Python，`httpx` + pydantic，`py.typed`）。
  模型由描述生成（`scripts/regen.py`，规范化后的 spec 一并入库，CI 断言「重生成无 diff」——
  spec 的来源是 Rust 侧生成工具的 `--dump-spec`，避免 golden 的占位掩码把易变值的真实类型烧成 string）；
  行为面与 Rust 侧逐方法同构：`ensure_ready` / `wait_ready`（wait-only 就绪轮询，不做任何注册动作）/
  `drain_session` / `list_all_sessions` / `watch` / `media_bytes`（404 后按「先 `media=1` 导出再取」重试一次）/`search`。
  `ensure_ready` 现在把 200 响应体里 `state` 为拒绝态（`account_conflict` 等）的应答映射为 `StatusError`
  快速失败，不再当作受理后空等到超时；`watch` 改为**单连接连续产出多帧**（原先每帧断开重连，空闲时
  也无限重连且每次重连都重放基线帧），分帧改为**字节级 LF**（`aiter_lines` 的 splitlines 语义会把含
  U+0085/U+2028/U+2029 的 JSON 正文拆断）并加 **1 MiB 未消费缓冲上限**（畸形无空行流不再无界膨胀），
  单帧解码失败记 WARNING 跳过而不再杀流，EOF 时把未终结残行并入末帧冲刷。
  本轮不发布 PyPI；测试对进程内 ASGI mock 跑，不依赖真库。
- **`clients/regen`（`weflow-regen`）**：生成工具（`cargo run -p weflow-regen`，`--check`
  供 CI 用）。它把服务端描述做确定性规范化（3.1 → 3.0：`type: [T, null]` 转 `nullable`、
  `Option<T>` 的 `oneOf` null 臂丢弃）后交给生成器 —— no-diff 门禁钉住的是整条
  规范化 + 生成管线。

### 修复

- **`/openapi.json` 为路径模板参数补 `parameters` 声明**：四条带占位符的操作
  （`DELETE /api/v1/accounts/{wxid}`、`GET /api/v1/media/{id}`、两条 Pull 路径）此前没有
  参数声明，违反 OpenAPI 规范（占位符必须有同名 path 参数）；golden 快照只记录输出、
  不校验合法性，因此一直无声。客户端生成器把这种文档当非法输入直接拒绝。
  守卫断言见 `tests/openapi.rs`（回归：模板占位符必须同名声明）。

## [0.7.0] - 2026-10-01

接口面的形状收敛：老面只做原生/富数据面，ChatLab 形状搬到 `/chatlab/*`；媒体只留一条按名取字节的路由；
鉴权通道减到两条。**破坏性变更较多**，逐条迁移见文末「迁移」。

- **`GET /chatlab/messages`** —— ChatLab 形状的消息面，也是原「混合面」
  （`/api/v1/messages?chatlab=1`）的新家：参数 `talker`（必填）/ `limit` / `offset` / `cursor` / 
  `start` / `end` / `media` / `keyword`；信封
  `{talker,count,page,chatlab,meta,members,messages}`，**不带 `success`**，
  `count` 是**本页条数**，消息**升序**，`page{hasMore,nextCursor}` 报告截断。
  `media=1` **真正执行导出** —— 旧的混合面在收集导出任务**之前**就 return 了，
  所以 `media=1` 在 ChatLab 形状上从未导出过；「先触发导出、再取字节」这条两步走在那个面上并不成立。
- **消息的媒体元数据**：拉取面与消息面每条消息新增 `media{type,fileName,md5}`（无媒体省略整键；
  `md5` 取不到时省略该键）。它是**元数据**：只有 `media=1` 且该条**确实写出了本地文件**时，
  `fileName` 才是可取句柄。
- 原生面 `media` 新增 `mediaId`：只在**本次请求的导出批次**确实写出了**内容摘要派生**的
  本地文件时出现（DB 名回落、外链、导出失败、超每请求上限的一律不给）。
- `group-members` 新增 `fromCache`（恒 `false`）与 `updatedAt`（**毫秒**，索引构建/更新完成时刻）。
- 通知面的**撤回帧**带上 `platformMessageId`（本仓事件里的 `rawid` 就是平台消息号）；
  `message.new` 仍为 `null` —— 给它要在推送热路径上逐事件查一次索引，而规范里该字段是可选的。

### 变更（破坏性）

- **老面不再输出 ChatLab 形状**：`/api/v1/messages` 与 `/api/v1/sessions` 上的
  `format=chatlab` / `chatlab=1` 开关已删除，两个面只输出原生形状。
- **读端点只有 GET**：`messages` / `sessions` / `contacts` / `group-members` / `media/{id}` /
  `push/messages` 的 POST 变 405。`/api/v1/sync` 与 `/health`、
  `/api/v1/accounts` 仍接受两个方法；`sns/*` 六条不变。
- **鉴权只剩两条通道**：`Authorization: Bearer` 与 `?access_token=`。
- **媒体字节只留 `GET /api/v1/media/{id}`**，三段式 `/api/v1/media/{talker}/{media_type}/{file}` 已删除。
- **`group-members` 的成员集合改为名册 ∪ 发言人**：从未发过言的成员也会出现（`messageCount` 为 0），
  `count` 会变大。同时删 `forceRefresh`（同步统一走 `/api/v1/sync`）与
  `withCounts` 别名，`refreshed` 键换成 `fromCache` + `updatedAt`。
- **`replyToMessageId` 在三个面统一为「无引用则省略该键」**（原生面此前是无引用给 `null`）。
- 原生面 `media` 的 `url` / `localPath` 未导出时**省略**（此前是空串）；
  `md5` 只在取不到时省略（已知摘要照给 —— 未导出不等于没有摘要）。
- 删除参数别名：`meiti` / `tupian` / `vioce`；删除 `POST /api/v1/accounts/{wxid}/deregister` 别名。
- 契约 pin 升到 `v0.4.0`（新增两条具名不变量与消息面用例；能力词表与端点登记同步）。

### 修复

- **按名取字节的同名多命中**：候选内容一致才服务（先比 size，必要时逐字节比较），不一致给 404。
  此前是「取第一个」且目录遍历顺序不确定 —— 同名在别的会话里可能是另一个文件，随便挑一个等于把
  「出现即可取」变成「出现即可取到某个东西」。只有**内容摘要派生**的名字才允许作为句柄下发，
  平台名（视频的 DB 名回落）与原文件名回落只作元数据。
- `group-members` 的排序稳定：先按 `messageCount` 降序、再按 uid 升序。只按计数排时，
  一大批计数为 0 的潜水成员顺序随哈希遍历顺序抖动，同一个群两次请求的顺序可能不同。
- **注销函数里那段英文注释改回与代码一致**：它写着重放历史「保持不动」，而代码是**清空条目 ＋
  保留 id 计数器 ＋ 推进 `generation`**（迁移到统一语义时改了行为，没改这段注释）。

### 文档

- `docs/weflow-server-api.md` 与 `docs/architecture.md` 随本批改动同步：路由清单、
  鉴权两条通道、`/chatlab` 四条路由、媒体按名解析与同名消歧规则、群成员集合。
- **补记**：老面 `GET /api/v1/sessions` 的 `cursor` 入参是随 0.6.0 的会话分页一起引入的
  （那时是纯增量）。本次它随「老面只认 offset」一起删掉了。
- `/chatlab/sessions` 的响应示例补上漏掉的 `count` 键（实际响应一直有它）。

### 迁移

过渡期一律**立即生效**。

| 改了什么 | 怎么迁 |
|---|---|
| 老面不再输出 ChatLab 形状（`chatlab=1` / `format=chatlab`） | 改用 `GET /chatlab/messages`（参数同名，见上）；会话发现面用 `/chatlab/sessions` |
| 老面不再识别 `cursor` | 老面用 `offset`；**注意失败模式**：继续传 `cursor` 不会报错，而是**静默回到第一页** |
| 旧参数拼写（`chatlab=1`、`format=chatlab`、`meiti`、`tupian`、`vioce`） | 一律**被静默忽略**（未知参数不报错）：改成 `media=1` 与类型参数，或改用新面 |
| 三段式媒体路由已删 | 改用 `GET /api/v1/media/{id}`（`{id}` 是导出文件名）；未导出前先请求 `media=1` |
| 鉴权只剩 Bearer 与 `?access_token=` | 删掉 `X-Api-Key`、`?token=` 与「把 token 放进 JSON body」的写法 |
| 原生面的 `replyToMessageId` 由「恒出现、无引用给 `null`」改为**省略该键** | 按下标「键存在」判断引用关系的代码改为「键存在**且非空**」 |
| `/chatlab/messages` 的外层没有 `hasMore`，改用 `page{hasMore,nextCursor}` | 按 `page.hasMore` 判断是否续页，用 `page.nextCursor` 回传 `cursor` |
| 读端点的 POST 变 405 | 改用 GET（参数走查询串） |
| `group-members` 的 `refreshed` 键没了，成员集合并了名册 | 改用 `fromCache` / `updatedAt`；按 `count` 分配 UI 的地方要接受更大的成员数 |
| 原生面 `media` 的 `url` / `localPath` 未导出时省略 | 判空从「空串」改为「键不存在」 |
## [0.6.1] - 2026-10-01

门禁与文档收口。**响应形状未变** —— 新增的是接口描述里的两条操作与更严的门禁。

### 新增

- **`/openapi.json` 补上两条真实存在的操作**：`POST /api/v1/sessions`、`GET /api/v1/sync`。
  它们一直能被调用却不在描述里，从描述生成客户端的人看不到它们。
- 路由现在有**唯一事实源**（`src/server/routes.rs`）：`build_router` 由它构建，
  与端点表的对等由 `documented_routes_match_the_openapi_table` 强制 —— 集合必须等于
  「路由 − 豁免」（`sns` 六个操作尚未 DTO 化，豁免写成代码常量），未声明的方法必须 405。

### 变更（对门禁，不对接口）

- 一致性套件带上跳过即失败：有用例被跳过时整套失败（此前跳过不影响退出码，
  「夹具少声明一个端点」会让用例静默变成不跑，而 CI 仍是绿的）。缺 `FLOW_CONTRACT_DIR`
  同样由静默通过改为失败。
- 契约 pin 升到 `v0.3.3`（`v0.3.1` 引入跳过即失败；`v0.3.2` 让 runner 校验 tag；
  `v0.3.3` 修公共段措辞）。夹具的 `contractVersion` 与 `conformance.pin` 由
  `pinned_contract_version_matches_the_fixture` 钉在一起，只改一处不再能溜过。
- golden 快照**缺失即失败**（此前缺失会被静默重建，drift 检测随之失效）。

### 修复

- SSE 面补 `mediaId` 的正路径端到端断言：导出根下有文件 → 帧里带 `mediaId` → 用该 id
  取回字节（此前只有单元测试证明「一次 stat 的判据对」）。
- 导出写入单测里两条「无残留」断言此前查错了位置（按 join 链算，一个落在 root 内、
  一个落在 root 的父目录），移除守卫时它们仍然是绿的。
- SSE 的 content-type 断言此前只在 `#[ignore]` 的真库测试里，现进了门禁。
- `CHANGELOG` 的 `[0.5.1]` 说明改为与史实一致：那段内容是**改名**归入 `0.6.0`，
  不是那时才写入本文件。

### 文档

- `docs/architecture.md` 补「工程与工具链」「测试与夹具」两节。
- `docs/weflow-server-api.md` 登记 `members[].roles` 为**有意不输出**（与 `isOwner` 同义，
  且受同一个「群主可能不在本页」的限制）。

### 迁移

无。
## [0.6.0] - 2026-09-27

ChatLab 适配层上线，**并接受一次破坏性发布**（五项，见下）。下游需按迁移表逐项核对。

> **版本归属**：安全修复与代理加固**实际随 0.5.1 发布**（2026-08-30，当时未单列条目）——
> 明细已归档到下方 **[0.5.1]** 节。本节只记 0.5.1 之后发生的变化。

### 修复

- `mediaId` **只在取得到字节时才通告**（「出现即可取」是承诺，不是尽力而为）。

### 破坏性变更

| 变更 | 改了什么 | 怎么迁 | 过渡期 |
|---|---|---|---|
| **`end=YYYYMMDD`** | 由「当天 0 点」变「当天 **23:59:59**」 | 若依赖旧语义，改用显式时刻参数 | **不适用** —— 唯一已知下游零影响（实测：只用 `start`）|
| **SSE `sync` 载荷** | weflow 统一为 `{event,generation,watermarks:[…]}`；qqflow 由平铺的 `lastRowidGroup`/`lastRowidC2c` 收敛为 `{event,watermarks:[{table,watermark}]}` | 订阅者若解析 `sync`，按新形状改 | **不适用** —— 唯一已知下游零影响（实测：显式忽略 `sync`）|
| **注销后重放** | 清重放条目 ＋ **保留** id 计数器 ＋ 基线带 `generation` | 依赖 `Last-Event-ID` 的下游需处理 `generation`（它区分「换了个账号」与「自己漏收了」）| **不适用** —— 唯一已知下游零影响 |
| **媒体 URL 去 token** | URL 不再内嵌 `?access_token=` | 旧的 `split("?",1)[0]` 写法**仍兼容**（不报错），可简化 | **不适用** —— 向后兼容 |
| **`message.new` 媒体字段** | weflow 补一个**过滤后的** `mediaId`（只在导出根下确有文件时出现），`md5` **保留** | 无 —— 纯增量 | **不适用** |

### 新增

- **ChatLab 适配面 `/chatlab/*`**（`baseUrl` 指向 `http://127.0.0.1:PORT/chatlab` 即可）：
  - `GET /chatlab/sessions` —— Pull 形状的发现面（`keyword` / `limit` / `cursor`）；
  - `GET /chatlab/sessions/{id}/messages` —— Pull 面；
  - `GET /chatlab/push/messages` —— **通知面**：只发元信息（`eventId` / `sessionId` / `timestamp` /
    `platformMessageId?`），**不发消息体**。规范对这条通道的定位是「仅通知，不假设事件可靠送达」，
    客户端收到后**去拉**那一页。
  - **老面 `/api/v1/*` 的路由集合与默认语义不变** —— 不关心新能力可以不迁。
- **SSE 连接建立就发一帧 `sync` 基线**：没有它，客户端在「连上」到「第一次水位变化」之间是盲的。
- **`generation`**：注销时递增，两个 SSE 面发同一个计数器。
- **`mediaId`**（SSE 的 `media` 里）：可直接走 `GET /api/v1/media/{id}` 取字节。
- **接入配方写在 `docs/*-api.md` 里**（四阶段 ＋ 实测数字 ＋ 接入者会踩的坑）。
- **群元数据接上真实数据源**（`/api/v1/group-members` 与 ChatLab 两面同步受益）：
  `groupNickname` 由恒空串 → 真实群名片（源 `contact_fts.db::chatroom_member_fts_v3`；
  缺该库时降级为空、服务照常启动）；`isOwner` 由恒 `false` → 每群恰一人 `true`
  （源 `contact.db::chat_room.owner`）；名册入库供 `/chatlab/sessions` 的 `memberCount` 使用。
  **此前 `group-members` 的群名片恒空曾让下游误判该端点冗余并退订——现在可以重新评估。**

## [0.5.1] - 2026-08-30

本版发布于 08-30，当时没有单列变更条目：内容挂在 `[未发布]` 标题下（三项安全修复由 `54b1fc2`
于 08-29 写入），0.6.0 只是把那一段**改名**归入自己名下。此处补记归档，**安全修复的归属以本节为准**。

### 安全

- **修复 SNS 导出的路径遍历（任意文件写入）**。`GET|POST /api/v1/sns/export` 的产物名原为
  `format!("sns-{scope}-{stamp}.{format}")`，其中 `scope` 是原始 `username` 请求参数——该参数
  只用作动态过滤条件，从不与 store 校验（传未知值得到空导出而非 404），因此以自由文本形式
  抵达 `exports_dir.join()` + `std::fs::write`。`sns-` 前缀**不构成防护**：Win32 会剥掉路径
  分量末尾的点，`sns-..` 因而规范化成一个名为 `sns-` 的普通目录，剩余载荷继续上穿；
  Windows 的规范化还是词法的、在用户态完成，中间目录无需存在即可折叠 `..`（同一载荷在
  Linux 上会 `ENOENT`）。扩展名由 `format` 控制、内容由 feed 控制，故为写原语。已实测确认。
  修复：`username` 先经 `pathsafe::slugify` 折叠，再对规范化后的根断言包含关系。
- **修复媒体代理的 SSRF（主机白名单可绕过）**。两处独立缺陷：`host_matches` 剥 userinfo 时
  对 `@` 做正向切分、取前段，而 URL 中 userinfo 在前，于是 `https://qq.com@evil.example/`
  递给白名单的是 `qq.com`、curl 连的是 `evil.example`；authority 又只在 `/` 处结束，`?x=.qq.com`
  同样可混过。此外 `curl -L` 由 curl 自行跟随跳转，发生在那次一锤子检查**之后**——白名单内
  任一开放重定向（`qq.com` 面积很大）即可将该端点变为任意目标抓取器，含回环与
  `169.254.169.254`。修复：`check_proxy_url` 同时作用于调用方 URL 与**每一跳**重定向目标
  （故去掉 `-L`、自行接管重定向循环，上限 5 跳）；`url_host` 改为从右侧切分 userinfo，并在
  `/`、`?`、`#` 三者最先出现处结束 authority。
- **导出写入方补齐包含性校验**。`media::export::write_out` 原先对 `talker` / `file_name`
  两个路径分量零校验，而 `file_name` 派生自消息自带的 `md5` XML 属性、`parser::attr` 亦零校验，
  即**发送方可控**。此前拦住它的是两处巧合（图片路径的 md5 完整性校验无法与非摘要字符串相等；
  `walk_find` 比对 `file_name()`，其中永不含分隔符），且无任何地方记录这两条性质是承重的。
- 新增 `pathsafe` 模块，收敛为全仓唯一的路径分量语义（`safe_segment` / `slugify` /
  `is_contained`），四处拼路径的调用点（SNS 导出、媒体路由、注销清理、导出写入方）全部改走它。
  原先四者各自漂移出不同的检查子集，均未覆盖末尾点、`:`（NTFS 备用数据流 `name.jpg:hidden`，
  压根不含分隔符）与控制字符。规则是**先派生名字，再对规范化根断言包含关系**——仅过滤输入
  正是 SNS 导出得以逃逸的原因。`is_contained` 失败时关闭，不退化为拿非规范化根做词法比较
  （verbatim 前缀与裸路径混比会静默永不匹配）。

### 修复

- 媒体路由的 `canonicalize` 与代理的 curl 子进程原先在 tokio worker 上做阻塞 IO，并发媒体读取
  会拖住无关请求（含 SSE 心跳），改入 `spawn_blocking`。
- 媒体代理原先把响应体写到共享临时目录下一个以纳秒时间戳命名的可预测路径，改为经 stdout
  管道回传、元数据走 stderr 带标签行（二进制载荷不可能被误读为状态行）。
- 媒体代理硬编码 `curl.exe`，与其"Windows/macOS/linux 都自带"的注释矛盾，改为按平台取二进制名。

### 变更

- **ChatLab 两个面对齐 0.0.2 标准枚举**，并把账号名与群名片拆成两个字段——依赖旧枚举取值、
  或把群名片当账号名使用的下游需同步调整。
- **`localType` 按打包字段掩码解析**：高 32 位（appmsg 子类型）此前不掩码即丢失，appmsg 一族
  大量落 `99 OTHER`；掩码后类型覆盖面恢复。
- Pull 面删除恒空的 `mediaPath`：该键从未有过值、规范列为可选——依赖它的下游改用 `media` 对象。
- CI 加固：clippy `-D warnings` ＋ 全量测试的 PR 门禁上线；路径逃逸用例按平台拆分；
  `graceful_shutdown` 用例串行化消除凭据库竞态。

### 说明

- 已知残留：白名单后缀下的主机名若**解析到**内网地址仍可通过。堵住它需把 DNS 解析放到能在
  连接前检查结果的位置，属抓取方式的实质改动而非校验层微调，已在 `proxy_fetch` 处注明。
- SNS 导出的产物**文件名**形状随之变化：`username` 中的非 `[A-Za-z0-9-_]` 字节折叠为 `_`
  （如 `12345678@chatroom` → `sns-12345678_chatroom-<stamp>.json`）。响应内容与导出语义不变。
- 遍历写入位于鉴权之后（`require_auth` 守着该端点）。若该 token 是共享的或强度不足，应按
  远程任意写入对待。

## [0.5.0] - 2026-08-28

账号面（注册 / 健康检查 / 注销）重新设计。**不兼容 0.4.x**：`/health` 响应形状变更，
下游需同步改造（见 `docs/weflow-server-api.md`）。

### 新增

- `GET /api/v1/accounts`（需鉴权）：返回账号明细
  `{wxid, state, message_count, error?, db_storage}`，含启动扫描发现但未注册的账号
  （`awaiting_key`）。**不受就绪门控**——账号 `indexing` 时正是客户端要轮询它的时候。
- `DELETE /api/v1/accounts/{wxid}`（需鉴权，别名 `POST /api/v1/accounts/{wxid}/deregister`）：
  注销账号，停止其 sync、清空索引，服务器回到未注册状态。可选 `purge_media=1` 清理该账号
  导出的媒体目录（**默认 false**）。判定 `deregistered` / `not_registered` / `wxid_mismatch`
  一律 HTTP 200。

### 变更

- **强制单账号**：同时只允许一个账号处于绑定态。注册第二个 `wxid` 返回
  `state=account_conflict`（HTTP 200，附 `occupied_by`/`occupied_status`），**在校验路径与
  密钥之前**即判定，且在位账号完全不受影响；换账号需显式注销。`error` 态账号仍持有绑定，
  一次解密失败不会把服务器交给别的账号；重新注册同一 `wxid` 即可重建自愈。
- `/health` 与 `/api/v1/health`（免鉴权）改为标量：`{status, version, account}`，
  `account` ∈ `unregistered | indexing | ready | error`。**不再列出账号数组**——该端点免鉴权，
  而启动扫描会为本机每个 `xwechat_files` 账号目录建条目，数组（乃至其长度）本身即泄露本机
  存在哪些账号、各自进度如何。账号身份、消息数、库路径与错误原因移至需鉴权的
  `GET /api/v1/accounts`。
- `AccountSync` 新增退场标志：注销后进行中的增量轮询会丢弃本轮读取结果而非写入已清空的
  store，也不再发事件（总线是进程级的）；索引中途注销不会在构建完成后复活账号。
- `register_account` 返回 `BindOutcome`（`Bound`/`Existing`/`Occupied`），守卫与插入共享同一次
  registry 锁——分两次取锁会让两个并发注册都看到空闲绑定。`start_account` 原样透传该判定。
- `/health` 的就绪判定改由 `AppState::account_phase` 单独提供（不取 store 读锁、不看发现结果）；
  `account_views` 退化为纯明细（只返回列表），不再兼职算就绪标志。
- `/api/v1/contacts` 的 `nickname` / `remark` / `alias` / `avatarUrl` 缺值时下发空字符串而非
  `null`。这是全仓最后一处直接序列化 `Option` 的地方——`/api/v1/group-members` 的同名四字段、
  以及 `/api/v1/messages` 与 ChatLab Pull 的 `groupNickname` / `avatar`（键名不同、来源同为
  联系人行）早已一律 `unwrap_or_default()`。`store::Contact`
  内部仍用 `Option<String>`——`display_name()` 要靠它区分「无 remark」与「remark 为空串」来做
  `remark > nickname > username` 回退，只有 JSON 边界拍平。
- 导出媒体 404 改用统一错误信封 `{success, code, message}`，不再返回
  `{"error": "Media not found"}`。此前同一个端点有两种 404 形状：路径不存在走裸 `error` 键，
  路径存在但打不开走信封。
- 启动扫描日志只报数量，不再打印发现的 wxid 清单。`/health` 与 `account_views` 为「免鉴权
  不得枚举账号」付了类型级代价（`AccountPhase` 没有 `AwaitingKey` 变体），日志打印完整清单
  会把这层设计绕过去；清单仍由需鉴权的 `GET /api/v1/accounts` 提供。
- 索引期间被注销的构建日志从 DEBUG 升为 INFO，并区分「索引已完成 / 索引失败 / 索引任务异常」
  三种结局。默认 `info` 级别下这是「注销一个 `indexing` 账号」在日志里的唯一痕迹，此前不可见。

### 说明

- 注销**不清空** SSE 重放历史：历史是进程级的，其 `id` 是总线级单调序列，清空会破坏与该账号
  无关的订阅者的 Last-Event-ID 重放。改为重新广播剩余就绪账号的水位基线（单账号下即空数组）。
- 注销后启动扫描发现过的账号回到 `awaiting_key` 并保留路径（它确实还在本机上），
  纯客户端注册的账号则整条消失。
- `purge_media` 默认关闭：导出布局是 `<talker>/<kind>/<file>`，没有账号维度，两个账号与同一
  talker 的会话共用一个目录，清理可能删掉另一账号导出的文件。清理范围严格限定在导出器写入的
  四类子目录，talker 目录本身仅用 `remove_dir`（非空即保留），**永不递归删除导出根**。
