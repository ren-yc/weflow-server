## 改动说明

<!-- 概括做了什么、为什么。标题用 Conventional Commits 格式（type/scope 英文，subject 中文），
     如 `fix(cursor): 第二段不再冻结在快照边界`。关联 issue 写 `Fixes #123`。
     标题与描述都不要夹带审查报告条目号、计划产物编号或「第 N 批」流水号——仓库读者无法据此回查。 -->

## 改动类型

<!-- 勾选适用的项；一次 PR 可以同时是行为变化加测试加文档。 -->

- [ ] 代码行为变化（feat / fix / refactor）
- [ ] 测试补充或更新（test）
- [ ] 文档（docs）
- [ ] 依赖、工具链或 CI（chore）

## 质量门禁（提交前本地全部通过，与 CI 及 AGENTS.md 对齐）

- [ ] `powershell -File scripts/build.ps1 clippy --all-targets --locked -- -D warnings`
- [ ] `powershell -File scripts/build.ps1 test --locked`（非 Windows 用 `bash scripts/build.sh test --locked`）
- [ ] `python scripts/check_docs.py --tree`（折点）
- [ ] `python scripts/check_docs_structure.py --tree`（结构）
- [ ] `python scripts/forbidden_refs.py --tree`（编号引用；提交信息用 `--message-file`）
- [ ] `bash scripts/check-privacy.sh`（Windows 用 Git Bash；WSL 的 `bash` 把 `whoami` 读成 `ubuntu`，会假红）
- [ ] `git diff --check 4b825dc642cb6eb9a060e54bf8d69288fbee4904..HEAD`（空白错误与冲突标记，空树口径）
- [ ] 改到 `clients/` 时：`cargo test --locked -p weflow-client`；Python SDK 在 `clients/python` 的 venv 里跑 `pytest tests/`
- [ ] 改到生成物时：`powershell -File scripts/build.ps1 run --locked -p weflow-regen -- --check` 与 `cd clients/python && python3 scripts/regen.py --check` 均无 diff
- [ ] 改到契约面时：pin 未变则跑 `--test conformance_runner -- --ignored`；要升 pin 则按 AGENTS.md「公共段」的流程走

## 测试

<!-- 新增或更新了哪些测试、覆盖了什么行为；纯文档改动写「不适用」。
     行为断言要能被搜到：写测试函数名或用例 id，不写「见相关测试」。 -->

## 文档同步

<!-- 端点、参数、配置、错误码、游标语义变化时，必须回写 docs/weflow-server-api.md 与 docs/architecture.md；
     架构细节写进 architecture，不要回流到 AGENTS.md。 -->

- [ ] 不涉及，或已回写 `docs/weflow-server-api.md`
- [ ] 不涉及，或已回写 `docs/architecture.md`
- [ ] 不涉及，或已在 `CHANGELOG.md` 补条目（行为变化 + 迁移方式 + 回归位置）

## 自检

- [ ] `git status --short` 与 `git ls-files --others --exclude-standard` 里没有 `tmp_*`、`*.log`、`*.db`、`weflow-server.json`
- [ ] 只包含与本 PR 相关的改动（没有顺手格式化无关文件）
- [ ] 注释、文档与提交信息里没有指向仓库外材料的编号（`scripts/forbidden_refs.py` 已跑）
