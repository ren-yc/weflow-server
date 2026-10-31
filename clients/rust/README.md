# weflow-client

Typed Rust client for [weflow-server](https://github.com/ren-yc/weflow-server), a read-only HTTP/SSE service over local WeChat 4.x databases.

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
    let client = Client::new("http://127.0.0.1:5033", "<token>");
    client.wait_ready("<wxid>", Duration::from_secs(30)).await?;
    let sessions = client.list_all_sessions(None, None).await?;
    println!("{} sessions", sessions.len());
    Ok(())
}
```

Requires `tokio` with `rt-multi-thread` and `macros`; the SDK uses tokio for timeouts and retries.

## Generated and handwritten layers

Types and request plumbing are generated; the behavior layer is handwritten. Never edit them by hand — run `cargo run -p weflow-regen --` instead.

## Scope

The server is strictly read-only: accounts, sessions, messages, contacts, group members, media export and an SSE push stream.

SNS timelines are the one gap: the server has those endpoints, the generated layer does not wrap them yet.

## Links

- [Server repository](https://github.com/ren-yc/weflow-server)
- [API reference](https://github.com/ren-yc/weflow-server/blob/master/docs/weflow-server-api.md)
- [CHANGELOG](https://github.com/ren-yc/weflow-server/blob/master/CHANGELOG.md)
- [License](https://github.com/ren-yc/weflow-server/blob/master/LICENSE) — MIT
