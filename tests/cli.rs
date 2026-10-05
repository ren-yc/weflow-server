//! 命令行子命令面的行为钉子：用真实二进制跑，断言**退出码**与**兼容口径**。
//!
//! 为什么不用单元测试代替：退出码是进程边界上的行为（clap 的 Error::exit、process::exit），
//! 在进程内测不到；而这些码正是脚本据以分支的东西——用 1 冒充 2，脚本就分不出
//! 「我参数写错了」和「服务没起来」。
//!
//! 全部用 CARGO_BIN_EXE_weflow-server 拿到 cargo 刚构建出来的二进制，不假设 target 目录结构。

use std::process::Command;

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// 跑一次二进制。第三个参数是**额外**环境变量。
///
/// 为什么不用 std::env::set_var：那是进程级全局，而同一个测试二进制里的用例是并行跑的——
/// 一个用例设的 token 会被另一个用例读到，断言就变成看调度运气。基座里三个开关一律清掉，
/// 免得开发者本机的值把测试变成非确定性。
fn run(args: &[&str], extra: &[(&str, &str)]) -> Out {
    let mut c = Command::new(env!("CARGO_BIN_EXE_weflow-server"));
    c.env_remove("WEFLOW_BASE_URL");
    c.env_remove("WEFLOW_TOKEN");
    c.env_remove("WEFLOW_EMBED_CONFIG");
    for (k, v) in extra {
        c.env(k, v);
    }
    let r = c.args(args).output().expect("spawn weflow-server");
    Out {
        code: r.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&r.stdout).to_string(),
        stderr: String::from_utf8_lossy(&r.stderr).to_string(),
    }
}

fn bare(args: &[&str]) -> Out {
    run(args, &[])
}

/// 兼容口径：**老写法一个字符都不用改**。
///
/// --version 的文案是既有的（weflow-server <版本号>），不是 clap 生成的格式；两者看着相近，
/// 但脚本里已有的正则会被咬。这条断言钉的是「走的是老解析器」这个事实本身。
#[test]
fn legacy_flags_still_handled_by_the_old_parser() {
    let o = bare(&["--version"]);
    assert_eq!(o.code, 0, "--version 应退出 0；stderr: {}", o.stderr);
    // 既有文案逐字钉住：不是 clap 生成的 "weflow-server 0.8.0" 近似串，而是老解析器那一行。
    let expect = format!("weflow-server {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(
        o.stdout.trim(),
        expect,
        "--version 必须由既有解析器处理（走 clap 会换掉文案），实际 stdout: {:?}",
        o.stdout
    );

    let h = bare(&["--help"]);
    assert_eq!(h.code, 0, "--help 应退出 0");
    assert!(
        h.stdout.contains("用法: weflow-server [选项]") && h.stdout.contains("--watch-debounce-ms"),
        "--help 应仍是既有解析器那份（含全部旗标说明），实际: {:?}",
        h.stdout
    );
}

/// serve 子命令 = 裸跑：后面的旗标仍由老解析器处理。
#[test]
fn serve_subcommand_is_the_same_as_bare_run() {
    let o = bare(&["serve", "--version"]);
    assert_eq!(o.code, 0, "serve 后面的旗标必须交给既有解析器；stderr: {}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        format!("weflow-server {}", env!("CARGO_PKG_VERSION")),
        "serve --version 应与 --version 给出同一行文案，实际: {:?}",
        o.stdout
    );
}

/// 用法错误必须是 2，而且不能伪装成取值错误。
#[test]
fn unknown_subcommand_is_a_usage_error_exit_2() {
    let o = bare(&["bogus"]);
    assert_eq!(o.code, 2, "未知子命令应为用法错误；stderr: {}", o.stderr);
    assert!(o.stderr.contains("bogus"), "报错要点名那个词，实际: {}", o.stderr);
}

/// search 的 --keyword 必填：缺了就是用法错误（2），不是运行期错误（1）。
#[test]
fn search_requires_keyword_as_usage_error() {
    let o = bare(&["search"]);
    assert_eq!(o.code, 2, "search 缺 --keyword 应为 2；stderr: {}", o.stderr);
    assert!(o.stderr.contains("keyword"), "报错要点名 --keyword，实际: {}", o.stderr);
}

/// HTTP 形态的 messages 必须点名 --talker（服务端按会话查询）。
#[test]
fn messages_over_http_requires_talker() {
    let o = run(&["messages"], &[("WEFLOW_TOKEN", "probe-token-not-used")]);
    assert_eq!(o.code, 2, "缺 --talker 是用法错误，应为 2；stderr: {}", o.stderr);
    assert!(o.stderr.contains("talker"), "报错要点名 --talker，实际: {}", o.stderr);
}

/// HTTP 形态缺 token 是运行期错误（1），并且要指路。
///
/// 这里必须「还没连上就失败」：若实现是先连再报鉴权，本断言会因端口上有没有服务而变成
/// 非确定性的。
#[test]
fn missing_token_fails_before_touching_the_network() {
    // 指向一个**保证没有监听**的端口：若实现是「先连、连不上再报鉴权」，错误文案会退化成连接失败，
    // 下面那条「不得出现连接失败措辞」的断言就会抓住它 —— 只断言「退 1 ＋ 提到变量名」抓不住。
    let o = run(&["sessions"], &[("WEFLOW_BASE_URL", "http://127.0.0.1:1")]);
    assert_eq!(o.code, 1, "缺 token 是运行期错误；stderr: {}", o.stderr);
    assert!(o.stderr.contains("WEFLOW_TOKEN"), "报错要给出补救（环境变量名），实际: {}", o.stderr);
    let lower = o.stderr.to_lowercase();
    for wording in ["connection refused", "tcp connect", "error sending request", "connect error"] {
        assert!(!lower.contains(wording), "缺 token 不该走到网络（出现「{wording}」）: {}", o.stderr);
    }
}

/// --embedded 只开放给只读查询类：accounts 与 sync 没有这个开关。
///
/// 为什么值得单独钉一条：把 --embedded 加到 accounts／sync 上是「顺手一致」的改法，而那会
/// 给出错误的答案——accounts 问的是服务端此刻绑定了什么，sync 是写动作；进程内路径回答的
/// 都不是这两个问题。
#[test]
fn embedded_is_refused_on_write_and_account_subcommands() {
    for sub in ["accounts", "sync"] {
        let o = bare(&[sub, "--embedded"]);
        assert_eq!(o.code, 2, "{sub} 不该接受 --embedded；stderr: {}", o.stderr);
        assert!(o.stderr.contains("embedded"), "{sub} 的报错应指向 --embedded，实际: {}", o.stderr);
    }
}

/// --embedded 缺配置文件时：运行期错误（1）＋ 点名环境变量。
#[test]
fn embedded_without_config_is_a_runtime_error() {
    let o = bare(&["sessions", "--embedded"]);
    assert_eq!(o.code, 1, "缺配置是运行期错误；stderr: {}", o.stderr);
    assert!(o.stderr.contains("WEFLOW_EMBED_CONFIG"), "报错要给出补救（环境变量名），实际: {}", o.stderr);
}

/// 步骤 0 第 2 项：`--rows` 是**测试专用隐藏参数**——不进 `--help`。
///
/// 为什么这条值得钉：参数一旦出现在帮助里，就会有用户拿它去「导出三十万条试试」，
/// 而那造出来的是假账号语料。它存在的唯一理由，是让大语料的内存断言跑得起来。
/// 另一半（发布二进制里根本没有这个参数）由 `#[cfg(feature = "testing")]` 在编译期
/// 保证：默认 feature 的整轮 clippy/test 编译的就是不含它的版本。
#[test]
fn export_help_does_not_advertise_the_rows_param() {
    let mut c = Command::new(env!("CARGO_BIN_EXE_weflow-server"));
    c.env_remove("WEFLOW_BASE_URL").env_remove("WEFLOW_TOKEN");
    let r = c.args(["export", "--help"]).output().expect("spawn");
    assert_eq!(r.status.code(), Some(0), "export --help 应成功");
    let text = String::from_utf8_lossy(&r.stdout).to_string();
    assert!(text.contains("--out"), "帮助里应有常规参数: {}", text);
    assert!(!text.contains("--rows"), "--rows 不该出现在帮助里: {}", text);
}

/// `--out` 是必需参数：不给就是用法错误（2），而不是把导出物写到某个默认目录。
///
/// 为什么刻意不给默认路径：落盘是有意的动作，猜一个目录会把几百个文件写到用户没打算
/// 放的地方，而那种事情发生时导出已经跑完了。
#[test]
fn export_requires_out_dir_as_usage_error() {
    let o = bare(&["export"]);
    assert_eq!(o.code, 2, "export 缺 --out 应为用法错误；stderr: {}", o.stderr);
    assert!(o.stderr.contains("out"), "报错要点名 --out，实际: {}", o.stderr);
}

/// 非法的 `--since` 是**用法错误（退出码 2）**，不是运行期错误（1）。
///
/// 为什么这条要紧：`--limit abc` 与 `--format xyz` 这类非法取值走 clap 的 value_parser、退 2；
/// 而 `--since` 此前只在后面手工解析、退 1。同一类「用法写错了」给出两种退出码，脚本就没法
/// 按码分流 —— `1` 在这套契约里是「连不上／被拒」那类可重试的运行期错误。
#[test]
fn invalid_since_is_a_usage_error_exit_2() {
    for args in [
        vec!["export", "--out", "unused-out-dir", "--since", "20240230"],
        vec!["messages", "--talker", "x", "--since", "abc"],
    ] {
        let o = bare(&args);
        assert_eq!(o.code, 2, "{args:?} 应为用法错误；stderr: {}", o.stderr);
        assert!(o.stderr.contains("since"), "报错要点名 --since，实际: {}", o.stderr);
    }
}
