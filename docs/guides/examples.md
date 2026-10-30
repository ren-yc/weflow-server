# 启动示例

本页是任务导向的操作指南（从 [API 参考](../weflow-server-api.md) 拆出）；端点、字段与错误形状见该参考。

---

```powershell
weflow-server.exe --port 5033 --watch-fallback-ms 5000 --log info
weflow-server.exe serve --port 5033      # 与上一行等价
weflow-server.exe sessions --json        # 子命令面
```

参数：`--show-token`、`--port`、`--host`、`--log`、`--log-format`、`--watch-debounce-ms`、`--watch-fallback-ms`、`--media-export-dir`、`--base-url`。全部仅命令行，无配置文件。

数据目录不可配置：Windows `%LOCALAPPDATA%\weflow-server`；媒体导出默认落在其下的 `api-media`，仅 `--media-export-dir` 可改。
