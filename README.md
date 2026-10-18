# weflow-server

无头 HTTP API + SSE 服务：实时监控、解密并提取本地 **微信 4.x** 聊天记录（`xwechat_files/<wxid>/db_storage`，
WCDB/SQLCipher-4 风格加密）。独立纯 Rust 实现，架构参考同目录 `qqflow-server`，接口契约对齐
**WeFlow HTTP API**。

## 获取

| 方式 | 入口 | 说明 |
|---|---|---|
| Release 二进制 | GitHub Releases 的最新 tag（三平台归档 ＋ `SHA256SUMS`） | 免工具链；校验后再运行 |
| `cargo install` | `cargo install --locked weflow-server` | 从 crates.io 编译 |
| 源码构建 | `powershell -File scripts\build.ps1 build --release`（Windows）／`bash scripts/build.sh build --release` | 前置工具链见 [AGENTS.md](AGENTS.md) 的「命令」 |

## 起服务

```powershell
weflow-server            # 默认 127.0.0.1:5033
weflow-server --port 5033 --host 127.0.0.1 --log info
```

- token 首次启动生成并存入系统凭据库（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service）；
  `weflow-server token` 打印。CLI 的 token 来源链是 `WEFLOW_TOKEN` → 本机凭据库。
- 数据目录：Windows `%LOCALAPPDATA%\weflow-server`。
- 停服务：前台 `Ctrl-C`。
- **零账号启动**：先注册再查询；注册前业务端点返回 503（`/health` 仍是 `starting`）。
  注册、密钥与账号面的细节见 [api 文档](docs/weflow-server-api.md)。

## 第一个请求

```bash
curl http://127.0.0.1:5033/health
curl -H "Authorization: Bearer $WEFLOW_TOKEN" \
  "http://127.0.0.1:5033/api/v1/sessions?limit=10"
```

`/health` 免鉴权且只回标量阶段；账号身份、消息数与库路径在需鉴权的 `GET /api/v1/accounts`。

## 范围

- ✅ 解密/读取层：纯 Rust 实现微信 4.0 页密码（AES-256-CBC + HMAC-SHA512，reserve=80，
  每库独立 enc_key + 16B salt）；活库直读（只读长连接，无镜像、无明文落盘），
  原始钥 PRAGMA key="x.." 跳过 KDF；页 1 HMAC 注册预校验。
- ✅ 密钥：**仅 API 注册**（不读取微信进程）：`POST /api/v1/accounts` 传 64-hex 密钥
  （每库独立 key 或统一 key）；密钥仅内存，重启后需重新注册。
- ✅ 数据读取：session.db（会话）/ message_*.db（`Msg_<md5>` 消息表 + Name2Id 反查）/ contact.db
  全量建内存索引，时间窗口水位增量同步。
- ✅ 实时监控：notify 监听 `db_storage`，防抖 350ms，慢速兜底轮询 30s；撤回检测。
- ✅ 服务封装：axum HTTP + SSE（health/accounts/messages/sessions/contacts/group-members/
  media/push/sync），另含 SNS 只读全套；默认端口 **5033**（WeFlow 5031、qqflow-server 5032）。
- ✅ 任务配方：[quickstart](docs/quickstart.md)（接 ChatLab、只要增量、导出含媒体、MCP、重启恢复）。
- ❌ 不做：微信进程内存读取/注入、防撤回钩子、桌面通知。

## 重启后恢复账号

密钥只存进程内存；服务重启后账号回到未注册（业务端点 503）。用
[`scripts/examples/register-accounts.ps1`](scripts/examples/register-accounts.ps1)（或
[`.sh`](scripts/examples/register-accounts.sh)）从你自备的本地配置读 key 后重新注册；完整配方
见 [docs/quickstart.md](docs/quickstart.md#配方五重启后恢复账号)。脚本只读配置、不代存密钥。

## 去哪看细节

| 想知道 | 看 |
|---|---|
| 这个版本改了什么（逐版行为变化与迁移） | [CHANGELOG.md](CHANGELOG.md) |
| 按任务怎么做（ChatLab/增量/导出/MCP/恢复） | [docs/quickstart.md](docs/quickstart.md) |
| HTTP 端点、CLI、退出码、错误形状 | [docs/weflow-server-api.md](docs/weflow-server-api.md) |
| 模块职责、feature 矩阵、嵌入与测试纪律 | [docs/architecture.md](docs/architecture.md) |
| agent 客户端接入（stdio MCP） | [docs/mcp.md](docs/mcp.md) |
| 发版流程与凭据/审批闸门 | [docs/release-runbook.md](docs/release-runbook.md) |
| 贡献者命令、钩子、提交规范、作为库嵌入 | [AGENTS.md](AGENTS.md) 与 [examples/embed.rs](examples/embed.rs) |
| 类型化 SDK | `clients/rust`、`clients/python`；`clients/ts` **未发布 npm**，从源码取 |

## 鸣谢

本项目借鉴了以下项目的部分功能特性（均为行为规格层面的参考，代码独立编写）：

- [hicccc77/WeFlow](https://github.com/hicccc77/WeFlow)（HTTP API 契约、监控/推送语义、db_storage 布局）
- [qqflow-server](https://github.com/ren-yc/qqflow-server)（同组织的姊妹仓库：服务架构、watch/SSE/两段式同步模式）
- [328336690/wechat-decrypt](https://github.com/328336690/wechat-decrypt)（微信 4.0 页密码实测参数）
- [0xlane/wechat-dump-rs](https://github.com/0xlane/wechat-dump-rs)（特征与格式参考，仓库已 DMCA 下架）

## 免责声明

仅供个人学习、研究与本地数据备份。API 仅监听 127.0.0.1；密钥经 HTTP 传入且仅内存保存
（不落盘）；鉴权 token 存 OS 凭据库（本地回环场景，非防泄密机制）；微信升级可能导致列名/消息格式解析退化
（值驱动探测 + 优雅降级，天然容错）。请遵守法律法规，仅解密**自己**的微信数据。
