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

The server is strictly read-only: accounts, sessions, messages, contacts, group members, media export and an SSE push stream.

SNS timelines are the one gap: the server has those endpoints, the generated layer does not wrap them yet.

## Links

- [Server repository](https://github.com/ren-yc/weflow-server)
- [API reference](https://github.com/ren-yc/weflow-server/blob/master/docs/weflow-server-api.md)
- [CHANGELOG](https://github.com/ren-yc/weflow-server/blob/master/CHANGELOG.md)
- [License](https://github.com/ren-yc/weflow-server/blob/master/LICENSE) — MIT
