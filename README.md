# weflow-server

[![crates.io](https://img.shields.io/crates/v/weflow-server.svg)](https://crates.io/crates/weflow-server)

[![CI](https://img.shields.io/github/actions/workflow/status/ren-yc/weflow-server/check.yml)](https://github.com/ren-yc/weflow-server/actions)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

无头 HTTP API + SSE 服务：实时监控、解密并提取本地 **微信 4.x** 聊天记录（`xwechat_files/<wxid>/db_storage`，WCDB/SQLCipher-4 风格加密）。

独立纯 Rust 实现，架构参考同目录 `qqflow-server`，接口契约对齐 **WeFlow HTTP API**。

## 家族术语

- **WeFlow** —— 安装版 HTTP API（默认端口 :5031）；三仓文档中的「WeFlow 形状 / 与 WeFlow 的差异」均以它为基线。
- **weflow-server** —— 本家族的微信 4.x 只读服务（默认端口 :5033），纯 Rust 实现，接口契约对齐 WeFlow HTTP API。
- **qqflow-server** —— 本家族的 QQ NT 只读服务（默认端口 :5032），与 weflow-server 同构。
- **WeFlow Legacy** —— briefdesk 里的旧采集插件，走安装版 WeFlow 的 HTTP 面取数。

## 获取

- **Release 二进制** — 入口：GitHub Releases 的最新 tag（三平台归档 ＋ `SHA256SUMS`）；说明：免工具链；校验后再运行
- **`cargo install`** — 入口：`cargo install --locked weflow-server`；说明：从 crates.io 编译
- **源码构建** — 入口：`powershell -File scripts\build.ps1 build --release`（Windows）／`bash scripts/build.sh build --release`；说明：前置工具链见 [AGENTS.md](AGENTS.md) 的「命令」

## 起服务

```powershell
weflow-server            # 默认 127.0.0.1:5033
weflow-server --port 5033 --host 127.0.0.1 --log info
```

- token 首次启动生成并存入系统凭据库（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service）；`weflow-server token` 打印。CLI 的 token 来源链是 `WEFLOW_TOKEN` → 本机凭据库。凭据库不可用时 token 只在启动日志打印一次（会话级），详见 quickstart 配方五。
- 数据目录：Windows `%LOCALAPPDATA%\weflow-server`。
- 停服务：前台 `Ctrl-C`。

## 注册账号（第一次查询之前）

服务是**零账号启动**的：`/health` 免鉴权、可先看阶段，而业务端点在账号 `ready` 之前一律返回 503。服务也不会自己去读微信进程——密钥要你用独立工具提取的每库 64-hex（本仓库**不做密钥提取**）。注册就一次 POST：

```bash
curl -X POST -H "Authorization: Bearer $WEFLOW_TOKEN" -H "Content-Type: application/json" \
  -d '{"wxid":"<你的 wxid>","key":"<64-hex 统一密钥>"}' \
  http://127.0.0.1:5033/api/v1/accounts
```

受理返回 `state: "accepted"`，后台开始建索引；轮询到 `ready` 即可查询：

```bash
curl -H "Authorization: Bearer $WEFLOW_TOKEN" http://127.0.0.1:5033/api/v1/accounts
```

每库独立密钥用 `keys`（路径→密钥映射）代替统一 `key`；图片键、字段全谱与重启恢复见 [quickstart 配方五](docs/quickstart.md#配方五重启后恢复账号)与 api 文档的注册端点小节。

## 第一个请求

```bash
curl http://127.0.0.1:5033/health
curl -H "Authorization: Bearer $WEFLOW_TOKEN" \
  "http://127.0.0.1:5033/api/v1/sessions?limit=10"
```

`/health` 免鉴权且只回标量阶段；账号身份、消息数与库路径在需鉴权的 `GET /api/v1/accounts`。

## 范围

- ✅ 解密/读取层：纯 Rust 实现微信 4.0 页密码（AES-256-CBC + HMAC-SHA512，reserve=80，每库独立 enc_key + 16B salt）；活库直读（只读长连接，无镜像、无明文落盘），原始钥 PRAGMA key="x.." 跳过 KDF；页 1 HMAC 注册预校验。
- ✅ 密钥：**仅 API 注册**（不读取微信进程）：`POST /api/v1/accounts` 传 64-hex 密钥（每库独立 key 或统一 key）；密钥仅内存，重启后需重新注册。
- ✅ 数据读取：session.db（会话）/ message_*.db（`Msg_<md5>` 消息表 + Name2Id 反查）/ contact.db 全量建内存索引，时间窗口水位增量同步。
- ✅ 实时监控：notify 监听 `db_storage`，防抖 350ms，慢速兜底轮询 30s；撤回检测。
- ✅ 服务封装：axum HTTP + SSE（health/accounts/messages/sessions/contacts/group-members/media/push/sync），另含 SNS 只读全套；默认端口 **5033**（WeFlow 5031、qqflow-server 5032）。
- ✅ 任务配方：[quickstart](docs/quickstart.md)（接 ChatLab、只要增量、导出含媒体、MCP、重启恢复）。
- ❌ 不做：微信进程内存读取/注入、防撤回钩子、桌面通知。

## 重启后恢复账号

密钥只存进程内存；服务重启后账号回到未注册（业务端点 503）。

用 [`scripts/examples/register-accounts.ps1`](scripts/examples/register-accounts.ps1)（或 [`.sh`](scripts/examples/register-accounts.sh)）。

从你自备的本地配置读 key 后重新注册。

完整配方见 [docs/quickstart.md](docs/quickstart.md#配方五重启后恢复账号)。脚本只读配置、不代存密钥。

## 去哪看细节

完整文档地图见 [docs/README.md](docs/README.md)：按 [Diátaxis](https://diataxis.fr/) 四类分工组织，另单列贡献入口；先按「我要做什么」选一份开始读。

- 教程（第一次跑通）：[docs/quickstart.md](docs/quickstart.md)
- 操作指南（按任务取数、接 MCP）：[docs/guides/](docs/guides/)、[docs/mcp.md](docs/mcp.md)
- 参考（查端点、参数、字段、变更）：[docs/weflow-server-api.md](docs/weflow-server-api.md)、[CHANGELOG.md](CHANGELOG.md)
- 解释（理解架构与取舍）：[docs/architecture.md](docs/architecture.md)
- 贡献（命令、门禁、提交规范）：[AGENTS.md](AGENTS.md)

类型化 SDK 见 `clients/`（`clients/ts` 未发布 npm，从源码取）；作为库嵌入见 [examples/embed.rs](examples/embed.rs)。

## 鸣谢

本项目借鉴了以下项目的部分功能特性（均为行为规格层面的参考，代码独立编写）：

- [hicccc77/WeFlow](https://github.com/hicccc77/WeFlow)（HTTP API 契约、监控/推送语义、db_storage 布局）
- [qqflow-server](https://github.com/ren-yc/qqflow-server)（同组织的姊妹仓库：服务架构、watch/SSE/两段式同步模式）
- [328336690/wechat-decrypt](https://github.com/328336690/wechat-decrypt)（微信 4.0 页密码实测参数）
- [0xlane/wechat-dump-rs](https://github.com/0xlane/wechat-dump-rs)（特征与格式参考，仓库已 DMCA 下架）

## 免责声明

仅供个人学习、研究与本地数据备份。API 仅监听 127.0.0.1；密钥经 HTTP 传入且仅内存保存（不落盘）；鉴权 token 存 OS 凭据库（本地回环场景，非防泄密机制）；微信升级可能导致列名/消息格式解析退化（值驱动探测 + 优雅降级，天然容错）。

请遵守法律法规，仅解密**自己**的微信数据。
