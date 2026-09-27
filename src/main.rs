//! 服务二进制。
//!
//! 只有这几行，而且**全部走 `pub` 门面** —— 它是独立 crate，看不见 `pub(crate)` 的实现面。
//! 这不是限制：它保证「嵌入者能做的事」与「二进制能做的事」是**同一个集合**。

fn main() {
    if let Err(e) = weflow_server::run_cli() {
        eprintln!("[fatal] {e:#}");
        std::process::exit(1);
    }
}
