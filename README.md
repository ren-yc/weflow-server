# weflow-server

[![crates.io](https://img.shields.io/crates/v/weflow-server.svg)](https://crates.io/crates/weflow-server)

[![CI](https://img.shields.io/github/actions/workflow/status/ren-yc/weflow-server/check.yml)](https://github.com/ren-yc/weflow-server/actions)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

无头 HTTP API + SSE 服务：实时监控、解密并提取本地 **微信 4.x** 聊天记录（WCDB/SQLCipher 加密库，只读直读、不落中间盘）。接口契约对齐安装版 WeFlow HTTP API（:5031）。

姊妹仓库 [qqflow-server](https://github.com/ren-yc/qqflow-server) 对 QQ NT 做同构的事（:5032）。本服务默认端口 **:5033**。

## Highlights

- **只读不落盘**：活库直读 ＋ 内存索引，从不写用户的数据库，也没有第二份数据副本。
- **密钥仅内存**：每库 64-hex 密钥经 API 注册，只在进程内存；本仓库不做密钥提取。
- **SSE 实时推送**：新消息与撤回经单条事件总线广播，迟到订阅者靠重放缓冲补齐。
- **ChatLab 兼容**：`/chatlab/*` 四条路由输出 ChatLab Format 形状，媒体可按需导出。
- **类型化 SDK**：Rust / Python 客户端（生成形状 ＋ 手写行为层），另有 TS 示例。
- **CLI 与 MCP**：命令行子命令取数与批量导出；stdio MCP 工具面供 agent 接入。

## 快速开始

获取（三选一）：

```bash
# Release 二进制：GitHub Releases 下载对应平台归档，sha256sum -c SHA256SUMS 校验后运行
cargo install --locked weflow-server
# 源码构建（前置工具链见 AGENTS.md 的「命令」）
powershell -File scripts\build.ps1 build --release      # Windows
bash scripts/build.sh build --release                   # Linux / macOS
```

起服务（token 首次启动生成并存入系统凭据库，`weflow-server token` 打印）：

```powershell
weflow-server            # 默认 127.0.0.1:5033
```

注册账号（服务零账号启动，业务端点在账号 `ready` 前一律 503；密钥要你用独立工具提取）：

```bash
curl -X POST -H "Authorization: Bearer $WEFLOW_TOKEN" -H "Content-Type: application/json" \
  -d '{"wxid":"<你的 wxid>","key":"<64-hex 统一密钥>"}' \
  http://127.0.0.1:5033/api/v1/accounts
```

第一个请求：

```bash
curl http://127.0.0.1:5033/health
curl -H "Authorization: Bearer $WEFLOW_TOKEN" \
  "http://127.0.0.1:5033/api/v1/sessions?limit=10"
```

重启后恢复、每库独立密钥、图片键等完整配方见 [quickstart 配方五](docs/quickstart.md#配方五重启后恢复账号)。

## 范围

- ✅ 解密/读取：纯 Rust 实现微信 4.0 页密码；活库只读长连接，无镜像、无明文落盘。
- ✅ 实时监控：notify 监听 ＋ 慢速兜底轮询；水位增量；撤回检测。
- ✅ 服务封装：axum HTTP + SSE 全套端点，含 SNS 只读面与 ChatLab 适配面。
- ❌ 不做：微信进程内存读取/注入、防撤回钩子、桌面通知、密钥提取。

## 文档

- 教程（5 分钟跑通）：[docs/quickstart.md](docs/quickstart.md)
- 操作指南（子命令、导出、MCP）：[docs/quickstart.md](docs/quickstart.md)、[docs/mcp.md](docs/mcp.md)
- 参考（端点、参数、字段、错误码）：[docs/weflow-server-api.md](docs/weflow-server-api.md)
- 解释（架构与陷阱）：[docs/architecture.md](docs/architecture.md)
- 贡献（命令、门禁、提交规范）：[AGENTS.md](AGENTS.md)；逐版变化：[CHANGELOG.md](CHANGELOG.md)
- SDK：`clients/`（`clients/ts` 未发布 npm）；库嵌入示例：[examples/embed.rs](examples/embed.rs)

## 鸣谢

本项目借鉴了以下项目的部分功能特性（均为行为规格层面的参考，代码独立编写）：

- [hicccc77/WeFlow](https://github.com/hicccc77/WeFlow)（HTTP API 契约、监控/推送语义、db_storage 布局）
- [qqflow-server](https://github.com/ren-yc/qqflow-server)（同组织的姊妹仓库：服务架构、watch/SSE/两段式同步模式）
- [328336690/wechat-decrypt](https://github.com/328336690/wechat-decrypt)（微信 4.0 页密码实测参数）
- [0xlane/wechat-dump-rs](https://github.com/0xlane/wechat-dump-rs)（特征与格式参考，仓库已 DMCA 下架）

## 免责声明

仅供个人学习、研究与本地数据备份。API 仅监听 127.0.0.1；密钥经 HTTP 传入且仅内存保存（不落盘）；鉴权 token 存 OS 凭据库（本地回环场景，非防泄密机制）。

微信升级可能导致列名/消息格式解析退化（值驱动探测 + 优雅降级，天然容错）。请遵守法律法规，仅解密**自己**的微信数据。
