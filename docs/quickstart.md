# weflow-server quickstart

从零到第一个请求，以及五个常见任务的完整配方。

## 获取与起服务

```bash
# Release 二进制：从 GitHub Releases 下载对应平台归档，校验后再运行
sha256sum -c SHA256SUMS

# crates.io（需要 C 工具链：SQLCipher 与 OpenSSL 从源码编译）
cargo install --locked weflow-server

# 源码构建（前置工具链见 AGENTS.md 的「命令」）
powershell -File scripts\build.ps1 build --release
```

起服务：

```powershell
weflow-server                                    # 等价于 serve，默认 127.0.0.1:5033
weflow-server --port 5033 --host 127.0.0.1 --log info
```

- token 首次启动生成并存进系统凭据库；来源链 `WEFLOW_TOKEN` → 本机凭据库，`weflow-server token` 打印。
- 凭据库不可用时 token 只在启动日志打印一次（会话级，重启即变）。
- 日志走 stderr（`weflow-server 2> 服务.log`）；数据目录 Windows `%LOCALAPPDATA%\weflow-server`；停服务 `Ctrl-C`。

## 第一个请求

```bash
curl http://127.0.0.1:5033/health
curl -H "Authorization: Bearer $WEFLOW_TOKEN" \
  "http://127.0.0.1:5033/api/v1/sessions?limit=10"
```

`/health` 免鉴权，只回标量阶段。业务端点要等账号 `state == "ready"`，之前一律返回 503。

## 配方一：接 ChatLab 前端

把 ChatLab 的 `baseUrl` 设为 `http://127.0.0.1:5033/chatlab`，然后走四步：

1. 发现：`GET /chatlab/sessions?limit=50`，记下 `sessions[].id` 与 `page.nextCursor`。
2. 全量：`GET /chatlab/sessions/{id}/messages?since=0&limit=500`，把 `sync.nextSince` 原样作为下一次的 `since`，直到 `sync.hasMore == false`。
3. 增量：`GET /chatlab/sessions/{id}/messages?since={lastPullAt}`；`since` 是排他下界。
4. 通知（可选）：`GET /chatlab/push/messages`（SSE）接收新消息元信息，再按需拉取。

> [!WARNING]
> 两套游标不能互喂：`nextCursor` 是数字页位，`since` 是秒级时刻。把页位当 `since` 传回去不会报错，而是返回几乎全量的数据。

`since` 缺省等于全量；`limit` 分页时响应必带 `sync` 块。去重按 `platformMessageId`。

## 配方二：只要增量（SSE）

1. 注册账号，轮询 `GET /api/v1/accounts` 直到 `state == "ready"`。
2. 连 `GET /api/v1/push/messages?access_token=<token>`；零账号时也会先收到 `ready` 基线帧。
3. 文件 watcher 把变更作为 SSE 事件推出（防抖 350ms，慢速兜底轮询 30s）。
4. 断线重连带 `Last-Event-ID` 续放；推送端点无就绪门控，换账号不会静默断开订阅者。

## 配方三：批量导出（含媒体）

```bash
weflow-server export --out ./export --format jsonl --with-media
```

- 每个会话一个文件（`<slug>.jsonl` 或 `<slug>.json`）；显示名折不出 ASCII 字母数字时回落到会话 id 的 slug，重名按出现顺序追加 `-2`／`-3`。
- 去重按大小写折叠（`Team` 与 `team` 是同一个文件）；`index` 永远留给清单，显示名是 `index` 的会话拿 `index-2`。
- JSONL 第一行是 `_type: header`，其后是 `_type: message` 行，不写 member 行（消息行自带 `accountName` 与 `groupNickname`）。
- JSON 形态是一个完整信封，整会话留在内存——大语料请用 jsonl。
- `index.json` 是本服务自造的编排清单，不属于 ChatLab 规范；导入请用单个会话文件。
- `--with-media` 把媒体字节下载到 `<out>/media/`，导出物里的 `media.fileName` 限定为确实落盘的句柄。
- 媒体名过本地路径校验（`../`、盘符、ADS、Win32 设备名一律跳过并计数）。`--resume` 复用要同时满足四条：上一轮清单记着该会话且文件名相同、没有 `.part` 残留、本轮带 `--with-media` 时上一轮也带过、清单本身完整。

- `--session <id>` 只导指定会话（可重复）；`--since` 只导某时间之后。
- 会话级失败记入 `skipped` 并以退出码 1 结束；`--resume` 命中记入 `reused`，不算失败；`messageCount:0` 的空会话产出 header-only 文件，也不算失败。
- 导出物里不得出现访问令牌：每行写盘前做一次子串检查，命中即整轮中止并删除半成品。
- 媒体承诺按轮成立：`--with-media --resume` 会核对句柄，有悬空就点名该会话并退 1。

## 配方四：让 agent 接入（MCP）

```bash
weflow-server mcp
```

1. 先起服务端并注册账号。
2. 在支持 stdio MCP 的客户端里把 command 指向该二进制，args 设为 `["mcp"]`。
3. token 走与服务端相同的来源链；MCP 进程不碰数据库密钥。

> [!WARNING]
> 工具输出会进入模型上下文，也就是对话内容离开本机。工具清单见 [mcp 文档](mcp.md)。

## 配方五：重启后恢复账号

密钥只存进程内存，服务重启后账号回到未注册（`/health` 的 `account` 是 `unregistered`，业务端点返回 503）。

### 1. 准备 token

同机可直接从凭据库取；跨机把 `WEFLOW_TOKEN` 设成服务端实际使用的那个值。

凭据库不可用时服务端用的是会话级 token，只在启动日志打印一次——此时 `token` 子命令与 `--show-token` 都会落空，必须从日志取当前值。

### 2. 准备恢复配置

把密钥放在自备的本地安全位置（不放进仓库、不写进脚本）。示例脚本读一个 JSON：

```json
{
  "base_url": "http://127.0.0.1:5033",
  "token": "<token>",
  "wxid": "<wxid>",
  "db_path": "C:\\Users\\<用户名>\\Documents\\xwechat_files\\<wxid>",
  "keys": {"session/session.db": "<64-hex>", "message/message_0.db": "<64-hex>"},
  "img_aes_key": "<16 位 hex，若重启前用过>",
  "img_xor_key": "<如 0x64，若重启前用过>"
}
```

`keys` 必须覆盖重启前使用的那套库。图片键与库密钥是两套，缺图片键会出现「文本正常、图片解码失败」。

### 3. 注册并确认

```bash
# Windows
powershell -File scripts/examples/register-accounts.ps1 -ConfigPath <配置.json>
# Linux / macOS
bash scripts/examples/register-accounts.sh <配置.json>
```

脚本只读配置、不写盘、不回显 token 或密钥。注册后轮询账号明细直到 `state == "ready"`：

```powershell
weflow-server accounts
```

## 命令行子命令

默认 feature 是 `["server", "cli", "mcp"]`，`cargo install` 装出来即有；`--no-default-features` 时整面消失。裸跑仍等于 `serve`。

| 子命令 | 打哪个面 | 要点 |
|---|---|---|
| `serve` | — | 起服务，等价裸跑 |
| `token` | 系统凭据库 | 打印 API token 并退出 |
| `sessions` | SDK `list_all_sessions` | `page_size=10000`，一次取尽 |
| `messages` | SDK `list_messages` | 必须给 `--talker`；`--since` 收 unix 秒或 `YYYYMMDD` |
| `search` | SDK `list_messages` | `--keyword` 必填；搜不到退出 0 |
| `contacts` | SDK `contacts` | 单页，全量请自己翻页 |
| `accounts` | SDK `accounts` | 只有 HTTP 形态，没有 `--embedded` |
| `sync` | SDK `sync_now` | 写动作，立刻跑一次增量同步 |
| `export` | SDK `list_all_sessions` ＋ `pull_page` ＋ `chatlab_messages` | 只走 HTTP，批量导出到 ChatLab Format |
| `mcp` | 经 SDK 打 HTTP 面 | stdio MCP 服务端，必须先起服务端 |

退出码：`0` 成功；`1` 运行期错误（连不上、被拒、缺密钥、缺配置）；`2` 用法错误（未知子命令、缺必需参数）。
输出默认是人类可读的紧凑行，`--json` 给机器可读形状；stdout 只放命令输出，日志一律走 stderr。

环境变量：`WEFLOW_BASE_URL`（默认 `http://127.0.0.1:5033`）、`WEFLOW_TOKEN`。token 一律不经命令行传递——它会进 shell history 与进程列表。

`--embedded`（进程内直读本地库）只开放给 `sessions`／`messages`／`search`／`contacts`，配置路径由 `WEFLOW_EMBED_CONFIG` 给出。

配置 JSON 形态：`{"wxid": "<wxid>", "db_path": "<wxid 目录>", "keys": {"<库相对路径>": "<64-hex>"}}`。

日志旗标是 global 旗标，任何子命令位置都可用。`--log` 取 `error|warn|info|debug`，子命令默认 `warn`、serve 默认 `info`。

`--log-format` 取 `text|json`（`json` 为 stderr 逐行一个对象）；非法取值退 2。`RUST_LOG` 优先于 `--log`，两个入口的日志都走 stderr 且不带 ANSI 颜色码。

## 启动旗标速查

| 旗标 | 默认 | 说明 |
|---|---|---|
| `--port` | `5033` | 监听端口 |
| `--host` | `127.0.0.1` | 绑定地址 |
| `--log` | `info` | 日志级别 |
| `--log-format` | `text` | 日志格式 |
| `--watch-debounce-ms` | `350` | 文件事件防抖 |
| `--watch-fallback-ms` | `30000` | 慢速兜底轮询，0 关闭 |
| `--media-export-dir` | `<data-dir>/api-media` | 媒体导出根目录 |
| `--base-url` | `http://<host>:<port>` | 媒体导出链接 base URL |
| `--show-token` | — | 打印已存的 API token 并退出 |

运行参数全部由命令行指定，无配置文件。数据目录不可配置：Windows `%LOCALAPPDATA%\weflow-server`。
