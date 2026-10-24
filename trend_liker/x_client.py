"""X API v2 client.

Note: X's API has no "For You" timeline. The closest endpoint is the
reverse-chronological home timeline, which is what we use here.
"""
from __future__ import annotations

import asyncio
from typing import Protocol

import httpx

from .models import Page, Post

BASE_URL = "https://api.x.com"


class XError(Exception):
    def __init__(self, status: int, detail: str = ""):
        super().__init__(f"X API error {status}: {detail}")
        self.status = status


class RateLimited(XError):
    def __init__(self, retry_after: float):
        super().__init__(429, "rate limited")
        self.retry_after = retry_after


class XClient(Protocol):
    async def home_timeline(
        self, user_id: str, max_results: int = 20, pagination_token: str | None = None
    ) -> Page: ...

    async def like(self, user_id: str, post_id: str) -> bool: ...


class HttpXClient:
    """Real client. `transport` is injectable so tests can mock the network."""

    def __init__(
        self,
        bearer_token: str,
        transport: httpx.AsyncBaseTransport | None = None,
        base_url: str = BASE_URL,
        max_retries: int = 2,
    ):
        self._http = httpx.AsyncClient(
            base_url=base_url,
            transport=transport,
            headers={"Authorization": f"Bearer {bearer_token}"},
            timeout=10.0,
        )
        self._max_retries = max_retries

    async def aclose(self) -> None:
        await self._http.aclose()

    async def _request(self, method: str, url: str, **kw) -> dict:
        for attempt in range(self._max_retries + 1):
            r = await self._http.request(method, url, **kw)
            if r.status_code == 429:
                wait = float(r.headers.get("retry-after", "1"))
                if attempt == self._max_retries:
                    raise RateLimited(wait)
                await asyncio.sleep(wait)
                continue
            if r.status_code >= 400:
                raise XError(r.status_code, r.text)
            return r.json()
        raise AssertionError("unreachable")

    async def home_timeline(
        self, user_id: str, max_results: int = 20, pagination_token: str | None = None
    ) -> Page:
        params = {
            "max_results": max_results,
            "tweet.fields": "author_id,created_at,public_metrics",
        }
        if pagination_token:
            params["pagination_token"] = pagination_token
        body = await self._request(
            "GET", f"/2/users/{user_id}/timelines/reverse_chronological", params=params
        )
        posts = [
            Post(
                id=t["id"],
                author_id=t.get("author_id", ""),
                text=t.get("text", ""),
                like_count=t.get("public_metrics", {}).get("like_count", 0),
                created_at=t.get("created_at", ""),
            )
            for t in body.get("data", [])
        ]
        return Page(posts, body.get("meta", {}).get("next_token"))

    async def like(self, user_id: str, post_id: str) -> bool:
        body = await self._request(
            "POST", f"/2/users/{user_id}/likes", json={"tweet_id": post_id}
        )
        return bool(body.get("data", {}).get("liked"))
