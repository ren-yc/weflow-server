# AGENTS.md

本文件为 AI 编码助手提供本仓库的协作规范。**先读「AI 协作规范」一节**——它是
两个上游仓库与契约仓库共用、逐字一致的公共段。

## 命令

```bash
# 构建 / 测试 / clippy —— 必须走包装脚本：它负责定位 MSVC 环境
powershell -File scripts\build.ps1 build --locked
powershell -File scripts\build.ps1 test --locked
powershell -File scripts\build.ps1 clippy --all-targets --locked -- -D warnings

# 非 Windows 与 CI 上的等价包装
bash scripts/build.sh test --locked

# 安装提交钩子（隐私检查 + 编号引用检查）—— clone 后先跑这个
powershell -ExecutionPolicy Bypass -File scripts/install-hooks.ps1
bash scripts/install-hooks.sh

# 编号引用扫描（钩子调用的是同一支脚本）
python scripts/forbidden_refs.py --tree    # 全量跟踪文件
python scripts/forbidden_refs.py           # staged 新增内容
```

**构建前置**（缺一即失败；包装脚本会明确指出缺哪个）：MSVC 的 `vcvars64.bat`、
**Strawberry Perl**（Git 自带的 MSYS perl 会破坏 Configure 里的 Windows 路径）、`nasm`。
VS 环境脚本可用 `$env:WEFLOW_VCVARS` 覆盖。

**真库探针**以 `#[ignore]` ＋ 环境变量门控，CI 不跑；启用方式见对应测试文件的头部注释。

<!-- common:begin -->
## AI 协作规范（所有开发助手必须遵守）

### 提交前质量门禁

- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`（全量）
- 两处都必须走 `scripts/build.ps1`（或 `.sh`）：它负责定位 MSVC 环境，直接跑 cargo 会在 vendored OpenSSL 上失败
- 新增功能必须补充或更新对应测试
- **不要为了「让当前任务快速完成」而跳过上述任何一步**；门禁失败必须先修复再提交
- **本仓库有意不设 `cargo fmt --check`**（理由见工作流内的注释）。不要以「顺手格式化」为由改动无关文件——那会让 review 淹没在噪声里

### 提交信息

- 格式：Conventional Commits；type 与 scope 用英文，subject 用中文
- **破坏性变更用 `!`**（如 `fix(security)!: …`），并在 body 写明**迁移方式**——只标 `!` 不说怎么迁，等于把成本推给下游
- 说明**行为变化**，不写流水号、不写指向仓库外材料的编号

### 禁止编号引用

- **禁止**在代码注释、文档、提交信息里写入指向**仓库之外一次性材料**的条目号。典型形态：
  - 条目码：单个或两个大写字母 + 数字；
  - 用「复核 / 审查报告 / 审计 / 排期 / 缺陷」等词归因、后面跟一个编号；
  - 本地计划文件名（`*-plan.md`、`PLAN-*.md` 一类）或临时产物目录名；
  - 流水号 + 量词「批」。
- **为什么**：这些编号指向仓库外的文档，仓库读者无法据此还原上下文；报告改版后编号还会失效。注释要说明**为什么这样做**与**不这样做会怎样**；提交信息要说明**行为变化**。
- **替代写法**：把编号换成「**原因 + 失败模式 + 仓库内的回归位置**」，且回归位置必须是一个**能被搜索到的具体名字**（测试函数名 / 断言名 / 用例 id）。写「见相关测试」不合格。
- **豁免**（不视为编号引用）：可跟踪的 issue / PR 编号、编码名与标准编号（UTF-8、RFC 5987）、静态检查码（`clippy::…`）、依赖版本号、控制字符名（C0 / C1）、十六进制字节序列，以及少量固定技术缩写（名单与判据见 `scripts/forbidden_refs.py` 的 `_EXEMPT`）。豁免是**剥离片段后再扫**：同一行夹带的其它编号照常判定。
- **例外**：确需保留某个编号时，在**同一行**写 `allow-plan-ref` 并说明理由。
- **工具与门禁**：`python scripts/forbidden_refs.py`（staged）／`--ref <基线>`（差异）／`--tree`（全量）／`--message-file <路径>`（提交信息）。退出码 `0` 无命中 ／ `1` 命中 ／ **`2` 扫描未执行，拒绝放行——空 diff 不等于干净**。

### 临时文件与一次性产物

- 禁止提交：`tmp_*` 一类的临时文件、以 `_probe` 或 `_review` 结尾的探针与评审目录、`PLAN-*.md` 一类的本地计划文件、`*.log`、本地生成的 `*.db`
- 协作者或其 agent 创建本地计划文件时，**必须写在仓库工作区之外**（系统临时目录或用户主目录）
- 提交前核对：`git status --short` 与 `git ls-files --others --exclude-standard`

### 文档同步义务

- 接口、行为、配置发生变化时，必须回写 `docs/*-api.md` 与 `docs/architecture.md`
- **架构细节写在 `docs/architecture.md`，不得回流到本文件**——本文件只承载协作规则、命令与门禁

### 测试纪律

- 夹具**禁止手工注入 store 字段**——夹具只能**造库**，不能造索引。手工注入会让断言在一个真实运行时不存在的状态上通过
- 真库探针必须 `#[ignore]` ＋ 环境变量门控；CI 不跑它们
- 一致性套件随 `cargo test` **阻塞**，不设观察期

### 钩子安装是 clone 后的第一步

- `bash` 与 `Python` 是**提交路径的硬依赖**：缺失时钩子**报错并阻止提交**，而不是静默跳过——失败开放的检查等于没有检查
- 克隆后先跑 `scripts/install-hooks.ps1`（或 `.sh`）
- `--no-verify` **仅限**「工具确实不可用、且已人工完成等价复核」，并必须在提交信息里写明原因；**不得**用它跳过隐私检查或编号引用检查

### 扫描器豁免清单的维护纪律

- 豁免表**只有一处**：`scripts/forbidden_refs.py` 的 `_EXEMPT`。**不在任何其它文件里另存副本**
- 新增豁免必须同时给出：① 它为什么与编号**同形却不是编号**；② 一个能**区分**二者的判据（而不是逐个列举）
- 禁止用行内 `allow-plan-ref` 绕过「豁免表变更需评审」这一层

### 公共段自身的修改流程

- 公共段**只有一个权威副本**：`flow-contract/AGENTS-common.md`
- 流程：① 先在 `flow-contract` 提 PR 并打 tag → ② **两个仓库同批**提 PR 同步公共段与 pin → ③ CI 的哈希比对必须通过
- **哈希比对失败时，在本仓库内修公共段**使其与所 pin 的 tag 一致；**不得**用「换一个 tag」或改比对脚本的方式让它通过——那正是防漂移机制的失效点
<!-- common:end -->

## 架构指引

微信 4.x 消息库的**只读** HTTP / SSE 服务（默认端口 **5033**）。活库直读 ＋ 内存索引，不落中间数据；数据库密钥由外部提供，本仓库**不做密钥提取**。

- **完整架构文档**：`docs/architecture.md` —— 模块职责、数据流、库与表结构、同步机制、
  服务层、配置、工程与工具链、设计要点与陷阱。**涉及架构的任务先读它。**
- **同步更新义务**：见公共段「文档同步义务」。
- **边界**：本文件只承载协作规则、命令与门禁；**架构细节一律写在 `docs/architecture.md`，
  不要回流到本文件。**
