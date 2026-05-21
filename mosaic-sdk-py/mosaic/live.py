"""Real-time co-editing session over WebSocket.

Requires the ``websockets`` extra:

    pip install websockets

"""

from __future__ import annotations

import asyncio
from typing import AsyncIterator


class LiveSession:
    """Bidirectional binary update stream against a mosaic-serve doc room.

    Usage:
        async with LiveSession.connect("ws://server:7700/ws/doc/payments.rs") as s:
            await s.send(b"my update")
            async for update in s.recv_iter():
                ...
    """

    def __init__(self, ws):
        self._ws = ws

    @classmethod
    async def connect(cls, url: str) -> "LiveSession":
        try:
            import websockets  # type: ignore[import-not-found]
        except ImportError as e:
            raise RuntimeError(
                "LiveSession requires the 'websockets' package: pip install websockets"
            ) from e
        ws = await websockets.connect(url)
        return cls(ws)

    async def send(self, update: bytes) -> None:
        await self._ws.send(update)

    async def recv(self) -> bytes | None:
        try:
            msg = await self._ws.recv()
        except Exception:
            return None
        if isinstance(msg, bytes):
            return msg
        return msg.encode("utf-8") if isinstance(msg, str) else None

    async def recv_iter(self) -> AsyncIterator[bytes]:
        while True:
            data = await self.recv()
            if data is None:
                return
            yield data

    async def close(self) -> None:
        await self._ws.close()

    async def __aenter__(self) -> "LiveSession":
        return self

    async def __aexit__(self, *exc) -> None:
        await self.close()
