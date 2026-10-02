"""Behavior-layer tests against an in-process ASGI mock that mirrors the
server's wire shapes. No real database: the fixtures here emit the same
JSON the server's golden snapshots pin."""

from __future__ import annotations

import json
from typing import Any, Callable, Dict, List, Optional

import httpx
import pytest

from weflow_sdk import Client, ClientError, NotReady
from weflow_sdk.generated.weflow_sdk import models as gen

TOKEN = "test-token-0123456789abcdef"


def auth_ok(headers: Dict[str, str]) -> None:
    assert headers.get("authorization") == f"Bearer {TOKEN}", "auth must go in the header"


class Mock:
    """Mutable fixture state shared between the test and the ASGI app."""

    def __init__(self) -> None:
        self.states: List[str] = []
        self.pull_pages: List[dict] = []
        self.pull_queries: List[dict] = []
        self.media_calls: List[str] = []
        self.media_hit_first = True
        self.chatlab_page: Optional[dict] = None
        self.messages_query: Optional[dict] = None
        self.sse_frames: List[str] = []
        self.sse_reconnect_ids: List[Optional[str]] = []

    def asgi_app(self):
        mock = self

        async def app(scope, receive, send):
            path = scope["path"]
            # parse query string
            raw = scope.get("query_string", b"").decode()
            query: Dict[str, str] = {}
            for kv in raw.split("&"):
                if kv:
                    k, _, v = kv.partition("=")
                    query[k] = v
            headers = {
                k.decode().lower(): v.decode()
                for k, v in scope.get("headers", [])
            }
            auth_ok(headers)
            body: Any = None
            if path == "/api/v1/accounts" and scope["method"] == "POST":
                body = {"success": True, "state": "indexing"}
            elif path == "/api/v1/accounts":
                state = mock.states.pop(0) if mock.states else "ready"
                body = {
                    "success": True,
                    "accounts": [
                        {"wxid": "wxid_mock", "db_storage": "",
                         "message_count": 1, "state": state}
                    ],
                }
            elif path.endswith("/messages") and path.startswith("/api/v1/sessions/"):
                mock.pull_queries.append(query)
                if mock.pull_pages:
                    body = mock.pull_pages.pop(0)
                else:
                    await send({"type": "http.response.start", "status": 404, "headers": []})
                    await send({"type": "http.response.body", "body": b"fixture exhausted"})
                    return
            elif path == "/chatlab/messages":
                mock.messages_query = query
                body = mock.chatlab_page
            elif path.startswith("/api/v1/media/"):
                mock.media_calls.append(path.rsplit("/", 1)[-1])
                if mock.media_hit_first:
                    mock.media_hit_first = False  # first GET misses; the miss mints the file
                    await send({"type": "http.response.start", "status": 404, "headers": []})
                    await send({"type": "http.response.body", "body": b"not exported"})
                    return
                await send({
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"application/octet-stream")],
                })
                await send({"type": "http.response.body", "body": b"png-bytes"})
                return
            elif path == "/api/v1/messages":
                mock.messages_query = query
                body = {"success": True, "count": 0, "has_more": False,
                        "talker": "", "media": {}, "messages": []}
            elif path == "/api/v1/push/messages":
                last = headers.get("last-event-id")
                mock.sse_reconnect_ids.append(last)
                payload = ("\n".join(mock.sse_frames) + "\n").encode()
                await send({
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"text/event-stream")],
                })
                await send({"type": "http.response.body", "body": payload})
                return
            if body is None:
                await send({"type": "http.response.start", "status": 404, "headers": []})
                await send({"type": "http.response.body", "body": b"fixture missing"})
                return
            payload = json.dumps(body).encode()
            await send({
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-type", b"application/json")],
            })
            await send({"type": "http.response.body", "body": payload})

        return app


def make_client(mock: Mock) -> Client:
    client = Client("http://mock", TOKEN)
    # Route the behavior layer's transport into the in-process mock.
    client._http = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=mock.asgi_app()),
        base_url="http://mock",
    )
    return client


def pull_page(msgs: list[dict], has_more: bool, next_since: int, next_offset: int) -> dict:
    return {
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "members": [],
        "messages": msgs,
        "meta": {"groupId": "", "name": "", "ownerId": "",
                 "platform": "weflow", "type": "chat"},
        "sync": {"hasMore": has_more, "nextSince": next_since,
                 "nextOffset": next_offset, "watermark": 2000},
    }


def msg(mid: int, ts: int) -> dict:
    return {"accountName": "alice", "content": f"m{mid}", "groupNickname": "",
            "platformMessageId": str(mid), "sender": "alice", "timestamp": ts, "type": 1}


async def test_ensure_ready_polls_until_ready() -> None:
    mock = Mock()
    mock.states = ["indexing", "indexing"]
    client = make_client(mock)
    await client.ensure_ready("wxid_mock",
                              {"wxid": "wxid_mock", "db_path": "X:/db"},
                              timeout=5)
    await client.aclose()


async def test_ensure_ready_times_out_with_last_state() -> None:
    mock = Mock()
    mock.states = ["indexing"] * 100
    client = make_client(mock)
    with pytest.raises(NotReady) as exc:
        await client.ensure_ready("wxid_mock",
                                  {"wxid": "wxid_mock", "db_path": "X:/db"},
                                  timeout=0.6)
    assert exc.value.last_state == "indexing"
    await client.aclose()


async def test_drain_session_echoes_cursors_verbatim() -> None:
    mock = Mock()
    mock.pull_pages = [
        pull_page([msg(1, 990), msg(2, 1000)], True, 1000, 7),
        pull_page([msg(3, 1500)], False, 2000, 0),
    ]
    client = make_client(mock)
    pages: list[list[int]] = []
    total = await client.drain_session(
        "alice", 500,
        lambda batch: pages.append([int(m.platform_message_id) for m in batch]),
    )
    assert total == 3
    assert pages == [[1, 2], [3]]
    assert len(mock.pull_queries) == 2
    assert mock.pull_queries[0] == {"since": "500"}
    assert mock.pull_queries[1] == {"since": "1000", "offset": "7"}
    await client.aclose()


async def test_media_bytes_exports_then_retries() -> None:
    mock = Mock()
    mock.chatlab_page = {
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "count": 0, "members": [], "messages": [],
        "meta": {"groupId": "", "name": "", "ownerId": "",
                 "platform": "weflow", "type": "chat"},
        "page": {"hasMore": False, "nextCursor": None},
        "talker": "alice",
    }
    client = make_client(mock)
    message = gen.ChatlabMessage.model_validate({
        "accountName": "alice", "content": "x", "groupNickname": "",
        "media": {"type": "image", "fileName": "abc.png", "md5": "z"},
        "platformMessageId": "1", "sender": "alice", "timestamp": 1, "type": 1,
    })
    data = await client.media_bytes(message)
    assert data == b"png-bytes"
    assert mock.media_calls == ["abc.png", "abc.png"]
    assert mock.messages_query == {"talker": "alice", "media": "1"}
    await client.aclose()


async def test_watch_decodes_frames_and_reconnects_with_last_event_id() -> None:
    mock = Mock()
    mock.sse_frames = [
        ": heartbeat",
        "id: 7",
        "event: message.new",
        "data: " + json.dumps({
            "event": "message.new", "rawid": "9", "sessionId": "alice",
            "sessionType": "chat", "sourceName": "alice",
            "timestamp": 1700000001, "content": "hi",
        }),
    ]
    client = make_client(mock)
    import asyncio as _asyncio

    # The stream body ends after one frame; watch() treats EOF as reconnect.
    # A plain async-for/break would leave that reconnect loop alive, so the
    # generator is driven by bounded anext() and explicitly closed.
    agen = client.watch()
    event = await _asyncio.wait_for(anext(agen), timeout=5)
    assert isinstance(event, gen.EventNew)
    assert event.rawid == "9"
    assert event.session_id == "alice"
    # Second stream: the reconnect sleeps 0.5s then re-opens the connection
    # (recording Last-Event-ID) before the next EOF retry. Poll the mock's
    # connection log instead of cancelling anext: cancellation lands inside
    # the generator's sleep and makes the timeout path brittle.
    import time as _time

    # Advance the generator past its yield: the reconnect runs only when the
    # consumer asks again. The second anext blocks inside the reconnect's
    # backoff sleep (the mock body is already drained), so bound it and
    # swallow the timeout - the connection log is what the assert checks.
    try:
        await _asyncio.wait_for(anext(agen), timeout=3)
    except (TimeoutError, StopAsyncIteration):
        pass
    await agen.aclose()
    assert any(v == "7" for v in mock.sse_reconnect_ids if v is not None), (
        f"reconnect must carry Last-Event-ID: {mock.sse_reconnect_ids}")
    await client.aclose()
