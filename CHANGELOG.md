# Changelog

本文件从 0.5.0 起维护。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [0.9.0] - 2026-10-07

### 变更

- **拉取面新增 `messages[].mediaId`（可选键，出现即可取）**：带媒体的行在**此刻确有可取字节**时给出
  媒体获取键（导出后的内容摘要名，即 `GET /api/v1/media/{id}` 的入参），**不可取时整个键省略**（不是 `null`、不是空串）。
  「出现即可取」是承诺：判据与 SSE **同一条**（本会话导出目录下确有该文件、且名字由内容摘要派生）。
  键的位置在**消息这一层**、不在 `media` 对象里：`media` 的键集仍钉为 `{type, fileName, md5}`
  （`media_shape_in_pull` 拒绝多余键），而且 `fileName` 说「这条媒体叫什么」、`mediaId` 说
  「这份字节现在取得到」——两件事合成一个键就会混谈。正反两侧都有回归：
  `pull_advertises_media_id_only_for_exported_digest_files`（落盘⇒按摘要干命中、句柄给实际落盘名；导出根为空⇒整键消失）。
- **`export --with-media` 的媒体获取改为按行分派（快路径＋慢路径）**：拉取面已给出 `mediaId` 的行
  **直接按句柄取字节，一发导出请求都不发**；只有有 `media` 却还没有句柄的那些行，才把**它们的时间窗**
  交给消息面（`/chatlab/messages?media=1`）触发按需导出。效果：`--resume` 与重复导出近乎零成本（已导出会话零导出请求）；窗口滑动静默缺件那一类问题不受影响，按页窗口的测试原样保留。
  回归位置：`media_id_from_pull_row_skips_the_export_round`（三行里只有没句柄的那行需要导出 ⇒
  恰好一发导出请求，且窗口只覆盖那一行的时间戳）。

- **`clients/rust`（`weflow-client`）与 `clients/python`（`weflow-sdk`）的 `media_bytes` 增加 `talker` 参数（破坏性）**：
  404 重试用的导出门需要**会话 id**，而此前用的是 `message.account_name`——那是发信人显示名，
  私聊里它是对面昵称、群聊里是发信人昵称，与会话 id 只在「显示名恰好没被改过」时相同，真实数据几乎必然对不上。**迁移方式**：
  `media_bytes(&message)` 改为 `media_bytes(&message, talker)`（Rust）/ `media_bytes(message, talker)`（Python），
  `talker` 用发起导出时传给 ChatLab 面的同一个会话 id。`media_bytes_by_id(id)` 不受影响。
  空 `talker` 现在两语言都**本地拒绝**（Rust 报 `UnexpectedBody`，Python 报 `ShapeError`）——
  此前 Rust 会把空串发给导出端点，把「参数无效」伪装成「句柄不可导出」。回归位置：
  `tests/behavior.rs::media_bytes_exports_then_retries_once_after_404`（夹具昵称 ≠ 会话 id，
  mock 拒绝错误 talker）与 `tests/cli_e2e.rs::with_media_skips_an_unfetchable_handle_without_failing`。
  消息无媒体句柄时的错误也从「合成 404 + 句子塞 url 字段」改为 `UnexpectedBody`/`ShapeError`——
  依赖旧错误形态分类的调用方需要调整。

- **MCP `get_messages` 响应新增 `sinceResolved`**：`since` 接受相对串（`7d`/`24h`），续拉发生在
  下一轮对话——重发相对串会把窗口悄悄前移。响应回给本轮解析出的**排他**绝对下界，续拉传
  `nextOffset` + `sinceResolved`；`since` 未提供时为 `null`。
- **CLI `contacts` 子命令分页化**：带 `--limit`/`--offset`，输出从裸数组改为
  `{total, hasMore, contacts}`（此前固定取第一页且不报总数）。
- **CLI 用法错误口径统一**：显式空 `--talker`、带空白的 `--since`、嵌入分支的同类输入，
  一律以用法错误退出（此前部分形态静默返回空结果退出 0）；`--since` 解析返回 trim 后的值。
- **TS 客户端 `watch` 的干净 EOF 改为重连**：服务端优雅关停/空闲重启不再静默终止跟随
  （与 qqflow 同构）。
- **发布流水线新增质量门**：release 前在同 tag 重跑 clippy、全量测试与契约 nails，
  构建依赖该门——测试红着打 tag 会被拒绝。

- **两个 SDK 的请求超时改为分层默认（破坏性；公开常量 `CONNECT_TIMEOUT`／`READ_TIMEOUT`／`STALL_TIMEOUT`）**：
  连接一律 **5s**；普通 JSON 请求**每次读**最多等 **30s**；`group_members(..., include_message_counts=True)`
  （整名册计数＝全会话扫描）、`media_bytes`／`media_bytes_by_id`（体积由发送方决定）、`sync_now()`
  （索引＋可能导出媒体）与 `watch()`（长连接）走**停滞上界 90s**。两个上界都按每次读操作计时、有进展即复位，
  **不是总时限**——「慢但在动」的大响应不会被判失败，而彻底静默的连接（对端被 kill 且没有 FIN、NAT 映射过期、
  笔记本睡眠唤醒）会报错而不是永久挂住。服务端每 25s 发一次 keep-alive ping，90s ≈ 四倍余量：丢一次心跳加一次
  调度抖动仍算健康，黑洞则会被发现。此前 Python 是「所有请求一律 30s」、Rust 是「所有请求一律无限制」，两边都
  不对：前者把「这个群很大」变成客户端错误、并把空闲 SSE 在 30s 掐断（心跳间隔 25s，只剩 5s 余量），后者让黑洞
  连接永不被发现。**迁移方式**：Python 的 `Client(..., timeout=)` 数值语义不变，但作用域从「所有请求」收窄为
  普通 JSON 请求（这是破坏性的一半），豁免族改由新增的 `stall_timeout=` 设界；Rust 用
  `Client::with_timeouts(base_url, token, connect, read, stall)`，`Client::new` 走公开默认；要彻底关掉停滞
  检测就传一个足够大的 `stall`。回归位置：`each_request_family_uses_its_own_time_budget`（Rust：驱动一个
  600ms 才回响应头的服务端，证明两个上界各自生效，退回任一路由即变红）、
  `test_published_timeout_budgets_travel_per_request` 与
  `test_watch_stream_is_not_bounded_by_the_json_read_timeout`（Python：断言 transport 实际收到的
  per-request 值）、`published_timeouts_match_the_documented_budgets`（三个常量的数值与大小关系）。

- **`wait_ready`／`ensure_ready` 把瞬时故障算作「还在等」，并把丢掉的轮次记进最终报错**（行为变化）：
  轮询账号列表时遇到连接失败或 5xx 不再中止整个等待。此前一次抖动就把「索引仍在建」变成客户端错误，与该函数自己的
  文档（只有 deadline 与账号 `error` 态失败）矛盾。**4xx 仍然立即失败**——那是配置错误（token 不对、路径写错），
  把它等满预算只会把可当场定位的问题拖成一次超时。但「等待」不等于「失忆」：一直连不上或一直 5xx 时，超时若仍报
  `not-registered`，就把矛头错误指向注册那一步，而真正的现象是服务端从头到尾没答过。现最终消息带上最后一次观察
  与丢掉的轮数（`…; N poll(s) never got through`）。回归位置：
  `wait_ready_rides_out_a_transient_5xx_but_not_a_4xx`（两语言两仓）。

- **httpx 解析不出 URL 的错误进入 `ClientError` 树**：`httpx.InvalidURL` 不属于 `httpx.HTTPError`
  家族，此前畸形 `base_url`（端口非法、主机含控制字符、残缺 IPv6 字面量）会裸逃过 `except ClientError`——
  调用方接住了服务端拒绝，却接不住自己写错的地址。GET／POST 两个入口包成 `TransportError`；SSE 入口**单独
  raise 后仍然上抛**（那里吞掉它等于把配置错误变成一条永不产出、也永不报错的流，退避还会封顶 30s 无限重试）。
  回归位置：`test_malformed_base_url_stays_inside_the_client_error_tree`。

- **`StatusError.url` 恒等于请求 URL**（破坏性，仅影响按 url 归因的调用方）：200 拒绝态此前把
  `state=…` 拼进 url 字段，于是按前缀/精确匹配分类的调用方会静默错分。拒绝态的 `state` 改由新增的 `detail`
  字段承载，`str(exc)` 仍然包含它。**迁移方式**：`StatusError` 此前从来没有 `state` 属性，拒绝态信息只藏在
  url 里，因此只有「从 url 字符串里解析 `state=`」的调用方需要改读 `exc.detail`；只读 `exc.status`／
  `exc.url` 的不受影响（url 现在更干净）。Rust 侧同类修正：媒体前置条件错误此前把散文塞进 `url`
  （`"(no media on message)"`、`"(empty talker)"`），现改为这条调用所属的媒体端点族（缺的正是句柄本身，
  此刻没有 id 段可填），原因写在 `detail`。回归位置：`test_status_error_url_stays_the_request_url`、
  `media_precondition_errors_name_the_real_endpoint_not_prose`。

- **媒体句柄的空值本地拒绝**：`media_bytes` 对空 `file_name`、`media_bytes_by_id` 对空
  `media_id` 现在不发请求直接报错（两侧都算上纯空白，与 Rust 的 `.trim().is_empty()` 同口径）。空句柄打到的
  是另一个路径，服务端答「查无此文件」，于是「调用方没给名字」被伪装成「这个句柄不可导出」。两侧的空白口径
  也在此统一（Rust 用 `.trim().is_empty()`、Python 用 `.strip()`）；`chatroomId` 一族的既有校验保持
  「只挡空串」不变——那是 Pull 面的既有承诺，不属本条。回归位置：
  `media_precondition_errors_name_the_real_endpoint_not_prose`（含「本地拒绝不得发出任何请求」的计数断言）与
  `test_media_bytes_reject_an_empty_handle_without_a_request`（含纯空白句柄）。

- **朋友圈 JSON 导出不带取图凭据（破坏性）**：媒体的 `token`／`key`（微信侧的访问与解密参数），
  以及两个同值冗余键 `rawThumb`／`resolvedThumbUrl`（与 `thumb`/`resolvedThumbUrl` 是同一个原始地址）
  已从导出物移除。导出文件常被转发、存档或贴进别处的文件，留着凭据＝每个导出包多带一份第三方可以
  直接使用的材料；而代理端点只读 `url`，移除**不影响任何取图路径**。
  **迁移方式**：需要凭据的调用方改走 `/api/v1/sns/media/proxy?url=…`（代理路径已在 `url`／`proxyUrl`
  给出）。`rawUrl` **刻意保留**：它是协议白名单的审计线索（HTML 渲染器不读它，恶意 scheme 因此进不了
  `href`，但读者要能在导出物里看见被拒的原始地址是什么）。回归位置：`sns_json_export_drops_credential_media_keys`。

- **消息表缺时间列时报错，不再静默返回空增量**（破坏性，仅影响异常列形态）：`read_new` 此前对
  「有 `local_id` 但没有时间列」的表返回 `Ok(空)`，而空 Vec 与「真的没有新行」在调用方完全不可区分
  ⇒ 这张表的增量**永久静默为空**（水位照记、页面照答，谁也不会回头查它）。同口径的「无 `local_id`
  列」本来就是 `Err`——两者行为不一致本身就是线索。现改为报错并点名表与成因。
  **影响面如实说明**：该错误在 `AccountSync::poll_once` 经 `?` 上抛，会中止本轮增量（与无 `local_id`
  列的既有行为相同）；初始建索引那边仍是逐表 `warn + skip`。真库里是否存在这种列形态属观察项。
  回归位置：`no_time_column_is_an_error_not_an_empty_increment`（配同表补上时间列即可读的正向对照）。

- **子集导出不再静默覆盖别的会话已交付的产物**（**破坏性**：原本能静默覆盖并退 0 的工作流，现在会拒绝并退 1）：`export --session` 的
  **非续跑轮**此前把文件名去重集合从空开始、编号按本轮输入重算，于是本轮会话能算出与某个**未在本轮**的
  会话已交付产物同名的文件名，`.part` 收尾 rename 直接把它盖掉；而新一轮 `index.json` 又没有那个会话的
  条目 ⇒ 交付物被换掉、清单不再提它、下一轮也无从自愈。现于 rename **之前**按**归属**判定并拒绝：既有
  产物登记在别的会话名下、或没有被任何一轮清单认领时，该会话记入 `skipped`（起手前就拒，不发请求、
  不留 `.part`），`skipped` 非空 ⇒ CLI 以 1 退出（沿用既有退出码口径，不新增码）。**同会话的有意重导
  不算覆盖**——它覆盖的是自己的旧产物；判据用归属区分这两种情形，若一并拒绝，「重跑同一个会话」就变成
  必须先手工删文件。**被保护的属主那一行会带进本轮新清单**：否则本轮收尾只写本轮交付的条目，属主的产物
  就从清单上消失 ⇒ 属主下次重导时，自己的同名文件变成「无主」、被同一个拒绝机制挡住——拒绝机制自造死锁。
  **边界（实测，别当成万能保险）**：这层拒绝只在非续跑轮需要，也只在**清单可读**时判得出归属。清单完好
  时 `--resume` 轮会把上一轮的名字先播种进去重集合，撞名根本算不出来（只会拿到 `-2` 后缀），因此那条路径
  本就不需要拒绝；而清单丢失时「本轮无完成记录 ⇒ 重写」是既有且**被需要**的自愈语义，此时无法区分「自己
  的产物丢了记录」与「别人的产物」（文件头归属校验的方案早已被否决）。边界回归位置：
  `resume_with_intact_index_avoids_the_collision_entirely`。
  **迁移方式**：① 有意重跑同一个会话——无需改动；② 确实要用本轮数据替换别人的产物——先删除该文件、
  或换一个输出目录；③ 想复用上一轮的完整交付——改用 `--resume`（四道判据：清单、残留 `.part`、
  媒体承诺、内容完整性）。
  回归位置：`subset_export_refuses_to_clobber_another_sessions_artifact`、
  `intentional_rerun_of_same_session_overwrites_itself`、`unowned_leftover_file_is_not_clobbered`、
  `refusing_a_collision_keeps_the_owner_exportable`（撤掉「带属主进清单」那一步实测变红）。
### 新增

- **两个 SDK 各补五项公共面（Rust 与 Python 同名同义）**：`sync_now()`（手动触发一次增量同步）、`pull_page(talker, since, offset, limit)`（**单页** Pull 入口，`drain_session` 改为复用它 ⇒ 游标装配从两处回到一处）、`chatlab_messages(...)`（ChatLab 形状的消息面，此前该面只被内部当触发导出用、没有公共入口）、`group_members(chatroom_id, include_message_counts)`、`list_all_sessions` 的关键词与页大小。
  回归位置：`sync_now_posts_with_the_bearer_token_and_decodes_counters`、`pull_page_decodes_the_sync_block_and_sends_the_cursors`、`pull_page_omits_defaulted_cursors_instead_of_sending_zero`、`chatlab_messages_decodes_the_chatlab_envelope_and_paging`、`group_members_decodes_roster_page_and_sends_chatroom_param`、`list_all_sessions_pages_and_collapses_cross_page_duplicates`。
  行为变化：`drain_session` 的请求序列不变；Python 侧原先从第二页起发 `offset=0`，现与 Rust 一致地省略（服务端默认值即 0，取到的页相同）。
  **Rust 侧的 `list_all_sessions` 此前只有页大小、没有关键词**（提交信息声称有，实现里没有）——调用方只能取回全量再本地过滤，而该面**以空页为终止条件**，过滤会缩短某一页、排在后面的命中项永远读不到。现已修平。

- **CLI 子命令面（`cli` feature，已进 `default`）**：`serve`／`token`／`sessions`／`messages`／`search`／`contacts`／`sync`／`export`。**裸跑仍等于 `serve`**（默认动作，既有启动方式不变）；退出码 `0/1/2` 的用法错误口径由 `tests/cli.rs` 逐例钉住。`--embedded` 只给只读查询类子命令——写动作与账号类没有该字段。
  **迁移方式**：`run_cli()` 由 `async fn` 改为同步分流入口（`main.rs` 随之不再自建运行时）。
- **`export` 子命令**：ChatLab Format 批量落盘，带 `--format json|jsonl`、`--session` 多选、`--resume` 续跑、`--limit`／`--since`／`--end`。`--with-media` 先经消息面触发服务端导出、再取字节落盘到 `<out>/media/`，**导出物里的 `media.fileName` 只保留确实落盘的句柄**（外链、未导出、导出失败的一律不写该字段——宁可少一个字段，也不给一个指向不存在文件的句柄）。回归位置：`with_media_keeps_only_handles_whose_bytes_are_on_disk`、`message_line_omits_media_without_a_handle`、`export_with_media_lands_bytes_and_keeps_the_handle`。导出物不写任何 URL；每行写盘前做令牌子串检查，命中即整轮中止并删除半成品。
- **MCP 工具面（`mcp` feature，已进 `default`）**：`mcp` 子命令在 stdio 上暴露 8 个**只读**查询工具（`list_sessions`／`get_messages`／`get_messages_raw`／`search_messages`／`get_contacts`／`get_media`／`group_members`／`sync_now`）。单次输出约 32 KB 字符预算，超预算少给条数并置 `truncated`；「数据离机」提示写进 instructions、每个工具的 description、README 顶部与新增的 `docs/mcp.md`（模型看不到 README）。依赖 `rmcp`／`schemars` 均为可选 ⇒ `--no-default-features` 的零 tokio 嵌入契约不受影响。
- **造库器移入库内（`testing` feature）**：批量导出的夹具要能真实生成会话库供 `tests/` 复用，此前只能靠测试内联的临时构造（夹具只能造库、不能造索引）。

### 修复

- **`export --with-media` 改为单遍：媒体导出窗口跟着 Pull 页走（性能与正确性）**：过去一个会话要走**两趟全历史遍历**——
  一趟用消息面（`/chatlab/messages?media=1`）翻页收集句柄并下载字节，另一趟用拉取面翻页写行。三处代价：
  ① `--since` 只管住了写出来的行，媒体那趟**根本不带这个时间窗**（消息面那个面收 `start`/`end`，CLI 此前没传），
  于是「只导最近一个月」仍会把整个会话的媒体导出并重下一遍；② 两趟之间一旦有并发同步推进水位，第一趟没覆盖到
  的消息会「有行、无句柄」地静默缺件，且没有任何计数说得出少了件；③ 句柄映射按整会话驻留内存，与 JSONL 那条
  「峰值常驻集与消息条数无关」的承诺相对抗。现在每取到一页 Pull 行，就用**这一页的时间窗**（`(since, nextSince]`
  正是消息面认的 `start`/`end`；同秒的行必然落在同一页，故窗口两端不漏行）导出一页、取字节、再写这一页的行：
  两趟并成一趟，`--since` 同时下推到导出面，映射只活在一页内。另加**磁盘存在性即去重**：已有摘要派生名的文件
  不重下（同名即同内容），这也是 `--resume` 能「只补下缺件」的依据。**迁移方式**：命令行与产物形状都不变，
  变的是成本与完整性：带 `--since` 时导出被限制在窗口内（导出量与相应请求数随之下降）；不带 `--since` 时消息面
  请求数与过去相当（仍是每请求 200 项分页），但**跨轮次不再重下**已在盘上的摘要文件、句柄映射从「整会话驻留」
  改为逐页释放，且两趟遍历之间窗口滑动导致的静默丢件整类消失。

  **后续演进（本节上方的按行分派条）**：窗口的定义已从「整页 `(since, nextSince]`」
  收窄为「**缺句柄那些行**的时间戳 `[min, max]`」——拉取面给出 `mediaId` 后，已有句柄的行不再
  参与导出请求，窗口只覆盖真正需要导出的行。
  回归位置：`media_window_follows_the_pull_page`（每个导出请求都带 `start`/`end`、请求条数等于 Pull 页数、两页窗口
  互不重叠、250 条句柄与 `media/` 文件集合大小相等）与 `with_media_reuses_bytes_already_on_disk`（mtime 不变＝没重下）。

- **MCP：预算截断时 `hasMore` 必须为真**。`get_messages`／`search_messages`／`get_contacts` 此前透出**页面自身的** `hasMore`，于是 `truncated: true` 与 `hasMore: false` 会同时出现——按 `hasMore` 判停的调用方会**静默停在不完整结果上**。现改为 `has_more || truncated`。回归位置：`mcp_truncation_reports_has_more_so_the_caller_does_not_stop`。
- **MCP：`search_messages` 的续拉游标此前无处回传**。响应给出 ChatLab 的 `nextCursor`，但该工具的参数里既无 cursor 也无 offset ⇒ **第 2 页永远取不到**。现改为 `offset` 入参 ＋ 响应给 `nextOffset`（本页是连续切片，该值恰指向被砍掉的第一条），并**移除**那个回传不了的 `nextCursor`；`get_contacts`／`get_messages` 在预算截断时同步补 `nextOffset`（此前两个游标都置 null，纯按字段续拉的调用方会永远重取同一页）。回归位置：`mcp_search_pagination_actually_advances`。
- **CLI：非法 `--since` 现在以用法错误退 2**。此前 clap 放行、手工解析再 anyhow 上抛退 1，而 `--limit abc` 走 value_parser 退 2——同一种「用法写错」两种退出码。回归位置：`invalid_since_is_a_usage_error_exit_2`。
- **CLI：取媒体字节只有 404 才算「句柄不可取」**。此前任何错误都被当成不可取而跳过，瞬时 5xx／网络错会**静默少下载媒体而整体仍退 0**。现只有 `Status { status: 404, .. }` 跳过、其余上抛。回归位置：`with_media_fails_loudly_when_bytes_fetch_errors`。
- **`export --resume` 要真是续跑**：① `begin` 建了文件后首行检查失败不回收 ⇒ 留下的空文件让 `--resume` 永久跳过该会话；② `--resume` 只看 `exists()` ⇒ 截断／空文件被当成已完成（现加 `file_is_complete`：空文件不算，jsonl 末字节须为换行）；③ 续跑沿用上一轮的文件名（编号按输入列表顺序算，列表一变就漂移出第二份产物）；④ 被跳过的会话不再进新清单（否则这次的 `index.json` 会把上一轮条目整个抹掉，全命中时变成空清单）；⑤ jsonl 收尾的 `flush().ok()` 吞错 ⇒ 磁盘满时留下截断产物却以成功收场（现 `flush()?`）。回归位置：`secret_check_covers_escaped_and_percent_encoded_forms` 所在的导出回归套件与 `tests/cli_e2e.rs`。
- **秘密检查覆盖转义与编码形态**：此前只比原文，秘密以 JSON 转义形（`a\"b`）或百分号编码形落盘时会逃逸。现同时比原文、转义形、编码形。
- **pathsafe：挡掉 Win32 保留设备名**。`safe_segment` 与 `slugify` 都不挡 `CON`／`NUL`／`COM1`…（大小写不敏感、**带扩展名也算**）。作末分量时 Win32 在触碰文件系统之前就把名字解析成设备：写 `NUL` **静默丢弃字节**、开 `COM1` 可能阻塞；而名字来自聊天库、由发送方可选。现 `safe_segment` 拒绝、`slugify` 加前导下划线（`CON` → `_CON`）。
- **两个 SDK 的错误族与参数校验归一**：① Python 侧 httpx 异常族此前不被包装 ⇒ `except ClientError` 接得住服务端拒绝却**漏掉网络故障**，现新增 `TransportError(ClientError)`；② `_decode` 只把 `>= 400` 当错 ⇒ 3xx 落进 `resp.json()` 变成 `ShapeError`（Rust 是「非 2xx 即错」），现按 `not 200 <= status < 300`；③ 时间界校验用 `str.isdigit()` **会放行全角数字** ⇒ 漏到服务端吃 400（Rust 本地拒绝），现改 ASCII-only；④ `group_members` 不校验空 `chatroomId` ⇒ 空名册会被读成「这个群没有成员」，现 fail-fast；⑤ Rust 的 `MessageQuery::params()` 把错误 URL 写死为 `/api/v1/messages`，经 `chatlab_messages` 调用时报错**指向另一个端点**，现由调用方传端点；⑥ `pull_page` 与两处 media 路径直拼 id ⇒ 含 `#`／`?` 时打到别的路径（Python 侧同样如此，httpx 不编码已拼好的 path），两侧都加路径段百分号编码。回归位置：`encode_path_segment_escapes_delimiters_but_keeps_real_id_shapes`、`empty_talker_error_names_the_endpoint_that_was_actually_called`、`group_members_rejects_an_empty_chatroom_without_a_request`。
- **SSE 重放历史改由生产者单点写入**：此前每个订阅端各自编号，会产生发布编号与投递倒序；并堵住订阅与基线发布留下的三个并发缺口（订阅/快照缝隙、基线发布越过归零基线）。增量读取改为**排空到不满页为止**，注销的副作用挪到账号校验之后。
- **Rust SDK 的 `watch()` 游标只在帧真正交付后才推进**：此前「收到即推进」⇒ 一帧解码失败或被消费者提前丢弃时，重连带上的 `Last-Event-ID` 会跳过那一条，客户端**静默漏消息**。现在游标悬在未交付的帧上不落；并且**新连接开始时清空上一连接遗留的事件 id**（悬空 id 会被服务端当重放起点，跨连接泄漏成「从别人读到的位置开始读」）。解码错误本身归 `Shape`（不再伪装成传输失败去重试）。回归位置：`watch_does_not_advance_the_cursor_past_an_undelivered_frame` 与 `watch_resets_the_pending_id_when_a_new_connection_starts`。

### 安全

- **SNS 的 HTML 导出逐字段转义并给链接加协议白名单**：正文与昵称等此前直接插进 HTML 模板；`href` 也不校验协议（`javascript:` 一类可原样落进导出物）。同源判定补上反斜杠形态，`createTime` 的裸插值一并转义。

### 变更（对门禁与工具链，不对接口）

- **接口契约 pin 在本段区间内两次升版：`v0.4.0` → `v0.5.1` → `v0.6.0`**（`conformance.pin` 与测试里的 `CONTRACT_VERSION` 同提交，tag 与契约仓 `VERSION` 对齐）。两跳内容不同：`v0.5.1` 未动解释记录，只加两枚钉子（游标 `offset+since+id` 组合、注销后重放旧事件 id）并配套改 runner 不变量与 case schema；`v0.6.0` 收拉取面 `mediaId`：钉子里 `pull-message-fields` 加断言 `media_id_shape_in_pull`，`INTERPRETATION.md` **新增第 9 条**（句柄位于消息这一层、「出现即可取」的判据，以及「可取性随时段变化、客户端须容忍句柄缺失」这条已知遗留）。CI 的契约 nails 步骤按 pin 的 tag 克隆钉子仓。
- **包装脚本不再把调用方首参注入第二遍**：`build.ps1` 此前在 `$args[0]` 恰为子命令时才补 `--features testing`，`build.ps1 --locked test` 这类写法会漏注入（报错是一堆「模块是私有的」），而 `--features=x`／`-F testing` 形式会被重复注入。
- **`graceful_shutdown` 测试的等待谓词升级为三段式**（端口可连 → token 可读 → 用该 token 打通一次鉴权），并把「端口未起」与「端口起但读不到凭据」两种失败**分开报错**、各给 remedy。纯测试侧，不改产品行为。
- **验收测试补强区分力**：改查值而非查键名、`--with-media` 断言句柄集合与 `media/` 文件集合相等、404 与 5xx 互为对照、缺 token 那条把 BASE_URL 指向保证无监听的端口。
- **`docs/architecture.md` 新增「已登记的两类构建告警（预期内，处置＝维持现状）」**：默认 feature 组合下的 `dead_code`（只被 `testing` 夹具或单测调用的内部辅助；CI 的门禁带 `--features testing` 所以看不到），以及链接期 `LNK4099`（vendored OpenSSL 缺 `ossl_static.pdb`，只影响调试信息）。
- **顶层 Python 包补 `py.typed`**：生成层内部有该标记、顶层手写包没有 ⇒ 消费方的类型注解全部静默失效（mypy 报 `import-untyped`）。

- **包装脚本的 feature 注入判定四处收口**：① 只看 `--` **之前**的参数（`--` 之后是转发给测试
  二进制／rustc 的，那里的 `--features` 不是给 cargo 的，此前会让整轮测试漏注入并报一堆「模块是私有的」）；
  ② 子命令不再假定是第一个参数（`--locked test` 这类全局选项先行的写法此前不注入）；③ `-p <crate>`／
  `--package <crate>`／`--package=<crate>`／`-p<crate>` 选中别的 package 时**不注入**——`testing` 只存在于
  根 package，注入会让 SDK 的测试直接报「does not contain this feature」（合并形态此前只认 `.StartsWith('-p')`
  与 `-p?*)`，漏了 `--package=`）；④ bash 侧切片改带引号展开，否则含空格或 glob 字符的选项值会在注入这一步
  被重新分词。回归位置：`scripts/tests/test_build_wrapper.py` 的
  `test_global_option_before_subcommand_still_gets_testing`、
  `test_features_after_double_dash_does_not_suppress_injection`、`test_merged_features_form_is_not_reinjected`、
  `test_package_selection_does_not_inject_root_only_feature`。
## [0.8.0] - 2026-10-04

### 变更

- **`clients/rust`（`weflow-client`）与 `clients/python`（`weflow-sdk`）删除 `search`（破坏性）**：
  同一端点上它已被 `list_messages` 完全覆盖——后者的参数是前者的超集（多了
  `limit`/`offset`/`media`），且两仓此前都没有任何测试盯着 `search`。**迁移方式**：
  `search(talker, keyword, start, end)` 改写为
  `list_messages(talker, keyword=…, start=…, end=…)`，语义不变（`end` 覆盖整天）。
  回归位置：`list_messages_pages_by_offset_and_exposes_native_fields`。
- **`ensure_ready` 对 200 拒绝态立即失败（行为变化）**：注册端点用 HTTP 200 表达业务拒绝
  （`account_conflict` / `invalid_key` / `invalid_db_path` / `unknown_qq`）。此前只有 Python 侧
  分类，Rust 侧不看响应体，会把确定性的拒绝一路轮询到超时并报「not-registered」——
  把根因（绑定被占、密钥被拒）伪装成「还没就绪」。现在两仓同规：Rust 返回
  `ClientError::Refused { state, .. }`，Python 抛 `StatusError(200, …state=…)`。
  **迁移方式**：对错误做穷尽匹配的调用方补一条 `Refused` 分支并读 `state`；原有的
  `NotReady` 分支保留，它仍覆盖真正未就绪而超时的情形。回归位置：
  `ensure_ready_fails_fast_on_a_refusal_state`。
- **`ensure_ready` 改为 `register` + `wait_ready` 的组合**：同一端点只有一处实现，
  注册契约变更不会只落在半个 SDK 里。行为不变。
- **时间界校验放宽为「`YYYYMMDD` 或 unix 秒」**：服务端两种都收，此前客户端只放行
  8 位日期，把合法的 unix 秒上界挡在本地（`end` 为裸日期时覆盖整天，这条不变）。

### 新增

- **两个 SDK 的公共面扩展（七项，Rust 与 Python 同名同义）**：`health()`（`/health`，
  **免鉴权且不发送凭据**）、`accounts()`（账号明细——`error` 与 `messageCount` 只在这个面）、
  `register(body)`（**非阻塞**注册，原始 `state`/`status` 作为值返回的 `RegisterOutcome`）、
  `wait_ready()`（Rust 侧补齐，wait-only，与 Python 对称）、`list_messages(MessageQuery)`
  （原生消息面：`start`/`end`/`limit`/`offset`/`media`，带 `rawContent`/`isSend`/`localType`）、
  `contacts(ContactsQuery)`、`media_bytes_by_id(id)`（按单段句柄取字节，不触发导出）。
  这些正是「就绪门控 / 轮询 / 媒体」三类消费者此前只能自己拼 HTTP 的部分。
- **`clients/rust` 的 `list_all_sessions` 补测试**：它以「空页」为终止条件（该面没有
  `hasMore`），此前没有任何断言盯着——停止条件写错会静默截断会话列表。回归位置：
  `list_all_sessions_pages_and_collapses_cross_page_duplicates`。
- **`docs/weflow-server-api.md` 的「类型化客户端（SDK）」一节改为公共面清单**：逐方法列出
  打哪个面与语义要点，并点出两处容易读错的地方——`list_all_sessions` 是取尽而
  `list_messages`/`contacts` 只取一页；时间界收 `YYYYMMDD` **或** unix 秒且 `end` 的裸日期
  覆盖整天。
- **同步引擎的增量变更检测加固**：已建立连接的库改以 SQLite `data_version`
  探测外部提交（连接本地基线，仅在同一连接上比较，故只在连接存续期有效），
  文件 mtime/size 戳退为未打开文件的前置门。原先仅靠文件戳：Windows 高负载下
  元数据回读可能滞后，一次写入会被误判「未变」而漏检。同批修正两处「失败轮次
  仍记账」：增量轮里无密钥/打开失败的文件不再记录戳；首次全量构建同理，只给
  成功打开的库记基线——否则其存量数据会被判「未变」而卡死到文件下次变化。
  WrongKey 这类确定性失败改为快速失败（不再每次重试都撞 5 秒 busy 上限），
  注册密钥更正后自动恢复重试。无外部接口变化；回归位置：`src/sync/mod.rs` 的
  `stamp_unchanged_but_committed_row_is_still_polled` 与
  `failed_open_does_not_swallow_the_next_poll`。
- **`clients/python`（`weflow-sdk`）`watch()` 的缓冲上限语义**：1 MiB 上限现在同时
  约束「单个完整帧」与「未成帧累计」——超限帧（无论格式是否合法）一律**不交付**，
  本次流结束并退避重连（原先超限的完整帧会先被交付）。这是行为变化；SDK 未发布、
  无迁移动作。回归位置：`clients/python/tests/test_behavior.py` 的
  `test_watch_rejects_oversized_single_frame_whole_and_split`（对照
  `test_watch_delivers_frame_just_under_the_cap`）。
- **`clients/python`（`weflow-sdk`）`watch()` 的重连退避**：退避只在「干净结束」
  （EOF、无超限、无传输错误）时复位到 0.5s，否则按 0.5→1→2→4… 升级到 30s 上限。
  原先每建一次连接即复位：畸形但能建连的流永远 0.5s 一轮（实测 3 秒 6 连），退避
  形同虚设。回归位置：`test_watch_backoff_escalates_on_persistent_overflow`（对照
  `test_watch_backoff_returns_to_floor_after_clean_stream_end`）。
- **`clients/python`（`weflow-sdk`）SSE 一帧多条 `data:` 行按规范以 LF 拼接**成单一
  载荷（原先逐行覆盖、只保留末行）。当前服务端每帧恰一行 `data:`，对现有形状逐字节
  中性；回归位置：`test_watch_joins_multiple_data_lines_per_frame`（三行边界对照
  `test_watch_joins_three_data_lines_boundary`）。
- **`clients/python`（`weflow-sdk`）拒绝态词表口径说明（无行为变化）**：`_REFUSAL_STATES` 是
  两仓共用的**对称超集**（`account_conflict` + `invalid_key` / `invalid_db_path` / `unknown_qq`），
  本仓服务端当前只产生 `account_conflict`——多出的三态本仓不产生，是刻意保留：词表按
  「账号面／SSE 全量」的服务端共同语义收敛，而不是按单端点裁剪，服务端将来补拒绝态时
  两侧 SDK 不必各改一遍。不构成误纳（不会把本仓合法应答误判成拒绝）。
- **`clients/ts` 示例**：POST 显式携带 `Content-Type: application/json`（服务端 axum
  Json 提取器对非 JSON 内容类型回 415，示例此前必然踩中）；`smoke.ts` 第 3 步改为
  真断言——只接受「业务拒绝（StatusError 带 state）」或「受理后就绪等待超时
  （NotReadyError）」两种合法结局，其它结局（415/5xx/网络错误/未知异常）非零退出；
  新增可入库的 `stub-server.mjs`（无真实服务即可门禁：`STUB_STATUS=500` 必须让
  smoke 变红）；`watch` 补齐真实帧组装（`id:`/`event:`/`data:` 切分、空行成帧、
  `Last-Event-ID`、字节上限），与 qqflow 版同构（weflow 版为异步生成器、qqflow 版为
  回调），README 写明两版等价。
- **`clients/python`（`weflow-sdk`）`watch` 撤销从未生效的 `poll_interval` 形参
  （破坏性）**：该形参在 0.7.0 引入后从未被函数体读取（Rust 侧亦无对应参数），
  现已从签名移除。**迁移方式**：调用方删掉该实参即可，行为不变。SDK 未发布，
  已核实在役调用点为零。

### 新增

- **`clients/python` ruff 门禁**：`clients/python/pyproject.toml` 落显式的
  `[tool.ruff]` 与 `[tool.ruff.lint]` 表（规则集写死在配置里、不依赖默认集；
  生成层的排除只写在配置文件中——CLI 传相对排除按 cwd 解析、传 `--config` 会改变
  配置内相对路径的基准，两种写法都会让排除静默失效并把生成层整套扫进去）；
  CI `check.yml` 增加对 `src` 与 `tests` 的 ruff 步骤，并附「摘掉排除后命中必须
  涨到千量级」的区分力自检（本地实测 2253 条）；`ruff` 以精确版本钉进 dev extras
  （默认规则集随版本漂移，钉死才可复现）。手写层首批告警清零（pyupgrade 系与
  `RUF022` 自动修后逐处人工过 diff；`watch` EOF 支路的裸 `except Exception` 收窄为
  与主循环相同的 `(ShapeError, ValidationError, AttributeError)`）。
- **`clients/python/src/weflow_sdk/LICENSE`**：仓库根许可证复制进包树，随构建产物
  分发（消费方打包时由各自的 package-data 声明决定是否入 wheel）。
- **`clients/python/scripts/regen.py --check` 改为对生成树做摘要比对**（此前只比
  `spec.json`）：重生成到临时目录、与已提交生成树逐文件比摘要，模型层或空白层面的
  漂移不再可能「spec 没变就算绿」。生成器版本钉在 npx 调用里（wrapper 2.41.0 →
  openapi-generator 7.25.0），不再依赖可漂移的 latest。
- **`clients/rust`（`weflow-client`）**：类型化 Rust SDK（workspace 成员，随根包版本 0.7.0）。
  类型与操作客户端由 `/openapi.json` 描述生成（生成物入库，CI 断言「重生成无 diff」）；
  行为面手写六件套：`ensure_ready`（注册 + 就绪轮询，吸收 503 语义）、`drain_session`
  （Pull 游标原样回传排空）、`list_all_sessions`、`watch`（SSE 重连带 `Last-Event-ID`、
  心跳过滤、`generation` 变化上报）、`media_bytes`（404 后按「先 `media=1` 导出再取」重试一次）、
  `search`（`YYYYMMDD` 客户端校验，`end` 覆盖当天）。本轮**不发布** crates.io（本地可安装）。
- **`clients/python`（`weflow-sdk`）**：类型化 Python SDK（纯 Python，`httpx` + pydantic，`py.typed`）。
  模型由描述生成（`scripts/regen.py`，规范化后的 spec 一并入库，CI 断言「重生成无 diff」——
  spec 的来源是 Rust 侧生成工具的 `--dump-spec`，避免 golden 的占位掩码把易变值的真实类型烧成 string）；
  行为面与 Rust 侧逐方法同构：`ensure_ready` / `wait_ready`（wait-only 就绪轮询，不做任何注册动作）/
  `drain_session` / `list_all_sessions` / `watch` / `media_bytes`（404 后按「先 `media=1` 导出再取」重试一次）/`search`。
  `ensure_ready` 现在把 200 响应体里 `state` 为拒绝态（`account_conflict` 等）的应答映射为 `StatusError`
  快速失败，不再当作受理后空等到超时；`watch` 改为**单连接连续产出多帧**（原先每帧断开重连，空闲时
  也无限重连且每次重连都重放基线帧），分帧改为**字节级 LF**（`aiter_lines` 的 splitlines 语义会把含
  U+0085/U+2028/U+2029 的 JSON 正文拆断）并加 **1 MiB 未消费缓冲上限**（畸形无空行流不再无界膨胀），
  单帧解码失败记 WARNING 跳过而不再杀流，EOF 时把未终结残行并入末帧冲刷。
  本轮不发布 PyPI；测试对进程内 ASGI mock 跑，不依赖真库。
- **`clients/regen`（`weflow-regen`）**：生成工具（`cargo run -p weflow-regen`，`--check`
  供 CI 用）。它把服务端描述做确定性规范化（3.1 → 3.0：`type: [T, null]` 转 `nullable`、
  `Option<T>` 的 `oneOf` null 臂丢弃）后交给生成器 —— no-diff 门禁钉住的是整条
  规范化 + 生成管线。

### 修复

- **`/openapi.json` 为路径模板参数补 `parameters` 声明**：四条带占位符的操作
  （`DELETE /api/v1/accounts/{wxid}`、`GET /api/v1/media/{id}`、两条 Pull 路径）此前没有
  参数声明，违反 OpenAPI 规范（占位符必须有同名 path 参数）；golden 快照只记录输出、
  不校验合法性，因此一直无声。客户端生成器把这种文档当非法输入直接拒绝。
  守卫断言见 `tests/openapi.rs`（回归：模板占位符必须同名声明）。

## [0.7.0] - 2026-10-01

接口面的形状收敛：老面只做原生/富数据面，ChatLab 形状搬到 `/chatlab/*`；媒体只留一条按名取字节的路由；
鉴权通道减到两条。**破坏性变更较多**，逐条迁移见文末「迁移」。

- **`GET /chatlab/messages`** —— ChatLab 形状的消息面，也是原「混合面」
  （`/api/v1/messages?chatlab=1`）的新家：参数 `talker`（必填）/ `limit` / `offset` / `cursor` / 
  `start` / `end` / `media` / `keyword`；信封
  `{talker,count,page,chatlab,meta,members,messages}`，**不带 `success`**，
  `count` 是**本页条数**，消息**升序**，`page{hasMore,nextCursor}` 报告截断。
  `media=1` **真正执行导出** —— 旧的混合面在收集导出任务**之前**就 return 了，
  所以 `media=1` 在 ChatLab 形状上从未导出过；「先触发导出、再取字节」这条两步走在那个面上并不成立。
- **消息的媒体元数据**：拉取面与消息面每条消息新增 `media{type,fileName,md5}`（无媒体省略整键；
  `md5` 取不到时省略该键）。它是**元数据**：只有 `media=1` 且该条**确实写出了本地文件**时，
  `fileName` 才是可取句柄。
- 原生面 `media` 新增 `mediaId`：只在**本次请求的导出批次**确实写出了**内容摘要派生**的
  本地文件时出现（DB 名回落、外链、导出失败、超每请求上限的一律不给）。
- `group-members` 新增 `fromCache`（恒 `false`）与 `updatedAt`（**毫秒**，索引构建/更新完成时刻）。
- 通知面的**撤回帧**带上 `platformMessageId`（本仓事件里的 `rawid` 就是平台消息号）；
  `message.new` 仍为 `null` —— 给它要在推送热路径上逐事件查一次索引，而规范里该字段是可选的。

### 变更（破坏性）

- **老面不再输出 ChatLab 形状**：`/api/v1/messages` 与 `/api/v1/sessions` 上的
  `format=chatlab` / `chatlab=1` 开关已删除，两个面只输出原生形状。
- **读端点只有 GET**：`messages` / `sessions` / `contacts` / `group-members` / `media/{id}` /
  `push/messages` 的 POST 变 405。`/api/v1/sync` 与 `/health`、
  `/api/v1/accounts` 仍接受两个方法；`sns/*` 六条不变。
- **鉴权只剩两条通道**：`Authorization: Bearer` 与 `?access_token=`。
- **媒体字节只留 `GET /api/v1/media/{id}`**，三段式 `/api/v1/media/{talker}/{media_type}/{file}` 已删除。
- **`group-members` 的成员集合改为名册 ∪ 发言人**：从未发过言的成员也会出现（`messageCount` 为 0），
  `count` 会变大。同时删 `forceRefresh`（同步统一走 `/api/v1/sync`）与
  `withCounts` 别名，`refreshed` 键换成 `fromCache` + `updatedAt`。
- **`replyToMessageId` 在三个面统一为「无引用则省略该键」**（原生面此前是无引用给 `null`）。
- 原生面 `media` 的 `url` / `localPath` 未导出时**省略**（此前是空串）；
  `md5` 只在取不到时省略（已知摘要照给 —— 未导出不等于没有摘要）。
- 删除参数别名：`meiti` / `tupian` / `vioce`；删除 `POST /api/v1/accounts/{wxid}/deregister` 别名。
- 契约 pin 升到 `v0.4.0`（新增两条具名不变量与消息面用例；能力词表与端点登记同步）。

### 修复

- **按名取字节的同名多命中**：候选内容一致才服务（先比 size，必要时逐字节比较），不一致给 404。
  此前是「取第一个」且目录遍历顺序不确定 —— 同名在别的会话里可能是另一个文件，随便挑一个等于把
  「出现即可取」变成「出现即可取到某个东西」。只有**内容摘要派生**的名字才允许作为句柄下发，
  平台名（视频的 DB 名回落）与原文件名回落只作元数据。
- `group-members` 的排序稳定：先按 `messageCount` 降序、再按 uid 升序。只按计数排时，
  一大批计数为 0 的潜水成员顺序随哈希遍历顺序抖动，同一个群两次请求的顺序可能不同。
- **注销函数里那段英文注释改回与代码一致**：它写着重放历史「保持不动」，而代码是**清空条目 ＋
  保留 id 计数器 ＋ 推进 `generation`**（迁移到统一语义时改了行为，没改这段注释）。

### 文档

- `docs/weflow-server-api.md` 与 `docs/architecture.md` 随本批改动同步：路由清单、
  鉴权两条通道、`/chatlab` 四条路由、媒体按名解析与同名消歧规则、群成员集合。
- **补记**：老面 `GET /api/v1/sessions` 的 `cursor` 入参是随 0.6.0 的会话分页一起引入的
  （那时是纯增量）。本次它随「老面只认 offset」一起删掉了。
- `/chatlab/sessions` 的响应示例补上漏掉的 `count` 键（实际响应一直有它）。

### 迁移

过渡期一律**立即生效**。

| 改了什么 | 怎么迁 |
|---|---|
| 老面不再输出 ChatLab 形状（`chatlab=1` / `format=chatlab`） | 改用 `GET /chatlab/messages`（参数同名，见上）；会话发现面用 `/chatlab/sessions` |
| 老面不再识别 `cursor` | 老面用 `offset`；**注意失败模式**：继续传 `cursor` 不会报错，而是**静默回到第一页** |
| 旧参数拼写（`chatlab=1`、`format=chatlab`、`meiti`、`tupian`、`vioce`） | 一律**被静默忽略**（未知参数不报错）：改成 `media=1` 与类型参数，或改用新面 |
| 三段式媒体路由已删 | 改用 `GET /api/v1/media/{id}`（`{id}` 是导出文件名）；未导出前先请求 `media=1` |
| 鉴权只剩 Bearer 与 `?access_token=` | 删掉 `X-Api-Key`、`?token=` 与「把 token 放进 JSON body」的写法 |
| 原生面的 `replyToMessageId` 由「恒出现、无引用给 `null`」改为**省略该键** | 按下标「键存在」判断引用关系的代码改为「键存在**且非空**」 |
| `/chatlab/messages` 的外层没有 `hasMore`，改用 `page{hasMore,nextCursor}` | 按 `page.hasMore` 判断是否续页，用 `page.nextCursor` 回传 `cursor` |
| 读端点的 POST 变 405 | 改用 GET（参数走查询串） |
| `group-members` 的 `refreshed` 键没了，成员集合并了名册 | 改用 `fromCache` / `updatedAt`；按 `count` 分配 UI 的地方要接受更大的成员数 |
| 原生面 `media` 的 `url` / `localPath` 未导出时省略 | 判空从「空串」改为「键不存在」 |
## [0.6.1] - 2026-10-01

门禁与文档收口。**响应形状未变** —— 新增的是接口描述里的两条操作与更严的门禁。

### 新增

- **`/openapi.json` 补上两条真实存在的操作**：`POST /api/v1/sessions`、`GET /api/v1/sync`。
  它们一直能被调用却不在描述里，从描述生成客户端的人看不到它们。
- 路由现在有**唯一事实源**（`src/server/routes.rs`）：`build_router` 由它构建，
  与端点表的对等由 `documented_routes_match_the_openapi_table` 强制 —— 集合必须等于
  「路由 − 豁免」（`sns` 六个操作尚未 DTO 化，豁免写成代码常量），未声明的方法必须 405。

### 变更（对门禁，不对接口）

- 一致性套件带上跳过即失败：有用例被跳过时整套失败（此前跳过不影响退出码，
  「夹具少声明一个端点」会让用例静默变成不跑，而 CI 仍是绿的）。缺 `FLOW_CONTRACT_DIR`
  同样由静默通过改为失败。
- 契约 pin 升到 `v0.3.3`（`v0.3.1` 引入跳过即失败；`v0.3.2` 让 runner 校验 tag；
  `v0.3.3` 修公共段措辞）。夹具的 `contractVersion` 与 `conformance.pin` 由
  `pinned_contract_version_matches_the_fixture` 钉在一起，只改一处不再能溜过。
- golden 快照**缺失即失败**（此前缺失会被静默重建，drift 检测随之失效）。

### 修复

- SSE 面补 `mediaId` 的正路径端到端断言：导出根下有文件 → 帧里带 `mediaId` → 用该 id
  取回字节（此前只有单元测试证明「一次 stat 的判据对」）。
- 导出写入单测里两条「无残留」断言此前查错了位置（按 join 链算，一个落在 root 内、
  一个落在 root 的父目录），移除守卫时它们仍然是绿的。
- SSE 的 content-type 断言此前只在 `#[ignore]` 的真库测试里，现进了门禁。
- `CHANGELOG` 的 `[0.5.1]` 说明改为与史实一致：那段内容是**改名**归入 `0.6.0`，
  不是那时才写入本文件。

### 文档

- `docs/architecture.md` 补「工程与工具链」「测试与夹具」两节。
- `docs/weflow-server-api.md` 登记 `members[].roles` 为**有意不输出**（与 `isOwner` 同义，
  且受同一个「群主可能不在本页」的限制）。

### 迁移

无。
## [0.6.0] - 2026-09-27

ChatLab 适配层上线，**并接受一次破坏性发布**（五项，见下）。下游需按迁移表逐项核对。

> **版本归属**：安全修复与代理加固**实际随 0.5.1 发布**（2026-08-30，当时未单列条目）——
> 明细已归档到下方 **[0.5.1]** 节。本节只记 0.5.1 之后发生的变化。

### 修复

- `mediaId` **只在取得到字节时才通告**（「出现即可取」是承诺，不是尽力而为）。

### 破坏性变更

| 变更 | 改了什么 | 怎么迁 | 过渡期 |
|---|---|---|---|
| **`end=YYYYMMDD`** | 由「当天 0 点」变「当天 **23:59:59**」 | 若依赖旧语义，改用显式时刻参数 | **不适用** —— 唯一已知下游零影响（实测：只用 `start`）|
| **SSE `sync` 载荷** | weflow 统一为 `{event,generation,watermarks:[…]}`；qqflow 由平铺的 `lastRowidGroup`/`lastRowidC2c` 收敛为 `{event,watermarks:[{table,watermark}]}` | 订阅者若解析 `sync`，按新形状改 | **不适用** —— 唯一已知下游零影响（实测：显式忽略 `sync`）|
| **注销后重放** | 清重放条目 ＋ **保留** id 计数器 ＋ 基线带 `generation` | 依赖 `Last-Event-ID` 的下游需处理 `generation`（它区分「换了个账号」与「自己漏收了」）| **不适用** —— 唯一已知下游零影响 |
| **媒体 URL 去 token** | URL 不再内嵌 `?access_token=` | 旧的 `split("?",1)[0]` 写法**仍兼容**（不报错），可简化 | **不适用** —— 向后兼容 |
| **`message.new` 媒体字段** | weflow 补一个**过滤后的** `mediaId`（只在导出根下确有文件时出现），`md5` **保留** | 无 —— 纯增量 | **不适用** |

### 新增

- **ChatLab 适配面 `/chatlab/*`**（`baseUrl` 指向 `http://127.0.0.1:PORT/chatlab` 即可）：
  - `GET /chatlab/sessions` —— Pull 形状的发现面（`keyword` / `limit` / `cursor`）；
  - `GET /chatlab/sessions/{id}/messages` —— Pull 面；
  - `GET /chatlab/push/messages` —— **通知面**：只发元信息（`eventId` / `sessionId` / `timestamp` /
    `platformMessageId?`），**不发消息体**。规范对这条通道的定位是「仅通知，不假设事件可靠送达」，
    客户端收到后**去拉**那一页。
  - **老面 `/api/v1/*` 的路由集合与默认语义不变** —— 不关心新能力可以不迁。
- **SSE 连接建立就发一帧 `sync` 基线**：没有它，客户端在「连上」到「第一次水位变化」之间是盲的。
- **`generation`**：注销时递增，两个 SSE 面发同一个计数器。
- **`mediaId`**（SSE 的 `media` 里）：可直接走 `GET /api/v1/media/{id}` 取字节。
- **接入配方写在 `docs/*-api.md` 里**（四阶段 ＋ 实测数字 ＋ 接入者会踩的坑）。
- **群元数据接上真实数据源**（`/api/v1/group-members` 与 ChatLab 两面同步受益）：
  `groupNickname` 由恒空串 → 真实群名片（源 `contact_fts.db::chatroom_member_fts_v3`；
  缺该库时降级为空、服务照常启动）；`isOwner` 由恒 `false` → 每群恰一人 `true`
  （源 `contact.db::chat_room.owner`）；名册入库供 `/chatlab/sessions` 的 `memberCount` 使用。
  **此前 `group-members` 的群名片恒空曾让下游误判该端点冗余并退订——现在可以重新评估。**

## [0.5.1] - 2026-08-30

本版发布于 08-30，当时没有单列变更条目：内容挂在 `[未发布]` 标题下（三项安全修复由 `54b1fc2`
于 08-29 写入），0.6.0 只是把那一段**改名**归入自己名下。此处补记归档，**安全修复的归属以本节为准**。

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

### 变更

- **ChatLab 两个面对齐 0.0.2 标准枚举**，并把账号名与群名片拆成两个字段——依赖旧枚举取值、
  或把群名片当账号名使用的下游需同步调整。
- **`localType` 按打包字段掩码解析**：高 32 位（appmsg 子类型）此前不掩码即丢失，appmsg 一族
  大量落 `99 OTHER`；掩码后类型覆盖面恢复。
- Pull 面删除恒空的 `mediaPath`：该键从未有过值、规范列为可选——依赖它的下游改用 `media` 对象。
- CI 加固：clippy `-D warnings` ＋ 全量测试的 PR 门禁上线；路径逃逸用例按平台拆分；
  `graceful_shutdown` 用例串行化消除凭据库竞态。

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
