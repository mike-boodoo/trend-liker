"""Client for lobbies: shared, live re-ranked pools of members' feeds."""
from __future__ import annotations

import json
from collections.abc import AsyncIterator

import httpx

from .forus import DEFAULT_URL, ForUsError, MAX_BATCH, to_candidate
from .models import Post

KINDS = ("impression", "like", "bookmark", "reply", "skip")


class LobbyClient:
    def __init__(self, base_url: str = DEFAULT_URL, transport: httpx.AsyncBaseTransport | None = None):
        self._http = httpx.AsyncClient(base_url=base_url, transport=transport, timeout=10.0)

    async def aclose(self) -> None:
        await self._http.aclose()

    async def _json(self, method: str, url: str, **kw):
        r = await self._http.request(method, url, **kw)
        if r.status_code >= 400:
            raise ForUsError(r.status_code, r.text)
        return r.json()

    async def create(self, lobby: str, name: str, owner: str, policy: dict | None = None) -> dict:
        body = {"id": lobby, "name": name, "owner": owner}
        if policy is not None:
            body["policy"] = policy
        return await self._json("POST", "/api/lobbies", json=body)

    async def profile(self, lobby: str) -> dict:
        return await self._json("GET", f"/api/lobbies/{lobby}")

    async def set_policy(self, lobby: str, owner: str, policy: dict) -> dict:
        return await self._json("PUT", f"/api/lobbies/{lobby}/policy", json={"user_id": owner, "policy": policy})

    async def join(self, lobby: str, user: str) -> dict:
        return await self._json("POST", f"/api/lobbies/{lobby}/members", json={"user_id": user})

    async def leave(self, lobby: str, user: str) -> dict:
        return await self._json("DELETE", f"/api/lobbies/{lobby}/members/{user}")

    async def contribute(self, lobby: str, user: str, posts: list[Post]) -> dict:
        """Share posts from `user`'s own feed. Protected-account posts are never sent;
        the server requires the user's publishing opt-in (403 otherwise)."""
        candidates = [to_candidate(p) for p in posts if not p.protected]
        totals = {"added": 0, "merged": 0, "off_topic": 0, "rejected": 0, "evicted": 0,
                  "skipped_protected": len(posts) - len(candidates)}
        for i in range(0, len(candidates), MAX_BATCH):
            r = await self._json("POST", f"/api/lobbies/{lobby}/contribute",
                                 json={"user_id": user, "items": candidates[i : i + MAX_BATCH]})
            for k in ("added", "merged", "off_topic", "rejected", "evicted"):
                totals[k] += r[k]
        return totals

    async def interact(self, lobby: str, user: str, item_id: str, kind: str, active: bool = True) -> int:
        if kind not in KINDS:
            raise ValueError(f"kind must be one of {KINDS}")
        body = {"user_id": user, "item_id": item_id, "kind": kind, "active": active}
        return (await self._json("POST", f"/api/lobbies/{lobby}/interactions", json=body))["version"]

    async def feed(self, lobby: str, user: str, limit: int = 50) -> dict:
        return await self._json("GET", f"/api/lobbies/{lobby}/feed", params={"user": user, "limit": limit})

    async def events(self, lobby: str, user: str) -> AsyncIterator[tuple[str, dict]]:
        """Yield (event, data) from the lobby's live stream until the caller stops."""
        async with self._http.stream("GET", f"/api/lobbies/{lobby}/events",
                                     params={"user": user}, timeout=None) as r:
            if r.status_code >= 400:
                raise ForUsError(r.status_code, (await r.aread()).decode())
            event, data = "message", []
            async for line in r.aiter_lines():
                if line.startswith(":"):
                    continue  # keep-alive comment
                if line == "":
                    if data:
                        yield event, json.loads("\n".join(data))
                    event, data = "message", []
                elif line.startswith("event:"):
                    event = line[6:].strip()
                elif line.startswith("data:"):
                    data.append(line[5:].lstrip())


async def share_feed(client: LobbyClient, agent, lobby: str) -> dict:
    """Pull the member's own feed (what their platform's algorithm shows them) into the lobby."""
    return await client.contribute(lobby, agent.profile.user_id, await agent.fetch())
