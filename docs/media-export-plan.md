# 媒体导出实现计划（media=1 导出管线，v1.5）

> 目标：对齐 WeFlow 的 `media=1` 行为——消息含媒体时，从微信本地缓存找到源文件、
> 解密/转换、复制到 `api-media/` 并由 `GET /api/v1/media/{talker}/{kind}/{file}` 直服。
> 现阶段基础：`src/media/mod.rs`（.dat V1/V2/V3 解密已实现并有单测）、
> `server/handlers/media.rs`（导出文件直服已实现、防穿越）、注册 API 已支持可选 `img_code`。

## 1. 源定位（已实测真库目录结构）

| 媒体 | 主路径 | 键来源 | 兜底 |
|---|---|---|---|
| 图片 image | `<账号>/msg/attach/<md5(sessionId)>/<yyyy-MM>/Img/<md5>.dat` | 消息 XML `<img md5=…/>`；`_t/_hd/.hd/_b` 变体为缩略图 | `hardlink.db` 图片硬链接解析；`FileStorage/Image*`（旧） |
| 语音 voice | 媒体库 `media_*.db`（silk 十六进制存储） | 消息 localId/svrId + 会话 | `wcdb_get_voice_data` 等价 SQL |
| 视频 video | `<账号>/msg/video/<yyyy-MM>/…` | `hardlink.db` 的 `video_hardlink_info_v4`（md5→path） | 3.x `FileStorage/Video` |
| 表情 emoji | `emoticon.db`（cdn_url）或 `msg/attach/…/Emoji` | XML `<emoji md5=…/>` | v1：cdn 直链即可，不落盘 |

- 已实测：`msg/attach/` 下按 32 位小写 hex 会话目录组织，全账号 **21,402 个 .dat**；
  `msg/video/` 按 `yyyy-MM` 分目录存在——真实数据可用于端到端验证。

## 2. 解密/转换
- `.dat` V1/V2/V3：`media::decrypt_dat`（已有）；V2 需 `img_code`（注册可选）；
  缺 img_code 时 V1 可用固定 key、V2 降级为“仅元数据”。
- 语音：媒体库原始 silk 字节 → `<svrId>.silk`；可选 `silk-rs` 解码 24kHz → `.wav`。
- 视频：先尝试明文（4.x 多为明文 mp4）；`07 08 56 32…` 头/密文时走
  ISAAC-64 密钥流（8 字节对齐生成后整段 reverse 取前 size；参考 WeFlow wasm 逻辑与
  isaac64 纯 TS 实现，Rust 内嵌实现 + 单测）。
- 表情：cdn 直链返回（不下载），本地有文件才落盘。

## 3. 导出管线（新模块 `src/media/export.rs`）
1. `resolve_media(kind, msg, store_ctx) -> Option<Source>`：按 §1 规则找源
   （`msg/attach/<md5(sessionId)>` 遍历 `yyyy-MM` 与文件名变体；hardlink 库查询走
   已解密镜像 `hardlink/hardlink.db` 快照）。
2. `export_media(source, out_dir, codec_ctx) -> Option<Vec<u8>/path>`：解密/转换 →
   写入 `<media_export_dir>/<talker>/<kind>/<file>`（临时文件+rename，幂等：同键同字节跳过）。
3. 返回值：URL = `base_url/api/v1/media/<talker>/<kind>/<file>` + 本地路径 localPath。
4. 并发：批量导出在 `spawn_blocking` 内、按 kind 限并发（默认 4），失败单个不阻塞整体。

## 4. API 集成（messages handler）
- `media=1/meiti=1` 触发；`image(tupian)/voice(vioce)/video/emoji` 子开关（WeFlow 对齐）。
- 消息对象 `media` 字段增强：
  `{type, fileName, md5, url, localPath, exported: bool}`；
  未导出成功时 url/localPath 为空（与当前“承诺即可取”原则一致：绝不 404）。
- `media` 顶层对象增加 `exportPath`（实际导出根）与 `count`（成功数）。

## 5. 测试与验证
1. 单测：ISAAC-64 向量（WeFlow 已知输出）、silk 头识别、hardlink 查询解析；
2. 集成（假库夹具）：
   - 构造 `msg/attach/<md5>/<date>/Img/<md5>.dat`（用 `media::encrypt` 合成的 V1/V2 样本）
     与对应消息行 → `media=1` 请求 → 断言 url/localPath 有效、直服端点 200 且字节一致、幂等跳过；
   - 缺 img_code 时 V2 降级；防穿越回归。
3. 真库在线验证（本机环境）：
   - 对真实图片消息 `media=1` → 导出 → `GET /api/v1/media/…` → JPEG/PNG 魔数校验；
   - 语音/视频各取 1 条实测；21,402 个 .dat 提供充分样本。

## 6. 里程碑
- M5.1 接入现有 dat 模块到导出管线（图片直服，最高优先级，样本丰富）
- M5.2 msg/attach + hardlink 解析与文件名变体匹配
- M5.3 语音：媒体库提取 + silk/wav
- M5.4 视频：hardlink + ISAAC-64 + 明文直通
- M5.5 messages/media API 集成 + 子开关 + 幂等
- M5.6 真库在线 e2e 与文档更新

## 7. 风险与降级
- 微信版本变动导致路径/命名漂移 → 值驱动探测 + hardlink 库为主源（WeFlow 亦如此）；
- 无 img_code 时 V2 图片不可解 → 元数据降级并提示注册 img_code；
- 大文件/大量导出 → 并发限制 + 幂等跳过 + 磁盘空间检查。
---

## 实施结果（2026-08-26，v1.5 落地）
- ✅ M5.1/M5.2 图片：`hardlink.db` 解析 + attach 定位 + dat V1/V2/legacy 解密；注册支持 `img_aes_key`+`img_xor_key`（或 `img_code` 派生）
- ✅ **wxgf 破解（无需 IDA）**：参考 wechat-decrypt 确认 `wxgf`=HEVC 裸流；导出时经 ffmpeg 自动转 PNG（查找顺序：`WEFLOW_SERVER_FFMPEG` env → WeFlow 自带 ffmpeg-static → PATH），无 ffmpeg 时保留 `.wxgf` 原样
- ✅ M5.3 语音：`VoiceInfo(svr_id)` → 去状态头 `.silk`
- ✅ M5.5 messages 集成：`media=1` + 子开关、幂等、200 条上限、单条失败不阻塞
- ⏳ M5.4 视频：明文 mp4 直通已实现并有单测；ISAAC-64 加密流暂无本地样本，待遇到再实现
- ⏳ 表情：cdn 外链已返回；本地 Emoji 目录落盘未做
- 前置修复：消息内容为 zstd 帧（含 `sender:\n` 前缀）——parser 字节层解压后媒体元数据才可解析

真库验证：扫描真实会话导出 **19 张 PNG（wxgf 转码，最大 2549×1403）+ 5 张 JPEG**，
回读 HTTP 200 且魔数校验通过；语音 silk 与 XML 声明长度一致。