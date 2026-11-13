# weflow-server HTTP API

本地微信 4.x 消息库的只读 HTTP / SSE 接口参考。以源码 `src/server/` 为准。

- 默认监听 `127.0.0.1:5033`（`--port` 可改）。
- 除 `/health`、`/api/v1/health` 与 `/openapi.json` 外，所有端点都需要鉴权。
- token 存于系统凭据库；无凭据库平台为会话级 token，随启动日志打印。
- 与 qqflow-server 的有意差异见 [flow-contract ADAPTATION.md](https://github.com/ren-yc/flow-contract/blob/master/ADAPTATION.md)。

## 目录

- [鉴权](#鉴权)
- [错误信封与错误码](#错误信封与错误码)
- [通用查询参数](#通用查询参数)
- [端点](#端点)
- [ChatLab 适配面](#chatlab-适配面)
- [SNS](#sns朋友圈本地缓存只读)
- [数据获取与安全模型](#数据获取与安全模型)
- [类型化客户端sdk](#类型化客户端sdk)

## 鉴权

两种传输等价：HTTP 头 `Authorization: Bearer <token>`，或查询参数 `?access_token=<token>`。

> [!WARNING]
> 只有这两条。`X-Api-Key`、`?token=` 与 POST JSON body 都不是通道；body 里的 `access_token` / `token` 从不被读入参数表。两种传输的 token 比对均为常时比较。

## 错误信封与错误码

所有错误共用同一信封。

```json
{ "success": false, "code": 400, "message": "..." }
```

| 码 | 含义 | 何时发生 | 处置 | 可否重试 |
|---|---|---|---|---|
| 400 | 参数错误 | 缺必填参数、取值非法、密钥校验失败 | 修正请求 | 否 |
| 401 | 鉴权失败 | token 缺失或不匹配 | 补 token | 否 |
| 404 | 未找到 | 未知路径、未知会话/群、媒体不存在 | 核对 id | 否 |
| 405 | 方法不允许 | 对只读端点用了 POST | 改用 GET | 否 |
| 500 | 服务异常 | 内部错误 | 记录并上报 | 视情况 |
| 503 | 未就绪 | 账号未注册或仍在 indexing | 轮询到 ready | 是 |

- `message` 不保证是英文：405 与未知路径 404 是中文，其余多为英文。
- 只有「会话名册、消息索引、群名册都不认识这个 id」才 404；`messageCount:0` 的会话在消息面与拉取面是 200 空页。没有 403 这一种授权判断。

## 通用查询参数

| 参数 | 适用面 | 说明 |
|---|---|---|
| `limit` | 全部列表面 | 条数上限，默认 100、上限 10000 |
| `offset` | 原生面 | 偏移量，默认 0 |
| `cursor` | 仅两个 `/chatlab` 面 | 原样回传 `page.nextCursor`；解析失败退回 `offset` |
| `start` / `end` | 消息面、拉取面、SNS | Unix 秒或 `YYYYMMDD`；`start` 取当天 0 点，`end` 取当天 23:59:59（包含） |
| `keyword` | sessions / contacts / messages | 关键词过滤，小写匹配 |
| `media` + `image`/`voice`/`video`/`emoji` | 消息面、ChatLab 消息面 | `1` 触发本页媒体导出，类型开关收窄范围 |

无法解析的时间或游标不报 400，退回默认值。上限分属五个面，不要互相换算。

| 上限 | 属于哪个面 | 含义 |
|---|---|---|
| 10000 | sessions、contacts、messages、chatlab messages | 单请求条数硬上限（默认 100） |
| 5000 | 两个 Pull 面 | 单页上限，且默认也是 5000 |
| 500 | SNS timeline | 单请求上限（默认 50） |
| 200 | MCP 工具参数 | 工具层默认 50、上限 200 |
| 200 | `media=1` | 每请求最多触发 200 项导出，不是返回条数 |

`GET /api/v1/group-members` 没有 `limit`，整名册一次给全。SDK 形参 `page_size` 不是 HTTP 参数名，它内部发的就是 `limit`。

## 端点

16 个端点小节覆盖 17 条路由：`/health` 两条路径合并一节，`/chatlab/sessions/{id}/messages` 与原生 Pull 同一节；另有 SNS 六端点合并为一张表。

路由表在 `server/routes.rs` 的 `ROUTES`，端点与路由的等价性由 api_smoke 的路由对等测试钉住。

### GET `/openapi.json` — 接口描述（免鉴权）

返回 OpenAPI 3 描述，由 `server/dto.rs` 的响应类型生成。

**请求**：
```http
GET /openapi.json
```

**参数**：无

**响应**：OpenAPI 3 JSON 文档（`application/json`）。

- 多形状端点用 `oneOf`：`POST /api/v1/accounts` 与 `DELETE /api/v1/accounts/{wxid}` 的响应形状由状态决定。
- 字节面标注 `application/octet-stream`，推送面标注 `text/event-stream`；端点级 `description` 携带对外闸门（导出每请求 200 项、单页上限 5000、重放缓冲 1000 条 / 600 秒）。错误响应不在描述里。

### GET/POST `/health`、`/api/v1/health` — 就绪状态与账号阶段（免鉴权）

两条路径等价，GET / POST 均可。

**请求**：
```http
GET /health
```

**参数**：无

**响应**：
```json
{ "status": "ok", "version": "<version>", "account": "ready" }
```

- `status`：`ok`（已绑定账号且 ready）或 `starting`（未注册 / indexing / error）。
- `account` ∈ `unregistered | indexing | ready | error`。
- 刻意不列出账号：它免鉴权，而账号数组本身会泄露这台机器上有哪些账号。账号身份、消息数与库路径改由 `GET /api/v1/accounts` 提供。`awaiting_key` 不会出现在这里。

### GET `/api/v1/accounts` — 账号明细（需鉴权）

**请求**：
```bash
curl -H "Authorization: Bearer <token>" http://127.0.0.1:5033/api/v1/accounts
```

**参数**：无

**响应**：
```json
{ "success": true, "accounts": [ { "wxid": "<wxid>", "state": "ready", "message_count": 0,
"db_storage": "C:\\...\\db_storage" } ] }
```

- `state` ∈ `awaiting_key | indexing | ready | error`；`error` 态附 `error` 字符串，其余状态不含该键。
- 除绑定账号外，还包含启动扫描发现但尚未注册的账号（`awaiting_key`）。列表按 `wxid` 升序，不受就绪门控。

`indexing` 时列表项额外带可选对象 `progress`：`{"tables_done":3,"tables_total":12,"messages_indexed":184321}`。其余状态整块省略；`tables_total` 为 `0` 表示分母未定；三个数不是同一瞬间的快照。

### POST `/api/v1/accounts` — 注册账号（客户端驱动启动）

**前置**：无。零账号启动合法。

**请求**：
```bash
curl -X POST -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"wxid":"<wxid>","key":"<64-hex>"}' http://127.0.0.1:5033/api/v1/accounts
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `wxid` | string | 是 | 账号标识 |
| `db_path` | string | 否 | 账号根目录（含 `db_storage/`）；默认按 wxid 推导 |
| `key` | string | 否 | 每库统一 enc_key（`keys` 缺省时的单一密钥） |
| `keys` | object | 否 | `db_storage` 相对路径 → 64-hex enc_key 映射 |
| `img_code` | string | 否 | WeFlow 兼容的图片密钥代号（由服务端派生 aes/xor） |
| `img_aes_key` | string | 否 | 直接指定图片解密密钥（优先于 `img_code`），必须与 `img_xor_key` 成对给 |
| `img_xor_key` | string | 否 | 同上，与 `img_aes_key` 成对 |

**响应**：
```json
{ "success": true, "wxid": "<wxid>", "state": "accepted", "status": "indexing",
"db_storage": "C:\\...\\db_storage" }
```

- `state`：`accepted` / `already_ready` / `in_progress` / `account_conflict`。密钥或路径非法时直接返回 400 错误信封，**不带 `state` 键**。`status` 是账号当前状态机值；`progress` 在 `indexing` 时附。
- `account_conflict` 时不含 `status` / `db_storage`，改附 `occupied_by` / `occupied_status`（HTTP 仍为 200，`success` 仍为 `true`）。
- **强制单账号**：一个进程同时只绑定一个 wxid；换账号必须先注销。**判定顺序**：冲突检查在密钥校验之前。
- **密钥仅存进程内存**，重启后需重新注册；注册时对目标库做页 1 HMAC 预校验，错钥立即 400。
- 重复注册同一 wxid 且状态为 `ready` / `indexing` 时不重建索引；仅 `error` / `awaiting_key` 会被替换重建。`error` 账号仍持有绑定。

### DELETE `/api/v1/accounts/{wxid}` — 注销账号（需鉴权）

释放绑定、清空内存索引、退场后台任务。

**请求**：
```bash
curl -X DELETE -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/accounts/<wxid>?purge_media=1"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `wxid` | string(path) | 是 | 要注销的账号；与在位账号不一致时什么都不做 |
| `purge_media` | bool | 否 | 是否同时删除本账号会话的媒体导出目录，默认 `false` |

**响应**（三种结果，HTTP 均为 200）：
```json
{ "success": true, "wxid": "<wxid>", "state": "deregistered", "previous_status": "ready",
"index_cleared": true, "purged_media": false, "purged_dirs": 0 }
```

- `deregistered`——已注销；`previous_status` 是注销前状态，`index_cleared` 表示内存索引确有内容被清空。
- `not_registered`——本就没有绑定账号，幂等。`wxid_mismatch`——路径里的 wxid 不是在位账号，附 `occupied_by` / `occupied_status`，在位账号毫发无损。
- 允许在 `indexing` 中途注销；正在跑的全量构建会看到退场标志并丢弃结果。注销时清掉 SSE 重放条目并推进 `generation`，同时广播一条 `sync` 基线；`id` 计数器保留。
- `purge_media=1` 只删 `<media_export_dir>/<talker>/{images,voices,videos,emojis}`，仅在会话目录已空时删它；导出根目录永不触碰。

### GET `/api/v1/messages` — 消息查询 + 媒体导出

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/messages?talker=<id>&limit=20"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `talker` | string | 是 | 会话标识；缺失或为空返回 400 |
| `limit` | integer | 否 | 本页条数，默认 100、上限 10000 |
| `offset` | integer | 否 | 分页偏移，默认 0 |
| `start` / `end` | string | 否 | 时间边界，Unix 秒或 `YYYYMMDD` |
| `keyword` | string | 否 | 关键词过滤 |
| `media` / `image` / `voice` / `video` / `emoji` | string | 否 | `media=1` 触发本页媒体导出，其余收窄类型 |

**响应**：
```json
{ "success": true, "talker": "<id>", "count": 20, "hasMore": true, "media": { "enabled": false,
"exportPath": "", "count": 0 }, "messages": [ { "localId": 1, "serverId": "8280000000000000001",
"localType": 1, "baseType": 1, "appmsgSubtype": null, "createTime": 1700000100, "sortSeq": 0, "isSend": 0,
"senderUsername": "<wxid>", "senderName": "<昵称>", "content": "你好", "rawContent": "你好", "parsedContent": "你好",
"replyToMessageId": "8200000000000000000", "quote": null, "media": { "type": "image",
"fileName": "<name>.jpg", "md5": "<md5>" } } ] }
```

- `serverId` 为字符串：i64 超出 JS 安全整数范围。`isSend` 为数字 `0` / `1`，不是布尔。
- `replyToMessageId` 有引用才出现，无引用时省略该键；`quote` 与 `media` 恒出现，无该物时 `null`。`media` 与 `media=1` 无关。

`localType` 是打包字段（`localType = (appmsgSubtype << 32) | baseType`），原样输出、永不改动。

`baseType` 是低 32 位；`appmsgSubtype` 是高 32 位，仅当 `baseType == 49` 时给值，否则 `null`。

11000 的高位 17 不是 appmsg 子类型。判链接卡片写 `baseType == 49 && appmsgSubtype == 5`，不要硬编码 12 位打包常数。两个字段都是只读派生值，不进任何 ChatLab 面。

媒体导出：`media=1` 时服务端按需导出本页媒体，并就地补齐 `media.url` / `media.localPath` / `media.exported`。

顶层 `media` 为汇总：`{ "enabled": true, "exportPath": "C:\\...\\api-media", "count": 5 }`。

`media` 导出后的形状：
```json
{"exported":true,"fileName":"<name>.jpg","localPath":"C:\\...\\<name>.jpg","md5":"<md5>",
"mediaId":"<md5>.jpg","type":"image","url":"/api/v1/media/<name>.jpg"}
```

- `exported: true` 是导出成功的唯一判据；顶层 `media.count` 等于本页 `exported` 为真的条数。单次请求最多导出 200 项。
- `url` 是根相对路径，不含 token。`mediaId` 可直接喂给 `GET /api/v1/media/{id}`，只在确实写出本地文件、且文件名由内容摘要派生时出现。
- 未导出时 `exported` / `url` / `localPath` / `mediaId` 都不出现（不是空串、也不是 `false`）；`md5` 只在取不到摘要时省略。`type` → 目录映射：`images / voices / videos / emojis`。

文件附件（`file`）不参与导出：导出仅覆盖图片、语音、视频、表情四类。`appmsgSubtype == 6` 的文件会带 `media` 对象（`type: "file"`），但 `exported` 恒为假。

安装版契约里的 `messages[].mediaPath` 本项目不输出：它描述批量导出的落盘位置，本服务只做按需导出。

### GET `/api/v1/sessions` — 会话列表

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/sessions?limit=50&offset=0"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `limit` | integer | 否 | 本页条数，默认 100、上限 10000 |
| `offset` | integer | 否 | 分页偏移，默认 0；本面只按 `offset` 翻页 |
| `keyword` | string | 否 | 关键词过滤 |

**响应**：
```json
{ "success": true, "count": 315, "sessions": [ { "username": "<wxid>@chatroom", "displayName": "项目群",
"type": 1, "sessionType": "group", "lastTimestamp": 1700000100, "unreadCount": 2, "messageCount": 4,
"summary": "..." } ] }
```

- `type` 为数值枚举：`0` 私聊、`1` 群聊、`2` 公众号、`3` 其他；`sessionType` 是同一枚举的字符串形式（`private` / `group` / `official` / `other`）。
- 下游建议用 `sessionType`：qqflow-server 的 `type` 数值含义与本仓不同，字符串则一致。排序按 `lastTimestamp` 降序、`username` 次键。

### GET `/api/v1/sessions/{id}/messages` — ChatLab 拉取（消息游标）

同一份实现也挂在 `GET /chatlab/sessions/{id}/messages` 上，两条路径逐字节同形。**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/sessions/<id>/messages?since=0&limit=500"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `since` | string | 否 | 排他下界，Unix 秒或 `YYYYMMDD`（取当天 0 点）；缺省 = 全量 |
| `end` | string | 否 | 上界，Unix 秒或 `YYYYMMDD`（含当天 23:59:59） |
| `limit` | integer | 否 | 单页条数，默认与上限均为 5000 |
| `offset` | integer | 否 | 页内偏移，默认 0 |

**响应**：
```json
{ "chatlab": { "version": "0.0.2", "generator": "weflow-server", "exportedAt": 1700000000 }, "meta": {
"name": "项目群", "platform": "wechat", "type": "group", "groupId": "<id>", "ownerId": "<wxid>" }, "members": [ {
"platformId": "<wxid>", "accountName": "<昵称>", "groupNickname": "<群名片>", "avatar": "" } ], "messages": [ {
"sender": "<wxid>", "accountName": "<昵称>", "groupNickname": "<群名片>", "timestamp": 1700000103, "type": 0,
"content": "大家好", "platformMessageId": "8200000000000000000", "media": { "type": "image",
"fileName": "<md5>.jpg", "md5": "<md5>" } } ], "sync": { "hasMore": true, "nextSince": 1700000103,
"nextOffset": 0, "watermark": 1700000200 } }
```

`messages[].mediaId` 是「此刻取得到字节」的承诺：出现时按它去 `GET /api/v1/media/{id}` 必须成功。判据是本会话导出目录下确有该文件、且名字由内容摘要派生；缺一个就整个键省略。

`media` 的键集固定为 `{type, fileName, md5}`；`mediaId` 在消息这一层，不在 `media` 里。本面不接受 `media` 参数——要还没导出过的媒体拿到字节，走 `/chatlab/messages?media=1`（每请求上限 200 项）。

游标语义：

- `since` 排他（`create_time > since`），`end` 包含（`<= end`）。
- 页面按时间戳整秒组补齐：达到 `limit` 后仍会把当前秒的剩余消息取完，故一页可能略多于 `limit`。这保证 `nextSince` 一定能前进。
- `nextSince` 是本页最后一条的时间戳，不是整个会话的最大时间戳。`nextOffset` 常为 `0`；两个游标都应原样回传，叠加会二次跳过同一批行。`watermark` 是本次拉取的时间上界；排空后 `nextSince` 停在该上界。类型码与字段语义见「ChatLab 适配面」的「字段语义」小节。

### GET `/api/v1/contacts` — 联系人

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/contacts?limit=100&offset=0"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `limit` | integer | 否 | 本页条数，默认 100、上限 10000 |
| `offset` | integer | 否 | 分页偏移，默认 0 |
| `keyword` | string | 否 | 关键词过滤 |

**响应**：
```json
{ "success": true, "count": 100, "total": 0, "hasMore": true, "contacts": [ { "username": "<wxid>",
"displayName": "<昵称>", "nickname": "<昵称>", "remark": "<备注>", "alias": "", "avatarUrl": "", "type": "friend" }
] }
```

- `displayName` 按 `remark > nickname > username` 解析，恒为字符串。`nickname`、`remark`、`alias`、`avatarUrl` 缺值时为空字符串。
- **必须翻页**：`limit` 默认 100，不传就只拿到前 100 条，截断外的联系人在下游会退化为显示 UID。`total` 为过滤后总数；`count` 是本页条数。
- 排序键为 `(displayName, username)`，保证全序稳定。`offset` 超出末尾返回空页且 `hasMore=false`。

### GET `/api/v1/group-members` — 群成员

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/group-members?chatroomId=<id>&includeMessageCounts=1"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `chatroomId` | string | 是 | 群 id（`talker` 为别名） |
| `includeMessageCounts` | string | 否 | `1` 时 `messageCount` 为真实值，否则恒为 `0` |

**响应**：
```json
{ "success": true, "chatroomId": "<id>", "count": 81, "fromCache": false, "updatedAt": 1700000100000,
"members": [ { "wxid": "<wxid>", "displayName": "<昵称>", "nickname": "<昵称>", "remark": "", "alias": "",
"groupNickname": "", "avatarUrl": "", "isOwner": false, "isFriend": true, "messageCount": 0 } ] }
```

- 成员标识键为 `wxid`（非 `username`）。成员集合是「名册 ∪ 发言人」：潜水成员也会出现，其 `messageCount` 为 `0`；发过言但已不在名册里的照旧保留。
- 名册里的成员可能连联系人档案都没有：那时 `displayName` 回落 `wxid`。排序按 `messageCount` 降序、`wxid` 升序。`messageCount` 仅在 `includeMessageCounts=1` 时为真实值。
- 元数据刷新失败时保留上一份完整联系人、群主、名册或群名片，并在后续同步重试；成功刷新后移除已删除的元数据项。
- `isOwner` 由 `chat_room.owner` 解析；群主不在本页时全为 `false`。`fromCache` 恒为 `false`：本请求既不读盘、也不触发同步。`updatedAt` 是索引构建/更新完成时刻，毫秒。

### POST `/api/v1/media/export` — 定向导出消息媒体

在当前账号整个会话中唯一定位目标，并导出可取的本地字节。

**请求**

```bash
curl -X POST -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"talker":"<session-id>","platformMessageId":"<message-id>","sender":"<sender-id>","timestamp":1700000000}' \
  "http://127.0.0.1:5033/api/v1/media/export"
```

将 `<session-id>`、`<message-id>` 和 `<sender-id>` 替换为消息所在会话、消息 ID 和发送者 ID。

**参数**

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `talker` | string | 是 | 会话 ID |
| `platformMessageId` | string | 是 | 平台消息 ID |
| `sender` | string | 是 | 发送者 ID |
| `timestamp` | integer | 是 | 发送时刻，Unix 秒 |

**响应**

```json
{"success":true,"mediaId":"<64-hex>.png","fileName":"<md5>.jpg","mediaUrl":"/api/v1/media/<64-hex>.png"}
```

- `mediaId` 是可取句柄：URL 安全单段 token，由落盘字节的摘要派生。`fileName` 是消息自带的展示名，**不能拿它取字节**，两者通常不相等。
- `mediaUrl` 是不含凭据的根相对地址，末段就是 `mediaId`；随后通过 `GET /api/v1/media/{id}` 取字节。接口不下载任意外链。
- 字段非法为 400、未鉴权为 401、目标或源文件缺失为 404、目标不唯一或注册切换为 409。
- 无可导出的本地媒体为 422、读取或导出失败为 500、账号未就绪为 503。旧消息不受默认分页大小限制。

### GET `/api/v1/media/{id}` — 按文件名取字节（唯一的字节面）

`{id}` 是导出产物的文件名，也就是消息的 `mediaId`；消息面的 `media.fileName` 只在导出回填后才与它相同。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/api/v1/media/<md5>.jpg" -o out.jpg
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string(path) | 是 | 导出产物文件名，即消息的 `mediaId` |

**响应**：
```text
200 OK
content-type: image/jpeg
content-length: <字节数>
<媒体字节流>

404 Not Found
{ "success": false, "code": 404, "message": "media not found" }
```

- 解析范围仅限导出根下的 `<会话>/<images|voices|videos|emojis>/<文件>` 四个类型目录，不接受任何路径。路径段先过 `pathsafe` 的边界规则，再在 `canonicalize` 后校验仍落在导出根内。
- 同名多命中：候选内容一致时服务排序后的第一个；内容不一致时返回 404。找不到、被拒绝、内容冲突都返回同一个 404 信封；冲突细节只写日志。无就绪门控。

### GET `/api/v1/push/messages` — SSE 事件流（免轮询推送）

**前置**：无。零账号时连接返回 200，先收到 `ready` 基线。

**请求**：
```bash
curl -N "http://127.0.0.1:5033/api/v1/push/messages?access_token=<token>"
```

**参数**：无（`wxid` 查询参数仅作语义提示，不影响订阅内容）

**响应**：SSE 事件流（`content-type: text/event-stream`）。

```json
{"event":"message.new","sessionId":"...","sessionType":"group","rawid":"...","sourceName":"...",
"groupName":"...","content":"...","timestamp":1700000100,"media":{"type":"image","fileName":"...","md5":"..."}
}
```

事件名：`ready`（`{"status":"ok"}`）、`message.new`、`message.revoke`（完整消息，含 `content` 与媒体元数据）、`sync`（`{"event":"sync","generation":N,"watermarks":[…]}`）。

- 连接建立就发一帧 `sync` 基线。帧携带 `id:` 序号；`Last-Event-ID` 头（或查询参数）可回放最近 1000 条 / 10 分钟。每 25 秒发送 `ping` 注释帧保活。
- 广播缓冲为 1024 条：订阅端落后超过这个数就收不到逐条事件，改为收到一条 `sync` 对齐。它与重放缓冲（1000 条）不是一回事。
- `mediaId` 只在导出根下确实有该文件、且文件名由内容摘要派生时才出现；`file` 类型不参与导出，永远不会有 `mediaId`。`media` 是元数据：不含 `url` / `localPath`，也绝不含解密用的 `aes_key`。
- SSE 事件不带 `localType` / `baseType` / `appmsgSubtype`；需要类型分支请按 `content` 与 `media.type`，或用 `sessionId` 回查 `/api/v1/messages`。进程收到退出信号时主动结束所有 SSE 流。

### GET/POST `/api/v1/sync` — 手动增量同步

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -X POST -H "Authorization: Bearer <token>" http://127.0.0.1:5033/api/v1/sync
```

**参数**：无

**响应**：
```json
{ "success": true, "newMessages": 3, "revokeMessages": 0 }
```

立即跑一次入库进度增量同步。新增行即使携带较早的发送时间也会进入索引；每个数据库分片独立推进。正常情况下由文件监视自动触发，此端点用于强制对账。

- 撤回计数键为 `revokeMessages`，不计入 `newMessages`。新消息同时会通过 SSE 推给已订阅的客户端。这是一个触发器，不返回消息体。

## ChatLab 适配面

`/chatlab/*` 是同一批数据在另一套形状上的入口：发现面、消息面、拉取面、通知面四条路由。它们与 `/api/v1/*` 共用同一份实现与同一条事件总线，差别只在形状；鉴权、错误信封、`Last-Event-ID` 重放与保活都与原生面完全一致。

### GET `/chatlab/sessions` — 发现面

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/chatlab/sessions?limit=50&cursor=<nextCursor>"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `keyword` | string | 否 | 按名称或 id 模糊匹配 |
| `limit` | integer | 否 | 本页条数，默认 100、上限 10000 |
| `cursor` | string | 否 | 原样回传上一页的 `page.nextCursor`；缺省或解析失败退到 `offset` |
| `offset` | integer | 否 | `cursor` 的退路，默认 0 |

**响应**：
```json
{ "sessions": [ { "id": "<id>", "name": "项目群", "platform": "wechat", "type": "group", "messageCount": 58000,
"memberCount": 86, "lastMessageAt": 1711468800 } ], "page": { "hasMore": true, "nextCursor": "2" } }
```

- `page` 总是给出；未发现 `page` 时客户端按单次全量处理。`cursor` 与查询条件绑定：`keyword` 变化后旧游标应视为失效。
- `memberCount` 是可选键：群名册加载得到才出现。`type` 只有 `group` / `private` 两个取值；公众号与「其它」都归到 `private`。排序为 `lastMessageAt` 降序、`id` 升序。

### GET `/chatlab/messages` — 消息面

**前置**：账号 `state == "ready"`。

**请求**：
```bash
curl -H "Authorization: Bearer <token>" \
  "http://127.0.0.1:5033/chatlab/messages?talker=<id>&limit=20"
```

**参数**：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `talker` | string | 是 | 会话标识 |
| `limit` | integer | 否 | 本页条数，默认 100、上限 10000 |
| `offset` | integer | 否 | 分页偏移，默认 0 |
| `cursor` | string | 否 | 解析不了退回 `offset` |
| `start` / `end` | string | 否 | 时间边界，Unix 秒或 `YYYYMMDD` |
| `keyword` | string | 否 | 关键词过滤 |
| `media` | string | 否 | `1` 触发导出；类型开关 `image`/`voice`/`video`/`emoji` |

**没有 `format` 参数**：这条面天生就是 ChatLab 形状。

**响应**：
```json
{ "talker": "<id>", "count": 20, "page": { "hasMore": true, "nextCursor": "20" }, "chatlab": {
"version": "0.0.2", "generator": "weflow-server", "exportedAt": 1700000000 }, "meta": { "name": "项目群",
"platform": "wechat", "type": "group", "groupId": "<id>", "ownerId": "<wxid>" }, "members": [ {
"platformId": "<wxid>", "accountName": "<昵称>", "groupNickname": "<群名片>", "avatar": "" } ], "messages": [ {
"sender": "<wxid>", "accountName": "<昵称>", "groupNickname": "<群名片>", "timestamp": 1700000103, "type": 0,
"content": "大家好", "platformMessageId": "8200000000000000000" } ] }
```

- 没有 `success`：它输出的是数据信封。`count` 是本页条数，不是总数；截断由 `page` 报告（`hasMore` 为假时 `nextCursor` 为 `null`）。消息按时间升序（与原生面的降序相反）。
- `members` 只含本页出现过的发送者（去重后），不是群名册。

`media=1` 时真正执行导出（每请求上限 200 项，`file` 永不导出），并把确实写出摘要派生本地文件的条目的 `messages[].media.fileName` 回填成实际导出文件名。

`messages[].media` 是媒体元数据 `{type, fileName, md5}`：无媒体时整个键省略，`md5` 取不到时省略该键。

### GET `/chatlab/push/messages` — 通知面

**前置**：无。

**请求**：
```bash
curl -N "http://127.0.0.1:5033/chatlab/push/messages?access_token=<token>"
```

**参数**：无

**响应**：SSE 帧。

```json
{ "event": "message.new", "eventId": "...", "platformMessageId": null, "sessionId": "...",
"timestamp": 1700000000 }
```

与 `/api/v1/push/messages` 唯一的差别是帧的形状：

| | `/api/v1/push/messages` | `/chatlab/push/messages` |
|---|---|---|
| 载荷 | 完整消息（含 `content` 与媒体元数据） | 只带标识与时间 |
| 定位 | WeFlow 兼容面 | 规范里的通知通道 |

- `message.revoke` 帧同形，但 `platformMessageId` 带上被撤回那条的平台消息号。不带正文：规范对这条通道的定位是「仅通知」，客户端收到后去拉那一页。
- `eventId` 与 `platformMessageId` 是两个不同的号。`sync` 基线帧带 `generation`（注销时递增）。

### ChatLab 字段语义

`messages[].type` 采用 ChatLab 0.0.2 标准枚举，不是微信原生 `local_type`：

- `0` TEXT / `1` IMAGE / `2` VOICE / `3` VIDEO / `4` FILE / `5` EMOJI / `7` LINK / `8` LOCATION
- `24` SHARE / `25` REPLY / `27` CONTACT / `80` SYSTEM / `81` RECALL / `99` OTHER

标准中 `6` 未分配，任何情况下都不会出现。

映射要点：

- 先对 `local_type` 掩码取低 32 位；不掩码则所有 appmsg 行都落 `99` OTHER。
- `local_type` 49（appmsg）按载荷细分：带 `refermsg` → `25` REPLY，子类型 6 文件 → `4` FILE，其余 → `7` LINK。子类型优先取打包高 32 位，其次取 XML `<type>`。
- `local_type` 10000/10002 按是否真正解出撤回载荷细分：是 → `81` RECALL，否 → `80` SYSTEM。
- 与 `/api/v1/messages` 的 `localType` 是两套独立编码：同一张图片在这里是 `type: 1`，在那里是 `localType: 3`。

`meta.type` 按标准只有 `group` / `private`；公众号等归入 `private`。需要更细的分类请用 `/api/v1/sessions` 的 `sessionType`。

与标准 / 安装版的已知差异：

- `meta.groupId` 私聊也输出（值等于会话 id）。
- `meta.groupAvatar`、`members[].aliases`、`members[].roles`、`messages[].mediaPath` 均不输出；`members[].avatar` 给 HTTP URL 或空串。

`roles` 与 `isOwner` 是同一件事的两种表达，本仓只用 `isOwner`。类型覆盖面：本仓能输出 `0/1/2/3/4/5/7/8/24/25/27/80/81/99`；qqflow-server 只能输出 `0/1/2/3/80/81/99`，下游应把未覆盖码按 `99` 兜底。

字段无值时的三种表示：**省略键**＝对象不存在或本次没请求；**`null`**＝有概念、此刻没值；**空串**＝恒为字符串的字段没有内容。

| 面 | 键恒出现（无值为 `null` 或 `0`） | 无值时省略键 |
|---|---|---|
| sessions | `sessions[].summary` | — |
| messages | `appmsgSubtype`、`media`、`quote`、`media.exportPath` / `enabled` / `count` | `replyToMessageId`、`media.md5` / `url` / `localPath` / `mediaId` / `exported` |
| 两个 ChatLab 列表 | `page.nextCursor`（排空时 `null`） | `sessions[].memberCount`、`messages[].replyToMessageId`、`media` |
| 拉取面 | `sync` 四键 | `messages[].replyToMessageId`、`media`、`mediaId`（`page` 整块不出现） |
| 两个 SSE 面 | `groupName`、`media`、`media.md5`、`platformMessageId` | 消息层 `mediaId` |
| SNS stats | `stats.timeRange`（空时间线 `null`） | — |

`/api/v1/contacts`、`/api/v1/group-members` 与 ChatLab 的 `members[]` 在没有该值时给空串而不是 `null`。

涉及字段：`nickname` / `remark` / `alias` / `avatarUrl` / `avatar` 等——它们在类型上恒为字符串。

跨仓的允许差异：

- 未请求导出时的 `media.exportPath`。
- SSE `message.new` 的 `groupName` / `media`。
- 通知帧 `platformMessageId`。
- 未请求计数时的 `members[].messageCount`。
- 未知会话 / 未知群的状态码与群成员面的键名、布尔取值。

逐条见 [flow-contract ADAPTATION.md](https://github.com/ren-yc/flow-contract/blob/master/ADAPTATION.md)。

## SNS（朋友圈，本地缓存只读）

六个端点，均 GET / POST 可用。SNS 一族不进 `/openapi.json`：它们强绑定 XML 形状、尚未 DTO 化。

- `/api/v1/sns/timeline` — `username`、`limit`（默认 50、上限 500）、`offset`、`start`、`end`；响应 `{success,count,total,hasMore,timeline:[…]}`
- `/api/v1/sns/usernames` — `verbose`；响应 `{success,count,usernames:[…]}`
- `/api/v1/sns/stats` — 不读业务参数；响应 `{success,stats:{feeds,posters,byKind,mediaItems,comments,timeRange}}`
- `/api/v1/sns/export` — `username`、`format=json\|html`；响应 `{success,file,path,count,bytes,format,stats}`
- `/api/v1/sns/export/stats` — `verbose`、`username`；默认 `{success,data:{totalPosts,totalFriends,myPosts}}`，`verbose=1` 为 `{success,stats,lastExport}`
- `/api/v1/sns/media/proxy` — `url`、`referer`、`user_agent`；响应为媒体字节流

timeline / export 的条目键共 20 个：

- 标识与时间：`tid` / `id` / `username` / `nickname` / `avatarUrl` / `createTime` / `feedId` / `displayName`
- 内容：`contentDesc` / `type` / `media` / `likes` / `comments` / `rawXml` / `location` / `kind` / `commentCount`

- `location` 为 `{latitude,longitude}`。`media` 每项含 `url/thumb/md5/encIdx/rawUrl/resolvedUrl/proxyUrl/proxyThumbUrl/width/height`。
- `likes` / `comments` 每项含 `username/nickname/createTime`（`comments` 另含 `content`）。
- `usernames` 默认元素是显示名字符串，`verbose=1` 时为 `{username,displayName,feedCount,lastPostTime}`。`export/stats` 带 `username` 时全部计数改为该用户切片（`myPosts` 恒指注册账号自身）。

> [!WARNING]
> 导出物不含取图凭据：`token` / `key` 与两个同值冗余键 `rawThumb` / `resolvedThumbUrl` 已从 JSON 导出移除，需要凭据的调用方改走 `/api/v1/sns/media/proxy?url=…`。`usernames` 默认给显示名，而 timeline 的 `username` 过滤认的是原始 wxid——直接回填会得到 `total:0` 且无任何告警，请用 `verbose=1` 返回的 `username`。

## 数据获取与安全模型

- **活库直读**：对微信加密库持只读长连接，`PRAGMA query_only`，全程不写源库、不在磁盘生成明文镜像/快照。变更检测轮询以（主文件 mtime/size, wal mtime/size）双指纹判断，仅对变化库做水位增量查询。
- **密钥策略**：注册入内存、重启失效需重注册；日志、导出、响应均不打码密钥值。媒体方面，图片 dat（V1/V2/XOR）解密、语音 silk 合并、视频明文直通、wxgf（HEVC）→PNG 需 ffmpeg（`WEFLOW_SERVER_FFMPEG` → 内置 ffmpeg → PATH）。

## 类型化客户端（SDK）

本仓库提供 `clients/rust`（crate `weflow-client`）与 `clients/python`（包 `weflow-sdk`），分层一致：生成类型 + 手写行为层。

**只读查询**：

- 账号与就绪：`health()` / `accounts()` / `wait_ready()`
- 消息与其它面：`pull_page()` / `drain_session()` / `list_messages()` / `chatlab_messages()` / `contacts()` / `group_members()` / `list_all_sessions()`
- 媒体与推送：`media_bytes()` / `media_bytes_by_id()` / `watch()`

**动作**：`register()`（非阻塞，返回原始 `state`/`status`）、`ensure_ready()`（注册 + 轮询，200 的拒绝态立即失败）、`sync_now()`（写动作，只有显式调用才触发）。

- 一页语义 vs 取尽语义：`pull_page` / `list_messages` / `chatlab_messages` / `contacts` 只取一页；`drain_session` / `list_all_sessions` 内部翻页到 `hasMore=false`。
- 游标必须原样回传：`pull_page` 的 `nextSince` / `nextOffset` 叠加会二次跳过同一批行。
- `group_members` 的计数开关关闭时不发参数（不是发 `0`）；`list_all_sessions` 的 `page_size` 上限 10000。`media_bytes` 在 404 后按「先 `media=1` 导出再取」自动重试一次；`media_bytes_by_id` 不触发导出。
- 重试时导出行按（平台消息 id、sender、timestamp）三元组定位：恰好一行用它的落盘名重试，零行报「导出页不含目标」，多行报「无法确定哪一行才是本条」。
- `watch()` 的游标只在帧被消费时推进；只有 `id:` 没有 `data:` 的悬空帧不推进。类型与操作客户端由 `/openapi.json` 生成（`cargo run -p weflow-regen` / `python scripts/regen.py`），生成物入库、CI 断言无 diff；不要手改生成物。
- 行为层手写：`clients/rust/src/client.rs` 与 `clients/python/src/weflow_sdk/client.py`。Python 另有 `SyncClient`，方法面与 `Client` 一一对应，内部跑专属事件循环线程。
- 错误按性质分派：HTTP 非 2xx → `Status`；连接/超时/重置/URL 解析失败 → `Transport`；JSON 合法但不合承诺形状 → `Shape`。超时按单次操作计时、有进展即复位，没有总时限：连接 5s、普通 JSON 请求 30s、成本由数据量决定的一族 90s。
