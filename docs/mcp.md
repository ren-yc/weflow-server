# MCP 工具面（stdio）

本服务可以作为 **MCP（Model Context Protocol）服务端**运行：把只读查询暴露成一组工具，供支持 MCP 的 agent 客户端调用。工具共 8 个：7 个只读查询 ＋ 1 个显式写动作（`sync_now`，会推进服务端水位并可能触发媒体导出）。

## 数据会离开本机

工具的输出会进入模型上下文——也就是**对话内容离开本机**。这条提示同时写在四处：本文件、[quickstart 的配方四](quickstart.md#配方四让-agent-接入mcp)、MCP `initialize` 响应的 `instructions`、每个工具的 description。

模型看不到仓库里的 markdown，所以后两处不是重复，而是**唯一会被模型读到的位置**。

## 前置与启动

MCP 进程不碰数据库密钥，只持有 API token 经 `weflow-client` 打本机 HTTP 面——代价是**必须先跑服务端**。理由见 [architecture.md](architecture.md) 的「库面与 feature」。

1. 服务端已启动（默认 `http://127.0.0.1:5033`）。
2. 环境变量 `WEFLOW_TOKEN` 已设置，**或**本机凭据库已有记录（同机免 export）；token 不经命令行传递，以免落进 shell history 与进程列表。
3. 一个支持 stdio MCP 的客户端。

```bash
weflow-server mcp
# 服务不在默认地址时
WEFLOW_BASE_URL=http://127.0.0.1:6002 weflow-server mcp
```

## 工具

| 工具 | 面 | 说明 |
|---|---|---|
| `list_sessions` | 会话发现面 | 可选 `keyword`（服务端过滤）；返回 username / displayName / sessionType / messageCount 等 |
| `get_messages` | Pull 面 | ChatLab 形状、时间升序；游标 `nextSince`/`nextOffset`，另回 `sinceResolved` |
| `get_messages_raw` | 原生消息面 | 带 `rawContent` / `isSend` / `localType` 与媒体元数据；按 `offset` 翻页 |
| `search_messages` | ChatLab 消息面 | 会话内关键词检索；按 `offset` 翻页 |
| `get_contacts` | 联系人面 | 备注 / 昵称 / 别名只在这个面出现 |
| `get_media` | 媒体面 | 只给句柄与访问地址，**不下发字节** |
| `group_members` | 群成员面 | 名册与发言人的并集（潜水成员也会出现，计数为 0） |
| `sync_now` | 同步端点 | **唯一的写动作**：推进水位并可能导出媒体 |

## 分页与截断

- 「取一页」类工具的 `limit` 默认 50、上限 200（超出按 200 计）。
- 单次输出预算约 32 KB：超出时**少给若干条**并置 `truncated: true`。第一条永远保留——否则一条长消息会得到「既无内容又无截断标记」的结果。
- `get_messages` 截断时**不返回整页游标** `nextSince`（用它续拉会跳过没给出去的那些条），但**返回 `nextOffset`**（连续切片，正好指向被砍掉的第一条）与 `sinceResolved`（本轮 `since` 解析出的**排他**绝对下界）。
  - 续拉请传 `nextOffset` 且 `since` 传 `sinceResolved`——不要重发相对串（如 `7d`）：续拉发生在下一轮对话，「现在」已经前移，相对串会把窗口悄悄前移。
  - 未提供 `since` 时 `sinceResolved` 为 null（等价于不传）。
- `search_messages` 与 `get_messages_raw` 同按 `offset` 翻页、本页是连续切片，截断时 `nextOffset` 正好指向被砍掉的第一条（并把 `hasMore` 置真）。

## 失败通道

- **工具级错误**（会话不存在、查无结果）：调用方看得到我们写的说明。
- **协议级错误**（缺 token、传输层失败）：多数客户端只渲染成一句笼统的内部错误，所以可读的说明只留给前者。

## 刻意不做

- **不下发媒体字节**：字节会瞬间塞满模型上下文；需要字节请走 HTTP 面。
- **不做跨会话关键词搜索**：消息面按会话查询，跨会话要逐个会话调用。
- **不接数据库密钥**：那是服务端的事。
