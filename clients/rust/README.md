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

let client = weflow_client::Client::new("http://127.0.0.1:5033", "your-token");
client.wait_ready("<wxid>", Duration::from_secs(30)).await?;
let sessions = client.list_all_sessions(Default::default()).await?;
```

## Scope

The server exposes a strictly read-only surface: accounts, sessions, messages, contacts, group members, SNS timelines (WeChat only), media export, and an SSE push stream. This client mirrors that surface one-to-one; see the server repository for the API reference and the interface contract the client is tested against.

## License

MIT
