#!/usr/bin/env python3
"""check_docs_structure.py 的单元自测。

重点钉住三件容易出事的东西：
  1. 散文段与列表项的切分口径（空行 = 边界；空行后的缩进块是**散文**，不是列表续行）；
  2. 六类检查各自**确实会红**（失败开放防护：一条永远不命中的规则等于没有规则）；
  3. 豁免面只豁免该豁免的（CHANGELOG 豁免目录与列表项、但不豁免散文；spec/** 全豁免）。

跑法：python -m unittest discover -s scripts/tests -p 'test_check_docs_structure.py' -v
"""
from __future__ import annotations

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import check_docs_structure as m  # noqa: E402


def cfg(**kw):
    base = dict(max_para=150, max_bullet=200, max_toc_lines=300, h2="jump", allow_levels=False)
    base.update(kw)
    return SimpleNamespace(**base)


def scan(text, path="docs/x-api.md", **kw):
    return m.scan_file(text, path, cfg(**kw))


class SplitLines(unittest.TestCase):
    def test_strips_trailing_blank_so_line_count_matches_editor(self):
        self.assertEqual(len(m.split_lines("a\nb\n\n")), 2)
        self.assertEqual(len(m.split_lines("a\nb")), 2)

    def test_crlf_tolerated(self):
        self.assertEqual(m.split_lines("a\r\nb\r\n"), ["a", "b"])


class BlockBoundaries(unittest.TestCase):
    def test_blank_line_after_bullet_starts_a_prose_block(self):
        # 这正是 SyncClient / 联系人 / 「后续演进」段的形态：bullet、空行、缩进两格的段落。
        text = "- **引子** 一句话\n\n  **续段标题**：" + "很长" * 80 + "\n"
        kinds = [k for k, _, _ in m.blocks(m.split_lines(text))]
        self.assertEqual(kinds, ["item", "prose"])

    def test_indented_continuation_without_blank_merges_into_item(self):
        text = "- **引子** 一句话\n  接着一段懒续行，中间没有空行\n"
        blks = m.blocks(m.split_lines(text))
        self.assertEqual(len(blks), 1)
        self.assertEqual(blks[0][0], "item")
        self.assertEqual(len(blks[0][2]), 2)

    def test_table_quote_heading_are_boundaries(self):
        text = "段落一\n\n| a | b |\n\n> 引用\n\n## 标题\n"
        kinds = [k for k, _, _ in m.blocks(m.split_lines(text))]
        self.assertEqual(kinds, ["prose"])  # 表格/引用/标题都不成块

    def test_fenced_content_is_ignored(self):
        text = "```\n" + "很长" * 200 + "\n```\n"
        self.assertEqual(m.blocks(m.split_lines(text)), [])


class ParagraphAndBulletLimits(unittest.TestCase):
    def test_long_prose_hits(self):
        text = "字" * 151 + "\n"
        probs = scan(text)
        self.assertTrue(any("散文段 151 字" in p for p in probs), probs)

    def test_prose_exactly_at_limit_passes(self):
        self.assertEqual([p for p in scan("字" * 150 + "\n") if "散文段" in p], [])

    def test_long_bullet_hits(self):
        text = "- " + "字" * 220 + "\n"
        probs = scan(text)
        self.assertTrue(any("列表项" in p for p in probs), probs)

    def test_bullet_continuation_counts_toward_bullet_length(self):
        text = "- 短\n  续行" + "字" * 220 + "\n"
        probs = scan(text)
        self.assertTrue(any("列表项" in p for p in probs), probs)

    def test_blank_separated_indented_block_is_prose_not_bullet(self):
        # 若把它错当列表续行，就会按列表项口径放行 250 字；按散文段口径必须红。
        text = "- 短引子\n\n  " + "字" * 250 + "\n"
        probs = scan(text)
        self.assertTrue(any("散文段" in p for p in probs), probs)
        self.assertFalse(any("列表项" in p for p in probs), probs)


class Toc(unittest.TestCase):
    def _doc(self, with_toc):
        head = "## 目录\n\n" + "".join(f"- [节{i}](#节{i})\n" for i in range(5)) + "\n" if with_toc else ""
        body = "".join(f"## 节{i}\n\n结论。\n\n" for i in range(5))
        # 每条都是独立短段（空行分隔），保证文档 >= 300 行且不触发散文预算。
        filler = "\n\n".join("短。" for _ in range(320))
        return head + body + filler + "\n"

    def test_long_doc_without_toc_hits(self):
        probs = scan(self._doc(False), path="docs/big.md")
        self.assertTrue(any("缺「## 目录」" in p for p in probs), probs)

    def test_long_doc_with_toc_passes(self):
        probs = scan(self._doc(True), path="docs/big.md")
        self.assertFalse(any("目录" in p for p in probs), probs)

    def test_short_doc_needs_no_toc(self):
        probs = scan("## 节\n\n结论。\n", path="docs/small.md")
        self.assertFalse(any("目录" in p for p in probs), probs)

    def test_changelog_exempt_from_toc(self):
        probs = scan(self._doc(False), path="CHANGELOG.md")
        self.assertFalse(any("目录" in p for p in probs), probs)


class H2Conclusion(unittest.TestCase):
    def test_jump_mode_flags_section_starting_with_subheading(self):
        probs = scan("## 服务层\n\n### 子节\n\n结论。\n", path="docs/arch.md")
        self.assertTrue(any("跳级缺结论" in p for p in probs), probs)

    def test_jump_mode_allows_section_starting_with_list(self):
        probs = scan("## 接口列表\n\n- 一条\n- 两条\n", path="docs/arch.md")
        self.assertFalse(any("跳级缺结论" in p for p in probs), probs)

    def test_toc_section_exempt(self):
        probs = scan("## 目录\n\n- [a](#a)\n", path="docs/arch.md")
        self.assertFalse(any("目录" in p for p in probs), probs)

    def test_strict_mode_flags_non_conclusion_first_line(self):
        probs = scan("## 节\n\n这不是结论\n", path="docs/arch.md", h2="strict")
        self.assertTrue(any("不是结论句" in p for p in probs), probs)


class EndpointSkeleton(unittest.TestCase):
    def test_endpoint_missing_request_params_fence_hits(self):
        text = "### GET `/api/v1/x` — 说明\n\n一句话。\n\n**响应**\n\n```json\n{}\n```\n"
        probs = scan(text)
        self.assertTrue(any("缺 **请求**" in p for p in probs), probs)
        self.assertTrue(any("**参数**" in p for p in probs), probs)

    def test_complete_endpoint_passes(self):
        text = (
            "### GET `/api/v1/x` — 说明\n\n一句话。\n\n**请求**\n\n```http\nGET /api/v1/x\n```\n\n"
            "**参数**：无\n\n**响应**\n\n```json\n{}\n```\n"
        )
        self.assertEqual([p for p in scan(text) if "端点「" in p], [])

    def test_no_skeleton_marker_suppresses(self):
        text = "### GET `/api/v1/x` — 说明\n\n<!-- no-skeleton -->\n\nx\n"
        self.assertEqual([p for p in scan(text) if "端点「" in p], [])

    def test_non_api_doc_not_checked_for_skeleton(self):
        text = "### GET `/api/v1/x` — 说明\n\n一句话。\n"
        self.assertEqual([p for p in scan(text, path="docs/quickstart.md") if "端点「" in p], [])


class EndpointLevelAndForm(unittest.TestCase):
    def test_h2_endpoint_hits(self):
        text = "## GET `/api/v1/x` — 说明\n\n**请求**\n\n```http\nGET /api/v1/x\n```\n\n**参数**：无\n"
        probs = scan(text)
        self.assertTrue(any("在 h2" in p for p in probs), probs)

    def test_backticked_method_form_is_not_canonical(self):
        # 整包反引号形态：`GET /chatlab/sessions` —— 会被识别为端点，
        # 但同时被判为「不是规范形态」，正是端点标题形态统一要改的对象。
        text = "### `GET /chatlab/sessions` — 发现面\n\n**请求**\n\n```http\nGET /chatlab/sessions\n```\n\n**参数**：无\n"
        probs = scan(text)
        self.assertTrue(any("不是 `VERB `/path`` 形态" in p for p in probs), probs)

    def test_http_fence_endpoint_detected_even_without_verb_title(self):
        text = "## 健康检查\n\n**请求**\n\n```http\nGET /health\n```\n\n**参数**：无\n"
        probs = scan(text)
        # qqflow 形态：能被识别为端点，并因 h2 + 无 VERB 标题被判两条。
        self.assertTrue(any("在 h2" in p for p in probs), probs)
        self.assertTrue(any("不是 `VERB `/path`` 形态" in p for p in probs), probs)

    def test_allow_levels_skips(self):
        text = "## 健康检查\n\n**请求**\n\n```http\nGET /health\n```\n\n**参数**：无\n"
        probs = scan(text, allow_levels=True)
        self.assertFalse(any("h2" in p or "VERB" in p for p in probs), probs)


class Closure(unittest.TestCase):
    def test_unclosed_fence_is_flagged(self):
        # 少一个闭围栏 → 其后内容不被检查。必须显式判红，否则「失败开放」。
        text = "## 节\n\n```text\n未闭合\n\n## 后面还有一节\n\n" + "字" * 400 + "\n"
        probs = scan(text, path="docs/x.md")
        self.assertTrue(any("代码围栏未闭合" in p for p in probs), probs)

    def test_unclosed_comment_is_flagged(self):
        text = "正文\n\n<!-- 忘了闭合\n\n## 后面\n\n" + "字" * 400 + "\n"
        probs = scan(text, path="docs/x.md")
        self.assertTrue(any("HTML 注释未闭合" in p for p in probs), probs)

    def test_balanced_fences_pass(self):
        text = "## 节\n\n```text\n代码\n```\n\n结论。\n"
        self.assertFalse(any("未闭合" in p for p in scan(text, path="docs/x.md")))

    def test_fence_inside_comment_does_not_derail_state(self):
        # 两套状态机顺序不一致时会在这种输入上分歧：注释里含奇数个围栏行。
        # 统一到 classify() 后，注释内的 ``` 属注释内容，不该翻转围栏状态，
        # 其后正文必须照常被检查（这里的 400 字超长段必须被散文段规则判红）。
        text = ("正文。\n\n<!-- 旧示例：\n```text\n```\n-->\n\n"
                + "字" * 400 + "\n")
        probs = scan(text, path="docs/x.md")
        self.assertTrue(any("散文段" in p for p in probs), probs)
        self.assertFalse(any("未闭合" in p for p in probs), probs)

    def test_commented_out_toc_heading_does_not_satisfy_toc_rule(self):
        # 被注释掉的 `## 目录` 不是真目录；320 行文档仍须判「缺目录」。
        commented = "<!--\n## 目录\n- [a](#a)\n- [b](#b)\n- [c](#c)\n- [d](#d)\n- [e](#e)\n-->\n"
        body = "".join(f"## 节{i}\n\n结论。\n\n" for i in range(5))
        filler = "\n\n".join("短。" for _ in range(320))
        probs = scan(commented + body + filler + "\n", path="docs/big.md")
        self.assertTrue(any("缺「## 目录」" in p for p in probs), probs)


class Exemptions(unittest.TestCase):
    def test_spec_fully_exempt(self):
        long = "字" * 400
        text = f"## 节\n\n{long}\n\n- {long}\n"
        self.assertEqual(scan(text, path="spec/chatlab-format.en.md"), [])

    def test_changelog_exempt_from_bullet_but_not_prose(self):
        probs = scan("- " + "字" * 400 + "\n\n" + "字" * 400 + "\n", path="CHANGELOG.md")
        self.assertFalse(any("列表项" in p for p in probs), probs)
        self.assertTrue(any("散文段" in p for p in probs), probs)

class ScanSelectionFailOpen(unittest.TestCase):
    """--tree/--ref 扫到 0 个文件必须拒绝放行（exit 2），不能静默通过；
    从子目录运行必须仍扫到仓库根下的全部 .md；staged 无改动不算错。"""

    def setUp(self):
        self._tmp = tempfile.mkdtemp(prefix="cds_failopen_")
        self._cwd = os.getcwd()

    def tearDown(self):
        os.chdir(self._cwd)
        shutil.rmtree(self._tmp, ignore_errors=True)

    @staticmethod
    def _git(*args):
        # CI runner 没有全局 git identity，且会把 GIT_AUTHOR_NAME 等置成空串——
        # 空值环境变量的优先级高于 -c 配置，会压掉注入的身份，必须先剥掉。
        env = {k: v for k, v in os.environ.items() if v or not k.startswith("GIT_")}
        env.setdefault("GIT_AUTHOR_NAME", "docs-test")
        env.setdefault("GIT_AUTHOR_EMAIL", "docs-test@example.invalid")
        env.setdefault("GIT_COMMITTER_NAME", "docs-test")
        env.setdefault("GIT_COMMITTER_EMAIL", "docs-test@example.invalid")
        return subprocess.run(["git", *args], check=True, env=env)

    def _init_repo_with_md(self):
        subprocess.run(["git", "init", "-q", self._tmp], check=True)
        with open(os.path.join(self._tmp, "a.md"), "w", encoding="utf-8") as fh:
            fh.write("# 标题\n\n短段。\n")
        subprocess.run(["git", "-C", self._tmp, "add", "-A"], check=True)

    def test_tree_zero_files_refuses(self):
        subprocess.run(["git", "init", "-q", self._tmp], check=True)  # 空仓库，无 .md
        os.chdir(self._tmp)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(m.main(["--tree"]), 2)

    def test_ref_zero_files_refuses(self):
        subprocess.run(["git", "init", "-q", self._tmp], check=True)
        self._git("-C", self._tmp, "commit", "--allow-empty", "-qm", "x")
        os.chdir(self._tmp)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(m.main(["--ref", "HEAD"]), 2)

    def test_tree_from_subdir_still_scans_root(self):
        self._init_repo_with_md()
        os.makedirs(os.path.join(self._tmp, "docs"))
        os.chdir(os.path.join(self._tmp, "docs"))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(m.main(["--tree"]), 0)
        self.assertIn("扫描 1 个文件", out.getvalue())

    def test_staged_no_change_is_ok(self):
        self._init_repo_with_md()
        self._git("-C", self._tmp, "commit", "-qm", "init")
        os.chdir(self._tmp)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(m.main([]), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
