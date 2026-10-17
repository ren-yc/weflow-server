<#
.SYNOPSIS
  从调用方自备的本地配置注册一个账号（重启后恢复用）。

.DESCRIPTION
  只读配置文件、不写盘、不回显 token 或密钥。配置形如：

    {
      "base_url": "http://127.0.0.1:5033",
      "token": "<API token>",
      "wxid": "<wxid>",
      "db_path": "C:\\...\\xwechat_files\\<wxid>",
      "keys": { "session/session.db": "<64-hex>", "message/message_0.db": "<64-hex>" },
      "img_aes_key": "<16 位 hex，可选>",
      "img_xor_key": "<如 0x64，可选>"
    }

  token 放在请求头，不进命令行；密钥只从配置读，不代存。
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$ConfigPath
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $ConfigPath)) {
    throw "配置不存在: $ConfigPath"
}
$cfg = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
foreach ($required in 'base_url', 'token', 'wxid') {
    if (-not $cfg.$required) { throw "配置缺少 $required" }
}

$body = [ordered]@{ wxid = $cfg.wxid }
if ($cfg.db_path) { $body.db_path = $cfg.db_path }
if ($cfg.keys) { $body.keys = $cfg.keys }
if ($cfg.img_aes_key) { $body.img_aes_key = $cfg.img_aes_key }
if ($cfg.img_xor_key) { $body.img_xor_key = $cfg.img_xor_key }

$json = $body | ConvertTo-Json -Depth 8 -Compress
$headers = @{ Authorization = "Bearer $($cfg.token)" }
$uri = '{0}/api/v1/accounts' -f $cfg.base_url.TrimEnd('/')
$resp = Invoke-RestMethod -Method Post -Uri $uri -Headers $headers -ContentType 'application/json' -Body $json
"state=$($resp.state) status=$($resp.status)"
