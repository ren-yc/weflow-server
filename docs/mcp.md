# MCP 工具面（stdio）

本服务可以作为 **MCP（Model Context Protocol）服务端**运行：把只读查询暴露成一组工具，供支持
MCP 的 agent 客户端调用。传输走 stdio，由一个独立的 `weflow-server mcp` 进程承担。

## 数据会离开本机

工具的输出会进入模型上下文 —— 也就是**对话内容离开本机**。这条提示同时写在四处：

- 本文件；
- `README.md` 顶部；
- MCP `initialize` 响应的 `instructions`；
- 每个工具的 description。

模型看不到 README，所以后两处不是重复，而是**唯一会被模型读到的位置**。

## 为什么是独立进程加 HTTP 客户端

MCP 进程**不碰数据库密钥**，也不重复实现解密与索引：它只持有 API token，经 `weflow-client`
打本机 HTTP 面。理由与 CLI 相同 —— 自己再拼一份 HTTP 会与 SDK 漂移，而只有 SDK 那一份被
契约测试钉住。代价是**必须先跑服务端**。

## 前置与启动

1. 服务端已启动（默认 `http://127.0.0.1:5033`）；
2. 环境变量 `WEFLOW_TOKEN` 已设置（值用 `weflow-server token` 取；token 不经命令行传递，以免
   落进 shell history 与进程列表）；
3. 一个支持 stdio MCP 的客户端。

```bash
weflow-server mcp
# 服务不在默认地址时
WEFLOW_BASE_URL=http://127.0.0.1:6002 weflow-server mcp
```

## 工具

| 工具 | 取数面 | 说明 |
| --- | --- | --- |
| `list_sessions` | 会话发现面 | 可选 `keyword`（服务端过滤）；返回 username / displayName / sessionType / messageCount 等 |
| `get_messages` | Pull 面 | ChatLab 形状、时间升序；游标 `nextSince` 与 `nextOffset` |
| `get_messages_raw` | 原生消息面 | 带 `rawContent` / `isSend` / `localType` 与媒体元数据；按 `offset` 翻页 |
| `search_messages` | ChatLab 消息面 | 会话内关键词检索；游标 `nextCursor` |
| `get_contacts` | 联系人面 | 备注 / 昵称 / 别名只在这个面出现 |
| `get_media` | —— | 只给句柄与访问地址，**不下发字节** |
| `group_members` | 群成员面 | 名册与发言人的并集（潜水成员也会出现，计数为 0） |
| `sync_now` | 同步端点 | **唯一的写动作**：推进水位并可能导出媒体 |

## 分页与截断

- 「取一页」类工具的 `limit` 默认 50、上限 200（超出按 200 计）。
- 单次输出的字符预算约 32 KB：超出时**少给若干条**并置 `truncated: true`。第一条永远保留 ——
  否则一条长消息会得到「既无内容又无截断标记」的结果，那是 agent 场景里最坏的一种失败。
- Pull 面与 ChatLab 消息面在**预算截断时不返回整页游标**：它们的游标指向整页的最后一条，
  用它续拉会跳过我们没给出去的那些条。此时请用更小的 `limit` 重取本页。
- `get_messages_raw` 例外：原生面按 offset 翻页、本页是连续切片，砍掉尾部后 `nextOffset` 正好
  指向被砍掉的第一条，所以它照常返回（并把 `hasMore` 置真）。

## 失败通道

- **工具级错误**（会话不存在、查无结果）：调用方看得到我们写的说明。
- **协议级错误**（缺 token、传输层失败）：多数客户端只渲染成一句笼统的内部错误，所以可读的
  说明只留给前者。

## 刻意不做

- **不下发媒体字节**：字节会瞬间塞满模型上下文；需要字节请走 HTTP 面。
- **不做跨会话关键词搜索**：消息面按会话查询，跨会话要逐个会话调用。
- **不接数据库密钥**：那是服务端的事。
