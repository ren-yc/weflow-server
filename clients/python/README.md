# weflow-sdk

Typed Python client for [weflow-server](https://github.com/ren-yc/weflow-server), a read-only HTTP/SSE service over local WeChat 4.x databases.

## Install

```
pip install weflow-sdk
```

## Minimal example

```python
import asyncio

from weflow_sdk import Client


async def main() -> None:
    client = Client(base_url="http://127.0.0.1:5033", token="<token>")
    await client.health()
    sessions = await client.list_all_sessions()
    await client.aclose()

asyncio.run(main())
```

`SyncClient` mirrors `Client` method-for-method and drives its own private event-loop thread.

## Generated and handwritten layers

Types are generated; the behavior layer is handwritten. Never edit `generated/` — run `python scripts/regen.py`.

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
