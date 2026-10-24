"""In-process mock of the X API endpoints we use, as an httpx transport."""
from __future__ import annotations

import json

import httpx


class MockXServer:
    def __init__(self, timelines: dict[str, list[dict]], valid_token: str = "test-token"):
        self.timelines = timelines
        self.valid_token = valid_token
        self.likes: dict[str, set[str]] = {}
        self.requests: list[httpx.Request] = []
        self.fail_next_with: list[int] = []  # queue of forced status codes
        self.fail_likes_with: int | None = None  # status for every like request

    @property
    def transport(self) -> httpx.MockTransport:
        return httpx.MockTransport(self._handle)

    def _handle(self, req: httpx.Request) -> httpx.Response:
        self.requests.append(req)
        if req.headers.get("authorization") != f"Bearer {self.valid_token}":
            return httpx.Response(401, json={"title": "Unauthorized"})
        if self.fail_next_with:
            code = self.fail_next_with.pop(0)
            headers = {"retry-after": "0"} if code == 429 else {}
            return httpx.Response(code, json={"title": "forced"}, headers=headers)

        parts = req.url.path.strip("/").split("/")  # 2 users {id} ...
        if len(parts) >= 4 and parts[:2] == ["2", "users"]:
            uid = parts[2]
            if req.method == "GET" and parts[3:] == ["timelines", "reverse_chronological"]:
                return self._timeline(uid, req.url.params)
            if req.method == "POST" and parts[3:] == ["likes"]:
                if self.fail_likes_with:
                    return httpx.Response(self.fail_likes_with, json={"title": "forced"})
                pid = json.loads(req.content)["tweet_id"]
                self.likes.setdefault(uid, set()).add(pid)
                return httpx.Response(200, json={"data": {"liked": True}})
        return httpx.Response(404, json={"title": "Not Found"})

    def _timeline(self, uid: str, params) -> httpx.Response:
        tweets = self.timelines.get(uid, [])
        size = int(params.get("max_results", 20))
        start = int(params.get("pagination_token", 0))
        chunk = tweets[start : start + size]
        meta: dict = {"result_count": len(chunk)}
        if start + size < len(tweets):
            meta["next_token"] = str(start + size)
        return httpx.Response(200, json={"data": chunk, "meta": meta})


def make_tweet(i: int, text: str, author: str = "a1", likes: int = 0) -> dict:
    return {
        "id": str(i),
        "author_id": author,
        "text": text,
        "created_at": "2026-10-24T00:00:00Z",
        "public_metrics": {"like_count": likes},
    }
