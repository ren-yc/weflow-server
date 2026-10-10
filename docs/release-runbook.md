# 发布手册（v0.9.0 起）

本文是把新版本发上 crates.io 与 PyPI 的操作手册。发布动作由 CI 的 `publish` 作业
自动执行（tag 触发），本手册覆盖它的前置配置、预演与人工兜底路径。

## 前置配置（一次性）

### GitHub Actions secrets（正式发布用）

| Secret | 用途 | 来源 |
|---|---|---|
| `CARGO_REGISTRY_TOKEN` | crates.io 发布 | crates.io → Account Settings → API Tokens（需 publish-weflow-server / publish-weflow-client 权限） |
| `PYPI_TOKEN_WEFLOW` | PyPI 发布 | pypi.org → Account Settings → API tokens（scope 限定到 weflow-sdk 这一个项目；根包只上 crates.io，不上 PyPI） |

### 本机凭据（预演与兜底用）

- cargo：`~/.cargo/credentials.toml`（已配置）。
- PyPI：`~/.pypirc`（已配置；testpypi 与正式 PyPI 各自独立的 token）。

## 正式发布流程

1. 确认 `Cargo.toml` 根包与 `clients/rust` 版本一致（0.9.0），CHANGELOG 有对应段。
   **并把 `## [0.9.0]` 的日期回填成实际发布日**（与 tag 同提交）——准备阶段写的是准备日，
   Keep a Changelog 的段日期应当是发布日。
2. `git tag v0.9.0 && git push origin v0.9.0`。
3. CI 链：guard（tag/版本一致性）→ quality-gate（双 OS clippy＋test＋契约 nails）
   → build（三平台产物）→ release（GitHub Release）→ **publish**。publish 刻意
   `needs: [guard, build, release]`：发布不可撤销，GitHub Release 的产物没齐之前
   不把包发出去。publish 内部＝crates.io 先 `weflow-client`、轮询稀疏索引确认条目可见、
   后发 `weflow-server`（根包的依赖声明带 `version = "0.9.0"`，要求 registry 上已存在
   该 SDK 版本）；随后 PyPI 上传 `weflow-sdk`。
4. 核对：crates.io 两个 crate 页、PyPI 项目页、GitHub Release 三平台产物。
5. 发布后：重建 sdk-dist 供应分支——
   `git subtree split -P clients/python/src/weflow_sdk -b sdk-dist` ＋
   `git push origin sdk-dist --force-with-lease`；再同步 briefdesk 的 vendor 与门禁。

## 预演

### PyPI（testpypi，本机）

```
cd clients/python
python -m build
twine check dist/*
twine upload --repository testpypi dist/*
# 验证可安装与可导入：
pip install --index-url https://test.pypi.org/simple/ --no-deps weflow-sdk==0.9.0
```

### crates.io（本机，只打包不上传）

```
cd clients/rust && cargo package --list --allow-dirty && cd ..
```

根包同理（在仓库根执行）可核对文件清单。**注意**：`cargo publish --dry-run` 对根包在
SDK 真上架前必然失败（它按发布后的清单解析依赖，那时 registry 上还找不到
`weflow-client = "0.9.0"`）——这不是缺陷，是发布顺序的直接后果。

## 人工兜底（CI publish 不可用时）

顺序与 CI 相同，全部本机执行。**每一步之前先查该版本是否已上架**（crates.io／PyPI
都拒绝重复版本；CI 里的幂等守卫在这里换成手工版）：

```
curl -s -o /dev/null -w '%{http_code}\n' https://crates.io/api/v1/crates/weflow-client/0.9.0
cd clients/rust   && cargo publish --locked   # 200 则跳过本步
# 轮询稀疏索引到条目可见（上传成功不等于索引可解析）：
until curl -sf https://index.crates.io/we/fl/weflow-client | grep -q '"vers":"0.9.0"'; do sleep 5; done
cd ..             && cargo publish --locked   # 同上，先查 weflow-server
cd clients/python && twine upload dist/*     # 先查 pypi.org/pypi/weflow-sdk/0.9.0/json
```

## 注意

- crates.io 与 PyPI **均不可撤销发布**（0.x 也一样，只能 yank）。
- 首发前实测未占用：crates.io 四个名字（本仓的 weflow-server／weflow-client，
  加上姊妹仓的 qqflow-server／qqflow-client）与 PyPI 的 weflow-sdk 当时都是 404。
  **临近发布日请重测一次**。
- 本地只发布到 testpypi；正式 PyPI 一律由 CI 执行（凭据在 Repo secrets）。
- CI 的 publish 作业对「已发布过」是幂等的（跳过而非报错），因此**重跑作业是安全的**；
  人工兜底路径没有这道守卫，照上面每一步先查再发。
