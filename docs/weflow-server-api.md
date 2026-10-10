# weflow-server HTTP API（v2 参考）

> 本文档以源码（`src/server/`）为准，描述当前实现的全部接口。
> 与 WeFlow 安装版 `HTTP-API.md` 契约对齐的部分在原文处标注。

- 服务默认监听 `127.0.0.1:5033`（`--port` 可改）
- 数据目录 = `%LOCALAPPDATA%\weflow-server`；访问 token 存于**系统凭据库**（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service；无凭据库平台为会话级 token，随启动日志打印）
- 所有 `/api/v1/*` 端点需要鉴权；`/health` 与 `/api/v1/health` 不需要

## 鉴权

两种传输等价（源码 `server/auth.rs::authorized`），按下表顺序检查：

| 方式 | 示例 |
|---|---|
| HTTP 头 | `Authorization: Bearer <token>` |
| 查询参数 | `?access_token=<token>` |

> 只有这两条。`X-Api-Key`、`?token=` 这第二种拼写、以及 POST JSON body 都**不是**通道：每多一条
> 就多一处凭据会被复制到的地方（请求体、代理日志、客户端抓包），而它们鉴权的是同一个东西。
> body 尤其不成立 —— 它连「这是谁的凭据」都区分不出来：`handlers::extract_params` 从不把
> `access_token` / `token` 从 body 带进参数表。

> 两种传输的 token 比对均为**常时比较**（constant-time，无提前返回，防时序侧信道；与 qqflow-server 同款）。

## 错误信封

```json
{ "success": false, "code": 400, "message": "..." }
```
- `success` 布尔；`code` 为 HTTP 状态码；`message` 人类可读描述。
- 状态码：鉴权失败 `401`；参数错误 `400`；未找到 `404`（**含未知路径**）；方法不允许 `405`；
  服务未就绪 `503`；服务异常 `500`。

> 这里**没有 403**：本服务只有「有 token / 没 token」这一种授权判断，不存在「已认证但无权访问」
> 的场景，因此没有构造器。此前文档写了 403 —— 那是照抄通用清单，**不要为它补一个假的实现**；
> 真出现权限模型时再加。

## 通用查询参数

| 参数 | 说明 |
|---|---|
| `limit` | 条数上限（默认 100，上限 10000） |
| `offset` | 偏移量（默认 0） |
| `cursor` | 翻页游标：**只有 `/chatlab/sessions` 与 `/chatlab/messages` 认它** —— 把上一次响应的 `page.nextCursor` 原样传回，解析不了就退回 `offset`。老面（`/api/v1/*`）只按 `offset` 翻页，多传的 `cursor` 会被忽略 |
| `start` / `end` | 时间边界：Unix 秒，或 `YYYYMMDD`。**`start` 取当天 0 点，`end` 取当天 23:59:59**（上界是包含的，日期形态必须覆盖当天）。无法解析时该条件被忽略，不报 400 |
| `keyword` | 关键词过滤（小写匹配；sessions/contacts/messages 通用） |
| 导出开关 | `media=1` 开启媒体导出；再按类型 `image=1`/`voice=1`/`video=1`/`emoji=1` 收窄到某一类 |

> **没有 ChatLab 开关**：`format=chatlab` / `chatlab=1` 已不存在，老面只输出原生/富数据形状。
> ChatLab 形状一律走 `/chatlab/*`（见「ChatLab 适配面」）—— 调用方不必知道还有另一种形状，
> 也不会因为漏传一个开关而拿到另一种。

> **「上限」分属四个不同的面，别互相换算**（文档里 200／500／5000／10000 都出现过，混读会得出
> 「两仓口径不一致」的错误结论；下表按源码实测逐端点列出）：
>
> | 数字 | 属于哪个面 | 含义（源码位置：各 handler 的 `parse_limit`／`clamp`） |
> |---|---|---|
> | **10000** | `GET /api/v1/sessions`、`/api/v1/contacts`、`/api/v1/messages`、`/chatlab/messages` | **单个请求返回条数**的硬上限（`limit` 默认 100） |
> | **5000** | Pull 面 `GET /api/v1/sessions/{id}/messages` 与 `/chatlab/sessions/{id}/messages` | 单页上限 5000，且**默认也是 5000**（与上面那几个面的默认 100 不同） |
> | **500** | `GET /api/v1/sns/timeline` | 朋友圈时间线单请求上限（默认 50）；这是朋友圈自己的面，与消息条数无关 |
> | **200**（MCP）| MCP 工具参数 `limit`（见 `docs/mcp.md`）| **工具层**对一次取页的默认 50／上限 200，比 HTTP 更严；它是「模型经 MCP 取数据」这一层的自限，与 HTTP 上限不构成矛盾 |
> | **200**（导出）| `media=1` 的**每请求导出项上限**（见「媒体导出」小节）| 一次请求**最多触发 200 项媒体导出**，不是返回条数上限；超出的项保持未导出，`exported` 不为真 |
>
> 两点容易踩的坑：① `GET /api/v1/group-members` **没有 `limit`**（整名册一次给全，不参与上面的换算）；
> ② SDK 形参 `page_size`（`list_all_sessions`）**不是 HTTP 参数名**——它内部发的就是 `limit`，
> 拿它当「另一个上限」会算错。所以「联系人一次最多能拿多少」＝10000，走 MCP 则被压到 200，
> 而 `media=1` 的 200 与这两者**无关**。

## 端点

### GET `/openapi.json` — 接口描述（**免鉴权**）

返回本服务的 OpenAPI 3 描述，由 `server/dto.rs` 里的响应类型经 `#[derive(ToSchema)]` 生成。

免鉴权是刻意的：它描述的是**形状**，不含账号、路径或密钥，而且正是给尚未拿到 token 的接入方
看的 —— 拿到它就能生成客户端，再凭 token 调真正的接口。

两点使用提示：

- **多形状端点用 `oneOf`**。`/api/v1/accounts`（POST）与 `/api/v1/accounts/{wxid}`（DELETE）
  的响应形状由状态决定，描述里列的是若干可能形状的并集 —— 生成客户端时应按 `oneOf` 处理，
  而不是当成「所有字段都可能存在」。
- **描述随 DTO 变**。改 DTO 就会改它，不需要手工同步；`tests/openapi.rs` 保证描述自身自洽
  （每个 `$ref` 都能解析、operationId 唯一、多形状确实用 `oneOf`），golden 快照则保证它
  的变更有人看过。
- **字节面与 SSE 面如实标注媒体类型**：媒体路由是 `application/octet-stream`（binary），
  推送面是 `text/event-stream`；端点级 `description` 携带各面的对外闸门（导出每请求 200 项、
  单页上限 5000、重放缓冲 1000 条/600 秒等）——生成的客户端不必再翻散文文档找这些数字。

错误响应（401/404/405 等）**不在描述里**：它们是跨端点的统一信封，见上文「错误信封」。

### GET/POST `/health`、`/api/v1/health`（免鉴权）

就绪状态 + 单个账号阶段（客户端可据此健康检查，无需轮询注册端点）：

```json
{
  "status": "ok",
  "version": "<version>",
  "account": "ready"
}
```

- `status`：`ok`（已绑定账号且 `ready`）或 `starting`（未注册 / 仍在 indexing / error）。
- `account` ∈ `unregistered | indexing | ready | error`：
  - `unregistered`——从未注册，或已被注销；
  - 其余三值即绑定账号的状态机值。

**本接口刻意不列出账号。** 它免鉴权，而启动扫描会为本机每个 `xwechat_files` 账号目录建一个
条目，因此账号数组——乃至它的长度——本身就在告诉任意未鉴权调用方：这台机器上有哪些账号、
各自进展到哪一步。账号身份、消息数、库路径与错误原因一律改由需鉴权的
[`GET /api/v1/accounts`](#get-apiv1accounts--账号明细需鉴权) 提供。

注意 `awaiting_key` **不会**出现在这里：启动扫描发现但未注册的账号不构成绑定，`account` 仍是
`unregistered`（它们在账号明细接口里可见）。

### GET `/api/v1/accounts` — 账号明细（需鉴权）

`/health` 不再承载的账号信息：

```json
{
  "success": true,
  "accounts": [
    {
      "wxid": "wxid_xxxx_1234",
      "state": "ready",
      "message_count": 217272,
      "db_storage": "D:\\AppData\\xwechat_files\\wxid_xxxx_1234\\db_storage"
    }
  ]
}
```

- `state` ∈ `awaiting_key | indexing | ready | error`（与账号状态机一致）；账号处于 `error` 时
  附 `error` 字符串（错误原因），其余状态不含该键。
- 除绑定账号外，还包含启动扫描发现但**尚未注册**的账号（`awaiting_key`），客户端可据此在注册
  之前看到本机存在哪些账号。
- 列表按 `wxid` 升序，便于客户端做稳定 diff。
- **不受就绪门控**：账号 `indexing` 时服务器正是「未就绪」，而那恰好是客户端要轮询本接口的
  时候。GET 无 body，token 走请求头或查询串。

### POST `/api/v1/accounts` — 注册账号（客户端驱动启动）

请求体（JSON）：

```json
{
  "wxid": "wxid_xxxxxxxxxxxxxxxx_1234",
  "db_path": "D:\\AppData\\xwechat_files\\wxid_xxxx_xxxx",
  "keys": { "session/session.db": "<64-hex enc_key>", "message/message_0.db": "<64-hex>" },
  "img_aes_key": "<16 位 hex>",
  "img_xor_key": "0x64"
}
```

| 字段 | 说明 |
|---|---|
| `wxid` | 账号标识（必填） |
| `db_path` | 账号根目录（含 `db_storage/`；默认按 wxid 推导） |
| `key` | 可选：每库统一 enc_key（`keys` 缺省时的单一密钥） |
| `keys` | 可选：`db_storage` 相对路径 → 64-hex enc_key 映射（微信 4.x 每库独立密钥） |
| `img_code` | 可选：WeFlow 兼容的图片密钥代号（由服务端派生 aes/xor） |
| `img_aes_key` / `img_xor_key` | 可选：直接指定图片解密密钥（优先于 `img_code`） |

响应：

```json
{ "success": true, "wxid": "wxid_xxxx_1234", "state": "accepted", "status": "indexing", "db_storage": "D:\\AppData\\xwechat_files\\wxid_xxxx_1234\\db_storage" }
```

- `state`：注册结果语义（qqflow-server 风格）——`accepted`（已接受，开始后台构建）/ `already_ready`（重复注册，账号已就绪）/ `in_progress`（重复注册，正在构建中）/ `account_conflict`（本服务已绑定**另一个** wxid，拒绝注册）；
- `status`：账号当前状态机值 ∈ `awaiting_key | indexing | ready | error`；
- `db_storage`：实际使用的库目录。

`account_conflict` 时不含 `status` / `db_storage`，改附在位账号信息（HTTP 仍为 `200`，`success`
仍为 `true`——请求本身合法，只是被策略拒绝）：

```json
{ "success": true, "wxid": "wxid_new_5678", "state": "account_conflict",
  "occupied_by": "wxid_xxxx_1234", "occupied_status": "ready" }
```

行为契约：
- **强制单账号**：一个进程同时只绑定一个 wxid。要换账号必须先注销（见下节），服务器不会为你
  静默顶掉在位账号——它可能正在被另一个客户端使用。
- **判定顺序**：冲突检查在密钥校验**之前**。占用中的服务器对携带别的 wxid 的注册一律回
  `account_conflict`，不会因为顺序颠倒而先返回 `400 密钥错误`，从而把「这个密钥对不对」告诉
  一个本来就无权注册的调用方。
- **密钥仅存进程内存，不落盘**；服务重启后需重新注册。
- 注册时对目标库做页 1 HMAC 预校验（`wcdb::verify_page1`），错钥立即拒绝（`400`）。
- 成功后启动阻塞式全量构建 + 文件事件监视任务；构建完成前账号状态为 `indexing`。
- **注册幂等**：重复注册**同一** wxid 且其状态为 `ready`/`indexing` 时**不会重建索引**、不会中止
  watcher，直接返回现有句柄（`state` 为 `already_ready`/`in_progress`）；仅 `error`（或
  `awaiting_key`）状态会被替换重建——密钥 / 路径填错后重新注册即可自愈。`error` 账号**仍持有
  绑定**，别的 wxid 依然会撞 `account_conflict`。

### DELETE `/api/v1/accounts/{wxid}` — 注销账号（需鉴权）

释放绑定、清空内存索引、退场后台任务，服务器回到未注册状态（`/health` 的 `account` 变回
`unregistered`）。注销只有这一条路由（回归见 `deregistration_is_authenticated_and_the_post_alias_is_gone`）。

| 参数 | 说明 |
|---|---|
| `{wxid}` | 路径参数：要注销的账号。**安全联锁**——与在位账号不一致时什么都不做 |
| `purge_media` | 可选，默认 `false`。同时删除本账号会话的媒体导出目录 |

响应（三种结果，HTTP 均为 `200`）：

```json
{ "success": true, "wxid": "wxid_xxxx_1234", "state": "deregistered",
  "previous_status": "ready", "index_cleared": true,
  "purged_media": false, "purged_dirs": 0 }
```

- `state: "deregistered"`——已注销。`previous_status` 是注销前的状态机值，`index_cleared` 表示
  内存索引确有内容被清空。
- `state: "not_registered"`——本就没有绑定账号。**幂等**：重复注销不报错。
- `state: "wxid_mismatch"`——路径里的 wxid 不是在位账号，附 `occupied_by` / `occupied_status`，
  **在位账号毫发无损**。这是防误注销的联锁：客户端以为自己在注销自己的账号，实际上服务器绑的
  是别人的。

行为契约：
- 允许在 `indexing` 中途注销。正在跑的全量构建会看到退场标志并丢弃结果，不会把数据写回已
  释放的索引。
- **SSE `history` 不清空**。事件总线与历史缓冲是进程级的、`id` 单调递增，清空会破坏无关订阅者的
  `Last-Event-ID` 重放。取而代之：注销后广播一条空的 `sync` 基线事件，订阅者据此得知水位归零。
- 启动扫描发现的账号注销后**退回 `awaiting_key`**（仍在账号明细里，可再次注册）；纯客户端指定
  路径的账号注销后彻底消失。
- `purge_media=true` 只删已知布局 `<media_export_dir>/<talker>/{images,voices,videos,emojis}`，
  随后仅在会话目录已空时删除它；导出根目录本身永不触碰，异常 talker 名（空、`.`、`..`、含路径
  分隔符）一律跳过。`purged_dirs` 是实际删除的会话目录数。
- **不注销**不会释放绑定：进程重启同样回到未注册状态，但那会丢掉所有内存索引与密钥。

### GET `/api/v1/messages` — 消息查询 + 媒体导出

参数：`talker`（会话标识，必填，为空返回空结果）、`limit`、`offset`、`start`、`end`、
`keyword`、`media` 及类型开关（见通用参数）。

这条只输出原生/富数据形状；ChatLab 形状走 `GET /chatlab/messages`（见「ChatLab 适配面」）。

消息对象键：

```json
{
  "localId": 1,
  "serverId": "8280000000000000001",
  "localType": 1,
  "baseType": 1,
  "appmsgSubtype": null,
  "createTime": 1700000100,
  "sortSeq": 0,
  "isSend": 0,
  "senderUsername": "wxid_friend_a",
  "senderName": "客户张三",
  "content": "你好",
  "rawContent": "你好",
  "parsedContent": "你好",
  "replyToMessageId": "8200000000000000000",
  "quote": { "platformMessageId": "...", "sender": "...", "accountName": "...", "content": "...", "type": 1 } | null,
  "media": { "type": "image", "fileName": "...", "md5": "..." } | null
}
```

- `serverId` 为**字符串**：i64 超出 JS 安全整数范围，直接出数字会在浏览器端丢精度。
- `isSend` 为数字 `0`（对方/系统）或 `1`（自己），不是布尔。
- 三个可选键各有各的条件，不要用同一条规则读：
  - `replyToMessageId`：**有引用才出现，无引用时省略该键**（不是给 `null`）。下游若按「键在不在」
    判断引用关系，应写成「键在且非空」。这条与消息面、拉取面同规 —— 三个面不再有形状差异。
  - `quote`：**恒出现**，无引用时是 `null`（引用快照与引用 id 是两件事：前者可能缺失，后者不会）。
  - `media`：**恒出现**，解析不出媒体时是 `null`；只要解析出媒体就带上（与 WeFlow 形状一致），
    与 `media=1` 无关。未导出时 `url` / `localPath` / `mediaId` **都不出现**（见下文的导出小节）。

#### `localType` 是打包字段：`baseType` / `appmsgSubtype`

微信 4.x 把两个值塞进同一列：

```
localType = (appmsgSubtype << 32) | baseType
```

一条文件附件存的是 `(6 << 32) | 49` = `25769803825`，**不是 49**。真库 218558 条里
62494 条（28.6%）的高 32 位非零，全是 appmsg；另有 6 条是 `(17 << 32) | 11000`，所以
掩码是无条件的，不能写成"仅当 base 是 49 时才掩"。

- `localType`：**原样输出打包值，永不改动**。下游已按它分支（例如按
  `21474836529` 认链接卡片），改成掩码后的值会让这些判断全部落空。
- `baseType`：低 32 位，就是常规的 `1/3/34/43/47/49/…`。
- `appmsgSubtype`：高 32 位，**仅当 `baseType == 49` 时给值**，否则为 `null`。
  11000 的高位 17 不是 appmsg 子类型，当成子类型发出去会诱导下游误读。

新增这两个字段是为了让下游不必硬编码 12 位打包常数：判链接卡片写
`baseType == 49 && appmsgSubtype == 5`，比 `localType == 21474836529` 可读得多。
两个字段都是**只读派生值**，不进任何 ChatLab 面（那边有自己的 `type` 枚举）。

常见 `appmsgSubtype`（真库计数）：5 链接卡片 36629、57 引用回复 17005、6 文件 3530、
19 合并转发 991、51 视频号 919、33/36 小程序 284、4 图文 384、2001 支付 269。

响应：

```json
{ "success": true, "talker": "wxid_friend_a", "count": 20, "hasMore": true,
  "media": { "enabled": false, "exportPath": "", "count": 0 },
  "messages": [ ... ] }
```

媒体导出：`media=1`（可再叠加类型开关）时，服务端按需导出本页媒体，并就地补齐每条消息
的 `media.url` / `media.localPath` / `media.exported`；顶层 `media` 为汇总：

```json
{ "enabled": true, "exportPath": "C:\\...\\api-media", "count": 5 }
```

```json
"media": { "exported": true, "fileName": "...", "localPath": "C:\\...\\api-media\\...",
           "md5": "...", "mediaId": "...", "type": "image",
           "url": "/api/v1/media/<file>" }
```

- **`exported: true` 是导出成功的唯一判据**；仅有 `media` 对象不代表字节可取（缺 md5 的
  非语音消息会被跳过），顶层 `media.count` 等于本页 `exported` 为真的条数。
- 单次请求最多导出 200 项以限制延迟，超出部分保持未导出，可缩小 `limit` 分批取。
- `url` 是**根相对路径**（形如 `/api/v1/media/<file>`），且**不含 token**：
  token 是只走请求头的凭据，拼进响应体会被复制到客户端日志与任何中间缓存；相对路径也
  免掉了把服务基址烤进响应——反代或换端口之后下发的地址仍然有效。调用方按自己的基址
  拼接，取字节时带上鉴权头。
- `mediaId` 是**可直接喂给字节面的句柄**（`GET /api/v1/media/{id}`）。它只在**本次请求确实
  写出了本地文件**、且文件名**由内容摘要派生**时出现 —— 外链（表情的 CDN 地址）与来自平台的
  名字（视频按 DB 名回落）都不给：按名取字节是**跨会话**解析的，平台名字可能同名异内容。
  未导出、导出失败、超出每请求上限的同样没有这个键。
- **未导出时 `exported` / `url` / `localPath` / `mediaId` 都不出现**（不是空串、也不是 `false`）：空串会被
  读成「有地址、有路径，只是空的」，而正确的读法是「这次没有导出」——`exported` 是条件键，靠「键在不在」
  判断字节可不可取。`md5` 只在**取不到摘要**时才省略 —— 未导出不等于
  没有摘要，已知摘要照给。
- 例外：表情（`emoji`）可能返回 CDN 绝对地址，那是第三方地址，不是本服务的路径。
- `type`→目录映射：`images / voices / videos / emojis`。

> 文件附件（`file`）暂不参与导出：与 WeFlow 官方契约一致，媒体导出仅覆盖图片/语音/视频/表情四类。

`appmsgSubtype == 6` 的文件附件会带 `media` 对象（`type: "file"`，含真实
`fileName` 与 `md5`），但 `exported` **恒为假** —— 文件类被导出闸门显式拒绝，不是因为
缺字段。SSE 推送的 `media` 元数据同形。判断字节是否可取始终看 `exported`，不要看
`media` 是否存在。

`media.fileName` 取自 appmsg 的 `<title>`（自动剥离 CDATA 包裹），并按 Windows
非法字符表做净化；`<title>` 缺失或为空时回落 `file_<localId>`。真库 3530 条文件里
回落 0 条。`md5` 3528 条有值 —— 另 2 条的 `<md5>` 元素存在但内容为空，属源数据缺失。

**ChatLab 形状不在这条路由上。** 它走 `GET /chatlab/messages`（见「ChatLab 适配面」）：那条天生
就是 ChatLab 形状，信封与参数都更接近规范（`page` 翻页、无 `success`、消息升序）。

安装版契约里的 `messages[].mediaPath` **本项目不输出**（消息面与拉取面都不输出）：它描述的是
批量导出的落盘位置，而本服务只做按需导出，给不出有意义的值。媒体字节走本接口的 `media` 对象
（`mediaId`）与 `GET /api/v1/media/{id}`。

### GET `/api/v1/sessions` — 会话列表

参数：`limit`、`offset`、`keyword`。翻页只按 `offset`；`cursor` 属于 ChatLab 面（见
`GET /chatlab/sessions`），这里多传会被忽略。

```json
{ "success": true, "count": 315, "sessions": [
  { "username": "wxid_xxx@chatroom", "displayName": "项目群",
    "type": 1, "sessionType": "group",
    "lastTimestamp": 1700000100, "unreadCount": 2, "messageCount": 4, "summary": "..." }
] }
```

`type` 为数值枚举：`0` 私聊、`1` 群聊、`2` 公众号、`3` 其他；`sessionType` 是同一枚举的
字符串形式（`private` / `group` / `official` / `other`）。**下游建议用 `sessionType`**：
qqflow-server 的 `type` 取值为 `1` 私聊 / `2` 群聊，数值含义与本项目不同，字符串则一致。

按 `lastTimestamp` 降序、`username` 次键（全序稳定，便于 offset 翻页）。

这条只输出原生形状。ChatLab 形状走 `GET /chatlab/sessions`：它带 `count` 与
`page{hasMore,nextCursor}`，`type` 是 `group` / `private` 字符串（见「ChatLab 适配面」）。

### GET `/api/v1/sessions/{id}/messages` — ChatLab 拉取（消息游标）

同一份实现也挂在规范约定的 `GET /chatlab/sessions/{id}/messages` 上：**同一个 handler、同一形状**，
两条路径逐字节同形（回归见 `both_pull_paths_are_byte_identical`）。`/api/v1/...` 是 WeFlow 兼容面
（安装版自己就有这条路径），`/chatlab/...` 是 ChatLab 的挂载点。

参数：`since`、`end`、`limit`（默认/上限 5000）、`offset`；`talker` 由路径段 `{id}` 提供。
返回 ChatLab 契约形状（`platformMessageId` 等键），并附带 `sync` 游标块：

```json
{ "chatlab": { "version": "0.0.2", "generator": "weflow-server", "exportedAt": 1700000000 },
  "meta": { "name": "项目群", "platform": "wechat", "type": "group", "groupId": "...@chatroom", "ownerId": "wxid_self" },
  "members": [
    { "platformId": "wxid_member_b", "accountName": "李四", "groupNickname": "四哥", "avatar": "" }
  ],
  "messages": [
    { "sender": "wxid_member_b", "accountName": "李四", "groupNickname": "四哥",
      "timestamp": 1700000103, "type": 0, "content": "大家好", "platformMessageId": "8200000000000000000",
      "media": { "type": "image", "fileName": "aabbccddeeff00112233445566778899.jpg", "md5": "…" } }
  ],
  "sync": { "hasMore": true, "nextSince": 1700000103, "nextOffset": 0, "watermark": 1700000200 } }
```

`since` / `end` 接受秒级时间戳或 `YYYYMMDD`。`end=YYYYMMDD` 是**包含**上界，解析为
当天 23:59:59（否则传一个日期会得到空结果）；`since=YYYYMMDD` 取当天 0 点。

`members` 仅含**本页**出现过的发送者，已去重。

`messages[].media` 是**媒体元数据**（`{type, fileName, md5}`）：无媒体时**整个键省略**，`md5`
取不到时省略该键。它**不代表字节可取** —— `fileName` 在这里始终是元数据名（本面不执行导出，
不会回填成实际导出名）。

`messages[].mediaId` 才是「**此刻取得到字节**」的承诺：出现时，按它去
`GET /api/v1/media/{id}` **必须成功**。判据是「本会话的导出目录下确实有这份文件、且名字由
内容摘要派生」——两个条件缺一个就**整个键省略**（不是给 `null`、也不是给一个必 404 的句柄：
调用方拿到 404 只会以为服务坏了，而它无从区分）。

**为什么句柄在消息这一层、不在 `media` 里**：`media` 的键集由一致性套件钉成
`{type, fileName, md5}` 并拒绝多余键（`media_shape_in_pull`）；更根本的是两件事含义不同 ——
`fileName` 说「这条媒体叫什么」，`mediaId` 说「这份字节现在取得到」。合成一键就会把
「有名字」与「可取」混谈。回归位置：`media_id_shape_in_pull`（契约）与
`media_id_from_pull_row_skips_the_export_round`（本仓 CLI）。

拉取面本身**不接受** `media` 参数：它是拉取面，不做导出。要**还没导出过**的那些媒体拿到
字节，走 `/chatlab/messages?media=1`（每请求上限 200 项，可带 `start`/`end` 限定时间窗）。

本接口**含** `messages[].replyToMessageId`。该字段在规范的**中文**字段表里，英文表漏了它，
而两种语言的版本历史都写它属于 0.0.2 新增——判据是版本历史，因此按中文表实现。
（此前这里写的是「不含」，理由是它不在英文表里；那是**共同误读同一处文档**，不是两处独立证据。）

**有引用才输出；无引用时省略该键**，而不是给 `null`：规范把它列为可选 *string*，
`null` 会让信任类型的读者拿到一个解析不了的值。引用目标的值等于**同一会话内**某条的
`platformMessageId`（跨页匹配不作保证）。

原生面（`/api/v1/messages`）与消息面（`/chatlab/messages`）在这一键上**同规**：三个面都不再输出
`null`。下游若按「键在不在」判断引用关系，改为「键在且非空」——回归分别是
`pull_carries_reply_to_message_id_only_when_a_quote_exists` 与
`chatlab_message_face_omits_reply_to_message_id_without_a_quote`。

对照之下 `messages[].groupNickname` **是**标准字段（语义为"发送时的群昵称"），所以两个面
都输出 —— 尽管安装版文档的 Pull 示例里没有列出它。判据是标准，不是示例的字段清单。

**`accountName` 与 `groupNickname` 是两个不同的名字**：`accountName` 是联系人自己的
显示名（`remark > nickname > username`），`groupNickname` 是该成员在**本群**的群昵称
（群名片）。没有群名片、或私聊会话时 `groupNickname` 为空串 —— 联系人的备注不是群昵称，
不会填到这里。要显示"群里的称呼"用 `groupNickname` 并回落到 `accountName`。

**`messages[].type` 采用 ChatLab 0.0.2 标准枚举**
（`docs.chatlab.fun/standard/chatlab-format`），不是微信原生 `local_type`：

| 码 | 含义 | | 码 | 含义 |
| -- | ---- |-| -- | ---- |
| 0 | TEXT | | 24 | SHARE |
| 1 | IMAGE | | 25 | REPLY |
| 2 | VOICE | | 27 | CONTACT |
| 3 | VIDEO | | 80 | SYSTEM |
| 4 | FILE | | 81 | RECALL |
| 5 | EMOJI | | 99 | OTHER |
| 7 | LINK | | | |
| 8 | LOCATION | | | |

**标准中 `6` 未分配，任何情况下都不会出现。** 映射要点：

- 映射先对 `local_type` **掩码取低 32 位**（打包语义见
  [`localType` 是打包字段](#localtype-是打包字段basetype--appmsgsubtype)）。不掩码则
  所有 appmsg 行都匹配不上、全部落 `99` OTHER —— 真库里那是 28.6% 的消息；
- `local_type` 49（appmsg）按载荷细分：带 `refermsg` → `25` REPLY，子类型 6
  文件 → `4` FILE，其余 → `7` LINK。子类型优先取打包高 32 位，其次取 XML `<type>`：
  真库 62494 条里两者一致 62493 条、高位从不缺失，XML 反而错一条；
- `local_type` 10000/10002 按是否真正解出撤回载荷细分：是 → `81` RECALL，
  否（普通系统通知）→ `80` SYSTEM。仅看 `local_type` 会把非撤回的 10002 误判成撤回；
- ⚠️ 与 `/api/v1/messages` 的 `localType` 是**两套独立编码**：同一张图片在这里是
  `type: 1`，在那里是 `localType: 3`。`localType` 是平台原生码、下游已按它分支，
  两者不可互换。

`meta.type` 按标准只有 `group` / `private` 两个取值，公众号等会归入 `private`；需要更细
的会话分类请用 `/api/v1/sessions` 的 `sessionType`。

**游标语义**（与 qqflow-server 一致）：

- `since` **排他**（`create_time > since`），`end` 包含（`<= end`）。因此把上一页的
  `nextSince` 原样传回不会重复取到边界那一条；
- 页面按时间戳整秒组补齐：达到 `limit` 后仍会把当前秒的剩余消息取完，故一页可能
  略多于 `limit`。这保证 `nextSince`（本页最后一条的时间戳）一定能前进，不会因为
  同秒消息被切断而卡住；
- `nextSince` 是**本页**最后一条的时间戳，不是整个会话的最大时间戳；
- `nextOffset` 常为 `0`：`since` 排他 + 整秒组对齐后，重新过滤已经排除了本页全部行，
  下一条未读就在偏移 0。仅当时间戳无法前进的退化情形才返回非 0。**两个游标都应原样
  回传**；若把 `nextOffset` 当成"累计已读条数"再叠加，会二次跳过同一批行。
  这一点**有意不同于 WeFlow（安装版）文档里示例的 `nextOffset: 5000`**：那个值配合
  排他的 `nextSince` 回传会 double-skip；
- `watermark` 是本次拉取的时间上界（`end` 或当前时间），不是最新消息的时间戳；
  排空后（`hasMore=false`）`nextSince` 停在该上界、`nextOffset` 归 0，可作为下次
  增量拉取的起点。

按上述规则循环直到 `hasMore=false`，可完整取回会话全部消息且不重复
（真库验证：3960 条 / 80 页，无丢无重）。

#### 与标准 / 安装版的已知差异

以下是有意不实现、或按本仓数据条件取舍的部分。这些字段**要么永不输出，要么按本仓条件给空串**
（`members[].avatar`）——**没有一个是「键恒出现、值为 `null`」**（三种表示的完整清单见下一节）：

| 字段 | 标准 | 安装版 | 本项目 | 原因 |
| ---- | ---- | ------ | ------ | ---- |
| `meta.groupId` | 群 ID（**仅群聊**） | 字段清单里有 | **私聊也输出**，值等于会话 id | 省略键、给空串、给会话 id 是三种不同的契约，改动会波及已按现状实现的下游；改用「群聊时 `groupId` 等于路径 id」限定语义（契约套件的 `meta_groupId_matches_id`） |
| `meta.groupAvatar` | 可选；CN 的「头像格式说明」接受 Data URL 与网络 URL 两种，EN 字段表只写 Data URL | 字段清单里有 | **不输出** | 本仓没有解析群头像来源（contact 行的 `avatar_url` 是用户头像，不是群头像）；即使有，转 Data URL 还要额外下载与转码 |
| `members[].aliases` | 可选，`string[]` | 未列出 | **不输出** | 原生形状已有 `alias` 单值，需要时从 `/api/v1/contacts` 取 |
| `members[].avatar` | 可选；**CN 的「头像格式说明」明确接受网络 URL**，EN 字段表只写 Data URL | 真实 URL | HTTP URL 或 `""` | 直接透传联系人行的 `avatar_url`；缺值为空串。规范版本之间不一致处按 CN 表取向（与 `replyToMessageId` 同一条取向） |
| `messages[].mediaPath` | **规范里没有这个字段**（EN / CN 字段表都没有） | 字段清单里有 | **消息面与拉取面都不输出** | 给不出有意义的值：它描述的是批量导出的落盘位置，而本服务只做按需导出；媒体元数据走 `messages[].media`，取字节走 `media=1` 导出后的 `mediaId` / `fileName` 加 `GET /api/v1/media/{id}` |
| `members[].roles` | 可选，`[{id}]`（CN 表列出，EN 表未列） | 未列出 | **不输出** | **有意不做**，不是遗漏：它与 `members[].isOwner` 是同一件事的两种表达，而 `isOwner` 已经在输出；且两者受同一个限制——群主不在本页时都无从判断。要判断群主请用 `isOwner` |

**类型覆盖面与 qqflow-server 不对等。** 本项目能输出
`0/1/2/3/4/5/7/8/24/25/27/80/81/99`；qqflow-server 只能输出 `0/1/2/3/80/81/99`
（QQ 侧没有引用关系抽取，也没有名片/位置/链接的细分解析）。同一个逻辑消息在两个平台上
可能一边是 `25` REPLY、另一边落到 `99` OTHER。下游做类型分支时应把未覆盖码按 `99` 兜底，
不要假设两个上游的枚举分布一致。

#### 字段无值时怎么表示：省略键 / `null` / 空串

同一个响应里会出现三种「没有值」，它们是**三种不同的契约**：**省略键**＝这个对象不存在或本次没请求；
**`null`**＝有这个概念、此刻没有值；**空串**＝类型上恒为字符串的字段没有内容。
下游按「有没有这个键」分支时，必须区分前两者——把 `null` 当成「键不存在」会让本来就该走的分支永远走不到。

| 面 | 字段 | 表示 | 说明 |
| --- | --- | --- | --- |
| `GET /api/v1/sessions` | `sessions[].summary` | 键恒出现，无摘要时 `null` | 库里没有摘要列；「有键但为空」与「没有这个键」的区别是有意的 |
| `GET /api/v1/messages` | `messages[].appmsgSubtype`、`messages[].media`、`messages[].quote` | 键恒出现，无该物时 `null` | |
| `GET /api/v1/messages` | `messages[].replyToMessageId` | 无引用时**省略键** | 可选字符串，给 `null` 会让按「可选 string」写的读者拿到类型不符的值 |
| `GET /api/v1/messages` | `media.md5`、`media.url`、`media.localPath`、`media.mediaId`、`media.exported` | 无值时**省略键** | 「出现即可取」是承诺（判据见 `docs/architecture.md` 的「媒体句柄只对『内容摘要派生』的名字下发」） |
| `GET /api/v1/messages` | `media.exportPath`、`media.enabled`、`media.count` | **恒出现**（`enabled: false` 时也给） | `exportPath` 是本次会话的导出根目录 |
| `GET /chatlab/sessions` | `sessions[].memberCount` | 不掌握名册时**省略键** | 可选字段，断言不得写成必填 |
| `GET /chatlab/sessions`、`GET /chatlab/messages` | `page.nextCursor` | 键恒出现，已排空时 `null` | 与「整个 `page` 块不存在」（＝完整单页）是两件事 |
| `GET /chatlab/messages` | `messages[].replyToMessageId`、`messages[].media` | 无值时**省略键** | `media.md5` 取不到摘要时也省略 |
| 拉取面 | `messages[].replyToMessageId`、`messages[].media` | 无值时**省略键** | 与消息面同形；**但 `media.fileName` 在本面不回填**（本面不导出） |
| 拉取面 | `messages[].mediaId` | 不可取时**省略键**（不给 `null`） | 「出现即可取」是承诺；判据＝本会话导出目录下确有该文件且名字由内容摘要派生（与 SSE 同一条规则） |
| 拉取面 | `page` | **不出现在响应里** | 进度走 `sync` 块（`hasMore` / `nextSince` / `nextOffset` / `watermark`），四个键恒出现 |
| SSE `message.new` | `groupName`、`media` | 键恒出现，无该物时 `null` | 键集本身是契约（回归见 `sse_payload_keys_are_pinned`） |
| SSE `message.new` | `media.md5` | 键恒出现，取不到摘要时 `null` | |
| SSE `message.new` | `media.mediaId` | 只在导出根下确有该文件时出现（否则**省略键**） | 承诺是「出现即可取」 |
| SSE `message.revoke` | `groupName` | 键恒出现，私聊时 `null` | |
| 通知面 `/chatlab/push/messages` | `platformMessageId` | 键恒出现：`message.new` 为 `null`，`message.revoke` 为**被撤回那条消息**的平台号 | 本仓事件里的 `rawid` 就是平台号，所以撤回帧不必另查；拉取面用它定位被撤回的那条（回归见 `chatlab_revoke_frame_carries_platform_message_id`） |
| SNS `GET /api/v1/sns/stats` | `stats.timeRange` | 时间线为空时 `null` | 该面不属 ChatLab 承诺面（见「SNS」一节） |

**空串是另一套约定**：`/api/v1/contacts`、`/api/v1/group-members` 与 ChatLab 的 `members[]` 在
没有该值时给空串而不是 `null`（`nickname` / `remark` / `alias` / `avatarUrl` / `avatar` 等，
见各端点小节）。判据是这些字段在类型上恒为字符串——给 `null` 会破坏「恒字符串」这条形状承诺。

**为什么要区分三者**：省略键让「可选字段」的读者不必写 `if v is None` 两套分支；`null` 保留
「键在、概念在、值此刻为空」这层信息（例如 `summary`、`quote`）；空串则让字符串字段永远可以直接
参与拼接与比较。三者混用才是问题——同一字段在不同面用不同表示，是本仓**刻意避免**的事。

**跨仓的允许差异**（两仓各自都成立，但取值不同；按一端写判空逻辑会在另一端出错）：

| 项 | 本仓 | qqflow-server |
| --- | --- | --- |
| 未请求导出时的 `media.exportPath` | 键恒出现（值是导出根） | 省略键 |
| SSE `message.new` 的 `groupName` / `media` | 键恒出现，无值时 `null` | 条件键（没有就不出现） |
| 通知帧 `platformMessageId` | 键恒出现：`message.new` 为 `null`，`message.revoke` 为平台号 | 恒省略 |
| 未请求计数时的 `members[].messageCount` | 键恒出现，值为 `0` | 省略键 |

**回归位置**：原生面的省略与保留由 `tests/golden/*.json` 逐字节钉住（`accounts.json` …
`messages-native.json` 等 23 份快照）；SSE 三类帧的键集在 `tests/sse_replay.rs`；
ChatLab 两面的形状由 `tests/api_smoke.rs` 与一致性套件共同钉住。

### GET `/api/v1/contacts` — 联系人

参数：`limit`（默认 100，上限 10000）、`offset`、`keyword`。

```json
{ "success": true, "count": 100, "total": 4533, "hasMore": true, "contacts": [
  { "username": "wxid_friend_a", "displayName": "客户张三", "nickname": "...",
    "remark": "客户张三", "alias": "", "avatarUrl": "", "type": "friend" }
] }
```

`displayName` 按 `remark > nickname > username` 解析，恒为字符串；`nickname`、`remark`、
`alias`、`avatarUrl` 源自联系人行，**缺值时为空字符串 `""`（不是 `null`）**，与
`/api/v1/group-members`、ChatLab Pull 一致。

**必须翻页**：`limit` 默认 100，不传就只拿到前 100 条（实测真实账号 4533 条），
截断外的联系人在下游会退化为显示 UID。按 `offset` 递增直到 `hasMore=false`：

- `total` 为过滤后总数，与 `offset` 无关；`count` 是本页条数；
- 排序键为 `(displayName, username)`——显示名不唯一，仅按显示名排序时并列项
  在多次请求间顺序不定，offset 翻页会漏行/重复行；加 username 次键保证全序稳定；
- `offset` 超出末尾返回空页且 `hasMore=false`。

### GET `/api/v1/group-members` — 群成员

参数：`chatroomId`（或 `talker` 别名）、`includeMessageCounts=1`。

```json
{ "success": true, "chatroomId": "...@chatroom", "count": 81, "fromCache": false,
  "updatedAt": 1700000100000, "members": [
  { "wxid": "wxid_member_b", "displayName": "...", "nickname": "...", "remark": "",
    "alias": "", "groupNickname": "", "avatarUrl": "",
    "isOwner": false, "isFriend": true, "messageCount": 0 }
] }
```

成员标识键为 `wxid`（非 `username`）。

**成员集合是「名册 ∪ 发言人」**：从未发过言的成员（潜水成员）也会出现，其 `messageCount` 为 `0`；
反过来，发过言但已不在名册里的（退群、名册缺失）照旧保留。因此 `count` 可能明显大于「最近说过话
的人数」——这是有意的：只列发言人会让「群里有谁」这个问题的答案取决于谁最近发过言。

- 名册里的成员可能连联系人档案都没有：那时 `displayName` **回落 `wxid`**，而不是空串（空串会让
  下游把每一行都显示成一样的空白）。
- 排序按 `messageCount` 降序、`wxid` 升序。次键不是装饰：一大批计数为 0 的潜水成员若只按计数排，
  相对顺序会随哈希遍历变化，同一个群两次请求的顺序都可能不同，下游按它做 diff 会看到满屏假变化。
- `messageCount` 仅在 `includeMessageCounts=1` 时为真实值，否则恒为 `0`。
- `isOwner` 由 `chat_room.owner` 解析——本页成员中恰为群主者为 `true`，群主不在本页（或缺
  `chat_room`/owner 数据）时全为 `false`。
- `fromCache` 恒为 `false`：名册与消息都在内存索引里，本请求**既不读盘、也不触发同步**
  （要强制对账请调 `GET|POST /api/v1/sync`）。`updatedAt` 是**索引构建/更新完成时刻**、**毫秒**，
  客户端据此判断这份成员表有多旧。

### GET `/api/v1/media/{id}` — 按文件名取字节（**唯一的字节面**）

`{id}` 是**导出文件名**（形如 `<md5>.<ext>`），也就是消息 `media.fileName` 或 `mediaId` 的值。
调用方只需要一个名字，不必重复会话与媒体类型（按会话与类型分层的旧路径已不再提供）。

- 解析范围仅限导出根下的 `<会话>/<images|voices|videos|emojis>/<文件>` 四个类型目录，
  **不接受任何路径**；别的目录里就算躺着同名文件也不服务——那些位置不是导出管线写出来的，
  服务它们等于把「导出根」变成「任意文件根」；
- 路径段先过 `pathsafe` 的边界规则（含尾点、尾空格、冒号这类 Windows 会特殊处理的分量），
  再在 `canonicalize` 后校验仍落在导出根内——符号链接可以让「文件存在」为真而真实目标在根外；
- **同名多命中**：不同会话下可能有同名文件。候选**内容一致**时服务排序后的第一个；**内容不一致**
  时返回 404（同名内容冲突）——随便挑一个等于把「出现即可取」变成「出现即可取到某个东西」，
  而调用方无从察觉。判内容先比大小，只有大小相同才逐字节读，因此成本是「候选数 × 文件大小」
  的线性量（回归见 `media_by_id_serves_an_exported_file`，含「同大小不同内容」的负例）；
- 找不到、被拒绝、内容冲突都返回**同一个 404 统一信封**
  `{ "success": false, "code": 404, "message": "media not found" }`（冲突细节只写日志：
  回给客户端等于泄露「别的会话里有什么」）；内容按扩展名推断 MIME 输出；
- **无就绪门控**：导出文件已在磁盘上，其可读性与当前是否有账号绑定无关。因此注销后若未带
  `purge_media=1`，此前导出的文件仍可访问；要一并清除须在注销时显式请求。

> 按文件名解析、而不是维护一张「id → 路径」的登记表：登记表会与磁盘漂移——
> 导出被清理后登记仍在，于是「出现即保证可取」就变成谎话。**磁盘本身就是唯一事实源**。
> 也正因如此，只有**由内容摘要派生**的名字才作为句柄下发（见消息面的 `mediaId` 一节）：
> 它同名即同内容，遍历所有会话的结果才是确定的。

### GET `/api/v1/push/messages` — SSE 事件流（免轮询推送）

**无就绪门控**（对齐 qqflow-server）：事件总线与重放历史挂在进程级状态上，不属于
任何单个账号。因此——

- **零账号时连接返回 200**（不是 503），先收到 `ready` 基线；账号注册并建索引完成后
  事件自然流入同一条连接，客户端无需在冷启动期退避重连；
- **替换 `error` 态账号不会孤儿化订阅者**：改正密钥后重注册，已连接的客户端继续收到
  新账号的事件（旧实现每次注册新建总线，订阅者会静默失聪且不断线）；
- 业务端点（`messages`/`sessions`/…）**仍有** 503 门控——索引未建完确实无法查询，
  与此处语义不同；账号面三个端点（注册 / 明细 / 注销）同样无门控，否则未就绪时客户端连
  「为什么没就绪」都查不到，也无法清掉一个卡在 `error` 的账号；
- `wxid` 查询参数仅作语义提示，不影响订阅内容（总线为进程级，非按账号隔离）。
- **注销后不断线**：注销时**清空重放条目并推进基线代号**，同时广播一条 `sync` 基线。
  - **条目清空**：上一个账号的事件对下一个账号没有意义。原先不清空的理由是「会破坏无关订阅者
    的 `Last-Event-ID` 重放」—— 那条理由来自**多账号**场景，而多账号已明确排除，故不再成立。
  - **`id` 计数器保留**：它是总线级单调序列。若跟着归零，带着旧 `Last-Event-ID` 重连的客户端
    会把新事件当成「已经收过」而**静默丢掉** —— 跳号看得见，丢事件看不见。
  - **基线带 `generation`**：客户端据此区分「注销后新账号刚开始」（该丢弃本地状态重新拉）与
    「自己漏收了」（该补拉）。少了它，这两种情况在协议上是同一件事。

事件（`event:` 名）：

| 事件 | data 形状 |
|---|---|
| `ready` | `{"status":"ok"}`（连接建立基线） |
| `message.new` | `{"event":"message.new","sessionId":"...","sessionType":"group","rawid":"...","sourceName":"...","groupName":"...","content":"...","timestamp":1700000100,"media":{"type":"image","fileName":"...","md5":"..."}}` |
| `message.revoke` | 同上（`event` 为 `message.revoke`） |
| `sync` | `{"event":"sync","generation":N,"watermarks":[…]}`（水位基线/重基）|

- **连接建立就发一帧 `sync` 基线**：没有它，客户端在「连上」到「第一次水位变化」之间是**盲的**，
  而这中间可能很长（账号空闲、或还没注册账号）。
- **`generation` 在注销时递增**：带着旧 `Last-Event-ID` 重连的客户端据此区分「注销后新账号刚开始」
  （该丢弃本地状态重新拉）与「自己漏收了」（该补拉）—— 少了它，这两种情况在协议上是同一件事。
  它与新的通知面（`/chatlab/push/messages`）发的是**同一个**计数器。

- 帧携带 `id:` 序号；`Last-Event-ID` 头（或查询参数）可回放最近 **1000 条 / 10 分钟**
  （序号为总线级单调值，跨账号注册保持连续）
- **广播缓冲为 1024 条**：订阅端落后超过这个数就收不到逐条事件，改为收到一条 `sync` 对齐。
  注意它与上面的重放缓冲（1000 条）**不是一回事**：前者防「慢订阅者悄悄丢消息」，
  后者是断开重连时的补发窗口。两个数字都要知道，只知其一会在另一种场景下误判。
- 每 25 秒发送 `ping` 注释帧保活
- `message.new` 的 `media` 仅在消息含图片/语音/视频/表情/文件时出现，否则为 `null`。

  其中 **`mediaId` 可直接取字节**（`GET /api/v1/media/{id}`），但它**只在导出根下确实有这个文件、
  且文件名由内容摘要派生时才出现**（取不到就不给键，不是给 `null`）。承诺是「**出现即可取**」，
  不是尽力而为：通告一个取不到的 id，调用方会拿到 404 并以为是服务坏了；反过来（能取到却没
  通告）只是少一个便捷入口，仍可走 REST 的 `media=1` 导出拿 `media.url`。非摘要派生的名字
  （视频按 DB 名回落）只作元数据，不给句柄——按名取字节是跨会话解析的，平台名字可能同名异内容。

  `file` 类型**不参与导出**（源文件不是媒体流），因此**永远不会有 `mediaId`** —— 这与「取不到时
  不给」是同一条规则的两个来源。
  它是**元数据**：不含 `url` / `localPath`（字节走 REST `/api/v1/messages?media=1` 导出），
  也绝不含解密用的 `aes_key`。推空占位链接只会让客户端误以为有可取地址。
  `type: "file"` 只表示识别出了文件附件，文件类**不参与导出**，REST 侧也取不到字节。
- SSE 事件**不带 `localType` / `baseType` / `appmsgSubtype`**，需要类型分支请按
  `content` 与 `media.type`，或用 `sessionId` 回查 `/api/v1/messages`。
- 订阅端滞后（broadcast 缓冲被覆盖）时补发一帧 `sync`，携带**当前真实水位**，客户端可据此
  重新增量拉取。该帧不占用总线序号（它只针对这一个滞后订阅者，占号会导致其他客户端跳号）。
- 进程收到退出信号时，服务端主动结束所有 SSE 流（**不等 3 秒宽限期超时**），客户端会看到连接正常关闭。

## ChatLab 适配面（`/chatlab/*`）

### 接入配方（四阶段，已用真实账号走通）

把 ChatLab 的 `baseUrl` 设为 `http://127.0.0.1:5033/chatlab` 即可。下面是规范的四阶段与每一步
在本服务上的**实测结果**（真实账号，一个 3756 条的群）：

| 阶段 | 请求 | 实测 |
|---|---|---|
| ① 发现 | `GET /chatlab/sessions?limit=50` | 50 个会话 ＋ `page{hasMore, nextCursor:"50"}` |
| ② 全量 | `GET /chatlab/sessions/{id}/messages?since=0&limit=500`，用 `sync.nextSince` 续拉 | **3756 条 / 8 页**收敛 |
| ③ 增量 | `GET …/messages?since={lastPullAt}` | 返回 `since` 之后的增量（实测 256 条） |
| ④ 通知 | `GET /chatlab/push/messages`（SSE，可选） | 立刻收到 `ready` 基线帧 |

**服务是零账号启动的**（客户端驱动）：先 `POST /api/v1/accounts` 传入 `{wxid, db_path, keys}`，
再轮询 `GET /api/v1/accounts` 到 `state == "ready"`。在此之前业务端点返回 `503`（`/health` 返回
`starting`），这是**有意的** —— 索引没建完确实查不了。

**分页语义**（阶段二的关键）：

- `sync.hasMore` 为真时必须**继续拉**，并把 `sync.nextSince` 原样作为下次的 `since`。
- `since` 是**排他**下界：客户端用 `nextSince` 续拉不会重复取到边界那一秒，也不会跳过它。
- 支持 `limit` 分页时 `sync` 块**必须**给（规范：缺了它 ChatLab 不保证自动续拉）。

**去重**：ChatLab 按 `platformMessageId` 去重，所以边界上就算有重叠也不会写重。本服务保证的是
不重复**返回**（同一秒的多条消息由 `nextOffset` 在页内补齐）。

### 四条路由

这四条与 `/api/v1/*` **共用同一份实现与同一条事件总线**，差别只在形状：老面只输出原生/富数据
形状（`format=chatlab` / `chatlab=1` 开关已删除），新面**天生就是** ChatLab 形状 —— 调用方不必
知道还有另一种，也不会因为漏传一个开关而拿到另一种。

| 路由 | 行为 |
|---|---|
| `GET /chatlab/sessions` | 发现面：`keyword`/`limit`/`cursor`；带 `count` 与 `page{hasMore,nextCursor}` |
| `GET /chatlab/messages` | 消息面：`talker` 必填，`limit`/`offset`/`cursor`/`start`/`end`/`media`/`keyword`；无 `success`，消息**升序** |
| `GET /chatlab/sessions/{id}/messages` | 拉取面：`since=0`／缺省 = 全量，`since>0` = 增量。**两种请求都返回完整信封**（`chatlab`／`members`／`meta`／`sync`／`messages` 恒在）——差别只在 `messages` 是「自 `since` 起」还是「自最早起」；`members` 始终是**本页发言人**的去重集合。这条此前写作「`since>0` 只带 `messages`」，与实现不符（真库复验时实测到信封恒在），已按实现更正 |
| `GET /chatlab/push/messages` | SSE **通知面**：只发元信息，不发消息体 |

鉴权、错误信封、`Last-Event-ID` 重放与保活都与老面**完全一致**（同一条总线、同一套连接机制）。

### `GET /chatlab/sessions` — Pull 形状的发现面

响应（规范形状）：

```json
{
  "sessions": [
    { "id": "…", "name": "项目群", "platform": "wechat", "type": "group",
      "messageCount": 58000, "memberCount": 86, "lastMessageAt": 1711468800 }
  ],
  "page": { "hasMore": true, "nextCursor": "2" }
}
```

参数 `keyword`（按名称或 id 模糊匹配）、`limit`、`cursor`（原样回传上一页的 `nextCursor`）。

- **`offset` 是 `cursor` 的退路**：规范不建议在发现接口用它（列表变化时会出现重复或漏项），
  因此首选 `cursor`；本实现仍接受 `offset`（`cursor` 缺失或解析失败时退到它），与消息面同规
  ——「坏值退化为默认而不是报错」是本服务所有分页参数的统一约定。
- **`page` 总是给出**：规范说客户端在响应里**未发现** `page` 时按「单次全量结果」处理 —— 那比
  「靠条数猜有没有截断」明确。契约套件里有一条断言正是查这个。
- `cursor` 与查询条件绑定：`keyword` 变化后旧游标应视为失效（本实现里它退化为第一页）。
- **`memberCount` 是可选键**：群名册加载得到才出现（私聊、或名册缺失时**不出现这个键**）。
  「没有名册」与「名册是空的」在下游是两件事 —— 前者不该被读成 `0`。
- `type` 只有 `group` / `private` 两个取值（规范的枚举就这么大）：公众号与「其它」都归到
  `private`，它们都是**一对一的对话**，而规范没有第三个格子可放。

排序为 `lastMessageAt` 降序、`id` 升序 —— 稳定，游标翻页因此不会跳项或重复。

### `GET /chatlab/messages` — 消息面

ChatLab 形状的消息查询。它是 `/api/v1/messages` 的姊妹面：**参数解析、筛选、排序、切片与导出
任务收集共用同一份实现**（同一批参数在两个面上给出同一批消息），差别在参数与信封。

参数：`talker`（必填）、`limit`（默认 100、上限 10000）、`offset`、`cursor`（解析不了退回
`offset`，与发现面同规）、`start` / `end`、`keyword`、`media` 及类型开关
`image`/`voice`/`video`/`emoji`。**没有 `format` 参数** —— 这条面天生就是 ChatLab 形状。

```json
{
  "talker": "…@chatroom",
  "count": 20,
  "page": { "hasMore": true, "nextCursor": "20" },
  "chatlab": { "version": "0.0.2", "generator": "weflow-server", "exportedAt": 1700000000 },
  "meta": { "name": "项目群", "platform": "wechat", "type": "group", "groupId": "…@chatroom", "ownerId": "wxid_self" },
  "members": [ { "platformId": "wxid_member_b", "accountName": "李四", "groupNickname": "四哥", "avatar": "" } ],
  "messages": [
    { "sender": "wxid_member_b", "accountName": "李四", "groupNickname": "四哥",
      "timestamp": 1700000103, "type": 0, "content": "大家好", "platformMessageId": "8200000000000000000" }
  ]
}
```

四条容易读错的口径：

- **没有 `success`**：它输出的是数据信封，而 `success` 是「操作结果」的语言；两者同时出现时，
  读者无法判断 `count`/`page` 是否可信。
- **`count` 是本页条数**，不是总数 —— 总数不在这个面上表达，截断由 `page` 报告（`hasMore` 为假
  时 `nextCursor` 为 `null`）。
- **消息按时间升序**（与原生面的降序相反）。ChatLab 的读者按正序合并，倒序会让他们以为最新一条
  排在最前面。
- **`members` 仍只含本页出现过的发送者**（去重后），不是群名册 —— 名册只出现在
  `/api/v1/group-members`。「这一页有谁在说话」与「这个群有谁」是两件事。

`media=1` 时**真正执行导出**（每请求上限 200 项，规则与原生面相同：`file` 永不导出），并把**确实
写出了摘要派生本地文件**的那些条目的 `messages[].media.fileName` 回填成**实际导出文件名** ——
那时它才是可取句柄，可以喂给 `GET /api/v1/media/{id}`。没落盘的（外链、失败、超上限、平台给的名字）
照常给元数据，不给句柄。

字段语义（`accountName` / `groupNickname` / `media` 元数据 / `type` 枚举 / `platformMessageId` /
`replyToMessageId`）与拉取面完全一致，见上文的拉取面一节。

### `GET /chatlab/push/messages` — 通知面

与 `/api/v1/push/messages` 唯一的差别是**帧的形状**：

| | `/api/v1/push/messages` | `/chatlab/push/messages` |
|---|---|---|
| 载荷 | 完整消息（含 `content` 与媒体元数据）| **只带标识与时间** |
| 定位 | WeFlow 兼容面 —— 已有客户端在解析它 | 规范里的通知通道 |

帧：

```json
{ "event": "message.new", "eventId": "…", "platformMessageId": null, "sessionId": "…", "timestamp": 1700000000 }
```

`message.revoke` 帧同形，但 `platformMessageId` **带上被撤回那条的平台消息号**
（回归见 `chatlab_revoke_frame_carries_platform_message_id`）。

- **为什么不带正文**：规范对这条通道的定位是「仅通知：ChatLab 不假设 SSE 事件可靠送达」——
  客户端收到后**去拉**那一页。带正文会诱导调用方把它当数据源，而它并不保证送达；不带，语义就
  没有歧义。**契约套件里有一条断言就查这个**（帧里出现 `content` 或 `messages` 即失败）。
- `eventId` 与 `platformMessageId` 是**两个不同的号**：前者是事件通道自己的标识，后者是那条消息
  在平台上的 id（拉取时用它定位）。
- **`platformMessageId` 按事件类型给**：撤回帧带上，`message.new` 恒为 `null`。新消息要把事件里的
  编号翻成平台消息号，得在推送热路径上逐事件查一次索引，而规范里这个字段是**可选**的 —— 定位新
  消息请用拉取面返回的同名字段，「收到通知后去拉那一页」本来就是这条通道的用法。撤回则相反：它是
  终态，不带上号客户端只能靠时间戳猜是哪条没了。
- `sync` 基线帧带 `generation`（注销时递增），客户端据此区分「注销后新账号刚开始」与「自己漏收
  了」—— 前者该丢弃本地状态重新拉，后者该补拉。

## GET/POST `/api/v1/sync` — 手动增量同步

立即跑一次水位增量同步（正常情况下由文件监视自动触发，此端点用于强制对账）：

```json
{ "success": true, "newMessages": 3, "revokeMessages": 0 }
```

撤回计数键为 `revokeMessages`，不计入 `newMessages`。新消息同时会通过 SSE 推给已订阅
的客户端。

**这是一个触发器，不返回消息体** —— 消息的唯一读取面是 `/api/v1/messages` 与 ChatLab
Pull，避免同一批数据出现第二种形状。WeFlow（安装版）没有这个接口，因此它没有可对齐的
上游契约；qqflow-server 的 `/api/v1/sync` 返回同一形状。

## SNS（朋友圈，本地缓存只读）

| 端点 | 参数 | 响应键 |
|---|---|---|
| `/api/v1/sns/timeline` | `limit`（**默认 50、上限 500**）、`offset`、`username`、`start`、`end` | `{count,total,feeds:[...]}` |
| `/api/v1/sns/usernames` | — | `{success,count,usernames:[...]}` |
| `/api/v1/sns/stats` | — | `{feeds, ...}` |
| `/api/v1/sns/export` | `format=json\|html`、`username` | `{count, entries/...}` |
| `/api/v1/sns/export/stats` | — | `{data:{totalPosts: N, ...}}` |
| `/api/v1/sns/media/proxy` | `url`、`referer`、`user_agent` | 媒体字节流（CDN 鉴权墙时返回明确错误） |

feeds 条目字段（以源码 `sns.rs` 为准）：`tid/userName/content(明文XML 解析后)/likes/comments/mediaList/location/rawXml` 的等效 JSON 键
（`mediaList` 每项含 `url/thumb/md5/encIdx/rawUrl/resolvedUrl/proxyUrl/proxyThumbUrl/width/height`）。

> **导出物不含取图凭据**：`token`／`key`（微信侧的访问与解密参数）与两个同值冗余键
> `rawThumb`／`resolvedThumbUrl` 已从 JSON 导出移除。代理端点只读 `url`（＋可选
> `referer`/`user_agent`），所以移除不影响任何取图路径；导出文件常被转发、存档或贴进
> 别处，留着凭据等于每个导出包多带一份第三方能直接使用的材料。
> **迁移方式**：需要凭据的调用方改走 `/api/v1/sns/media/proxy?url=…`（代理路径已在
> `url`/`proxyUrl` 里给出），不要依赖导出物内的原始凭据。`rawUrl` 刻意保留：它是协议
> 白名单的审计线索（HTML 渲染器不读它，恶意 scheme 因此进不了 `href`，而读者要能看见
> 被拒的原始地址）。回归位置：`sns_json_export_drops_credential_media_keys`。

## 数据获取与安全模型

- **活库直读**：对微信加密库持只读长连接（`db/live.rs`，qqflow 式），`PRAGMA query_only`，
  全程不写源库、**不在磁盘生成明文镜像/快照**。
- **变更检测**：轮询以 (主文件 mtime/size, wal mtime/size) 双指纹判断变化，仅对变化库做
  水位增量查询 `(create_time, sort_seq, local_id) > watermark`。
- **密钥策略**：注册入内存、重启失效需重注册；日志、导出、响应均不打码密钥值。
- **媒体**：图片 dat(V1/V2/XOR) 解密、语音 silk 合并、视频明文直通、wxgf(HEVC)→PNG 需
  ffmpeg（环境变量 `WEFLOW_SERVER_FFMPEG` → WeFlow 内置 ffmpeg → PATH）。

## 命令行子命令（`cli` feature）

同一个二进制带一个子命令面。**默认 feature 是 `["server", "cli", "mcp"]`**，所以 `cargo install weflow-server`
装出来即有；`--no-default-features` 时整面消失（连 `clap` 与 SDK 都不进依赖树）。

**兼容口径（重要）**：

- **裸跑仍等于 `serve`**：`weflow-server --port 5033` 一个字符都不用改；
- 以旗标开头的写法、以及 `serve` 后面的旗标，**原样交给既有参数解析器**——因此 `--help`／
  `--version`／`--show-token`／配置文件加载的行为逐字不变，`serve --port 6002` 与 `--port 6002` 等价；
- `token` 子命令 = 既有的 `--show-token`；
- 第一个参数既不是旗标也不是已知子命令时，由 clap 报「未知子命令」并以 **2** 退出。老解析器
  遇到这种情况只会说「参数 bogus 缺少值」（退出 1），那是把用法错误伪装成取值错误。

退出码约定（回归位置：`tests/cli.rs`）：

| 码 | 含义 | 例子 |
| --- | --- | --- |
| `0` | 成功 | `weflow-server sessions` |
| `1` | 运行期错误：连不上、被拒、缺密钥、缺配置文件 | 未设 `WEFLOW_TOKEN` 就跑查询 |
| `2` | 用法错误：未知子命令、缺必需参数 | `weflow-server bogus`、`search` 不带 `--keyword` |

| 子命令 | 打哪个面 | 要点 |
| --- | --- | --- |
| `serve` | — | 起服务；等价裸跑 |
| `token` | 系统凭据库 | 打印 API token 并退出 |
| `sessions` | SDK `list_all_sessions` | `page_size=10000`（服务端硬上限），一次取尽 |
| `messages` | SDK `list_messages` | HTTP 形态必须给 `--talker`（服务端按会话查询）；`--since` 接受 unix 秒或 `YYYYMMDD` |
| `search` | SDK `list_messages` 带 `keyword` | `--keyword` 必填；搜不到不是错误（退出 0） |
| `contacts` | SDK `contacts` | 单页；要全量请自己带 `--limit`/`--offset` 翻页 |
| `accounts` | SDK `accounts` | **只有 HTTP 形态**（没有 `--embedded`）：这一面问的是「服务端此刻实际绑定了什么」，进程内索引给的是另一个答案 |
| `sync` | SDK `sync_now` | **写动作**：让服务端立刻跑一次增量同步；同样没有 `--embedded` |
| `export` | SDK `list_all_sessions` ＋ `pull_page` ＋ `chatlab_messages` | **只走 HTTP**（不提供 `--embedded`）：批量导出到 ChatLab Format 文件；`--with-media` 时用同一页的时间窗调 `chatlab_messages(media=1)` 触发导出并下载字节，见下一节 |

环境变量：`WEFLOW_BASE_URL`（默认 `http://127.0.0.1:5033`）、`WEFLOW_TOKEN`（API token）。
**token 一律不经命令行传递**——命令行会落进 shell history 与进程列表，而这个值能读出整份聊天记录。

`--embedded`（进程内直读本地库，不起也不打 HTTP）**只开放给只读查询类**（`sessions`／`messages`／
`search`／`contacts`）。它读与 `examples/embed.rs` **同一个**配置文件（`{"wxid":…, "db_path":…,
"keys":{…}}`），路径由 `WEFLOW_EMBED_CONFIG` 给出；密钥仍只从环境变量／磁盘配置来。

输出：默认是人类可读的紧凑行（每类只挑最常看的几列），`--json` 给机器可读形状。

## 批量导出（`export`）

`weflow-server export --out <目录> [--format jsonl|json] [--session <id> …] [--since <t>] [--resume] [--with-media]`

**只走 HTTP**：这个面不提供 `--embedded`。服务端已经把数据库密钥握在内存里，CLI 只做编排与
落盘；否则一个可能跑几分钟的任务会长时间持有密钥，还得把密钥带上命令行（它会进 shell history
与进程列表）。

产物布局：

- **每个会话一个文件**（`<slug>.jsonl` 或 `<slug>.json`）。`<slug>` 由会话显示名经
  `pathsafe::slugify` 得到；**折叠后不含任何 ASCII 字母数字时**（纯中文群名会被折成一串下划线，
  既不可读也极易互撞）**回落到会话 id 的 slug**；同名会话按出现顺序追加 `-2`／`-3`。文件名是
  确定性函数——这是 `--resume` 成立的前提。两个补充口径：

  - **去重按大小写折叠**（Windows 卷默认大小写不敏感，`Team` 与 `team` 是同一个文件），而交付名
    保留原大小写；
  - **编排文件名 `index` 永远留给清单**：显示名恰好是 `index` 的会话拿到 `index-2`，否则 json 形态下
    会话信封会被清单原地覆盖。

  回归位置：`export::tests::slug_collision_gets_deterministic_suffix`。
- **JSONL 形态**：第一行是 `_type: header`（含 `chatlab` 与 `meta`），其后是 `_type: message` 行。
  规范建议按时间升序，因此**页内**排序（跨页排序会把内存恒定这条承诺打破）。**不写 member 行**：
  流式写不出「先集齐成员再写消息」的顺序，而规范说成员行可选、缺省时由导入器从消息收集；
  本服务的消息行自带 `accountName` 与 `groupNickname`，信息不丢。
- **JSON 形态**：一个会话一个完整信封（`chatlab`／`meta`／`members`／`messages`，不带行型标记）。
  整会话留在内存，因此大语料请用 jsonl。
- **`index.json`**：本服务自造的编排清单（会话 → 文件 → 条数）。**它不属于 ChatLab 规范，导入
  请用单个 `<slug>.jsonl`／`.json`；整个目录不可导入。**
- **`--with-media`**：把本会话用到的媒体字节下载到 `<目录>/media/`，并把导出物里的
  `media.fileName` **限定为确实落盘的那些句柄**。实现是**单遍**的：每取到一页 Pull 行，就用这一页的时间窗
  （`(since, nextSince]` 正好对应消息面认的 `start`/`end`，两端都是闭区间的秒级戳；同秒的行必然落在同一页里，
  所以窗口两端不漏行）**按行分两条路**：

  · **快路径**——拉取面已经给出 `messages[].mediaId` 的那些行，**直接按那个句柄取字节，一发导出请求都不发**。
  · **慢路径**——有 `media` 却还没有句柄的那些行，把**这些行的时间窗**交给消息面（`/chatlab/messages?media=1`，
    该面**每请求最多导出 200 项**，超出部分靠翻页续传）触发按需导出，再从回填的可取句柄取字节。

  然后才写这一页的行；顺序不能反 —— 服务端只有在真的写出了本地副本之后，才把 `fileName` 回填成可取句柄。
  **为什么不能整批撤掉慢路径**：撤掉就等于「媒体句柄只能靠全历史那趟预遍历拿」，而那一趟不认 `--since`
  （本仓刚修掉的正是这个）。快路径让「已经导出过」的会话（`--resume`、重复导出）近乎零成本，慢路径只在
  真需要导出时才付出成本；两条路都不再有全历史遍历。

  **`--since` 因此同时下推到导出面**：过去它是两趟独立的全历史遍历，`--since` 只管住写出来的行、媒体照样把
  整个会话导出并重下一遍；而两趟之间若有并发同步推进水位，第一趟没覆盖到的消息会「有行、无句柄」地静默缺件。
  磁盘上已有的摘要文件**不重下**（内容摘要名同名即同内容，按存在性复用是安全的）——这也是 `--resume` 能
  「只补下缺件」的依据。

  回归位置：`cli_e2e::media_window_follows_the_pull_page`（慢路径的窗口跟着 Pull 页走：每个导出请求都带
  `start`/`end`、请求条数等于 Pull 页数、两页窗口互不重叠，且 250 条句柄与 `media/` 文件集合大小相等）、
  `cli_e2e::media_id_from_pull_row_skips_the_export_round`（快路径：三行里只有没句柄的那一行需要导出 ⇒
  恰好一发导出请求，且窗口只覆盖那一行的时间戳）、`cli_e2e::with_media_reuses_bytes_already_on_disk`
  （第二次跑不得重下覆盖）。注意**两个面给的名字不必相同**：消息面回填的是
  导出后的内容摘要名，拉取面携带的仍是索引里的原始名，所以句柄**按消息 id 对账**（不是按名字比对），
  导出物里写的是实际落盘的那个名字。外链媒体与未能导出的媒体**不会**留下句柄：宁可少一个 `media`
  字段，也不给一个指向不存在文件的句柄。单个媒体取不到只跳过，不升级成会话级失败。
- **响应里的媒体名要过本地路径校验**：服务端回传的 `media.fileName` 要拿去拼 `<目录>/media/` 下的路径，
  `../`、盘符、ADS、Win32 设备名这类值配合 `join` 能写到导出目录之外（URL 段编码只防 HTTP 层）。
  因此落盘前先过 `pathsafe::safe_segment`，非法名按 404 同级跳过**并计数可见**（汇总行给出个数）。
  同名（含仅大小写不同，折叠口径与会话名一致）的媒体只下载一份字节，引用它的每条消息都映射到**实际落盘的
  那个名字**。
  回归位置：`export::tests::with_media_keeps_only_handles_whose_bytes_are_on_disk`、
  `export::tests::message_line_omits_media_without_a_handle`、
  `cli_e2e::with_media_rejects_unsafe_response_file_names` 与
  `cli_e2e::with_media_maps_exported_names_back_to_message_ids`。

三条硬约束：

1. **导出物里不得出现访问令牌**。服务端的媒体是**根相对路径**（`/api/v1/media/<file>`，**不含
   令牌** —— 令牌只走请求头或 `?access_token=`，响应体从不嵌它）。导出仍然**不写任何 URL**
   （相对路径换台机器就失效），媒体只以 `{type, fileName}` 表达；
   并且每一行写盘前会拿调用方给的令牌做一次子串检查，**命中即整轮中止**（不是跳过该会话）
   并删掉半成品。回归位置：`export::tests::secret_in_output_aborts_and_removes_partial_file`。
2. **会话级失败不静默**：取数失败的会话被记入 `skipped`，而只要 `skipped` 非空，CLI 就以退出码 1
   结束。静默少导几个会话是这类工具最坏的失败方式。**`--resume` 的命中不算失败**：记入 `reused`
   （幂等完成），同参数续跑以退出码 0 收场。
3. **最终名是唯一的完成标记**：会话先写 `<slug>.<格式>.part`，整个会话成功收尾后才 `rename` 成最终名。
   因此 `.part` 残留意味着「没写完」，续跑一律重写；失败的那一轮只删 `.part`，**不会**碰上一轮已经
   交付的完整产物。清单 `index.json` 同样先 `index.json.part` 再改名 —— 它是续跑唯一的「完成记录」来源，
   半路被杀的截断清单会让下一轮误判「没有上一轮」。
4. **`--resume` 复用要同时满足四条**（缺一条就重写，并在日志点名原因）：上一轮清单记着该会话、
   且它登记的**就是本轮这个文件名**（换格式 `jsonl`／`json` 后盘上的另一扩展名文件是孤儿，
   不算本轮产物）、没有 `.part` 残留、本轮带 `--with-media` 时上一轮也带过媒体。
   「有个同名文件」单独不构成完成记录：清单被删或被截断时，那会让从没导出过的会话被静默判成已完成，
   而它在新清单里没有条目，既看不见也不能自愈。
5. **媒体承诺按轮成立**：`--with-media --resume` 复用上一轮产物时，CLI 会逐个核对复用会话里的
   `fileName` 在 `media/` 下确有字节；有悬空就点名该会话并以退出码 1 结束（媒体目录被清理或
   搬走过的交付包不该静默通过）。处理办法：对点名的会话去掉 `--resume` 重跑，或恢复 `media/`。
6. **单个会话的起手／收尾失败只跳过该会话**（记入 `skipped`），不中止整轮 —— 整轮中止会让本轮已写出
   的会话留在盘上却不进清单，比留一个 `.part` 更难恢复。令牌泄漏仍是整轮中止。

   回归位置：`export::tests::resume_rewrites_an_incomplete_artifact`、
   `export::tests::failed_rerun_keeps_the_previous_complete_artifact`、
   `cli_e2e::export_writes_a_file_against_a_live_service`（含续跑第二次退 0）。

内存：JSONL 逐页写盘、写完即丢，**峰值常驻集与消息条数无关**；`--with-media` 的句柄映射现在**只活在
这一页**（O(本页不同媒体数)，不随会话长度增长），因此媒体侧不再破坏这条承诺。同秒组的消息会被服务端
扩页带出（Pull 段的「同秒扩页」），那一项仍在承诺之外。大语料的实测口径与造库工具（隐藏的
`--rows` 参数，仅 `testing` feature 下编译进二进制）见 `docs/architecture.md` 的「测试与夹具」。
## 启动示例

```powershell
weflow-server.exe --port 5033 --watch-fallback-ms 5000 --log info
weflow-server.exe serve --port 5033      # 与上一行等价
weflow-server.exe sessions --json        # 子命令面
```

参数：`--show-token`、`--port`、`--host`、`--log`、`--watch-debounce-ms`、
`--watch-fallback-ms`、`--media-export-dir`、`--base-url`（全部仅命令行，无配置文件）。
数据目录不可配置：Windows `%LOCALAPPDATA%\weflow-server`；媒体导出默认落在其下的
`api-media`，仅 `--media-export-dir` 可改。

## 类型化客户端（SDK）

本仓库提供两种语言的同构客户端：`clients/rust`（Rust）与 `clients/python`（Python，`weflow-sdk`）。
两者分层一致：生成类型 + 手写行为层，行为方法的语义（503 是等待、游标原样回传、`Last-Event-ID` 重连、404 后先导出再取）在两侧保持相同。

本仓库自带类型化 Rust 客户端：`clients/rust`（crate 名 `weflow-client`，workspace 成员）。

- **类型与操作客户端是生成的**：出处是 `/openapi.json` 的描述（生成工具 `clients/regen`，
  `cargo run -p weflow-regen` 重新生成；生成物入库，CI 断言「重生成无 diff」）。**不要手改**
  `clients/rust/src/generated/` 下的任何文件。
- **行为层是手写的**（`clients/rust/src/client.rs`）：下面这张表就是**公共面**——
  每个方法都有具名测试（Rust 在 `clients/rust/tests/behavior.rs`，Python 在
  `clients/python/tests/test_behavior.py`），没有测试的能力不进这张表。

  | 方法 | 打哪个面 | 语义要点 |
  | --- | --- | --- |
  | `health()` | `GET /health` | **免鉴权**，只给标量阶段与版本；客户端不带凭据（有测试钉住） |
  | `accounts()` | `GET /api/v1/accounts` | 账号明细；`error` 与 `messageCount` 只在这里 |
  | `register(body)` | `POST /api/v1/accounts` | **非阻塞**，返回原始 `state`/`status`（`RegisterOutcome`）；拒绝态是**值**不是错误 |
  | `ensure_ready(account, body, timeout)` | 注册 ＋ 轮询 | `register` ＋ `wait_ready` 的组合；**200 的拒绝态立即失败**，不等超时 |
  | `wait_ready(account, timeout)` | `GET /api/v1/accounts` | **只等待、不注册**（wait-only）；中间态是等待不是错误 |
  | `pull_page(talker, since, offset, limit)` | Pull 面 | **一页语义**：`since` 排他、`offset` 是同一时间组内的游标；两游标必须原样回传。`limit` 是单页上限（服务端封顶 5000），`None` 即服务端默认 |
  | `drain_session(talker, since, on_page)` | Pull 面 | **取尽语义**（内部逐页调到 `hasMore=false`，每页回调）；游标（`nextSince`/`nextOffset`）原样回传，按 (时间组, offset) 翻页 |
  | `list_messages(query)` | `GET /api/v1/messages` | **原生面，一页语义**：`offset` 进、`hasMore` 出；带 `rawContent`/`isSend`/`localType`，且只有它能 `media=1` 导出。时间界收 `YYYYMMDD` 或 unix 秒，客户端先校验 |
  | `chatlab_messages(talker, …)` | `GET /chatlab/messages` | **ChatLab 形状面，一页语义**：升序、ChatLab type 码、`media` 在消息上、`count`/`page` 翻页、**没有 `success`**；查询参数与 `list_messages` 同一套（含 `keyword`） |
  | `contacts(query)` | `GET /api/v1/contacts` | **一页语义**；ChatLab 面完全不覆盖联系人 |
  | `group_members(chatroom, include_message_counts)` | `GET /api/v1/group-members` | 成员集合＝**名册 ∪ 发言人**（潜水成员出现、计数 0）；计数开关**关闭时不发参数**而非发 `0`；**空群号本地拒绝**（与 Python 侧同措辞：空名册会被读成「这个群没有成员」） |
  | `list_all_sessions(page_size, keyword)` | `GET /api/v1/sessions` | **取尽语义**（内部翻页到空页），跨页重复折叠并告警；`keyword` 是**服务端过滤**（翻的是过滤后的列表，不是取回来再剪）；`page_size` 上限 10000 |
  | `media_bytes(message, talker)` | `GET /api/v1/media/{id}` | 从 ChatLab 消息取；`talker` 是**会话 id**（不是 `accountName` 显示名）；404 后按「先 `media=1` 导出再取」自动重试一次 |
  | `media_bytes_by_id(id)` | `GET /api/v1/media/{id}` | 按**单段句柄**取（原生面的 `mediaId`，或 `media.url` 末段）；不触发导出 |
  | `watch()` | SSE `/api/v1/push/messages` | `Last-Event-ID` 重连（游标**只在帧被消费时推进**：只有 `id:` 没有 `data:` 的悬空帧不推进，否则那条事件会从重放窗口消失）、心跳注释帧过滤、`generation` 变化上报给调用方决定是否回退 Pull 补拉 |
  | `sync_now()` | `POST /api/v1/sync` | **写动作**（推进水位、可能导出媒体）：刻意不进入任何轮询路径，只有显式调用才触发（有测试钉住读路径零命中） |

  **两个容易读错的地方**：① `list_all_sessions` 与 `drain_session` 是取尽，而 `pull_page`/
  `list_messages` 只取一页；`contacts` 是一页语义但响应带 `total`/`hasMore`（CLI 的 `contacts` 子命令
  也透传分页参数并回给这两个字段）；② 时间界收 `YYYYMMDD` **或** unix 秒，`end` 作为上界时裸日期覆盖**整天**。
- **错误按性质分派变体**：HTTP 非 2xx → `Status`；连接/超时/重置/**URL 解析不出来** → `Transport`；
  响应是合法 JSON 但不合承诺形状 → `Shape`。解码是**先取字节再单独解析**的：`resp.json::<T>()` 会把解码失败也包成
  传输错误，于是「服务端答错了」与「网络断了」混成一类 —— 而调用方正是按变体分流的（重试传输故障
  合理，重试形状错误不合理）。`Status.url` **恒等于请求 URL**，不掺描述文字（按 url 归因的调用方会静默错分类），
  拒绝态的 `state` 另走 `detail` 字段。
- **超时默认（Rust 与 Python 一致；TS 仅示例，不在承诺面内）**：三个公开常量，且**每一个都按单次操作计时、
  有进展即复位——没有一个是总时限**（reqwest 的 `timeout()` 是总时限，所以这里刻意用 `read_timeout`，与 httpx 同语义）：
  连接 `CONNECT_TIMEOUT` **5s**（服务端不在时要立刻失败）；普通 JSON 请求 `READ_TIMEOUT` **30s**；成本由数据量或
  后台工作决定的一族——`group_members(..., include_message_counts=True)`（整名册计数＝全会话扫描）、
  `media_bytes`／`media_bytes_by_id`（体积由发送方决定）、`sync_now()`（索引＋可能导出媒体）、`watch()`（长连接）——
  用 `STALL_TIMEOUT` **90s**。判据两面：把 30s 套到这些面上会把「这个群很大」变成客户端错误（慢不等于坏）；
  而**完全不设上界**又会让黑洞连接（对端被 kill 且没有 FIN、NAT 映射过期、笔记本唤醒后）永不被发现——对 SSE 尤其
  致命：重连分支根本不可达，表现为一条既不产出也不报错的流。90s ≈ 服务端 25s keep-alive 间隔的四倍，丢一次心跳
  加一次调度抖动仍算健康，黑洞则会被抓到。自定义走 Python 的 `timeout=`／`stall_timeout=`，或 Rust 的
  `Client::with_timeouts(base_url, token, connect, read, stall)`。回归位置：
  `each_request_family_uses_its_own_time_budget`（Rust：驱动一个 600ms 才回响应头的真实服务端，两个上界各自生效，
  退回任一路由即变红）、`test_published_timeout_budgets_travel_per_request` 与
  `test_watch_stream_is_not_bounded_by_the_json_read_timeout`（Python：断言 transport 实际收到的 per-request 值，
  不是只读常量）、`published_timeouts_match_the_documented_budgets`（三个常量的数值与大小关系）。
- 鉴权走 `Authorization: Bearer`；客户端从不把 token 放进 URL（`/health` 是唯一免鉴权端点）。
- 本轮**不发布** crates.io：本地 `cargo build -p weflow-client` 即可使用。

- **Python 侧**：`clients/python`（包 `weflow-sdk`）。模型生成走 `scripts/regen.py`
  （spec 经 Rust 生成工具的 `--dump-spec` 取得，绕开 golden 的占位掩码）；行为层是
  `httpx.AsyncClient` 异步实现，`from weflow_sdk import Client` 即用。测试对进程内
  ASGI mock 跑：`clients/python/.venv/Scripts/python -m pytest tests/`。
