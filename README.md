# weflow-server

无头 HTTP API + SSE 服务：实时监控、解密并提取本地 **微信 4.x** 聊天记录（`xwechat_files/<wxid>/db_storage`，
WCDB/SQLCipher-4 风格加密）。独立纯 Rust 实现，架构参考同目录 `qqflow-server`，接口契约对齐
**WeFlow HTTP API**（`docs/HTTP-API.md`）。

## 范围

- ✅ 解密/读取层：纯 Rust 实现微信 4.0 页密码（AES-256-CBC + HMAC-SHA512，reserve=80，
  每库独立 enc_key + 16B salt）；活库直读（qqflow 式只读长连接，无镜像、无明文落盘），
  原始钥 PRAGMA key="x.." 跳过 KDF；页 1 HMAC 注册预校验；WAL 合并工具保留（探针/验证用）
- ✅ 密钥：**仅 API 注册**（不读取微信进程）：`POST /api/v1/accounts` 传 64-hex 密钥
  （每库独立 key 或统一 key），页 1 HMAC 确定性校验；密钥仅内存
- ✅ 数据读取：session.db（会话）/ message_*.db（`Msg_<md5>` 消息表 + Name2Id 反查）/ contact.db
  全量建内存索引，时间窗口水位（create_time, sort_seq, local_id）增量同步
- ✅ 实时监控：notify（ReadDirectoryChangesW/inotify/FSEvents）监听 `db_storage`，防抖 350ms，
  慢速兜底轮询 30s；撤回检测（local_type 10000/10002 + 关键词 + XML msgid 回溯）
- ✅ 服务封装：axum HTTP + SSE（WeFlow 契约：health/accounts/messages/sessions/contacts/
  group-members/media/push/sync），另含 SNS 只读全套（timeline/usernames/stats/export/
  export-stats/media-proxy），默认端口 **5033**（WeFlow 5031、qqflow-server 5032）
- ✅ 测试：SQLCipher 互操作 roundtrip（bundled sqlcipher 造库 → 本实现解密）、假库夹具驱动
  索引/增量/撤回、HTTP 契约、文件事件 e2e；真库探针 `#[ignore]`（环境变量开启）
- ❌ 不做：微信进程内存读取/注入、防撤回钩子、桌面通知

## 构建

| 平台 | 前置条件 | 构建命令 |
|---|---|---|
| Windows | Rust MSVC toolchain + Visual Studio（Desktop C++）+ [Strawberry Perl](https://strawberryperl.com) | `powershell -File scripts\build.ps1 build` |
| Linux | Rust + build-essential（gcc/make；perl 系统自带） | `bash scripts/build.sh build` |
| macOS | Rust + Xcode CLT | `bash scripts/build.sh build` |

SQLCipher 与 OpenSSL 为源码编译（测试夹具互操作需要），故要求 C 工具链与 perl；wrapper 自动定位
MSVC 环境与 Perl/nasm（Windows 专属），透传全部 cargo 参数（`test`/`clippy`/`build --release` 同理）。

## 发布

版本号以 `Cargo.toml` 为唯一来源：`-V`/`--version` 与运行时版本信息均编译自 `env!("CARGO_PKG_VERSION")`，
不要在其他文件里再写一遍版本号。推送 `v<版本>` tag 后，GitHub Actions（`.github/workflows/release.yml`）
自动在 Windows / Linux / macOS 三平台构建 release 二进制，校验 tag 与 `Cargo.toml` 版本一致后，
打包为 `weflow-server-<版本>-<平台目标>` 归档并附 `SHA256SUMS` 发布到 GitHub Release。

```bash
cargo install cargo-edit            # 一次性；提供 cargo set-version
cargo set-version 0.1.1             # 或手动编辑 Cargo.toml 的 version 字段
git commit -am "chore: release v0.1.1"
git tag v0.1.1 && git push origin master --tags   # tag 触发自动发布
```

## 运行

```powershell
# 1. 准备密钥：用你信任的工具获得每库 32 字节(64 hex) 的 enc_key（微信 4.x 每库独立）；
#    或使用 WeFlow/wechat-dump 系工具导出 keys 列表后手动填入
#    密钥提取参考 repo（本地只读取证工具，不入库）：
#     https://github.com/TANGandXUE/wcdb-key-tool
# 2. 启动
.\weflow-server.exe
.\weflow-server.exe --port 5033 --host 127.0.0.1 --log info
```

命令行参数：`--show-token`（打印已存 token 后退出）/ `--port`（默认 5033）/ `--host`（默认 127.0.0.1）/ `--log`（默认 info）/
`--watch-debounce-ms`（默认 350）/ `--watch-fallback-ms`（默认 30000，0 关闭）/
`--media-export-dir`（默认 `<data-dir>/api-media`）/ `--base-url`。
数据目录：Windows `%LOCALAPPDATA%\weflow-server`；访问 token 生成后存入**系统凭据库**（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service；无凭据库平台为会话级并在启动日志打印）。token **仅在首次生成时**打印到启动日志，之后可用 `--show-token` 随时获取。
**账号为客户端驱动**：启动后无账号，密钥由客户端注册（仅内存保存，不落盘）：

```bash
curl -X POST http://127.0.0.1:5033/api/v1/accounts \
  -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"wxid":"wxid_xxx","key":"<64hex 统一密钥>", "db_path":"C:\\Users\\<用户>\\Documents\\xwechat_files\\wxid_xxx"}'
```

每库独立密钥时传 `keys` 映射（相对路径 → 64hex），至少 session.db 的 key 必须匹配
（用于 page-1 HMAC 校验）。可选 `img_code` 用于图片 `.dat` 解密（xorKey=code&0xff，
aesKey=MD5(code+wxid)[:16]），或直接指定 `img_aes_key`/`img_xor_key`（推荐）。
密钥错误时账号进入 `error` 状态，重新注册即可恢复。

SSE 推送（token 任一通道：`Authorization: Bearer` / `X-Api-Key` / 查询或 POST body 参数 `access_token`/`token`，比对为常时比较）：

```bash
curl -N "http://127.0.0.1:5033/api/v1/push/messages?access_token=<token>"
```

完整接口文档见 [docs/weflow-server-api.md](docs/weflow-server-api.md)（与 WeFlow `docs/HTTP-API.md` 契约对齐）。

## 测试

```powershell
powershell -File scripts\build.ps1 test   # Windows
bash scripts/build.sh test                 # Linux/macOS
```

真库验证（ground-truth 探针与下游客户端模拟）默认跳过，需真实微信数据：
`WEFLOW_TEST_DB_ROOT`（指向 `db_storage` 所在账号目录）+ 密钥文件/环境变量，见
`tests/real_db_groundtruth.rs`。

## 目录结构

```
src/
├─ config.rs / logging.rs      # CLI（无配置文件）、日志
├─ keystore/                   # 密钥形状校验（64 hex）、img_code 派生；仅内存
├─ db/
│  ├─ scan.rs                  # xwechat_files 账号发现 + db_storage 库枚举
│  ├─ wcdb.rs                  # 微信 4.0 页密码（加密/解密/HMAC 校验/WAL 帧）
│  ├─ live.rs                  # qqflow 式活库直读连接池（只读 SQLCipher，无镜像）
│  └─ open.rs                  # rusqlite 打开/列探测辅助
├─ parser/                     # 消息内容解析（XML / zstd / 类型占位符 / 引用 / 撤回）
├─ store/                      # 内存索引（会话/联系人/消息/水位）+ 查询
├─ sync/                       # 实时同步引擎（poll + 事件）+ watch（notify 防抖/兜底）
└─ server/                     # axum：鉴权三通道、账号注册、HTTP 端点、SSE、媒体直服
tests/
├─ common/                     # SQLCipher 假库夹具（微信同构布局 + 造数 + WAL）
├─ wcdb_roundtrip.rs           # 互操作仲裁：sqlcipher 造库 → 本实现解密 → SQLite 重开
├─ index_build.rs              # 索引/增量/降级
├─ fs_watch_e2e.rs             # 文件事件 → 同步 → SSE
├─ api_smoke.rs                # HTTP 契约
└─ real_db_groundtruth.rs      # #[ignore] 真库探针
```

## 鸣谢

本项目借鉴了以下项目的部分功能特性（均为行为规格层面的参考，代码独立编写）：

- [hicccc77/WeFlow](https://github.com/hicccc77/WeFlow)（HTTP API 契约、监控/推送语义、db_storage 布局）
- [qqflow-server](https://github.com/)（同目录本地仓库：服务架构、watch/SSE/两段式同步模式）
- [328336690/wechat-decrypt](https://github.com/328336690/wechat-decrypt)（微信 4.0 页密码实测参数）
- [0xlane/wechat-dump-rs](https://github.com/0xlane/wechat-dump-rs)（特征与格式参考，仓库已 DMCA 下架）

## 免责声明

仅供个人学习、研究与本地数据备份。API 仅监听 127.0.0.1；密钥经 HTTP 传入且仅内存保存
（不落盘）；鉴权 token 存 OS 凭据库（本地回环场景，非防泄密机制）；微信升级可能导致列名/消息格式解析退化
（值驱动探测 + 优雅降级，天然容错）。请遵守法律法规，仅解密**自己**的微信数据。