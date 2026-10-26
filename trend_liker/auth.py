"""Sign in to the universal-feed server and get a session token for API clients."""
from __future__ import annotations

from collections.abc import Awaitable, Callable

import httpx

from .forus import DEFAULT_URL, ForUsError

SESSION_COOKIE = "tl_session"


async def sign_in_with_x(
    approve: Callable[[str], Awaitable[str]],
    base_url: str = DEFAULT_URL,
    transport: httpx.AsyncBaseTransport | None = None,
) -> str:
    """Run Sign in with X and return the session token.

    `approve` receives X's authorization URL and returns the callback URL X redirects
    back to (in production a person approves in a browser; tests use a mock X).
    The same HTTP client carries the server's state cookie through, as a browser would.
    """
    async with httpx.AsyncClient(base_url=base_url, transport=transport, follow_redirects=False) as c:
        start = await c.get("/auth/x/login")
        if start.status_code not in (302, 303, 307) or "location" not in start.headers:
            raise ForUsError(start.status_code, start.text)
        done = await c.get(await approve(start.headers["location"]))
        token = c.cookies.get(SESSION_COOKIE)
        if done.status_code != 303 or not token:
            raise ForUsError(done.status_code, done.text)
        return token


async def dev_login(user: str, base_url: str = DEFAULT_URL, transport: httpx.AsyncBaseTransport | None = None) -> str:
    """Session token for `user` via the server's development sign-in (AUTH_DEV_LOGIN=1 only)."""
    async with httpx.AsyncClient(base_url=base_url, transport=transport) as c:
        r = await c.post("/auth/dev/login", json={"user_id": user})
        if r.status_code >= 400:
            raise ForUsError(r.status_code, r.text)
        return r.json()["token"]
