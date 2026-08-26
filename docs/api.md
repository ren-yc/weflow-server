# weflow-server HTTP API

接口形态与 [WeFlow HTTP API](https://doc.weflow.top/) 对齐（微信版），默认监听 `127.0.0.1:5033`
（避免与 WeFlow 5031、qqflow-server 5032 冲突）。除健康检查外所有 `/api/v1/*` 接口受 Token 保护，
三种传参方式任选其一：

1. `Authorization: Bearer <token>`
2. `?access_token=<token>`（SSE 长连接推荐）
3. POST JSON body `{"access_token": "<token>"}`

## 端点

| 端点 | 说明 |
|---|---|
| `GET/POST /health`、`/api/v1/health` | 健康检查（免鉴权） |
| `POST /api/v1/accounts` | 注册账号：`wxid` + `key`（64-hex，统一 key）或 `keys`（相对路径→64-hex 的每库 key 映射）+ 可选 `db_path`、`img_code` |
| `GET/POST /api/v1/messages` | `talker` 必填；`limit/offset/start/end/keyword/chatlab/format` |
| `GET/POST /api/v1/sessions` | 会话列表（`format=chatlab` 输出 ChatLab 形态） |
| `GET /api/v1/sessions/{id}/messages` | ChatLab Pull 增量同步（`since/end/limit/offset` + `sync` 块） |
| `GET/POST /api/v1/contacts` | 联系人（contact.db） |
| `GET/POST /api/v1/group-members` | 群成员（`chatroomId`，`includeMessageCounts`） |
| `GET/POST /api/v1/media/{talker}/{mediaType}/{file}` | 导出媒体直服（防穿越、MIME 表） |
| `GET/POST /api/v1/push/messages` | SSE：`ready` → `message.new` / `message.revoke`，25s ping |
| `GET/POST /api/v1/sync` | 手动增量同步（watch 同路径） |

## 注册账号

```bash
# 统一 key（微信 4.x 若每库同 key）：
curl -X POST http://127.0.0.1:5033/api/v1/accounts \
  -H "Authorization: Bearer $(cat %LOCALAPPDATA%\weflow-server\token.txt)" \
  -H "Content-Type: application/json" \
  -d '{"wxid":"wxid_xxx","key":"<64hex>","db_path":"C:\\Users\\me\\Documents\\xwechat_files\\wxid_xxx"}'

# 每库独立 key：
curl -X POST ... -d '{"wxid":"wxid_xxx","keys":{"session/session.db":"<64hex>","message/message_0.db":"<64hex>"}}'
```

密钥以页 1 HMAC 确定性校验：错 key 一定失败（账号进入 `error` 状态，重注册恢复）；密钥仅内存。

## 消息对象字段

`localId` / `serverId`(字符串，防精度丢失) / `localType` / `createTime`(秒) / `isSend` / `senderUsername`
/ `senderName` / `content`(显示文本或占位符) / `rawContent` / `parsedContent` / `replyToMessageId`
/ `quote{platformMessageId,sender,accountName,content,type}` / `media{type,fileName,md5}`。

## SSE 事件

`message.new` / `message.revoke`（字段：`event/sessionId/sessionType/rawid/sourceName/groupName(群)/
content/timestamp`；`rawid` 为原消息 serverId 字符串；`sessionType` ∈ private|group|official|other）。
只推送收信；撤回事件携带被撤原消息 id 与内容。连接建立先收 `ready`，事件带自增 `id:`（可配合
`Last-Event-ID` 恢复），25s `: ping` 心跳。

## 错误信封

非 2xx 响应体：`{"success":false,"code":<http_status>,"message":"..."}`。

## ChatLab 对接

`baseUrl` 填 `http://127.0.0.1:5033/api/v1`，Token 填 `%LOCALAPPDATA%\weflow-server\token.txt` 内容。
### 媒体导出（media=1，v1.5）
- `messages?media=1` 触发导出；子开关 `image(tupian)/voice(vioce)/video/emoji` 可选。
- 消息 `media` 对象填充 `url`（含 `access_token`）、`localPath`、`exported: true`；
  顶层 `media.exportPath` 为导出根目录。
- 图片源：`hardlink.db:image_hardlink_info_v4` → `msg/attach/<md5(session)>/<月>/Img/<md5>.dat`，
  dat V1（内置密钥）/V2（注册 `img_aes_key`+`img_xor_key`，或 `img_code` 派生）/legacy XOR；
  输出按真实格式命名 `.jpg/.png/.gif/.webp/.wxgf`（wxgf 容器转码见 M5.7）。
- 语音：`media_*.db:VoiceInfo(svr_id)` → 去 1 字节状态头的 `.silk`（24kHz silk）。
- 视频：`video_hardlink_info_v4` → `msg/video/<月>/<file>` 明文直通（加密流待 ISAAC-64）。
- 表情：emoticon 库 cdn 直链（external url，不落盘）。
- 幂等：同内容重复请求直接复用；单条失败不影响其余（保持元数据返回）。
### SNS（朋友圈，v1.6）
- `GET /api/v1/sns/timeline?username=&limit=&offset=&start=&end=`：时间线（feedId/username/displayName/createTime/type/content/commentCount/media[{kind,md5,url,thumb,width,height}]）；媒体为 CDN 引用（thumb/full）。
- `GET /api/v1/sns/usernames`：去重发布者（按最近发布排序，含 feedCount/lastPostTime/displayName）。
- `GET /api/v1/sns/stats`：聚合统计。
- `GET|POST /api/v1/sns/export?username=&format=json|html`：把缓存时间线序列化为文件（`<data_dir>/exports/`），返回 path/count/bytes/stats。
- `GET /api/v1/sns/export/stats`：聚合统计（含 byYear）+ 最近一次导出产物信息。
- `GET /api/v1/sns/media/proxy?url=`：SNS 媒体受限中继——仅允许 *.qpic.cn/*.qpic.com/*.qq.com；标准图直通、wxgf→PNG；上游拒绝时返回 502 `cdn_rejected`（mmsns CDN 需微信客户端上下文，匿名拉取不可行——已实证）。
- 有意不实现（只读原则）：`DELETE /sns/post/{id}`、`/sns/block-delete/*`（机制研究见 weflow-analysis.md 附录 A）。
### 账号持久化（v1.6）
- `POST /api/v1/accounts` 成功后，注册载荷（wxid/db_path/keys/img keys）会写入
  `<data_dir>/accounts/<wxid>.json`；服务重启时**自动恢复**全部账号并重建索引/watcher，
  无需外部配置文件或重复注册。
- 该文件包含明文库密钥，敏感级别等同 `all_keys.json`；删除对应文件即可取消自动恢复。