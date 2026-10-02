"""Handwritten behavior layer over :mod:`weflow_sdk.generated`.

The generated face covers shapes: request URLs, response models, error
decoding. It cannot cover behavior - pagination loops, reconnection,
readiness polling - and its operations carry no query parameters (the
description currently declares only responses), so every request here
builds its own query string and reuses the generated models purely for
deserialization. The transport is ``httpx.AsyncClient``; the generated
``api_client`` stays in the tree as the typed request face but the
behavior layer does not route through it.

Mirrors ``clients/rust/src/client.rs`` method by method, including the
failure semantics: 503 while indexing is *waiting*, not an error.
"""

from __future__ import annotations

import asyncio
import json as _json
import logging
import re
from dataclasses import dataclass
from typing import Any, AsyncIterator, Callable, Optional

import httpx
from pydantic import ValidationError

from .generated.weflow_sdk import models as gen

log = logging.getLogger(__name__)


class ClientError(Exception):
    """Base error for the behavior layer."""


class StatusError(ClientError):
    """The server answered, but not with a usable body."""

    def __init__(self, status: int, url: str) -> None:
        super().__init__(f"HTTP {status} on {url}")
        self.status = status
        self.url = url


class ShapeError(ClientError):
    """The body did not match the documented shape."""


class NotReady(ClientError):
    """``ensure_ready`` hit its deadline before the account reached ready."""

    def __init__(self, timeout: float, last_state: str) -> None:
        super().__init__(
            f"account did not become ready within {timeout}s (last state: {last_state})"
        )
        self.timeout = timeout
        self.last_state = last_state


class BadDate(ClientError):
    """``search`` was given a malformed YYYYMMDD bound."""


@dataclass(frozen=True)
class _SseFrame:
    event: Optional[str]
    data: Optional[str]
    id_: Optional[str]


def _parse_sse_block(block: str) -> Optional[_SseFrame]:
    event = data = id_ = None
    for line in block.splitlines():
        if line.startswith(":"):
            continue  # heartbeat comment
        if line.startswith("id:"):
            id_ = line[3:].strip()
        elif line.startswith("event:"):
            event = line[6:].strip()
        elif line.startswith("data:"):
            data = line[5:].strip()
    if data is None:
        return None
    return _SseFrame(event, data, id_)


class Client:
    """Async client for one weflow-server instance.

    ``token`` is the API token the server printed on first start (or
    ``--show-token``). Auth goes in the ``Authorization`` header only -
    never in the URL.
    """

    def __init__(self, base_url: str, token: str, timeout: float = 30.0) -> None:
        self._http = httpx.AsyncClient(timeout=timeout)
        self._base = base_url.rstrip("/")
        self._token = token

    async def aclose(self) -> None:
        await self._http.aclose()

    async def __aenter__(self) -> "Client":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.aclose()

    # ---- plumbing -------------------------------------------------------

    def _url(self, path: str) -> str:
        return self._base + path

    async def _get_json(self, path: str, query: dict[str, str]):
        url = self._url(path)
        resp = await self._http.get(
            url,
            headers={"Authorization": f"Bearer {self._token}"},
            params=query,
        )
        return await self._decode(resp, url)

    @staticmethod
    async def _decode(resp: httpx.Response, url: str):
        if resp.status_code >= 400:
            raise StatusError(resp.status_code, url)
        try:
            return resp.json()
        except _json.JSONDecodeError as exc:  # pragma: no cover - defensive
            raise ShapeError(str(exc)) from exc

    # ---- ensure_ready ---------------------------------------------------

    async def ensure_ready(self, account: str, body: dict, timeout: float = 120.0) -> None:
        """Register (idempotently) and poll until the account is ready.

        Intermediate states (``indexing``) are waiting, not errors; errors
        are reserved for the deadline or the account landing in ``error``.
        """
        url = self._url("/api/v1/accounts")
        resp = await self._http.post(
            url,
            headers={"Authorization": f"Bearer {self._token}"},
            json=body,
        )
        if resp.status_code >= 400:
            raise StatusError(resp.status_code, url)
        deadline = asyncio.get_running_loop().time() + timeout
        while True:
            listing = gen.AccountsList.model_validate(
                await self._get_json("/api/v1/accounts", {})
            )
            mine = next((a for a in listing.accounts if a.wxid == account), None)
            if mine is not None:
                if mine.state is gen.AccountStatus.READY:
                    return
                if mine.state is gen.AccountStatus.ERROR:
                    raise NotReady(timeout, f"error: {mine.error or ''}")
                last_state = str(mine.state.value)
            else:
                last_state = "not-registered"
            if asyncio.get_running_loop().time() >= deadline:
                raise NotReady(timeout, last_state)
            await asyncio.sleep(0.25)

    # ---- drain_session --------------------------------------------------

    async def drain_session(
        self,
        talker: str,
        since: Optional[int],
        on_page: Callable[[list[gen.PullMessage]], None],
    ) -> int:
        """Drain one session through the Pull cursor loop.

        Cursors are echoed verbatim (``next_since`` / ``next_offset`` come
        back exactly as the server sent them): the server pages by
        (timestamp group, offset), and a client-derived cursor is how pages
        get silently skipped or replayed.
        """
        next_since = since
        next_offset: Optional[int] = None
        total = 0
        while True:
            query: dict[str, str] = {}
            if next_since is not None:
                query["since"] = str(next_since)
            if next_offset is not None:
                query["offset"] = str(next_offset)
            page = gen.PullEnvelope.model_validate(
                await self._get_json(f"/api/v1/sessions/{talker}/messages", query)
            )
            total += len(page.messages)
            on_page(page.messages)
            if not page.sync.has_more:
                return total
            next_since = page.sync.next_since
            next_offset = page.sync.next_offset

    # ---- list_all_sessions ----------------------------------------------

    async def list_all_sessions(self) -> list[gen.SessionNative]:
        """Fetch the complete session list via offset paging.

        The list is a live view; duplicates across pages are collapsed with a
        warning (losing data silently is the failure mode this loop must not
        have).
        """
        out: list[gen.SessionNative] = []
        seen: set[str] = set()
        offset = 0
        while True:
            page = gen.SessionsNative.model_validate(
                await self._get_json("/api/v1/sessions", {"offset": str(offset)})
            )
            count = len(page.sessions)
            for session in page.sessions:
                if session.username not in seen:
                    seen.add(session.username)
                    out.append(session)
                else:
                    log.warning(
                        "session list shifted during pagination: duplicate %s collapsed",
                        session.username,
                    )
            if count == 0:
                return out
            offset += count

    # ---- search ---------------------------------------------------------

    async def search(
        self,
        talker: str,
        keyword: str,
        start: Optional[str] = None,
        end: Optional[str] = None,
    ) -> gen.MessagesNative:
        """Keyword + time-window search; YYYYMMDD validated client-side.

        ``end`` covers the whole day, same as the server.
        """
        for field, value in (("start", start), ("end", end)):
            if value is not None and not re.fullmatch(r"\d{8}", value):
                raise BadDate(f"invalid date bound {field}={value!r}: expected YYYYMMDD")
        query = {"talker": talker, "keyword": keyword}
        if start is not None:
            query["start"] = start
        if end is not None:
            query["end"] = end
        return gen.MessagesNative.model_validate(
            await self._get_json("/api/v1/messages", query)
        )

    # ---- media_bytes ----------------------------------------------------

    async def media_bytes(self, message: gen.ChatlabMessage) -> bytes:
        """Fetch media bytes for a uniquely-named handle.

        One automatic retry after a 404: the caller may have serialized the
        handle before the export finished, and the ``media=1`` re-export is
        what mints the file. A second 404 means the handle was never
        exportable and the error propagates.
        """
        if message.media is None:
            raise StatusError(404, "(no media on message)")
        name = message.media.file_name
        url = self._url(f"/api/v1/media/{name}")
        resp = await self._http.get(url, headers={"Authorization": f"Bearer {self._token}"})
        if resp.status_code == 404:
            await self._get_json(
                "/chatlab/messages",
                {"talker": message.account_name, "media": "1"},
            )
            resp = await self._http.get(url, headers={"Authorization": f"Bearer {self._token}"})
        if resp.status_code >= 400:
            raise StatusError(resp.status_code, url)
        return resp.content

    # ---- watch ------------------------------------------------------------

    async def watch(self, *, poll_interval: float = 0.5) -> AsyncIterator[Any]:
        """Yield live server events, reconnecting with ``Last-Event-ID``.

        Heartbeat comment frames are skipped. ``sync`` frames carry the
        server's ``generation``; a jump means the replay buffer cannot fill
        the gap and the caller should fall back to :meth:`drain_session`.
        Cancelling the iteration closes the stream.
        """
        last_event_id: Optional[str] = None
        backoff = 0.5
        while True:
            try:
                headers = {
                    "Authorization": f"Bearer {self._token}",
                    "Accept": "text/event-stream",
                }
                if last_event_id is not None:
                    headers["Last-Event-ID"] = last_event_id
                event: Optional[Any] = None
                async with self._http.stream(
                    "GET", self._url("/api/v1/push/messages"), headers=headers
                ) as resp:
                    if resp.status_code >= 400:
                        raise StatusError(resp.status_code, self._url("/api/v1/push/messages"))
                    backoff = 0.5
                    block: list[str] = []
                    async for line in resp.aiter_lines():
                        if line.strip():
                            block.append(line)
                            continue
                        if not block:
                            continue
                        frame = _parse_sse_block("\n".join(block))
                        block = []
                        if frame is None:
                            continue
                        if frame.id_ is not None and frame.id_.isdigit():
                            last_event_id = frame.id_
                        decoded = self._decode_event(frame)
                        if decoded is not None:
                            event = decoded
                            break
                    if event is None and block:
                        # EOF with a pending frame: servers may close without
                        # the trailing blank line, and the last event would
                        # silently vanish with the buffer.
                        frame = _parse_sse_block("\n".join(block))
                        block = []
                        if frame is not None:
                            if frame.id_ is not None and frame.id_.isdigit():
                                last_event_id = frame.id_
                            try:
                                event = self._decode_event(frame)
                            except Exception as exc:
                                log.warning("undecodable final SSE frame: %s", exc)
                if event is not None:
                    yield event
            except httpx.HTTPError as exc:
                log.warning("SSE stream error, reconnecting: %s", exc)
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 30.0)

    @staticmethod
    def _decode_event(frame: _SseFrame) -> Optional[Any]:
        assert frame.data is not None
        try:
            payload = _json.loads(frame.data)
        except _json.JSONDecodeError as exc:
            raise ShapeError(str(exc)) from exc
        kind = payload.get("event", frame.event or "")
        if kind == "message.new":
            return gen.EventNew.model_validate(payload)
        if kind == "message.revoke":
            return gen.EventRevoke.model_validate(payload)
        if kind == "sync":
            return gen.EventSync.model_validate(payload)
        # Notification face / unknown kinds: meta-only pass-through so a new
        # server event kind degrades instead of killing the stream.
        log.debug("ignoring unknown SSE event kind: %s", kind)
        return None
