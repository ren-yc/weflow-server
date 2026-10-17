# weflow-server quickstart

本文件收任务导向的接入配方。当前先收「重启后恢复账号」；其余配方（接 ChatLab、增量同步、
批量导出、MCP）随后补齐。

## 重启后恢复账号

密钥只存进程内存（见 `docs/architecture.md` 的「密钥不落盘」），所以**服务重启后账号回到
未注册**：`/health` 的 `account` 是 `unregistered`，业务端点返回 503。恢复动作只有一步——
用**与重启前相同的 key 集**重新调 `POST /api/v1/accounts`。

### 1. 准备 token

token 的来源链是 `WEFLOW_TOKEN`（非空）→ 本机凭据库（与 `weflow-server token` 同一份）。
同机可以直接从凭据库取；跨机把 `WEFLOW_TOKEN` 设成服务端实际使用的那个值。

### 2. 准备恢复配置

把 key 放在**你自备的本地安全位置**（不放进仓库、不写进脚本）。示例脚本读一个 JSON：

```json
{
  "base_url": "http://127.0.0.1:5033",
  "token": "<API token>",
  "wxid": "<wxid>",
  "db_path": "C:\\Users\\<用户名>\\Documents\\xwechat_files\\<wxid>",
  "keys": {
    "session/session.db": "<64-hex enc_key>",
    "message/message_0.db": "<64-hex enc_key>"
  },
  "img_aes_key": "<16 位 hex，若重启前用过>",
  "img_xor_key": "<如 0x64，若重启前用过>"
}
```

`keys` 必须覆盖重启前使用的那套库；**图片键与库密钥是两套**，缺图片键会出现「文本正常、
图片解码失败」（详见架构文档的「密钥轮换的症状面」）。

### 3. 注册并确认

```powershell
powershell -File scripts/examples/register-accounts.ps1 -ConfigPath <你的配置.json>
```

```bash
bash scripts/examples/register-accounts.sh <你的配置.json>
```

脚本只读配置、不写盘、不回显 token 或密钥；注册后轮询账号明细直到 `state == "ready"`：

```powershell
# 需要 token；同机未设 WEFLOW_TOKEN 时 CLI 会回落到凭据库
weflow-server accounts
```
