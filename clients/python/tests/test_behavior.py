"""Behavior-layer tests against an in-process ASGI mock that mirrors the
server's wire shapes. No real database: the fixtures here emit the same
JSON the server's golden snapshots pin."""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any

import httpx
import pytest

from weflow_sdk import Client, ClientError, NotReady
from weflow_sdk import client as sdkmod
from weflow_sdk.generated.weflow_sdk import models as gen

TOKEN = "test-token-0123456789abcdef"


def auth_ok(headers: dict[str, str]) -> None:
    assert headers.get("authorization") == f"Bearer {TOKEN}", "auth must go in the header"


class Mock:
    """Mutable fixture state shared between the test and the ASGI app."""

    def __init__(self) -> None:
        self.states: list[str] = []
        self.pull_pages: list[dict] = []
        self.pull_queries: list[dict] = []
        self.media_calls: list[str] = []
        self.media_hit_first = True
        self.chatlab_page: dict | None = None
        self.messages_query: dict | None = None
        # Raw bytes for the SSE response body; str fixtures are encoded.
        self.sse_frames: list[str] = []
        self.sse_body: bytes | None = None
        # Per-connection bodies, popped in order; the last one repeats.
        self.sse_seq: list[bytes] = []
        # Split delivery: when set, sent as separate body messages.
        self.sse_chunks: list[bytes] | None = None
        self.sse_connections: int = 0
        self.sse_last_ids: list[str | None] = []
        # Request counters: which endpoints a call actually touched.
        self.post_calls: int = 0
        self.get_accounts_calls: int = 0
        # The POST /api/v1/accounts response body (registration semantics).
        self.register_body: dict | None = None
        # When set, GET /api/v1/accounts answers this page verbatim.
        self.accounts_page: dict | None = None
        # When set, GET /api/v1/messages answers this page verbatim.
        self.native_page: dict | None = None
        # When set, GET /api/v1/contacts answers this page verbatim.
        self.contacts_page: dict | None = None
        # FIFO pages for GET /api/v1/sessions.
        self.sessions_pages: list[dict] = []
        # Whether each /health hit carried credentials (it must not).
        self.health_auth: list[bool] = []
        self.health_body: dict | None = None

    def asgi_app(self):
        mock = self

        async def app(scope, receive, send):
            path = scope["path"]
            raw = scope.get("query_string", b"").decode()
            query: dict[str, str] = {}
            for kv in raw.split("&"):
                if kv:
                    k, _, v = kv.partition("=")
                    query[k] = v
            headers = {
                k.decode().lower(): v.decode()
                for k, v in scope.get("headers", [])
            }
            if path == "/health":
                # Unauthenticated by design: record whether credentials rode
                # along (the SDK must not send any) instead of asserting them.
                mock.health_auth.append("authorization" in headers)
            else:
                auth_ok(headers)
            body: Any = None
            if path == "/api/v1/accounts" and scope["method"] == "POST":
                mock.post_calls += 1
                body = mock.register_body or {"success": True, "state": "indexing"}
            elif path == "/api/v1/accounts":
                mock.get_accounts_calls += 1
                if mock.accounts_page is not None:
                    body = mock.accounts_page
                else:
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
                # The native face is camelCase on the wire; a fallback with
                # snake_case keys (or a missing exportPath) decodes as a shape
                # break for every caller of this route.
                body = mock.native_page or {
                    "success": True, "count": 0, "hasMore": False, "talker": "",
                    "media": {"count": 0, "enabled": False, "exportPath": ""},
                    "messages": [],
                }
            elif path == "/health":
                body = mock.health_body or {
                    "account": "ready", "status": "ok", "version": "0.0.0",
                }
            elif path == "/api/v1/contacts":
                body = mock.contacts_page
            elif path == "/api/v1/sessions":
                body = mock.sessions_pages.pop(0) if mock.sessions_pages else None
            elif path == "/api/v1/push/messages":
                mock.sse_connections += 1
                mock.sse_last_ids.append(headers.get("last-event-id"))
                if mock.sse_seq:
                    payload = mock.sse_seq.pop(0) if len(mock.sse_seq) > 1 else mock.sse_seq[0]
                elif mock.sse_body is None:
                    payload = ("\n".join(mock.sse_frames) + "\n").encode()
                else:
                    payload = mock.sse_body
                await send({
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"text/event-stream")],
                })
                if mock.sse_chunks is not None:
                    last = len(mock.sse_chunks) - 1
                    for i, part in enumerate(mock.sse_chunks):
                        await send({
                            "type": "http.response.body",
                            "body": part,
                            "more_body": i < last,
                        })
                else:
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


def sse_frame(event: str, payload: dict, id_: int) -> list[str]:
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
    # The readiness poll never ran: counted GETs, not an empty list.
    assert mock.get_accounts_calls == 0
    await client.aclose()


async def test_wait_ready_is_wait_only() -> None:
    # wait_ready must not register: only the listing endpoint is polled.
    # Counted, not inferred - an empty states list would also 'pass'
    # if the poll simply never ran.
    mock = Mock()
    mock.states = ["indexing", "ready"]
    client = make_client(mock)
    await client.wait_ready("wxid_mock", timeout=5)
    assert mock.post_calls == 0
    assert mock.get_accounts_calls == 2
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


# ---- strengthened SSE guards: over-cap frames, multi-data join, backoff ----
# (test_watch_caps_the_buffer_on_malformed_streams et al.)

_NL = chr(10)  # LF spelled without escapes: fixtures below build raw frames


class _StopWatch(Exception):
    """Raised from a stubbed sleep to end watch() deterministically."""

async def test_watch_caps_the_buffer_on_malformed_streams(monkeypatch, caplog) -> None:
    # A stream that never closes a frame would grow the buffer without
    # bound; past the cap watch ends the stream and reconnects instead.
    # NOTE: the in-process ASGITransport buffers the whole response body,
    # so this test's signal is the cap WARNING + escalating reconnects, not
    # connection timing. Deleting the cap logic makes assertion 1+2 fail.
    mock = Mock()
    mock.sse_body = b"data: " + b"x" * (1 << 21)  # 2 MiB, no blank line ever
    client = make_client(mock)
    with caplog.at_level(logging.WARNING, logger="weflow_sdk.client"):
        agen = client.watch()
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(anext(agen), timeout=3)
        await agen.aclose()
        warned = [r.getMessage() for r in caplog.records]
    await client.aclose()
    cap_number = str(sdkmod._SSE_BUFFER_CAP)
    # 1) loud: the guard names the cap in a WARNING
    assert any(cap_number in m and "over" in m for m in warned)
    # 2) the stream was abandoned, not buffered: the reconnect loop ran
    assert mock.sse_connections >= 2
    # 3) contrast: lift the cap - the same stream then goes through the
    #    EOF-flush path (undecodable final frame), NOT the cap path: no
    #    warning carrying the cap number.
    monkeypatch.setattr(sdkmod, "_SSE_BUFFER_CAP", 1 << 30)
    mock2 = Mock()
    mock2.sse_body = b"data: " + b"x" * (1 << 21)
    client2 = make_client(mock2)
    caplog.clear()
    with caplog.at_level(logging.WARNING, logger="weflow_sdk.client"):
        agen2 = client2.watch()
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(anext(agen2), timeout=2)
        await agen2.aclose()
        warned2 = [r.getMessage() for r in caplog.records]
    await client2.aclose()
    assert not any(cap_number in m for m in warned2)


async def test_watch_delivers_valid_frames_after_overflow_reconnect() -> None:
    # Ending the over-cap stream is only half the guard: the reconnect
    # must still deliver the legitimate frames that follow.
    good = _NL.join(sse_frame("message.new", new_payload("11", "after"), 12)) + _NL + _NL
    mock = Mock()
    mock.sse_seq = [b"data: " + b"x" * (1 << 21), good.encode()]
    client = make_client(mock)
    agen = client.watch()
    (event,) = await collect(agen, 1, timeout=10)
    assert event.rawid == "11"
    assert mock.sse_connections >= 2
    await agen.aclose()
    await client.aclose()


async def test_watch_rejects_oversized_single_frame_whole_and_split() -> None:
    # The cap bounds one *complete* frame too: an over-cap frame is not
    # delivered and the stream reconnects - whether it lands as one chunk
    # or is split across two body messages.
    big = json.dumps(new_payload("9", "x" * ((1 << 20) + 8192)))
    whole = ("id: 7" + _NL + "event: message.new" + _NL + "data: " + big
             + _NL + _NL).encode()
    mock = Mock()
    mock.sse_body = whole
    client = make_client(mock)
    agen = client.watch()
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(anext(agen), timeout=2)
    await agen.aclose()
    await client.aclose()
    assert mock.sse_connections >= 2, "over-cap whole frame must end the stream"

    mock2 = Mock()
    mock2.sse_chunks = [
        ("id: 7" + _NL + "event: message.new" + _NL + "data: " + big + _NL).encode(),
        _NL.encode(),
    ]
    client2 = make_client(mock2)
    agen2 = client2.watch()
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(anext(agen2), timeout=2)
    await agen2.aclose()
    await client2.aclose()
    assert mock2.sse_connections >= 2, "over-cap split frame must end the stream"


async def test_watch_delivers_frame_just_under_the_cap() -> None:
    # Contrast for the single-frame guard: a frame below the cap must still
    # be delivered, else "reject over-cap frames" would just mean "reject
    # everything" and the guard would have no observable upper bound.
    body = json.dumps(new_payload("13", "fits"))
    assert len(body) < sdkmod._SSE_BUFFER_CAP
    mock = Mock()
    mock.sse_body = ("id: 1" + _NL + "event: message.new" + _NL + "data: " + body
                     + _NL + _NL).encode()
    client = make_client(mock)
    agen = client.watch()
    (event,) = await collect(agen, 1)
    assert event.rawid == "13"
    await agen.aclose()
    await client.aclose()


async def test_watch_joins_multiple_data_lines_per_frame() -> None:
    # SSE spec: several data lines in one frame join with LF into the one
    # payload that gets dispatched. The old per-line overwrite kept only the
    # last line and dropped the frame as undecodable - a shape the server
    # does not emit today, pinned here as a guard.
    text = json.dumps(new_payload("9", "joined"))
    cut = text.index(",") + 1
    mock = Mock()
    mock.sse_body = ("id: 7" + _NL + "event: message.new" + _NL + "data: " + text[:cut]
                     + _NL + "data: " + text[cut:] + _NL + _NL).encode()
    client = make_client(mock)
    agen = client.watch()
    (event,) = await collect(agen, 1)
    assert event.rawid == "9"
    assert event.content == "joined"
    await agen.aclose()
    await client.aclose()


async def test_watch_joins_three_data_lines_boundary() -> None:
    text = json.dumps(new_payload("12", "three lines"))
    c1 = text.index(",") + 1
    c2 = text.index(",", c1) + 1
    mock = Mock()
    mock.sse_body = ("id: 7" + _NL + "data: " + text[:c1] + _NL + "data: " + text[c1:c2]
                     + _NL + "data: " + text[c2:] + _NL + _NL).encode()
    client = make_client(mock)
    agen = client.watch()
    (event,) = await collect(agen, 1)
    assert event.rawid == "12"
    assert event.content == "three lines"
    await agen.aclose()
    await client.aclose()


async def test_watch_backoff_escalates_on_persistent_overflow(monkeypatch) -> None:
    # A stream that overflows on every connection must escalate 0.5, 1, 2,
    # 4... The old shape reset the delay on every 200, so a malformed-but-
    # connectable server reconnected forever at a flat 0.5s (measured: 6
    # connects in 3s). Sleep durations are captured through a stub, so the
    # assertion is about the schedule, not wall-clock.
    recorded: list = []
    real_sleep = asyncio.sleep

    async def fake_sleep(delay, *a, **kw):
        recorded.append(delay)
        if len(recorded) >= 4:
            raise _StopWatch()
        await real_sleep(0)

    monkeypatch.setattr(sdkmod.asyncio, "sleep", fake_sleep)
    mock = Mock()
    mock.sse_seq = [b"data: " + b"x" * (1 << 21)]
    client = make_client(mock)
    agen = client.watch()
    try:
        with pytest.raises(_StopWatch):
            await anext(agen)
    finally:
        monkeypatch.undo()
        await agen.aclose()
        await client.aclose()
    assert recorded == [0.5, 1.0, 2.0, 4.0]


async def test_watch_backoff_returns_to_floor_after_clean_stream_end(monkeypatch) -> None:
    # The contrast assertion: a stream that ends cleanly (EOF, no overflow)
    # reconnects from the 0.5s floor instead of carrying a stale doubled
    # delay over. One frame per connection, three deliveries: without the
    # clean-end reset the schedule would be [0.5, 1.0].
    recorded: list = []
    real_sleep = asyncio.sleep

    async def fake_sleep(delay, *a, **kw):
        recorded.append(delay)
        if len(recorded) >= 2:
            raise _StopWatch()
        await real_sleep(0)

    monkeypatch.setattr(sdkmod.asyncio, "sleep", fake_sleep)
    mock = Mock()
    mock.sse_body = (_NL.join(sse_frame("message.new", new_payload("9", "hi"), 7))
                     + _NL + _NL).encode()
    client = make_client(mock)
    agen = client.watch()
    try:
        first = await anext(agen)
        assert first.rawid == "9"
        second = await anext(agen)
        assert second.rawid == "9"
        with pytest.raises(_StopWatch):
            await anext(agen)
    finally:
        monkeypatch.undo()
        await agen.aclose()
        await client.aclose()
    assert recorded == [0.5, 0.5]


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


async def test_watch_drops_oversized_final_frame_at_eof_without_blank() -> None:
    # A final frame that is over cap AND lacks the trailing blank line must
    # not be delivered: the accumulation check (pending lines + buffer after
    # each chunk) fires before the EOF flush can fold the tail, so the
    # stream ends un-trusted and reconnects. Deleting the accumulation cap
    # check would deliver this frame; that is covered by the cap test above
    # - this pins the no-blank-line edge specifically.
    big = json.dumps(new_payload("14", "x" * ((1 << 20) + 8192)))
    body = ("id: 9" + _NL + "event: message.new" + _NL + "data: " + big).encode()
    mock = Mock()
    mock.sse_body = body
    client = make_client(mock)
    agen = client.watch()
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(anext(agen), timeout=2)
    await agen.aclose()
    await client.aclose()
    assert mock.sse_connections >= 2, "over-cap EOF frame must not be delivered"


# ---- health / accounts / register ---------------------------------------


async def test_health_reports_version_and_account_phase() -> None:
    mock = Mock()
    mock.health_body = {"account": "ready", "status": "ok", "version": "9.9.9"}
    client = make_client(mock)
    health = await client.health()
    assert health.version == "9.9.9"
    assert health.status == "ok"
    assert health.account is gen.AccountPhase.READY
    assert mock.health_auth == [False], "/health is unauthenticated: no credentials"


async def test_accounts_expose_state_error_and_message_count() -> None:
    mock = Mock()
    mock.accounts_page = {
        "success": True,
        "accounts": [
            {"wxid": "wxid_a", "db_storage": "X:/a", "message_count": 12,
             "state": "ready"},
            {"wxid": "wxid_b", "db_storage": "X:/b", "message_count": 0,
             "state": "error", "error": "bad key"},
        ],
    }
    client = make_client(mock)
    accounts = await client.accounts()
    assert accounts[0].message_count == 12
    assert accounts[1].error == "bad key", "the failure reason exists only on this face"


async def test_register_returns_raw_state_without_polling() -> None:
    mock = Mock()
    mock.register_body = {
        "success": False, "state": "account_conflict",
        "occupied_by": "wxid_other", "occupied_status": "ready",
    }
    client = make_client(mock)
    outcome = await client.register({"wxid": "wxid_mock", "db_path": "X:/db"})
    assert outcome.state == "account_conflict"
    assert outcome.status is None, "this state carries no status"
    assert outcome.body["occupied_by"] == "wxid_other", "refusal extras stay reachable"
    assert mock.post_calls == 1, "one POST, no retry"
    assert mock.get_accounts_calls == 0, "register must not poll: waiting is wait_ready's job"


# ---- list_messages / contacts / media_bytes_by_id ------------------------


async def test_list_messages_pages_by_offset_and_exposes_native_fields() -> None:
    mock = Mock()
    mock.native_page = {
        "success": True, "count": 1, "hasMore": True, "talker": "alice",
        "media": {"count": 0, "enabled": False, "exportPath": "X:/export"},
        "messages": [{
            "appmsgSubtype": None, "baseType": 1, "content": "hi",
            "createTime": 1_700_000_000, "isSend": 1, "localId": 7, "localType": 3,
            "media": {"fileName": "abc.png", "mediaId": "abc123", "md5": "d41d8",
                      "type": "image"},
            "parsedContent": "hi", "quote": None, "rawContent": "<msg>hi</msg>",
            "replyToMessageId": "41", "senderName": "张三", "senderUsername": "alice",
            "serverId": "42", "sortSeq": 1,
        }],
    }
    client = make_client(mock)
    page = await client.list_messages("alice", limit=500, offset=1000, media=True)
    assert page.has_more is True, "paging continues until has_more is false"
    assert page.media.export_path == "X:/export"
    message = page.messages[0]
    assert message.raw_content == "<msg>hi</msg>", "the ChatLab shape drops rawContent"
    assert message.is_send == 1
    assert message.local_type == 3
    assert message.media is not None and message.media.media_id == "abc123"
    assert mock.messages_query == {
        "talker": "alice", "limit": "500", "offset": "1000", "media": "1",
    }


async def test_list_messages_accepts_unix_seconds_and_rejects_garbage() -> None:
    mock = Mock()
    client = make_client(mock)
    await client.list_messages("alice", start="1700000000")
    assert mock.messages_query["start"] == "1700000000", "the server parses unix seconds too"
    with pytest.raises(sdkmod.BadDate):
        await client.list_messages("alice", end="2025-01-01")


async def test_contacts_page_decodes_rows_and_paging_fields() -> None:
    mock = Mock()
    mock.contacts_page = {
        "success": True, "count": 1, "total": 42, "hasMore": True,
        "contacts": [{"alias": "", "avatarUrl": "", "displayName": "张三",
                      "nickname": "三儿", "remark": "客户张三", "type": "friend",
                      "username": "alice"}],
    }
    client = make_client(mock)
    page = await client.contacts(limit=100, offset=0)
    assert page.count == 1
    assert page.total == 42
    assert page.has_more is True
    assert page.contacts[0].display_name == "张三"


async def test_list_all_sessions_pages_and_collapses_cross_page_duplicates() -> None:
    def session(username: str) -> dict:
        return {"displayName": username, "lastTimestamp": 1, "messageCount": 0,
                "sessionType": "private", "summary": None, "type": 0,
                "unreadCount": 0, "username": username}

    mock = Mock()
    # A live list can shift between pages: "b" shows up twice. The loop stops
    # on an empty page (this face has no has_more).
    mock.sessions_pages = [
        {"success": True, "count": 2, "sessions": [session("a"), session("b")]},
        {"success": True, "count": 2, "sessions": [session("b"), session("c")]},
        {"success": True, "count": 0, "sessions": []},
    ]
    client = make_client(mock)
    all_sessions = await client.list_all_sessions()
    assert [s.username for s in all_sessions] == ["a", "b", "c"]


async def test_media_bytes_by_id_fetches_a_single_segment_handle() -> None:
    mock = Mock()
    mock.media_hit_first = False  # no export side door: the handle already resolves
    client = make_client(mock)
    data = await client.media_bytes_by_id("abc123.png")
    assert data == b"png-bytes"
    assert mock.media_calls == ["abc123.png"], "one GET for the handle it was given"
