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

1. 版本号有**五处**要一起改，CI 的 guard 只兜第一处（根包）：`Cargo.toml`（根包）、
   `clients/rust/Cargo.toml`（SDK crate）、`clients/python/pyproject.toml`（Python SDK）、
   `clients/ts/package.json` ＋ `clients/ts/package-lock.json`（示例，不发 npm 但进页面与包）。
   另有**生成物与快照内嵌版本号**不能手改、只能靠重新生成对齐：`clients/rust/src/generated/gen.rs`、
   `clients/python/src/*_sdk/generated/**`（spec.json ＋ 每个模块 docstring）、`tests/golden/` 下的
   `openapi.json` **与 `health.json`／`health-alias.json`**（三份都带版本串）；`Cargo.lock` 里本包那条
   由 `cargo update -w` ＋ `--locked` 兜住。改完版本号**必须**重跑两仓 regen ＋ golden（`UPDATE_GOLDEN=1`）——
   否则 `regen --check` 与快照比对会在 push 时红，那正是版本链完整性的唯一机器判据。
   `CHANGELOG.md` 的 `## [0.9.0]` 段日期**回填成实际发布日**并与 tag 同提交：准备阶段写的是
   准备日，Keep a Changelog 的段日期应当是发布日。
2. **先 `git push origin master`、等那条 check 变绿，再打 tag**——这一步不能省，也不能只推 tag。
   `check.yml` 的触发是 `push: branches: [master]`，**推 tag 不触发它**；而 release 链的 quality-gate
   只有 clippy＋test＋契约 nails，比 check.yml 少十几道门（编号引用扫描、公共段哈希、embed 例子、
   **Rust/Python 的 regen --check**、typed SDK 测试、ruff、构建脚本透传测试、完整契约套件）。
   regen --check 那道守的是「陈旧的 gen.rs／Python 生成树被不可撤销地发上 registry」——本流程里
   最能安静发错东西的口子，且 tag 链**不跑**它。所以顺序固定为：
   `git push origin master` → 等 check 绿（它跑的就是 tag 将要指向的提交）→
   `git tag v0.9.0` ＋ `git push origin v0.9.0`（Windows PowerShell 5.1 不认 `&&`，分两行）。
3. CI 跑到 publish 时停在环境审批 → 你放行。作业内部顺序：先 SDK crate、轮询
   crates.io 稀疏索引确认条目可见、再发根包（根包的依赖声明带 `version`，registry
   上必须先有那个版本的 SDK）；PyPI 侧构建 wheel＋sdist 后用 pypa 官方动作上传。
   **首次发新 crate 时，这个 publish 作业必定在 SDK 那一步红**：本仓刻意不在 CI 里放
   crates.io 长期凭据（`CARGO_REGISTRY_TOKEN` 未配置），新 crate 的首发按 crates.io 的要求
   只能本机手工做一次。红的这步会打印下一步指引；此时 **GitHub Release 已经建好并公开**
   （release 作业排在 publish 之前），而 crates.io/PyPI 还什么都没有——这是**预期状态**，
   不是事故。照「首发（本机）」一节发完两个 crate，再**重跑这个失败的 publish 作业**，
   幂等守卫会跳过已上架的、继续往下走。
4. 核对：crates.io 两个 crate 页、PyPI 项目页、GitHub Release 三平台产物。
5. 发布后：重建 sdk-dist 供应分支——
   `git subtree split -P clients/python/src/weflow_sdk -b sdk-dist` ＋
   `git push origin sdk-dist --force-with-lease`；再同步 briefdesk 的 vendor 与门禁。

## 首发（本机，只做一次）

新 crate 的首发。顺序＝先 SDK 后根包（根包要解析 registry 上的 SDK 版本）。**在仓库根、PowerShell
里整块执行**（`curl.exe` 在 Windows 上是真 curl，不是 PowerShell 别名）；cargo 一律走包装脚本——
它负责定位 MSVC 环境，直接跑 cargo 会在 vendored OpenSSL 上失败（`publish` 子命令不在注入
`--features testing` 的名单里，那条注入只服务 test/clippy）。

这两条命令**刻意不带** `--allow-dirty`：首发要发的就是提交里的那份内容，工作树有未提交改动时
应当先提交再发。被挡住时 cargo 的原话是（实测，逐字）：

> `error: 3 files in the working directory contain changes that were not yet committed into git:`
> 后跟文件清单，再跟 `to proceed despite this and include the uncommitted changes, pass the`
> ``--allow-dirty` flag`

那是它在替你把关，别用 `--allow-dirty` 绕过。**作用域不对称**（实测）：脏检查以**各 package
自己的目录**为界——根包的 package root 就是仓库根，因此根级任何未提交跟踪文件（包括本手册）
都会挡住它；而 `-p <仓名>-client` 的根在 `clients/rust`，仓库根有未提交改动时那一步照样通过
（实测退出码 0）。所以**别把「SDK 那步没报错」当成工作树干净**，两都要看过。

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
  PyPI 侧同理：pypa 动作带 `skip-existing`，上传中途失败重跑会跳过已传的文件、补齐其余的。
  **唯一的例外**：若某个 dist 是用**别的路径**（本机 `twine upload`、或曾关掉 attestations）
  传上去的，重跑会永久跳过它、不再补构建证明——所以正式 PyPI 一律走 CI，别在本机补传。
- **其余不可逆点**（除 registry 外）：`git tag v0.9.0` 推上去后删/移得干净、但 GitHub Release
  的资产不会随之消失；`release` 作业在 publish **之前**就把 Release 建好并公开了（见上）；
  sdk-dist 供应分支的 `--force-with-lease` 是重写历史。三者都不像 registry 那样不可撤，
  但都会留下公开痕迹，动手前想清楚。

## tag 之后 CI 红了怎么办

按触发顺序，越靠前越该停下来修、而不是硬着头皮往下发：

- **guard 红**（tag 与根包版本不一致）：说明 tag 打错或版本号没对齐。此时**什么都没发布**、
  Release 也还没建。改正提交 → `git push origin master` → 等 check 绿 → 删旧 tag 重打：
  `git tag -d v0.9.0` ＋ `git push origin :refs/tags/v0.9.0`（实测 PS 5.1 下 `--delete v0.9.0` 也原样
  透传、两种写法等效，冒号式只是与「推 tag」对称、少记一条参数形态）→ 再按第 2 步打 tag 推 tag。
- **quality-gate / build / release 红**：同上去掉那个 tag、修好、重推。**别**在红的中间态
  去手工补发 registry——那样 GitHub Release 与 registry 会长期不一致。
- **publish 红在 crates.io SDK 步**（首次新 crate）：按「首发（本机）」发完，重跑该作业即可。
- **publish 红在 PyPI 上传步**：先分清是「OIDC 换凭据失败」（多半是 pypi.org 的 pending
  publisher 没登记，或 environment 名不是 `pypi`／workflow 名不是 `release.yml`）还是
  「上传本身失败」。前者去 pypi.org 补登记后重跑；后者直接重跑（`skip-existing` 兜幂等）。
