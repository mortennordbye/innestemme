"""The engine's `POST /api/ask`: one request in, the answer's text out."""

from __future__ import annotations

import aiohttp

from .const import TIMEOUT_SECONDS


class CannotConnect(Exception):
    """The engine did not answer, or has no `/api/ask` (no `ask-token` set)."""


class InvalidToken(Exception):
    """The engine refused the token."""


async def ask(session: aiohttp.ClientSession, url: str, token: str, text: str) -> str:
    """The answer to `text`; empty when the assistant ends quietly."""
    try:
        async with session.post(
            f"{url}/api/ask",
            params={"q": text},
            headers={"Authorization": f"Bearer {token}"},
            timeout=aiohttp.ClientTimeout(total=TIMEOUT_SECONDS),
        ) as response:
            if response.status == 401:
                raise InvalidToken
            if response.status == 404:
                raise CannotConnect("the engine has no /api/ask: set ask-token and web: true")
            body = await response.json(content_type=None)
            if response.status != 200:
                raise CannotConnect(body.get("reason", f"status {response.status}"))
            return str(body.get("answer") or "")
    except (aiohttp.ClientError, TimeoutError, ValueError) as error:
        raise CannotConnect(str(error)) from error
