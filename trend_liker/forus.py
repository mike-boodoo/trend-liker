"""Client + connector for the universal-feed ("For Us" / "For Me") server."""
from __future__ import annotations

import httpx

from .connector import Query, Record
from .models import Post

DEFAULT_URL = "http://127.0.0.1:3000"


class ForUsError(Exception):
    def __init__(self, status: int, detail: str = ""):
        super().__init__(f"for-us error {status}: {detail}")
        self.status = status


class ForUsClient:
    def __init__(self, base_url: str = DEFAULT_URL, transport: httpx.AsyncBaseTransport | None = None):
        self._http = httpx.AsyncClient(base_url=base_url, transport=transport, timeout=10.0)

    async def aclose(self) -> None:
        await self._http.aclose()

    async def _json(self, method: str, url: str, **kw):
        r = await self._http.request(method, url, **kw)
        if r.status_code >= 400:
            raise ForUsError(r.status_code, r.text)
        return r.json()

    async def feeds(self) -> list[dict]:
        return await self._json("GET", "/api/feeds")

    async def subscriptions(self, user: str) -> list[str]:
        return (await self._json("GET", f"/api/users/{user}/subscriptions"))["public_feeds"]

    async def subscribe(self, user: str, feed: str) -> list[str]:
        body = await self._json("POST", f"/api/users/{user}/subscriptions", json={"feed": feed})
        return body["public_feeds"]

    async def ingest(self, items: list[dict]) -> dict:
        return await self._json("POST", "/api/ingest", json={"items": items})

    async def home_feed(self, user: str) -> list[dict]:
        return (await self._json("GET", f"/api/users/{user}/feed"))["items"]


class ForUsConnector:
    """Queryable source over a user's For-Me-subscribed For-Us feeds."""

    name = "for-us"

    def __init__(self, client: ForUsClient):
        self.client = client

    async def query(self, q: Query) -> list[Record]:
        out = []
        for item in await self.client.home_feed(q.user_id):
            haystack = {w.strip(".,!?#").lower() for w in item["text"].split()} | set(item["categories"])
            if haystack & q.terms:
                out.append(Record(self.name, item["id"], item["text"], float(item["score"])))
        return out


MAX_BATCH = 500  # server rejects larger batches
LIKES_FOR_FULL_SCORE = 10_000


def to_candidate(post: Post) -> dict:
    """X post -> universal-feed ingest candidate (score = engagement in [0.5, 1])."""
    score = 0.5 + 0.5 * min(post.like_count, LIKES_FOR_FULL_SCORE) / LIKES_FOR_FULL_SCORE
    return {
        "source": "X",
        "external_id": post.id,
        "author": post.author_id,
        "text": post.text,
        "url": f"https://x.com/i/status/{post.id}",
        "score": score,
    }


async def publish_posts(client: ForUsClient, posts: list[Post]) -> dict:
    """Push posts into the shared For Us feeds. Protected-account posts are never sent."""
    candidates = [to_candidate(p) for p in posts if not p.protected]
    totals = {"ingested": 0, "duplicates": 0, "unclassified": 0, "rejected": 0,
              "skipped_protected": len(posts) - len(candidates)}
    for i in range(0, len(candidates), MAX_BATCH):
        r = await client.ingest(candidates[i : i + MAX_BATCH])
        totals["ingested"] += len(r["ingested"])
        for k in ("duplicates", "unclassified", "rejected"):
            totals[k] += r[k]
    return totals
