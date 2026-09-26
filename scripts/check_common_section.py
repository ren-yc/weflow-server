#!/usr/bin/env python3
"""校验本仓 AGENTS.md 的公共段，与契约仓库**所 pin tag** 的内容逐字一致。

为什么比对「所 pin tag 的内容」而不是别的：
  * 比对本仓工作副本没有意义（自己跟自己比）；
  * 让 tag 可变也同样没有意义——只要能把 pin 换成一个内容恰好匹配的 tag，
    守卫就形同虚设。所以**升级 pin 是一个需要评审的动作**，而本脚本只负责
    回答「当前 pin 与当前内容是否一致」。

用法：
    python scripts/check_common_section.py --conformance conformance

退出码：0 一致；1 不一致；2 环境问题（读不到文件 / 缺标记）——**按拒绝放行处理**。
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

BEGIN = "<!-- common:begin -->"
END = "<!-- common:end -->"


def normalize(text: str) -> str:
    """统一行尾并去掉末尾空行：Windows 检出的 CRLF 不应造成假失败。"""
    return text.replace("\r\n", "\n").rstrip("\n")


def extract_common_section(agents_md: str) -> str:
    m = re.search(re.escape(BEGIN) + r"\n(.*?)\n" + re.escape(END), agents_md, re.S)
    if not m:
        raise ValueError(
            "AGENTS.md 里找不到 " + BEGIN + " / " + END + " 标记对；"
            "缺标记时无法确定比对范围，按环境问题处理（拒绝放行）"
        )
    return m.group(1)


def sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser(description="公共段一致性校验")
    ap.add_argument("--conformance", default="conformance", help="契约仓库的检出目录")
    ap.add_argument("--agents", default="AGENTS.md", help="本仓 AGENTS.md 路径")
    ap.add_argument("--pin", default="conformance.pin", help="记录所 pin tag 的文件")
    args = ap.parse_args()

    try:
        head = Path(args.agents).read_text(encoding="utf-8")
        want = Path(args.conformance, "AGENTS-common.md").read_text(encoding="utf-8")
        version = Path(args.conformance, "VERSION").read_text(encoding="utf-8")
        pin = Path(args.pin).read_text(encoding="utf-8").strip()
    except OSError as e:
        print(f"[公共段] 读取失败，按拒绝放行处理：{e}", file=sys.stderr)
        return 2

    try:
        got = extract_common_section(head)
    except ValueError as e:
        print(f"[公共段] {e}", file=sys.stderr)
        return 2

    # 版本自洽：pin 去掉前缀 v 必须等于契约仓库的 VERSION。
    # 不做这一步的话，pin 指向的 tag 与 checkout 到的内容可能不是一回事。
    expected_version = pin[1:] if pin.startswith("v") else pin
    if version.strip() != expected_version:
        print(
            f"[公共段] pin 与契约版本不一致：{args.pin}={pin} "
            f"但 {args.conformance}/VERSION={version.strip()}",
            file=sys.stderr,
        )
        return 1

    if normalize(got) != normalize(want):
        print("[公共段] 与契约仓库不一致：", file=sys.stderr)
        print(f"  本仓           {sha256(normalize(got))}", file=sys.stderr)
        print(f"  {pin}  {sha256(normalize(want))}", file=sys.stderr)
        print(
            "  改公共段请改 flow-contract/AGENTS-common.md 并打新 tag，"
            "再在两个仓库同批升 conformance.pin。",
            file=sys.stderr,
        )
        return 1

    print(f"公共段与 {pin} 一致（sha256 {sha256(normalize(got))[:16]}…）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
