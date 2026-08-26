# weflow-server 实施计划（定稿）

> 2026-07 决策（用户确认）：**仅微信 4.x**（对齐 WeFlow）；**密钥仅走 API 注册**（不碰微信进程，不内存扫描）；
> **v1 不做 SNS**；**先用自建加密假库验证**（真库探针保留 `#[ignore]`，待用户提供真机环境）。

## 1. 目标与范围

用 Rust 实现无头（headless）微信数据库实时监控 + 解密提取服务：账号扫描 → 密钥注册 → WCDB 解密 →
内存索引 → 文件事件驱动的实时增量 → WeFlow 兼容 HTTP + SSE API。
架构参考 `E:\Develop\qqflow-server`（已通读全部源码），接口契约对齐 WeFlow（`~/Downloads/Weflow/`
5.1.0 全源码 + [doc.weflow.top](https://doc.weflow.top/)），解密算法以 wechat-decrypt（Python）实测源码、
Kanxue/cn-sec 分析、DeepWiki 镜像（wechat-dump-rs 已 DMCA 下架）交叉验证。参考材料：
`~/Downloads/Weflow/ref/`（wechat-decrypt 克隆）与 `_fetch_wechat/`（抓取的资料）。
默认端口 **5033**（避免与 WeFlow 5031 / qqflow-server 5032 冲突）。

### v1 做
账号发现、密钥 API 注册（每库独立 enc_key 或统一 key 均可）、WCDB 页密码、
实时监控与增量提取（含撤回）、消息/会话/联系人/群成员解析、媒体（图片 dat / 语音 silk / 视频）与导出、
HTTP+SSE API（health / accounts / messages / sessions / sessions/:id/messages / contacts /
group-members / media / push/messages）、四层测试与文档。

### v1 不做
朋友圈 SNS、防撤回钩子、桌面通知、任何形式的微信进程内存读取/注入（Windows/Linux/macOS 均只走注册）。

## 2. 已确认事实

### 2.1 微信 4.0 加密规格（wechat-decrypt 实测源码 + Kanxue/cn-sec + DeepWiki 交叉验证）
- 格式 = SQLCipher 4 定制：AES-256-CBC + HMAC-SHA512；页大小 4096；reserve=80（IV 16 + HMAC 64）
- 文件头前 16 字节 = salt（每库独立 salt，无版本字节）；**每个库独立 enc_key**（32 字节/64 hex）
- 页布局：第 1 页 `[salt 16][密文 4000][IV @4016][HMAC @4032]`；其余页 `[密文 4016][IV][HMAC]`
- IV 直接存页内 4016..4032（无需派生）；CBC 无填充（4000=250×16、4016=251×16）
- mac_key = PBKDF2-HMAC-SHA512(enc_key, salt 逐字节 XOR 0x3a, 迭代 2, dklen 32)
- HMAC 输入 = 密文 + IV + LE u32(pgno)；页尾 64B 为 HMAC-SHA512
- 解密后第 1 页 = `"SQLite format 3\0"` + 明文 + 80 零字节；其余页 = 明文 + 80 零字节（usable=4016）
- 密钥校验：对任一库计算 page1 HMAC 比对即确定性验证（无假阳性）
- WAL：预分配固定 4MB（**不能按 size 检测变化，只能 mtime/last_write**）；帧 = `[24B 头][4096B 加密页]`；
  帧头 salt1/salt2（BE @8/@12）必须匹配 WAL 头 salt（BE @16/@20），否则为旧周期遗留帧跳过；
  有效帧按帧头 pgno 解密后写回页 `(pgno-1)*4096`
- 内存特征（仅存档参考，v1 不实现）：`x'<64hex_key><32hex_salt>'`（96/64/>96 hex），进程 Weixin.exe

### 2.2 微信 4.0 数据布局与表结构
- 根目录：Windows `Documents\xwechat_files`；账号目录 `<wxid>` 或 `<自定义号>_<4位后缀>`
  （判定：含 `db_storage/`；wxid_X 与 wxid_X_xxxx 并存时优先含 session.db、更新、带后缀者）
- `<账号>/db_storage/`：`session/session.db`（会话主入口，Session 表）、`message/message_*.db`
  （聊天记录分片，含 `Msg_[MD5(会话id)]` 表（大小写不敏感匹配，WeFlow 事件侧为小写 `msg_<md5>`）
  及 `Name2Id%` rowid→user_name 反查表）、`contact/contact.db`（contact/contact_label/biz_info）、
  `emoticon/`、`sns/`、`hardlink/hardlink.db`（video_hardlink_info_v4）、`media_*.db`（语音等）
- 消息列：local_id / server_id(int64，JSON 按字符串) / local_type / sort_seq / create_time(秒) /
  is_send / sender_username / real_sender_id / message_content / compress_content
  （**zstd 压缩的 XML**，需 zstd crate）/ packed_info_data
- 排序/增量：`ORDER BY create_time ASC, sort_seq ASC, local_id ASC`；`WHERE create_time >= ?` 时间窗口
- 会话字段（Session 表）：username/talker_id、sort_timestamp、last_timestamp、summary、last_msg_type、
  unread_count、type；`@chatroom` 结尾=群、`gh_` 前缀=公众号、local_type==1=好友
- 附件：图片 `<账号>/msg/attach/<sessionMd5>/<yyyy-MM>/Img/<md5>.dat`（sessionMd5=MD5(sessionId)），
  视频 `<账号>/msg/video/`，文件 `msg/file`
- 图片 dat：旧 XOR（无头单字节）/ V1（头 `07 08 56 31 08 07`，AES key 固定 `cfcd208495d565ef`）/
  V2（头 `07 08 56 32 08 07`，15B 头 [magic6][aesSize LE@6][xorSize LE@10]，负载 =
  AES-128-ECB 段 + 明文段 + XOR 段；aesKey/xorKey 由注册时提供的 `img_code` 派生：
  xorKey=code&0xFF、aesKey=MD5(code+wxid) hex 前 16 ASCII 字节）
- 视频：ISAAC-64 PRNG 密钥流（8 字节对齐生成后整段 reverse 取前 size）；4.x 多为明文 mp4
- 语音：媒体库取 hex → silk 解码 24kHz → WAV（Rust crate：silk-rs/silk-codec）
- 全局配置 `all_users/config/global_config`（MMKV，AES-128-CFB，key=`xwechat_crypt_key`，iv 全 0）——
  v1 不解析（仅存档）

### 2.3 WeFlow HTTP API 契约（docs/HTTP-API.md + httpService.ts 校对）
- 本地 `127.0.0.1:5031`，前缀 `/api/v1`，JSON；SSE=`GET /api/v1/push/messages`
- 鉴权三种：`Authorization: Bearer` / `?access_token=` / POST body `access_token`（常数时间比较）
- 端点：health（免鉴权）、push/messages（SSE）、messages（talker 必填，limit 1..10000，
  offset/start/end/keyword/chatlab/format/media(meiti)/image(tupian)/voice(vioce)/video/emoji）、
  sessions（+format=chatlab）、sessions/:id/messages（since/end/limit≤5000/offset + sync 块）、
  contacts、group-members（chatroomId|talker、includeMessageCounts|withCounts、forceRefresh）、
  media/{path}（防穿越、7 种 MIME）
- 消息对象：localId/serverId(字符串)/localType/createTime/isSend/senderUsername/content/rawContent/
  parsedContent/replyToMessageId/quote/mediaType/mediaFileName/mediaUrl/mediaLocalPath
- 会话：username/displayName/type/lastTimestamp/unreadCount；ChatLab：id/name/platform=wechat/
  type/messageCount/lastMessageAt；pull 响应带 sync{hasMore,nextSince,nextOffset,watermark}
- 联系人：username/displayName/remark/nickname/alias/avatarUrl/type；群成员：wxid/displayName/
  nickname/remark/alias/groupNickname/avatarUrl/isOwner/isFriend/messageCount
- SSE：`ready` → 回放（Last-Event-ID，1000 条 TTL 10min）→ `message.new`/`message.revoke`，25s ping；
  字段 event/sessionId/sessionType('private'|'group'|'official'|'other')/rawid/sourceName/
  groupName(群)/content/timestamp(秒)；**不推本地路径**；只推收信
- 类型映射：localType 1文本/3图片/34语音/43视频/47表情/42名片/48位置/49文件引用/50视频号/10000系统/
  10002撤回；ChatLab 映射 0文本1图片2语音3视频4文件5表情6链接7位置8红包20转账21拍一拍22通话23分享
  24引用25转发26名片27系统80撤回81其他99
- 媒体导出目录 `{cache}/api-media/{会话}/{images|voices|videos|emojis}/`

### 2.4 WeFlow 实时监控机制（messagePushService.ts / wcdbCore.ts / chatService.ts）
- 监控 `<账号>/db_storage/session/` 与 `message/` 的数据库文件写入（原生 ReadDirectoryChangesW；
  wechat-decrypt 另证实 WAL 预分配 4MB 固定大小、mtime 即变化信号）
- 事件防抖 350ms；消息表事件后 500ms 二次扫描（兜底"先写消息表后更会话表"时序）
- 增量：`WHERE create_time >= 上次水位-2s ORDER BY create_time, sort_seq, local_id LIMIT 1000`
  时间窗口游标（非 rowid/文件大小）；去重 TTL 10min；只推收信
- 撤回：lastMsgType==10002 / summary 关键词 / unread 减少触发；local_type IN (10000,10002) + 关键词
  （撤回/revokemsg/replacemsg）直查窗口 150s LIMIT 20；XML msgid/newmsgid/oldmsgid/svrid 回溯原消息；
  自己撤回不推
- 会话变化无独立 SSE 事件（内部同步触发）

### 2.5 qqflow-server 可复用清单
- 可照搬：server（axum 路由/鉴权三通道/错误信封/媒体直服防穿越）、sync（watch+防抖+兜底轮询、
  增量两段式 read-then-apply、SSE 基线/落后重发/15s ping）、config/logging、store 骨架
  （RwLock 单锁、惰性排序、单一 mediaId 可获取规则）、测试基建（假库夹具/roundtrip 仲裁/#[ignore] 真库探针）
- 必须重写：db/scan（微信目录布局）、解密层（微信 SQLCipher4 页密码，非 qq 的 1024B 头+VFS）、
  parser（消息列 + XML + zstd）、store/index 列映射（Msg_<md5> 表 + Name2Id 反查）、names 数据源、
  media 键（msg/attach/<md5> 目录）
- 红线全部保留：直读活库、全量内存索引、watch tick 零文件 IO、两段式同步防重复、spawn_blocking、
  统一错误信封、结构化优先解析、值驱动列探测、优雅退化不 panic、隐私检查 hook

## 3. 设计决议

### 3.1 模块树
```
weflow-server/
├─ Cargo.toml           # edition 2024；纯 Rust 加密栈（aes/pbkdf2/hmac/sha2/md5/rand/zstd）
│                       # rusqlite bundled（无 sqlcipher/openssl C 构建，Windows 仅需 MSVC）
├─ rust-toolchain.toml  # 1.97.1
├─ README.md / LICENSE(MIT) / docs/api.md / docs/architecture.md
├─ scripts/build.ps1 / build.sh
├─ src/
│  ├─ main.rs / lib.rs / config.rs / logging.rs
│  ├─ keystore/         # 64-hex 形状校验（4.x enc_key）、可选 keys 映射表、可选 img_code；仅内存
│  ├─ db/
│  │  ├─ mod.rs
│  │  ├─ scan.rs        # xwechat_files 账号发现 + db_storage 库枚举（含 -wal 标注）
│  │  ├─ wcdb.rs        # 页密码：encrypt_page/decrypt_page/verify_page1/decrypt_db/decrypt_wal
│  │  ├─ live.rs       # 活库只读连接池（qqflow 式；无明文落盘）
│  │  └─ open.rs        # rusqlite 打开/PRAGMA 探测辅助
│  ├─ parser/           # 消息行→结构体：show/lastMsgType/summary、XML 内容解析（zstd 解压）、
│  │                    #   媒体名提取、引用、撤回；结构化优先 + 启发式兜底
│  ├─ store/            # RwLock<Store>：sessions/convs/contacts/group_cards/names/watermark；
│  │                    #   query 快照排序（时间/关键词/分页/ChatLab）；Name2Id 反查
│  ├─ media/            # dat V1/V2/XOR 解密、silk→wav、ISAAC-64 视频（rand 0.5 移植）、导出
│  ├─ sync/             # watch（notify 监控 session/ 与 message/ 目录 + mtime 兜底轮询）
│  │                    #   + 增量游标 + 撤回扫描 + 事件广播（含回放缓冲）
│  └─ server/           # axum：health/accounts/messages/sessions/pull/contacts/group-members/
│                       #   media/*/push/messages（SSE）+ token 鉴权三通道
└─ tests/
   ├─ common/mod.rs     # 自建 WCDB 加密假库夹具（encrypt 侧 + 造数 + WAL 帧生成）
   ├─ wcdb_roundtrip.rs # 加密→解密→rusqlite 打开仲裁；错 key/WAL patch/截断文件
   ├─ api_smoke.rs      # tower oneshot HTTP 契约
   ├─ fs_watch_e2e.rs   # 文件事件→同步→SSE 真实 e2e
   ├─ real_db_groundtruth.rs  # #[ignore]：WEFLOW_TEST_DB_ROOT/KEY 环境变量开启
   └─ downstream_client.rs   # #[ignore]：下游客户端模拟
```

### 3.2 密钥方案（API 注册，不碰微信进程）
- `POST /api/v1/accounts`：`{wxid, key?, keys?: {rel_path: 64hex}, img_code?, db_path?}`
  - `key`：统一密钥，注册时对每个库做 page1 HMAC 验证，记录匹配/不匹配清单；
    `keys`：每库独立 enc_key 映射（微信 4.0 实为每库独立 enc_key + salt）；
    两者至少给一个；密钥仅内存保存，不落盘
  - `img_code`：可选，用于图片 dat V2 解密（xorKey/aesKey 派生）
- 校验逻辑即确定性 HMAC：无假阳性，错误 key 直接判 invalid

### 3.3 解密管线（4.x）
1. `db/wcdb.rs`：按 §2.1 实现页密码（decrypt_page / verify_page1 / decrypt_db / decrypt_wal_full）；
2. `db/live.rs`：对 db_storage 下各库持只读长连接（qqflow 式；注册时页1 HMAC 预校验），
   之后以 (db,wal) 指纹识别变更库，做水位增量查询（无整库重解密）。
3. `db/open.rs`：rusqlite 打开快照；`PRAGMA table_info` 值驱动探测列；缺列/坏库降级不崩溃；
4. 会话/索引构建为活库直读；查询路径不再依赖任何镜像中间层。

### 3.4 实时监控
- watch（notify）`<账号>/db_storage/session/` 与 `message/`（含 -wal 文件 mtime 变化——
  预分配 4MB 固定大小，不能看 size）；防抖 350ms；消息表事件 500ms 二次扫描；
  慢速兜底轮询（stat mtime，默认 30s，0 关闭）；
- 增量：时间窗口游标（create_time/sort_seq/local_id 水位，回退 2s），两段式 read-then-apply；
- 撤回扫描：窗口 150s + LIMIT 20 + XML msgid 回溯；自己撤回不推；
- SSE：ready → 回放 → message.new/revoke，25s ping；字段按 §2.3。

### 3.5 查询与索引
- sessions（Session 表 + 消息统计合并）、contacts（contact.db）、群成员（Name2Id + 群表）、
  消息按会话入内存；显示名优先级：备注 > 昵称 > wxid；群内：群名片 > 备注 > 昵称；
- queries：(ts,sort_seq,local_id)、keyword、日历边界、ChatLab 输出、media 导出标记。

## 4. CLI 与配置
`--port 5033 / --host 127.0.0.1 / --log info / --watch-debounce-ms 350 / --watch-fallback-ms 30000 /
--media-export-dir <data-dir>/api-media / --base-url`
数据目录：Windows `%LOCALAPPDATA%\weflow-server`、Linux `~/.local/share/weflow-server`、
macOS `~/Library/Application Support/weflow-server`；token 自动生成持久化 `token.txt`（日志只打路径）。

## 5. 测试策略
1. 单测：页密码（自编样本/错 key/截断）、keystore、parser（zstd XML）、media dat 编解码、名称优先级；
2. tests/common：WCDB 加密假库夹具（encrypt 侧实现 + 8 行标准集 + 假 WAL 帧）；
3. 集成：wcdb_roundtrip（仲裁：解密→SQLite 打开、WAL patch、错 key 必然失败）、api_smoke、fs_watch_e2e；
4. 真库探针 `#[ignore]`（`WEFLOW_TEST_DB_ROOT/KEY` 环境变量）——**本机无微信**，需要你后续提供
   微信 4.x 数据目录 + 密钥（或安装登录微信后运行）。

## 6. 风险与降级
- WCDB 参数以两份独立来源交叉验证过；仍有偏差余量 → roundtrip 假库仲裁 + 真库探针兜底；
- WAL 预分配 4MB：watch 必须监控 mtime（size 不变）；patch 校验 WAL 头 salt 防旧帧；
- 微信升级漂移：值驱动探测 + 优雅退化；
- 若未来需要自动取 key、或新版特征失效需要逆向（Weixin.exe/Weixin.dll），**届时我会请你启用
  IDA Pro**（本机 IDA MCP 当前未连接，WeFlow 安装包内也有闭源 wx_key.dll/wcdb_api.dll 可作分析对象）。

## 7. 里程碑
1. M1：cargo 骨架 + config/logging + keystore + WCDB 页密码 + roundtrip 测试 ✅
2. M2：scan + live + open + 全量索引 + names（假库夹具驱动）✅
3. M3：sync（watch/增量/撤回）+ SSE 推送 ✅
4. M4：server API 全端点 ✅
5. M5：media 解密导出 ✅（dat V1/V2/XOR + hardlink/media库定位 + messages media=1 集成；wxgf 转码器与 ISAAC-64 视频为 M5.7）
6. M6：文档、隐私检查、真库探针 ✅

## 8. 真库验证结果（2026-08-26，副本方式，原库零修改）
- 密钥真相：**微信 4.x 每库独立 enc_key**（`all_keys.json` 格式，内存中为 `x'<64hex key><32hex salt>'` 99 字节形态）；config/weflow-server.json 里的单一 `key` 只是旧回退值，**并非当前库密钥**
- 解密：**26/27 库页 1 HMAC 全部验证通过**（唯一失败 migrate/unspportmsg.db 无 key 条目，微信遗留 8KB 空壳），23 库 integrity=ok（fts 库为 python sqlite3 解析器差异）
- 索引：**315 会话（SessionTable）、312 会话方、216,206 条消息（483 张 Msg_<md5> 表）、4,526 联系人**；发送者显示名解析率 93.5%
- WAL 回放：带 4MB 预分配 WAL 的活跃库（session/contact/message_0 等）全部成功 patch
- 探针修复项：会话表实名为 `SessionTable`（非 Session）；`Name2Id` 表确认；增量/撤回/查询契约待微信在线状态复验（副本为冻结快照）
- 零修改证明：副本 174/174 字节一致；探测后原库 130/174 不变、44 个变化文件全部为运行中 Weixin.exe（08-22 起在线）在探测结束后持续写入（wal mtime 探测后仍在刷新）；我们全部工具仅只读
- 真库探针运行方式：`WEFLOW_TEST_DB_ROOT=<账号目录> WEFLOW_TEST_KEYS_JSON=<all_keys.json> cargo test --test real_db_groundtruth -- --ignored`