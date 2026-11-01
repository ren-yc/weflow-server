#!/usr/bin/env python3
"""check_common_section.py 的单元自测：钉住「默认行为不变」与「参数化可用」。

用临时目录造出契约检出副本与本仓文件，不依赖真实的 conformance 工作副本
（那是 gitignore 的本地检出，版本可能与 pin 不一致，不能作为测试基准）。

跑法：python -m unittest discover -s scripts/tests -p 'test_check_common_section.py' -v
"""
from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "check_common_section.py"
BEGIN = "<!-- common:begin -->"
END = "<!-- common:end -->"


class CommonSection(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.conf = self.root / "conformance"
        self.conf.mkdir()
        (self.conf / "VERSION").write_text("0.7.0\n", encoding="utf-8")
        (self.root / "conformance.pin").write_text("v0.7.0\n", encoding="utf-8")

    def tearDown(self):
        self.tmp.cleanup()

    def run_script(self, *extra):
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--conformance", str(self.conf), *extra],
            cwd=self.root, capture_output=True, text=True,
        )
        # 某些 runner 环境（如 CI 的 pwsh 包装）下子进程流可能为 None，不能假设恒为 str。
        return r.returncode, (r.stdout or "") + (r.stderr or "")

    def test_default_agents_md_happy_path(self):
        (self.conf / "AGENTS-common.md").write_text("公共正文\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(
            f"前言\n{BEGIN}\n公共正文\n{END}\n后记\n", encoding="utf-8"
        )
        rc, out = self.run_script()
        self.assertEqual(rc, 0, out)

    def test_mismatch_returns_1(self):
        (self.conf / "AGENTS-common.md").write_text("公共正文\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(f"{BEGIN}\n别的内容\n{END}\n", encoding="utf-8")
        rc, out = self.run_script()
        self.assertEqual(rc, 1, out)

    def test_missing_marker_returns_2(self):
        (self.conf / "AGENTS-common.md").write_text("公共正文\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text("没有标记\n", encoding="utf-8")
        rc, out = self.run_script()
        self.assertEqual(rc, 2, out)

    def test_pin_version_mismatch_returns_1(self):
        (self.conf / "AGENTS-common.md").write_text("公共正文\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(f"{BEGIN}\n公共正文\n{END}\n", encoding="utf-8")
        (self.root / "conformance.pin").write_text("v0.6.6\n", encoding="utf-8")
        rc, out = self.run_script()
        self.assertEqual(rc, 1, out)
        self.assertIn("pin 与契约版本不一致", out)

    def test_parameterized_section_and_source(self):
        d = self.conf / "templates"
        d.mkdir()
        (d / "shared-section.md").write_text("共享段正文\n", encoding="utf-8")
        docs = self.root / "docs"
        docs.mkdir()
        (docs / "shared-section.md").write_text(
            f"本仓序言\n{BEGIN}\n共享段正文\n{END}\n本仓附录\n", encoding="utf-8"
        )
        rc, out = self.run_script(
            "--section", "docs/shared-section.md", "--source", "templates/shared-section.md"
        )
        self.assertEqual(rc, 0, out)

    def test_custom_markers(self):
        (self.conf / "tpl.md").write_text("模板正文\n", encoding="utf-8")
        (self.root / "X.md").write_text(
            "<!-- tpl:begin -->\n模板正文\n<!-- tpl:end -->\n", encoding="utf-8"
        )
        rc, out = self.run_script(
            "--section", "X.md", "--source", "tpl.md",
            "--marker-begin", "<!-- tpl:begin -->", "--marker-end", "<!-- tpl:end -->",
        )
        self.assertEqual(rc, 0, out)

    def test_agents_alias_still_works(self):
        (self.conf / "AGENTS-common.md").write_text("公共正文\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(f"{BEGIN}\n公共正文\n{END}\n", encoding="utf-8")
        rc, out = self.run_script("--agents", "AGENTS.md")
        self.assertEqual(rc, 0, out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
