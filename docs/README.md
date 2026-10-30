# 文档地图

本目录按 [Diátaxis](https://diataxis.fr/) 的四类分工组织：**教程**带你第一次跑通，**操作指南**按任务给步骤，**参考**供查阅事实，**解释**讲清设计与取舍。先按下表选一份开始读。

| 我想… | 读这个 | 类型 |
|---|---|---|
| 5 分钟跑通（获取 → 起服务 → 注册 → 第一个请求） | [quickstart.md](quickstart.md) | 教程 |
| 用命令行子命令取数、看退出码与日志旗标 | [guides/cli.md](guides/cli.md) | 操作指南 |
| 批量导出消息（含媒体、`--resume`、产物布局） | [guides/export.md](guides/export.md) | 操作指南 |
| 复制一段可用的启动命令 | [guides/examples.md](guides/examples.md) | 操作指南 |
| 让 agent 接入（stdio MCP） | [mcp.md](mcp.md) | 操作指南 |
| 发版、审批闸门与凭据处理 | [release-runbook.md](release-runbook.md) | 操作指南 |
| 查某个端点、参数、字段、错误码 | [weflow-server-api.md](weflow-server-api.md) | 参考 |
| 改代码前理解模块职责、数据流与陷阱 | [architecture.md](architecture.md) | 解释 |

## 阅读路径

按你的角色挑一条最短路径：

- **第一次接触本仓**：先 [quickstart.md](quickstart.md) 跑通，再按上面的表挑任务。
- **要改代码**：[architecture.md](architecture.md) → 根目录 [AGENTS.md](../AGENTS.md)（命令、门禁、提交规范）。
- **改接口或行为**：[weflow-server-api.md](weflow-server-api.md) 是端点事实的唯一来源；改动须同步回写该文件与 `architecture.md`（见公共段的「文档同步义务」）。

## 其它入口

本表之外的入口（不属 Diátaxis 四类，单列）：

| 我想… | 读这个 | 类型 |
|---|---|---|
| 逐版行为变化与迁移方式 | [CHANGELOG.md](../CHANGELOG.md) | 参考（时间序） |
| 装 SDK 用客户端 | `clients/rust`、`clients/python`（`clients/ts` 未发布 npm，从源码取） | 操作指南 |
| 贡献代码（命令、门禁、提交规范） | [AGENTS.md](../AGENTS.md) | 操作指南 |
| 看项目门面 | [README.md](../README.md) | — |
