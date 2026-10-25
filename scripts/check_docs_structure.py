#!/usr/bin/env python3
"""结构门禁 —— 把 AGENTS.md「文档排版」里原本无人执行的规则变成可机器判定的检查。

背景：13 条排版规则里只有第 2 条（折点，scripts/check_docs.py）有守卫。实测结果是
规则事实上失效：14 个主文档有 165 个散文段超过 150 字上限、200 余个列表项超过 200 字、
2 个 300 行以上的文档没有目录、两仓端点小节的层级与标题形态完全不同。本脚本补上这些判据。
「失败开放的检查等于没有检查」——因此扫描未执行时返回 2，而不是当作通过；未闭合的代码围栏
或 HTML 注释会让其后整篇脱离检查，同样判红（见第 7 类）。

检查项（六类）与**豁免面**（豁免必须显式写在这里，否则规则会退化成「看人下菜」）：

  1. 目录存在        行数 >= --max-toc-lines（默认 300）的文档须有「## 目录」，且其中
                     含 >= 5 条 `](#…)` 锚点链接。
                     豁免：CHANGELOG.md（时间序倒排，目录无意义）、spec/**（上游哈希锁定快照）
  2. 散文段预算      普通段落 <= --max-para 字（默认 150）。列表项不计入此项。
                     豁免：spec/**
  3. 列表项长度      列表项 <= --max-bullet 字（默认 200）。
                     豁免：CHANGELOG.md（规则 13 单独口径）、spec/**
                     （AGENTS.md 的 `## AI 协作规范` 公共段若超限，只能改上游权威副本再升 pin）
  4. 端点骨架        api 文档里每个端点小节须含 `**请求**`、参数指示（`**参数**` 开头的一行，
                     含「**参数**：无」这种写法）、以及至少一个围栏代码块。
                     豁免：小节内含 `<!-- no-skeleton -->` 的（迁移期逐节补齐时用）
  5. 节首结论        每个 `##`（「目录」除外）的首个内容行以 。／：／） 收尾。
                     模式：jump（默认，只查「首个内容行是小节标题」的跳级病灶）／strict（全查）／off
                     豁免：CHANGELOG.md（版本标题下本就该是 `### 新增`）、spec/**、AGENTS.md（其
                     `## AI 协作规范` 在 common 公共段内）
  6. 端点层级与形态  api 文档里端点小节须为 `###`，且标题以 `VERB `/path`` 开头。
                     豁免：--allow-levels（迁移期，允许层级尚未统一的中间态）
  7. 围栏/注释闭合   代码围栏（``` / ~~~）与 HTML 注释（<!-- -->）必须闭合。未闭合会让其后的
                     整篇内容**静默脱离**上面六类检查（实测：中部少一个闭围栏，之后 400 字
                     超长段得到 0 命中）——这正是「失败开放」。故未闭合一律判红。

仅对 `docs/*-api.md` 施加第 4、6 项——其它文档（quickstart、README、mcp）里有大量
http 示例片段，按端点口径要求它们只会产生假阳性。

块切分算法（与三仓文档重构计划的量化基线口径逐字一致，是本脚本最易误报之处）：

  * 围栏（``` / ~~~）内不参与判定；围栏本身是边界。
  * HTML 注释（<!-- … -->）不参与判定；注释本身是边界。
  * 空行、ATX 标题、表格行（`^\\s*\\|`）、引用块（`^\\s*>`）、分隔线（`---` 等）都是边界。
  * **列表项** = 标记行（`^\\s*([-*+]|\\d{1,3}[.)])\\s+`）＋ 紧随其后的、**无空行分隔**的
    懒续行。**空行之后的缩进块另起一个散文块**——它与列表项在渲染上是两个段落，按散文段
    口径判定。（这正是 weflow api「SyncClient」段、qqflow api 联系人段、两仓 CHANGELOG
    「后续演进」段的真实形态：顶格或缩进两格、前面隔一个空行。）
  * 其余连续非空行合并为一个散文块。
  * 字数 = 各行 `strip()` 后拼接的**字符数**（CJK 计 1，不按显示列）。
  * 列表项 200 字上限是规则 4「bullet 至多 3 行」的**字符近似**（约 50–65 字/行 × 3），
    不是精确的渲染行数——它的目的是挡住 106 字×5 行那种「粗体引子 + 多行论述」的伪列表。
  * 只认**围栏**代码块（``` / ~~~），不认 4 空格缩进代码块——本仓文档的代码一律用围栏；
    缩进块会被当作散文段。若将来出现缩进代码块，按散文段判定会偏严（宁严勿漏）。

用法：
  python scripts/check_docs_structure.py                       # staged 的 .md 改动（pre-commit）
  python scripts/check_docs_structure.py --ref origin/master    # 相对基线的差异（CI）
  python scripts/check_docs_structure.py --tree                # 全量跟踪 .md（CI 终验）
  python scripts/check_docs_structure.py --stats --tree        # 只打印统计、不判定（基线核对）

退出码：0 = 无命中；1 = 命中；2 = 扫描未执行，**拒绝放行**（空 diff ≠ 干净）。
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

if hasattr(sys.stdout, "reconfigure"):
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

_HEADING = re.compile(r"^\s{0,3}(#{1,6})\s")
_TABLE = re.compile(r"^\s*\|")
_QUOTE = re.compile(r"^\s*>")
_FENCE = re.compile(r"^\s*(```|~~~)\s*([A-Za-z0-9_+-]*)")
_HR = re.compile(r"^\s{0,3}(-{3,}|\*{3,}|_{3,})\s*$")
_LIST = re.compile(r"^(\s*)([-*+]|\d{1,3}[.)])\s+")
_TOC_ANCHOR = re.compile(r"\]\(#")

# 端点标题的**规范形态**：`GET `/path`` / `GET/POST `/path`` —— VERB 在前、路径在反引号里。
_METHODS = r"(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)"
_CANON_VERB = re.compile(rf"^{_METHODS}(?:/{_METHODS})?\s+`/")
# 端点**识别**（宽松）：只要标题里出现「方法 + 空白 + /路径」就算，涵盖三种历史写法——
#   * 规范式：`GET `/api/v1/x``
#   * 整包式：`` `GET /chatlab/sessions` ``
#   * 括注式：`健康检查（GET /health）` / `接口描述（GET /openapi.json，免鉴权）`
# 识别用宽松式、形态检查仍用上面的严格式，于是这些写法会被判为「标题不是规范形态」，
# 正是骨架统一要改的对象。只在**标题**里找，不扫正文——否则「注意事项」里提到 POST 也会中招。
_DETECT_METHOD_IN_TITLE = re.compile(rf"{_METHODS}\s+`?/")

API_DOC = re.compile(r"(^|/)docs/[^/]+-api\.md$")
EXEMPT_TOC = re.compile(r"(^|/)CHANGELOG\.md$|(^|/)spec/")
EXEMPT_BULLET = re.compile(r"(^|/)CHANGELOG\.md$|(^|/)spec/")
EXEMPT_PARA = re.compile(r"(^|/)spec/")
# 节首结论豁免：
#   * CHANGELOG.md —— 规则 13 单独口径；版本标题下第一个内容行本就该是 `### 新增`/`### 修复`
#     这类子标题，按「跳级」判它是把版本条目结构当成病灶。
#   * spec/** —— 上游哈希锁定快照，不得改。
#   * AGENTS.md —— `## AI 协作规范` 属 `common:begin/end` 公共段，由 flow-contract 维护，
#     仓内直改会被 check_common_section.py 判红；它的结论行只能在上游修。
EXEMPT_H2 = re.compile(r"(^|/)CHANGELOG\.md$|(^|/)spec/|(^|/)AGENTS\.md$")


def split_lines(text: str) -> list[str]:
    """按 \\n 切行并去掉末尾的空行，使行数与编辑器里看到的一致（无尾空行 +1 伪影）。"""
    lines = text.replace("\r\n", "\n").split("\n")
    while lines and lines[-1] == "":
        lines.pop()
    return lines


def classify(lines: list[str]):
    """**唯一的**行分类器：所有消费者（切块、提标题、找端点、查闭合）都从这里取同一份结论。

    返回 (kinds, openings, in_fence_end, in_comment_end, fence_at, comment_at)：
      * kinds[i] ∈ {fence, comment, blank, heading, table, quote, hr, list, prose}
      * openings = {行号: 围栏 info string}，**只含开栏行**（闭栏行也是 kind=fence，但不在 openings）
      * in_fence_end / in_comment_end：扫描到末尾时是否仍处于围栏 / 注释内（未闭合为真）
      * fence_at / comment_at：未闭合时，对应开栏 / 开注释的行号（0 基）

    为什么必须只有一个分类器：此前切块、提标题、找端点三个消费者各自维护一份围栏/注释状态机，
    且**判定顺序不一致**（一个先判围栏、一个先判注释）。于是「HTML 注释块内含奇数个围栏行」时两份
    状态机结论相反：闭合检查认为一切正常，切块却把其后整篇当围栏静默跳过——「失败开放」换个入口
    又回来了。统一成一个 tokenizer 后这类分歧从根上不存在。
    顺序固定为**先注释、后围栏**：注释里的 ``` 是注释内容，不该翻转围栏状态。
    """
    kinds: list[str] = []
    openings: dict[int, str] = {}
    in_fence = False
    in_comment = False
    fence_at = None
    comment_at = None
    for idx, raw in enumerate(lines):
        line = raw.rstrip("\r")
        if in_comment:
            kinds.append("comment")
            if "-->" in line:
                in_comment = False
            continue
        if in_fence:
            kinds.append("fence")
            if _FENCE.match(line):
                in_fence = False
            continue
        if line.lstrip().startswith("<!--"):
            kinds.append("comment")
            if "-->" not in line:
                in_comment = True
                comment_at = idx
            continue
        m = _FENCE.match(line)
        if m:
            kinds.append("fence")
            in_fence = True
            fence_at = idx
            openings[idx] = m.group(2) or ""
            continue
        if not line.strip():
            kinds.append("blank")
        elif _HEADING.match(line):
            kinds.append("heading")
        elif _HR.match(line):
            kinds.append("hr")
        elif _TABLE.match(line):
            kinds.append("table")
        elif _QUOTE.match(line):
            kinds.append("quote")
        elif _LIST.match(line):
            kinds.append("list")
        else:
            kinds.append("prose")
    return kinds, openings, in_fence, in_comment, fence_at, comment_at


def blocks(lines: list[str]):
    """切块，返回 [(kind, start_index0, [lines...]), ...]，kind ∈ {'prose','item'}。

    列表项与散文块的分界见模块 docstring；这里只实现「空行 = 边界」这一条核心口径。
    """
    kinds, *_ = classify(lines)
    out = []
    i, n = 0, len(lines)
    while i < n:
        k = kinds[i]
        if k in ("fence", "comment", "blank", "heading", "table", "quote", "hr"):
            i += 1
            continue
        blk = [lines[i].rstrip("\r")]
        j = i + 1
        while j < n and kinds[j] == "prose":      # 懒续行（含缩进续行）并入本块
            blk.append(lines[j].rstrip("\r"))
            j += 1
        out.append(("item" if k == "list" else "prose", i, blk))
        i = j
    return out


def block_len(blk: list[str]) -> int:
    return sum(len(l.strip()) for l in blk)


def headings(lines: list[str]):
    """返回 [(line_index0, level, title)]，围栏内与 HTML 注释内的伪标题都被跳过。"""
    kinds, *_ = classify(lines)
    out = []
    for idx, k in enumerate(kinds):
        if k != "heading":
            continue
        m = _HEADING.match(lines[idx])
        if m:
            out.append((idx, len(m.group(1)), lines[idx].strip()[len(m.group(1)):].strip()))
    return out


def toc_check(lines: list[str], path: str) -> list[str]:
    hs = headings(lines)
    toc_idx = [idx for idx, lvl, title in hs if lvl == 2 and title == "目录"]
    problems = []
    if not toc_idx:
        problems.append(f"{path}: 行数 {len(lines)} >= 阈值但缺「## 目录」小节")
        return problems
    start = toc_idx[0]
    body = []
    for idx in range(start + 1, len(lines)):
        if _HEADING.match(lines[idx]) and len(_HEADING.match(lines[idx]).group(1)) <= 2:
            break
        body.append(lines[idx])
    n_anchor = sum(1 for l in body if _TOC_ANCHOR.search(l))
    if n_anchor < 5:
        problems.append(f"{path}: 「## 目录」只有 {n_anchor} 条锚点链接（< 5）")
    return problems


def h2_conclusion(lines: list[str], path: str, mode: str) -> list[str]:
    if mode == "off":
        return []
    hs = headings(lines)
    problems = []
    for k, (idx, lvl, title) in enumerate(hs):
        if lvl != 2 or title == "目录":
            continue
        # 本节边界 = 下一个 level <= 2 的标题（子标题属于本节，不能拿它当边界，
        # 否则「## X 紧跟一个 ### 子标题」这种跳级病灶永远落在边界外、查不出来）。
        end = len(lines)
        for idx2, lvl2, _t2 in hs[k + 1:]:
            if lvl2 <= 2:
                end = idx2
                break
        first = None
        for j in range(idx + 1, end):
            if lines[j].strip():
                first = lines[j]
                break
        if first is None:
            continue
        if _HEADING.match(first):
            problems.append(f"{path}:{idx + 1}: 「## {title}」首个内容是子标题（跳级缺结论）")
        elif mode == "strict" and not first.rstrip().endswith(("。", "：", "）", ")", "!", "！", "?", "？")):
            problems.append(f"{path}:{idx + 1}: 「## {title}」首个内容行不是结论句：{first.strip()[:40]!r}")
    return problems


def endpoint_sections(lines: list[str]):
    """返回可判定为「端点小节」的 [(line_idx, level, title, body_lines)]。

    识别依据（任一即可，宁可多认不可漏认——漏认等于给违规开天窗）：
      * 标题里出现「方法 + 路径」（规范式 / 整包反引号式 / 括注式）；或
      * 小节正文里有 info string 为 http 的围栏代码块（无方法名的端点小节即此形态）。
    """
    hs = headings(lines)
    _kinds, openings, *_ = classify(lines)
    out = []
    for k, (idx, lvl, title) in enumerate(hs):
        if lvl > 3:
            continue
        start = idx + 1
        end = hs[k + 1][0] if k + 1 < len(hs) else len(lines)
        body = lines[start:end]
        has_http = any(openings.get(j, "").lower() == "http" for j in range(start, end))
        if _DETECT_METHOD_IN_TITLE.search(title) or has_http:
            out.append((idx, lvl, title, body))
    return out


def skeleton_check(lines: list[str], path: str) -> list[str]:
    if not API_DOC.search(path):
        return []
    problems = []
    for idx, lvl, title, body in endpoint_sections(lines):
        text = "\n".join(body)
        if "<!-- no-skeleton -->" in text:
            continue
        missing = []
        if "**请求**" not in text:
            missing.append("**请求**")
        if not re.search(r"^\s*\*\*参数\*\*", text, re.M):
            missing.append("**参数**")
        if not re.search(r"^\s*```", text, re.M):
            missing.append("围栏代码块")
        if missing:
            problems.append(f"{path}:{idx + 1}: 端点「{title}」缺 {'、'.join(missing)}")
    return problems


def level_check(lines: list[str], path: str, allow_levels: bool) -> list[str]:
    if not API_DOC.search(path) or allow_levels:
        return []
    problems = []
    for idx, lvl, title, _body in endpoint_sections(lines):
        if lvl != 3:
            problems.append(f"{path}:{idx + 1}: 端点「{title}」在 h{lvl}（应为 h3）")
        if not _CANON_VERB.match(title):
            problems.append(f"{path}:{idx + 1}: 端点标题不是 `VERB `/path`` 形态：{title!r}")
    return problems


def fence_state(lines: list[str]):
    """返回 (in_fence, in_comment, fence_line0, comment_line0)，用于检测未闭合的围栏/注释。

    未闭合是**失败开放**的源头：blocks()/headings()/endpoint_sections() 都会在围栏或注释里
    静默跳过其后全部内容，于是漏一个结束标记就让整篇后半段失去所有检查、且没有任何信号。
    直接复用 classify()，与所有其它消费者共用**同一份**状态机——否则「两套状态机判定顺序不同」
    会让某类输入在闭合检查里显示正常、在切块时却把其后整篇静默跳过。
    """
    _kinds, _openings, in_fence, in_comment, fence_at, comment_at = classify(lines)
    return in_fence, in_comment, fence_at, comment_at


def closure_check(lines: list[str], path: str) -> list[str]:
    in_fence, in_comment, fence_at, comment_at = fence_state(lines)
    problems = []
    if in_fence:
        problems.append(f"{path}:{fence_at + 1}: 代码围栏未闭合（其后内容不被检查）")
    if in_comment:
        problems.append(f"{path}:{comment_at + 1}: HTML 注释未闭合（其后内容不被检查）")
    return problems


def scan_file(text: str, path: str, cfg) -> list[str]:
    lines = split_lines(text)
    problems = []
    # 闭合性先查：未闭合会让后面所有检查「看着通过其实没查」，必须最先暴露。
    problems.extend(closure_check(lines, path))
    if not EXEMPT_PARA.search(path):
        for kind, start, blk in blocks(lines):
            if kind == "prose":
                ln = block_len(blk)
                if ln > cfg.max_para:
                    problems.append(f"{path}:{start + 1}: 散文段 {ln} 字 > {cfg.max_para}")
    if not EXEMPT_BULLET.search(path):
        for kind, start, blk in blocks(lines):
            if kind == "item":
                ln = block_len(blk)
                if ln > cfg.max_bullet:
                    problems.append(f"{path}:{start + 1}: 列表项 {ln} 字 > {cfg.max_bullet}")
    if not EXEMPT_TOC.search(path) and len(lines) >= cfg.max_toc_lines:
        problems.extend(toc_check(lines, path))
    if not EXEMPT_H2.search(path):
        problems.extend(h2_conclusion(lines, path, cfg.h2))
    problems.extend(skeleton_check(lines, path))
    problems.extend(level_check(lines, path, cfg.allow_levels))
    return problems


def stats_file(text: str, path: str) -> dict:
    """基线统计。固定按默认阈值（150/300/200）计——它的用途是「与历史基线对照」，
    不是替代真正的门禁判定（后者才用 --max-para/--max-bullet）。"""
    lines = split_lines(text)
    bl = blocks(lines)
    prose = [block_len(b) for k, _, b in bl if k == "prose"]
    items = [block_len(b) for k, _, b in bl if k == "item"]
    return {
        "path": path,
        "lines": len(lines),
        "prose_n": len(prose),
        "prose_gt_max": sum(1 for v in prose if v > 150),
        "prose_gt_300": sum(1 for v in prose if v > 300),
        "prose_max": max(prose) if prose else 0,
        "item_n": len(items),
        "item_gt_200": sum(1 for v in items if v > 200),
        "item_max": max(items) if items else 0,
        "toc": bool([1 for idx, lvl, t in headings(lines) if lvl == 2 and t == "目录"]),
        "endpoints": len(endpoint_sections(lines)) if API_DOC.search(path) else 0,
    }


def git(args):
    r = subprocess.run(["git", *args], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip() or "git 命令失败")
    return r.stdout


def tracked_md():
    return [p for p in git(["ls-files", "*.md"]).splitlines() if p.strip()]


def changed_md_staged():
    out = git(["diff", "--cached", "--name-only", "--diff-filter=ACM", "--", "*.md"])
    return [p for p in out.splitlines() if p.strip()]


def changed_md_ref(ref):
    out = git(["diff", "--name-only", f"{ref}...HEAD", "--", "*.md"])
    return [p for p in out.splitlines() if p.strip()]


def main(argv=None):
    ap = argparse.ArgumentParser(description="文档结构门禁（目录/段落/列表项/骨架/结论/层级）")
    ap.add_argument("--ref", default=None, help="扫描相对该基线的差异（CI 用）")
    ap.add_argument("--tree", action="store_true", help="全量扫描跟踪的 .md")
    ap.add_argument("--max-para", type=int, default=150, help="散文段字符上限（默认 150）")
    ap.add_argument("--max-bullet", type=int, default=200, help="列表项字符上限（默认 200）")
    ap.add_argument("--max-toc-lines", type=int, default=300, help="超过多少行必须有目录（默认 300）")
    ap.add_argument("--require-h2-conclusion", dest="h2", default="jump",
                    choices=["jump", "strict", "off"], help="节首结论检查模式（默认 jump）")
    ap.add_argument("--allow-levels", action="store_true", help="迁移期：不检查端点层级与标题形态")
    ap.add_argument("--stats", action="store_true", help="只打印统计、不判定")
    ap.add_argument("--json", action="store_true", help="与 --stats 合用，输出 JSON")
    args = ap.parse_args(argv)

    try:
        if args.tree:
            files, target = tracked_md(), "全量跟踪 .md"
        elif args.ref:
            files, target = changed_md_ref(args.ref), f"相对 {args.ref} 的差异"
        else:
            files, target = changed_md_staged(), "staged .md 改动"
    except (RuntimeError, OSError) as e:
        print(f"[结构检查] 扫描未执行，拒绝放行：{e}", file=sys.stderr)
        return 2

    if args.stats:
        rows = []
        for f in files:
            try:
                with open(f, encoding="utf-8", errors="replace") as fh:
                    rows.append(stats_file(fh.read(), f))
            except OSError as e:
                print(f"[结构检查] 统计跳过 {f}：{e}", file=sys.stderr)
        if args.json:
            print(json.dumps(rows, ensure_ascii=False, indent=2))
        else:
            print(f"{'file':44s} {'lines':>5s} {'prose>150':>9s} {'>300':>5s} {'max':>5s} "
                  f"{'item>200':>8s} {'imax':>5s} {'toc':>4s} {'ep':>3s}")
            tot_p = tot_p3 = tot_i = 0
            for r in rows:
                tot_p += r["prose_gt_max"]
                tot_p3 += r["prose_gt_300"]
                tot_i += r["item_gt_200"]
                print(f"{r['path']:44s} {r['lines']:5d} {r['prose_gt_max']:9d} {r['prose_gt_300']:5d} "
                      f"{r['prose_max']:5d} {r['item_gt_200']:8d} {r['item_max']:5d} "
                      f"{'Y' if r['toc'] else '-':>4s} {r['endpoints']:3d}")
            print(f"\n合计：散文段>150 = {tot_p}，散文段>300 = {tot_p3}，列表项>200 = {tot_i}")
        return 0

    problems = []
    for f in files:
        try:
            with open(f, encoding="utf-8", errors="replace") as fh:
                text = fh.read()
        except OSError as e:
            print(f"[结构检查] 跳过 {f}：{e}", file=sys.stderr)
            continue
        problems.extend(scan_file(text, f, args))

    if problems:
        print(f"结构门禁命中（{target}，{len(problems)} 条）：")
        for p in problems:
            print(f"  {p}")
        print("\n修法见 AGENTS.md「文档排版」第 1/4/9 条；"
              "或对本节加 `<!-- no-skeleton -->`（仅迁移期骨架补齐用）。")
        return 1
    print(f"结构门禁通过（{target}，扫描 {len(files)} 个文件）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
