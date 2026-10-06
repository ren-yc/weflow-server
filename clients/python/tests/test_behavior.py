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

from weflow_sdk import (
    CONNECT_TIMEOUT,
    READ_TIMEOUT,
    Client,
    ClientError,
    NotReady,
    ShapeError,
    StatusError,
)
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
        # Status the media route answers with once the first miss is done.
        self.media_status: int = 200
        self.chatlab_page: dict | None = None
        self.messages_query: dict | None = None
        # When set, the chatlab export route rejects any query whose `talker`
        # differs from it (the retry must ask for the session id, not the
        # display name).
        self.chatlab_query_talker: str | None = None
        # Raw bytes for the SSE response body; str fixtures are encoded.
        self.sse_frames: list[str] = []
        self.sse_body: bytes | None = None
        # Per-connection bodies, popped in order; the last one repeats.
        self.sse_seq: list[bytes] = []
        # Split delivery: when set, sent as separate body messages.
        self.sse_chunks: list[bytes] | None = None
        self.sse_connections: int = 0
        # Status the SSE route answers with (default 200).
        self.sse_status: int = 200
        self.sse_last_ids: list[str | None] = []
        # Request counters: which endpoints a call actually touched.
        self.post_calls: int = 0
        self.get_accounts_calls: int = 0
        # POST /api/v1/sync hits: a manual sync is a *write*, so the counter is
        # what proves no read path triggers one on its own.
        self.sync_calls: int = 0
        # The POST /api/v1/accounts response body (registration semantics).
        self.register_body: dict | None = None
        # When set, GET /api/v1/accounts answers this page verbatim.
        self.accounts_page: dict | None = None
        # Statuses the accounts route answers, in order (FIFO), before the normal
        # page. A transient 5xx must be waited out by wait_ready; a 4xx must not.
        self.accounts_status_seq: list[int] = []
        # When set, GET /api/v1/messages answers this page verbatim.
        self.native_page: dict | None = None
        # When set, GET /api/v1/contacts answers this page verbatim.
        self.contacts_page: dict | None = None
        # When set, GET /api/v1/group-members answers this page verbatim.
        self.group_members_page: dict | None = None
        # Query params seen by the group-members route, in order.
        self.group_members_queries: list[dict] = []
        # FIFO pages for GET /api/v1/sessions.
        self.sessions_pages: list[dict] = []
        # Query params seen by the sessions route, in order.
        self.sessions_queries: list[dict] = []
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
            elif path == "/api/v1/sync":
                mock.sync_calls += 1
                body = {"success": True, "newMessages": 7, "revokeMessages": 2}
            elif path == "/api/v1/accounts":
                mock.get_accounts_calls += 1
                if mock.accounts_status_seq:
                    await send({
                        "type": "http.response.start",
                        "status": mock.accounts_status_seq.pop(0),
                        "headers": [],
                    })
                    await send({"type": "http.response.body", "body": b"not yet"})
                    return
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
                expected = mock.chatlab_query_talker
                if expected is not None and query.get("talker") != expected:
                    await send({"type": "http.response.start", "status": 400, "headers": []})
                    await send({"type": "http.response.body", "body": b"export asked with wrong talker"})
                    return
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
                    "status": mock.media_status,
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
            elif path == "/api/v1/group-members":
                mock.group_members_queries.append(query)
                body = mock.group_members_page
            elif path == "/api/v1/sessions":
                mock.sessions_queries.append(query)
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
                    "status": mock.sse_status,
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


async def test_wait_ready_rides_out_a_transient_5xx_but_not_a_4xx() -> None:
    # The account listing is polled while the server builds its index. A momentary
    # 5xx is a dropped round, not a verdict - aborting here would turn "still
    # warming up" into a client-side failure. A 4xx is a misconfiguration and must
    # surface at once instead of spinning out the whole budget.
    mock = Mock()
    mock.accounts_status_seq = [503, 403]
    client = make_client(mock)
    with pytest.raises(StatusError) as exc:
        await client.wait_ready("wxid_mock", timeout=30)
    assert exc.value.status == 403, "4xx propagates verbatim"
    assert mock.get_accounts_calls == 2, "the 5xx round must be followed by another poll"
    assert mock.accounts_status_seq == [], "both injected statuses were consumed"
    assert mock.post_calls == 0, "wait_ready stays wait-only"
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


async def test_pull_page_decodes_the_sync_block_and_sends_the_cursors() -> None:
    mock = Mock()
    mock.pull_pages = [pull_page([msg(1, 1000)], True, 1000, 4)]
    client = make_client(mock)
    page = await client.pull_page("alice", 500, offset=7, limit=3)
    assert [m.platform_message_id for m in page.messages] == ["1"]
    assert page.sync.has_more is True
    assert page.sync.next_since == 1000
    assert page.sync.next_offset == 4
    assert page.sync.watermark == 2000
    assert mock.pull_queries[0] == {"since": "500", "offset": "7", "limit": "3"}
    await client.aclose()


async def test_pull_page_omits_defaulted_cursors_instead_of_sending_zero() -> None:
    """Absence, not ``0``: the server defaults both cursors, so a client that
    sends ``since=0`` is claiming a cursor it never read."""
    mock = Mock()
    mock.pull_pages = [pull_page([], False, 0, 0)]
    client = make_client(mock)
    await client.pull_page("alice", None)
    assert mock.pull_queries[0] == {}
    await client.aclose()


# ---- media --------------------------------------------------------------


async def test_media_bytes_exports_then_retries() -> None:
    # The display name and the session id must DIFFER: the old retry used
    # `message.account_name` as the export `talker`, and the fixture let the
    # two coincide so the mistake was invisible. The mock now rejects any
    # export query whose talker is not the session id we pass.
    mock = Mock()
    mock.chatlab_page = {
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "count": 0, "members": [], "messages": [],
        "meta": {"groupId": "", "name": "", "ownerId": "",
                 "platform": "weflow", "type": "chat"},
        "page": {"hasMore": False, "nextCursor": None},
        "talker": "alice",
    }
    mock.chatlab_query_talker = "wxid_alice"
    client = make_client(mock)
    message = gen.ChatlabMessage.model_validate({
        "accountName": "Alice DISPLAY", "content": "x", "groupNickname": "",
        "media": {"type": "image", "fileName": "abc.png", "md5": "z"},
        "platformMessageId": "1", "sender": "alice", "timestamp": 1, "type": 1,
    })
    data = await client.media_bytes(message, "wxid_alice")
    assert data == b"png-bytes"
    assert mock.media_calls == ["abc.png", "abc.png"]
    assert mock.messages_query == {"talker": "wxid_alice", "media": "1"}
    await client.aclose()


async def test_media_bytes_rejects_redirect_like_statuses() -> None:
    """The bytes check must be "not 2xx", not "4xx/5xx".

    A 302 without `Location` (or a 304) must surface as StatusError: falling
    through the old ">= 400" check would hand the caller an empty redirect
    body as if it were the media bytes, with no signal at all.
    """
    for status in (302, 304):
        mock = Mock()
        mock.chatlab_page = {
            "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
            "count": 0, "members": [], "messages": [],
            "meta": {"groupId": "", "name": "", "ownerId": "",
                     "platform": "weflow", "type": "chat"},
            "page": {"hasMore": False, "nextCursor": None},
            "talker": "alice",
        }
        mock.media_status = status
        client = make_client(mock)
        message = gen.ChatlabMessage.model_validate({
            "accountName": "alice", "content": "x", "groupNickname": "",
            "media": {"type": "image", "fileName": "abc.png", "md5": "z"},
            "platformMessageId": "1", "sender": "alice", "timestamp": 1, "type": 1,
        })
        with pytest.raises(sdkmod.StatusError) as exc_info:
            await client.media_bytes(message, "wxid_alice")
        assert exc_info.value.status == status
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


async def test_watch_treats_a_3xx_as_an_error_not_a_reconnectable_stream() -> None:
    """A 3xx on the SSE endpoint must surface as StatusError.

    The status check was `>= 400` while every other entry uses `not 2xx`:
    a gateway 302 with an empty body decoded as a clean EOF, the backoff
    reset, and the client looped forever at 0.5s with no error signal.
    """
    mock = Mock()
    mock.sse_status = 302
    client = make_client(mock)
    agen = client.watch()
    try:
        # The timeout is load-bearing: the pre-fix code never raises here
        # (a 3xx decoded as a clean stream and the watch looped forever),
        # so without it a regression turns into a hung test instead of a
        # red one.
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(anext(agen), timeout=5)
            print("UNREACHABLE: no error within 5s - reconnect storm?")
        raise AssertionError("3xx did not surface as StatusError")
    except sdkmod.StatusError as exc_info:
        assert exc_info.status == 302
        assert exc_info.url.endswith("/api/v1/push/messages")
    finally:
        await agen.aclose()
        await client.aclose()


async def test_health_shape_corruption_is_a_shape_error() -> None:
    """A 200 with a body of the wrong type lands in ShapeError.

    pydantic's own ValidationError used to escape the ClientError tree for
    every route that decodes through model_validate; callers that classify
    "except ClientError" saw an unplanned exception class. One pin here is
    enough: every route shares the same wrapper.
    """
    mock = Mock()
    # Truthy body, wrong field *type*: `account` must be an enum string, so a
    # number fails model_validate — and the mock's `or`-fallback must not kick
    # in the way an empty list would.
    mock.health_body = {"account": 123, "status": "ok", "version": "9.9.9"}
    client = make_client(mock)
    with pytest.raises(sdkmod.ShapeError):
        await client.health()
    await client.aclose()


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


async def test_chatlab_messages_decodes_the_chatlab_envelope_and_paging() -> None:
    mock = Mock()
    mock.chatlab_page = {
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "count": 1,
        "members": [{"accountName": "张三", "avatar": "", "groupNickname": "",
                     "platformId": "alice", "username": "alice"}],
        "messages": [{"accountName": "alice", "content": "hi",
                      "groupNickname": "", "platformMessageId": "42",
                      "sender": "alice", "timestamp": 1700000000, "type": 1}],
        "meta": {"groupId": "", "name": "", "ownerId": "",
                 "platform": "weflow", "type": "private"},
        "page": {"hasMore": True, "nextCursor": "1000"},
        "talker": "alice",
    }
    client = make_client(mock)
    page = await client.chatlab_messages("alice", keyword="hi", limit=50)
    assert page.count == 1
    assert page.page.has_more is True
    assert page.page.next_cursor == "1000"
    assert page.messages[0].platform_message_id == "42"
    assert page.messages[0].type == 1, "ChatLab type codes, not the native localType"
    assert mock.messages_query == {"talker": "alice", "keyword": "hi", "limit": "50"}


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


async def test_group_members_decodes_roster_page_and_sends_chatroom_param() -> None:
    mock = Mock()
    mock.group_members_page = {
        "success": True, "chatroomId": "123@chatroom", "count": 2,
        "fromCache": False, "updatedAt": 1700000000123,
        "members": [
            {"alias": "", "avatarUrl": "", "displayName": "潜水者",
             "groupNickname": "", "isFriend": False, "isOwner": False,
             "messageCount": 0, "nickname": "", "remark": "", "wxid": "quiet"},
            {"alias": "a", "avatarUrl": "", "displayName": "张三",
             "groupNickname": "张三", "isFriend": True, "isOwner": True,
             "messageCount": 9, "nickname": "三儿", "remark": "客户张三",
             "wxid": "alice"},
        ],
    }
    client = make_client(mock)
    page = await client.group_members("123@chatroom", include_message_counts=True)
    assert page.count == 2
    assert page.updated_at == 1700000000123, (
        "updatedAt is **milliseconds** — a seconds truncation silently halves "
        "freshness precision")
    # The roster includes silent members: a zero-count row is legal, not an error.
    assert page.members[0].message_count == 0
    assert page.members[1].is_owner, "exactly one owner when the roster carries one"
    assert len(mock.group_members_queries) == 1
    q = mock.group_members_queries[0]
    # The mock reads the raw ASGI query string without percent-decoding, so
    # this is the wire form: httpx encodes the '@' in the chatroom id.
    assert q["chatroomId"] == "123%40chatroom"
    assert q.get("includeMessageCounts") == "1", "counts asked for"
    await client.aclose()


async def test_group_members_omits_include_message_counts_when_false() -> None:
    """The off switch: absence, not ``0`` — the server reads it through a
    flexible bool parser, and the wire shape for "don't scan the conversation"
    is absence."""
    mock = Mock()
    mock.group_members_page = {
        "success": True, "chatroomId": "123@chatroom", "count": 0,
        "fromCache": False, "updatedAt": 0, "members": [],
    }
    client = make_client(mock)
    page = await client.group_members("123@chatroom")
    assert page.members == []
    assert len(mock.group_members_queries) == 1
    assert "includeMessageCounts" not in mock.group_members_queries[0]
    await client.aclose()


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
    # The polling consumer asks for the server's maximum page size: the request
    # count is sessions / page_size, and the default page is two orders of
    # magnitude smaller than the cap.
    all_sessions = await client.list_all_sessions(page_size=10000)
    assert [s.username for s in all_sessions] == ["a", "b", "c"]
    assert len(mock.sessions_queries) == 3, "one request per page, plus the empty terminator"
    for i, q in enumerate(mock.sessions_queries):
        assert q.get("limit") == "10000", f"page {i} must carry the page size: {q}"
    assert mock.sessions_queries[0]["offset"] == "0"
    assert mock.sessions_queries[1]["offset"] == "2", "offset advances by rows returned"


async def test_media_bytes_by_id_fetches_a_single_segment_handle() -> None:
    mock = Mock()
    mock.media_hit_first = False  # no export side door: the handle already resolves
    client = make_client(mock)
    data = await client.media_bytes_by_id("abc123.png")
    assert data == b"png-bytes"
    assert mock.media_calls == ["abc123.png"], "one GET for the handle it was given"


# ---- sync_now -----------------------------------------------------------


async def test_sync_now_posts_with_the_bearer_token_and_decodes_counters() -> None:
    """``sync_now`` is a write: one POST, bearer auth, counters decoded."""
    mock = Mock()
    client = make_client(mock)
    result = await client.sync_now()
    assert result.success is True
    assert result.new_messages == 7
    assert result.revoke_messages == 2
    assert mock.sync_calls == 1
    await client.aclose()


async def test_no_read_path_triggers_a_sync() -> None:
    """Readiness probing must never sync on its own.

    A client that synced while merely asking whether the server is up would turn
    every probe into a disk scan of the live database.
    """
    mock = Mock()
    mock.states = ["indexing"] * 100
    client = make_client(mock)
    await client.health()
    with pytest.raises(NotReady):
        await client.ensure_ready("wxid_mock",
                                  {"wxid": "wxid_mock", "db_path": "X:/db"},
                                  timeout=0.6)
    assert mock.sync_calls == 0
    await client.aclose()

# ---- three behaviours fixed by the fourth serial-review round ----------------


async def test_time_bounds_are_ascii_only() -> None:
    """Full-width digits are not a time bound.

    `str.isdigit()` accepts them, so such input used to slip through and be
    refused by the server with a 400 - while the Rust client rejects it locally.
    The same input yielding two error classes is the divergence being fixed.
    """
    client = make_client(Mock())
    with pytest.raises(sdkmod.BadDate):
        # Written as escapes so the source carries no ambiguous literal.
        await client.chatlab_messages("wxid_a", end="\uff11\uff12\uff13")


async def test_group_members_rejects_an_empty_chatroom() -> None:
    """An empty chatroomId asks the server a different question.

    A 200 with an empty roster would read as "this group has no members", so the
    client refuses locally - the same fail-fast `talker` gets everywhere else.
    """
    client = make_client(Mock())
    with pytest.raises(sdkmod.ShapeError):
        await client.group_members("")


async def test_transport_failures_are_client_errors() -> None:
    """A refused connection must land inside the ClientError tree.

    httpx raises its own family, so "except ClientError" used to catch every
    server refusal while missing every network failure. Every public request
    entry goes through _http_get/_http_post; walk one representative per
    transport method (an entry that called self._http directly let a raw
    httpx error escape for that route alone).
    """
    client = Client("http://127.0.0.1:1", TOKEN, timeout=2.0)
    get_entries = [
        lambda: client.health(),
        lambda: client.accounts(),
        lambda: client.group_members("10001"),
    ]
    post_entries = [
        lambda: client.register({"qq": "10001", "key": "k", "db_path": "X:/a"}),
        lambda: client.sync_now(),
    ]
    for entry in get_entries + post_entries:
        with pytest.raises(sdkmod.TransportError):
            await entry()
        with pytest.raises(sdkmod.ClientError):
            await entry()



class _BudgetRecorder(httpx.AsyncBaseTransport):
    """Wraps the mock transport and records the timeout httpx actually applied.

    The published budgets are only worth something if a request that must be
    unbounded actually *travels* unbounded - this is the one place both budgets
    are observable, so the assertions below are about the wire, not about a
    constant someone could change without anything failing.
    """

    def __init__(self, inner: httpx.AsyncBaseTransport) -> None:
        self._inner = inner
        self.seen: list[tuple[str, dict]] = []

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.seen.append((request.url.path, dict(request.extensions.get("timeout") or {})))
        return await self._inner.handle_async_request(request)


def make_budget_client(mock: Mock) -> tuple[Client, _BudgetRecorder]:
    client = Client("http://mock", TOKEN)
    recorder = _BudgetRecorder(httpx.ASGITransport(app=mock.asgi_app()))
    # Same budget config the real client builds, only the transport is mocked.
    client._http = httpx.AsyncClient(transport=recorder, timeout=client._timeout)
    return client, recorder

async def test_published_timeout_budgets_travel_per_request() -> None:
    """Connect 5s everywhere; read 30s for JSON, **none** for size-bounded work.

    A whole-roster message count, a media body and a sync pass are bounded only by
    the library or the upload - applying the JSON read budget to them turns "this
    group is big" into a client-side failure. The uncounted roster listing must
    stay on the ordinary budget, otherwise the switch is just "no timeouts at all".
    """
    mock = Mock()
    mock.group_members_page = {
        "success": True, "chatroomId": "10001", "count": 0,
        "fromCache": False, "updatedAt": 1700000000123, "members": [],
    }
    client, rec = make_budget_client(mock)
    await client.accounts()
    await client.group_members("10001", include_message_counts=False)
    await client.group_members("10001", include_message_counts=True)
    await client.sync_now()
    # The media route mints the file on its first miss by default; this test
    # only needs the fetch itself, so start from a hit.
    mock.media_hit_first = False
    await client.media_bytes_by_id("deadbeef.png")

    for path, to in rec.seen:
        assert to["connect"] == CONNECT_TIMEOUT, f"{path} lost the connect bound: {to}"
    by_path = [to for _, to in rec.seen]
    assert by_path[0]["read"] == READ_TIMEOUT, f"accounts: {by_path[0]}"
    assert by_path[1]["read"] == READ_TIMEOUT, f"uncounted roster: {by_path[1]}"
    assert by_path[2]["read"] is None, f"counted roster: {by_path[2]}"
    assert by_path[3]["read"] is None, f"sync: {by_path[3]}"
    assert by_path[4]["read"] is None, f"media: {by_path[4]}"
    await client.aclose()


async def test_watch_stream_is_not_bounded_by_the_json_read_timeout() -> None:
    """The SSE connection is long-lived by contract: it must carry no read bound.

    A read bound is charged per read operation and the server pings every 25s, so the
    30s JSON budget leaves a 5s margin - one proxy stall drops a healthy idle
    stream, and the client reconnects so quickly the loss looks like a normal end.
    """
    mock = Mock()
    mock.sse_body = ("\n".join(sse_frame("message.new", new_payload("9", "hi"), 7)) + "\n\n").encode()
    client, rec = make_budget_client(mock)
    agen = client.watch()
    await collect(agen, 1, timeout=5)
    await agen.aclose()
    stream = [to for path, to in rec.seen if path.endswith("/api/v1/push/messages")]
    assert stream, f"the stream request was never observed: {rec.seen}"
    assert all(to["read"] is None for to in stream), stream
    assert all(to["connect"] == CONNECT_TIMEOUT for to in stream), stream
    await client.aclose()



async def test_status_error_url_stays_the_request_url() -> None:
    """`StatusError.url` is the request URL - nothing else.

    The 200-refusal used to fold the state name into that field, so a caller
    matching on url (prefix, exact, per-endpoint metrics) silently misclassified
    the refusal while the human-readable message still looked right. The state
    now rides on its own field.
    """
    mock = Mock()
    mock.register_body = {
        "success": True, "state": "account_conflict",
        "wxid": "wxid_other", "occupied_by": "wxid_other",
    }
    client = make_client(mock)
    with pytest.raises(StatusError) as exc:
        await client.ensure_ready("wxid_other", {"wxid": "wxid_other", "db_path": "X:/db"}, timeout=5)
    assert exc.value.url == "http://mock/api/v1/accounts", exc.value.url
    assert exc.value.detail == "state=account_conflict", exc.value.detail
    assert "account_conflict" in str(exc.value), str(exc.value)
    await client.aclose()


async def test_malformed_base_url_stays_inside_the_client_error_tree() -> None:
    """httpx raises InvalidURL for an unparsable URL, and it is NOT an HTTPError.

    Wrapping only the HTTPError family let a bad base_url (bad port, control
    character in the host, malformed IPv6 literal) escape every documented
    `except ClientError` - the caller caught server refusals but not its own
    typo. The URL is built per request, so all three entry points share the fix.
    """
    client = Client("http://h:99999x", TOKEN, timeout=2.0)
    for entry in (client.health, client.accounts, client.sync_now):
        with pytest.raises(ClientError):
            await entry()
    await client.aclose()


async def test_media_bytes_reject_an_empty_handle_without_a_request() -> None:
    """Same fail-fast as talker/chatroom: an empty handle is a different path.

    The server answers "no such file", so without the local check a missing
    file_name reads as an unexportable handle rather than a caller mistake.
    """
    mock = Mock()
    client = make_client(mock)
    message = gen.ChatlabMessage.model_validate({
        "accountName": "alice", "content": "x", "groupNickname": "",
        "media": {"type": "image", "fileName": "", "md5": "z"},
        "platformMessageId": "1", "sender": "alice", "timestamp": 1, "type": 1,
    })
    with pytest.raises(ShapeError):
        await client.media_bytes(message, "wxid_alice")
    with pytest.raises(ShapeError):
        await client.media_bytes_by_id("")
    assert mock.media_calls == [], "no request may go out for an empty handle"
    await client.aclose()


async def test_media_bytes_rejects_a_redirect_on_the_first_hit() -> None:
    """The existing test covers 3xx on the *retry*; this covers the first hit.

    The bytes check is "not 2xx" for both fetches. Only the retry being
    checked would let a first-hop 302 hand back an empty redirect body.
    """
    mock = Mock()
    mock.media_hit_first = False  # first GET already answers
    mock.media_status = 302
    client = make_client(mock)
    message = gen.ChatlabMessage.model_validate({
        "accountName": "alice", "content": "x", "groupNickname": "",
        "media": {"type": "image", "fileName": "abc.png", "md5": "z"},
        "platformMessageId": "1", "sender": "alice", "timestamp": 1, "type": 1,
    })
    with pytest.raises(StatusError) as exc:
        await client.media_bytes(message, "wxid_alice")
    assert exc.value.status == 302, exc.value.status
    assert exc.value.url == "http://mock/api/v1/media/abc.png", exc.value.url
    await client.aclose()
