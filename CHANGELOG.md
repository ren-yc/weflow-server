# Changelog

本文件从 0.5.0 起维护。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [未发布]

### 安全

- **修复 SNS 导出的路径遍历（任意文件写入）**。`GET|POST /api/v1/sns/export` 的产物名原为
  `format!("sns-{scope}-{stamp}.{format}")`，其中 `scope` 是原始 `username` 请求参数——该参数
  只用作动态过滤条件，从不与 store 校验（传未知值得到空导出而非 404），因此以自由文本形式
  抵达 `exports_dir.join()` + `std::fs::write`。`sns-` 前缀**不构成防护**：Win32 会剥掉路径
  分量末尾的点，`sns-..` 因而规范化成一个名为 `sns-` 的普通目录，剩余载荷继续上穿；
  Windows 的规范化还是词法的、在用户态完成，中间目录无需存在即可折叠 `..`（同一载荷在
  Linux 上会 `ENOENT`）。扩展名由 `format` 控制、内容由 feed 控制，故为写原语。已实测确认。
  修复：`username` 先经 `pathsafe::slugify` 折叠，再对规范化后的根断言包含关系。
- **修复媒体代理的 SSRF（主机白名单可绕过）**。两处独立缺陷：`host_matches` 剥 userinfo 时
  对 `@` 做正向切分、取前段，而 URL 中 userinfo 在前，于是 `https://qq.com@evil.example/`
  递给白名单的是 `qq.com`、curl 连的是 `evil.example`；authority 又只在 `/` 处结束，`?x=.qq.com`
  同样可混过。此外 `curl -L` 由 curl 自行跟随跳转，发生在那次一锤子检查**之后**——白名单内
  任一开放重定向（`qq.com` 面积很大）即可将该端点变为任意目标抓取器，含回环与
  `169.254.169.254`。修复：`check_proxy_url` 同时作用于调用方 URL 与**每一跳**重定向目标
  （故去掉 `-L`、自行接管重定向循环，上限 5 跳）；`url_host` 改为从右侧切分 userinfo，并在
  `/`、`?`、`#` 三者最先出现处结束 authority。
- **导出写入方补齐包含性校验**。`media::export::write_out` 原先对 `talker` / `file_name`
  两个路径分量零校验，而 `file_name` 派生自消息自带的 `md5` XML 属性、`parser::attr` 亦零校验，
  即**发送方可控**。此前拦住它的是两处巧合（图片路径的 md5 完整性校验无法与非摘要字符串相等；
  `walk_find` 比对 `file_name()`，其中永不含分隔符），且无任何地方记录这两条性质是承重的。
- 新增 `pathsafe` 模块，收敛为全仓唯一的路径分量语义（`safe_segment` / `slugify` /
  `is_contained`），四处拼路径的调用点（SNS 导出、媒体路由、注销清理、导出写入方）全部改走它。
  原先四者各自漂移出不同的检查子集，均未覆盖末尾点、`:`（NTFS 备用数据流 `name.jpg:hidden`，
  压根不含分隔符）与控制字符。规则是**先派生名字，再对规范化根断言包含关系**——仅过滤输入
  正是 SNS 导出得以逃逸的原因。`is_contained` 失败时关闭，不退化为拿非规范化根做词法比较
  （verbatim 前缀与裸路径混比会静默永不匹配）。

### 修复

- 媒体路由的 `canonicalize` 与代理的 curl 子进程原先在 tokio worker 上做阻塞 IO，并发媒体读取
  会拖住无关请求（含 SSE 心跳），改入 `spawn_blocking`。
- 媒体代理原先把响应体写到共享临时目录下一个以纳秒时间戳命名的可预测路径，改为经 stdout
  管道回传、元数据走 stderr 带标签行（二进制载荷不可能被误读为状态行）。
- 媒体代理硬编码 `curl.exe`，与其"Windows/macOS/linux 都自带"的注释矛盾，改为按平台取二进制名。

### 说明

- 已知残留：白名单后缀下的主机名若**解析到**内网地址仍可通过。堵住它需把 DNS 解析放到能在
  连接前检查结果的位置，属抓取方式的实质改动而非校验层微调，已在 `proxy_fetch` 处注明。
- SNS 导出的产物**文件名**形状随之变化：`username` 中的非 `[A-Za-z0-9-_]` 字节折叠为 `_`
  （如 `12345678@chatroom` → `sns-12345678_chatroom-<stamp>.json`）。响应内容与导出语义不变。
- 遍历写入位于鉴权之后（`require_auth` 守着该端点）。若该 token 是共享的或强度不足，应按
  远程任意写入对待。

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
