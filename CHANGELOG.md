# Changelog

本文件从 0.5.0 起维护。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [0.5.0] - 2026-08-28

账号面（注册 / 健康检查 / 注销）重新设计。**不兼容 0.4.x**：`/health` 响应形状变更，
下游需同步改造（见 `docs/weflow-server-api.md`）。

### 新增

- `GET /api/v1/accounts`（需鉴权）：返回账号明细
  `{wxid, state, message_count, error?, db_storage}`，含启动扫描发现但未注册的账号
  （`awaiting_key`）。**不受就绪门控**——账号 `indexing` 时正是客户端要轮询它的时候。
- `DELETE /api/v1/accounts/{wxid}`（需鉴权，别名 `POST /api/v1/accounts/{wxid}/deregister`）：
  注销账号，停止其 sync、清空索引，服务器回到未注册状态。可选 `purge_media=1` 清理该账号
  导出的媒体目录（**默认 false**）。判定 `deregistered` / `not_registered` / `wxid_mismatch`
  一律 HTTP 200。

### 变更

- **强制单账号**：同时只允许一个账号处于绑定态。注册第二个 `wxid` 返回
  `state=account_conflict`（HTTP 200，附 `occupied_by`/`occupied_status`），**在校验路径与
  密钥之前**即判定，且在位账号完全不受影响；换账号需显式注销。`error` 态账号仍持有绑定，
  一次解密失败不会把服务器交给别的账号；重新注册同一 `wxid` 即可重建自愈。
- `/health` 与 `/api/v1/health`（免鉴权）改为标量：`{status, version, account}`，
  `account` ∈ `unregistered | indexing | ready | error`。**不再列出账号数组**——该端点免鉴权，
  而启动扫描会为本机每个 `xwechat_files` 账号目录建条目，数组（乃至其长度）本身即泄露本机
  存在哪些账号、各自进度如何。账号身份、消息数、库路径与错误原因移至需鉴权的
  `GET /api/v1/accounts`。
- `AccountSync` 新增退场标志：注销后进行中的增量轮询会丢弃本轮读取结果而非写入已清空的
  store，也不再发事件（总线是进程级的）；索引中途注销不会在构建完成后复活账号。
- `register_account` 返回 `BindOutcome`（`Bound`/`Existing`/`Occupied`），守卫与插入共享同一次
  registry 锁——分两次取锁会让两个并发注册都看到空闲绑定。`start_account` 原样透传该判定。
- `/health` 的就绪判定改由 `AppState::account_phase` 单独提供（不取 store 读锁、不看发现结果）；
  `account_views` 退化为纯明细（只返回列表），不再兼职算就绪标志。
- `/api/v1/contacts` 的 `nickname` / `remark` / `alias` / `avatarUrl` 缺值时下发空字符串而非
  `null`。这是全仓最后一处直接序列化 `Option` 的地方——`/api/v1/group-members` 的同名四字段、
  以及 `/api/v1/messages` 与 ChatLab Pull 的 `groupNickname` / `avatar`（键名不同、来源同为
  联系人行）早已一律 `unwrap_or_default()`。`store::Contact`
  内部仍用 `Option<String>`——`display_name()` 要靠它区分「无 remark」与「remark 为空串」来做
  `remark > nickname > username` 回退，只有 JSON 边界拍平。
- 导出媒体 404 改用统一错误信封 `{success, code, message}`，不再返回
  `{"error": "Media not found"}`。此前同一个端点有两种 404 形状：路径不存在走裸 `error` 键，
  路径存在但打不开走信封。
- 启动扫描日志只报数量，不再打印发现的 wxid 清单。`/health` 与 `account_views` 为「免鉴权
  不得枚举账号」付了类型级代价（`AccountPhase` 没有 `AwaitingKey` 变体），日志打印完整清单
  会把这层设计绕过去；清单仍由需鉴权的 `GET /api/v1/accounts` 提供。
- 索引期间被注销的构建日志从 DEBUG 升为 INFO，并区分「索引已完成 / 索引失败 / 索引任务异常」
  三种结局。默认 `info` 级别下这是「注销一个 `indexing` 账号」在日志里的唯一痕迹，此前不可见。

### 说明

- 注销**不清空** SSE 重放历史：历史是进程级的，其 `id` 是总线级单调序列，清空会破坏与该账号
  无关的订阅者的 Last-Event-ID 重放。改为重新广播剩余就绪账号的水位基线（单账号下即空数组）。
- 注销后启动扫描发现过的账号回到 `awaiting_key` 并保留路径（它确实还在本机上），
  纯客户端注册的账号则整条消失。
- `purge_media` 默认关闭：导出布局是 `<talker>/<kind>/<file>`，没有账号维度，两个账号与同一
  talker 的会话共用一个目录，清理可能删掉另一账号导出的文件。清理范围严格限定在导出器写入的
  四类子目录，talker 目录本身仅用 `remove_dir`（非空即保留），**永不递归删除导出根**。
