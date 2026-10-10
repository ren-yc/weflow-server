# weflow-client

Typed Rust client for [weflow-server](https://github.com/ren-yc/weflow-server) — a headless, read-only HTTP/SSE service over local WeChat 4.x chat databases.

Generated from the server's OpenAPI description (types + request plumbing), with a handwritten behavior layer on top: transport failures folded into a proper error tree, readiness polling, pagination helpers, SSE streaming, and media download helpers.

## Install

```
cargo add weflow-client
```

## Minimal example

```rust
use std::time::Duration;
use weflow_client::client::{Client, ClientError};

#[tokio::main]
async fn main() -> Result<(), ClientError> {
    let client = Client::new("http://127.0.0.1:5033", "your-token");
    client.wait_ready("<wxid>", Duration::from_secs(30)).await?;
    // page_size / keyword are both optional; None keeps the server defaults.
    let sessions = client.list_all_sessions(None, None).await?;
    println!("{} sessions", sessions.len());
    Ok(())
}
```

需要 `tokio`（`rt-multi-thread` ＋ `macros`）作为依赖 —— SDK 自身用 tokio 做超时与重试。

## Scope

The server exposes a strictly read-only surface: accounts, sessions, messages, contacts, group members, SNS timelines, media export, and an SSE push stream. This client covers **all of it except the SNS timelines** — the server has those endpoints, the generated layer does not wrap them yet, so reach for plain HTTP if you need them (the gap is deliberate, not silent: the contract pins the surface this client is tested against). See the server repository for the API reference.

## License

MIT
