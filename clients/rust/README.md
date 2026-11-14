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

Types and request plumbing are generated; the behavior layer is handwritten. Never edit generated files by hand.

From the repository root, run `powershell -File scripts/build.ps1 run -p weflow-regen --`.

## Scope

Source databases stay read-only. `sync` refreshes the in-memory index; media export writes local copies.

SNS timelines are the one gap: the server has those endpoints, the generated layer does not wrap them yet.

## Targeted media recovery

Restore one unique message with `export_media_by_message(talker, platform_message_id, sender, timestamp)`.

The result provides the actual `mediaId`, `fileName` and credential-free relative `mediaUrl`.

`media_bytes` tries the existing filename first. After a 404 it exports the target once and fetches the returned handle once.

- 404: target or original media missing.
- 409: ambiguous target or registration changed.
- 422: no supported local media.
- 405/501: server lacks targeted export; upgrade it before recovery.

I/O and decode errors propagate. No recent-message search is used.

## Links

- [Server repository](https://github.com/ren-yc/weflow-server)
- [API reference](https://github.com/ren-yc/weflow-server/blob/master/docs/weflow-server-api.md)
- [CHANGELOG](https://github.com/ren-yc/weflow-server/blob/master/CHANGELOG.md)
- [License](https://github.com/ren-yc/weflow-server/blob/master/LICENSE) — MIT
