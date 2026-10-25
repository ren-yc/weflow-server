#!/usr/bin/env python3
"""折点扫描 —— 检测中文散文里会产生「词中空格」的段内换行。

背景：GitHub 把段内软换行渲染成一个空格。若一行以**汉字**结尾、下一行又以
**汉字或全角开括号**开头，渲染后就会出现词中空格（如「不做密钥 提取」「仅内存
保存 （不落盘）」）。本扫描器只抓这一类折点，把它挡在提交 / CI 之前。

判定口径（与方案一致）：
  * 折点 = 行尾是汉字，且下一物理行首是汉字或全角开括号（（「『【〔〖 等）；
  * 只统计「续行」：正文段落 p→p、列表项内部 l→p、引用块内部 q→q；
    两个独立列表项之间（l→l）不插空格，不算折点；
  * 行尾两空格是显式 <br>，不算折点；
  * 行尾为全角标点（。，；：！？等）时不算折点——句末标点后的正常换行不产生词中空格。

豁免面（不扫描）：
  1. 代码围栏（``` / ~~~）与缩进式代码块（连续 4 空格/Tab 缩进，直到空行/去缩进）；
  2. 表格行（以 | 开头）及其续行；
  3. HTML 注释（<!-- 到 -->）；
  4. 标题行（# 开头）；
  5. `<!-- common:begin -->` 至 `<!-- common:end -->` 公共段整段
     （由 flow-contract 的 sha256 哈希纪律维护，本仓不得私改）；
  6. clients/*/README.md —— 统一英文后不含中文，天然无折点。

用法：
  python scripts/check_docs.py                # 扫描 staged 的 .md 改动（pre-commit）
  python scripts/check_docs.py --ref origin/master   # 扫描相对基线的差异（CI）
  python scripts/check_docs.py --tree         # 全量扫描跟踪的 .md

退出码：0 = 无折点；1 = 有折点；2 = 扫描未执行（git 取差异失败）——**拒绝放行**。
空 diff 不等于干净：取不到差异时必须返回 2，否则门禁失守。
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys

HAN = re.compile(r"[\u4e00-\u9fff]")
# 全角开括号：（【「『《〈〔〖 及中文引号
OPEN_FW = re.compile(
    "[\uff08\uff3b\uff5b\u300c\u300e\u3010\u300a\u3008\u3009\u3014\u3016\u201c\u2018]"
)
CB = "<!-- common:begin -->"
CE = "<!-- common:end -->"

_TABLE = re.compile(r"^\s*\|")
_HR = re.compile(r"^\s{0,3}(-{3,}|\*{3,}|_{3,})\s*$")
_HEADING = re.compile(r"^\s{0,3}#{1,6}\s")
_FENCE = re.compile(r"^\s*(```|~~~)")
_LIST = re.compile(r"^(\s*)([-*+]|\d{1,3}[.)])\s+")
_QUOTE = re.compile(r"^\s*>")
_INDENT_CODE = re.compile(r"^(    |\t)")


def classify(lines):
    kinds, in_fence, in_comment, prev_blank, in_indent = [], False, False, True, False
    for raw in lines:
        line = raw.rstrip("\r")
        if in_comment:
            kinds.append("c")
            if "-->" in line:
                in_comment = False
            prev_blank = False
            continue
        if _FENCE.match(line):
            kinds.append("code")
            in_fence = not in_fence
            prev_blank = False
            in_indent = False
            continue
        if in_fence:
            kinds.append("code")
            prev_blank = False
            continue
        if line.lstrip().startswith("<!--"):
            kinds.append("c")
            if "-->" not in line:
                in_comment = True
            prev_blank = False
            continue
        # 缩进式代码块：一旦进入，连续 4 空格/Tab 缩进（非列表续行）都算代码，
        # 直到空行或去缩进——否则多行缩进块内部会产生误报。
        if in_indent:
            if not line.strip() or not _INDENT_CODE.match(line):
                in_indent = False
            else:
                kinds.append("code")
                prev_blank = False
                continue
        if not line.strip():
            kinds.append("b")
            prev_blank = True
            continue
        if prev_blank and _INDENT_CODE.match(line) and not _LIST.match(line):
            kinds.append("code")
            prev_blank = False
            in_indent = True
            continue
        if _HEADING.match(line):
            kinds.append("h")
        elif _HR.match(line):
            kinds.append("hr")
        elif _TABLE.match(line):
            kinds.append("t")
        elif _QUOTE.match(line):
            kinds.append("q")
        elif _LIST.match(line):
            kinds.append("l")
        else:
            kinds.append("p")
        prev_blank = False
    return kinds


def common_lines(lines):
    out, inside = set(), False
    for i, raw in enumerate(lines):
        if raw.strip() == CB:
            inside = True
        if inside:
            out.add(i)
        if raw.strip() == CE:
            inside = False
    return out


def is_break(lines, kinds, i):
    if i + 1 >= len(lines):
        return False
    ka, kb = kinds[i], kinds[i + 1]
    if ka == "q" and kb == "q":
        raw_a = re.sub(r"^\s*>\s?", "", lines[i])
        a = raw_a.rstrip()
        b = re.sub(r"^\s*>\s?", "", lines[i + 1]).lstrip()
    elif ka in ("p", "l") and kb == "p":
        raw_a = lines[i]
        a, b = raw_a.rstrip(), lines[i + 1].lstrip()
    else:
        return False
    # 行尾两空格是显式 <br>：必须在 rstrip 之前判断（否则永远为假）
    if raw_a.endswith("  "):
        return False
    if not a or not b or a.endswith("\\"):
        return False
    return bool(HAN.match(a[-1]) and (HAN.match(b[0]) or OPEN_FW.match(b[0])))


def scan_text(text):
    lines = text.split("\n")
    kinds = classify(lines)
    common = common_lines(lines)
    hits = []
    for i in range(len(lines) - 1):
        if i in common or (i + 1) in common:
            continue
        if is_break(lines, kinds, i):
            hits.append((i + 1, lines[i].rstrip()[-20:], lines[i + 1].lstrip()[:20]))
    return hits


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
    ap = argparse.ArgumentParser(description="折点扫描（中文段内换行导致的词中空格）")
    ap.add_argument("--ref", default=None, help="扫描相对该基线的差异（CI 用）")
    ap.add_argument("--tree", action="store_true", help="全量扫描跟踪的 .md")
    args = ap.parse_args(argv)

    try:
        if args.tree:
            files, target = tracked_md(), "全量跟踪 .md"
        elif args.ref:
            files, target = changed_md_ref(args.ref), f"相对 {args.ref} 的差异"
        else:
            files, target = changed_md_staged(), "staged .md 改动"
    except (RuntimeError, OSError) as e:
        print(f"[折点检查] 扫描未执行，拒绝放行：{e}", file=sys.stderr)
        return 2

    files = [f for f in files if not re.match(r"^clients/[^/]+/README\.md$", f)]

    total = []
    for f in files:
        try:
            with open(f, encoding="utf-8", errors="replace") as fh:
                text = fh.read()
        except OSError:
            continue
        for ln, tail, head in scan_text(text):
            total.append((f, ln, tail, head))

    if total:
        print(f"检测到折点（{target}）：行尾汉字 + 下一行以汉字/全角开括号开头，渲染会产生词中空格。")
        for f, ln, tail, head in total:
            print(f"  {f}:{ln}  …{tail!r} + {head!r}…")
        print("\n修法：把该段合并为一个物理行（懒换行），或在 ASCII 空格/行内代码/链接边界折行。")
        return 1
    print(f"未发现折点（{target}，扫描 {len(files)} 个文件）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
