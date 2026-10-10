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
首发成功后建议升级成绑定 `weflow-sdk` 的正式 publisher，并勾上 latest 标记
（否则后续版本在页面上会被标成「非最新发布版」）。TestPyPI 预演同理：那边的
pending publisher 的环境名要与 workflow 里 testpypi 一步所用的 environment 名一致。

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
2. `git tag v0.9.0 && git push origin v0.9.0`。
3. CI 跑到 publish 时停在环境审批 → 你放行。作业内部顺序：先 SDK crate、轮询
   crates.io 稀疏索引确认条目可见、再发根包（根包的依赖声明带 `version`，registry
   上必须先有那个版本的 SDK）；PyPI 侧构建 wheel＋sdist 后用 pypa 官方动作上传。
4. 核对：crates.io 两个 crate 页、PyPI 项目页、GitHub Release 三平台产物。
5. 发布后：重建 sdk-dist 供应分支——
   `git subtree split -P clients/python/src/weflow_sdk -b sdk-dist` ＋
   `git push origin sdk-dist --force-with-lease`；再同步 briefdesk 的 vendor 与门禁。

## 首发（本机，只做一次）

新 crate 的首发。顺序＝先 SDK 后根包（根包要解析 registry 上的 SDK 版本）：

```
cd clients/rust && cargo publish --locked
until curl -sf -A flow-release-guard https://index.crates.io/we/fl/weflow-client | grep -q '"vers":"0.9.0"'; do sleep 5; done
cd .. && cargo publish --locked
```

（`index.crates.io/<前两字符>/<第3-4字符>/<crate名>` 是稀疏索引路径规则。）
做完回到第 3 步重跑 publish 作业，它会跳过已上架的两步、继续跑 PyPI。

## 预演

### PyPI（testpypi，本机）

```
cd clients/python
python -m build
twine check dist/*
twine upload --repository testpypi dist/*
pip install --index-url https://test.pypi.org/simple/ --no-deps weflow-sdk==0.9.0
```

### crates.io（本机，只打包不上传）

```
cd clients/rust && cargo package --list --allow-dirty
```

在仓库根执行同样命令可核对根包清单。**注意**：`cargo publish --dry-run` 对根包在
SDK 真上架前必然失败——它按发布后的清单解析依赖，那时 registry 上还没有
`weflow-client = "0.9.0"`。这不是缺陷，是发布顺序的直接后果。

## 人工兜底（CI publish 不可用时）

每一步之前先查该版本是否已上架（两个索引都拒绝重复版本）：

```
curl -s -o NUL -w '%{http_code}\n' -A flow-release-guard https://crates.io/api/v1/crates/weflow-client/0.9.0
cd clients/rust   && cargo publish --locked     # 200 则跳过
until curl -sf -A flow-release-guard https://index.crates.io/we/fl/weflow-client | grep -q '"vers":"0.9.0"'; do sleep 5; done
cd ..             && cargo publish --locked     # 同上，先查 weflow-server
cd clients/python && twine upload dist/*       # 先查 pypi.org/pypi/weflow-sdk/0.9.0/json
```

## 注意

- crates.io 与 PyPI **均不可撤销发布**（0.x 也一样，只能 yank）。
- 首发前实测未占用：crates.io 四个名字（本仓两个 crate ＋ 姊妹仓两个）与 PyPI 的
  `weflow-sdk` 当时都是 404。**临近发布日请重测一次**。
- 本地只发布到 testpypi；正式 PyPI 一律由 CI 执行（OIDC，无人持有 token）。
- CI 的 publish 作业对「已发布过」是幂等的（跳过而非报错），因此**重跑作业是安全的**。
