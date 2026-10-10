# 发布手册（v0.9.0 起）

本文是把新版本发上 crates.io 与 PyPI 的操作手册。正式上传由 CI 的 `publish` 作业
执行（tag 触发、带人工审批闸门），本手册覆盖前置配置、预演、首发与兜底路径。

## 前置配置（一次性）

**PyPI 侧本仓不保存任何长期凭据**（本机与 CI 都不持有）；crates.io 侧的新 crate 首发
只能人工做一次。两条路径的官方约束不同，分开说明。

### GitHub environment（人工闸门）

Settings → Environments → 新建 **`pypi`**，勾 **Required reviewers**（填你自己）。
publish 作业挂在这个环境上：产物齐了之后会**停在审批**等你放行——发布不可撤销，
这是唯一的人工确认点。CI 链＝guard → quality-gate（双 OS）→ build（三平台）→
release（GitHub Release）→ publish。

### PyPI：登记 pending publisher（没有 token 这一步）

pypi.org → 账号 → Publishing → **Add trusted publisher** → 选 GitHub，填：
- Owner `ren-yc`、Repository `weflow-server`
- **Workflow name** `release.yml`（逐字一致——它进的是 OIDC claim）
- **Environment** `pypi`（与上面的 GitHub 环境同名；写成别的名字换不到凭据）

首发之前只能用 **pending publisher**：项目还不存在，无法把权限限定到具体项目。
**首次成功上传后它会自动变成绑定该项目的正式 publisher，不需要再配置**（PyPI 官方
文档明确如此）。两点注意：① pending publisher **不预留名字**——正式发布前若项目名
被他人注册，这条 pending 记录失效；② 表单字段＝**项目名**（pending 特有：项目还不
存在，得告诉 PyPI 要创建哪个项目）＋ Owner／Repository／Workflow name／Environment，
**没有 latest 之类的开关**（「是否最新发布版」由 PyPI 按版本号与 pre-release 标记自行判定）。
**CI 里没有 TestPyPI 步骤**（只有一个上传到正式 PyPI 的动作），所以 TestPyPI 的
预演在本机做（见「预演」一节），不需要在 test.pypi.org 登记 publisher。

### crates.io：新 crate 首发在本机做

crates.io 没有「预登记一个尚不存在的 crate」的机制——publisher 要挂在已存在的
crate 上；而 cargo 的可信发布支持也还没进 stable 工具链（本仓 pin 1.97.1）。因此
**首次发布按下面「首发（本机）」一节做**；做完直接重跑 publish 作业：里面的守卫
检测到该版本已上架就跳过、不报错，把后续步骤跑完。

之后想让 CI 代劳，二选一：等 cargo 的可信发布进入 stable 后在 crates.io 配
publisher；或给本仓配一个 scoped 的 `CARGO_REGISTRY_TOKEN` secret（publish 作业
会读它；没配时作业会明确报「该版本尚未上架，请按本手册本机手工发一次」，
不会含糊失败）。

### 本机凭据（预演与首发用）

- cargo：`~/.cargo/credentials.toml`（已配置）——首发那一次靠它。
- PyPI：`~/.pypirc`（已配置；testpypi 与正式 PyPI 各自独立的 token）。**本机只往
  testpypi 发**；正式 PyPI 一律由 CI 走 OIDC。

## 正式发布流程

1. 确认 `Cargo.toml` 根包与 `clients/rust` 版本一致，CHANGELOG 有对应段；
   **并把 `## [0.9.0]` 的日期回填成实际发布日**（与 tag 同提交）——准备阶段写的是
   准备日，Keep a Changelog 的段日期应当是发布日。
2. 打 tag 并推送（Windows PowerShell 5.1 不认 `&&`，分两行跑）：
   `git tag v0.9.0` ＋ `git push origin v0.9.0`。
3. CI 跑到 publish 时停在环境审批 → 你放行。作业内部顺序：先 SDK crate、轮询
   crates.io 稀疏索引确认条目可见、再发根包（根包的依赖声明带 `version`，registry
   上必须先有那个版本的 SDK）；PyPI 侧构建 wheel＋sdist 后用 pypa 官方动作上传。
4. 核对：crates.io 两个 crate 页、PyPI 项目页、GitHub Release 三平台产物。
5. 发布后：重建 sdk-dist 供应分支——
   `git subtree split -P clients/python/src/weflow_sdk -b sdk-dist` ＋
   `git push origin sdk-dist --force-with-lease`；再同步 briefdesk 的 vendor 与门禁。

## 首发（本机，只做一次）

新 crate 的首发。顺序＝先 SDK 后根包（根包要解析 registry 上的 SDK 版本）。**在仓库根、PowerShell
里整块执行**（`curl.exe` 在 Windows 上是真 curl，不是 PowerShell 别名）；cargo 一律走包装脚本——
它负责定位 MSVC 环境，直接跑 cargo 会在 vendored OpenSSL 上失败（`publish` 子命令不在注入
`--features testing` 的名单里，那条注入只服务 test/clippy）。

```powershell
powershell -File scripts/build.ps1 publish --locked -p weflow-client
# 轮询稀疏索引到条目可见（上传成功≠可解析；上限 5 分钟）。
$idx = 'https://index.crates.io/we/fl/weflow-client'
for ($i = 0; $i -lt 60; $i++) {
  $hit = (curl.exe -s -A flow-release-guard $idx) | Select-String -SimpleMatch '"vers":"0.9.0"'
  if ($hit) { break }
  Start-Sleep -Seconds 5
}
if (-not $hit) { throw 'weflow-client 0.9.0 五分钟内未出现在稀疏索引里，根包发布无法继续' }
powershell -File scripts/build.ps1 publish --locked -p weflow-server
```

（`index.crates.io/<前两字符>/<第3-4字符>/<crate名>` 是稀疏索引路径规则；`-p` 让两条命令都在仓库根
选到目标 package，不需要 cd 进子目录。）
做完回到第 3 步重跑 publish 作业，它会跳过已上架的两步、继续跑 PyPI。

## 预演

### PyPI（testpypi，本机）

0.9.0 这一轮**已做过**（两仓的 wheel＋sdist 已在 test.pypi.org 上，且验证过可安装、
可导入）。同一版本不能重复上传 testpypi，所以再预演时要么换个开发号（如
`0.9.0.dev1`，试完还原），要么把 CI 也接上 TestPyPI 并给上传步加 `skip-existing`。

```powershell
Push-Location clients/python
python -m build
twine check dist/*
twine upload --repository testpypi dist/*
pip install --index-url https://test.pypi.org/simple/ --no-deps weflow-sdk==0.9.0
Pop-Location
```

### crates.io（本机，只列清单不上传）

```powershell
powershell -File scripts/build.ps1 package --list --allow-dirty -p weflow-client
powershell -File scripts/build.ps1 package --list --allow-dirty -p weflow-server
# 核对两件事：
# - SDK 包里必须有 Cargo.toml／README／LICENSE／src —— LICENSE 是这一轮才挪进打包范围的
#   （原先放在 clients/ 下，在打包目录之外，crate 里其实没有许可证全文）。
# - 根包按 git 跟踪文件收，会带上 docs／tests／.github 等；重点是**别**把本机参数文件
#   （weflow-server.json，含真实库路径与密钥）带进去 —— 它未被 git 跟踪，所以不会出现在
#   清单里，看到它就说明有人把它 add 了，立即停手。
# --allow-dirty 只让工作树有未提交改动时也能看清单，不改变打包内容。
```

仓库根对两个 package 都用 `-p` 选包，不必切进子目录。**注意**：`cargo publish --dry-run` 对根包在
SDK 真上架前必然失败——它按发布后的清单解析依赖，那时 registry 上还没有
`weflow-client = "0.9.0"`。这不是缺陷，是发布顺序的直接后果。

## 人工兜底（CI publish 不可用时）

**仓库根、PowerShell 里整块执行**；每一步之前先查该版本是否已上架（两个索引都拒绝重复版本），
cargo 走包装脚本。`Invoke-WebRequest` 遇 404 会**抛异常**，故用 try/catch 把状态码折出来——
不折的话，版本未上架（首发时正是如此）会把脚本打断。

```powershell
# 先查该版本是否已上架（200＝已上架，跳过本步）；未配置凭据时 cargo publish 会响亮失败。
foreach ($crate in 'weflow-client', 'weflow-server') {
  $code = try {
    (Invoke-WebRequest -UseBasicParsing -Headers @{ 'User-Agent' = 'flow-release-guard' }
      "https://crates.io/api/v1/crates/$crate/0.9.0").StatusCode
  } catch { [int]$_.Exception.Response.StatusCode }
  if ($code -eq 200) { Write-Host "$crate 0.9.0 已上架，跳过"; continue }
  powershell -File scripts/build.ps1 publish --locked -p $crate
  if ($crate -eq 'weflow-client') {   # 根包要等 SDK 在索引里可见
    for ($i = 0; $i -lt 60; $i++) {
      $hit = (curl.exe -s -A flow-release-guard https://index.crates.io/we/fl/weflow-client) |
        Select-String -SimpleMatch '"vers":"0.9.0"'
      if ($hit) { break }
      Start-Sleep -Seconds 5
    }
    if (-not $hit) { throw 'SDK 未在索引出现，中止（别在解析不到依赖时发根包）' }
  }
}
```

PyPI 侧**默认没有本机兜底**：本仓按设计不保存任何长期上传凭据（`~/.pypirc` 只有
`[testpypi]`，正式 PyPI 由 CI 走 OIDC 发）。真要本机补发，得先在 `~/.pypirc` 补一个
`[pypi]` 段与对应 token，再进 `clients/python` 执行 `twine upload dist/*`（先查
`pypi.org/pypi/weflow-sdk/0.9.0/json`）；用完请撤掉该凭据，别让它长期躺在本机。

## 注意

- crates.io 与 PyPI **均不可撤销发布**（0.x 也一样，只能 yank）。
- 首发前实测未占用：crates.io 四个名字（本仓两个 crate ＋ 姊妹仓两个）与 PyPI 的
  `weflow-sdk` 当时都是 404。**临近发布日请重测一次**。
- 本地只发布到 testpypi；正式 PyPI 一律由 CI 执行（OIDC，无人持有 token）。
- CI 的 publish 作业对「已发布过」是幂等的（跳过而非报错），因此**重跑作业是安全的**。
