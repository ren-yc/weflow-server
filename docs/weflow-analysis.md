# WeFlow 5.1（最终版）实现深度分析

> 分析对象：`C:\Program Files\WeFlow`（安装版，5.1.x 最终发行）＋ 源码仓库
> `~/Downloads/Weflow/WeFlow-f3b0cf5d3ac1b162739a49d0c2dc070c119c0e28`。
> 方法：二进制资产清点与差异对比、导出表/字符串取证、bundle（app.asar 解包后 dist-electron）
> 主进程代码逆向、IDA Pro 反编译关键函数。状态：持续更新。

## 1. 部署资产总览

### 1.1 程序本体
```
C:\Program Files\WeFlow\
├─ WeFlow.exe                 236,042,240  Electron 主程序（Chrome/Node 运行时）
├─ resources\
│  ├─ app.asar                 488,704,225  应用代码包（主进程 dist-electron + 渲染层 dist）
│  ├─ app.asar.unpacked\       原生 node 插件与运行时（koffi、sherpa-onnx、silk-wasm、
│  │                           ffmpeg-static、lightningcss、jszip、liquid-glass 等）
│  ├─ app-update.yml           自动更新源配置
│  ├─ elevate.exe              提权辅助
│  └─ resources\               自定义二进制资源（见 1.2）
└─ locales\ …                  Chromium 区域文件
```

### 1.2 `resources\resources\` 二进制资产（与仓库源码版对比）
| 路径 | 大小 | 与仓库版差异 | 作用 |
|---|---|---|---|
| `wcdb\win32\x64\wcdb_api.dll` | 1,424,896 | **不同**（仓库 1,359,872） | 主 WCDB 封装层：open/monitor/查询/导出/云/AI 全功能 API（C ABI，koffi 调用） |
| `wcdb\win32\x64\WCDB.dll` | 9,664,512 | 逐字节一致 | 腾讯 WCDB 核心（内嵌 SQLCipher 4.1.0 community + SQLite3.26-fork）：全部加解密与 SQL 引擎 |
| `wcdb\win32\x64\SDL2.dll` | 2,500,096 | 一致 | wrapper 依赖（UI/音频无关，链接依赖） |
| `key\win32\x64\wx_key.dll` | 244,224 | **不同**（仓库 195,072） | 微信进程密钥提取/恢复（Hook + V4 内存扫描），带应用侧认证 |
| `image\win32\x64\img_helper.dll` | 23,040 | 一致 | 微信进程辅助注入助手（mmtools 血统，Init/UninstallImgHelper） |
| `wedecrypt\win32\x64\weflow-image-native-win32-x64.node` | 290,816 | 一致 | 图片 .dat V4 解密高性能原生插件（nativeImageDecrypt） |
| `welive\win32\x64\welive.exe` | 2,142,720 | **仓库缺失** | 独立 CLI 数据导出器（“WeLive”，raw-json 导出） |
| `welive\win32\x64\resources\win32\x64\wcdb_api.dll` | 1,191,936 | **仓库缺失** | WeLive 自带（较老）wcdb_api：导出 wxa### 系列 109 个混淆导出 |
| `welive\win32\x64\resources\win32\x64\WCDB.dll` | 9,664,512 | 与主版一致 | 同上 |
| `runtime\win32\*.dll` | — | 一致 | VC 运行库（msvcp140/vcruntime140 系列） |
| `fonts\…`、`icons\…`、`installer\…`、`image\README.md` | — | — | 字体/图标/安装脚本 |

**关键结论**：与开源仓库相比，最终版仅差异三件：`wcdb_api.dll`（较大，功能面扩展）、
`wx_key.dll`（新构建，见 §5）、以及全新 `welive\` 子产品；WCDB 核心与图片解密插件字节一致。

## 2. 整体架构

```
┌────────────────────────── Electron 主进程 (dist-electron/main.js) ──────────────────────────┐
│  config-service（electron-store + safeStorage 加密落盘）  http-service(API 5031/SSE)          │
│  wcdb-service ──(koffi FFI)──▶ wcdb_api.dll ──▶ WCDB.dll（SQLCipher4.1 核心）                │
│  key-service   ──(koffi FFI)──▶ wx_key.dll ──Hook/扫描──▶ 运行中的 Weixin.exe                │
│  chat-service / export-service（含 welive raw 模式）/ sns-service / message-push-service      │
│  image-decrypt（native addon / JS 兜底）/ video-service（wasm ffmpeg+isaac64）/               │
│  voice-transcribe（sherpa-onnx / whisper）/ 云控制 / 上报（api.weflow.top）                    │
│  wcdbWorker（worker_threads）⇄ 命名管道 \\.\pipe\weflow_monitor_{n} ⇐ wcdb_api 目录监视      │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
 微信数据（db_storage/*.db SQLCipher4 每库独立 enc_key）  ← 实时直读/WAL/文件事件
```

## 3. 数据库访问层（wcdb_api.dll + WCDB.dll）

### 3.1 打开协议（IDA 实证，见此前记录）
- `wcdb_open_account(path, hexKey, &handle)`：hex key 原样透传（无 hex 解码）。
- 候选页大小循环：**4096 → canOpen 失败 → 1024**；`Database::setCipherKey(UnsafeData(key), pageSize, CipherVersion=0)`。
- CipherVersion=0 ⇒ 不下发 `cipher_compatibility`，即纯 SQLCipher 默认：AES-256-CBC、
  HMAC-SHA512、kdf=PBKDF2-HMAC-SHA512/256000、页 4096、reserve 80；key 以 64-hex ASCII 作
  **口令**（sqlite3_key 第 3 参），AES 主钥 = PBKDF2(口令, 文件前 16B salt, 256000)。
- 真实微信库为**每库独立 enc_key**（键值对 `all_keys.json`；内存 `x'<64hex key><32hex salt>'` 99B）。

### 3.2 查询/游标面（wcdb_api 导出，节选）
`sessions` / `messages` / `message_cursor`（asc/desc、begin/endTs、batch）/ `group_members` /
`contacts_compact` / `display_names` / `avatar_urls` / `head_image` / `emoticon(cdn|caption|strict)` /
`media_schema_summary` / `message_by_id|svrid` / `aggregate_stats` / `annual_report*` /
`dual_report_stats` / `ai_*`（会话/消息上下文检索）/ `cloud_*` / `export_table_snapshot` /
`import_table_snapshot(_with_schema)` / `sns_*`（post 删除/防删触发）/ `mark_all_read` /
`reorder_sessions` / `update|delete_message`（本地改/删消息）/ `add_custom_emoticon`。

### 3.3 监控管道（wcdb_api 原生实现，IDA 实证）
- 管道名：`\\.\pipe\weflow_monitor_<PID>`（十进制进程号；`sub_18004B370` 生成，
  全局保存；`wcdb_get_monitor_pipe_name` 可查）。
- `wcdb_start_monitor_pipe()`（IDA @0x180105150）：
  - 先决：`byte_180155F94`（authenticated，由 wcdb_authenticate 成功置位）否则走失败路径；
    已启动（`dword_180153000>0`）返回 -1005；
  - `CreateNamedPipeW(name, PIPE_ACCESS_OUTBOUND=2, PIPE_TYPE_BYTE|READMODE_BYTE|WAIT=1,
    1 实例, 4096 出缓冲, 0 入缓冲)` —— **服务端单向推送**（JS 侧为客户端）；
  - `ConnectNamedPipe`（阻塞等待 worker 接入）。
- Worker 侧（wcdbWorker.js）已还原：net.createConnection → 按行 JSON.parse →
  `monitorCallback(action||'update', raw)` → `postMessage{type:'monitor',payload:{type,json}}`
  → 主进程 monitorListener；世代号防串台、断线重连。
- 事件字段：`action ∈ {update,create,delete}`、`table ∈ {Session, msg_<md5>, …}`，
  `collectSessionIdsFromPayload` 按 md5 反查会话（全 32/前 16 位）。

### 3.4 认证/保护（wcdb_api 原生，IDA 实证 @0x18008ECE0）
`wcdb_authenticate(token, port, issuedAtMs)`：
1. **许可证时间门**：`time64(NULL) > 1790812799`（= **2026-09-30 23:59:59 UTC**）→ 返回 -1005
   （内置有效期；超期版本即失效）。
2. token：必须 48 位 hex（`isxdigit` 全检，长度非 48 → -1008 "missing grant"）；
   port ∈ [1,65535]、issuedAtMs>0，否则 -1008。
3. 组包并发送（`sub_180108F70(port, msg, &reply, 1500)`，超时 1500ms）：
   消息含 `\tv1\t`（协议版本 v1 字面量 @0x18008EF20）+ token + `\n`，
   与会话字段布局对应应用侧 server 校验（mode@0、token@4、180s 窗）。
4. 应答必须为 `"OK"`（2 字节 0x4B4F）→ 置位全局认证标志
   `byte_180155F94=1`、`dword_180153000=0`；否则 -1010；发送失败 -1009。
5. 应用侧（bundle `nn` 类）同 §3.4 上节：本地 TCP server 校验 mode/token/窗口后回 `OK `。

### 3.6 导出游标/触发器/快照 API 面（wcdb_api 导出全貌，115 项）
- 游标族：`open_message_cursor` / `open_message_cursor_with_key` / `fetch_message_batch` /
  `close_message_cursor` / `set_message_cursor_projection`（`InitExportCursorHeap` 批量池）。
- 触发器族（防撤回/防删）：`install/uninstall/check_message_anti_revoke_trigger`、
  `install/uninstall/check_sns_block_delete_trigger`、`delete_sns_post`。
- 快照：`export_table_snapshot` / `import_table_snapshot(_with_schema)`。
- 媒体：`resolve_image_hardlink(_batch)`、`resolve_video_hardlink_md5(_batch)`、
  `get_voice_data(_batch)`、`get_head_image_buffers`、`get_emoticon_caption(_strict)`、`get_emoticon_cdn_url`。
- 云/AI/统计：`cloud_init/report/stop`、`get_db_status`、`get_aggregate_stats`、
  `get_annual_report_*`、`get_dual_report_stats`、`get_sns_annual/export_stats`、`ai_*`（会话/消息上下文检索）。
- 写作：`update_message` / `delete_message` / `update|delete_custom_emoticon` /
  `mark_all_sessions_read` / `reorder_sessions_by_time` / `set_my_wxid`。

### 3.5 监控管道协议（JS 侧完整实证：wcdbWorker.js）
```
wcdb_start_monitor_pipe()      # 原生侧：目录监视（ReadDirectoryChangesW 类）+ 管道服务端
wcdb_get_monitor_pipe_name()   # 默认 \\.\pipe\weflow_monitor（号段可随机）
worker: net.createConnection(pipe)  →  按 \r?\n 切行（容错 }\s*{ → } {、NUL→空格）
  JSON.parse(行) → monitorCallback(json.action||"update", rawJson)
  → parentPort.postMessage({type:"monitor", payload:{type, json}})
  → main.js monitorListener(payload)   # 断线 scheduleReconnect，generation 防串台
```
事件负载至少含 `action ∈ {update,create,delete}` 与 `table ∈ {Session, msg_<md5>, …}`，
`collectSessionIdsFromPayload` 反查会话（全 32 位或前 16 位 md5）。

## 4. 密钥与微信进程交互（wx_key.dll，安装版新构建）

- 导出：`InitializeHook / PollKeyData / CleanupHook / RecoverDbKey / RecoverDbKeyEx /
  GetImageKey / GetStatusMessage / GetLastErrorMsg`。
- 运行前需应用侧授权：环境变量 `WEFLOW_XKEY_AUTH_PORT/TOKEN/MODE/TS`（与保护握手同源）。
- 完整流程（中文日志实证）：
  1. “开始初始化Hook系统”→ **间接系统调用**（Indirect Syscalls，规避内核回调/AV 的
     Nt* 直接调用）→ “正在打开目标进程”(Weixin.exe) → 分配远程数据缓冲区/远程伪栈 →
     “初始化IPC通信/启动IPC监听” → **安装远程 Hook** → “安装成功，现在登录微信…”
     （要求用户重新登录微信以触发密钥回调！）
  2. `RecoverDbKey/RecoverDbKeyEx`（新）：调用方需传入 db 文件路径 →
     读取其第一页用于验证 → 内存扫描 internal_db_key（“未找到可验证的数据库密钥，
     候选地址…”/“内存扫描失败：internal_db_key”）→ 候选管线：**原始候选 → 去重 →
     双 UUIDv4 过滤**（候选与 4.x 的 wxid UUIDv4 结构邻接特征）→ **以 db 页 1 对
     internal_db_key 做 XOR 验证**（“已使用 internal_db_key XOR 验证”）→ 返回 64-hex。
  3. 版本门：获取微信版本失败→“目标进程可能已退出”；仅支持“及以上 4.x 版本”，
     否则“不支持的微信版本”。
  4. 版本特征：扫描 “唯一码文件” `key_(\d+)_.+\.statistic`（微信缓存目录内按版本命名）；
     旧版分支 <4.1.4 / 4.0.x / 4.1.4–4.1.6.14 / >4.1.6.14（社区已知，DLL 内亦有分支）。
  4. 返回 JSON：`{..., "keys":[{...}], "aesKey":"…", "xorKey":…}`（图片键同时返回）。
- `GetImageKey`：code → xorKey=code&0xff；aesKey=MD5(code+wxid)[:16] ASCII。

## 5. 图片/媒体解密
- `weflow-image-native-win32-x64.node`：V4 格式 `[6B 魔数][aesSize LE@6][xorSize LE@10]`
  + AES-128-ECB 段 + 明文段 + XOR 段；失败回退 JS 实现。
- `img_helper.dll`（mmtools 血统，`InitImgHelper/UninstallImgHelper`）：“Weixin.dll not found”
  提示 + 注入式辅助（用于在微信进程内提供原生图片/媒体能力）。
- 视频：hardlink 库查询 → wasm ffmpeg 解码；ISAAC-64 密钥流（`wasm_video_decode.wasm`）。
- 语音：媒体库 hex → silk-wasm 24kHz → WAV；转写 sherpa-onnx/whisper。

## 5b. WeLive（welive.exe，最终版新增子产品）
- 位置：`resources\resources\welive\win32\x64\welive.exe`（无版本信息，纯资源编译）
  + 自带 `resources\win32\x64\{wcdb_api.dll(1.19MB,wxa### 混淆导出 109 个),
  WCDB.dll(与主版逐字节一致)}`。
- 依赖面：仅 KERNEL32/ws2_32/ntdll/ucrt —— 运行期 `LoadLibrary` 加载自带的 WCDB 栈，
  不依赖 Electron。
- CLI 参数（字符串取证）：`--export / --media-* / --logs-dir / --batch-* / --parse-(-content) /
  --account-* / --image-* / --raw-json / --readable / --config / --state-* / --sanity(ze) /
  <db path> / --my-wxid / --session-(-key|-en) / --out-dir / --include-media` 等。
- 角色：把微信库导出为“每会话 raw-json/jsonl”的独立无头导出器（WeLive 产品线）；
  主程序导出服务的 `weliveRawExportPaths: Map<sessionId,file>` + `collectMessagesFromWeliveRaw`
  直接逐行读取其结果（免开库模式，仅需 myWxid）。
- 自带 wcdb_api 同样包含 `\\.\pipe\weflow_monitor_` 管道、SessionTable/表情CDN/会话统计 SQL。

## 6. 导出与 WeLive
- 主导出服务支持 JSON/HTML/TXT/Excel/CSV/PGSQL/ChatLab；`ExportCursorHeap`（批量游标池）。
- **welive.exe**：原生 CLI 导出器（参数 `--export/--media-*/--logs-*/--batch-*/--parse-*/
  --account-*/--image-*/--raw-json/--readable…`），自带老版 wcdb_api.dll（wxa### 混淆导出；
  含会话统计 SQL、表情 CDN 表 SQL、监控管道字符串），把库导出为**每会话 raw-jsonl**；
  主程序导出器以 `weliveRawExportPaths: Map<sessionId, file>` 直接逐行读这些文件
  （`collectMessagesFromWeliveRaw`，免开库模式）。

## 7. 网络面
- HTTP API（127.0.0.1:5031，token 三通道，SSE 推送 `ready/message.new/message.revoke`，
  25s ping、Last-Event-ID 回放 1000 条/10min）——与 docs/HTTP-API.md 一致。
- 遥测/上报：`https://api.weflow.top/api/report`、`/api/reports/batch`、`/api/token`
  （崩溃/使用统计与令牌服务；可在设置关闭）。

## 8. 关键差异与审计结论（vs 开源仓库）
1. 唯一实质不同的二进制：wcdb_api.dll（功能面扩展）、wx_key.dll（新增 RecoverDbKey*）、welive 子产品。
2. 反逆向对策：-1005 环境门（authenticate 握手）、诱饵字符串
   （`DATA_CORRUPTED_BY_PIRACY_PROTECTION`、“compatibility glue”等反 LLM/分析幻觉串）、
   反盗版文案（“你的违规修改已被加密存证。/ 检测到非法授权版本，请访问…/ 提示：请支持正版项目”）。
3. 密钥不落明文：os_crypt（DPAPI + Chromium AES-GCM v10）包装 config `safe:` 值。
4. wx_key.dll 使用**间接系统调用 + 远程伪栈**的抗检测注入，且要求应用侧
   `WEFLOW_XKEY_AUTH_*` 环境变量授权（防盗用）。
5. app.asar.unpacked 原生资产：koffi(FFI)、ffmpeg-static(79MB)、sherpa-onnx(语音转写)、
   silk-wasm(语音解码)、lightningcss、tailwindcss-oxide、electron-liquid-glass、jszip。

## 9. 收尾备注（wcdb_api.dll 原生侧已由 IDA 完成；剩余为后续可选深挖）
- ✅ `wcdb_authenticate` 原生握手（时间门/48-hex token/v1 协议/OK 应答/错误码）——见 §3.4
- ✅ 监控管道原生侧（名称格式/单向管道/单实例/ConnectNamedPipe）——见 §3.3
- ✅ 导出/触发器/媒体/云 API 面全清单（115 导出）——见 §3.6
- ⏳ wx_key.dll `RecoverDbKey*` 反编译（V4 XOR 特征字节与版本分支常量）——需把
      `Downloads\test` 或安装目录的 `wx_key.dll` 载入 IDA（或有授权时运行时实证）
- ⏳ welive.exe 主流程（CLI → LoadLibrary 自带 wcdb_api(wxa###) 调用映射）——需 IDA 加载 welive.exe
- ⏳ 监控事件 JSON 的原生组装函数字段全集（JS 侧字段已实证；原生侧字符串在未解析数据区，
      IDA xref 未建立，需手工建串后重分析——非必要）

> 分析基线：`C:\Program Files\WeFlow`（5.1 最终版）；IDA 用于 wcdb_api.dll/WCDB.dll 关键函数；
> 其余（wx_key/img_helper/welive）以导出表+字符串+运行证伪方式覆盖。文档随新证据滚动更新。
---

## 附录 A：SNS 与防删机制研究（2026-08-26）

### A.1 sns.db 结构（真库实证 + doc.weflow.top 互证）
- `SnsTimeLine(tid, user_name, content, pack_info_buf)`：content 为**明文 XML**
  （非压缩）：`<SnsDataItem><TimelineObject><id/><username/><createTime/>
  <contentDesc/><ContentObject><type/><mediaList><media><id/><type>(2=图,6=视频)/>
  <thumb …>cdn</thumb><url md5="…" key="…" enc_idx="1" token="…">cdn</url>
  <size width height totalSize/>…`，尾部 `<CommentList><user_comment …/>*`。
- 实测账号：3806 条动态、279 位发布者、8767 个媒体引用、11629 条评论；
  时间跨度 2013-10 → 在线实时。
- **媒体不落本地盘**（仅 CDN 引用：shmmsns.qpic.cn + key/token/enc_idx）——
  WeFlow 的 `/sns/proxy` 即为此设计的下载代理。
- 分页断点：`SnsMainTimeLineBreakFlag / SnsUserTimeLineBreakFlagV2 (tid, break_flag)`。

### A.2 「朋友圈防删」机制（IDA 反编译 wcdb_install_sns_block_delete_trigger @0x1800EE3D0）
```sql
CREATE TRIGGER IF NOT EXISTS block_delete_SnsTimeLine
BEFORE DELETE ON SnsTimeLine
BEGIN SELECT RAISE(IGNORE); END;
```
- 安装前先查 sqlite_master 是否已有同名触发器（有→"already_installed"）；
- 本质是 **SQL 层静默拦截 DELETE**（RAISE(IGNORE)），无进程注入；
- uninstall 对应 DROP TRIGGER；check 对应 COUNT 查询；
- ⚠️ 这是**对活库的写操作**，与本项目"数据库只读"原则冲突——不实现。
  我们的镜像+内存索引天然保留已抓取过的动态（源库删了我们也有），等效达成目的。

### A.3 消息防撤回对照（同族机制，未实现）
`wcdb_install_message_anti_revoke_trigger` 同理应为 message 库上的触发器/等价物，
本项目的只读原则同样排除写库型实现；撤回检测走增量读取 revoke 行（已实现）。