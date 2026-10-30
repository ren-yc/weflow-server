# 批量导出（`export`）

本页是任务导向的操作指南（从 [API 参考](../weflow-server-api.md) 拆出）；端点、字段与错误形状见该参考。

---

`weflow-server export --out <目录> [--format jsonl|json] [--session <id> …] [--since <t>] [--resume] [--with-media]`

**只走 HTTP**：这个面不提供 `--embedded`。服务端已经把数据库密钥握在内存里，CLI 只做编排与落盘；否则一个可能跑几分钟的任务会长时间持有密钥，还得把密钥带上命令行（它会进 shell history 与进程列表）。

产物布局：

- **每个会话一个文件**（`<slug>.jsonl` 或 `<slug>.json`）。`<slug>` 由会话显示名经 `pathsafe::slugify` 得到；**折叠后不含任何 ASCII 字母数字时**（纯中文群名会被折成一串下划线，既不可读也极易互撞）**回落到会话 id 的 slug**；同名会话按出现顺序追加 `-2`／`-3`。文件名是确定性函数——这是 `--resume` 成立的前提。两个补充口径：

  - **去重按大小写折叠**（Windows 卷默认大小写不敏感，`Team` 与 `team` 是同一个文件），而交付名保留原大小写；
  - **编排文件名 `index` 永远留给清单**：显示名恰好是 `index` 的会话拿到 `index-2`，否则 json 形态下会话信封会被清单原地覆盖。

  回归位置：`export::tests::slug_collision_gets_deterministic_suffix`。
- **JSONL 形态**：第一行是 `_type: header`（含 `chatlab` 与 `meta`），其后是 `_type: message` 行。规范建议按时间升序，因此**页内**排序（跨页排序会把内存恒定这条承诺打破）。**不写 member 行**：流式写不出「先集齐成员再写消息」的顺序，而规范说成员行可选、缺省时由导入器从消息收集；本服务的消息行自带 `accountName` 与 `groupNickname`，信息不丢。
- **JSON 形态**：一个会话一个完整信封（`chatlab`／`meta`／`members`／`messages`，不带行型标记）。整会话留在内存，因此大语料请用 jsonl。
- **`index.json`**：本服务自造的编排清单（会话 → 文件 → 条数）。**它不属于 ChatLab 规范，导入请用单个 `<slug>.jsonl`／`.json`；整个目录不可导入。**
- **`--with-media`**：把本会话用到的媒体字节下载到 `<目录>/media/`，并把导出物里的 `media.fileName` **限定为确实落盘的那些句柄**。实现是**单遍**的：每取到一页 Pull 行，就用这一页的时间窗（`(since, nextSince]` 正好对应消息面认的 `start`/`end`，两端都是闭区间的秒级戳；同秒的行必然落在同一页里，所以窗口两端不漏行）**按行分两条路**：

  · **快路径**——拉取面已经给出 `messages[].mediaId` 的那些行，**直接按那个句柄取字节，一发导出请求都不发**。· **慢路径**——有 `media` 却还没有句柄的那些行，把**这些行的时间窗**交给消息面（`/chatlab/messages?media=1`，该面**每请求最多导出 200 项**，超出部分靠翻页续传）触发按需导出，再从回填的可取句柄取字节。

  然后才写这一页的行；顺序不能反 —— 服务端只有在真的写出了本地副本之后，才把 `fileName` 回填成可取句柄。**为什么不能整批撤掉慢路径**：撤掉就等于「媒体句柄只能靠全历史那趟预遍历拿」，而那一趟不认 `--since`（本仓刚修掉的正是这个）。快路径让「已经导出过」的会话（`--resume`、重复导出）近乎零成本，慢路径只在真需要导出时才付出成本；两条路都不再有全历史遍历。

  **`--since` 因此同时下推到导出面**：过去它是两趟独立的全历史遍历，`--since` 只管住写出来的行、媒体照样把整个会话导出并重下一遍；而两趟之间若有并发同步推进水位，第一趟没覆盖到的消息会「有行、无句柄」地静默缺件。磁盘上已有的摘要文件**不重下**（内容摘要名同名即同内容，按存在性复用是安全的）——这也是 `--resume` 能「只补下缺件」的依据。

  回归位置：`cli_e2e::media_window_follows_the_pull_page`（慢路径的窗口跟着 Pull 页走：每个导出请求都带 `start`/`end`、请求条数等于 Pull 页数、两页窗口互不重叠，且 250 条句柄与 `media/` 文件集合大小相等）、`cli_e2e::media_id_from_pull_row_skips_the_export_round`（快路径：三行里只有没句柄的那一行需要导出 ⇒ 恰好一发导出请求，且窗口只覆盖那一行的时间戳）、`cli_e2e::with_media_reuses_bytes_already_on_disk`（第二次跑不得重下覆盖）。

注意**两个面给的名字不必相同**：消息面回填的是导出后的内容摘要名，拉取面携带的仍是索引里的原始名，所以句柄**按消息 id 对账**（不是按名字比对），导出物里写的是实际落盘的那个名字。外链媒体与未能导出的媒体**不会**留下句柄：宁可少一个 `media` 字段，也不给一个指向不存在文件的句柄。单个媒体取不到只跳过，不升级成会话级失败。
- **响应里的媒体名要过本地路径校验**：服务端回传的 `media.fileName` 要拿去拼 `<目录>/media/` 下的路径，`../`、盘符、ADS、Win32 设备名这类值配合 `join` 能写到导出目录之外（URL 段编码只防 HTTP 层）。因此落盘前先过 `pathsafe::safe_segment`，非法名按 404 同级跳过**并计数可见**（汇总行给出个数）。同名（含仅大小写不同，折叠口径与会话名一致）的媒体只下载一份字节，引用它的每条消息都映射到**实际落盘的那个名字**。回归位置：`export::tests::with_media_keeps_only_handles_whose_bytes_are_on_disk`、`export::tests::message_line_omits_media_without_a_handle`、`cli_e2e::with_media_rejects_unsafe_response_file_names` 与 `cli_e2e::with_media_maps_exported_names_back_to_message_ids`。

三条硬约束：

1. **导出物里不得出现访问令牌**。服务端的媒体是**根相对路径**（`/api/v1/media/<file>`，**不含令牌** —— 令牌只走请求头或 `?access_token=`，响应体从不嵌它）。导出仍然**不写任何 URL**（相对路径换台机器就失效），媒体只以 `{type, fileName}` 表达；并且每一行写盘前会拿调用方给的令牌做一次子串检查，**命中即整轮中止**（不是跳过该会话）并删掉半成品。回归位置：`export::tests::secret_in_output_aborts_and_removes_partial_file`。
2. **会话级失败不静默**：取数失败的会话被记入 `skipped`，而只要 `skipped` 非空，CLI 就以退出码 1 结束。静默少导几个会话是这类工具最坏的失败方式。**`--resume` 的命中不算失败**：记入 `reused`（幂等完成），同参数续跑以退出码 0 收场。**空会话不算失败**（0.10.0 起）：`messageCount:0` 的会话在消息面是 200 空页（「存在但没有内容」与「不存在」已分离），导出它得到一个只含 header 的空产物，既不进 `skipped` 也不影响退出码；`--session` 拼错一个**不存在**的 id 仍按失败记入 `skipped` 并退 1（回归位置：`export_still_fails_on_an_unknown_session_id`、`export_completes_a_listed_session_without_messages`）。
3. **最终名是唯一的完成标记**：会话先写 `<slug>.<格式>.part`，整个会话成功收尾后才 `rename` 成最终名。因此 `.part` 残留意味着「没写完」，续跑一律重写；失败的那一轮只删 `.part`，**不会**碰上一轮已经交付的完整产物。清单 `index.json` 同样先 `index.json.part` 再改名 —— 它是续跑唯一的「完成记录」来源，半路被杀的截断清单会让下一轮误判「没有上一轮」。
4. **`--resume` 复用要同时满足四条**（缺一条就重写，并在日志点名原因）：上一轮清单记着该会话、且它登记的**就是本轮这个文件名**（换格式 `jsonl`／`json` 后盘上的另一扩展名文件是孤儿，不算本轮产物）、没有 `.part` 残留、本轮带 `--with-media` 时上一轮也带过媒体。「有个同名文件」单独不构成完成记录：清单被删或被截断时，那会让从没导出过的会话被静默判成已完成，而它在新清单里没有条目，既看不见也不能自愈。
5. **媒体承诺按轮成立**：`--with-media --resume` 复用上一轮产物时，CLI 会逐个核对复用会话里的 `fileName` 在 `media/` 下确有字节；有悬空就点名该会话并以退出码 1 结束（媒体目录被清理或搬走过的交付包不该静默通过）。处理办法：对点名的会话去掉 `--resume` 重跑，或恢复 `media/`。
6. **单个会话的起手／收尾失败只跳过该会话**（记入 `skipped`），不中止整轮 —— 整轮中止会让本轮已写出的会话留在盘上却不进清单，比留一个 `.part` 更难恢复。令牌泄漏仍是整轮中止。

   回归位置：`export::tests::resume_rewrites_an_incomplete_artifact`、`export::tests::failed_rerun_keeps_the_previous_complete_artifact`、`cli_e2e::export_writes_a_file_against_a_live_service`（含续跑第二次退 0）。

内存：JSONL 逐页写盘、写完即丢，**峰值常驻集与消息条数无关**；`--with-media` 的句柄映射现在**只活在这一页**（O(本页不同媒体数)，不随会话长度增长），因此媒体侧不再破坏这条承诺。同秒组的消息会被服务端扩页带出（Pull 段的「同秒扩页」），那一项仍在承诺之外。大语料的实测口径与造库工具（隐藏的 `--rows` 参数，仅 `testing` feature 下编译进二进制）见 [架构文档](../architecture.md) 的「测试与夹具」；导出流程本身以本页为准。
