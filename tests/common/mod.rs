//! 夹具造库器的**转发层** —— 真身在 `weflow_server::testing`（随 `testing` feature 编译）。
//!
//! 为什么真身不在测试目录里：需要这份造库器的有两个调用方，而它们互相看不见
//! 对方的代码 —— 集成测试是独立 crate（链库），**根包二进制同样是独立 crate**，
//! 批量导出的夹具生成入口（CLI 的 `--rows`）够不着只在 `tests/` 下存在的模块。
//! 移到库里之后，两边用的是同一份造库器，夹具不会各自漂移。
//!
//! 保留这一层是让既有集成测试的 `mod common;` 与 `common::build_wechat_account(..)`
//! 调用点原样可用；新代码可以直接用 `weflow_server::testing`。

#![allow(unused_imports)]

pub use weflow_server::testing::*;
