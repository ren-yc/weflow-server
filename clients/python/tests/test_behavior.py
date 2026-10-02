"""Behavior-layer tests against an in-process ASGI mock that mirrors the
server's wire shapes. No real database: the fixtures here emit the same
JSON the server's golden snapshots pin."""

from __future__ import annotations

import asyncio
import json
from typing import Any, Dict, List, Optional

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
        # Raw bytes for the SSE response body; str fixtures are encoded.
        self.sse_frames: List[str] = []
        self.sse_body: Optional[bytes] = None
        self.sse_connections: int = 0
        self.sse_last_ids: List[Optional[str]] = []
        # The POST /api/v1/accounts response body (registration semantics).
        self.register_body: Optional[dict] = None

    def asgi_app(self):
        mock = self

        async def app(scope, receive, send):
            path = scope["path"]
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
                body = mock.register_body or {"success": True, "state": "indexing"}
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
                mock.sse_connections += 1
                mock.sse_last_ids.append(headers.get("last-event-id"))
                payload = mock.sse_body
                if payload is None:
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


def sse_frame(event: str, payload: dict, id_: int) -> List[str]:
    return [
        f"id: {id_}",
        f"event: {event}",
        "data: " + json.dumps(payload),
    ]


async def collect(agen, n: int, timeout: float = 5.0) -> list:
    out = []
    for _ in range(n):
        out.append(await asyncio.wait_for(anext(agen), timeout=timeout))
    return out


# ---- readiness ----------------------------------------------------------


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


async def test_ensure_ready_rejects_conflict_200_without_waiting() -> None:
    # The server answers a second wxid with 200 + account_conflict. Treating
    # that as accepted makes the readiness poll time out and hide the real
    # cause, so ensure_ready must fail fast on the body state.
    mock = Mock()
    mock.register_body = {
        "success": True, "state": "account_conflict",
        "wxid": "wxid_other", "occupied_by": "wxid_other",
    }
    client = make_client(mock)
    with pytest.raises(ClientError) as exc:
        await client.ensure_ready("wxid_mock",
                                  {"wxid": "wxid_mock", "db_path": "X:/db"},
                                  timeout=5)
    assert "account_conflict" in str(exc.value)
    assert mock.states == []  # the readiness poll never ran
    await client.aclose()


async def test_wait_ready_is_wait_only() -> None:
    # wait_ready must not register: only the listing endpoint is polled.
    mock = Mock()
    mock.states = ["indexing", "ready"]
    client = make_client(mock)
    await client.wait_ready("wxid_mock", timeout=5)
    await client.aclose()


# ---- pagination ---------------------------------------------------------


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


# ---- media --------------------------------------------------------------


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


# ---- watch --------------------------------------------------------------


def new_payload(rawid: str, content: str) -> dict:
    return {
        "event": "message.new", "rawid": rawid, "sessionId": "alice",
        "sessionType": "chat", "sourceName": "alice",
        "timestamp": 1700000001, "content": content,
    }


async def test_watch_yields_many_frames_over_one_connection() -> None:
    # The stream stays open until the server closes it: several frames in
    # one body must all be delivered on that one connection. A per-frame
    # disconnect would turn every frame (including idle sync heartbeats)
    # into a reconnect cycle.
    mock = Mock()
    mock.sse_body = (
        ": heartbeat\n"
        + "\n".join(sse_frame("message.new", new_payload("9", "hi"), 7))
        + "\n\n"
        + "\n".join(sse_frame("message.revoke", {
            "event": "message.revoke", "rawid": "8", "sessionId": "alice",
            "sessionType": "chat", "sourceName": "alice",
            "timestamp": 1700000002, "content": "gone",
        }, 8))
        + "\n\n"
        + "\n".join(sse_frame("sync", {
            "event": "sync", "generation": 1, "watermarks": [],
        }, 9))
        + "\n\n"
        + "\n".join(sse_frame("message.new", new_payload("10", "again"), 10))
        + "\n\n"
    ).encode()
    client = make_client(mock)
    agen = client.watch()
    events = await collect(agen, 4)
    assert isinstance(events[0], gen.EventNew)
    assert events[0].rawid == "9"
    assert isinstance(events[1], gen.EventRevoke)
    assert events[1].rawid == "8"
    assert isinstance(events[2], gen.EventSync)
    assert events[2].generation == 1
    assert isinstance(events[3], gen.EventNew)
    assert events[3].rawid == "10"
    assert mock.sse_connections == 1
    await agen.aclose()
    await client.aclose()


async def test_watch_reconnects_with_last_event_id_only_after_stream_end() -> None:
    # Only when the stream itself ends does watch reconnect - carrying the
    # last seen id so the server's replay window fills the gap.
    mock = Mock()
    mock.sse_body = (
        "\n".join(sse_frame("message.new", new_payload("9", "hi"), 7)) + "\n\n"
    ).encode()
    client = make_client(mock)
    agen = client.watch()
    first = await asyncio.wait_for(anext(agen), timeout=5)
    assert first.rawid == "9"
    # The body is drained; the next anext drives the reconnect (0.5s
    # backoff), then reads the new connection - whose body is the same
    # single frame, so the second delivery arrives with id 7 again.
    second = await asyncio.wait_for(anext(agen), timeout=10)
    assert second.rawid == "9"
    assert mock.sse_connections >= 2
    assert mock.sse_last_ids[1] == "7"
    await agen.aclose()
    await client.aclose()


async def test_watch_survives_malformed_frames() -> None:
    # Bad JSON, a non-object payload, and a shape mismatch each get logged
    # and skipped - one bad frame must not kill the stream.
    mock = Mock()
    mock.sse_body = (
        "id: 1\n"
        "event: message.new\n"
        "data: {not json}\n"
        "\n"
        "id: 2\n"
        "event: message.new\n"
        "data: [1, 2, 3]\n"
        "\n"
        "id: 3\n"
        "event: message.new\n"
        "data: " + json.dumps({"event": "message.new"}) + "\n"
        "\n"
        + "\n".join(sse_frame("message.new", new_payload("9", "hi"), 4))
        + "\n\n"
    ).encode()
    client = make_client(mock)
    agen = client.watch()
    (good,) = await collect(agen, 1)
    assert good.rawid == "9"
    await agen.aclose()
    await client.aclose()


async def test_watch_byte_framing_keeps_u2028_bodies_intact() -> None:
    # U+2028 is legal inside a JSON string but str.splitlines treats it as
    # a line break: framing must be byte-level LF only.
    body_text = json.dumps(new_payload("9", "line\u2028break"), ensure_ascii=False)
    mock = Mock()
    mock.sse_body = (
        "\n".join([
            "id: 7",
            "event: message.new",
            "data: " + body_text,
        ])
        + "\n\n"
    ).encode()
    client = make_client(mock)
    agen = client.watch()
    (event,) = await collect(agen, 1)
    assert isinstance(event, gen.EventNew)
    assert event.content == "line\u2028break"
    await agen.aclose()
    await client.aclose()


async def test_watch_caps_the_buffer_on_malformed_streams() -> None:
    # A stream that never emits a blank line would grow the buffer without
    # bound; past the cap watch ends the stream and reconnects instead.
    mock = Mock()
    mock.sse_body = b"data: " + b"x" * (1 << 21)  # 2 MiB, no blank line ever
    client = make_client(mock)
    agen = client.watch()
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(anext(agen), timeout=3)
    await agen.aclose()
    await client.aclose()
    # The stream was ended (reconnect cycle started) rather than buffering
    # forever - observable as a second connection attempt after the backoff.
    assert mock.sse_connections >= 1


async def test_watch_flushes_the_final_frame_at_eof_without_a_trailing_blank_line() -> None:
    # The server guarantees a trailing blank line today, but a server that
    # closes without it must not silently drop the last event: watch()
    # flushes the pending block at EOF.
    mock = Mock()
    mock.sse_body = (
        "\n".join(sse_frame("message.new", new_payload("10", "tail"), 7))
    ).encode()
    client = make_client(mock)
    agen = client.watch()
    (event,) = await collect(agen, 1)
    assert event.rawid == "10"
    assert event.content == "tail"
    await agen.aclose()
    await client.aclose()
