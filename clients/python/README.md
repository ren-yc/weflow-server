# weflow-sdk

Typed Python client for [weflow-server](https://github.com/ren-yc/weflow-server) — a headless, read-only HTTP/SSE service over local WeChat 4.x chat databases.

Generated from the server's OpenAPI description (types + request plumbing), with a handwritten behavior layer on top: transport failures folded into a proper error tree, readiness polling, pagination helpers, SSE streaming, and media download helpers.

## Install

```
pip install weflow-sdk
```

## Minimal example

```python
import asyncio

from weflow_sdk import Client


async def main() -> None:
    client = Client(base_url="http://127.0.0.1:5033", token="your-token")
    await client.health()
    sessions = await client.list_all_sessions()
    await client.aclose()


asyncio.run(main())
```

## Scope

The server exposes a strictly read-only surface: accounts, sessions, messages, contacts, group members, SNS timelines (WeChat only), media export, and an SSE push stream. This client mirrors that surface one-to-one; see the server repository for the API reference and the interface contract the client is tested against.

## License

MIT
