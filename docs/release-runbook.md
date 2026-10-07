# 发布手册（v0.9.0 起）

本文是把新版本发上 crates.io 与 PyPI 的操作手册。发布动作由 CI 的 `publish` 作业
自动执行（tag 触发），本手册覆盖它的前置配置与人工兜底路径。

## 前置配置（一次性）

### GitHub Actions secrets（正式发布用）

| Secret | 用途 | 来源 |
|---|---|---|
| `CARGO_REGISTRY_TOKEN` | crates.io 发布 | crates.io → Account Settings → API Tokens（需 publish-weflow-server / publish-weflow-client 权限） |
| `PYPI_TOKEN_WEFLOW` | PyPI 发布 | pypi.org → Account Settings → API tokens（scope 限定到本项目的两个包） |

### 本机凭据（预演与兜底用）

- cargo：`~/.cargo/credentials.toml`（已配置）。
- PyPI：`~/.pypirc`（已配置；testpypi 与正式 PyPI 各自独立的 token）。

## 正式发布流程

1. 确认 `Cargo.toml` 根包与 `clients/rust` 版本一致（0.9.0），CHANGELOG 有对应段。
2. `git tag v0.9.0 && git push origin v0.9.0`。
3. CI 链：guard（tag/版本一致性）→ quality-gate（双 OS clippy＋test＋契约 nails）
   → build（三平台产物）→ release（GitHub Release）→ **publish**（crates.io：先
   `weflow-client` 后 `weflow-server`，根包依赖声明里的 `version = "0.9.0"` 要求 registry
   上已存在该 SDK 版本；随后 PyPI 上传 `weflow-sdk`）。
4. 核对：crates.io 两个 crate 页、PyPI 项目页、GitHub Release 三平台产物。
5. 发布后：重建 sdk-dist 供应分支——
   `git subtree split -P clients/python/src/weflow_sdk -b sdk-dist` ＋
   `git push origin sdk-dist --force-with-lease`。

## 预演（testpypi，本机执行）

```
cd clients/python
python -m build
twine check dist/*
twine upload --repository testpypi dist/*
# 验证可安装与可导入：
pip install --index-url https://test.pypi.org/simple/ --no-deps weflow-sdk==0.9.0
```

## 人工兜底（CI publish 不可用时）

顺序与 CI 相同，全部本机执行：

```
cd clients/rust   && cargo publish --locked
# 等 crates.io 索引收编（约 30s）后：
cd ../..          && cargo publish --locked
cd clients/python && twine upload dist/*
```

## 注意

- crates.io 与 PyPI **均不可撤销发布**（0.x 也一样，只能 yank）。
- PyPI 项目名 weflow-sdk／对应服务端名均在首发前实测未占用。
- 本地只发布到 testpypi；正式 PyPI 一律由 CI 执行（凭据在 Repo secrets）。
