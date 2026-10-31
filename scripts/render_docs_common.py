#!/usr/bin/env python3
"""从公共模板 + 本仓变量渲染发布手册，防止两仓单边漂移。

模板在 `flow-contract` 的 `docs-common/release-runbook.tpl.md`；两仓经 `conformance` 克隆
随所 pin 的 tag 取得（默认读 `conformance/docs-common/release-runbook.tpl.md`）。本仓变量在
`docs/release-runbook.vars.toml`。渲染结果写 `docs/release-runbook.md`。

单边漂移的防线是「重生一致」：`--check` 把文件与「模板＋变量」重新渲染的结果逐字比对，
不同即红。它比逐字哈希更适应本文件（变量随发版而变），且仍能挡住只手改一边。

用法：
    python scripts/render_docs_common.py             # 渲染并写 --out
    python scripts/render_docs_common.py --check     # 渲染并与 --out 比对，不同即非零
    python scripts/render_docs_common.py --smoke     # 只渲染并校验（占位符全替换、无空节）

退出码：0 正常；1 不一致 / 校验失败；2 无法执行（模板或变量缺失）——拒绝放行。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# 模板里允许出现的占位符（大写）。新增占位符必须同时进这份清单与 vars。
PLACEHOLDER_KEYS = [
    "SERVER", "CLIENT", "SDK", "SDK_MOD", "ENVP", "IDX_PREFIX", "SIBLING",
    "SDIST_ENTRIES", "SDIST_BYTES", "SIB_ENTRIES", "SIB_BYTES",
    "CHACHA_NOTE", "CHACHA_IT",
]

_RESIDUAL = re.compile(r"\{\{[A-Z_]+\}\}")


def load_vars(path: str) -> dict[str, str]:
    """极简 TOML 子集读取：只支持 `key = "值"` / `key = 数字` 与注释。

    刻意不引入 toml 依赖：本仓库与两仓都只用 stdlib，且变量表形态固定。
    """
    data: dict[str, str] = {}
    for raw in Path(path).read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, val = line.split("=", 1)
        key = key.strip()
        val = val.strip()
        if len(val) >= 2 and val[0] in "\"'" and val[-1] == val[0]:
            val = val[1:-1]
        data[key] = val
    return data


def render(template: str, variables: dict[str, str]) -> str:
    out = template
    for key in PLACEHOLDER_KEYS:
        low = key.lower()
        if low not in variables:
            raise KeyError(f"变量缺失：{low}")
        out = out.replace("{{" + key + "}}", str(variables[low]))
    residual = sorted(set(_RESIDUAL.findall(out)))
    if residual:
        raise ValueError("未替换的占位符：" + ", ".join(residual))
    return out


def _empty_sections(rendered: str) -> list[str]:
    """`## ` 标题与下一个 `## ` 之间若无任何非空、非标题行，视为空节。"""
    lines = rendered.split("\n")
    empties: list[str] = []
    idx = [i for i, l in enumerate(lines) if l.startswith("## ")]
    for n, start in enumerate(idx):
        end = idx[n + 1] if n + 1 < len(idx) else len(lines)
        body = [l for l in lines[start + 1:end] if l.strip() and not l.startswith("#")]
        if not body:
            empties.append(lines[start].strip())
    return empties


def main() -> int:
    ap = argparse.ArgumentParser(description="渲染发布手册（模板 + 变量）")
    ap.add_argument("--template", default="conformance/docs-common/release-runbook.tpl.md")
    ap.add_argument("--vars", default="docs/release-runbook.vars.toml")
    ap.add_argument("--out", default="docs/release-runbook.md")
    ap.add_argument("--check", action="store_true", help="与 --out 比对，不同即非零")
    ap.add_argument("--smoke", action="store_true", help="只渲染并校验，不比对文件")
    args = ap.parse_args()

    try:
        template = Path(args.template).read_text(encoding="utf-8")
        variables = load_vars(args.vars)
    except (OSError, KeyError) as e:
        print(f"[发布手册] 无法执行：{e}", file=sys.stderr)
        return 2

    try:
        rendered = render(template, variables)
    except (KeyError, ValueError) as e:
        print(f"[发布手册] 渲染失败：{e}", file=sys.stderr)
        return 1

    empties = _empty_sections(rendered)

    if args.smoke:
        if empties:
            print("[发布手册] 冒烟失败，空节：" + "；".join(empties), file=sys.stderr)
            return 1
        print("[发布手册] 冒烟通过：占位符全替换、无空节")
        return 0

    if args.check:
        try:
            current = Path(args.out).read_text(encoding="utf-8")
        except OSError as e:
            print(f"[发布手册] 无法读取 {args.out}：{e}", file=sys.stderr)
            return 2
        if current != rendered:
            print(
                f"[发布手册] 不一致：{args.out} 与「模板＋变量」渲染结果不同——"
                "请重跑本脚本重生成后一并提交",
                file=sys.stderr,
            )
            return 1
        if empties:
            print("[发布手册] 校验失败，空节：" + "；".join(empties), file=sys.stderr)
            return 1
        print("[发布手册] 一致")
        return 0

    # 写字节而非 write_text：Python 的文本写会把 \n 翻译成 os.linesep（Windows 上 CRLF），
    # 那会让本地渲染出 CRLF、CI（Linux）渲染出 LF，`--check` 在两边结论相反。固定 LF
    # 且与仓库现有文档一致（全部 LF）。
    Path(args.out).write_bytes(rendered.encode("utf-8"))
    print(f"[发布手册] 已渲染 {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
