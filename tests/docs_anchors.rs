//! 文档锚点守卫：把「文档同步义务」变成 CI 可执行。
//!
//! 为什么需要它：文档里的内部链接会**静默腐烂**——标题改名、章节删除之后，
//! 链接仍然「看起来没问题」，直到有人点它。这类腐烂没有任何编译期信号。
//!
//! 三条守卫，各自**先断言自己的边界标记存在**再检查：
//!   1. 同文档锚点：每个 `](#...)` 都必须命中本文档的某个标题；
//!      指向其它文件的相对链接必须指向存在的文件。
//!   2. 目录覆盖：含「目录」小节的文档，其目录必须覆盖全部顶层章节。
//!   3. 模块表链接：模块表里若出现指向详解小节的链接，它们必须命中详解小节。
//!
//! 为什么第 3 条是「若出现」：本仓库的架构文档目前是 MVP 节集，详解小节尚未落地。
//! 边界不存在时守卫会**打印一行说明并跳过**，而不是假装通过——
//! 一个会静默退化成「什么都没查」的守卫，比没有守卫更危险。

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// 围栏判定用的反引号字符（写成转义，避免本文件的示例被自己当成围栏）。
const FENCE: char = '`';

fn repo_root() -> PathBuf {
    // cargo 会设置这个变量；独立用 rustc --test 编译时退化为当前目录。
    match option_env!("CARGO_MANIFEST_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => env::current_dir().expect("取不到当前目录"),
    }
}

/// GitHub 的标题 slug 规则。
///
/// 小写 → 去掉既非字母数字、也非 `_`/`-`/空格的字符 → 空格转连字符；
/// 同名标题按出现次序追加 `-1`、`-2`（第一个不加后缀）。
fn slug(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let kept: String = lowered
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .collect();
    kept.replace(' ', "-")
}

/// 围栏 = 行首**三个及以上**连续的反引号（缩进不超过 3 个空格）。
///
/// 单个反引号开头的行是内联代码，**不是**围栏：把它当围栏会让后续真标题被跳过，
/// 于是「提取到的标题集合」凭空变小，守卫随即产生假阳性。
fn is_fence(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let t = line.trim_start_matches(' ');
    let fence_char = match t.chars().next() {
        Some(c) if c == FENCE || c == '~' => c,
        _ => return false,
    };
    t.chars().take_while(|c| *c == fence_char).count() >= 3
}

/// 提取文档的标题锚点（跳过围栏内的代码块）。
fn anchors(text: &str) -> Vec<String> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let t = line.trim_start();
        if !t.starts_with('#') {
            continue;
        }
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if hashes == 0 || hashes > 6 {
            continue;
        }
        let rest = &t[hashes..];
        if !rest.starts_with(' ') {
            continue;
        }
        let base = slug(rest);
        let n = seen.entry(base.clone()).or_insert(0);
        let anchor = if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        *n += 1;
        out.push(anchor);
    }
    out
}

/// 提取 `[文字](目标)` 形式的链接目标。
fn links(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == ']' && chars[i + 1] == '(' {
            let mut j = i + 2;
            let mut acc = String::new();
            while j < chars.len() && chars[j] != ')' {
                acc.push(chars[j]);
                j += 1;
            }
            out.push(acc);
            i = j;
        }
        i += 1;
    }
    out
}

/// 扫描目标：这几份文档受守卫保护。
fn targets() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = vec![root.join("README.md"), root.join("AGENTS.md"), root.join("docs/architecture.md")];
    if let Ok(entries) = fs::read_dir(root.join("docs")) {
        let mut api: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with("-api.md"))
                    .unwrap_or(false)
            })
            .collect();
        api.sort();
        out.extend(api);
    }
    out
}

/// 截出「目录」小节自身的正文（到下一个顶层标题为止）。
///
/// 覆盖判定必须**只看目录小节里的链接**：用全文链接集合去判，只要标题在文档任何
/// 位置被链接过一次就会「覆盖」，于是删掉目录项也不会报警——守卫形同虚设。
fn toc_section(text: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_end() == "## 目录" {
            inside = true;
            continue;
        }
        if inside {
            if line.starts_with("## ") {
                break;
            }
            out.push(line);
        }
    }
    if inside {
        Some(out.join("\n"))
    } else {
        None
    }
}

struct Doc {
    path: PathBuf,
    text: String,
    anchors: Vec<String>,
    links: Vec<String>,
    toc: Option<String>,
}

fn load() -> Vec<Doc> {
    let mut docs = Vec::new();
    for path in targets() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let anchors = anchors(&text);
        let links = links(&text);
        let toc = toc_section(&text);
        docs.push(Doc { path, text, anchors, links, toc });
    }
    docs
}

// ── 守卫 1：锚点与相对链接 ──

#[test]
fn same_document_anchors_resolve() {
    let docs = load();
    assert!(!docs.is_empty(), "一份受保护的文档都没读到——路径或工作目录不对，守卫没有可检查的对象");
    let mut problems = Vec::new();
    for doc in &docs {
        // 边界断言：没有任何标题时，「所有锚点都命中」是空真，必须显式挡住。
        assert!(
            !doc.anchors.is_empty(),
            "{} 里一个标题都没提取到——围栏规则或路径不对，守卫会退化成空真",
            doc.path.display()
        );
        let set: HashSet<&String> = doc.anchors.iter().collect();
        let dir = doc.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        for link in &doc.links {
            if let Some(anchor) = link.strip_prefix('#') {
                if !set.contains(&anchor.to_string()) {
                    problems.push(format!("{}: 锚点 #{anchor} 不存在", doc.path.display()));
                }
            } else if !link.contains("://") && !link.starts_with("mailto:") {
                let target = link.split('#').next().unwrap_or("");
                if !target.is_empty() && !dir.join(target).exists() {
                    problems.push(format!("{}: 链接目标 {target} 不存在", doc.path.display()));
                }
            }
        }
    }
    assert!(problems.is_empty(), "悬空文档链接：\n  {}", problems.join("\n  "));
}

// ── 守卫 2：目录覆盖 ──

#[test]
fn toc_covers_top_level_sections() {
    let docs = load();
    assert!(!docs.is_empty(), "一份受保护的文档都没读到");
    let mut checked = 0;
    let mut problems = Vec::new();
    for doc in &docs {
        // 边界断言：只对**声明了目录**的文档生效。
        // 注意必须**整行精确匹配**：用 `contains("## 目录")` 会把「## 目录结构」
        // 这类不相干小节也算进来，于是「目录没有链接」变成假阳性。
        let Some(toc) = doc.toc.as_deref() else {
            continue;
        };
        checked += 1;
        let toc_text = toc.to_string();
        let toc_links: Vec<String> = links(&toc_text)
            .into_iter()
            .filter(|l| l.starts_with('#'))
            .collect();
        assert!(
            !toc_links.is_empty(),
            "{} 有「目录」小节但一个锚点链接都没有",
            doc.path.display()
        );
        for section in doc
            .text
            .lines()
            .filter_map(|l| l.strip_prefix("## "))
            .filter(|s| !s.starts_with("目录"))
        {
            let want = slug(section);
            if !toc_links.iter().any(|l| l == &format!("#{want}")) {
                problems.push(format!("{}: 目录未覆盖「{section}」", doc.path.display()));
            }
        }
    }
    assert!(checked > 0, "没有任何文档声明了「目录」小节——守卫 2 没有可检查的对象，请确认架构文档结构");
    assert!(problems.is_empty(), "目录覆盖不全：\n  {}", problems.join("\n  "));
}

// ── 守卫 3：模块表 → 详解小节 ──

#[test]
fn module_table_links_point_into_details() {
    let docs = load();
    assert!(!docs.is_empty(), "一份受保护的文档都没读到");
    let mut applicable = 0;
    let mut problems = Vec::new();
    for doc in &docs {
        // 边界：只有存在「核心模块详解」小节时本条才有检查对象。
        // 不存在就明确报告「不适用」，而不是悄悄通过。
        if !doc.text.contains("### 核心模块详解") {
            continue;
        }
        applicable += 1;
        let detail: HashSet<&String> = doc.anchors.iter().collect();
        for link in &doc.links {
            let Some(anchor) = link.strip_prefix('#') else {
                continue;
            };
            if anchor.starts_with("核心模块") && !detail.contains(&anchor.to_string()) {
                problems.push(format!("{}: 模块表链接 #{anchor} 未命中详解小节", doc.path.display()));
            }
        }
    }
    if applicable == 0 {
        eprintln!("[docs_anchors] 守卫 3 不适用：没有文档包含「核心模块详解」小节（MVP 节集尚未落地该小节）");
    }
    assert!(problems.is_empty(), "模块表链接未命中：\n  {}", problems.join("\n  "));
}
