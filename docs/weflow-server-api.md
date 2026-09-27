# weflow-server HTTP API（v2 参考）

> 本文档以源码（`src/server/`）为准，描述当前实现的全部接口。
> 与 WeFlow 安装版 `HTTP-API.md` 契约对齐的部分在原文处标注。

- 服务默认监听 `127.0.0.1:5033`（`--port` 可改）
- 数据目录 = `%LOCALAPPDATA%\weflow-server`；访问 token 存于**系统凭据库**（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service；无凭据库平台为会话级 token，随启动日志打印）
- 所有 `/api/v1/*` 端点需要鉴权；`/health` 与 `/api/v1/health` 不需要

## 鉴权

五种传输等价（源码 `server/auth.rs::authorized`），按下表顺序检查：

| 方式 | 示例 |
|---|---|
| HTTP 头 | `Authorization: Bearer <token>` |
| HTTP 头 | `X-Api-Key: <token>` |
| 查询参数 | `?access_token=<token>` |
| 查询参数 | `?token=<token>` |
| POST JSON body | `{"access_token": "<token>"}`（或 `"token"`） |

> query 与 body 合并为同一参数表（body 优先），故后三种在实现上是同一处检查。

> 所有通道的 token 比对均为**常时比较**（constant-time，无提前返回，防时序侧信道；与 qqflow-server 同款）。

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
| `cursor` | 翻页游标：把上一次响应的 `page.nextCursor` 原样传回。解析不了就退回 `offset`。两者做的是同一件事（本面按偏移翻页），`cursor` 只是免去调用方自己算下一个偏移 |
| `start` / `end` | 时间边界：Unix 秒，或 `YYYYMMDD`。**`start` 取当天 0 点，`end` 取当天 23:59:59**（上界是包含的，日期形态必须覆盖当天）。无法解析时该条件被忽略，不报 400 |
| `keyword` | 关键词过滤（小写匹配；sessions/contacts/messages 通用） |
| `format=chatlab` / `chatlab=1` | 输出 ChatLab 风格形状 |
| 导出开关 | `media=1`（或 `meiti=1`）开启媒体导出；再按类型 `image=1`/`voice=1`/`video=1`/`emoji=1`（兼容拼音别名 `tupian=1`、`vioce=1`） |

## 端点

### GET `/openapi.json` — 接口描述（**免鉴权**）

返回本服务的 OpenAPI 3 描述，由 `server/dto.rs` 里的响应类型经 `#[derive(ToSchema)]` 生成。

免鉴权是刻意的：它描述的是**形状**，不含账号、路径或密钥，而且正是给尚未拿到 token 的接入方
看的 —— 拿到它就能生成客户端，再凭 token 调真正的接口。

两点使用提示：

- **多形状端点用 `oneOf`**。`/api/v1/sessions`、`/api/v1/messages`、`/api/v1/accounts`（POST）
  的响应形状由参数或状态决定，描述里列的是若干可能形状的并集 —— 生成客户端时应按 `oneOf`
  处理，而不是当成「所有字段都可能存在」。
- **描述随 DTO 变**。改 DTO 就会改它，不需要手工同步；`tests/openapi.rs` 保证描述自身自洽
  （每个 `$ref` 都能解析、operationId 唯一、多形状确实用 `oneOf`），golden 快照则保证它
  的变更有人看过。

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
`unregistered`）。别名 `POST /api/v1/accounts/{wxid}/deregister`，语义完全相同——给不便发
DELETE 的客户端用。

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

### GET/POST `/api/v1/messages` — 消息查询 + 媒体导出

参数：`talker`（会话标识，必填，为空返回空结果）、`limit`、`offset`、`start`、`end`、
`keyword`、`format/chatlab`、`media` 及类型开关（见通用参数）。

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
  "replyToMessageId": null,
  "quote": { "platformMessageId": "...", "sender": "...", "accountName": "...", "content": "...", "type": 1 } | null,
  "media": { "type": "image|voice|video|emoji|file", "fileName": "...", "md5": "...", "url": "", "localPath": "" } | null
}
```

- `serverId` 为**字符串**：i64 超出 JS 安全整数范围，直接出数字会在浏览器端丢精度。
- `isSend` 为数字 `0`（对方/系统）或 `1`（自己），不是布尔。
- `media` 只要消息可解析出媒体就存在（与 WeFlow 形状一致），与 `media=1` 无关；未导出时
  `url` / `localPath` 为**空字符串**而非缺键，客户端可稳定取字段。

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
"media": { "type": "image", "fileName": "...", "md5": "...",
           "url": "/api/v1/media/<talker>/images/<file>",
           "localPath": "C:\\...\\api-media\\...", "exported": true }
```

- **`exported: true` 是导出成功的唯一判据**；仅有 `media` 对象不代表字节可取（缺 md5 的
  非语音消息会被跳过），顶层 `media.count` 等于本页 `exported` 为真的条数。
- 单次请求最多导出 200 项以限制延迟，超出部分保持未导出，可缩小 `limit` 分批取。
- `url` 是**根相对路径**（形如 `/api/v1/media/<talker>/images/<file>`），且**不含 token**：
  token 是只走请求头的凭据，拼进响应体会被复制到客户端日志与任何中间缓存；相对路径也
  免掉了把服务基址烤进响应——反代或换端口之后下发的地址仍然有效。调用方按自己的基址
  拼接，取字节时带上鉴权头。
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

`format=chatlab` / `chatlab=1` 时改为输出 ChatLab 信封（消息按时间**正序**）：
`chatlab` / `meta`（含 `ownerId`）/ `members` / `messages`，外层保留
`success` / `talker` / `count` / `hasMore`。字段语义同 ChatLab Pull（见下节），并按
WeFlow（安装版）契约额外带 `messages[].replyToMessageId`。

安装版契约里的 `messages[].mediaPath` **本项目不输出**（两个 ChatLab 面都不输出）：
媒体导出由 `media=1` 开关控制且只在原生形状回填，这里给不出有意义的值，恒空的键比
没有键更容易误导。媒体字节走本接口的 `media` 对象 + `/api/v1/media/{id}`。

两处差异要注意：`accountName`（联系人自己的显示名）与 `groupNickname`（本群群昵称）是
**两个不同字段**，与原生形状的 `senderName` 语义不同；`messages[].type` 是 ChatLab
标准枚举，与原生 `localType` 是**两套独立编码**。枚举全表见下节。

### GET/POST `/api/v1/sessions` — 会话列表

参数：`limit`、`offset`、`cursor`、`keyword`、`format/chatlab`。

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
`format=chatlab` / `chatlab=1` 时改为输出

```json
{ "sessions": [ { "id", "name", "platform", "type", "messageCount", "lastMessageAt" } ],
  "count": 1,
  "page": { "hasMore": true, "nextCursor": "1" } }
```

其中 `type` 为 `group` / `private` 字符串，`count` 是**本页条数**（与原生面同义）。

**`page` 块是必需的。** ChatLab 把「没有 `page` 块」的响应读作「这就是完整一页」，
所以不带 `page` 的截断会被下游当成全量——默认 `limit` 是 100，普通账号就能撞上，
表现是「第 101 个会话凭空消失」且不报错。`hasMore` 为假时 `nextCursor` 为 `null`；
把 `nextCursor` 原样回传即可继续翻页。

### GET `/api/v1/sessions/{id}/messages` — ChatLab 拉取（消息游标）

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
      "timestamp": 1700000103, "type": 0, "content": "大家好", "platformMessageId": "8200000000000000000" }
  ],
  "sync": { "hasMore": true, "nextSince": 1700000103, "nextOffset": 0, "watermark": 1700000200 } }
```

`since` / `end` 接受秒级时间戳或 `YYYYMMDD`。`end=YYYYMMDD` 是**包含**上界，解析为
当天 23:59:59（否则传一个日期会得到空结果）；`since=YYYYMMDD` 取当天 0 点。

`members` 仅含**本页**出现过的发送者，已去重。

本接口**含** `messages[].replyToMessageId`。该字段在规范的**中文**字段表里，英文表漏了它，
而两种语言的版本历史都写它属于 0.0.2 新增——判据是版本历史，因此按中文表实现。
（此前这里写的是「不含」，理由是它不在英文表里；那是**共同误读同一处文档**，不是两处独立证据。）

**有引用才输出；无引用时省略该键**，而不是给 `null`：规范把它列为可选 *string*，
`null` 会让信任类型的读者拿到一个解析不了的值。引用目标的值等于**同一会话内**某条的
`platformMessageId`（跨页匹配不作保证）。

`/api/v1/messages` 的既有形状**不受本次改动影响**：它的原生形状与 `format=chatlab` 形状
仍按原样输出 `null`（下游已依赖该形状，改动属破坏性）。因此同一个字段在两个面上
「无引用」的表达方式不同——这是**有意的**，不要顺手「统一」，那会破坏 `/api/v1/messages` 的调用方。

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

以下是有意不实现或受数据限制的部分，下游不要依赖这些字段存在：

| 字段 | 标准 | 安装版 | 本项目 | 原因 |
| ---- | ---- | ------ | ------ | ---- |
| `meta.groupAvatar` | 可选，要求 Data URL | 字段清单里有 | **不输出** | 库里只有 HTTP 头像 URL，转 Data URL 需要额外下载与转码；给 HTTP URL 会违反标准的 Data URL 约定 |
| `members[].aliases` | 可选，`string[]` | 未列出 | **不输出** | 原生形状已有 `alias` 单值，需要时从 `/api/v1/contacts` 取 |
| `members[].avatar` | 可选，要求 Data URL | 真实 URL | HTTP URL 或 `""` | 直接透传联系人行的 `avatar_url`，**不是** Data URL；缺值为空串 |
| `messages[].mediaPath` | 不在标准 | 字段清单里有 | **两个面都不输出** | 给不出有意义的值（导出受 `media=1` 控制且只回填原生形状）；媒体字节请走 `/api/v1/messages` 的 `media` 对象（`exported: true` 才是可取判据） |

**类型覆盖面与 qqflow-server 不对等。** 本项目能输出
`0/1/2/3/4/5/7/8/24/25/27/80/81/99`；qqflow-server 只能输出 `0/1/2/3/80/81/99`
（QQ 侧没有引用关系抽取，也没有名片/位置/链接的细分解析）。同一个逻辑消息在两个平台上
可能一边是 `25` REPLY、另一边落到 `99` OTHER。下游做类型分支时应把未覆盖码按 `99` 兜底，
不要假设两个上游的枚举分布一致。

### GET/POST `/api/v1/contacts` — 联系人

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

### GET/POST `/api/v1/group-members` — 群成员

参数：`chatroomId`（或 `talker` 别名）、`includeMessageCounts=1`、`forceRefresh=1`
（先跑一次增量同步保证成员集合最新）。

```json
{ "success": true, "chatroomId": "...@chatroom", "count": 81, "refreshed": true, "members": [
  { "wxid": "wxid_member_b", "displayName": "...", "nickname": "...", "remark": "",
    "alias": "", "groupNickname": "", "avatarUrl": "",
    "isOwner": false, "isFriend": true, "messageCount": 0 }
] }
```

成员标识键为 `wxid`（非 `username`）。`messageCount` 仅在 `includeMessageCounts=1`
时为真实值，否则恒为 `0`；`isOwner` 当前始终 `false`（群主信息不在已解析的表中）。

### GET/POST `/api/v1/media/{id}` — 按文件名直服

`{id}` 是**导出文件名**（形如 `<md5>.<ext>`），也就是消息 `media.fileName` 的值。
三段式路由要求调用方重复会话与媒体类型；这条只需要一个名字。

- 解析范围仅限导出目录下的 `<会话>/<类型>/` 四类子目录，**不接受任何路径**；
- 防穿越与三段式共用同一条规则（`pathsafe`），并在 `canonicalize` 后校验仍落在导出目录内；
- 找不到文件 → 404 统一信封；内容按扩展名推断 MIME；
- 与三段式一样**无就绪门控**。

> 按文件名解析、而不是维护一张「id → 路径」的登记表：登记表会与磁盘漂移——
> 导出被清理后登记仍在，于是「出现即保证可取」就变成谎话。文件名由内容摘要派生、
> 全局唯一，**磁盘本身就是唯一事实源**。

### GET/POST `/api/v1/media/{talker}/{media_type}/{file}` — 导出媒体直服

- `media_type` ∈ `images|voices|videos|emojis`
- 双重防穿越：路径段拒绝 `..`/`./`/`\`，且 `canonicalize` 后必须落在导出目录内
- 404 走统一错误信封 `{ "success": false, "code": 404, "message": "media not found" }`；
  内容按扩展名推断 MIME 输出
- **无就绪门控**（与 qqflow-server 不同，后者此端点要求就绪）：导出文件已在磁盘上，
  其可读性与当前是否有账号绑定无关。因此注销后若未带 `purge_media=1`，此前导出的
  文件仍可访问；要一并清除须在注销时显式请求。

### GET/POST `/api/v1/push/messages` — SSE 事件流（免轮询推送）

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
| `sync` | `{"event":"sync","watermarks":[...]}`（水位基线/重基） |

- 帧携带 `id:` 序号；`Last-Event-ID` 头（或查询参数）可回放最近 **1000 条 / 10 分钟**
  （序号为总线级单调值，跨账号注册保持连续）
- **广播缓冲为 1024 条**：订阅端落后超过这个数就收不到逐条事件，改为收到一条 `sync` 对齐。
  注意它与上面的重放缓冲（1000 条）**不是一回事**：前者防「慢订阅者悄悄丢消息」，
  后者是断开重连时的补发窗口。两个数字都要知道，只知其一会在另一种场景下误判。
- 每 25 秒发送 `ping` 注释帧保活
- `message.new` 的 `media` 仅在消息含图片/语音/视频/表情/文件时出现，否则为 `null`。
  它是**元数据**：不含 `url` / `localPath`（字节走 REST `/api/v1/messages?media=1` 导出），
  也绝不含解密用的 `aes_key`。推空占位链接只会让客户端误以为有可取地址。
  `type: "file"` 只表示识别出了文件附件，文件类**不参与导出**，REST 侧也取不到字节。
- SSE 事件**不带 `localType` / `baseType` / `appmsgSubtype`**，需要类型分支请按
  `content` 与 `media.type`，或用 `sessionId` 回查 `/api/v1/messages`。
- 订阅端滞后（broadcast 缓冲被覆盖）时补发一帧 `sync`，携带**当前真实水位**，客户端可据此
  重新增量拉取。该帧不占用总线序号（它只针对这一个滞后订阅者，占号会导致其他客户端跳号）。
- 进程收到退出信号时，服务端主动结束所有 SSE 流（**不等 3 秒宽限期超时**），客户端会看到连接正常关闭。

## ChatLab 适配面（`/chatlab/*`）

**接入配方**：把 ChatLab 的 `baseUrl` 指向 `http://127.0.0.1:5033/chatlab` 即可。这三条路由与
`/api/v1/*` **共用同一份实现与同一条事件总线**，差别只在默认语义 —— 老面靠 `format=chatlab`
参数切换，新面**天生就是** ChatLab 形状，调用方不必知道还有另一种。**老面一行未改。**

| 路由 | 行为 |
|---|---|
| `GET /chatlab/sessions` | 始终返回 ChatLab 会话形状；带 `count` 与 `page{hasMore,nextCursor}` |
| `GET /chatlab/sessions/{id}/messages` | `since=0`／缺省 = 全量（必带 `chatlab+meta+members+messages`）；`since>0` = 增量（只带 `messages`） |
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

- **不给 `offset`**：规范明确不建议在发现接口用它（列表变化时会出现重复或漏项）。
- **`page` 总是给出**：规范说客户端在响应里**未发现** `page` 时按「单次全量结果」处理 —— 那比
  「靠条数猜有没有截断」明确。契约套件里有一条断言正是查这个。
- `cursor` 与查询条件绑定：`keyword` 变化后旧游标应视为失效（本实现里它退化为第一页）。
- **`memberCount` 是可选键**：群名册加载得到才出现（私聊、或名册缺失时**不出现这个键**）。
  「没有名册」与「名册是空的」在下游是两件事 —— 前者不该被读成 `0`。
- `type` 只有 `group` / `private` 两个取值（规范的枚举就这么大）：公众号与「其它」都归到
  `private`，它们都是**一对一的对话**，而规范没有第三个格子可放。

排序为 `lastMessageAt` 降序、`id` 升序 —— 稳定，游标翻页因此不会跳项或重复。

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

- **为什么不带正文**：规范对这条通道的定位是「仅通知：ChatLab 不假设 SSE 事件可靠送达」——
  客户端收到后**去拉**那一页。带正文会诱导调用方把它当数据源，而它并不保证送达；不带，语义就
  没有歧义。**契约套件里有一条断言就查这个**（帧里出现 `content` 或 `messages` 即失败）。
- `eventId` 与 `platformMessageId` 是**两个不同的号**：前者是事件通道自己的标识，后者是那条消息
  在平台上的 id（拉取时用它定位）。撤回事件里后者是被撤回那条的 id。
- `sync` 基线帧带 `generation`（注销时递增），客户端据此区分「注销后新账号刚开始」与「自己漏收
  了」—— 前者该丢弃本地状态重新拉，后者该补拉。

### GET/POST `/api/v1/sync` — 手动增量同步

立即跑一次水位增量同步（正常情况下由文件监视自动触发，此端点用于强制对账）：

```json
{ "success": true, "newMessages": 3, "revokeMessages": 0 }
```

撤回计数键为 `revokeMessages`，不计入 `newMessages`。新消息同时会通过 SSE 推给已订阅
的客户端。

**这是一个触发器，不返回消息体** —— 消息的唯一读取面是 `/api/v1/messages` 与 ChatLab
Pull，避免同一批数据出现第二种形状。WeFlow（安装版）没有这个接口，因此它没有可对齐的
上游契约；qqflow-server 的 `/api/v1/sync` 返回同一形状。

### SNS（朋友圈，本地缓存只读）

| 端点 | 参数 | 响应键 |
|---|---|---|
| `/api/v1/sns/timeline` | `limit`（**默认 50、上限 500**）、`offset`、`username`、`start`、`end` | `{count,total,feeds:[...]}` |
| `/api/v1/sns/usernames` | — | `{success,count,usernames:[...]}` |
| `/api/v1/sns/stats` | — | `{feeds, ...}` |
| `/api/v1/sns/export` | `format=json\|html`、`username` | `{count, entries/...}` |
| `/api/v1/sns/export/stats` | — | `{data:{totalPosts: N, ...}}` |
| `/api/v1/sns/media/proxy` | `url`、`referer`、`user_agent` | 媒体字节流（CDN 鉴权墙时返回明确错误） |

feeds 条目字段（以源码 `sns.rs` 为准）：`tid/userName/content(明文XML 解析后)/likes/comments/mediaList/location/rawXml` 的等效 JSON 键
（`mediaList` 每项含 `url/thumb/md5/width/height`）。

## 数据获取与安全模型

- **活库直读**：对微信加密库持只读长连接（`db/live.rs`，qqflow 式），`PRAGMA query_only`，
  全程不写源库、**不在磁盘生成明文镜像/快照**。
- **变更检测**：轮询以 (主文件 mtime/size, wal mtime/size) 双指纹判断变化，仅对变化库做
  水位增量查询 `(create_time, sort_seq, local_id) > watermark`。
- **密钥策略**：注册入内存、重启失效需重注册；日志、导出、响应均不打码密钥值。
- **媒体**：图片 dat(V1/V2/XOR) 解密、语音 silk 合并、视频明文直通、wxgf(HEVC)→PNG 需
  ffmpeg（环境变量 `WEFLOW_SERVER_FFMPEG` → WeFlow 内置 ffmpeg → PATH）。

## 启动示例

```powershell
weflow-server.exe --port 5033 --watch-fallback-ms 5000 --log info
```

参数：`--show-token`、`--port`、`--host`、`--log`、`--watch-debounce-ms`、
`--watch-fallback-ms`、`--media-export-dir`、`--base-url`（全部仅命令行，无配置文件）。
数据目录不可配置：Windows `%LOCALAPPDATA%\weflow-server`；媒体导出默认落在其下的
`api-media`，仅 `--media-export-dir` 可改。
