# weflow-server quickstart

任务导向的接入配方。先选获取方式，再按要做的事挑一节。

## 获取

| 方式 | 入口 | 说明 |
|---|---|---|
| Release 二进制 | GitHub Releases 的最新 tag，下载对应平台归档＋`SHA256SUMS` | 免工具链；校验 `sha256sum -c SHA256SUMS` 后再运行 |
| `cargo install` | `cargo install --locked weflow-server` | 从 crates.io 编译；仍需 C 工具链（SQLCipher/OpenSSL 源码编译） |
| 源码构建 | Windows `powershell -File scripts\build.ps1 build --release`；Linux/macOS `bash scripts/build.sh build --release` | 前置（MSVC/Perl/nasm、gcc/make）与命令见 [AGENTS.md](../AGENTS.md)，产物在 `target/release/` |

## 起服务与停服务

```powershell
weflow-server            # 等价于 serve，默认 127.0.0.1:5033
weflow-server --port 5033 --host 127.0.0.1 --log info
```

- token：首次启动生成并存入系统凭据库；`weflow-server token` 打印。CLI 的 token 来源链是
  `WEFLOW_TOKEN` → 本机凭据库。
- 数据目录：Windows `%LOCALAPPDATA%\weflow-server`。
- 停服务：前台 `Ctrl-C`；收到信号会停止 watcher 与 SSE 并退出。
- **零账号启动**：先注册再查询；注册前业务端点返回 503（`/health` 仍是 `starting`）。
  注册/密钥/账号面的细节见 [api 文档](weflow-server-api.md)。

## 第一个请求

```bash
curl http://127.0.0.1:5033/health
curl -H "Authorization: Bearer $WEFLOW_TOKEN" \
  "http://127.0.0.1:5033/api/v1/sessions?limit=10"
```

（`/health` 免鉴权且只回标量阶段；账号身份与消息数在 `GET /api/v1/accounts`。）

## 配方一：接 ChatLab 前端（pull 面四步）

把 ChatLab 的 `baseUrl` 设为 `http://127.0.0.1:5033/chatlab`：

1. 发现：`GET /chatlab/sessions?limit=50`，记下 `sessions[].id` 与 `page.nextCursor`。
2. 全量：`GET /chatlab/sessions/{id}/messages?since=0&limit=500`，用响应里的 `sync.nextSince`
   原样作为下一次的 `since`，直到 `sync.hasMore == false`。
3. 增量：`GET /chatlab/sessions/{id}/messages?since={lastPullAt}`（`since` 是**排他**下界）。
4. 通知（可选）：`GET /chatlab/push/messages`（SSE）接收新消息元信息，再按需拉取；通知只带
   元数据，**媒体可取性以客户端落地为准**——新图的 `mediaId` 在全图落地前（数十秒到分钟级）
   可能仍为空，服务端承诺的是「出现即可取」。

两套游标**不能互喂**：`nextCursor` 是数字页位（列表语义），`since` 是秒级时刻（排他下界）。
把页位当 `since` 传回去不会被报错拒绝，而是被当成一个极早的时刻——返回 200 和几乎全量的数据，
属于最难发现的那类静默错。

`since` 缺省等于全量；`limit` 分页时响应**必须**带 `sync` 块。去重按 `platformMessageId`。

深入：api 文档的「ChatLab 适配面（`/chatlab/*`）」各小节，翻页与条数数字看「通用查询参数」里的上限表。

## 配方二：只要增量（watcher / SSE）

1. 注册账号并等到 `GET /api/v1/accounts` 的 `state == "ready"`。
2. 连 `GET /api/v1/push/messages?access_token=<token>`；零账号时也会先收到 `ready` 基线帧。
3. 之后服务端的文件 watcher（防抖 350ms，慢速兜底轮询 30s）会把变更作为 SSE 事件推出。
4. 断线重连带 `Last-Event-ID` 续放；推送端点**无就绪门控**，换账号不会静默断开订阅者。

深入：api 文档端点小节「SSE 事件流（免轮询推送）」；重放缓冲与事件形状见「数据获取与安全模型」。

## 配方三：导出全部消息（含媒体）

```bash
weflow-server export --out ./export --format jsonl --with-media
```

- `--with-media` 把媒体字节下载到 `<out>/media/`，导出物里的 `media.fileName` 是可取句柄。
- `--resume` 复用上一轮已完成的会话；**媒体承诺按轮成立**：带 `--with-media` 复用时会核对句柄。
- 只导出个别会话用 `--session <id>`（可重复）；只要某时间之后用 `--since`。
- 媒体可取性以**客户端落地**为准：新图在客户端全图落地前（数十秒到分钟级）可能仍无句柄；
  缺句柄时退回消息面的按需导出，不要当错误。

深入：api 文档「批量导出（`export`）」（产物形状、`index.json`、失败跳过语义都在那里）。

## 配方四：让 agent 接入（MCP）

```bash
weflow-server mcp
```

在支持 stdio MCP 的客户端里把 command 指向该二进制、args 设为 `["mcp"]`；服务端需已启动并注册
账号。工具默认只读，详见 [mcp 文档](mcp.md)。

> **工具输出会进入模型上下文 —— 也就是对话内容离开本机。**

## 配方五：重启后恢复账号

密钥只存进程内存（见 [架构文档](architecture.md) 的「密钥不落盘」），所以**服务重启后账号回到
未注册**：`/health` 的 `account` 是 `unregistered`，业务端点返回 503。恢复动作只有一步——
用**与重启前相同的 key 集**重新调 `POST /api/v1/accounts`。

### 1. 准备 token

token 的来源链是 `WEFLOW_TOKEN`（非空）→ 本机凭据库（与 `weflow-server token` 同一份）。
同机可以直接从凭据库取；跨机把 `WEFLOW_TOKEN` 设成服务端实际使用的那个值。
一个会踩的坑：本机凭据库不可用时，服务端生成的是**会话级 token**（不落库、随每次启动而变，
只在启动日志打印一次）——此时凭据库里没有值可回落，`token` 子命令与 `--show-token` 都会
落空，必须从启动日志取当前值并设进 `WEFLOW_TOKEN`。

### 2. 准备恢复配置

把 key 放在**你自备的本地安全位置**（不放进仓库、不写进脚本）。示例脚本读一个 JSON：

```json
{
  "base_url": "http://127.0.0.1:5033",
  "token": "<API token>",
  "wxid": "<wxid>",
  "db_path": "C:\\Users\\<用户名>\\Documents\\xwechat_files\\<wxid>",
  "keys": {
    "session/session.db": "<64-hex enc_key>",
    "message/message_0.db": "<64-hex enc_key>"
  },
  "img_aes_key": "<16 位 hex，若重启前用过>",
  "img_xor_key": "<如 0x64，若重启前用过>"
}
```

`keys` 必须覆盖重启前使用的那套库；**图片键与库密钥是两套**，缺图片键会出现「文本正常、
图片解码失败」（详见架构文档的「密钥轮换的症状面」）。

### 3. 注册并确认

```powershell
powershell -File scripts/examples/register-accounts.ps1 -ConfigPath <你的配置.json>
```

```bash
bash scripts/examples/register-accounts.sh <你的配置.json>
```

脚本只读配置、不写盘、不回显 token 或密钥；注册后轮询账号明细直到 `state == "ready"`：

```powershell
# 需要 token；同机未设 WEFLOW_TOKEN 时 CLI 会回落到凭据库
weflow-server accounts
```

## TypeScript 客户端

`clients/ts` **未发布到 npm**；需要时从源码取（`clients/ts/README.md`，随仓库版本走）。Rust/Python
SDK 的用法见 [api 文档](weflow-server-api.md)。
