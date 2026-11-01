# 文档地图

按「我想…」选文件。类型四类来自 [Diátaxis](https://diataxis.fr/)：教程 / 操作指南 / 参考 / 解释。

| 我想… | 读这个 | 类型 |
|---|---|---|
| 5 分钟跑通（获取 → 起服务 → 注册 → 第一个请求） | [quickstart.md](quickstart.md) | 教程 |
| 用命令行子命令取数、看退出码与日志旗标 | [quickstart.md#命令行子命令](quickstart.md#命令行子命令) | 操作指南 |
| 批量导出消息（含媒体、`--resume`、产物布局） | [quickstart.md#配方三批量导出含媒体](quickstart.md#配方三批量导出含媒体) | 操作指南 |
| 复制一段可用的启动命令 | [quickstart.md#启动旗标速查](quickstart.md#启动旗标速查) | 操作指南 |
| 让 agent 接入（stdio MCP） | [mcp.md](mcp.md) | 操作指南 |
| 查某个端点、参数、字段、错误码 | [weflow-server-api.md](weflow-server-api.md) | 参考 |
| 逐版行为变化与迁移方式 | [CHANGELOG.md](../CHANGELOG.md) | 参考 |
| 改代码前理解模块职责、数据流与陷阱 | [architecture.md](architecture.md) | 解释 |

## 阅读路径

- **第一次接触本仓**：先 [quickstart.md](quickstart.md) 跑通，再按上面的表挑任务。
- **要改代码**：[architecture.md](architecture.md) → 根目录 [AGENTS.md](../AGENTS.md)（命令、门禁、提交规范）。
- **改接口或行为**：[weflow-server-api.md](weflow-server-api.md) 是端点事实的唯一来源；改动须同步回写该文件与 `architecture.md`（见公共段的「文档同步义务」）。

`clients/ts` 未发布 npm，从源码取。
