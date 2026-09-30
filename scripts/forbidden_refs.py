"""编号引用扫描 —— 禁止在注释 / 文档 / 提交信息中夹带仓库外不可访问的编号指针。

背景：本地计划产物与审查报告不在仓库内，读者无法据编号还原上下文。注释应当
解释「为什么」与失败模式，而不是指向一份外部文档的条目号；提交信息应当描述
行为变化，而不是流水号。

命中规则（详见 _RULES；示例一律写成占位形态，避免规则文本自我命中）：
  1. 组合编号：中文方括号包裹的「序号 · 条目码」形态；
  2. 归因信号词 + 任意编号：复核 / 审查报告 / 审计 / 排期 / 缺陷 后面跟编号；
  3. 条目码：单个或两个大写字母前缀 + 数字，可带次级编号；
  4. 单字母码 + 冒号：字母数字码后紧跟中文或英文冒号（仅注释 / 文档语境）；
  5. 计划产物路径：本地计划文件名、临时产物目录名、会话产物目录名；
  6. 流水号批次：流水号 + 量词「批」；
  7. 章节引用：符号 + 数字编号（可带子节），如指向外部文档的 x.y 节形态。
     **不扫 .md** —— 文档内部锚点与外部章节引用同形且合法（本仓文档大量使用）；
     提交信息里引用本仓文档的合法形态带「文档」限定词，由负向后视豁免；
  8. 计划指涉：与「计划」构成**指引结构**的词组（词组本体见 _R_PLANREF 字符串）。
     判据：外部计划在措辞里充当被指引的对象；本仓自己的计划性材料一律以
     「计划文件 / 计划产物」复合词出现（归规则 5 与豁免表）或带「本 / 该 / 工作」
     等限定词——这些形态不落入词组。

豁免（不构成编号引用）：编码名、RFC 编号、sha 摘要、控制字符名、静态检查码
指令及其码表、少量固定技术缩写（见 _EXEMPT）、依赖版本号、**十六进制字节序列**
（形如压缩格式魔数那样以空格分隔的双位十六进制串）、可跟踪的 issue 编号，以及
行内显式豁免标记 allow-plan-ref。豁免片段是**剥离后再扫**，同行其它编号照常判定。
提交信息只扫会真正进入提交的部分（注释行与 scissors 之后的 diff 不算）。

用法：
  python scripts/forbidden_refs.py                        # 扫描 staged 新增内容
  python scripts/forbidden_refs.py --ref origin/master     # CI：相对基线的差异
  python scripts/forbidden_refs.py --tree                  # 全量跟踪文件
  python scripts/forbidden_refs.py --message-file <路径>   # 提交信息

退出码：0 = 无命中；1 = 命中；2 = 扫描未执行（git 取差异失败 / 提交信息读不到）。
**空 diff 不等于干净** —— 取不到差异时必须拒绝放行，否则门禁失守。
"""

from __future__ import annotations

import argparse
import io
import re
import subprocess
import sys
import tokenize
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

# CI 的 Windows runner 默认 stdout 可能不是 UTF-8，中文输出会 UnicodeEncodeError
if hasattr(sys.stdout, "reconfigure"):
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]
    except Exception:  # noqa: BLE001, S110
        pass

_CJK = re.compile(r"[\u3400-\u9fff\uf900-\ufaff\u3000-\u303f\uff00-\uffef]")

# 本计划的当前文件名（唯一权威处）。计划改名时**只改这一行**——
# 规则写成「模式 + 单点常量」，因此不依赖计划的最终文件名与位置。
_PLAN_FILE_STEM = "server-refactor-plan"

# 规则 1：组合编号（中文方括号 + 序号 + 分隔符 + 条目码）
_R_COMBO = r"【\s*\d+\s*[·・]\s*[A-Z]?\d+\s*】"
# 规则 2：归因信号词 + 编号。数字后接中文量词（其次 / 第 3 条）不算编号引用。
_R_ATTRIB = (
    r"(?:复核|审查报告|审计|排期|缺陷)\s*[【#]?\s*"
    r"(?:[A-Z]{1,2}\d+(?:[-\u2013\u2014·]\d+)?|[A-Z]{1,2}-?\d{1,3}|#?\d{1,4})"
    # (?!\d) 防回溯：四位年份不能被截成前三位而绕过后面的量词排除
    r"(?!\d)(?!\s*[次条个轮遍张年月日])"
)
# 规则 3：条目码（1-2 个大写字母 + 数字，可带次级编号）。仅大写：小写版本号
# 不命中；三字母以上的缩写因前缀长度限制不命中。
# 数字位限 1-2：四位诊断码（编译器错误号一类）不命中。
_R_CODE = r"(?<![A-Za-z0-9_])[A-Z]{1,2}[-\u2013\u2014·]?\d{1,2}(?![A-Za-z0-9_])"
# 规则 4：单字母码 + 冒号（仅用于注释 / 文档语境，见 _looks_like_prose）
_R_COLON = r"(?<![A-Za-z0-9_])[A-Z]\d{1,2}\s*[：:]"
# 规则 5：计划产物路径（模式 + 单点常量；不硬编码具体路径）
_R_PATH = (
    r"(?:IMPLEMENTATION-)?PLAN-[A-Z0-9]|TODO-[A-Z0-9]|_tmp_|_review/|plan-sess_|"
    + re.escape(_PLAN_FILE_STEM)
)
# 规则 6：流水号批次
_R_BATCH = r"第\s*[0-9一二三四五六七八九十]+\s*批"
# 规则 7：章节引用（符号 + 数字，可带子节）。两道豁免写进模式本身：
#   「文档 X」形态的本仓引用（负向后视，两个宽度各挡一种写法）。
#   .md 整类排除在 _collect_hits 里做——文档内锚点与外部章节引用无法用文本区分，
#   而本仓文档（接口文档、CHANGELOG）满篇都是合法锚点；提交信息与代码注释里的
#   章节指针没有这种合法形态，那才是要拦的目标。示例写占位形态避免自我命中。
#   裸的章节符号在提交信息里一律**判疑**：本仓文档的缩写引用（不带限定词的
#   单节号）也会被拦——两仓与外部材料的章节号同形、文本上无法区分，
#   宁可拦错让作者补「文档」限定词或行内豁免标记，也不放走真正的外部指针。
#   实测：区间外 3 条基线前提交因裸节号缩写被标，均属此类有意代价。
_R_SECTION = r"(?<!文档 )(?<!文档)\s*§\s*\d+(?:\.\d+)*"
# 规则 8：计划指涉（指引结构词组）。词组本体只出现在字符串里——本文件按注释
# 扫描、字符串不扫，因此词组不会自我命中；上面 docstring 给出完整判据。
# 两个贪心词必须带**动词搭配**后视，否则会误伤普通并列结构——本文件自己就被
# 自己抓过一次：主题行把「章节引用」与「计划指涉词组」并列写下，连接词恰好拼出
# 了指引形态。同形不同构的判据：指涉 = 连接词 + 计划 + 动词搭配；并列 = 连接词
# + 计划 + 名词头。占位写法：词组本体只放下面的字符串里（字符串不扫）。
# 「计划 + 方位词」保留裸形态：它在本仓语境里几乎总是指引（实测全树仅命中共知的
# 历史违规），残余风险在此声明。
_R_PLANREF = (
    r"见计划|计划里|"
    r"计划中(?=确认|记载|写明|说明|指出|列出|提到)|"
    r"与计划(?=经|预判|一致|实测|核对|相符|评估|判定|量化|确认)|"
    r"计划豁免|计划预判"
)

_RULES: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("组合编号", re.compile(_R_COMBO)),
    ("归因词+编号", re.compile(_R_ATTRIB)),
    ("条目码", re.compile(_R_CODE)),
    ("字母码+冒号", re.compile(_R_COLON)),
    ("计划产物路径", re.compile(_R_PATH)),
    ("流水号批次", re.compile(_R_BATCH)),
    ("章节引用", re.compile(_R_SECTION)),
    ("计划指涉", re.compile(_R_PLANREF)),
)

# 规则 4 只在「注释 / 文档」语境下生效：代码里的分支标签不是编号引用。
_COLON_RULE_ONLY_PROSE = True

# 豁免片段：命中后从文本中**剥离**再套规则，而不是整行放行——整行放行会让
# 「静态检查码指令 —— 见复核 <条目码>」这类同行夹带的真实编号也一起溜过。
_EXEMPT = tuple(
    re.compile(p, re.IGNORECASE)
    for p in (
        # ── 通用 ──
        r"UTF-\d+",
        r"RFC\s*\d+",
        r"\bsha\d+\b",
        r"\bC0\b",
        r"\bC1\b",
        r"\bMD5\b",
        r"\bMP[34]\b",
        # 十六进制字节序列：以空格分隔的双位十六进制串（压缩格式魔数一类）。
        # 这类串里的片段与「条目码」同形，但显然是字节而非编号——
        # 实测真库里就有一例（某个魔数中间的两位被误判）。
        r"\b[0-9A-F]{2}(?:\s+[0-9A-F]{2})+\b",
        # ── Rust 生态 ──
        r"clippy::[a-z_]+",
        r"\bE\d{4}\b",  # 编译器诊断码
        # ── 本项目的固定技术缩写（形态与条目码相同但不是编号）──
        # 字节序宽度：LE32 / BE16 一类。与条目码同形，判据是**总与位宽同现**
        # 且处于字节布局表达式里（如 LE32(pgno)），而非引用外部条目。
        r"\b(?:LE|BE)\d{1,2}\b",
        r"\bV[1-4]\b",  # 媒体容器版本
        r"\bAES\b",
        r"\bHMAC\b",
        r"\bSHA\d*\b",
        r"\bPBKDF2\b",
        r"\bSQLCipher\b",
        r"\bWCDB\b",
        r"\bFTS\d\b",
    )
)

# 行内豁免标记：确实需要保留编号时（如引用仓库内文件里的真实标识符）
_ALLOW_MARKER = "allow-plan-ref"

_TEXT_SUFFIXES = (
    ".py",
    ".rs",
    ".md",
    ".yml",
    ".yaml",
    ".toml",
    ".ps1",
    ".sh",
)


@dataclass(frozen=True)
class Hit:
    """一条命中：路径 + 行号 + 命中的规则名（可多条，用 + 连接）+ 该行原文。"""

    path: str
    line: int
    rule: str
    text: str


def _looks_like_prose(line: str) -> bool:
    """判断一行是否处于「注释 / 文档」语境：以注释符号开头，或含中日韩文字。

    用于增量（diff）扫描：diff 里拿不到完整语法上下文，代码行不应因规则 4
    被误判；而注释与文档行必然以注释符号开头或含中文。
    """
    stripped = line.lstrip()
    if stripped.startswith(("#", "//", "*", "/*", "<!--", ";;")):
        return True
    return bool(_CJK.search(line))


def _collect_hits(path: str, line: int, text: str, *, prose: bool = True) -> list[Hit]:
    """对单个「注释 / 文档片段」套用全部规则。"""
    if _ALLOW_MARKER in text:
        return []
    stripped = text
    for rx in _EXEMPT:
        stripped = rx.sub(" ", stripped)
    matched: list[str] = []
    if path.lower().endswith(".md"):
        # .md 排除整条「章节引用」规则：文档内锚点与外部章节引用同形（见规则注释）。
        skip_section = True
    else:
        skip_section = False
    for rule, rx in _RULES:
        if rule == "字母码+冒号" and (_COLON_RULE_ONLY_PROSE and not prose):
            continue
        if rule == "章节引用" and skip_section:
            continue
        if rx.search(stripped):
            matched.append(rule)
    if not matched:
        return []
    # 同一行只报一条（规则名合并），避免报告重复膨胀
    return [Hit(path=path, line=line, rule="+".join(matched), text=text.strip())]


def _python_segments(source: str) -> list[tuple[int, str]]:
    """提取 Python 源码中的注释片段（不含字符串字面量）。"""
    segments: list[tuple[int, str]] = []
    try:
        for token in tokenize.generate_tokens(io.StringIO(source).readline):
            if token.type == tokenize.COMMENT:
                segments.append((token.start[0], token.string))
    except (tokenize.TokenError, IndentationError):
        # 语法不完整（如 diff 片段）：退化为逐行注释识别
        for lineno, line in enumerate(source.splitlines(), 1):
            if line.lstrip().startswith("#"):
                segments.append((lineno, line))
    return segments


# Rust / 类 C 语言通用的行注释起点
_LINE_COMMENT_START = ("//", "*", "/*")


def _line_comment_segments(source: str, markers: tuple[str, ...]) -> list[tuple[int, str]]:
    """按行注释符号提取片段（shell / yml / toml / ps1）。"""
    return [
        (lineno, line)
        for lineno, line in enumerate(source.splitlines(), 1)
        if line.lstrip().startswith(markers)
    ]


def _rust_trailing_comment_segments(source: str) -> list[tuple[int, str]]:
    """提取 Rust 代码行末尾的行注释（整行注释由 _line_comment_segments 负责）。

    注释起点之前的代码里两种引号都成对时才视为注释起点，排除字符串内的
    双斜杠（如 URL）；冒号紧跟的双斜杠视为 URL 不切。
    **已知局限**：不处理原始字符串（r#"..."#）——若将来在原始字符串里写双斜杠，
    需要在此处补规则。
    """
    segments: list[tuple[int, str]] = []
    for lineno, line in enumerate(source.splitlines(), 1):
        if line.lstrip().startswith(_LINE_COMMENT_START):
            continue
        start = 0
        while True:
            idx = line.find("//", start)
            if idx < 0:
                break
            prefix = line[:idx]
            if idx > 0 and line[idx - 1] == ":":
                start = idx + 2
                continue
            if all(prefix.count(q) % 2 == 0 for q in ('"', "'")):
                segments.append((lineno, line[idx:]))
                break
            start = idx + 2
    return segments


def _block_comment_segments(
    source: str, opener: str, closer: str
) -> list[tuple[int, str]]:
    """提取块注释片段（斜杠星号形态）。行内的末尾注释由调用方另行处理。"""
    segments: list[tuple[int, str]] = []
    inside = False
    for lineno, line in enumerate(source.splitlines(), 1):
        if inside or opener in line:
            segments.append((lineno, line))
            if closer in line and not inside:
                continue
        if opener in line and closer not in line.split(opener, 1)[1]:
            inside = True
        elif inside and closer in line:
            inside = False
    return segments


def _fenced_lines(source: str) -> list[tuple[int, str]]:
    """Markdown：跳过围栏内的代码块，其余行视为正文。

    围栏 = 行首 **3 个及以上**连续的反引号（缩进不超过 3 个空格）；以**单个**
    反引号开头的行是内联代码，**不是**围栏——把后者当围栏会让后续真标题被
    跳过，守卫随即产生假阳性。
    """
    segments: list[tuple[int, str]] = []
    fence = False
    for lineno, line in enumerate(source.splitlines(), 1):
        stripped = line.lstrip(" ")
        indent = len(line) - len(stripped)
        if indent <= 3 and stripped[:3] in ("\u0060\u0060\u0060", "~~~"):
            fence = not fence
            continue
        if not fence:
            segments.append((lineno, line))
    return segments


def iter_segments(path: str, source: str) -> list[tuple[int, str]]:
    """按文件类型提取需要检查的「注释 / 文档」片段。

    改动本节等于改动「漏写会不会被拦」的边界——AGENTS.md 的扫描面一节须同步。
    """
    suffix = Path(path).suffix.lower()
    if suffix == ".py":
        return _python_segments(source)
    if suffix == ".rs":
        return (
            _line_comment_segments(source, _LINE_COMMENT_START)
            + _rust_trailing_comment_segments(source)
            + _block_comment_segments(source, "/*", "*/")
        )
    if suffix == ".md":
        return _fenced_lines(source)
    if suffix in (".yml", ".yaml", ".toml", ".ps1", ".sh"):
        return _line_comment_segments(source, ("#",))
    return []


def scan_source(path: str, source: str) -> list[Hit]:
    """扫描单个文件内容的注释 / 文档片段。"""
    hits: list[Hit] = []
    seen: set[tuple[int, int]] = set()
    for lineno, text in iter_segments(path, source):
        key = (lineno, hash(text))
        if key in seen:
            continue
        seen.add(key)
        hits.extend(_collect_hits(path, lineno, text))
    return hits


def scan_text(path: str, text: str) -> list[Hit]:
    """扫描「片段文本」（提交信息 / diff 新增行），按语境宽松判定。"""
    hits: list[Hit] = []
    for lineno, line in enumerate(text.splitlines(), 1):
        hits.extend(_collect_hits(path, lineno, line, prose=_looks_like_prose(line)))
    return hits


def _git(args: list[str], *, cwd: Path | None = None) -> str:
    proc = subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} 失败：{proc.stderr.strip()}")
    return proc.stdout


def _repo_root() -> Path:
    """定位仓库根。

    必须显式解析：从子目录运行时，git 默认返回**相对当前目录**的路径，
    拼到别处后文件不存在、读取失败被静默跳过，全量扫描零命中「通过」——
    等于没扫。钩子与 CI 都在仓库根运行，但命令行不保证。
    """
    return Path(_git(["rev-parse", "--show-toplevel"]).strip())


def tracked_files(root: Path | None = None) -> list[str]:
    """列出纳入扫描的跟踪文件。"""
    base = root or _repo_root()
    out = _git(["ls-files"], cwd=base)
    return [
        line
        for line in out.splitlines()
        if line and line.lower().endswith(_TEXT_SUFFIXES)
    ]


def scan_tree(root: Path | None = None) -> list[Hit]:
    """全量扫描跟踪文件。"""
    base = root or _repo_root()
    hits: list[Hit] = []
    for rel in tracked_files(base):
        path = base / rel
        try:
            source = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        hits.extend(scan_source(rel, source))
    return hits


_HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def _added_lines(diff_text: str) -> dict[str, set[int]]:
    """从 unified diff 解析「新增行的新文件行号」：{路径: {行号}}。"""
    added: dict[str, set[int]] = {}
    current: str | None = None
    lineno = 0
    for line in diff_text.splitlines():
        if line.startswith("+++ "):
            raw = line[4:].strip()
            current = None if raw == "/dev/null" else raw.removeprefix("b/")
            if current is not None:
                added.setdefault(current, set())
            continue
        if line.startswith("@@"):
            match = _HUNK_RE.match(line)
            lineno = int(match.group(1)) if match else 0
            continue
        if current is None or lineno == 0:
            continue
        if line.startswith("+"):
            added[current].add(lineno)
        if line.startswith(("+", " ")):
            lineno += 1
    return added


def _file_at_rev(rev: str, path: str) -> str | None:
    """取某版本下的文件内容；失败（新增/删除文件）返回 None。"""
    try:
        return _git(["show", f"{rev}:{path}"])
    except RuntimeError:
        return None


def scan_lines(path: str, source: str, lines: set[int]) -> list[Hit]:
    """在给定的行号集合上扫描该文件内容（增量定位的纯函数形态）。"""
    return [hit for hit in scan_source(path, source) if hit.line in lines]


def scan_diff(diff_text: str, *, staged: bool) -> list[Hit]:
    """扫描增量变更：只报新增行上的命中。

    定位方式：先解析新增行行号，再取该文件的**完整版本**做语法分段，只保留
    落在新增行上的命中——这样多行注释内的行号依然精确。

    版本选择：行号是**新文件**的行号，因此内容必须取「新增侧」的版本——
    staged 取索引，否则取 HEAD。取基线会把新行号套到旧内容上，报出一堆
    「本轮已删除」的假命中。取不到（新增或删除的文件）时跳过。
    """
    hits: list[Hit] = []
    for path, lines in _added_lines(diff_text).items():
        if not lines:
            continue
        source = _file_at_rev(":0" if staged else "HEAD", path)
        if source is None:
            continue
        hits.extend(scan_lines(path, source, lines))
    return hits


_SCISSORS = "------------------------ >8 ------------------------"


def strip_git_commentary(message: str) -> str:
    """去掉提交信息文件里不会进入提交的部分：注释行与 scissors 之后的 diff。"""
    kept: list[str] = []
    for line in message.splitlines():
        if line.startswith("#"):
            if _SCISSORS in line:
                break
            continue
        kept.append(line)
    return "\n".join(kept)


def scan_commit_message(message: str) -> list[Hit]:
    """扫描提交信息（subject + body；git 注释行与 scissors 之后不算）。"""
    return scan_text("<commit-message>", strip_git_commentary(message))


def _format(hits: Iterable[Hit]) -> str:
    return "\n".join(
        f"  {hit.path}:{hit.line} [{hit.rule}] {hit.text[:120]}" for hit in hits
    )


def _staged_diff() -> str:
    return _git(["diff", "--cached", "--unified=0"])


def _range_diff(ref: str) -> str:
    return _git(["diff", "--unified=0", f"{ref}...HEAD"])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="禁止编号引用扫描")
    parser.add_argument("--ref", default=None, help="扫描相对该基线的差异（CI 用）")
    parser.add_argument("--tree", action="store_true", help="全量扫描跟踪文件")
    parser.add_argument("--message-file", default=None, help="扫描提交信息文件")
    args = parser.parse_args(argv)

    # git 取不到差异时必须拒绝放行：当作空 diff 会「扫描没跑却通过」。
    # CI 里 fetch 失败或基线引用无效都会走到这里，静默通过等于门禁失守。
    try:
        if args.message_file:
            text = Path(args.message_file).read_text(encoding="utf-8", errors="replace")
            hits = scan_commit_message(text)
            target = f"提交信息 {args.message_file}"
        elif args.tree:
            hits = scan_tree()
            target = "全量跟踪文件"
        elif args.ref:
            hits = scan_diff(_range_diff(args.ref), staged=False)
            target = f"相对 {args.ref} 的差异"
        else:
            hits = scan_diff(_staged_diff(), staged=True)
            target = "staged 新增内容"
    except (RuntimeError, OSError) as e:
        # OSError：提交信息文件读不到（钩子参数错误等）——同样属于「扫描没跑」
        print(
            f"[forbidden-refs] 扫描未执行，拒绝放行：{e}\n"
            "请检查 git 环境/基线引用/提交信息文件后重试",
            file=sys.stderr,
        )
        return 2

    if hits:
        print(f"检测到编号引用（{target}），请改为说明「为什么」与失败模式：")
        print(_format(hits))
        print(
            "\n编号指向仓库外不可访问的文档，读者无法据此还原上下文。"
            "\n确需保留时，请在同行加注释 allow-plan-ref（并在 review 中说明理由）。"
        )
        return 1
    print("未发现编号引用")
    return 0


if __name__ == "__main__":
    sys.exit(main())
